use super::{Cancellation, ToolError, read_line_capped};
use crate::tools::selection::PHASE_TIME;
use crate::tools::{EffectIntent, ToolCall, ToolLimits, ToolResourceClass};
use std::time::{Duration, Instant};

pub(super) fn primary_command(intent: &EffectIntent) -> bool {
    matches!(intent.call, ToolCall::Command { .. })
        && intent.target == crate::session::worker::EffectTarget::Workspace
}

pub(super) fn selection_limits(limits: &ToolLimits) -> ToolLimits {
    let defaults = ToolLimits::default();
    let mut limits = limits.clone();
    limits.memory_bytes = limits.memory_bytes.min(defaults.memory_bytes);
    limits.max_processes = limits.max_processes.min(defaults.max_processes);
    limits.resource_class = ToolResourceClass::Default;
    limits
}

const CAPTURE_REPORT_TIME: Duration = Duration::from_secs(1);

pub(super) fn capture_deadlines(
    completion: tokio::time::Instant,
) -> Result<(tokio::time::Instant, tokio::time::Instant), ToolError> {
    let now = tokio::time::Instant::now();
    let capture = completion
        .checked_sub(CAPTURE_REPORT_TIME)
        .filter(|deadline| *deadline > now)
        .ok_or_else(|| ToolError::Timeout(String::new()))?
        .min(now + PHASE_TIME);
    Ok((capture, capture + CAPTURE_REPORT_TIME))
}

fn monotonic_nanos() -> Result<u64, ToolError> {
    #[cfg(target_os = "linux")]
    let (seconds, nanos) = {
        let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        (time.tv_sec, time.tv_nsec)
    };
    #[cfg(target_os = "macos")]
    let (seconds, nanos) = {
        // Match std::Instant's unslewed mach_absolute_time clock, including its resolution.
        let time = nix::time::ClockId::from_raw(nix::libc::CLOCK_UPTIME_RAW)
            .now()
            .map_err(|_| ToolError::Operation)?;
        (time.tv_sec(), time.tv_nsec())
    };
    u64::try_from(seconds)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1_000_000_000))
        .and_then(|seconds| seconds.checked_add(u64::try_from(nanos).ok()?))
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
        Err(ToolError::ChangedInput) => b"SELECT_CHANGED\n",
        Err(ToolError::Configuration) => b"SELECT_CONFIGURATION\n",
        Err(ToolError::Operation) => b"SELECT_OPERATION\n",
        Err(ToolError::NotFound) => b"SELECT_NOT_FOUND\n",
        Err(ToolError::FilesystemDenied) => b"SELECT_FILESYSTEM_DENIED\n",
        Err(
            ToolError::MissingConfiguration
            | ToolError::HostPrefixConflict { .. }
            | ToolError::InactiveHostArchiveUnconfirmed
            | ToolError::InactiveHostArchiveConflict
            | ToolError::AccountBusy
            | ToolError::ProtectionUnavailable
            | ToolError::QueueFull
            | ToolError::Path
            | ToolError::AtPath { .. }
            | ToolError::OutsideWorkspace
            | ToolError::ProtectedPath
            | ToolError::SymbolicLink
            | ToolError::FilesystemBoundary
            | ToolError::NotDirectory
            | ToolError::UnterminatedQuotedPath
            | ToolError::OutputCeiling
            | ToolError::Conflict
            | ToolError::Pattern(_)
            | ToolError::Uncertain
            | ToolError::StaleGitLocks(_)
            | ToolError::GuardRejected(_)
            | ToolError::GuardBootstrap(_)
            | ToolError::HostHelperSearch
            | ToolError::CredentialIncludes,
        ) => b"SELECT_REJECTED\n",
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
        b"SELECT_CHANGED\n" => Err(ToolError::ChangedInput),
        b"SELECT_CONFIGURATION\n" => Err(ToolError::Configuration),
        b"SELECT_OPERATION\n" => Err(ToolError::Operation),
        b"SELECT_NOT_FOUND\n" => Err(ToolError::NotFound),
        b"SELECT_FILESYSTEM_DENIED\n" => Err(ToolError::FilesystemDenied),
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

    #[tokio::test(start_paused = true)]
    async fn capture_reserves_bounded_report_time_inside_the_action() {
        for remaining in [Duration::from_secs(30), Duration::from_secs(3)] {
            let now = tokio::time::Instant::now();
            let completion = now + remaining;
            let (capture, report) = capture_deadlines(completion).unwrap();
            assert_eq!(
                capture,
                (completion - CAPTURE_REPORT_TIME).min(now + PHASE_TIME)
            );
            assert_eq!(report - capture, CAPTURE_REPORT_TIME);
            assert!(report <= completion);
            tokio::time::advance(capture - now).await;
            let (mut peer, stream) = tokio::io::duplex(64);
            let mut stream = tokio::io::BufReader::new(stream);
            let delivery = async {
                tokio::time::sleep(CAPTURE_REPORT_TIME / 2).await;
                peer.write_all(b"CAPTURE_DONE\n").await.unwrap();
            };
            let (result, ()) = tokio::join!(
                tokio::time::timeout_at(report, read_line_capped(&mut stream, 64)),
                delivery
            );
            assert_eq!(result.unwrap().unwrap(), b"CAPTURE_DONE\n");
        }
        for remaining in [Duration::ZERO, CAPTURE_REPORT_TIME / 2, CAPTURE_REPORT_TIME] {
            assert!(matches!(
                capture_deadlines(tokio::time::Instant::now() + remaining),
                Err(ToolError::Timeout(_))
            ));
        }
    }

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
            (Err(ToolError::Path.at_path("nested/file")), "rejected"),
            (Err(ToolError::OutsideWorkspace), "rejected"),
            (Err(ToolError::ProtectedPath), "rejected"),
            (Err(ToolError::SymbolicLink), "rejected"),
            (Err(ToolError::FilesystemBoundary), "rejected"),
            (Err(ToolError::NotDirectory), "rejected"),
            (Err(ToolError::UnterminatedQuotedPath), "rejected"),
            (Err(ToolError::ChangedInput), "changed"),
            (Err(ToolError::Configuration), "configuration"),
            (Err(ToolError::Operation), "operation"),
            (Err(ToolError::NotFound), "not-found"),
            (Err(ToolError::FilesystemDenied), "filesystem-denied"),
        ] {
            let mut bytes = response(&result);
            if expected == "rejected" {
                assert_eq!(bytes, b"SELECT_REJECTED\n");
            }
            let result = receive(&mut bytes, tokio::time::Instant::now() + PHASE_TIME).await;
            assert!(match expected {
                "done" => result.is_ok(),
                "limit" => matches!(result, Err(ToolError::Limit)),
                "timeout" => matches!(result, Err(ToolError::Timeout(_))),
                "rejected" => matches!(result, Err(ToolError::Path)),
                "changed" => matches!(result, Err(ToolError::ChangedInput)),
                "configuration" => matches!(result, Err(ToolError::Configuration)),
                "operation" => matches!(result, Err(ToolError::Operation)),
                "not-found" => matches!(result, Err(ToolError::NotFound)),
                "filesystem-denied" => matches!(result, Err(ToolError::FilesystemDenied)),
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
            let (stop, stopped) = std::sync::mpsc::sync_channel(1);
            let worker = tokio::task::spawn_blocking(move || {
                let deadline = Instant::now() + PHASE_TIME;
                let mut selector = Selector::new(0, deadline);
                if phase == "match" {
                    match selector.add("", b"**/**/**/**/*a*a*a*a*a*a*a*a") {
                        Ok(()) | Err(ToolError::Timeout(_)) => {}
                        Err(error) => panic!("unexpected matcher setup failure: {error}"),
                    }
                }
                selector.in_progress = Some(Box::new(move || {
                    started.send(()).unwrap();
                    stopped.recv_timeout(Duration::from_secs(30)).unwrap();
                    assert!(worker_cancel.is_cancelled());
                }));
                if phase == "compile" {
                    selector.add(
                        "",
                        b"**/**/**/**/*a*a*a*a*a*a*a*a\n{a,b}{a,b}{a,b}{a,b}[!0-9][a-z]",
                    )
                } else {
                    selector.excluded("nested/aaaaaaaa", false).map(|_| ())
                }
            });
            ready.await.unwrap();
            assert!(
                !worker.is_finished(),
                "{phase} call must still be in progress"
            );
            let (mut peer, stream) = tokio::io::duplex(64);
            peer.write_all(b"SELECT_").await.unwrap();
            let mut stream = tokio::io::BufReader::new(stream);
            let cancel = cancellation.clone();
            let operation = async {
                cancel.cancel();
                stop.send(()).unwrap();
                receive(&mut stream, tokio::time::Instant::now() + PHASE_TIME).await
            };
            let result = wait(
                tokio::time::Instant::now() + PHASE_TIME,
                &mut cancellation,
                operation,
            )
            .await;
            assert!(matches!(result, Err(ToolError::Operation)));
            match worker.await.unwrap() {
                Ok(()) | Err(ToolError::Timeout(_)) => {}
                Err(error) => panic!("unexpected {phase} failure: {error}"),
            }
            assert!(cancellation.is_cancelled());
        }
    }
}
