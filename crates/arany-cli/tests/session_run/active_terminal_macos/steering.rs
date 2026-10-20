use super::*;
use arany::{Event, GoalStatus};

const NOTE: &str = "use the smaller fixture";

fn semantic_input(stream: &mut std::net::TcpStream) -> serde_json::Value {
    let body = read_request(stream);
    let wire: serde_json::Value = serde_json::from_slice(&body).expect("Responses request");
    assert_eq!(wire["model"], "model-1");
    crate::semantic::input(&wire)
}

fn answer_check(stream: &mut std::net::TcpStream, index: usize) {
    let outcome = match index {
        0 => serde_json::json!("ok"),
        1 => {
            serde_json::json!({"outcome":{"type":"delegate","children":[{"objective":"synthetic child","profile":null}]}})
        }
        _ => serde_json::json!(r#"{"summary":"synthetic summary"}"#),
    };
    send_response(stream, index, outcome);
}

fn replay(state: &std::path::Path) -> (Vec<arany::EventEnvelope>, arany::SessionView) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("replay runtime");
    let store = Store::open_read_only(StateRoot::open_existing(state).unwrap()).unwrap();
    let id = runtime
        .block_on(arany::list_sessions(
            StateRoot::open_existing(state).unwrap(),
            std::fs::canonicalize(state.parent().unwrap().join("project")).unwrap(),
        ))
        .unwrap()[0]
        .id;
    let events = runtime.block_on(store.load_session(id)).unwrap();
    runtime.block_on(store.close()).unwrap();
    let view = arany::SessionView::replay(id, &events).unwrap().unwrap();
    (events, view)
}

fn escaped(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|byte| byte.escape_ascii())
        .map(char::from)
        .collect()
}

fn ordered_cues_end(bytes: &[u8], cues: &[&[u8]]) -> Option<usize> {
    let mut offset = 0;
    for cue in cues {
        let mut found = false;
        while offset < bytes.len() {
            if bytes[offset..].starts_with(cue) {
                offset += cue.len();
                found = true;
                break;
            }
            if bytes[offset..].starts_with(b"\x1b[")
                && let Some(end) = bytes[offset..]
                    .iter()
                    .enumerate()
                    .skip(2)
                    .find(|(_, byte)| matches!(**byte, 0x40..=0x7e))
                    .map(|(index, _)| offset + index + 1)
            {
                offset = end;
            } else {
                offset += 1;
            }
        }
        if !found {
            return None;
        }
    }
    Some(offset)
}

fn run_native(note_delivery: bool, classic: bool) {
    let temp = tempfile::tempdir().expect("private native fixture");
    let workspace = temp.path().join("project");
    let state = temp.path().join("state");
    if !classic {
        crate::seed_fullscreen(&state);
    }
    std::fs::create_dir(&workspace).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    write_profile(&state, listener.local_addr().unwrap().port());
    let (ready_sender, ready) = mpsc::channel();
    let (release_sender, release) = mpsc::channel();
    let (stop_sender, stop) = mpsc::channel();
    let peer = thread::spawn(move || {
        let count = if note_delivery { 6 } else { 4 };
        for index in 0..count {
            let mut stream = crate::process::accept_with_deadline(
                &listener,
                Duration::from_secs(15),
                &format!("macOS synthetic request {index}"),
            );
            stream.set_nonblocking(false).unwrap();
            let input = semantic_input(&mut stream);
            if index < 3 {
                answer_check(&mut stream, index);
                continue;
            }
            if note_delivery {
                match index {
                    3 => {
                        assert_eq!(input["objective"], "steer");
                        send_response(
                            &mut stream,
                            index,
                            serde_json::json!({"outcome":{"type":"delegate","children":[{"objective":"synthetic child","profile":null}]}}),
                        );
                    }
                    4 => {
                        ready_sender.send(()).unwrap();
                        release.recv_timeout(Duration::from_secs(10)).unwrap();
                        send_response(&mut stream, index, serde_json::json!("child result"));
                    }
                    _ => {
                        assert_eq!(input["steering_notes"], serde_json::json!([NOTE]));
                        send_response(&mut stream, index, serde_json::json!("synthesized"));
                    }
                }
            } else {
                assert_eq!(input["objective"], "pause");
                ready_sender.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(10)).unwrap();
                send_response(
                    &mut stream,
                    index,
                    serde_json::json!({"outcome":{"type":"delegate","children":[{"objective":"must not run","profile":null}]}}),
                );
            }
        }
        if !note_delivery {
            stop.recv_timeout(Duration::from_secs(15)).unwrap();
            listener.set_nonblocking(true).unwrap();
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock,
                "pause must stop before another call"
            );
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
    let input_master = master.try_clone().unwrap();
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
        .args(["--provider", "custom:local", "--model", "model-1"]);
    if classic {
        command.arg("--classic");
    }
    command
        .arg(if note_delivery { "steer" } else { "pause" })
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::piped())
        .stderr(Stdio::from(slave));
    own_terminal(&mut command);
    let child = command.spawn().unwrap();
    drop(command);
    let (mut child, mut input, master) = (ChildGuard::new(child), input_master, master);
    let mut declined = false;
    let mut transcript = Vec::new();
    let mut offered = false;
    let mut controlled = false;
    let mut control_observed = false;
    let mut released = false;
    let mut goal_paused = false;
    let mut exited = false;
    let mut provider_ready = false;
    let mut control_after = 0;
    let mut offered_after = 0;
    let mut pause_reply_after = 0;
    let mut pause_sent = false;
    let mut completed_after = None;
    let mut paused_after = None;
    let mut answered_queries = 0;
    let capture = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::process::capture_terminal(
            child.child(),
            master,
            Duration::from_secs(25),
            512 * 1024,
            |bytes| {
                transcript.clear();
                transcript.extend_from_slice(bytes);
                let queries = transcript
                    .windows(4)
                    .filter(|part| *part == b"\x1b[6n")
                    .count();
                assert!(queries <= 32, "bounded native cursor queries");
                crate::reply_positioned_cursor_queries(
                    &mut input,
                    &transcript,
                    &mut answered_queries,
                );
                crate::process::decline_workspace_consent(
                    &mut input.try_clone().unwrap(),
                    &transcript,
                    &mut declined,
                );
                if !provider_ready {
                    provider_ready = ready.try_recv().is_ok();
                }
                if !declined || !provider_ready {
                    return;
                }
                if !offered && note_delivery {
                    offered_after = transcript.len();
                    input.write_all(NOTE.as_bytes()).unwrap();
                    input.write_all(b"\r").unwrap();
                    offered = true;
                }
                if note_delivery
                    && offered
                    && !controlled
                    && ordered_cues_end(
                        &transcript[offered_after..],
                        &[b"Steering", b"note", b"waits"],
                    )
                    .is_some()
                {
                    // BackTab cycles permission feedback without editing the retained note.
                    input.write_all(b"\x1b[Z").unwrap();
                    control_after = transcript.len();
                    controlled = true;
                }
                if note_delivery
                    && controlled
                    && !control_observed
                    && (ordered_cues_end(&transcript[control_after..], &[b"Read", b"only"])
                        .is_some()
                        || ordered_cues_end(
                            &transcript[control_after..],
                            &[b"Native", b"Tool", b"protection", b"unavailable"],
                        )
                        .is_some())
                {
                    control_observed = true;
                }
                if !note_delivery && !controlled {
                    control_after = transcript.len();
                    input.write_all(b"/goal pause\r").unwrap();
                    controlled = true;
                }
                if note_delivery
                    && control_observed
                    && !released
                    && transcript[control_after..]
                        .windows(6)
                        .any(|part| part == b"\x1b[?25h")
                {
                    release_sender.send(()).unwrap();
                    released = true;
                }
                if !note_delivery
                    && controlled
                    && !pause_sent
                    && ordered_cues_end(
                        &transcript[control_after..],
                        &[
                            b"Pausing", b"at", b"the", b"next", b"step;", b"Ctrl+C", b"cancels",
                            b"now",
                        ],
                    )
                    .is_some()
                {
                    release_sender.send(()).unwrap();
                    pause_reply_after = transcript.len();
                    pause_sent = true;
                }
                if !note_delivery
                    && pause_sent
                    && !goal_paused
                    && let Some(offset) = ordered_cues_end(
                        &transcript[pause_reply_after..],
                        &[
                            b"Goal",
                            b"paused;",
                            b"use",
                            b"/goal",
                            b"resume",
                            b"to",
                            b"continue.",
                        ],
                    )
                {
                    goal_paused = true;
                    paused_after = Some(pause_reply_after + offset);
                }
                if note_delivery
                    && released
                    && completed_after.is_none()
                    && let Some(offset) = transcript
                        .windows(b"synthesized".len())
                        .position(|part| part == b"synthesized")
                {
                    completed_after = Some(offset + b"synthesized".len());
                }
                if note_delivery
                    && !exited
                    && completed_after.is_some_and(|offset| {
                        transcript[offset..]
                            .windows(b"ready \xc2\xb7".len())
                            .any(|part| part == b"ready \xc2\xb7")
                    })
                {
                    input.write_all(b"/exit\r").unwrap();
                    exited = true;
                }
                if !note_delivery
                    && !exited
                    && goal_paused
                    && paused_after.is_some_and(|offset| {
                        transcript[offset..]
                            .windows(b"ready \xc2\xb7".len())
                            .any(|part| part == b"ready \xc2\xb7")
                    })
                {
                    input.write_all(b"/exit\r").unwrap();
                    exited = true;
                }
            },
        )
    }));
    let output = match capture {
        Ok(output) => output,
        Err(payload) => {
            let head = escaped(&transcript[..transcript.len().min(512)]);
            let tail = escaped(&transcript[transcript.len().saturating_sub(2048)..]);
            eprintln!(
                "native PTY watchdog stages: declined={declined}, provider_ready={provider_ready}, offered={offered}, controlled={controlled}, control_observed={control_observed}, released={released}, pause_sent={pause_sent}, goal_paused={goal_paused}, exited={exited}; transcript head={head:?}; tail={tail:?}"
            );
            std::panic::resume_unwind(payload);
        }
    };
    assert!(
        declined
            && (!note_delivery || offered && controlled && control_observed && released)
            && (note_delivery || controlled && goal_paused),
        "native interaction stages reached ({note_delivery}, {classic}); output tail: {}",
        String::from_utf8_lossy(&transcript[transcript.len().saturating_sub(4096)..])
    );
    assert_eq!(
        answered_queries > 0,
        classic,
        "Classic uses the explicit bounded cursor peer"
    );
    if !note_delivery {
        assert!(
            exited,
            "paused Session exited ({classic}); output tail: {}",
            String::from_utf8_lossy(&transcript[transcript.len().saturating_sub(4096)..])
        );
        stop_sender.send(()).unwrap();
    } else {
        assert!(
            exited,
            "completed Run returned to the idle prompt ({classic}); output tail: {}",
            String::from_utf8_lossy(&transcript[transcript.len().saturating_sub(4096)..])
        );
    }
    peer.join().unwrap();
    assert_eq!(output.status.code(), Some(0));
    restored(&input, &before);
    let (events, view) = replay(&state);
    assert_eq!(view.runs.len(), 1);
    if note_delivery {
        assert_eq!(view.runs[0].status, arany::RunStatus::Finished);
        assert_eq!(view.runs[0].steering_notes, [NOTE]);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, Event::GoalSteeringNoteRecorded { .. }))
                .count(),
            1
        );
        assert!(
            ordered_cues_end(
                &transcript,
                &[b"Steering", b"note", b"sent", b"to", b"the", b"agent"],
            )
            .is_some()
        );
    } else {
        assert_eq!(view.runs[0].status, arany::RunStatus::Cancelled);
        assert_eq!(view.goals[0].status, GoalStatus::Interrupted);
        assert!(view.runs[0].steering_notes.is_empty());
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.event, Event::ToolStarted { .. }))
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, Event::ProviderCallRecorded { .. }))
                .count(),
            1
        );
    }
}

#[test]
fn native_attached_steering_reaches_synthesis_while_controls_remain_live() {
    let _gate = crate::process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for classic in [false, true] {
        run_native(true, classic);
    }
}

#[test]
fn native_attached_pause_stops_at_the_held_call_boundary() {
    let _gate = crate::process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for classic in [false, true] {
        run_native(false, classic);
    }
}
