use super::{
    active_terminal::{ProductGuard, product_child_of, transcript_field},
    loopback::ChildGuard,
    session_picker::{PtyResponses, pump, tail},
};
use arany::{
    CompactionRecord, CompactionStatus, Event, EventEnvelope, SessionId, StateRoot, Store,
};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use std::{
    io::{Read, Write},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn wait(
    output: &mut impl Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut PtyResponses,
    from: usize,
    needle: &[u8],
) -> usize {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        pump(output, input, transcript, answered);
        if let Some(matched) = crate::process::pty_text_matches(&transcript[from..], needle).next()
        {
            return from + matched.end;
        }
        assert!(
            Instant::now() < end,
            "rewind PTY missing {}: {}",
            tail(needle),
            tail(transcript)
        );
        std::thread::yield_now();
    }
}

#[test]
fn rewind_pty_classic_fullscreen_narrow_and_linear_restore_a_prompt_without_submitting() {
    for (mode, width) in [("--classic", 80), ("", 40), ("--screen-reader", 60)] {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = root.path().join("state");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let source = runtime.block_on(async {
            let engine = super::engine_support::open(
                StateRoot::admit(&state).unwrap(),
                arany::EngineConfig::default(),
            )
            .unwrap();
            let provider = std::sync::Arc::new(
                arany::test_support::scenario::ScriptedProvider::new(vec![
                    arany::test_support::goals::finish_with("First"),
                    arany::test_support::goals::finish_with("Second"),
                ])
                .with_profile("openai", "gpt-5.4"),
            );
            let mut id = None;
            for prompt in ["First prompt", "Second prompt"] {
                let mut request = arany::test_support::scenario::request(&workspace, prompt);
                request.include_paths.clear();
                if id.is_some() {
                    request.title = None;
                }
                request.session_id = id;
                id = Some(
                    super::engine_support::run_to_end(&engine, &provider, request)
                        .await
                        .unwrap()
                        .session_id,
                );
            }
            engine.close().await.unwrap();
            id.unwrap()
        });
        let before = runtime.block_on(async {
            let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
            let events = store.load_session(source).await.unwrap();
            store.close().await.unwrap();
            events
        });
        let shell = format!(
            "trap : INT; stty rows 24 cols {width}; printf 'SHELL_PID:%s\\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" {mode} --no-color --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\" --provider openai --model gpt-5.4; code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$code\""
        );
        let mut command: Command = super::isolation::account_isolated_script(root.path());
        command
            .env_clear()
            .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
            .env("ARANY_TEST_STATE", &state)
            .env("ARANY_TEST_WORKSPACE", &workspace)
            .env("ARANY_TEST_SESSION", source.to_string())
            .env("OPENAI_API_KEY", "synthetic-test-key")
            .env("TERM", "xterm-256color")
            .env("SHELL", "/bin/sh")
            .current_dir(&workspace)
            .args(["-q", "-e", "-c", &shell, "/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = ChildGuard::new(command.spawn().unwrap());
        let mut input = child.child().stdin.take().unwrap();
        let mut output = child.child().stdout.take().unwrap();
        fcntl_setfl(&output, fcntl_getfl(&output).unwrap() | OFlags::NONBLOCK).unwrap();
        let mut transcript = Vec::new();
        let mut answered = PtyResponses::default();
        let linear = mode == "--screen-reader";
        wait(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            0,
            if linear { b"Input:\r\n" } else { b"\x1b[?25h" },
        );
        let shell_pid = transcript_field(&String::from_utf8_lossy(&transcript), "SHELL_PID:")
            .parse::<u32>()
            .unwrap();
        let _product = ProductGuard::new(product_child_of(shell_pid), state.clone());
        if !linear {
            if mode == "--classic" {
                // Folder consent consumes the first Escape; typing before the composer returns would join it as Alt.
                wait(
                    &mut output,
                    &mut input,
                    &mut transcript,
                    &mut answered,
                    0,
                    b"Ask Arany",
                );
            }
            let from = transcript.len();
            // Crossterm collapses adjacent legacy Escape bytes into one event.
            input.write_all(b"saved draft\x1b[27u\x1b[27u").unwrap();
            wait(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                from,
                b"Esc again",
            );
            wait(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                from,
                b"history;",
            );
            let from = transcript.len();
            input.write_all(b"\x1b[A").unwrap();
            wait(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                from,
                b"Hsaved",
            );
            input.write_all(b"\x03").unwrap();
        }
        let from = transcript.len();
        input.write_all(b"/rewind\r").unwrap();
        wait(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            b"Choose a checkpoint",
        );
        if linear {
            wait(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                from,
                b" UTC",
            );
            assert!(!String::from_utf8_lossy(&transcript[from..]).contains(" ms UTC"));
        }
        if !linear {
            let from = transcript.len();
            input.write_all(b"\x1b").unwrap();
            wait(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                from,
                b"cancelled",
            );
            let from = transcript.len();
            input.write_all(b"\x1b[27u\x1b[27u").unwrap();
            wait(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                from,
                b"Esc again",
            );
            wait(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                from,
                b"Choose a checkpoint",
            );
        }
        let from = transcript.len();
        input
            .write_all(if linear { b"2\r" } else { b"\r" })
            .unwrap();
        wait(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            b"conversation",
        );
        let from = transcript.len();
        input
            .write_all(if linear {
                b"Restore conversation\r"
            } else {
                b"\x1b[A\r"
            })
            .unwrap();
        let restored = wait(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            if linear { b"Second prompt" } else { b"Second" },
        );
        if linear {
            assert!(String::from_utf8_lossy(&transcript[from..]).contains("Draft (not sent):"));
        }
        wait(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            restored,
            if linear { b"Input:\r\n" } else { b"\x1b[?25h" },
        );
        let from = transcript.len();
        input.write_all(b"\x03").unwrap();
        wait(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            if linear { b"Input:\r\n" } else { b"\x1b[?25h" },
        );
        let from = transcript.len();
        input.write_all(b"/exit\r").unwrap();
        wait(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            b"TTY_AFTER:",
        );
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            if let Some(status) = child.child().try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < end, "PTY exit deadline");
            std::thread::yield_now();
        }
        let text = String::from_utf8_lossy(&transcript);
        assert_eq!(
            transcript_field(&text, "TTY_BEFORE:"),
            transcript_field(&text, "TTY_AFTER:")
        );
        runtime.block_on(async {
            let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
            let after = store.load_session(source).await.unwrap();
            assert_eq!(after[..before.len()], before[..]);
            assert!(matches!(
                after[before.len()..],
                [] | [EventEnvelope {
                    event: Event::SessionDefaultChanged { .. },
                    ..
                }]
            ));
            store.close().await.unwrap();
            let sessions =
                arany::list_sessions(StateRoot::open_existing(&state).unwrap(), workspace.clone())
                    .await
                    .unwrap();
            assert_eq!(sessions.len(), 2);
            let fork = sessions
                .iter()
                .find(|session| session.id != source)
                .unwrap();
            let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
            let view = store.load_view(fork.id).await.unwrap().unwrap();
            assert_eq!(view.runs.len(), 1);
            assert!(view.goals.is_empty());
            store.close().await.unwrap();
        });
    }
}
#[tokio::test]
async fn show_inspects_invalid_history_read_only_and_keeps_valid_output_stable() {
    use arany::test_support::{engine, goals, scenario};
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state_path = temp.path().join("state");
    let engine = engine::open(
        StateRoot::admit(&state_path).unwrap(),
        arany::EngineConfig::default(),
    )
    .unwrap();
    let provider = std::sync::Arc::new(
        scenario::ScriptedProvider::new(vec![goals::finish_with("Committed answer")])
            .with_profile("openai", "gpt-5.4"),
    );
    let mut request = scenario::request(&workspace, "Show committed history");
    request.include_paths.clear();
    let outcome = super::engine_support::run_to_end(&engine, &provider, request)
        .await
        .unwrap();
    let second_provider = std::sync::Arc::new(
        scenario::ScriptedProvider::new(vec![goals::finish_with("Other session answer")])
            .with_profile("openai", "gpt-5.4"),
    );
    let mut second_request = scenario::request(&workspace, "Inspect another history");
    second_request.include_paths.clear();
    let second_outcome =
        super::engine_support::run_to_end(&engine, &second_provider, second_request)
            .await
            .unwrap();
    engine.close().await.unwrap();

    assert_valid_show_output(&workspace, &state_path, outcome.session_id);
    let valid_events = stored_events(&state_path, outcome.session_id);
    assert_other_session_corruption(
        &workspace,
        &state_path,
        outcome.session_id,
        second_outcome.session_id,
    );
    assert_gap_after_selected_session(
        &workspace,
        &state_path,
        outcome.session_id,
        second_outcome.session_id,
    );
    assert_mixed_decode_and_transition(&workspace, &state_path, outcome.session_id, &valid_events);
    assert_mixed_reference_and_transition(
        &workspace,
        &state_path,
        outcome.session_id,
        &valid_events,
    );
    assert_payload_failure_and_selected_gap(
        &workspace,
        &state_path,
        outcome.session_id,
        &valid_events,
    )
    .await;
}

fn assert_valid_show_output(
    workspace: &std::path::Path,
    state_path: &std::path::Path,
    session: SessionId,
) {
    for format in ["text", "jsonl"] {
        let output = show_session(workspace, state_path, session, format);
        assert!(output.status.success());
        assert_eq!(
            output.stdout,
            show_session(workspace, state_path, session, format).stdout
        );
    }
}

type StoredEvent = (
    i64,
    String,
    i64,
    String,
    Option<String>,
    Option<String>,
    i64,
);

fn show_session(
    workspace: &std::path::Path,
    state_path: &std::path::Path,
    session: SessionId,
    format: &str,
) -> std::process::Output {
    show_selector(workspace, state_path, &session.to_string(), format)
}

fn show_selector(
    workspace: &std::path::Path,
    state_path: &std::path::Path,
    selector: &str,
    format: &str,
) -> std::process::Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_arany"));
    command
        .env_clear()
        .current_dir(workspace)
        .args(["show", "--output", format, "--state-dir"])
        .arg(state_path)
        .arg(selector);
    crate::process_output_before_deadline(command)
}

fn stored_events(state_path: &std::path::Path, session: SessionId) -> Vec<StoredEvent> {
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
    let mut statement = connection
        .prepare("SELECT sequence, kind, event_version, payload, run_id, agent_run_id, created_at_ms FROM events WHERE session_id = ?1 ORDER BY sequence")
        .unwrap();
    statement
        .query_map([session.to_string()], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn assert_other_session_corruption(
    workspace: &std::path::Path,
    state_path: &std::path::Path,
    session: SessionId,
    other_session: SessionId,
) {
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
    let (sequence, kind, payload): (i64, String, String) = connection
        .query_row(
            "SELECT sequence, kind, payload FROM events WHERE session_id = ?1 ORDER BY sequence DESC LIMIT 1",
            [other_session.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    connection
        .execute(
            "UPDATE events SET payload = '{}' WHERE session_id = ?1 AND sequence = ?2",
            rusqlite::params![other_session.to_string(), sequence],
        )
        .unwrap();
    drop(connection);
    let text =
        String::from_utf8(show_session(workspace, state_path, session, "text").stdout).unwrap();
    assert!(text.contains(&format!(
        "History invalid at sequence {sequence}: invalid Event payload"
    )));
    assert!(text.contains("Committed answer"));
    assert!(!text.contains(&format!("{sequence} {kind}")));
    let jsonl =
        String::from_utf8(show_session(workspace, state_path, session, "jsonl").stdout).unwrap();
    assert!(jsonl.contains(&format!(
        "\"History invalid at sequence {sequence}: invalid Event payload"
    )));
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
    let run_id: String = connection
        .query_row(
            "SELECT run_id FROM events WHERE session_id = ?1 AND run_id IS NOT NULL LIMIT 1",
            [session.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    drop(connection);
    for selector in [run_id.as_str(), "00000000-0000-4000-8000-000000000000"] {
        let output = show_selector(workspace, state_path, selector, "text");
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE events SET payload = ?1 WHERE session_id = ?2 AND sequence = ?3",
            rusqlite::params![payload, other_session.to_string(), sequence],
        )
        .unwrap();
}

fn assert_gap_after_selected_session(
    workspace: &std::path::Path,
    state_path: &std::path::Path,
    session: SessionId,
    later_session: SessionId,
) {
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
    let sequence: i64 = connection
        .query_row(
            "SELECT MAX(sequence) FROM events WHERE session_id = ?1",
            [later_session.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    connection
        .execute(
            "UPDATE events SET sequence = sequence + 1 WHERE session_id = ?1 AND sequence = ?2",
            rusqlite::params![later_session.to_string(), sequence],
        )
        .unwrap();
    drop(connection);
    let text =
        String::from_utf8(show_session(workspace, state_path, session, "text").stdout).unwrap();
    assert!(text.contains("invalid journal sequence"));
    assert!(text.contains("Committed answer"));
    assert!(!text.contains("Other session answer"));
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE events SET sequence = sequence - 1 WHERE session_id = ?1 AND sequence = ?2",
            rusqlite::params![later_session.to_string(), sequence + 1],
        )
        .unwrap();
}

fn assert_mixed_decode_and_transition(
    workspace: &std::path::Path,
    state_path: &std::path::Path,
    session: SessionId,
    events: &[StoredEvent],
) {
    let first = events.first().unwrap();
    let last = events.last().unwrap();
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE events SET kind = 'SessionRenamed', payload = '{\"title\":\"Mixed fault\"}' WHERE session_id = ?1 AND sequence = ?2",
            rusqlite::params![session.to_string(), first.0],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE events SET payload = '{}' WHERE session_id = ?1 AND sequence = ?2",
            rusqlite::params![session.to_string(), last.0],
        )
        .unwrap();
    drop(connection);
    let text =
        String::from_utf8(show_session(workspace, state_path, session, "text").stdout).unwrap();
    assert!(text.contains(&format!(
        "History invalid at sequence {}: invalid Session transition",
        first.0
    )));
    let jsonl =
        String::from_utf8(show_session(workspace, state_path, session, "jsonl").stdout).unwrap();
    assert!(jsonl.contains(&format!(
        "\"History invalid at sequence {}: invalid Session transition",
        first.0
    )));
    restore_event(state_path, session, first);
    restore_event(state_path, session, last);
}

fn assert_mixed_reference_and_transition(
    workspace: &std::path::Path,
    state_path: &std::path::Path,
    session: SessionId,
    events: &[StoredEvent],
) {
    let last = events.last().unwrap();
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
    let (sequence, payload): (i64, String) = connection
        .query_row(
            "SELECT sequence, payload FROM events WHERE session_id = ?1 AND kind = 'RunStarted' ORDER BY sequence LIMIT 1",
            [session.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut run_start: serde_json::Value = serde_json::from_str(&payload).unwrap();
    run_start["config"]["compaction_event_sequence"] = serde_json::json!(u64::MAX);
    run_start["config"]["compaction_content_digest"] = serde_json::json!(vec![1_u8; 32]);
    connection
        .execute(
            "UPDATE events SET payload = ?1 WHERE session_id = ?2 AND sequence = ?3",
            rusqlite::params![
                serde_json::to_string(&run_start).unwrap(),
                session.to_string(),
                sequence
            ],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE events SET kind = ?1, event_version = ?2, payload = ?3 WHERE session_id = ?4 AND sequence = ?5",
            rusqlite::params![events[0].1, events[0].2, events[0].3, session.to_string(), last.0],
        )
        .unwrap();
    drop(connection);
    let text =
        String::from_utf8(show_session(workspace, state_path, session, "text").stdout).unwrap();
    assert!(text.contains(&format!(
        "History invalid at sequence {sequence}: invalid compaction snapshot"
    )));
    assert!(text.contains("Show committed history"));
    let jsonl =
        String::from_utf8(show_session(workspace, state_path, session, "jsonl").stdout).unwrap();
    assert!(jsonl.contains(&format!(
        "\"History invalid at sequence {sequence}: invalid compaction snapshot"
    )));
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE events SET payload = ?1 WHERE session_id = ?2 AND sequence = ?3",
            rusqlite::params![payload, session.to_string(), sequence],
        )
        .unwrap();
    restore_event(state_path, session, last);
}

async fn assert_payload_failure_and_selected_gap(
    workspace: &std::path::Path,
    state_path: &std::path::Path,
    session: SessionId,
    events: &[StoredEvent],
) {
    assert_payload_failure(workspace, state_path, session, events).await;
    assert_selected_gap(workspace, state_path, session, events).await;
}

async fn assert_payload_failure(
    workspace: &std::path::Path,
    state_path: &std::path::Path,
    session: SessionId,
    events: &[StoredEvent],
) {
    let last = events.last().unwrap();
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE events SET payload = '{}' WHERE session_id = ?1 AND sequence = ?2",
            rusqlite::params![session.to_string(), last.0],
        )
        .unwrap();
    drop(connection);
    let corrupted_events = stored_events(state_path, session);
    let store =
        arany::Store::open_read_only(StateRoot::open_existing(state_path).unwrap()).unwrap();
    assert!(matches!(
        store.load_session(session).await,
        Err(arany::StoreError::InvalidHistory | arany::StoreError::InvalidSnapshot)
    ));
    store.close().await.unwrap();
    let output = show_session(workspace, state_path, session, "text");
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains(&format!(
        "History invalid at sequence {}: invalid Event payload",
        last.0
    )));
    assert!(text.contains("Committed answer"));
    let jsonl =
        String::from_utf8(show_session(workspace, state_path, session, "jsonl").stdout).unwrap();
    assert!(jsonl.contains(&format!(
        "\"History invalid at sequence {}: invalid Event payload",
        last.0
    )));
    assert_eq!(stored_events(state_path, session), corrupted_events);
    restore_event(state_path, session, last);
    assert_hostile_kind(workspace, state_path, session, last);
    assert_oversized_kind(workspace, state_path, session, last);
    assert_malformed_text_kind(workspace, state_path, session, last);
}

fn assert_hostile_kind(
    workspace: &std::path::Path,
    state_path: &std::path::Path,
    session: SessionId,
    event: &StoredEvent,
) {
    let kind = "Bad\u{0085}\u{202e}Kind";
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE events SET kind = ?1 WHERE session_id = ?2 AND sequence = ?3",
            rusqlite::params![kind, session.to_string(), event.0],
        )
        .unwrap();
    drop(connection);
    let text = show_session(workspace, state_path, session, "text");
    assert!(
        String::from_utf8(text.stdout)
            .unwrap()
            .contains("unknown Event kind or version")
    );
    let jsonl = show_session(workspace, state_path, session, "jsonl");
    let jsonl = String::from_utf8(jsonl.stdout).unwrap();
    assert!(jsonl.contains("unknown Event kind or version"));
    assert!(jsonl.contains("Bad\\u0085\\u202eKind"));
    assert!(!jsonl.contains('\u{0085}') && !jsonl.contains('\u{202e}'));
    restore_event(state_path, session, event);
}

fn assert_oversized_kind(
    workspace: &std::path::Path,
    state_path: &std::path::Path,
    session: SessionId,
    event: &StoredEvent,
) {
    let oversized_kind = "K".repeat(64 * 1024);
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE events SET kind = ?1 WHERE session_id = ?2 AND sequence = ?3",
            rusqlite::params![oversized_kind, session.to_string(), event.0],
        )
        .unwrap();
    drop(connection);
    let output = show_session(workspace, state_path, session, "jsonl");
    let jsonl = String::from_utf8(output.stdout).unwrap();
    assert!(jsonl.contains("[invalid kind]"));
    assert!(jsonl.len() < 64 * 1024);
    restore_event(state_path, session, event);
}

fn assert_malformed_text_kind(
    workspace: &std::path::Path,
    state_path: &std::path::Path,
    session: SessionId,
    event: &StoredEvent,
) {
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE events SET kind = CAST(x'ff' AS TEXT) WHERE session_id = ?1 AND sequence = ?2",
            rusqlite::params![session.to_string(), event.0],
        )
        .unwrap();
    drop(connection);
    let output = show_session(workspace, state_path, session, "jsonl");
    let jsonl = String::from_utf8(output.stdout).unwrap();
    assert!(jsonl.contains("invalid Event payload"));
    assert!(jsonl.contains("[invalid kind]"));
    assert!(jsonl.contains("undecodable"));
    restore_event(state_path, session, event);
}

async fn assert_selected_gap(
    workspace: &std::path::Path,
    state_path: &std::path::Path,
    session: SessionId,
    events: &[StoredEvent],
) {
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
    let missing = events[2].0;
    connection
        .execute(
            "DELETE FROM events WHERE session_id = ?1 AND sequence = ?2",
            rusqlite::params![session.to_string(), missing],
        )
        .unwrap();
    let tail: i64 = connection
        .query_row(
            "SELECT sequence FROM events WHERE session_id = ?1 AND sequence > ?2 ORDER BY sequence LIMIT 1",
            rusqlite::params![session.to_string(), missing],
            |row| row.get(0),
        )
        .unwrap();
    drop(connection);
    let gapped_events = stored_events(state_path, session);
    let store =
        arany::Store::open_read_only(StateRoot::open_existing(state_path).unwrap()).unwrap();
    assert!(matches!(
        store.load_session(session).await,
        Err(arany::StoreError::InvalidHistory)
    ));
    store.close().await.unwrap();
    let output = show_session(workspace, state_path, session, "text");
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains(&format!(
        "History invalid at sequence {tail}: invalid journal sequence"
    )));
    assert!(text.contains(&format!("{tail} ")));
    assert_eq!(stored_events(state_path, session), gapped_events);
}

fn restore_event(state_path: &std::path::Path, session: SessionId, event: &StoredEvent) {
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE events SET kind = ?1, event_version = ?2, payload = ?3 WHERE session_id = ?4 AND sequence = ?5",
            rusqlite::params![event.1, event.2, event.3, session.to_string(), event.0],
        )
        .unwrap();
}

fn prefix_digest(state_path: &std::path::Path, session_id: SessionId, through: i64) -> [u8; 32] {
    use sha2::Digest;
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
    let mut statement = connection
        .prepare("SELECT sequence, session_id, run_id, agent_run_id, kind, payload, created_at_ms FROM events WHERE session_id = ?1 AND sequence <= ?2 ORDER BY sequence")
        .unwrap();
    let rows = statement
        .query_map(rusqlite::params![session_id.to_string(), through], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let mut digest = sha2::Sha256::new();
    digest.update(b"arany-session-prefix-v1\0");
    for (sequence, session, run, agent, kind, payload, created_at_ms) in rows {
        digest.update((sequence as u64).to_be_bytes());
        digest.update(session.as_bytes());
        for scope in [run, agent] {
            if let Some(scope) = scope {
                digest.update([1]);
                digest.update(scope.as_bytes());
            } else {
                digest.update([0]);
            }
        }
        for field in [kind, payload] {
            digest.update((field.len() as u32).to_be_bytes());
            digest.update(field.as_bytes());
        }
        digest.update(created_at_ms.to_be_bytes());
    }
    digest.finalize().into()
}

async fn append_test_compaction(state_path: &std::path::Path, session: SessionId) -> i64 {
    use sha2::Digest;
    let root = StateRoot::open_existing(state_path).unwrap();
    let store = arany::Store::open(root).unwrap();
    let events = store.load_session(session).await.unwrap();
    let view = store.load_view(session).await.unwrap().unwrap();
    let run = view.runs.last().unwrap();
    let config = run.config.as_ref().unwrap();
    let covered_sequence = run.finished_sequence.unwrap();
    let record = CompactionRecord {
        rate_limits: None,
        covered_run_id: run.id,
        covered_sequence,
        source_digest: prefix_digest(state_path, session, covered_sequence as i64),
        recent_files: Vec::new(),
        provider: config.provider.profile(),
        model: config.model.clone(),
        prompt_version: 1,
        prompt: arany::EffectivePrompts::default().pin(arany::PromptFile::Compaction),
        schema_version: 1,
        compiler_version: 5,
        source_bytes: 1,
        source_tokens_estimate: 1,
        input_tokens: None,
        output_tokens: None,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        response_id: None,
        wire_provenance: None,
        chatgpt_provenance: None,
        output_token_bound: config.output_token_bound,
        status: CompactionStatus::Succeeded {
            summary: "A durable inherited summary".into(),
            content_digest: sha2::Sha256::digest(b"A durable inherited summary").into(),
            summary_bytes: 27,
            summary_tokens_estimate: 7,
        },
    };
    store
        .append(
            session,
            arany::Event::ContextCompacted {
                record,
                active_ms: None,
            },
        )
        .await
        .unwrap();
    store.close().await.unwrap();
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
    let sequence = connection
        .query_row(
            "SELECT sequence FROM events WHERE session_id = ?1 AND kind = 'ContextCompacted' ORDER BY sequence DESC LIMIT 1",
            [session.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(events.last().unwrap().sequence < sequence as u64);
    sequence
}

#[tokio::test]
async fn show_inspects_fork_with_inherited_compaction_and_bad_source_tail() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state_path = temp.path().join("state");
    let (source, fork, compaction_sequence) = create_compacted_fork(&workspace, &state_path).await;
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
    let sequence: i64 = connection
        .query_row(
            "SELECT sequence FROM events WHERE session_id = ?1 AND kind = 'SessionRenamed' ORDER BY sequence DESC LIMIT 1",
            [source.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    connection
        .execute(
            "UPDATE events SET payload = '{}' WHERE session_id = ?1 AND sequence = ?2",
            rusqlite::params![source.to_string(), sequence],
        )
        .unwrap();
    drop(connection);

    let store =
        arany::Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
    let inspection = store.inspect_session(fork).await.unwrap();
    let view = inspection.prefix_view.unwrap();
    assert_eq!(view.runs.len(), 2);
    assert!(
        view.compactions
            .iter()
            .any(|compaction| compaction.event_sequence == compaction_sequence as u64)
    );
    assert!(matches!(
        inspection.events.as_slice(),
        [event] if matches!(event.event, Ok(EventEnvelope { event: Event::SessionForked { .. }, .. }))
    ));
    store.close().await.unwrap();
    show_fork_output(&workspace, &state_path, fork, sequence);
}

async fn create_compacted_fork(
    workspace: &std::path::Path,
    state_path: &std::path::Path,
) -> (SessionId, SessionId, i64) {
    use arany::test_support::{engine, goals, scenario};
    let engine = engine::open(
        StateRoot::admit(state_path).unwrap(),
        arany::EngineConfig::default(),
    )
    .unwrap();
    let provider = std::sync::Arc::new(
        scenario::ScriptedProvider::new(vec![goals::finish_with("First committed answer")])
            .with_profile("openai", "gpt-5.4"),
    );
    let mut request = scenario::request(workspace, "Create a compaction boundary");
    request.include_paths.clear();
    let source = super::engine_support::run_to_end(&engine, &provider, request)
        .await
        .unwrap()
        .session_id;
    engine.close().await.unwrap();
    let compaction_sequence = append_test_compaction(state_path, source).await;

    let engine = engine::open(
        StateRoot::open_existing(state_path).unwrap(),
        arany::EngineConfig::default(),
    )
    .unwrap();
    let second_provider = std::sync::Arc::new(
        scenario::ScriptedProvider::new(vec![goals::finish_with("Second committed answer")])
            .with_profile("openai", "gpt-5.4"),
    );
    let mut second_request = scenario::request(workspace, "Select inherited compaction");
    second_request.session_id = Some(source);
    second_request.title = None;
    second_request.include_paths.clear();
    super::engine_support::run_to_end(&engine, &second_provider, second_request)
        .await
        .unwrap();
    let store =
        arany::Store::open_read_only(StateRoot::open_existing(state_path).unwrap()).unwrap();
    let view = store.load_view(source).await.unwrap().unwrap();
    assert_eq!(
        view.runs
            .last()
            .unwrap()
            .config
            .as_ref()
            .unwrap()
            .compaction_event_sequence,
        Some(compaction_sequence as u64)
    );
    store.close().await.unwrap();
    let fork = engine
        .fork_session(
            source,
            workspace.to_path_buf(),
            Some("Compacted fork".into()),
        )
        .await
        .unwrap();
    arany::rename_session(
        StateRoot::open_existing(state_path).unwrap(),
        workspace.to_path_buf(),
        source,
        "After fork boundary".into(),
    )
    .await
    .unwrap();
    engine.close().await.unwrap();
    (source, fork, compaction_sequence)
}

fn show_fork_output(
    workspace: &std::path::Path,
    state_path: &std::path::Path,
    fork: SessionId,
    sequence: i64,
) {
    let show = |format: &str| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_arany"));
        command
            .env_clear()
            .current_dir(workspace)
            .args(["show", "--output", format, "--state-dir"])
            .arg(state_path)
            .arg(fork.to_string());
        crate::process_output_before_deadline(command)
    };
    let text_output = show("text");
    assert_eq!(text_output.status.code(), Some(1));
    let text = String::from_utf8(text_output.stdout).unwrap();
    assert!(text.contains(&format!(
        "History invalid at sequence {sequence}: invalid Event payload; shown read-only, not resumable"
    )));
    assert!(text.contains("SessionForked"));
    assert!(!text.contains("First committed answer"));
    assert!(!text.contains("Second committed answer"));
    assert!(text_output.stderr.is_empty());
    let records: Vec<serde_json::Value> = String::from_utf8(show("jsonl").stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(records.iter().any(|line| {
        line.get("notice").and_then(serde_json::Value::as_str).is_some_and(|notice| {
            notice == format!("History invalid at sequence {sequence}: invalid Event payload; shown read-only, not resumable")
        })
    }));
    assert!(records.iter().any(|line| {
        line.get("kind").and_then(serde_json::Value::as_str) == Some("SessionForked")
            && line
                .get("payload")
                .is_some_and(serde_json::Value::is_object)
    }));
}
