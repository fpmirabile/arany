use super::*;
use crate::loopback::{held_stream_provider, read_request, write_profile};
use arany::{CheckDisposition, CheckEvent, CheckId, CheckView, CustomCheckStatus, CustomProfile};
use std::net::TcpListener;

const COLUMNS: usize = 80;
const ROWS: usize = 24;
const MAX_REVIEW_PAGES: usize = 128;
const UNSELECTED_CANARY: &str = "unselected-credential-canary";

#[derive(Clone, Copy)]
pub(super) enum Presentation {
    ScreenReader,
    Inline,
}

impl Presentation {
    fn start(
        self,
        temp: &tempfile::TempDir,
        workspace: &Path,
        state: &Path,
        key: Option<&str>,
    ) -> SetupPty {
        let mode = match self {
            Self::ScreenReader => SetupPtyMode::Manager,
            Self::Inline => SetupPtyMode::InlineManager,
        };
        start_setup_pty_with_key(temp, workspace, state, mode, key)
    }

    pub(super) fn choose(self, pty: &mut SetupPty, choice: usize) -> usize {
        let from = pty.transcript.len();
        let keys = match self {
            Self::ScreenReader => format!("{choice}\r").into_bytes(),
            Self::Inline => [b"\x1b[B".repeat(choice - 1), b"\r".to_vec()].concat(),
        };
        pty.input.write_all(&keys).expect("manager choice");
        from
    }

    pub(super) fn menu(self, pty: &mut SetupPty, from: usize, title: &str) {
        match self {
            Self::ScreenReader => wait_setup_choices(pty, from),
            Self::Inline => {
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut cursor = from;
                loop {
                    let screen = inline_frame(pty, cursor, title);
                    if screen.iter().any(|row| row.trim() == title) {
                        break;
                    }
                    cursor = pty.transcript.len();
                    assert!(Instant::now() < deadline, "manager title frame");
                }
            }
        }
    }

    pub(super) fn composer(self, pty: &mut SetupPty, from: usize) {
        match self {
            Self::ScreenReader => wait_for_after(
                &mut pty.output,
                &mut pty.input,
                &mut pty.transcript,
                &mut pty.answered,
                from,
                b"Input:\r\n",
            ),
            Self::Inline => {
                inline_frame(pty, from, "Ask Arany");
            }
        }
    }
}

pub(super) fn inline_frame(pty: &mut SetupPty, from: usize, needle: &str) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(end) = pty.transcript[from..]
            .windows(6)
            .rposition(|part| matches!(part, b"\x1b[?25l" | b"\x1b[?25h"))
        {
            let screen = crate::screen_rows(&pty.transcript[..from + end + 6], COLUMNS, ROWS);
            if screen.join(" ").contains(needle) {
                return screen;
            }
        }
        pump(
            &mut pty.output,
            &mut pty.input,
            &mut pty.transcript,
            &mut pty.answered,
        );
        assert!(
            Instant::now() < deadline,
            "completed setup frame missing {needle}: {}",
            tail(&pty.transcript)
        );
        thread::yield_now();
    }
}

fn open_check(pty: &mut SetupPty, presentation: Presentation) -> usize {
    let from = presentation.choose(pty, 2);
    presentation.menu(pty, from, "Custom routes");
    let from = presentation.choose(pty, 1);
    presentation.menu(pty, from, "Custom route local");
    presentation.choose(pty, 3)
}

fn accept_check(pty: &mut SetupPty, presentation: Presentation, from: usize) -> usize {
    match presentation {
        Presentation::ScreenReader => {
            acknowledge_linear_pages(
                &mut pty.output,
                &mut pty.input,
                &mut pty.transcript,
                &mut pty.answered,
                (from, b"empty Enter selects 1. Back\r\nInput:\r\n"),
            );
            presentation.choose(pty, 2)
        }
        Presentation::Inline => {
            let mut cursor = from;
            for _ in 0..MAX_REVIEW_PAGES {
                let screen = inline_frame(pty, cursor, "Check custom route?");
                cursor = pty.transcript.len();
                if screen.iter().any(|line| line.contains("Accept")) {
                    pty.input
                        .write_all(b"\t\r")
                        .expect("authorize synthetic check");
                    return cursor;
                }
                pty.input
                    .write_all(b"\r")
                    .expect("review check warning page");
            }
            panic!("bounded warning pages exhausted");
        }
    }
}

pub(super) fn acknowledge_result(
    pty: &mut SetupPty,
    presentation: Presentation,
    from: usize,
) -> (usize, String) {
    let mut text = String::new();
    let mut cursor = from;
    let total = match presentation {
        Presentation::ScreenReader => wait_linear_page(
            &mut pty.output,
            &mut pty.input,
            &mut pty.transcript,
            &mut pty.answered,
            (cursor, 1),
        ),
        Presentation::Inline => page_total(&inline_frame(pty, cursor, "Page 1/")),
    };
    assert!((1..=MAX_REVIEW_PAGES).contains(&total));
    for page in 1..=total {
        match presentation {
            Presentation::ScreenReader => {
                assert_eq!(
                    wait_linear_page(
                        &mut pty.output,
                        &mut pty.input,
                        &mut pty.transcript,
                        &mut pty.answered,
                        (cursor, page)
                    ),
                    total
                );
                text.push_str(&linear_result_page(pty, cursor, page, total));
            }
            Presentation::Inline => {
                let screen = inline_frame(pty, cursor, &format!("Page {page}/"));
                let header = screen
                    .iter()
                    .position(|row| row.contains(&format!("Page {page}/")))
                    .unwrap();
                let (_, body) = screen[header].split_once("\\u{000a}").unwrap();
                text.push_str(body.trim());
            }
        }
        cursor = pty.transcript.len();
        pty.input
            .write_all(b"\r")
            .expect("acknowledge check result page");
    }
    (cursor, text)
}

fn linear_result_page(pty: &mut SetupPty, from: usize, page: usize, total: usize) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    let marker = format!("Choose: Page {page}/{total}\\u{{000a}}");
    loop {
        if let Some((_, body)) =
            String::from_utf8_lossy(&pty.transcript[from..]).split_once(&marker)
            && let Some((line, _)) = body.split_once("\r\n")
        {
            return line.to_owned();
        }
        pump(
            &mut pty.output,
            &mut pty.input,
            &mut pty.transcript,
            &mut pty.answered,
        );
        assert!(Instant::now() < deadline, "complete linear result page");
        thread::yield_now();
    }
}

pub(super) fn assert_result_contains(text: &str, expected: &str) {
    let compact = |value: &str| {
        value
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
    };
    assert!(
        compact(text).contains(&compact(expected)),
        "acknowledged result missing expected status"
    );
}

fn page_total(screen: &[String]) -> usize {
    screen
        .iter()
        .find_map(|row| row.split_once("Page 1/").map(|(_, suffix)| suffix))
        .map(|suffix| {
            suffix
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn manager_check_acknowledgment_selects_verified_route_and_completes_a_turn() {
    for presentation in [Presentation::ScreenReader, Presentation::Inline] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let state = temp.path().join("state");
        std::fs::create_dir(&workspace).unwrap();
        let provider = held_stream_provider(&state, "manager checked answer");
        let mut pty = presentation.start(&temp, &workspace, &state, Some("test-key"));
        let warning = open_check(&mut pty, presentation);
        let checking = accept_check(&mut pty, presentation, warning);
        let (listing, result) = acknowledge_result(&mut pty, presentation, checking);
        assert_result_contains(&result, "Disposition: succeeded");
        assert_result_contains(&result, "Status: verified; ");
        assert_result_contains(&result, " left");
        assert!(
            provider.streaming.try_recv().is_err(),
            "no Run before selection"
        );
        assert_check_facts(&state, CheckDisposition::Succeeded, 3);
        presentation.menu(&mut pty, listing, "Custom routes");
        let action = presentation.choose(&mut pty, 1);
        presentation.menu(&mut pty, action, "Custom route local");
        let inspected = presentation.choose(&mut pty, 1);
        let (listing, details) = acknowledge_result(&mut pty, presentation, inspected);
        assert!(
            !String::from_utf8_lossy(&pty.transcript[inspected..listing])
                .contains("Choose: Protocol:"),
            "inspection must not announce an extra unpaged copy"
        );
        assert_result_contains(&details, "Check: verified; ");
        assert_result_contains(&details, " left");
        presentation.menu(&mut pty, listing, "Custom routes");
        let action = presentation.choose(&mut pty, 1);
        presentation.menu(&mut pty, action, "Custom route local");
        let chat = presentation.choose(&mut pty, 2);
        presentation.composer(&mut pty, chat);
        submit_checked_turn(&mut pty, presentation, &provider);
        pty.input.write_all(b"/exit\r").unwrap();
        finish_check_pty(pty);
        provider.peer.join().unwrap();
        assert_checked_run(&state, &workspace);
    }
}

fn submit_checked_turn(
    pty: &mut SetupPty,
    presentation: Presentation,
    provider: &crate::loopback::HeldStream,
) {
    let from = pty.transcript.len();
    pty.input
        .write_all(b"answer after manager check\r")
        .unwrap();
    wait_provider_gate(pty, &provider.streaming);
    provider.release.send(()).unwrap();
    wait_provider_gate(pty, &provider.consumed);
    if matches!(presentation, Presentation::Inline) {
        inline_frame(pty, from, "● Arany:");
    }
    presentation.composer(pty, from);
    assert!(String::from_utf8_lossy(&pty.transcript).contains("manager checked answer"));
    assert_private_output(&pty.transcript);
}

fn wait_provider_gate(pty: &mut SetupPty, gate: &std::sync::mpsc::Receiver<()>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match gate.try_recv() {
            Ok(()) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                panic!("synthetic Provider gate closed")
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        pump(
            &mut pty.output,
            &mut pty.input,
            &mut pty.transcript,
            &mut pty.answered,
        );
        assert!(
            Instant::now() < deadline,
            "synthetic Provider gate deadline"
        );
        thread::yield_now();
    }
}

fn assert_checked_run(state: &Path, workspace: &Path) {
    runtime().block_on(async {
        let sessions = arany::list_sessions(
            StateRoot::open_existing(state).unwrap(),
            workspace.to_owned(),
        )
        .await
        .unwrap();
        assert_eq!(sessions.len(), 1);
        let store = Store::open_read_only(StateRoot::open_existing(state).unwrap()).unwrap();
        let view = store.load_view(sessions[0].id).await.unwrap().unwrap();
        let events = store.load_session(view.id).await.unwrap();
        assert_eq!(view.runs.len(), 1);
        assert_eq!(view.runs[0].status, arany::RunStatus::Finished);
        assert_eq!(
            view.runs[0].assistant_message.as_deref(),
            Some("manager checked answer")
        );
        assert_eq!(
            view.runs[0]
                .agents
                .iter()
                .map(|agent| agent.provider_calls.len())
                .sum::<usize>(),
            1
        );
        assert_eq!(
            view.runs[0].config.as_ref().unwrap().provider,
            "custom:local"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.event, arany::Event::ProviderSetupEditStarted { .. }))
        );
        assert_private_output(
            arany_cli::presentation::render_session(
                &view,
                &events,
                arany_cli::presentation::Output::Jsonl,
            )
            .as_bytes(),
        );
        store.close().await.unwrap();
    });
}

#[test]
fn manager_empty_account_removal_waits_for_acknowledgment() {
    for presentation in [Presentation::ScreenReader, Presentation::Inline] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let state = temp.path().join("state");
        std::fs::create_dir(&workspace).unwrap();
        let mut pty = presentation.start(&temp, &workspace, &state, None);
        let accounts = presentation.choose(&mut pty, 1);
        presentation.menu(&mut pty, accounts, "API accounts");
        for kind in [1, 2] {
            let removal = presentation.choose(&mut pty, 5);
            presentation.menu(&mut pty, removal, "Remove saved access");
            let result = presentation.choose(&mut pty, kind);
            let (accounts, message) = acknowledge_result(&mut pty, presentation, result);
            assert_result_contains(&message, "No saved account");
            presentation.menu(&mut pty, accounts, "API accounts");
        }
        let chat = presentation.choose(&mut pty, 6);
        presentation.composer(&mut pty, chat);
        pty.input.write_all(b"/exit\r").unwrap();
        finish_check_pty(pty);
        runtime().block_on(async {
            assert!(
                arany::list_sessions(StateRoot::open_existing(&state).unwrap(), workspace,)
                    .await
                    .unwrap()
                    .is_empty()
            );
        });
    }
}

#[test]
fn manager_check_back_before_consent_leaves_no_check_facts_or_requests() {
    for presentation in [Presentation::ScreenReader, Presentation::Inline] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let state = temp.path().join("state");
        std::fs::create_dir(&workspace).unwrap();
        let listener = listener();
        write_profile(&state, listener.local_addr().unwrap().port());
        let mut pty = presentation.start(&temp, &workspace, &state, Some("test-key"));
        let warning = open_check(&mut pty, presentation);
        match presentation {
            Presentation::ScreenReader => wait_for_after(
                &mut pty.output,
                &mut pty.input,
                &mut pty.transcript,
                &mut pty.answered,
                warning,
                b"Choose: Page 1/",
            ),
            Presentation::Inline => {
                inline_frame(&mut pty, warning, "Check custom route?");
            }
        }
        let before = std::fs::read(state.join("provider-profiles.json")).unwrap();
        let back = pty.transcript.len();
        pty.input.write_all(b"\x1b").unwrap();
        presentation.menu(&mut pty, back, "Custom routes");
        presentation.choose(&mut pty, 3);
        finish_check_pty(pty);
        let database = rusqlite::Connection::open_with_flags(
            state.join("events.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        assert_eq!(
            database
                .query_row("SELECT COUNT(*) FROM provider_check_events", [], |row| row
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            0
        );
        assert_eq!(
            std::fs::read(state.join("provider-profiles.json")).unwrap(),
            before
        );
        assert_no_egress(&listener);
    }
}

fn finish_check_pty(mut pty: SetupPty) {
    let result = wait_product(pty.child.take());
    pump(
        &mut pty.output,
        &mut pty.input,
        &mut pty.transcript,
        &mut pty.answered,
    );
    assert!(result.status.success(), "manager lifecycle failed");
    assert_private_output(&pty.transcript);
    assert_private_output(&result.stdout);
    assert_private_output(&result.stderr);
}

#[derive(Clone, Copy)]
enum InterruptedCheck {
    Cancel,
    LostReply,
}

#[test]
fn manager_check_cancellation_and_lost_reply_settle_unknown_and_block_dependent_work() {
    for presentation in [Presentation::ScreenReader, Presentation::Inline] {
        for interruption in [InterruptedCheck::Cancel, InterruptedCheck::LostReply] {
            interrupted_check(presentation, interruption);
        }
    }
}

fn interrupted_check(presentation: Presentation, interruption: InterruptedCheck) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let state = temp.path().join("state");
    std::fs::create_dir(&workspace).unwrap();
    let listener = listener();
    write_profile(&state, listener.local_addr().unwrap().port());
    let mut pty = presentation.start(&temp, &workspace, &state, Some("test-key"));
    let warning = open_check(&mut pty, presentation);
    let checking = accept_check(&mut pty, presentation, warning);
    let mut stream = crate::process::accept_with_deadline(
        &listener,
        Duration::from_secs(10),
        "manager check dispatched",
    );
    read_request(&mut stream);
    match interruption {
        InterruptedCheck::Cancel => {
            pty.input
                .write_all(b"\x03")
                .expect("cancel dispatched check");
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            assert_eq!(
                stream.read(&mut [0]).unwrap(),
                0,
                "cancelled check closes transport"
            );
        }
        InterruptedCheck::LostReply => stream.shutdown(std::net::Shutdown::Both).unwrap(),
    }
    drop(stream);
    let (_, result) = acknowledge_result(&mut pty, presentation, checking);
    assert_result_contains(&result, "Disposition: uncertain");
    assert_private_output(&pty.transcript);
    finish_check_pty(pty);
    assert_check_facts(&state, CheckDisposition::Uncertain, 1);
    assert_dependent_run_refused(&state, &workspace, "custom Provider evidence unavailable");
    assert_no_egress(&listener);
}

#[derive(Clone, Copy)]
enum Refusal {
    MissingCredential,
    Destination,
}

#[test]
fn manager_check_missing_variable_and_destination_refusal_send_no_requests() {
    for presentation in [Presentation::ScreenReader, Presentation::Inline] {
        for refusal in [Refusal::MissingCredential, Refusal::Destination] {
            refused_check(presentation, refusal);
        }
    }
}

fn refused_check(presentation: Presentation, refusal: Refusal) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let state = temp.path().join("state");
    std::fs::create_dir(&workspace).unwrap();
    let listener = listener();
    write_profile(&state, listener.local_addr().unwrap().port());
    if matches!(refusal, Refusal::Destination) {
        let path = state.join("provider-profiles.json");
        let mut profile: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        profile["profiles"][0]["endpoint"] = serde_json::json!(format!(
            "https://localhost:{}/v1/responses",
            listener.local_addr().unwrap().port()
        ));
        std::fs::write(&path, serde_json::to_vec(&profile).unwrap()).unwrap();
    }
    let mut pty = presentation.start(&temp, &workspace, &state, None);
    let warning = open_check(&mut pty, presentation);
    let checking = accept_check(&mut pty, presentation, warning);
    let (listing, result) = acknowledge_result(&mut pty, presentation, checking);
    assert_result_contains(&result, "Disposition: failed");
    match refusal {
        Refusal::MissingCredential => {
            assert_result_contains(&result, "ARANY_PROVIDER_LOCAL_KEY");
            assert_result_contains(&result, "not set");
        }
        Refusal::Destination => {
            assert_result_contains(&result, "destination or route was refused");
            assert!(
                !result.contains("not set"),
                "destination admission precedes variable lookup"
            );
        }
    }
    presentation.menu(&mut pty, listing, "Custom routes");
    presentation.choose(&mut pty, 3);
    finish_check_pty(pty);
    assert_check_facts(&state, CheckDisposition::Failed, 0);
    let reason = match refusal {
        Refusal::MissingCredential => "custom Provider evidence unavailable",
        Refusal::Destination => "invalid custom Provider profile configuration",
    };
    assert_dependent_run_refused(&state, &workspace, reason);
    assert_no_egress(&listener);
}

fn listener() -> TcpListener {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}

fn assert_no_egress(listener: &TcpListener) {
    assert!(
        listener
            .accept()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock),
        "no extra or refused request"
    );
}

fn assert_dependent_run_refused(state: &Path, workspace: &Path, reason: &str) {
    let output = wait_product(
        Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .env(
                "ARANY_TEST_ACCOUNT_ROOT",
                workspace.parent().unwrap().join("account-root"),
            )
            .env("HOME", workspace.parent().unwrap().join("fixture-home"))
            .env(
                "XDG_STATE_HOME",
                workspace.parent().unwrap().join("fixture-state"),
            )
            .env(
                "XDG_DATA_HOME",
                workspace.parent().unwrap().join("fixture-data"),
            )
            .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
            .current_dir(workspace)
            .args([
                "exec",
                "--provider",
                "custom:local",
                "--model",
                "model-1",
                "--collaboration",
                "single",
                "answer",
                "--state-dir",
            ])
            .arg(state)
            .arg("--workspace")
            .arg(workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, format!("error: {reason}\n").as_bytes());
    assert_private_output(&output.stderr);
    runtime().block_on(async {
        assert!(
            arany::list_sessions(
                StateRoot::open_existing(state).unwrap(),
                workspace.to_owned()
            )
            .await
            .unwrap()
            .is_empty()
        );
    });
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn assert_check_facts(state: &Path, disposition: CheckDisposition, calls: u8) {
    let database = rusqlite::Connection::open_with_flags(
        state.join("events.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let mut statement = database
        .prepare("SELECT check_id, payload FROM provider_check_events ORDER BY sequence")
        .unwrap();
    let facts = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(!facts.is_empty());
    let id: CheckId = facts[0].0.parse().unwrap();
    assert!(
        facts
            .iter()
            .all(|(candidate, _)| candidate == &id.to_string())
    );
    let events = facts
        .iter()
        .map(|(_, payload)| serde_json::from_str::<CheckEvent>(payload).unwrap())
        .collect::<Vec<_>>();
    let replay = CheckView::replay(id, &events).unwrap().unwrap();
    assert_eq!(replay.disposition, Some(disposition));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, CheckEvent::ProviderCheckCallStarted { .. }))
            .count(),
        usize::from(calls)
    );
    assert!(matches!(
        events.first(),
        Some(CheckEvent::ProviderCheckStarted { .. })
    ));
    assert!(matches!(
        events.get(1),
        Some(CheckEvent::ProviderCheckEvidenceCleared)
    ));
    assert!(matches!(
        events.last(),
        Some(CheckEvent::ProviderCheckFinished { .. })
    ));
    for event in &events {
        assert_private_output(&serde_json::to_vec(event).unwrap());
    }
    drop(statement);
    drop(database);
    runtime().block_on(async {
        let root = StateRoot::open_existing(state).unwrap();
        let profiles = CustomProfile::load_all(&root).unwrap();
        let statuses = CustomProfile::check_statuses(&root, &profiles)
            .await
            .unwrap();
        assert_eq!(
            matches!(statuses[0].1, CustomCheckStatus::VerifiedUntil(_)),
            disposition == CheckDisposition::Succeeded
        );
        let store = Store::open_read_only(root).unwrap();
        assert_eq!(store.load_check(id).await.unwrap().unwrap(), replay);
        store.close().await.unwrap();
    });
    assert_private_output(&std::fs::read(state.join("events.sqlite3")).unwrap());
}

fn assert_private_output(bytes: &[u8]) {
    for canary in ["test-key", UNSELECTED_CANARY] {
        assert!(
            !bytes
                .windows(canary.len())
                .any(|part| part == canary.as_bytes()),
            "credential disclosed"
        );
    }
}
