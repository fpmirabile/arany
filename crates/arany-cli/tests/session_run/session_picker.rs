use super::active_terminal::{
    ProductGuard, assert_tty_restored, product_child_of_executable, transcript_field,
};
use crate::pty::Pty;
use arany::{SessionView, StateRoot, Store, create_session};
use std::{
    os::unix::fs::PermissionsExt,
    thread,
    time::{Duration, Instant},
};

pub(super) use crate::pty::screen_contains;
pub(super) use crate::{screen_rows, tail};

fn mouse_modes_enabled(bytes: &[u8]) -> Vec<u32> {
    [1000, 1002, 1003, 1006, 1015]
        .into_iter()
        .filter(|mode| {
            let enable = format!("\x1b[?{mode}h");
            bytes
                .windows(enable.len())
                .any(|part| part == enable.as_bytes())
        })
        .collect()
}

fn count(bytes: &[u8], needle: &[u8]) -> usize {
    bytes
        .windows(needle.len())
        .filter(|part| *part == needle)
        .count()
}

fn run_picker_case(
    name: &str,
    width: u16,
    selection: &[u8],
    older_selected: bool,
    stale_activation: bool,
    fullscreen: bool,
) {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("Session runtime");
    let root = StateRoot::admit(&state).expect("state root");
    let older = runtime
        .block_on(create_session(
            StateRoot::open_existing(&state).expect("state reopen"),
            workspace.clone(),
            Some("Older".into()),
        ))
        .expect("older Session");
    let newer = runtime
        .block_on(create_session(
            StateRoot::open_existing(&state).expect("state reopen"),
            workspace.clone(),
            Some("Newer".into()),
        ))
        .expect("newer Session");
    drop(root);

    let no_color = if width < 20 || stale_activation {
        "--no-color "
    } else {
        ""
    };
    let classic = if fullscreen { "" } else { "--classic " };
    let command = format!(
        "stty rows 24 cols {width}; printf 'SHELL_PID:%s\\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" {classic}{no_color}--state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\""
    );
    let mut pty = Pty::new(
        crate::product::PtyLaunch::new(
            crate::product::Isolation::Passwd(crate::isolation::isolated_wrapper(temp.path())),
            &command,
        )
        .executable("/arany")
        .session(&state, &workspace)
        .term("xterm")
        .spawn(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    // Keys reach the picker only after its armed frame; earlier input is discarded.
    while !crate::armed_selector(&pty.transcript, 0, width.into(), 24)
        .is_some_and(|screen| screen.iter().any(|line| line.contains("Newer")))
    {
        pty.pump();
        assert!(
            Instant::now() < deadline,
            "armed picker not visible in {name}: {}",
            tail(&pty.transcript)
        );
        thread::yield_now();
    }
    let shell_pid = String::from_utf8_lossy(&pty.transcript)
        .lines()
        .find_map(|line| line.trim_end_matches('\r').strip_prefix("SHELL_PID:"))
        .expect("picker shell PID")
        .parse()
        .expect("numeric picker shell PID");
    let _product = ProductGuard::new(
        product_child_of_executable(shell_pid, std::path::Path::new("/arany")),
        state.clone(),
    );
    if width < 20 {
        let cues: [&[u8]; 5] = [b"Up/Dn", b"Enter", b"Esc", b"Older", b"Newer"];
        pty.pump_until(
            &format!("narrow picker frame incomplete in {name}"),
            |transcript| {
                cues.iter().all(|cue| {
                    screen_rows(transcript, width.into(), 24)
                        .iter()
                        .any(|line| line.contains(std::str::from_utf8(cue).unwrap()))
                })
            },
        );
    }
    if fullscreen {
        // Rows respond to the pointer once Fullscreen has enabled its global capture.
        pty.pump_until(&format!("picker capture missing in {name}"), |transcript| {
            count(transcript, b"\x1b[?1000h") != 0
        });
    }
    let selected_at = pty.send(selection);
    pty.pump_until(&format!("picker did not close in {name}"), |transcript| {
        transcript[selected_at..]
            .windows(b"\x1b[?25h".len())
            .any(|part| part == b"\x1b[?25h")
    });
    if fullscreen {
        let selected = &pty.transcript[selected_at..];
        let returned = selected
            .windows(b"\x1b[?25h".len())
            .position(|part| part == b"\x1b[?25h")
            .expect("composer returned");
        assert_eq!(
            count(&selected[..returned], b"\x1b[?1000l"),
            0,
            "Fullscreen keeps its global capture when selection returns in {name}: {}",
            tail(selected)
        );
    }
    if stale_activation {
        let original_mode = std::fs::metadata(&state)
            .expect("fixture State mode")
            .permissions();
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755))
            .expect("make fixture State unsafe");
        let rejected_at = pty.send(b"/resume\r");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pty.pump();
            let recent = &pty.transcript[rejected_at..];
            if recent
                .windows(b"unsafe".len())
                .any(|part| part == b"unsafe")
                && recent.windows(6).any(|part| part == b"\x1b[?25h")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "unsafe listing feedback: {}",
                tail(recent)
            );
            thread::yield_now();
        }
        std::fs::set_permissions(&state, original_mode).expect("restore fixture State mode");
        assert!(
            pty.transcript[rejected_at..]
                .windows(b"Arany \xc2\xb7 error:".len())
                .any(|part| part == b"Arany \xc2\xb7 error:"),
            "unavailable Session listing needs an error heading: {}",
            tail(&pty.transcript[rejected_at..(rejected_at + 512).min(pty.transcript.len())])
        );
        let open_at = pty.send(b"/resume\r");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pty.pump();
            if crate::armed_selector(&pty.transcript, open_at, width.into(), 24).is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "attached picker: {}",
                tail(&pty.transcript[open_at..])
            );
            thread::yield_now();
        }
        let original_workspace = temp.path().join("workspace-original");
        let replacement_workspace = temp.path().join("workspace-replacement");
        std::fs::rename(&workspace, &original_workspace).expect("retain original Workspace");
        std::fs::create_dir(&workspace).expect("replace fixture Workspace identity");
        let failed_at = pty.send(b"\r");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pty.pump();
            let recent = &pty.transcript[failed_at..];
            if recent
                .windows(b"Workspace".len())
                .any(|part| part == b"Workspace")
                && recent.windows(6).any(|part| part == b"\x1b[?25h")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "stale activation: {}",
                tail(recent)
            );
            thread::yield_now();
        }
        std::fs::rename(&workspace, &replacement_workspace).expect("retain replacement fixture");
        std::fs::rename(&original_workspace, &workspace).expect("restore original Workspace");
        assert!(
            pty.transcript[failed_at..]
                .windows(b"Arany \xc2\xb7 error:".len())
                .any(|part| part == b"Arany \xc2\xb7 error:"),
            "rejected Session activation needs an error heading: {}",
            tail(&pty.transcript[failed_at..(failed_at + 512).min(pty.transcript.len())])
        );
    }
    pty.send(b"/rename Mouse\r/exit\r");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pty.pump();
        if pty
            .child
            .child()
            .try_wait()
            .expect("picker status")
            .is_some()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "picker did not exit in {name}: {}",
            tail(&pty.transcript)
        );
        thread::yield_now();
    }
    let result = pty.finish();
    assert!(
        result.status.success(),
        "picker {name} failed: {}",
        tail(&pty.transcript)
    );
    assert_eq!(result.stderr, b"");
    let output_text = String::from_utf8(pty.transcript).expect("PTY transcript UTF-8");
    let before = output_text
        .lines()
        .find_map(|line| line.strip_prefix("TTY_BEFORE:"))
        .expect("initial terminal settings")
        .trim_end_matches('\r');
    let after = output_text
        .split_once("TTY_AFTER:")
        .expect("restored terminal marker")
        .1
        .lines()
        .next()
        .expect("restored terminal settings")
        .trim_end_matches('\r');
    assert_eq!(before, after, "terminal restored after {name}");
    if fullscreen {
        assert_eq!(count(output_text.as_bytes(), b"\x1b[?1000h"), 1, "{name}");
        assert_eq!(count(output_text.as_bytes(), b"\x1b[?1000l"), 1, "{name}");
    } else {
        assert_eq!(
            mouse_modes_enabled(output_text.as_bytes()),
            Vec::<u32>::new(),
            "Classic picker enabled mouse reporting in {name}"
        );
        assert!(!output_text.contains("\x1b[?1049h"));
    }

    let store = Store::open_read_only(StateRoot::open_existing(&state).expect("state reopen"))
        .expect("read-only Store");
    let expected = if older_selected {
        [(older, "Mouse"), (newer, "Newer")]
    } else {
        [(older, "Older"), (newer, "Mouse")]
    };
    for (id, title) in expected {
        let events = runtime.block_on(store.load_session(id)).expect("Events");
        let view = SessionView::replay(id, &events)
            .expect("strict replay")
            .expect("Session");
        assert_eq!(view.title, title, "{name} chose the wrong Session");
        assert_eq!(events.len(), if title == "Mouse" { 2 } else { 1 });
        assert!(view.runs.is_empty());
    }
    runtime.block_on(store.close()).expect("close Store");
}

#[test]
fn session_picker_selects_rows_by_keyboard_without_mouse_capture() {
    run_picker_case(
        "keyboard and stale activation",
        80,
        b"\x1b[B\r",
        true,
        true,
        false,
    );
    run_picker_case("keyboard", 80, b"\x1b[B\r", true, false, false);
    run_picker_case("newest", 80, b"\r", false, false, false);
    run_picker_case("16-column keyboard", 16, b"\x1b[B\r", true, false, false);
}

#[test]
fn fullscreen_session_picker_mouse_selects_the_row_under_the_pointer() {
    // Two Sessions in 24 rows: the 8-row picker starts at row 17, so Newer is row 18 and Older 19.
    run_picker_case(
        "fullscreen mouse",
        80,
        b"\x1b[<65;3;18M\x1b[<64;3;18M\x1b[<35;3;19M\x1b[<0;3;19M",
        true,
        false,
        true,
    );
}

fn screen_reader_entry(
    state: &std::path::Path,
    workspace: &std::path::Path,
    resume: bool,
) -> String {
    let entry = if resume {
        "--continue"
    } else {
        "--continue --provider openai --model gpt-5.4"
    };
    let shell = format!(
        "before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" {entry}; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\""
    );
    let mut pty = Pty::new(
        crate::product::PtyLaunch::new(
            crate::product::Isolation::Passwd(crate::isolation::isolated_wrapper(
                state.parent().expect("fixture root"),
            )),
            &shell,
        )
        .executable("/arany")
        .session(state, workspace)
        .term("xterm")
        .spawn(),
    );
    pty.wait_for(b"Input:\n");
    if !resume {
        let original_mode = std::fs::metadata(state)
            .expect("fixture State mode")
            .permissions();
        std::fs::set_permissions(state, std::fs::Permissions::from_mode(0o755))
            .expect("make fixture State unsafe");
        pty.send_and_wait(
            b"/resume\r",
            b"Notice: Error: state directory unavailable or unsafe\r\nInput:\r\n",
        );
        std::fs::set_permissions(state, original_mode).expect("restore fixture State mode");
    }
    pty.send(b"/exit\r");
    let result = pty.finish();
    assert!(
        result.status.success(),
        "Session exit: {}",
        tail(&pty.transcript)
    );
    assert_eq!(result.stderr, b"");
    assert_eq!(
        pty.answered.cursor_queries, 0,
        "screen-reader mode uses no cursor query"
    );
    let text = String::from_utf8(pty.transcript).expect("screen-reader UTF-8");
    assert_tty_restored(&text);
    assert!(
        !text.contains('\x1b'),
        "screen-reader output is control-free"
    );
    transcript_field(&text, "Session: ").to_owned()
}

#[test]
fn screen_reader_continue_reopens_the_empty_workspace_session() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    super::slash_completion::saved_conversation(
        &state,
        &workspace,
        arany::SessionDefaults::default(),
    );

    let created = screen_reader_entry(&state, &workspace, false);
    let continued = screen_reader_entry(&state, &workspace, true);
    assert_eq!(continued, created, "--continue must not create a Session");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("read-only runtime");
    runtime.block_on(async {
        let root = StateRoot::open_existing(&state).expect("existing State");
        let sessions = arany::list_sessions(root, workspace.clone())
            .await
            .expect("Workspace Sessions");
        assert_eq!(sessions.len(), 1);
        let store = Store::open_read_only(StateRoot::open_existing(&state).expect("state reopen"))
            .expect("read-only Store");
        let events = store.load_session(sessions[0].id).await.expect("Events");
        let view = SessionView::replay(sessions[0].id, &events)
            .expect("strict replay")
            .expect("Session");
        assert!(view.runs.is_empty());
        assert_eq!(view.defaults.provider.as_deref(), Some("openai"));
        assert_eq!(view.defaults.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(events.len(), 2);
        store.close().await.expect("close Store");
    });
}

#[path = "session_picker/termination.rs"]
mod termination;
