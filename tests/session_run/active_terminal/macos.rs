use super::loopback::{ChildGuard, check_profile, read_request, send_response, write_profile};
use arany::{RunStatus, SessionId, SessionView, StateRoot, Store};
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process, waitid};
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

fn own_terminal(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // The child has an admitted PTY on fd 0; only async-signal-safe syscalls run before exec.
    unsafe {
        command.pre_exec(|| {
            if nix::libc::setsid() == -1
                || nix::libc::ioctl(0, nix::libc::TIOCSCTTY.into(), 0) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[derive(Clone, Copy, Debug)]
enum Exit {
    Term,
    Hangup,
    Cancel,
    Suspend,
    Failure,
}

fn restored(input: &std::fs::File, before: &nix::sys::termios::Termios) {
    assert_eq!(
        crate::process::terminal_settings(input),
        *before,
        "native terminal settings restoration"
    );
}

#[test]
fn native_active_terminal_restores_signals_cancellation_and_failed_calls() {
    for linear in [true, false] {
        for exit in [
            Exit::Term,
            Exit::Hangup,
            Exit::Cancel,
            Exit::Suspend,
            Exit::Failure,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let workspace = temp.path().join("project");
            let state = temp.path().join("state");
            std::fs::create_dir(&workspace).unwrap();
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            write_profile(&state, listener.local_addr().unwrap().port());
            let (ready_tx, ready) = mpsc::sync_channel(1);
            let (release_tx, release) = mpsc::sync_channel(1);
            let peer = thread::spawn(move || {
                listener.set_nonblocking(true).unwrap();
                for index in 0..4 {
                    let deadline = Instant::now() + Duration::from_secs(15);
                    let mut stream = loop {
                        match listener.accept() {
                            Ok((stream, _)) => break stream,
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                assert!(
                                    Instant::now() < deadline,
                                    "native request {index} missing"
                                );
                                thread::yield_now();
                            }
                            Err(error) => panic!("native peer accept: {error}"),
                        }
                    };
                    stream.set_nonblocking(false).unwrap();
                    let body = read_request(&mut stream);
                    let wire: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(wire["model"], "model-1");
                    assert_eq!(wire["text"]["format"]["strict"], true);
                    let input: serde_json::Value =
                        serde_json::from_str(wire["input"].as_str().unwrap()).unwrap();
                    if index == 3 {
                        assert_eq!(input["objective"], "cancel");
                        assert_eq!(input["phase"], "root_plan");
                        assert_eq!(input["history"], serde_json::json!([]));
                        assert_eq!(input["includes"], serde_json::json!([]));
                        assert!(input["workspace_guidance"].is_null());
                        assert!(input["tools"].is_null());
                        ready_tx.send(()).unwrap();
                        release.recv_timeout(Duration::from_secs(10)).unwrap();
                        if matches!(exit, Exit::Failure) {
                            stream.write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                        } else {
                            stream
                                .set_read_timeout(Some(Duration::from_secs(5)))
                                .unwrap();
                            assert_eq!(
                                stream.read(&mut [0]).unwrap(),
                                0,
                                "cancelled native request must close"
                            );
                        }
                        return;
                    }
                    assert!(input["workspace_guidance"].is_null());
                    let outcome = match index {
                        0 => {
                            serde_json::json!({"outcome":{"type":"finish","summary":"ok","result":"ok"}})
                        }
                        1 => {
                            serde_json::json!({"outcome":{"type":"delegate","children":["synthetic child"]}})
                        }
                        _ => serde_json::json!({"summary":"synthetic summary"}),
                    };
                    send_response(&mut stream, index, outcome);
                }
            });
            check_profile(&workspace, &state);
            let geometry = nix::pty::Winsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            let pty = nix::pty::openpty(Some(&geometry), None).unwrap();
            let slave = std::fs::File::from(pty.slave);
            let master = std::fs::File::from(pty.master);
            for descriptor in [&slave, &master] {
                rustix::io::fcntl_setfd(descriptor, rustix::io::FdFlags::CLOEXEC).unwrap();
            }
            let before = crate::process::terminal_settings(&slave);
            let mut input = master.try_clone().unwrap();
            let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
            command
                .env_clear()
                .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account"))
                .env("HOME", temp.path().join("legacy-home"))
                .env("XDG_STATE_HOME", temp.path().join("legacy-state"))
                .env("XDG_DATA_HOME", temp.path().join("legacy-data"))
                .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
                .env("TERM", if linear { "dumb" } else { "xterm" })
                .current_dir(&workspace)
                .args(["--no-color", "--state-dir"])
                .arg(&state)
                .arg("--workspace")
                .arg(&workspace)
                .args(["--provider", "custom:local", "--model", "model-1", "cancel"])
                .stdin(Stdio::from(slave.try_clone().unwrap()))
                .stdout(Stdio::piped())
                .stderr(Stdio::from(slave));
            if linear {
                command.arg("--screen-reader");
            }
            own_terminal(&mut command);
            let mut child = ChildGuard::new(command.spawn().unwrap());
            drop(command);
            let pid = Pid::from_raw(child.child().id().try_into().unwrap()).unwrap();
            let mut queries = 0;
            let mut declined = false;
            let mut released = false;
            let mut suspended = false;
            let mut exited = false;
            let output = crate::process::capture_terminal(
                child.child(),
                master,
                Duration::from_secs(20),
                512 * 1024,
                |bytes| {
                    let count = bytes.windows(4).filter(|part| *part == b"\x1b[6n").count();
                    assert!(count <= 8, "native cursor-query bound");
                    for _ in queries..count {
                        input.write_all(b"\x1b[1;1R").unwrap();
                    }
                    queries = count;
                    crate::process::decline_workspace_consent(&mut input, bytes, &mut declined);
                    if !released && ready.try_recv().is_ok() {
                        match exit {
                            Exit::Term => kill_process(pid, Signal::TERM).unwrap(),
                            Exit::Hangup => kill_process(pid, Signal::HUP).unwrap(),
                            Exit::Cancel => kill_process(pid, Signal::INT).unwrap(),
                            Exit::Suspend => kill_process(pid, Signal::TSTP).unwrap(),
                            Exit::Failure => {}
                        }
                        released = true;
                        if !matches!(exit, Exit::Suspend) {
                            release_tx.send(()).unwrap();
                        }
                    }
                    if released
                        && matches!(exit, Exit::Suspend)
                        && !suspended
                        && let Some(status) = waitid(
                            WaitId::Pid(pid),
                            WaitIdOptions::STOPPED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
                        )
                        .unwrap()
                        && status.stopped()
                    {
                        restored(&input, &before);
                        kill_process(pid, Signal::CONT).unwrap();
                        kill_process(pid, Signal::TERM).unwrap();
                        release_tx.send(()).unwrap();
                        suspended = true;
                    }
                    let expected = if matches!(exit, Exit::Failure) {
                        b"Status: failed".as_slice()
                    } else {
                        b"Status: cancelled".as_slice()
                    };
                    if released
                        && !exited
                        && matches!(exit, Exit::Cancel | Exit::Failure)
                        && (if linear {
                            bytes
                                .windows(b"Input:".len())
                                .enumerate()
                                .any(|(offset, part)| {
                                    part == b"Input:"
                                        && bytes[..offset]
                                            .windows(expected.len())
                                            .any(|part| part == expected)
                                })
                        } else {
                            bytes.windows(expected.len()).any(|part| part == expected)
                        })
                    {
                        kill_process(pid, Signal::TERM).unwrap();
                        exited = true;
                    }
                },
            );
            assert!(
                released && declined,
                "native {linear:?} {exit:?} admission was not exercised: {:?}",
                String::from_utf8_lossy(&output.stderr[output.stderr.len().saturating_sub(4096)..])
            );
            peer.join().unwrap();
            assert_eq!(
                output.status.code(),
                Some(1),
                "native {linear:?} {exit:?} signal exit: {:?}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                output.stdout.is_empty(),
                "cancel/failure must emit no answer"
            );
            restored(&input, &before);
            assert_eq!(suspended, matches!(exit, Exit::Suspend));
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let sessions = arany::list_sessions(
                    StateRoot::open_existing(&state).unwrap(),
                    workspace.clone(),
                )
                .await
                .unwrap();
                assert_eq!(sessions.len(), 1);
                let id: SessionId = sessions[0].id;
                let store =
                    Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
                let events = store.load_session(id).await.unwrap();
                store.close().await.unwrap();
                let view = SessionView::replay(id, &events).unwrap().unwrap();
                assert_eq!(view.runs.len(), 1);
                assert_eq!(
                    view.runs[0].status,
                    if matches!(exit, Exit::Failure) {
                        RunStatus::Failed
                    } else {
                        RunStatus::Cancelled
                    }
                );
                assert!(view.runs[0].assistant_message.is_none());
            });
        }
    }
}
