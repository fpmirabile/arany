use arany::{Engine, EngineConfig, StateRoot, Telemetry};
use arany_cli::protocol::rpc::*;
use clap::Args;
use server::{ClientMessage, ServerMessage};
use std::{io::IsTerminal, path::PathBuf};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{mpsc, watch},
};

mod items;
mod server;
mod threads;

pub(super) const INBOUND_CAPACITY: usize = 64;
pub(super) const OUTBOUND_CAPACITY: usize = 256;

#[derive(Args)]
pub(crate) struct ServeArgs {
    #[arg(long)]
    pub stdio: bool,
    #[arg(long)]
    pub state_dir: Option<PathBuf>,
}

pub(crate) fn admit(args: &ServeArgs) -> Result<(), &'static str> {
    if !args.stdio {
        return Err("unsupported transport; pass --stdio");
    }
    if std::io::stdin().is_terminal() || std::io::stdout().is_terminal() {
        return Err("serve requires piped stdin and stdout");
    }
    Ok(())
}

pub(crate) async fn run(
    args: ServeArgs,
    telemetry: Telemetry,
) -> Result<super::CommandOutput, String> {
    let state_dir = super::state_dir(args.state_dir).map_err(str::to_owned)?;
    let state = StateRoot::admit(&state_dir)
        .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
    arany::enable_development_diagnostics(&state);
    let mut signals = super::exec::StopSignals::install()
        .map_err(|_| "signal handling unavailable".to_owned())?;
    let engine = Engine::open(
        state,
        EngineConfig {
            telemetry,
            guard_executable: std::env::current_exe().ok(),
        },
    )
    .map_err(|_| "state store unavailable".to_owned())?;
    let (inbound, receiver) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, writer) = mpsc::channel(OUTBOUND_CAPACITY);
    let input = std::fs::File::from(
        std::os::fd::AsFd::as_fd(&std::io::stdin())
            .try_clone_to_owned()
            .map_err(|_| "protocol input unavailable".to_owned())?,
    );
    let output = std::fs::File::from(
        std::os::fd::AsFd::as_fd(&std::io::stdout())
            .try_clone_to_owned()
            .map_err(|_| "protocol output unavailable".to_owned())?,
    );
    let mut reader_task = tokio::spawn(read_messages(
        tokio::fs::File::from_std(input),
        inbound.clone(),
        outbound.clone(),
    ));
    let mut writer_task = tokio::spawn(write_messages(tokio::fs::File::from_std(output), writer));
    let mut reader_done = false;
    let mut writer_done = false;
    let (shutdown, shutdown_rx) = watch::channel(false);
    let result = shutdown_grace(shutdown_rx, async {
        let mut result = {
            let serving = server::run(engine, state_dir, receiver, outbound.clone(), shutdown.clone());
            tokio::pin!(serving);
            tokio::select! {
                result = &mut serving => {
                    if result.is_ok() {
                        let read = (&mut reader_task).await
                            .map_err(|_| "protocol reader failed".to_owned()).and_then(|r| r);
                        reader_done = true;
                        read.and(result)
                    } else {
                        result
                    }
                }
                _ = signals.next() => {
                    shutdown.send_replace(true);
                    let _ = inbound.send(ClientMessage::Eof).await;
                    serving.await
                }
                result = &mut reader_task => {
                    reader_done = true;
                    shutdown.send_replace(true);
                    // The reader queues its own Eof before it finishes, including after an error.
                    let read = result.map_err(|_| "protocol reader failed".to_owned()).and_then(|r| r);
                    let stopped = serving.await;
                    read.and(stopped)
                }
                result = &mut writer_task => {
                    writer_done = true;
                    shutdown.send_replace(true);
                    let written = result.map_err(|_| "protocol writer failed".to_owned()).and_then(|r| r);
                    let _ = inbound.send(ClientMessage::Eof).await;
                    let stopped = serving.await;
                    written.and(stopped)
                }
            }
        };
        shutdown.send_replace(true);
        if !reader_done {
            if reader_task.is_finished() {
                let read = (&mut reader_task).await
                    .map_err(|_| "protocol reader failed".to_owned()).and_then(|r| r);
                result = read.and(result);
            } else {
                reader_task.abort();
                let _ = (&mut reader_task).await;
            }
            reader_done = true;
        }
        drop(outbound);
        if !writer_done {
            let written = (&mut writer_task).await;
            writer_done = true;
            written.map_err(|_| "protocol writer failed".to_owned())??;
        }
        result
    }).await;
    if !reader_done {
        reader_task.abort();
        let _ = reader_task.await;
    }
    if !writer_done {
        writer_task.abort();
        let _ = writer_task.await;
    }
    result?;
    Ok(super::CommandOutput {
        stdout: String::new(),
        stderr: String::new(),
        success: true,
    })
}

async fn shutdown_grace(
    mut shutdown: watch::Receiver<bool>,
    serving: impl std::future::Future<Output = Result<(), String>>,
) -> Result<(), String> {
    tokio::pin!(serving);
    tokio::select! { biased;
        result = &mut serving => result,
        _ = async {
            // Release the watch read guard before cleanup sends another shutdown notification.
            let _ = shutdown.wait_for(|stopping| *stopping).await;
        } => {
            tokio::time::timeout(super::exec::SHUTDOWN_GRACE, serving)
                .await
                .unwrap_or_else(|_| Err("shutdown timed out; Run may be interrupted; check Session history".into()))
        }
    }
}

async fn read_messages(
    input: impl AsyncRead + Unpin,
    inbound: mpsc::Sender<ClientMessage>,
    outbound: mpsc::Sender<ServerMessage>,
) -> Result<(), String> {
    let mut input = BufReader::with_capacity(64 * 1024, input);
    let result = async {
        while let Some(line) = read_bounded_line(&mut input)
            .await
            .map_err(|_| "protocol input failed".to_owned())?
        {
            match line.and_then(|line| decode_line(&line)) {
                Ok(message) => {
                    if inbound.send(ClientMessage::Message(message)).await.is_err() {
                        return Ok(());
                    }
                }
                Err(error) => {
                    let response = Message::Error(ErrorResponse {
                        jsonrpc: Default::default(),
                        id: None,
                        error: ErrorObject {
                            code: if error == CodecError::TooLarge {
                                -32600
                            } else {
                                -32700
                            },
                            message: error.to_string(),
                            data: (error == CodecError::TooLarge).then_some(ErrorData {
                                reason: Reason::MessageTooLarge,
                            }),
                        },
                    });
                    server::send(&outbound, response).await?;
                }
            }
        }
        Ok(())
    }
    .await;
    let _ = inbound.send(ClientMessage::Eof).await;
    result
}

async fn read_bounded_line(
    input: &mut (impl AsyncBufRead + Unpin),
) -> std::io::Result<Option<Result<Vec<u8>, CodecError>>> {
    let mut line = Vec::new();
    let mut oversized = false;
    loop {
        let buffer = input.fill_buf().await?;
        if buffer.is_empty() {
            return Ok(if oversized {
                Some(Err(CodecError::TooLarge))
            } else if line.is_empty() {
                None
            } else {
                Some(Ok(line))
            });
        }
        let end = buffer.iter().position(|byte| *byte == b'\n').map(|n| n + 1);
        let count = end.unwrap_or(buffer.len());
        if !oversized {
            if count > MAX_LINE_BYTES - line.len() {
                oversized = true;
                line.clear();
            } else {
                line.extend_from_slice(&buffer[..count]);
            }
        }
        input.consume(count);
        if end.is_some() {
            return Ok(Some(if oversized {
                Err(CodecError::TooLarge)
            } else {
                Ok(line)
            }));
        }
    }
}

async fn write_messages(
    mut output: impl AsyncWrite + Unpin,
    mut outbound: mpsc::Receiver<ServerMessage>,
) -> Result<(), String> {
    while let Some(message) = outbound.recv().await {
        let line =
            encode_line(&message).map_err(|_| "protocol output exceeds its bound".to_owned())?;
        output
            .write_all(&line)
            .await
            .map_err(|_| "protocol output failed".to_owned())?;
        output
            .flush()
            .await
            .map_err(|_| "protocol output failed".to_owned())?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/serve/in_process.rs"]
mod tests;
