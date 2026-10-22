use super::{Cancellation, ToolError, read_line_capped};
use crate::tools::selection::PHASE_TIME;
use std::time::{Duration, Instant};

fn monotonic_nanos() -> Result<u64, ToolError> {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    u64::try_from(time.tv_sec)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1_000_000_000))
        .and_then(|seconds| seconds.checked_add(u64::try_from(time.tv_nsec).ok()?))
        .ok_or(ToolError::Operation)
}

pub(super) fn encode_deadline(deadline: tokio::time::Instant) -> Result<[u8; 8], ToolError> {
    let clock = monotonic_nanos()?;
    let remaining = deadline
        .into_std()
        .saturating_duration_since(Instant::now())
        .min(PHASE_TIME);
    let nanos = u64::try_from(remaining.as_nanos()).map_err(|_| ToolError::Limit)?;
    Ok(clock
        .checked_add(nanos)
        .ok_or(ToolError::Limit)?
        .to_le_bytes())
}

pub(in crate::tools) fn decode_deadline(frame: [u8; 8]) -> Result<Instant, ToolError> {
    let now = Instant::now();
    let remaining =
        Duration::from_nanos(u64::from_le_bytes(frame).saturating_sub(monotonic_nanos()?));
    if remaining > PHASE_TIME {
        return Err(ToolError::Limit);
    }
    Ok(now + remaining)
}

pub(super) fn response<T>(result: &Result<T, ToolError>) -> &'static [u8] {
    match result {
        Ok(_) => b"SELECT_DONE\n",
        Err(ToolError::Limit) => b"SELECT_LIMIT\n",
        Err(ToolError::Timeout(_)) => b"SELECT_TIMEOUT\n",
        Err(_) => b"SELECT_REJECTED\n",
    }
}

pub(super) async fn receive(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
    deadline: tokio::time::Instant,
) -> Result<(), ToolError> {
    if tokio::time::Instant::now() >= deadline {
        return Err(ToolError::Timeout(String::new()));
    }
    let response = tokio::time::timeout_at(deadline, read_line_capped(reader, 64))
        .await
        .map_err(|_| ToolError::Timeout(String::new()))??;
    if tokio::time::Instant::now() >= deadline {
        return Err(ToolError::Timeout(String::new()));
    }
    match response.as_slice() {
        b"SELECT_DONE\n" => Ok(()),
        b"SELECT_LIMIT\n" => Err(ToolError::Limit),
        b"SELECT_TIMEOUT\n" => Err(ToolError::Timeout(String::new())),
        _ => Err(ToolError::Path),
    }
}

pub(super) async fn wait<T>(
    deadline: tokio::time::Instant,
    cancellation: &mut Cancellation,
    operation: impl Future<Output = Result<T, ToolError>>,
) -> Result<T, ToolError> {
    let result = tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(ToolError::Operation),
        result = tokio::time::timeout_at(deadline, operation) => result.unwrap_or(Err(ToolError::Timeout(String::new()))),
    };
    if cancellation.is_cancelled() {
        Err(ToolError::Operation)
    } else if tokio::time::Instant::now() >= deadline {
        Err(ToolError::Timeout(String::new()))
    } else {
        result
    }
}

#[cfg(test)]
pub(in crate::tools::guard) mod tests {
    use super::*;
    use crate::tools::selection::Selector;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn transported_deadline_never_restarts_the_action_budget() {
        let action = tokio::time::Instant::now() + Duration::from_millis(25);
        let decoded = decode_deadline(encode_deadline(action).unwrap()).unwrap();
        assert!(decoded <= action.into_std());
        let expired = decode_deadline([0; 8]).unwrap();
        assert!(matches!(
            Selector::new(0, expired).add("", b"ignored"),
            Err(ToolError::Timeout(_))
        ));
        assert!(matches!(
            decode_deadline(u64::MAX.to_le_bytes()),
            Err(ToolError::Limit)
        ));
    }

    pub(in crate::tools::guard) async fn protocol_outcomes() {
        for (result, expected) in [
            (Ok(()), "done"),
            (Err(ToolError::Limit), "limit"),
            (Err(ToolError::Timeout(String::new())), "timeout"),
            (Err(ToolError::Path), "rejected"),
        ] {
            let mut bytes = response(&result);
            let result = receive(&mut bytes, tokio::time::Instant::now() + PHASE_TIME).await;
            assert!(match expected {
                "done" => result.is_ok(),
                "limit" => matches!(result, Err(ToolError::Limit)),
                "timeout" => matches!(result, Err(ToolError::Timeout(_))),
                "rejected" => matches!(result, Err(ToolError::Path)),
                _ => unreachable!(),
            });
        }
        for bytes in [b"SELECT_UNKNOWN\n".as_slice(), b"DONE\n"] {
            assert!(matches!(
                receive(&mut &bytes[..], tokio::time::Instant::now() + PHASE_TIME).await,
                Err(ToolError::Path)
            ));
        }
        let oversized = [b'x'; 65];
        assert!(matches!(
            receive(
                &mut oversized.as_slice(),
                tokio::time::Instant::now() + PHASE_TIME
            )
            .await,
            Err(ToolError::Limit)
        ));
        assert!(matches!(
            receive(
                &mut b"SELECT_DONE\n".as_slice(),
                tokio::time::Instant::now()
            )
            .await,
            Err(ToolError::Timeout(_))
        ));
        let (peer, stream) = tokio::io::duplex(64);
        let mut stream = tokio::io::BufReader::new(stream);
        let deadline = tokio::time::Instant::now();
        assert!(matches!(
            receive(&mut stream, deadline).await,
            Err(ToolError::Timeout(_))
        ));
        drop(peer);
        assert!(matches!(
            receive(&mut stream, tokio::time::Instant::now() + PHASE_TIME).await,
            Err(ToolError::Operation)
        ));
    }

    #[tokio::test]
    async fn selection_cancellation_drains_inflight_compilation_and_matching() {
        for phase in ["compile", "match"] {
            let mut cancellation = Cancellation::new();
            let worker_cancel = cancellation.clone();
            let (started, ready) = tokio::sync::oneshot::channel();
            let worker = tokio::task::spawn_blocking(move || {
                let mut started = Some(started);
                let mut calls = 0;
                let deadline = Instant::now() + PHASE_TIME;
                let mut selector = Selector::new(0, deadline);
                selector
                    .add(
                        "",
                        b"**/**/**/**/*a*a*a*a*a*a*a*a\n{a,b}{a,b}{a,b}{a,b}[!0-9][a-z]",
                    )
                    .unwrap();
                while !worker_cancel.is_cancelled() && Instant::now() < deadline {
                    if phase == "compile" {
                        selector = Selector::new(0, deadline);
                        selector.add("", b"**/**/**/**/*a*a*a*a*a*a*a*a").unwrap();
                    } else {
                        selector.excluded("nested/aaaaaaaa", false).unwrap();
                    }
                    calls += 1;
                    if let Some(started) = started.take() {
                        let _ = started.send(());
                    }
                }
                calls
            });
            ready.await.unwrap();
            let (mut peer, stream) = tokio::io::duplex(64);
            peer.write_all(b"SELECT_").await.unwrap();
            let mut stream = tokio::io::BufReader::new(stream);
            let cancel = cancellation.clone();
            let operation = async {
                cancel.cancel();
                receive(&mut stream, tokio::time::Instant::now() + PHASE_TIME).await
            };
            let result = wait(
                tokio::time::Instant::now() + PHASE_TIME,
                &mut cancellation,
                operation,
            )
            .await;
            assert!(matches!(result, Err(ToolError::Operation)));
            assert!(worker.await.unwrap() > 0);
            assert!(cancellation.is_cancelled());
        }
    }
}
