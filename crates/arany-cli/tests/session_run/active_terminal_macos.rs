use super::loopback::{ChildGuard, check_profile, read_request, send_response, write_profile};
use arany::{RecordKind, RunStatus, SessionId, SessionView, StateRoot, Store};
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process, waitid};
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

pub(super) fn own_terminal(command: &mut Command) {
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

fn wait_stderr_disconnection(mut probe: std::fs::File, context: &str) {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

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
            Err(error) => panic!("native stderr fault probe: {context}: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "native stderr fault missing: {context}"
        );
        thread::yield_now();
    }
}

#[test]
fn native_resume_picker_restores_mouse_and_terminal_without_changing_history() {
    for (signal, suspend, columns, rows, fullscreen) in [
        (Signal::TERM, false, 80, 24, true),
        (Signal::HUP, false, 80, 24, true),
        (Signal::TERM, true, 80, 24, true),
        (Signal::TERM, false, 16, 8, true),
        (Signal::TERM, false, 80, 24, false),
        (Signal::TERM, true, 80, 24, false),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("project");
        let state = temp.path().join("state");
        if !fullscreen {
            crate::seed_classic(&state);
        }
        std::fs::create_dir(&workspace).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let id = runtime
            .block_on(arany::create_session(
                StateRoot::admit(&state).unwrap(),
                workspace.clone(),
                Some("Stable".into()),
            ))
            .unwrap();
        let geometry = nix::pty::Winsize {
            ws_row: rows,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let (master, slave) = crate::process::cloexec_pty(Some(&geometry));
        let before = crate::process::terminal_settings(&master);
        let probe = master.try_clone().unwrap();
        let input = master.try_clone().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
        command
            .env_clear()
            .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account"))
            .env("HOME", temp.path().join("fixture-home"))
            .env("XDG_STATE_HOME", temp.path().join("fixture-state"))
            .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
            .env("TERM", "xterm")
            .current_dir(&workspace)
            .args(["--no-color", "--resume", "--state-dir"])
            .arg(&state)
            .arg("--workspace")
            .arg(&workspace)
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::piped())
            .stderr(Stdio::from(slave));
        own_terminal(&mut command);
        // Close fixture PTYs before reaping a child on unwind.
        let (mut child, mut input, probe) =
            (ChildGuard::new(command.spawn().unwrap()), input, probe);
        drop(command);
        let pid = Pid::from_raw(child.child().id().try_into().unwrap()).unwrap();
        let mut declined = false;
        let mut signalled = false;
        let mut suspended = false;
        let mut continued = false;
        let mut resumed_at = 0;
        let mut suspended_at = 0;
        let mut queries = 0;
        let mut keyboard_queries = 0;
        // Fullscreen captures the mouse from acquisition; Classic never enables it.
        let captures = usize::from(fullscreen);
        let output = crate::process::capture_terminal(
            child.child(),
            master,
            Duration::from_secs(20),
            64 * 1024,
            |bytes| {
                if fullscreen {
                    assert!(!bytes.windows(4).any(|part| part == b"\x1b[6n"));
                } else {
                    assert!(bytes.windows(4).filter(|part| *part == b"\x1b[6n").count() <= 128);
                    super::reply_positioned_cursor_queries(&mut input, bytes, &mut queries);
                }
                keyboard::answer_supported_queries(&mut input, bytes, &mut keyboard_queries);
                crate::process::decline_workspace_consent(&mut input, bytes, &mut declined);
                let enabled = bytes
                    .windows(8)
                    .filter(|part| *part == b"\x1b[?1000h")
                    .count();
                let armed_with = |from: usize| {
                    super::armed_selector(bytes, from, columns.into(), rows.into()).is_some_and(
                        |screen| screen.iter().any(|row| row.starts_with("› 1. Stable")),
                    )
                };
                let frame_complete = armed_with(0);
                if !signalled && enabled == captures && frame_complete {
                    assert_ne!(crate::process::terminal_settings(&probe), before);
                    suspended_at = bytes.len();
                    kill_process(pid, if suspend { Signal::TSTP } else { signal }).unwrap();
                    signalled = true;
                }
                if signalled
                    && suspend
                    && !suspended
                    && let Some(status) = waitid(
                        WaitId::Pid(pid),
                        WaitIdOptions::STOPPED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
                    )
                    .unwrap()
                    && status.stopped()
                {
                    restored(&probe, &before);
                    let disabled = bytes
                        .windows(8)
                        .filter(|part| *part == b"\x1b[?1000l")
                        .count();
                    if disabled < captures {
                        return;
                    }
                    assert_eq!(disabled, captures);
                    if fullscreen {
                        let suspension = &bytes[suspended_at..];
                        let Some(screen_release) = suspension
                            .windows(b"\x1b[?1049l".len())
                            .position(|part| part == b"\x1b[?1049l")
                        else {
                            return;
                        };
                        let pop = suspension
                            .windows(b"\x1b[<u".len())
                            .position(|part| part == b"\x1b[<u")
                            .expect("suspend pops keyboard mode");
                        assert!(
                            pop < screen_release,
                            "suspend pops keyboard mode before screen release"
                        );
                    }
                    resumed_at = bytes.len();
                    kill_process(pid, Signal::CONT).unwrap();
                    suspended = true;
                }
                if suspended && !continued && enabled == 2 * captures && armed_with(resumed_at) {
                    assert_ne!(crate::process::terminal_settings(&probe), before);
                    kill_process(pid, signal).unwrap();
                    continued = true;
                }
            },
        );
        assert!(
            declined && signalled,
            "native picker admission at {columns}x{rows}, fullscreen {fullscreen}"
        );
        assert_eq!(suspended, suspend);
        assert_eq!(continued, suspend);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        restored(&probe, &before);
        keyboard::assert_keyboard_restored(&output.stderr, true);
        let count = if suspend { 2 * captures } else { captures };
        for sequence in [b"\x1b[?1000h", b"\x1b[?1000l"] {
            assert_eq!(
                output
                    .stderr
                    .windows(8)
                    .filter(|part| *part == sequence)
                    .count(),
                count,
                "fullscreen {fullscreen}"
            );
        }
        let text = String::from_utf8_lossy(&output.stderr);
        if fullscreen {
            // Exactly button, drag and SGR reporting, each released on SIGTSTP and on exit.
            for mode in [1000, 1002, 1006] {
                for toggle in ['h', 'l'] {
                    assert_eq!(
                        text.matches(&format!("\x1b[?{mode}{toggle}")).count(),
                        count,
                        "Fullscreen mode {mode}{toggle}"
                    );
                }
            }
            for mode in [1003, 1007, 1015] {
                assert!(
                    !text.contains(&format!("\x1b[?{mode}h")),
                    "Fullscreen enabled {mode}"
                );
            }
            let last_disable = text.rfind("\x1b[?1000l").expect("capture released");
            assert!(
                text.rfind("\x1b[?1000h").expect("capture enabled") < last_disable,
                "exit leaves capture released"
            );
        } else {
            for mode in [1049, 1000, 1002, 1003, 1006, 1007, 1015] {
                assert!(
                    !text.contains(&format!("\x1b[?{mode}h")),
                    "Classic picker enabled {mode}"
                );
            }
        }
        runtime.block_on(async {
            let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
            let events = store.load_session(id).await.unwrap();
            store.close().await.unwrap();
            let view = SessionView::replay(id, &events).unwrap().unwrap();
            assert_eq!(
                events.len(),
                1,
                "picker interruption preserves canonical history"
            );
            assert_eq!(view.title, "Stable");
            assert!(view.runs.is_empty());
            let sessions =
                arany::list_sessions(StateRoot::open_existing(&state).unwrap(), workspace)
                    .await
                    .unwrap();
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].id, id);
        });
    }
}

#[test]
fn native_resume_picker_renderer_failure_restores_surviving_terminal() {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

    for (columns, rows, deadline_fault) in [(80, 24, false), (16, 8, false), (80, 24, true)] {
        let mut acquired = false;
        let mut owned_pid = None;
        let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let temp = tempfile::tempdir().unwrap();
            let workspace = temp.path().join("project");
            let state = temp.path().join("state");
            std::fs::create_dir(&workspace).unwrap();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let id = runtime
                .block_on(arany::create_session(
                    StateRoot::admit(&state).unwrap(),
                    workspace.clone(),
                    Some("Stable".into()),
                ))
                .unwrap();
            let geometry = nix::pty::Winsize {
                ws_row: rows,
                ws_col: columns,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            let (input, slave) = crate::process::cloexec_pty(Some(&geometry));
            let (master, stderr) = crate::process::cloexec_pty(Some(&geometry));
            let before = crate::process::terminal_settings(&input);
            let probe = stderr.try_clone().unwrap();
            let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
            command
                .env_clear()
                .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account"))
                .env("HOME", temp.path().join("fixture-home"))
                .env("XDG_STATE_HOME", temp.path().join("fixture-state"))
                .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
                .env("TERM", "xterm")
                .current_dir(&workspace)
                .args(["--no-color", "--resume", "--state-dir"])
                .arg(&state)
                .arg("--workspace")
                .arg(&workspace)
                .stdin(Stdio::from(slave))
                .stdout(Stdio::piped())
                .stderr(Stdio::from(stderr));
            own_terminal(&mut command);
            // Close fixture PTYs before reaping a child on unwind.
            let (mut child, mut master, mut input, probe) = (
                ChildGuard::new(command.spawn().unwrap()),
                master,
                input,
                probe,
            );
            drop(command);
            let flags = fcntl_getfl(&master).unwrap();
            fcntl_setfl(&master, flags | OFlags::NONBLOCK).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut transcript = Vec::new();
            let mut declined = false;
            loop {
                let mut bytes = [0; 4096];
                match master.read(&mut bytes) {
                    Ok(0) => panic!("native picker output closed before renderer fault"),
                    Ok(count) => {
                        assert!(transcript.len() + count <= 64 * 1024);
                        transcript.extend_from_slice(&bytes[..count]);
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => panic!("native picker output: {error}"),
                }
                crate::process::decline_workspace_consent(&mut input, &transcript, &mut declined);
                // The redraw key below acts only once the picker is armed.
                if transcript
                    .windows(b"\x1b[?1000h".len())
                    .any(|part| part == b"\x1b[?1000h")
                    && super::armed_selector(&transcript, 0, columns.into(), rows.into())
                        .is_some_and(|screen| {
                            screen.iter().any(|row| row.starts_with("› 1. Stable"))
                        })
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "native picker renderer fault admission at {columns}x{rows}"
                );
                assert!(child.child().try_wait().unwrap().is_none());
                thread::yield_now();
            }
            assert!(declined);
            assert!(!transcript.windows(4).any(|part| part == b"\x1b[6n"));
            assert_ne!(crate::process::terminal_settings(&input), before);
            acquired = true;
            if deadline_fault {
                let pid = Pid::from_raw(child.child().id().try_into().unwrap()).unwrap();
                owned_pid = Some(pid);
                kill_process(pid, Signal::STOP).unwrap();
                crate::process::capture_terminal(
                    child.child(),
                    master,
                    Duration::ZERO,
                    64 * 1024,
                    |_| {},
                );
                unreachable!("zero-duration capture must fail before collection");
            }
            drop(master);
            wait_stderr_disconnection(probe, "native resume picker renderer");
            input.write_all(b"\x1b[B").unwrap();
            let output = crate::process::capture_terminal(
                child.child(),
                input.try_clone().unwrap(),
                Duration::from_secs(10),
                64 * 1024,
                |_| {},
            );
            assert_eq!(output.status.code(), Some(1));
            assert!(output.stdout.is_empty());
            restored(&input, &before);
            runtime.block_on(async {
                let store =
                    Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
                let events = store.load_session(id).await.unwrap();
                store.close().await.unwrap();
                let view = SessionView::replay(id, &events).unwrap().unwrap();
                assert_eq!(
                    events.len(),
                    1,
                    "picker renderer failure preserves canonical history"
                );
                assert_eq!(view.title, "Stable");
                assert!(view.runs.is_empty());
            });
        }));
        if deadline_fault {
            assert!(acquired, "deadline fault follows native picker acquisition");
            let failure = observation.expect_err("deadline fault must unwind owned native process");
            let message = failure
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| failure.downcast_ref::<String>().map(String::as_str))
                .unwrap();
            assert_eq!(message, "subprocess exit/output EOF deadline exceeded");
            assert_eq!(
                waitid(
                    WaitId::Pid(owned_pid.unwrap()),
                    WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
                )
                .unwrap_err(),
                rustix::io::Errno::CHILD,
                "deadline unwind must reap its exact child"
            );
        } else if let Err(failure) = observation {
            std::panic::resume_unwind(failure);
        }
    }
}

#[test]
fn native_partial_acquisition_restores_raw_mode_when_initial_output_disconnects() {
    use rustix::fs::{Mode, OFlags, open};

    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    let state = temp.path().join("state");
    let account = temp.path().join("account");
    std::fs::create_dir(&workspace).unwrap();
    let geometry = nix::pty::Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let (input, slave) = crate::process::cloexec_pty(Some(&geometry));
    let (master, stderr) = crate::process::cloexec_pty(Some(&geometry));
    let before = crate::process::terminal_settings(&input);
    let probe = stderr.try_clone().unwrap();
    let name = rustix::termios::ttyname(&stderr, Vec::new()).unwrap();
    let mut filler = std::fs::File::from(
        open(
            name,
            OFlags::WRONLY | OFlags::NOCTTY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut filled = 0;
    loop {
        match filler.write(&[b'x'; 1024]) {
            Ok(count) => {
                assert!(count > 0);
                filled += count;
                assert!(filled <= 1024 * 1024, "bounded private PTY backpressure");
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => panic!("native partial-acquisition backpressure: {error}"),
        }
        assert!(Instant::now() < deadline, "private output PTY did not fill");
    }
    assert!(filled > 0);
    drop(filler);
    assert!(
        !rustix::fs::fcntl_getfl(&stderr)
            .unwrap()
            .contains(OFlags::NONBLOCK)
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
    command
        .env_clear()
        .env("ARANY_TEST_ACCOUNT_ROOT", &account)
        .env("HOME", temp.path().join("fixture-home"))
        .env("XDG_STATE_HOME", temp.path().join("fixture-state"))
        .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
        .env("TERM", "xterm")
        .current_dir(&workspace)
        .args(["--no-color", "--state-dir"])
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .stdin(Stdio::from(slave))
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr));
    own_terminal(&mut command);
    // Close fixture PTYs before reaping a child on unwind.
    let (mut child, input, probe, master) = (
        ChildGuard::new(command.spawn().unwrap()),
        input,
        probe,
        master,
    );
    drop(command);
    let deadline = Instant::now() + Duration::from_secs(10);
    while crate::process::terminal_settings(&input) == before {
        assert!(child.child().try_wait().unwrap().is_none());
        assert!(
            Instant::now() < deadline,
            "native initial raw acquisition missing"
        );
        thread::yield_now();
    }
    assert!(!state.exists());
    assert!(!account.exists());
    drop(master);
    wait_stderr_disconnection(probe, "native partial acquisition");
    let output = crate::process::capture_terminal(
        child.child(),
        input.try_clone().unwrap(),
        Duration::from_secs(10),
        16 * 1024,
        |_| {},
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    restored(&input, &before);
    assert!(
        !state.exists(),
        "partial acquisition creates no Session state"
    );
    assert!(
        !account.exists(),
        "partial acquisition precedes account admission"
    );
}

fn read_until_consent(
    reader: &mut std::fs::File,
    transcript: &mut Vec<u8>,
    child: &mut ChildGuard,
    context: &str,
) {
    let flags = rustix::fs::fcntl_getfl(&*reader).unwrap();
    rustix::fs::fcntl_setfl(&*reader, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
    let started = Instant::now();
    let deadline = started + Duration::from_secs(20);
    let mut first_byte = None;
    let mut consent = false;
    let mut suggested_input = Vec::new();
    while !consent {
        let mut bytes = [0; 4096];
        match reader.read(&mut bytes) {
            Ok(0) => panic!(
                "{}",
                crate::process::terminal_wait_report(
                    &format!("native consent closed before admission: {context}"),
                    started,
                    first_byte,
                    transcript,
                    child.child().try_wait().unwrap(),
                )
            ),
            Ok(count) => {
                assert!(transcript.len() + count <= 64 * 1024);
                first_byte.get_or_insert_with(Instant::now);
                transcript.extend_from_slice(&bytes[..count]);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => panic!("native consent read: {error}"),
        }
        crate::process::decline_workspace_consent(&mut suggested_input, transcript, &mut consent);
        let status = child.child().try_wait().unwrap();
        if Instant::now() >= deadline || status.is_some() {
            panic!(
                "{}",
                crate::process::terminal_wait_report(
                    &format!("native consent missing: {context}"),
                    started,
                    first_byte,
                    transcript,
                    status,
                )
            );
        }
        crate::process::wait_readable(&*reader);
    }
}

#[test]
fn native_startup_chat_view_honors_default_saved_and_flag() {
    for (saved, classic) in [(None, false), (Some("classic"), false), (None, true)] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("project");
        let state = temp.path().join("state");
        std::fs::create_dir(&workspace).unwrap();
        if let Some(view) = saved {
            StateRoot::admit(&state)
                .unwrap()
                .record(RecordKind::UiPreferences)
                .replace(&super::saved_ui_preferences("tabs", view))
                .unwrap();
        }
        let fullscreen = saved.is_none() && !classic;
        let geometry = nix::pty::Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let (input, slave) = crate::process::cloexec_pty(Some(&geometry));
        let before = crate::process::terminal_settings(&input);
        let master = input.try_clone().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
        command
            .env_clear()
            .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account"))
            .env("HOME", temp.path().join("fixture-home"))
            .env("XDG_STATE_HOME", temp.path().join("fixture-state"))
            .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
            .env("TERM", "xterm")
            .current_dir(&workspace)
            .args(["--no-color", "--state-dir"])
            .arg(&state)
            .arg("--workspace")
            .arg(&workspace)
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::piped())
            .stderr(Stdio::from(slave));
        if classic {
            command.arg("--classic");
        }
        own_terminal(&mut command);
        // Close fixture PTYs before reaping a child on unwind.
        let (mut child, mut input, mut master) =
            (ChildGuard::new(command.spawn().unwrap()), input, master);
        drop(command);
        let mut transcript = Vec::new();
        read_until_consent(
            &mut master,
            &mut transcript,
            &mut child,
            "startup chat view",
        );
        assert_eq!(
            transcript.windows(4).any(|part| part == b"\x1b[6n"),
            !fullscreen,
            "only Classic observes the inline cursor"
        );
        let text = String::from_utf8_lossy(&transcript);
        for mode in [1049, 1000, 1002, 1006] {
            assert_eq!(
                text.contains(&format!("\x1b[?{mode}h")),
                fullscreen,
                "startup mode {mode}: saved {saved:?}, --classic {classic}"
            );
        }
        for mode in [1003, 1007, 1015] {
            assert!(!text.contains(&format!("\x1b[?{mode}h")));
        }
        input.write_all(&[3]).unwrap();
        let output = crate::process::capture_terminal(
            child.child(),
            master,
            Duration::from_secs(10),
            64 * 1024,
            |_| {},
        );
        assert_eq!(output.status.code(), Some(0));
        restored(&input, &before);
    }
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
            StateRoot::admit(&state).unwrap();
            let entries = || {
                let mut names = std::fs::read_dir(&state)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect::<Vec<_>>();
                names.sort();
                names
            };
            let seeded = entries();
            std::fs::create_dir(&workspace).unwrap();
            let geometry = nix::pty::Winsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            let (input, slave) = crate::process::cloexec_pty(Some(&geometry));
            let before = crate::process::terminal_settings(&input);
            let broken = case.starts_with("stderr-");
            let mut fault_probe = None;
            let (master, stderr) = if broken {
                let (master, stderr) = crate::process::cloexec_pty(Some(&geometry));
                fault_probe = Some(stderr.try_clone().unwrap());
                (master, stderr)
            } else {
                (input.try_clone().unwrap(), slave.try_clone().unwrap())
            };
            let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
            command
                .env_clear()
                .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account"))
                .env("HOME", temp.path().join("fixture-home"))
                .env("XDG_STATE_HOME", temp.path().join("fixture-state"))
                .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
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
            // Close fixture PTYs before reaping a child on unwind.
            let (mut child, mut input, mut master) =
                (ChildGuard::new(command.spawn().unwrap()), input, master);
            drop(command);
            let pid = Pid::from_raw(child.child().id().try_into().unwrap()).unwrap();
            let mut transcript = Vec::new();
            if case != "stderr-before" {
                read_until_consent(
                    master.as_mut().unwrap(),
                    &mut transcript,
                    &mut child,
                    &format!("{linear} {case}"),
                );
                assert_eq!(
                    transcript.windows(8).any(|part| part == b"\x1b[?1049h"),
                    !linear,
                    "default chat view is fullscreen: {linear} {case}"
                );
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
            if let Some(probe) = fault_probe {
                wait_stderr_disconnection(probe, &format!("folder consent: {linear} {case}"));
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
            assert_eq!(entries(), seeded, "no Session admission: {linear} {case}");
            assert!(!transcript.windows(6).any(|part| part == b"Setup:"));
            assert_eq!(std::fs::read_dir(&workspace).unwrap().count(), 0);
            for name in ["fixture-home", "fixture-state", "fixture-data"] {
                assert!(!temp.path().join(name).exists());
            }
            let account = temp.path().join("account");
            if account.exists() {
                for entry in std::fs::read_dir(&account).unwrap() {
                    let entry = entry.unwrap();
                    let name = entry.file_name();
                    assert!(
                        name == "account-credentials.lock"
                            || name == "history.lock"
                            || name == "events.sqlite3",
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
                    let mut stream = crate::process::accept_with_deadline(
                        &listener,
                        Duration::from_secs(15),
                        &format!("native request {index}"),
                    );
                    stream.set_nonblocking(false).unwrap();
                    let body = read_request(&mut stream);
                    let wire: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(wire["model"], "model-1");
                    assert!(wire.get("text").is_none());
                    if index == 2 {
                        crate::loopback::assert_compaction_request(&wire);
                    }
                    let input: serde_json::Value = crate::semantic::input(&wire);
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
                            // A Retry-After past the action deadline ends the call without a same-route retry.
                            stream.write_all(b"HTTP/1.1 503 Unavailable\r\nRetry-After: 86400\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
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
                            serde_json::json!("ok")
                        }
                        1 => {
                            serde_json::json!({"outcome":{"type":"delegate","children":[{"objective":"synthetic child","profile":null}]}})
                        }
                        _ => serde_json::json!(r#"synthetic summary"#),
                    };
                    if index == 2 {
                        assert_eq!(outcome, "synthetic summary", "plain-text compaction reply");
                    }
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
            let (master, slave) = crate::process::cloexec_pty(Some(&geometry));
            let before = crate::process::terminal_settings(&slave);
            let terminal_probe = matches!(exit, Exit::PtyLoss).then(|| slave.try_clone().unwrap());
            let mut input = Some(master.try_clone().unwrap());
            let mut input_master = None;
            let (master, stderr) = if matches!(exit, Exit::PtyLoss | Exit::BrokenStderr) {
                if matches!(exit, Exit::PtyLoss) {
                    input_master = Some(master);
                }
                crate::process::cloexec_pty(Some(&geometry))
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
                .env("HOME", temp.path().join("fixture-home"))
                .env("XDG_STATE_HOME", temp.path().join("fixture-state"))
                .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
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
            let output = if let Some(probe) = stderr_probe {
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
                wait_stderr_disconnection(probe, &format!("active terminal: {linear} {exit:?}"));
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
                        let expected = match (linear, matches!(exit, Exit::Failure)) {
                            (true, true) => b"Status: failed".as_slice(),
                            (true, false) => b"Status: cancelled".as_slice(),
                            (false, true) => b"failed.".as_slice(),
                            (false, false) => b"continue.".as_slice(),
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

#[path = "active_terminal_macos/steering.rs"]
mod steering;

#[test]
fn native_explicit_classic_uses_native_scrollback_and_dumps_bounded_details() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    let state = temp.path().join("state");
    std::fs::create_dir(&workspace).unwrap();
    let id = crate::seed_transcript_session(&state, &workspace);
    assert!(!state.join("ui-preferences.json").exists());
    let geometry = nix::pty::Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let (input, slave) = crate::process::cloexec_pty(Some(&geometry));
    let before = crate::process::terminal_settings(&input);
    let master = input.try_clone().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
    command
        .env_clear()
        .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account"))
        .env("HOME", temp.path().join("fixture-home"))
        .env("XDG_STATE_HOME", temp.path().join("fixture-state"))
        .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
        .env("TERM", "xterm")
        .current_dir(&workspace)
        .args(["--classic", "--no-color", "--state-dir"])
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .arg("--resume")
        .arg(id.to_string())
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::piped())
        .stderr(Stdio::from(slave));
    own_terminal(&mut command);
    let (mut child, mut input, master) = (ChildGuard::new(command.spawn().unwrap()), input, master);
    drop(command);
    let mut queries = 0;
    let mut declined = false;
    let mut stage = 0;
    let mut from = 0;
    let mut dump_from = 0;
    let mut prelude = Vec::new();
    let output = crate::process::capture_terminal(
        child.child(),
        master,
        Duration::from_secs(20),
        256 * 1024,
        |bytes| {
            let count = bytes.windows(4).filter(|part| *part == b"\x1b[6n").count();
            assert!(count <= 128);
            super::reply_positioned_cursor_queries(&mut input, bytes, &mut queries);
            crate::process::decline_workspace_consent(&mut input, bytes, &mut declined);
            // The idle composer caret is found by its border wherever the viewport sits.
            let idle_caret = |from: usize| {
                super::composer_caret(bytes, from, 80, 24).filter(|(column, _)| *column == 3)
            };
            if stage == 0
                && let Some((_, screen)) = idle_caret(0)
            {
                // Idle Classic takes only its content, so the composer border sits under history.
                let border = screen
                    .iter()
                    .rposition(|row| row.starts_with("─ Ask Arany"))
                    .expect("composer border");
                super::assert_history_above(&screen, border, "idle Classic");
                from = bytes.len();
                input.write_all(b"\x1b[5~\x1b[6~\x0f").unwrap();
                stage = 1;
            } else if (4..=9).contains(&stage) {
                let tail = &bytes[from..];
                let (needle, send): (&[u8], &[u8]) = match stage {
                    4 => (b"Resume \xc2\xb7", b"\x1b"),
                    5 => (b"", b"/agents\r"),
                    6 => (b"Last", b"\x1b"),
                    7 => (b"", b"/help\r"),
                    8 => (b"local controls", b"\x1b"),
                    _ => (b"", b"/exit\r"),
                };
                // A selector takes keys only after its armed frame is drawn.
                let frame_ready = if matches!(stage, 4 | 6 | 8) {
                    super::armed_selector(bytes, from, 80, 24).is_some()
                } else {
                    idle_caret(from).is_some()
                };
                if (needle.is_empty() || tail.windows(needle.len()).any(|part| part == needle))
                    && frame_ready
                {
                    if matches!(stage, 5 | 7 | 9) {
                        assert!(
                            !tail.windows(4).any(|part| part == b"\x1b[2J"),
                            "settled history must not be cleared"
                        );
                    }
                    from = bytes.len();
                    input.write_all(send).unwrap();
                    stage += 1;
                }
            } else if (1..=3).contains(&stage)
                && bytes[from..]
                    .windows(b"reached;".len())
                    .position(|part| part == b"reached;")
                    .is_some_and(|end| idle_caret(from + end + b"reached;".len()).is_some())
            {
                let dump = &bytes[from..];
                let heading = dump
                    .windows(b"Transcript".len())
                    .position(|part| part == b"Transcript")
                    .unwrap();
                assert_eq!(
                    dump.windows(b"DETAIL_CANARY".len())
                        .filter(|part| *part == b"DETAIL_CANARY")
                        .count(),
                    16
                );
                if stage == 1 {
                    prelude = dump[..heading].to_vec();
                    dump_from = from;
                }
                if stage == 2 {
                    assert_eq!(&dump[..heading], prelude, "page keys produce no output");
                }
                from = bytes.len();
                input
                    .write_all(match stage {
                        1 => b"\x0f",
                        2 => b"/transcript\r",
                        _ => b"/resume\r",
                    })
                    .unwrap();
                stage += 1;
            }
            assert!(
                !bytes[dump_from..]
                    .windows(5)
                    .any(|part| part == b"\x1b]52;")
            );
        },
    );
    assert_eq!(stage, 10);
    let first_query = output
        .stderr
        .windows(4)
        .position(|part| part == b"\x1b[6n")
        .expect("Classic startup cursor query");
    assert!(
        output.stderr[..first_query].ends_with(b"\x1b[24;1H"),
        "Classic reserves rows below any existing shell output"
    );
    super::assert_classic_owned_clear(&output.stderr, from);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    for mode in [1049, 1000, 1002, 1003, 1006, 1007, 1015] {
        assert!(
            !text.contains(&format!("\x1b[?{mode}h")),
            "Classic enabled {mode}"
        );
    }
    restored(&input, &before);
}

fn find(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    bytes.windows(needle.len()).position(|part| part == needle)
}

fn rfind(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    bytes.windows(needle.len()).rposition(|part| part == needle)
}

#[test]
fn native_fullscreen_pager_dumps_bounded_transcript_while_a_run_streams() {
    const ANSWER: &str = "STREAM_CANARY";
    const FOOTER: &str = "Transcript · / find · n/N · j/k · g/G";
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    let state = temp.path().join("state");
    std::fs::create_dir(&workspace).unwrap();
    let id = crate::seed_transcript_session(&state, &workspace);
    let provider = super::loopback::held_stream_provider(&state, ANSWER);
    check_profile(&workspace, &state);
    let geometry = nix::pty::Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let (input, slave) = crate::process::cloexec_pty(Some(&geometry));
    let before = crate::process::terminal_settings(&input);
    let master = input.try_clone().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
    command
        .env_clear()
        .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account"))
        .env("HOME", temp.path().join("fixture-home"))
        .env("XDG_STATE_HOME", temp.path().join("fixture-state"))
        .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .env("TERM", "xterm")
        .current_dir(&workspace)
        .args(["--no-color", "--state-dir"])
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .arg("--resume")
        .arg(id.to_string())
        .args(["--provider", "custom:local", "--model", "model-1", "stream"])
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::piped())
        .stderr(Stdio::from(slave));
    own_terminal(&mut command);
    let (mut child, mut input, master) = (ChildGuard::new(command.spawn().unwrap()), input, master);
    drop(command);
    let mut declined = false;
    let mut stage = 0;
    let mut from = 0;
    let mut dump_from = 0;
    let mut back_from = 0;
    let output = crate::process::capture_terminal(
        child.child(),
        master,
        Duration::from_secs(40),
        256 * 1024,
        |bytes| {
            assert!(!bytes.windows(4).any(|part| part == b"\x1b[6n"));
            crate::process::decline_workspace_consent(&mut input, bytes, &mut declined);
            let tail = &bytes[from..];
            let footer = |bytes: &[u8]| super::screen_rows(bytes, 80, 24)[23].starts_with(FOOTER);
            // The lines `[` appended after leaving the alternate screen, decoded tall enough to
            // keep the scrolled-off part, once the last one is the return cue.
            let dumped = |bytes: &[u8]| {
                let start = find(bytes, b"\x1b[?1049l")?;
                let mut lines = super::screen_rows(&bytes[start..], 80, 512)
                    .into_iter()
                    .map(|row| row.trim_end().to_owned())
                    .collect::<Vec<_>>();
                let end = lines.iter().rposition(|line| !line.is_empty())?;
                lines.truncate(end + 1);
                (lines[end] == "Enter returns to Arany").then_some((start, lines))
            };
            let dump = (stage == 2).then(|| dumped(tail)).flatten();
            let next: Option<&[u8]> = match stage {
                0 if provider.streaming.try_recv().is_ok() => Some(b"\x0f"),
                1 if footer(bytes) => Some(b"["),
                2 if dump.is_some() => {
                    let (start, lines) = dump.expect("dumped lines");
                    let released = find(tail, b"\x1b[?1000l").expect("capture released");
                    assert!(released < start, "capture is released before the text");
                    assert_eq!(
                        lines[..2],
                        [
                            "Arany transcript:",
                            "Earlier Runs omitted; arany show prints the whole conversation"
                        ]
                    );
                    // 17 seeded Runs and the active one: the 16 newest whole Runs, read like chat.
                    let count =
                        |expected: &str| lines.iter().filter(|line| *line == expected).count();
                    assert_eq!(count("You:"), 16);
                    assert_eq!(count("DETAIL_CANARY \\u{001b}]52;c;inert\\u{0007}"), 15);
                    assert_eq!(count("  Synthetic answer"), 15);
                    let active = lines
                        .iter()
                        .rposition(|line| line == "You:")
                        .expect("active Run");
                    assert_eq!(lines[active + 1], "  stream");
                    assert_eq!(find(tail, b"\x1b]52"), None);
                    dump_from = from;
                    provider.release.send(()).unwrap();
                    stage = 3;
                    None
                }
                3 if provider.consumed.try_recv().is_ok() => {
                    assert_eq!(
                        find(tail, b"\x1b[?1049h"),
                        None,
                        "no redraw during the dump"
                    );
                    assert_eq!(find(tail, b"\x1b[?1000h"), None);
                    Some(b"\r")
                }
                4 if find(tail, b"\x1b[?1049h").is_some()
                    && find(tail, b"\x1b[?1006h").is_some()
                    && footer(tail)
                    && {
                        let rows = super::screen_rows(tail, 80, 24);
                        rows.iter().enumerate().skip(1).any(|(index, row)| {
                            row.trim_end() == format!("  {ANSWER}")
                                && rows[index - 1].trim_end() == "● Arany:"
                        })
                    } =>
                {
                    back_from = from;
                    Some(b"q")
                }
                // An active Run drops an empty-draft Ctrl+D, so wait for the settled `ready`.
                5 if find(tail, b"\x1b[?25h\x1b[22;3H").is_some()
                    && super::screen_rows(bytes, 80, 24)[23].starts_with("ready") =>
                {
                    Some(b"\x04")
                }
                _ => None,
            };
            if let Some(keys) = next {
                from = bytes.len();
                input.write_all(keys).unwrap();
                stage += 1;
            }
        },
    );
    assert_eq!(stage, 6);
    assert!(output.status.success());
    restored(&input, &before);
    provider.peer.join().unwrap();
    let stderr = &output.stderr;
    let dump = &stderr[dump_from..back_from];
    assert!(find(dump, b"\x1b[?1049l").is_some() && find(dump, b"\x1b[?1000l").is_some());
    for mode in [
        b"\x1b[?1049h".as_slice(),
        b"\x1b[?1000h",
        b"\x1b[?1002h",
        b"\x1b[?1006h",
    ] {
        assert!(
            find(&stderr[back_from..], mode).is_some(),
            "Enter reacquires"
        );
    }
    let text = String::from_utf8_lossy(stderr);
    for mode in [1003, 1007, 1015] {
        assert!(
            !text.contains(&format!("\x1b[?{mode}h")),
            "Fullscreen enabled {mode}"
        );
    }
    for mode in [1049, 1000, 1002, 1006] {
        let enabled = text.rfind(&format!("\x1b[?{mode}h")).expect("mode enabled");
        assert!(
            text.rfind(&format!("\x1b[?{mode}l"))
                .is_some_and(|disabled| enabled < disabled),
            "exit releases {mode}"
        );
    }
    let left = rfind(stderr, b"\x1b[?1049l").expect("alternate screen left");
    assert_eq!(
        find(&stderr[left..], b"\x1b[2J"),
        None,
        "the dump stays on the primary screen"
    );
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native pbcopy gate; replaces the general pasteboard"
)]
fn native_copy_round_trips_non_ascii_answer_through_pbcopy() {
    const ANSWER: &str = "Copia ✓ café 日本語\nsegunda línea";
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    let state = temp.path().join("state");
    std::fs::create_dir(&workspace).unwrap();
    let id = crate::seed_answer_session(&state, &workspace, ANSWER);
    let geometry = nix::pty::Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let (input, slave) = crate::process::cloexec_pty(Some(&geometry));
    let before = crate::process::terminal_settings(&input);
    let master = input.try_clone().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
    command
        .env_clear()
        .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account"))
        .env("HOME", temp.path().join("fixture-home"))
        .env("XDG_STATE_HOME", temp.path().join("fixture-state"))
        .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
        .env("TERM", "xterm")
        .current_dir(&workspace)
        .args(["--classic", "--no-color", "--state-dir"])
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .arg("--resume")
        .arg(id.to_string())
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::piped())
        .stderr(Stdio::from(slave));
    own_terminal(&mut command);
    let (mut child, mut input, master) = (ChildGuard::new(command.spawn().unwrap()), input, master);
    drop(command);
    let mut queries = 0;
    let mut declined = false;
    let mut stage = 0;
    let mut from = 0;
    let mut copy_from = None;
    let output = crate::process::capture_terminal(
        child.child(),
        master,
        Duration::from_secs(20),
        256 * 1024,
        |bytes| {
            assert!(bytes.windows(4).filter(|part| *part == b"\x1b[6n").count() <= 128);
            super::reply_positioned_cursor_queries(&mut input, bytes, &mut queries);
            crate::process::decline_workspace_consent(&mut input, bytes, &mut declined);
            // The idle composer caret is found by its border wherever the viewport sits.
            let caret =
                super::composer_caret(bytes, from, 80, 24).is_some_and(|(column, _)| column == 3);
            if stage < 2 && caret {
                from = bytes.len();
                input
                    .write_all(if stage == 0 { b"/copy\r" } else { b"/exit\r" })
                    .unwrap();
                copy_from = copy_from.or((stage == 0).then_some(from));
                stage += 1;
            }
        },
    );
    assert_eq!(stage, 2);
    assert!(output.status.success());
    restored(&input, &before);
    let copied = &output.stderr[copy_from.expect("copy submitted")..];
    for fallback in [b"\x1b]52".as_slice(), b"Copy unavailable"] {
        assert!(
            !copied.windows(fallback.len()).any(|part| part == fallback),
            "pbcopy was not the destination: {}",
            String::from_utf8_lossy(copied).escape_debug()
        );
    }
    let mut paste = ChildGuard::new(
        Command::new("/usr/bin/pbpaste")
            .env_clear()
            .env("LANG", "en_US.UTF-8")
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let pasted = crate::process::capture(paste.child(), Duration::from_secs(5), 64 * 1024);
    assert!(pasted.status.success());
    assert_eq!(String::from_utf8(pasted.stdout).unwrap(), ANSWER);
}

#[path = "active_terminal/keyboard.rs"]
mod keyboard;
