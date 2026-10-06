use super::{pump, tail};
use crate::active_terminal::{ProductGuard, product_child_of_executable, tty_settings};
use crate::loopback::{ChildGuard, wait_product};
use arany::{SessionView, StateRoot, Store, create_session};
use rustix::{
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    process::{Pid, Signal, kill_process},
    pty::{OpenptFlags, grantpt, ioctl_tiocgptpeer, openpt, ptsname, unlockpt},
    termios::{Winsize, tcsetwinsize},
};
use std::{
    fs::File,
    io::{Read, Write},
    process::Stdio,
    thread,
    time::{Duration, Instant},
};

fn product_state(pid: Pid) -> char {
    let status = std::fs::read_to_string(format!("/proc/{}/status", pid.as_raw_pid()))
        .expect("product process status");
    status
        .lines()
        .find_map(|line| {
            line.strip_prefix("State:")
                .and_then(|state| state.trim().chars().next())
        })
        .expect("product process state")
}

fn run_picker_signal_case(signal: Signal, suspend: bool, columns: u16, rows: u16) {
    let signal_name = match signal {
        Signal::TERM => "SIGTERM",
        Signal::HUP => "SIGHUP",
        _ => panic!("unsupported picker signal fixture"),
    };
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("Session runtime");
    let root = StateRoot::admit(&state).expect("state root");
    let session_id = runtime
        .block_on(create_session(
            root,
            workspace.clone(),
            Some("Stable".into()),
        ))
        .expect("seed Session");

    let command = format!(
        "stty rows {rows} cols {columns}; printf 'SHELL_PID:%s\\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\""
    );
    let mut attached = crate::process::isolated_script(temp.path());
    attached
        .env_clear()
        .env("ARANY_TEST_EXE", "/arany")
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", &command, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut attached = ChildGuard::new(attached.spawn().expect("picker process"));
    let mut input = attached.child().stdin.take().expect("PTY input");
    let mut output = attached.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout pipe flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking stdout pipe");
    let mut transcript = Vec::new();
    let mut answered = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !transcript
        .windows(b"\x1b[?1000h".len())
        .any(|part| part == b"\x1b[?1000h")
        || (columns == 16
            && (!transcript.windows(b"Esc".len()).any(|part| part == b"Esc")
                || !transcript
                    .windows(b"\x1b[?25l".len())
                    .any(|part| part == b"\x1b[?25l")))
    {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "picker frame did not complete at {columns}x{rows}: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    let visible = String::from_utf8_lossy(&transcript);
    let shell_pid = visible
        .lines()
        .find_map(|line| line.strip_prefix("SHELL_PID:"))
        .expect("shell PID marker")
        .trim_end_matches('\r')
        .parse::<u32>()
        .expect("shell PID");
    let before = visible
        .lines()
        .find_map(|line| line.strip_prefix("TTY_BEFORE:"))
        .expect("initial terminal settings")
        .trim_end_matches('\r')
        .to_owned();
    let product_pid = product_child_of_executable(shell_pid, std::path::Path::new("/arany"));
    let _product_guard = ProductGuard::new(product_pid, state.clone());
    assert_ne!(
        tty_settings(product_pid),
        before,
        "raw mode active in picker"
    );
    if suspend {
        kill_process(product_pid, Signal::TSTP).expect("suspend picker");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            if product_state(product_pid) == 'T'
                && transcript
                    .windows(b"\x1b[?1000l".len())
                    .any(|part| part == b"\x1b[?1000l")
            {
                break;
            }
            assert!(Instant::now() < deadline, "picker did not suspend");
            thread::yield_now();
        }
        assert_eq!(
            tty_settings(product_pid),
            before,
            "raw mode released while stopped"
        );
        kill_process(product_pid, Signal::CONT).expect("resume picker");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            let enables = transcript
                .windows(b"\x1b[?1000h".len())
                .filter(|part| *part == b"\x1b[?1000h")
                .count();
            if enables == 2 {
                break;
            }
            assert!(Instant::now() < deadline, "picker did not reacquire mouse");
            thread::yield_now();
        }
        assert_ne!(tty_settings(product_pid), before, "raw mode reacquired");
    }
    kill_process(product_pid, signal).expect("terminate picker");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        if attached
            .child()
            .try_wait()
            .expect("picker status")
            .is_some()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "picker did not exit after {signal_name}: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    let result = wait_product(attached.take());
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    assert!(
        !result.status.success(),
        "{signal_name} exits unsuccessfully"
    );
    assert_eq!(result.stderr, b"");
    let transcript = String::from_utf8(transcript).expect("PTY transcript UTF-8");
    let disabled = transcript.rfind("\x1b[?1000l").expect("mouse disabled");
    let restored = transcript.find("TTY_AFTER:").expect("restoration marker");
    assert!(
        disabled < restored,
        "mouse disabled before shell regained TTY"
    );
    let after = transcript[restored + "TTY_AFTER:".len()..]
        .lines()
        .next()
        .expect("restored terminal settings")
        .trim_end_matches('\r');
    assert_eq!(before, after, "raw mode restored after {signal_name}");
    let expected = if suspend { 2 } else { 1 };
    assert_eq!(transcript.matches("\x1b[?1000h").count(), expected);
    assert_eq!(transcript.matches("\x1b[?1000l").count(), expected);
    assert!(
        transcript.contains(&format!("terminated by {signal_name}")),
        "missing {signal_name} notice"
    );

    let store = Store::open_read_only(StateRoot::open_existing(&state).expect("state reopen"))
        .expect("read-only Store");
    let events = runtime
        .block_on(store.load_session(session_id))
        .expect("Session Events");
    let view = SessionView::replay(session_id, &events)
        .expect("strict replay")
        .expect("Session");
    assert_eq!(events.len(), 1, "picker signal leaves Session unchanged");
    assert_eq!(view.title, "Stable");
    runtime.block_on(store.close()).expect("close Store");
}

#[test]
fn picker_signals_release_and_reacquire_mouse_with_terminal_ownership() {
    for (signal, suspend, columns, rows) in [
        (Signal::TERM, false, 80, 24),
        (Signal::HUP, false, 80, 24),
        (Signal::TERM, true, 80, 24),
        (Signal::TERM, false, 16, 8),
    ] {
        run_picker_signal_case(signal, suspend, columns, rows);
    }
}

fn drain(reader: &mut impl Read, bytes: &mut Vec<u8>) {
    loop {
        let mut chunk = [0; 4096];
        match reader.read(&mut chunk) {
            Ok(0) => return,
            Ok(count) => {
                assert!(bytes.len() + count <= 64 * 1024, "bounded PTY transcript");
                bytes.extend_from_slice(&chunk[..count]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => panic!("PTY read failed: {error}"),
        }
    }
}

#[test]
fn picker_renderer_failure_restores_raw_mode_without_changing_session() {
    for (columns, rows) in [(80, 24), (16, 8)] {
        for attached_entry in [false, true] {
            run_picker_renderer_failure_case(columns, rows, attached_entry);
        }
    }
}

fn run_picker_renderer_failure_case(columns: u16, rows: u16, attached_entry: bool) {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("Session runtime");
    let session_id = runtime
        .block_on(create_session(
            StateRoot::admit(&state).expect("state root"),
            workspace.clone(),
            Some("Stable".into()),
        ))
        .expect("seed Session");

    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)
        .expect("stderr PTY master");
    grantpt(&master).expect("grant stderr PTY");
    unlockpt(&master).expect("unlock stderr PTY");
    let slave = ioctl_tiocgptpeer(
        &master,
        OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC,
    )
    .expect("stderr PTY slave");
    tcsetwinsize(
        &slave,
        Winsize {
            ws_row: rows,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .expect("stderr PTY size");
    let slave_path = ptsname(&master, Vec::new())
        .expect("stderr PTY path")
        .into_string()
        .expect("UTF-8 PTY path");
    let mut stderr_master = File::from(master);
    let flags = fcntl_getfl(&stderr_master).expect("stderr master flags");
    fcntl_setfl(&stderr_master, flags | OFlags::NONBLOCK).expect("nonblocking stderr master");
    let mut stderr_probe = File::from(slave);

    let command = format!(
        "stty rows {rows} cols {columns}; printf 'SHELL_PID:%s\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; exec 2>\"$ARANY_TEST_STDERR_PTY\"; \"$ARANY_TEST_EXE\" --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\""
    );
    let mut attached = crate::process::isolated_script(temp.path());
    attached
        .env_clear()
        .env("ARANY_TEST_EXE", "/arany")
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("ARANY_TEST_STDERR_PTY", &slave_path)
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", &command, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut attached = ChildGuard::new(attached.spawn().expect("picker process"));
    let mut input = attached.child().stdin.take().expect("PTY input");
    let mut output = attached.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout pipe flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking stdout pipe");
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut answered = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !stderr_bytes
        .windows(b"Esc".len())
        .any(|part| part == b"Esc")
        || !stderr_bytes
            .windows(b"\x1b[?25l".len())
            .any(|part| part == b"\x1b[?25l")
    {
        drain(&mut output, &mut stdout_bytes);
        drain(&mut stderr_master, &mut stderr_bytes);
        let queries = stdout_bytes
            .windows(4)
            .chain(stderr_bytes.windows(4))
            .filter(|part| *part == b"\x1b[6n")
            .count();
        assert!(queries <= 32, "bounded cursor queries");
        while answered < queries {
            input.write_all(b"\x1b[2;1R").expect("cursor response");
            answered += 1;
        }
        assert!(
            Instant::now() < deadline,
            "picker frame not drawn; stderr: {}; stdout: {}",
            tail(&stderr_bytes),
            tail(&stdout_bytes)
        );
        thread::yield_now();
    }
    assert!(
        stderr_bytes
            .windows(b"\x1b[?1000h".len())
            .any(|part| part == b"\x1b[?1000h"),
        "picker enabled mouse capture"
    );
    let stdout_text = String::from_utf8_lossy(&stdout_bytes);
    let shell_pid = stdout_text
        .lines()
        .find_map(|line| line.strip_prefix("SHELL_PID:"))
        .expect("shell PID")
        .trim_end_matches('\r')
        .parse::<u32>()
        .expect("shell PID");
    let before = stdout_text
        .lines()
        .find_map(|line| line.strip_prefix("TTY_BEFORE:"))
        .expect("initial terminal settings")
        .trim_end_matches('\r')
        .to_owned();
    let product_pid = product_child_of_executable(shell_pid, std::path::Path::new("/arany"));
    let _product_guard = ProductGuard::new(product_pid, state.clone());
    assert_ne!(tty_settings(product_pid), before, "picker owns raw mode");

    if attached_entry {
        let transitions: [(&[u8], &[u8], &[u8]); 2] = [
            (b"\r", b"\x1b[?1000l", b"\x1b[?25h"),
            (b"/resume\r", b"\x1b[?1000h", b"\x1b[?25l"),
        ];
        for (keys, capture, cursor) in transitions {
            let frame_at = stderr_bytes.len();
            input.write_all(keys).expect("attached picker transition");
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                drain(&mut output, &mut stdout_bytes);
                drain(&mut stderr_master, &mut stderr_bytes);
                let queries = stdout_bytes
                    .windows(4)
                    .chain(stderr_bytes.windows(4))
                    .filter(|part| *part == b"\x1b[6n")
                    .count();
                assert!(queries <= 32, "bounded cursor queries");
                while answered < queries {
                    input.write_all(b"\x1b[2;1R").expect("cursor response");
                    answered += 1;
                }
                let recent = &stderr_bytes[frame_at..];
                if recent.windows(capture.len()).any(|part| part == capture)
                    && recent.windows(cursor.len()).any(|part| part == cursor)
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "attached picker transition at {columns}x{rows}: {}",
                    tail(recent)
                );
                thread::yield_now();
            }
        }
        assert_eq!(
            stderr_bytes
                .windows(8)
                .filter(|part| *part == b"\x1b[?1000h")
                .count(),
            2,
            "attached picker reacquires mouse capture"
        );
        assert_eq!(
            stderr_bytes
                .windows(8)
                .filter(|part| *part == b"\x1b[?1000l")
                .count(),
            1,
            "initial picker releases mouse capture before composer"
        );
        assert_ne!(
            tty_settings(product_pid),
            before,
            "attached picker owns raw mode"
        );
    }

    drop(stderr_master);
    let flags = fcntl_getfl(&stderr_probe).expect("stderr probe flags");
    fcntl_setfl(&stderr_probe, flags | OFlags::NONBLOCK).expect("nonblocking stderr probe");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match stderr_probe.write(b"x") {
            Err(error) if error.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) => {
                break;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("stderr probe failed unexpectedly: {error}"),
        }
        assert!(Instant::now() < deadline, "stderr PTY did not fail writes");
        thread::yield_now();
    }
    input.write_all(b"\x1b[A").expect("trigger picker redraw");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        drain(&mut output, &mut stdout_bytes);
        if attached
            .child()
            .try_wait()
            .expect("picker status")
            .is_some()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "broken picker renderer did not exit"
        );
        thread::yield_now();
    }
    let result = wait_product(attached.take());
    drain(&mut output, &mut stdout_bytes);
    assert!(
        !result.status.success(),
        "broken renderer exits unsuccessfully"
    );
    assert_eq!(result.stderr, b"");
    let transcript = String::from_utf8(stdout_bytes).expect("PTY transcript UTF-8");
    let after = transcript
        .split_once("TTY_AFTER:")
        .expect("restoration marker")
        .1
        .lines()
        .next()
        .expect("restored terminal settings")
        .trim_end_matches('\r');
    assert_eq!(
        before, after,
        "raw mode restored after picker render failure"
    );
    assert!(!transcript.contains("Answer:"));

    let store = Store::open_read_only(StateRoot::open_existing(&state).expect("state reopen"))
        .expect("read-only Store");
    let events = runtime
        .block_on(store.load_session(session_id))
        .expect("Events");
    let view = SessionView::replay(session_id, &events)
        .expect("strict replay")
        .expect("Session");
    assert_eq!(events.len(), 1, "renderer failure leaves Session unchanged");
    assert_eq!(view.title, "Stable");
    runtime.block_on(store.close()).expect("close Store");
}
