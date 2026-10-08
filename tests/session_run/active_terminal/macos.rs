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
    PtyLoss,
    BrokenStderr,
}

fn restored(input: &std::fs::File, before: &nix::sys::termios::Termios) {
    assert_eq!(
        crate::process::terminal_settings(input),
        *before,
        "native terminal settings restoration"
    );
}

#[test]
fn native_folder_consent_restores_before_account_setup_or_session_work() {
    for linear in [true, false] {
        for case in [
            "input",
            "INT",
            "TERM",
            "HUP",
            "stderr-before",
            "stderr-after",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let workspace = temp.path().join("project");
            let state = temp.path().join("state");
            std::fs::create_dir(&workspace).unwrap();
            let geometry = nix::pty::Winsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            let pty = nix::pty::openpty(Some(&geometry), None).unwrap();
            let slave = std::fs::File::from(pty.slave);
            let mut input = std::fs::File::from(pty.master);
            let before = crate::process::terminal_settings(&input);
            let broken = case.starts_with("stderr-");
            let mut fault_probe = None;
            let (master, stderr) = if broken {
                let output = nix::pty::openpty(Some(&geometry), None).unwrap();
                let stderr = std::fs::File::from(output.slave);
                fault_probe = Some(stderr.try_clone().unwrap());
                (std::fs::File::from(output.master), stderr)
            } else {
                (input.try_clone().unwrap(), slave.try_clone().unwrap())
            };
            for descriptor in [&slave, &input, &master, &stderr] {
                rustix::io::fcntl_setfd(descriptor, rustix::io::FdFlags::CLOEXEC).unwrap();
            }
            if let Some(probe) = &fault_probe {
                rustix::io::fcntl_setfd(probe, rustix::io::FdFlags::CLOEXEC).unwrap();
            }
            let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
            command
                .env_clear()
                .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account"))
                .env("HOME", temp.path().join("legacy-home"))
                .env("XDG_STATE_HOME", temp.path().join("legacy-state"))
                .env("XDG_DATA_HOME", temp.path().join("legacy-data"))
                .env("TERM", if linear { "dumb" } else { "xterm" })
                .current_dir(&workspace)
                .args(["--no-color", "--state-dir"])
                .arg(&state)
                .arg("--workspace")
                .arg(&workspace)
                .stdin(Stdio::from(slave))
                .stdout(Stdio::piped())
                .stderr(Stdio::from(stderr));
            if linear {
                command.arg("--screen-reader");
            }
            own_terminal(&mut command);
            let mut master = Some(master);
            if case == "stderr-before" {
                drop(master.take());
            }
            let mut child = ChildGuard::new(command.spawn().unwrap());
            drop(command);
            let pid = Pid::from_raw(child.child().id().try_into().unwrap()).unwrap();
            let mut transcript = Vec::new();
            if case != "stderr-before" {
                let reader = master.as_mut().unwrap();
                let flags = rustix::fs::fcntl_getfl(&*reader).unwrap();
                rustix::fs::fcntl_setfl(&*reader, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut consent = false;
                let mut suggested_input = Vec::new();
                while !consent {
                    let mut bytes = [0; 4096];
                    match reader.read(&mut bytes) {
                        Ok(0) => panic!("native consent closed before admission: {linear} {case}"),
                        Ok(count) => {
                            assert!(transcript.len() + count <= 64 * 1024);
                            transcript.extend_from_slice(&bytes[..count]);
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                            ) => {}
                        Err(error) => panic!("native consent read: {error}"),
                    }
                    crate::process::decline_workspace_consent(
                        &mut suggested_input,
                        &transcript,
                        &mut consent,
                    );
                    assert!(
                        Instant::now() < deadline,
                        "native consent missing: {linear} {case}"
                    );
                    assert!(child.child().try_wait().unwrap().is_none());
                    thread::yield_now();
                }
                let active = crate::process::terminal_settings(&input);
                assert_eq!(
                    active == before,
                    linear,
                    "native consent settings: {linear} {case}"
                );
                match case {
                    "input" => input.write_all(&[3]).unwrap(),
                    "INT" => kill_process(pid, Signal::INT).unwrap(),
                    "TERM" => kill_process(pid, Signal::TERM).unwrap(),
                    "HUP" => kill_process(pid, Signal::HUP).unwrap(),
                    "stderr-after" => drop(master.take()),
                    _ => unreachable!(),
                }
            }
            if let Some(mut probe) = fault_probe {
                assert_eq!(
                    probe.write(b"x").unwrap_err().raw_os_error(),
                    Some(nix::libc::EIO),
                    "positive native stderr disconnection: {linear} {case}"
                );
                if case == "stderr-after" {
                    kill_process(pid, Signal::TERM).unwrap();
                }
            }
            let output = crate::process::capture_terminal(
                child.child(),
                master.take().unwrap_or_else(|| input.try_clone().unwrap()),
                Duration::from_secs(10),
                64 * 1024,
                |_| {},
            );
            transcript.extend_from_slice(&output.stderr);
            assert_eq!(
                output.status.code(),
                Some(if matches!(case, "input" | "INT") {
                    0
                } else {
                    1
                }),
                "native consent exit: {linear} {case}: {:?} stdout {:?}",
                String::from_utf8_lossy(&transcript),
                String::from_utf8_lossy(&output.stdout)
            );
            assert!(output.stdout.is_empty());
            assert!(!transcript.windows(4).any(|part| part == b"\x1b[6n"));
            restored(&input, &before);
            assert!(!state.exists(), "no Session admission: {linear} {case}");
            assert!(!transcript.windows(6).any(|part| part == b"Setup:"));
            assert_eq!(std::fs::read_dir(&workspace).unwrap().count(), 0);
            for name in ["legacy-home", "legacy-state", "legacy-data"] {
                assert!(!temp.path().join(name).exists());
            }
            let account = temp.path().join("account");
            if account.exists() {
                for entry in std::fs::read_dir(&account).unwrap() {
                    let entry = entry.unwrap();
                    let name = entry.file_name();
                    assert!(
                        name == "account-credentials.lock" || name == "events.sqlite3",
                        "folder trust admission must not create account metadata: {name:?}"
                    );
                    assert_eq!(entry.metadata().unwrap().len(), 0);
                }
            }
        }
    }
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
            Exit::PtyLoss,
            Exit::BrokenStderr,
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
            let terminal_probe = matches!(exit, Exit::PtyLoss).then(|| slave.try_clone().unwrap());
            let mut input = Some(master.try_clone().unwrap());
            let mut input_master = None;
            let (master, stderr) = if matches!(exit, Exit::PtyLoss | Exit::BrokenStderr) {
                if matches!(exit, Exit::PtyLoss) {
                    input_master = Some(master);
                }
                let output = nix::pty::openpty(Some(&geometry), None).unwrap();
                for descriptor in [&output.master, &output.slave] {
                    rustix::io::fcntl_setfd(descriptor, rustix::io::FdFlags::CLOEXEC).unwrap();
                }
                (
                    std::fs::File::from(output.master),
                    std::fs::File::from(output.slave),
                )
            } else {
                (master, slave.try_clone().unwrap())
            };
            let output_master = matches!(exit, Exit::PtyLoss).then(|| master.try_clone().unwrap());
            let stderr_before = crate::process::terminal_settings(&master);
            let stderr_probe =
                matches!(exit, Exit::BrokenStderr).then(|| stderr.try_clone().unwrap());
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
                .stdin(Stdio::from(slave))
                .stdout(Stdio::piped())
                .stderr(Stdio::from(stderr));
            if linear {
                command.arg("--screen-reader");
            }
            own_terminal(&mut command);
            let mut child = ChildGuard::new(command.spawn().unwrap());
            drop(command);
            let pid = Pid::from_raw(child.child().id().try_into().unwrap()).unwrap();
            let mut declined = false;
            let mut released = false;
            let mut suspended = false;
            let mut exited = false;
            let output = if let Some(mut probe) = stderr_probe {
                use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
                let mut master = master;
                let flags = fcntl_getfl(&master).unwrap();
                fcntl_setfl(&master, flags | OFlags::NONBLOCK).unwrap();
                let deadline = Instant::now() + Duration::from_secs(15);
                let mut transcript = Vec::new();
                loop {
                    let mut bytes = [0; 4096];
                    match master.read(&mut bytes) {
                        Ok(0) => panic!("native stderr closed before the fault"),
                        Ok(count) => {
                            assert!(transcript.len() + count <= 512 * 1024);
                            transcript.extend_from_slice(&bytes[..count]);
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                            ) => {}
                        Err(error) => panic!("native stderr read: {error}"),
                    }
                    crate::process::decline_workspace_consent(
                        input.as_mut().unwrap(),
                        &transcript,
                        &mut declined,
                    );
                    if ready.try_recv().is_ok() {
                        break;
                    }
                    assert!(Instant::now() < deadline, "native renderer fault admission");
                    assert!(
                        child.child().try_wait().unwrap().is_none(),
                        "native product exited before fault"
                    );
                    thread::yield_now();
                }
                assert!(!transcript.windows(4).any(|part| part == b"\x1b[6n"));
                drop(master);
                let flags = fcntl_getfl(&probe).unwrap();
                fcntl_setfl(&probe, flags | OFlags::NONBLOCK).unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    match probe.write(b"x") {
                        Err(error) if error.raw_os_error() == Some(nix::libc::EIO) => break,
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                            ) => {}
                        Ok(_) => {}
                        Err(error) => panic!("native stderr fault probe: {error}"),
                    }
                    assert!(Instant::now() < deadline, "native stderr fault missing");
                    thread::yield_now();
                }
                drop(probe);
                input
                    .as_mut()
                    .unwrap()
                    .write_all(if linear { b"x\r" } else { b"x" })
                    .unwrap();
                released = true;
                release_tx.send(()).unwrap();
                let mut output = crate::process::capture_terminal(
                    child.child(),
                    input.as_ref().unwrap().try_clone().unwrap(),
                    Duration::from_secs(20),
                    512 * 1024,
                    |_| {},
                );
                transcript.extend_from_slice(&output.stderr);
                output.stderr = transcript;
                output
            } else {
                crate::process::capture_terminal(
                    child.child(),
                    master,
                    Duration::from_secs(20),
                    512 * 1024,
                    |bytes| {
                        assert!(!bytes.windows(4).any(|part| part == b"\x1b[6n"));
                        if let Some(input) = &mut input {
                            crate::process::decline_workspace_consent(input, bytes, &mut declined);
                        }
                        if !released && ready.try_recv().is_ok() {
                            match exit {
                                Exit::Term => kill_process(pid, Signal::TERM).unwrap(),
                                Exit::Hangup => kill_process(pid, Signal::HUP).unwrap(),
                                Exit::Cancel => kill_process(pid, Signal::INT).unwrap(),
                                Exit::Suspend => kill_process(pid, Signal::TSTP).unwrap(),
                                Exit::Failure => {}
                                Exit::PtyLoss => {
                                    drop(input.take());
                                    drop(input_master.take());
                                }
                                Exit::BrokenStderr => {
                                    unreachable!("separate native output fault owner")
                                }
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
                                WaitIdOptions::STOPPED
                                    | WaitIdOptions::NOHANG
                                    | WaitIdOptions::NOWAIT,
                            )
                            .unwrap()
                            && status.stopped()
                        {
                            restored(input.as_ref().unwrap(), &before);
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
                )
            };
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
            assert!(!output.stderr.windows(4).any(|part| part == b"\x1b[6n"));
            if matches!(exit, Exit::PtyLoss) {
                assert_eq!(
                    nix::sys::termios::tcgetattr(terminal_probe.as_ref().unwrap()).unwrap_err(),
                    nix::errno::Errno::ENOTTY,
                    "the lost controlling terminal must be disconnected"
                );
                restored(output_master.as_ref().unwrap(), &stderr_before);
                drop(output_master);
            } else {
                restored(input.as_ref().unwrap(), &before);
            }
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
