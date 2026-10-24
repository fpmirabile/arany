use arany::WorkspaceIdentity;
use arany::{
    AgentRole, AgentRunId, AgentStatus, CollaborationPolicy, ContextUsage, EngineError, Event,
    RunConfig, RunId, RunStatus, SessionDefaults, SessionId, SessionView, StateRoot, Store,
    StoreError, continue_session, create_session, list_sessions, rename_session, resume_session,
    set_session_defaults,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn assert_git_range_outputs(
    run: &arany::RunView,
    workspace: &std::path::Path,
    home: &std::path::Path,
    git: &str,
) {
    let output = |index: usize| {
        serde_json::from_str::<serde_json::Value>(
            &run.tools[index].observation.as_ref().unwrap().output,
        )
        .unwrap()
    };
    let oid = |revision: &str| {
        let result = std::process::Command::new(git)
            .args(["rev-parse", revision])
            .current_dir(workspace)
            .env_clear()
            .env("HOME", home)
            .env("LC_ALL", "C")
            .bounded_output_for(std::time::Duration::from_secs(20), 16 * 1024)
            .unwrap();
        assert!(result.status.success());
        String::from_utf8(result.stdout).unwrap().trim().to_owned()
    };
    let range = output(5);
    assert_eq!(
        range["comparison"],
        serde_json::json!({
            "base": {"revision": "HEAD~1", "oid": oid("HEAD~1")},
            "head": {"revision": "protected", "oid": oid("protected")}
        })
    );
    assert_eq!(
        range["commits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|commit| commit["subject"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["second commit"]
    );
    assert!(!range.to_string().contains("BRANCH_DISCLOSURE_CANARY"));
    assert!(!range.to_string().contains("private fixture"));
    let diff = output(6);
    assert_eq!(
        diff["comparison"],
        serde_json::json!({
            "base": {"revision": "HEAD~1", "oid": oid("HEAD~1")},
            "head": {"revision": "HEAD", "oid": oid("HEAD")}
        })
    );
    assert!(diff["patch"].as_str().unwrap().contains("+bravo"));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn assert_git_range_continuation(run: &arany::RunView, requests: &[arany::ProviderRequest]) {
    for (request_index, tool_index) in [(6, 5), (7, 6), (8, 7), (9, 8)] {
        assert_eq!(
            requests[request_index]
                .input
                .tools
                .as_ref()
                .unwrap()
                .observations()
                .last()
                .unwrap()
                .output,
            run.tools[tool_index].observation.as_ref().unwrap().output
        );
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn assert_git_range_replay(run: &arany::RunView, replay: &arany::RunView) {
    for index in [5, 6, 7, 8] {
        assert_eq!(
            replay.tools[index].observation,
            run.tools[index].observation
        );
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn assert_restricted_git_branch_journey(branches: &serde_json::Value, workspace: &std::path::Path) {
    let timed_out = branches["timed_out"].as_bool().unwrap();
    let rows = branches["branches"].as_array().unwrap();
    assert_eq!(branches["history_complete"], true);
    assert_eq!(branches["truncated"], timed_out);
    if branches["comparison_base"].is_null() {
        assert!(timed_out && rows.is_empty());
        return;
    }
    assert_eq!(branches["comparison_base"]["revision"], "HEAD~1");
    assert!(rows.len() <= 3);
    if !timed_out {
        assert_eq!(rows.len(), 3);
    }
    for (index, row) in rows.iter().enumerate() {
        let name = [
            "refs/heads/main",
            "refs/heads/protected",
            "refs/remotes/review/topic",
        ][index];
        let oid = std::fs::read_to_string(workspace.join(".git").join(name)).unwrap();
        assert_eq!(row["name"], name);
        assert_eq!(row["name_truncated"], false);
        assert_eq!(row["oid"], oid.trim());
        assert_eq!(row["kind"], if index == 2 { "remote" } else { "local" });
        assert_eq!(row["current"], index == 0);
        assert!(row["symbolic_target"].is_null());
        assert!(row["ahead"].is_null() && row["behind"].is_null());
        assert_eq!(row["counts_withheld"], true);
        assert_eq!(row["counts_capped"], false);
        assert!(row["subject"].is_null());
        assert_eq!(row["subject_withheld"], true);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn assert_closed_git_views(
    state: &std::path::Path,
    home: &std::path::Path,
    workspace: &std::path::Path,
    events: &[arany::EventEnvelope],
    view: &SessionView,
) {
    use arany_cli::presentation::{Output, render_session};
    use arany_cli::protocol::EventLine;
    for format in ["text", "jsonl"] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .env("HOME", home)
            .current_dir(workspace)
            .args(["show", "--state-dir"])
            .arg(state)
            .args(["--output", format])
            .arg(view.id.to_string())
            .bounded_output_for(std::time::Duration::from_secs(30), 4 * 1024 * 1024)
            .unwrap();
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(output.stderr, b"");
        let canary = b"BRANCH_DISCLOSURE_CANARY";
        for bytes in [
            &output.stdout,
            &output.stderr,
            &std::fs::read(state.join("events.sqlite3")).unwrap(),
        ] {
            assert!(!bytes.windows(canary.len()).any(|window| window == canary));
        }
        if format == "text" {
            assert_eq!(
                output.stdout,
                render_session(view, events, Output::Text).as_bytes()
            );
        } else {
            let actual: Vec<EventLine> = String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(
                actual,
                events.iter().map(EventLine::from).collect::<Vec<_>>()
            );
        }
    }
}

use process::BoundedOutput;
use std::process::Command;

#[cfg(unix)]
#[path = "session_run/worker.rs"]
mod worker;

#[cfg(unix)]
fn reply_positioned_cursor_queries(
    input: &mut impl std::io::Write,
    bytes: &[u8],
    answered: &mut usize,
) {
    for (index, (offset, _)) in bytes
        .windows(4)
        .enumerate()
        .filter(|(_, part)| *part == b"\x1b[6n")
        .enumerate()
    {
        if index < *answered {
            continue;
        }
        let position = bytes[..offset]
            .windows(2)
            .enumerate()
            .rev()
            .filter(|(_, part)| *part == b"\x1b[")
            .find_map(|(start, _)| {
                let tail = &bytes[start + 2..offset];
                let end = tail.iter().position(|byte| (0x40..=0x7e).contains(byte))?;
                (tail[end] == b'H').then_some(&tail[..end])
            })
            .expect("Classic query follows an explicit cursor position");
        assert!(
            position.len() <= 16
                && position
                    .iter()
                    .all(|byte| byte.is_ascii_digit() || *byte == b';')
        );
        input.write_all(b"\x1b[").unwrap();
        input.write_all(position).unwrap();
        input.write_all(b"R").unwrap();
        *answered += 1;
    }
}

#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
pub(crate) fn tail(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .rev()
        .take(512)
        .collect::<String>()
        .chars()
        .rev()
        .flat_map(char::escape_default)
        .collect()
}

#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
pub(crate) struct PtyResponses {
    pub(crate) cursor_queries: usize,
    consent_from: usize,
    auto_consent: bool,
}

#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
impl Default for PtyResponses {
    fn default() -> Self {
        Self {
            cursor_queries: 0,
            consent_from: 0,
            auto_consent: true,
        }
    }
}

#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
impl PtyResponses {
    pub(crate) fn manual_consent() -> Self {
        Self {
            auto_consent: false,
            ..Self::default()
        }
    }
}

#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
pub(crate) fn pump(
    output: &mut impl std::io::Read,
    input: &mut impl std::io::Write,
    transcript: &mut Vec<u8>,
    answered: &mut PtyResponses,
) {
    loop {
        let mut chunk = [0; 4096];
        match output.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                assert!(
                    transcript.len() + count <= 64 * 1024,
                    "bounded PTY transcript: {} bytes captured; last bytes: {}",
                    transcript.len(),
                    tail(&transcript[transcript.len().saturating_sub(300)..])
                );
                transcript.extend_from_slice(&chunk[..count]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => panic!("PTY read failed: {error}"),
        }
    }
    let queries = transcript
        .windows(4)
        .filter(|part| *part == b"\x1b[6n")
        .count();
    assert!(queries <= 128, "bounded cursor queries");
    reply_positioned_cursor_queries(input, transcript, &mut answered.cursor_queries);
    if answered.auto_consent {
        let mut declined = false;
        process::decline_workspace_consent(
            input,
            &transcript[answered.consent_from..],
            &mut declined,
        );
        if declined {
            answered.consent_from = transcript.len();
        }
    }
}

#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
fn screen_rows(transcript: &[u8], columns: usize, rows: usize) -> Vec<String> {
    terminal_grid(transcript, columns, rows).1
}

// Decodes cursor moves, clears, line feeds and scroll regions; rows leaving the top of
// the screen enter the returned scrollback, as in an emulator.
#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
fn terminal_grid(transcript: &[u8], columns: usize, rows: usize) -> (Vec<String>, Vec<String>) {
    let blank = || vec![' '; columns];
    let mut grid = vec![blank(); rows];
    let mut scrollback = Vec::new();
    let (mut top, mut bottom) = (0, rows - 1);
    let (mut row, mut column) = (0, 0);
    let mut scroll_up = |grid: &mut Vec<Vec<char>>, top: usize, bottom: usize, count: usize| {
        for _ in 0..count {
            let line = grid.remove(top);
            if top == 0 {
                scrollback.push(line);
            }
            grid.insert(bottom, blank());
        }
    };
    let text = String::from_utf8_lossy(transcript);
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\x1b' if chars.peek() == Some(&'[') => {
                chars.next();
                let mut parameters = String::new();
                let mut command = None;
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        command = Some(next);
                        break;
                    }
                    parameters.push(next);
                }
                let mut values = parameters
                    .trim_start_matches('?')
                    .split(';')
                    .map(|value| value.parse::<usize>().unwrap_or(0));
                let first = values.next().unwrap_or(0);
                match command {
                    Some('H') => {
                        row = first.max(1).min(rows) - 1;
                        column = values.next().unwrap_or(1).max(1).min(columns) - 1;
                    }
                    Some('J') if first == 2 || first == 3 => {
                        grid.iter_mut().for_each(|line| line.fill(' '));
                    }
                    Some('J') => {
                        grid[row][column..].fill(' ');
                        grid[row + 1..].iter_mut().for_each(|line| line.fill(' '));
                    }
                    Some('K') => grid[row][column..].fill(' '),
                    Some('r') if !parameters.starts_with('?') => {
                        top = first.max(1).min(rows) - 1;
                        bottom = values.next().unwrap_or(rows).clamp(1, rows) - 1;
                        if parameters.is_empty() || top >= bottom {
                            (top, bottom) = (0, rows - 1);
                        }
                        (row, column) = (0, 0);
                    }
                    Some('S') => scroll_up(&mut grid, top, bottom, first.max(1)),
                    Some('T') => {
                        for _ in 0..first.max(1) {
                            grid.remove(bottom);
                            grid.insert(top, blank());
                        }
                    }
                    _ => {}
                }
            }
            '\r' => column = 0,
            '\n' if row == bottom => scroll_up(&mut grid, top, bottom, 1),
            '\n' => row = (row + 1).min(rows - 1),
            character if !character.is_control() && column < columns => {
                grid[row][column] = character;
                column += 1;
            }
            _ => {}
        }
    }
    let lines = |lines: Vec<Vec<char>>| {
        lines
            .into_iter()
            .map(|line| line.into_iter().collect::<String>())
            .collect::<Vec<_>>()
    };
    (lines(scrollback), lines(grid))
}

// The latest Classic composer caret after `from`, located on the decoded screen by the
// composer border instead of a fixed row, since the content-sized viewport moves.
// Returns the 1-based caret column and the decoded screen.
#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
fn composer_caret(
    bytes: &[u8],
    from: usize,
    columns: usize,
    rows: usize,
) -> Option<(usize, Vec<String>)> {
    let marker = b"\x1b[?25h\x1b[";
    let start = from
        + bytes[from..]
            .windows(marker.len())
            .rposition(|part| part == marker)?
        + marker.len();
    let rest = &bytes[start..];
    let row_digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
    let rest = rest[row_digits..].strip_prefix(b";")?;
    let column_digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
    if rest.get(column_digits) != Some(&b'H') {
        return None;
    }
    let row = String::from_utf8_lossy(&bytes[start..start + row_digits])
        .parse::<usize>()
        .ok()?;
    let column = String::from_utf8_lossy(&rest[..column_digits])
        .parse::<usize>()
        .ok()?;
    let end = start + row_digits + 1 + column_digits + 1;
    let screen = screen_rows(&bytes[..end], columns, rows);
    let border = screen
        .iter()
        .rposition(|line| line.starts_with("─ Ask Arany"))?;
    // The caret sits on a draft row between the composer's top and bottom borders.
    (row > border + 1 && row <= border + 1 + 4).then_some((column, screen))
}

// Native history ends directly above the Classic composer: at most the two blank rows that
// end every native message lie between them, never rows of an oversized viewport.
#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
fn assert_history_above(screen: &[String], composer_top: usize, context: &str) {
    let above = &screen[..composer_top];
    let gap = above
        .iter()
        .rev()
        .take_while(|row| row.trim().is_empty())
        .count();
    assert!(
        gap <= 2 && gap < above.len(),
        "{context}: {gap} blank rows between history and the composer: {screen:?}"
    );
}

// A selector takes keys only once armed: its finished frame shows the focused `› N.` row.
#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
fn armed_selector(bytes: &[u8], from: usize, columns: usize, rows: usize) -> Option<Vec<String>> {
    let end = from
        + bytes[from..]
            .windows(6)
            .rposition(|part| part == b"\x1b[?25l")?
        + 6;
    let screen = screen_rows(&bytes[..end], columns, rows);
    screen
        .iter()
        .any(|row| {
            row.strip_prefix("› ")
                .and_then(|rest| rest.split_once(". "))
                .is_some_and(|(number, _)| {
                    !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
                })
        })
        .then_some(screen)
}

#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
fn assert_classic_owned_clear(bytes: &[u8], exit_from: usize) {
    let clear = exit_from
        + bytes[exit_from..]
            .windows(3)
            .rposition(|part| part == b"\x1b[J")
            .expect("Classic exit clears its owned area");
    let before = terminal_grid(&bytes[..exit_from], 80, 24);
    let after = terminal_grid(&bytes[..clear + 3], 80, 24);
    let owned_top = before
        .1
        .iter()
        .position(|row| row.starts_with("─ Ask Arany"))
        .expect("Classic composer visible before exit cleanup");
    let settled = before
        .0
        .iter()
        .chain(&before.1[..owned_top])
        .filter(|row| !row.trim().is_empty())
        .collect::<Vec<_>>();
    assert!(
        !settled.is_empty(),
        "settled history must be present before cleanup"
    );
    let remaining = after
        .0
        .iter()
        .chain(&after.1)
        .filter(|row| !row.trim().is_empty())
        .collect::<Vec<_>>();
    assert_eq!(
        remaining, settled,
        "exit must preserve all settled history and clear every transient row"
    );
}

#[test]
fn account_path_uses_the_private_binary_fixture() {
    let root = arany::test_support::isolate_account_root();
    if cfg!(not(debug_assertions)) && std::env::var_os("ARANY_TEST_ACCOUNT_ROOT").is_some() {
        assert!(matches!(
            StateRoot::account_path(),
            Err(arany::StoreError::InvalidStateDirectory)
        ));
        return;
    }
    assert_eq!(StateRoot::account_path().unwrap(), root);
    if std::env::var_os("ARANY_TEST_ACCOUNT_ROOT").is_none() {
        assert!(root.starts_with(std::fs::canonicalize(std::env::temp_dir()).unwrap()));
    }
}

#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
fn prepare_test_file_account_at(account_root: &std::path::Path) -> uuid::Uuid {
    let state = StateRoot::admit(account_root).expect("test account root");
    let account_id = uuid::Uuid::now_v7();
    let record = serde_json::json!({
        "schema": 1,
        "storage": "private_file",
        "account": {
            "schema": 2,
            "id": account_id,
            "provider": "openai",
            "model": "gpt-5.4",
            "effort": null,
            "api_key": "synthetic-existing-key",
            "anthropic_workspace_id": null
        }
    });
    state
        .record(arany::RecordKind::SavedAccount)
        .replace(&serde_json::to_vec(&record).expect("test record"))
        .expect("test file backend");
    account_id
}

fn tool_outcome(call: arany::ToolCall) -> arany::ProviderOutcome {
    arany::ProviderOutcome::Tools(vec![arany::ToolRequest {
        id: arany::ToolUseId::new("call_1").expect("scripted Tool id"),
        call,
    }])
}

#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
fn planned(request: &arany::ProviderRequest) -> bool {
    request.input.goal_plan_contract.is_some()
        || request
            .input
            .tools
            .as_ref()
            .is_some_and(|tools| tools.steps.iter().any(|step| step.plan.is_some()))
}

#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
fn safe_run_diagnostics(run: &arany::RunView) -> String {
    let agents = run
        .agents
        .iter()
        .map(|agent| {
            (
                agent.status,
                agent
                    .provider_calls
                    .iter()
                    .map(|call| call.disposition)
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<Vec<_>>();
    let tools = run
        .tools
        .iter()
        .map(|tool| {
            tool.observation.as_ref().map(|observation| {
                let result = serde_json::from_str::<serde_json::Value>(&observation.output).ok();
                (
                    observation.disposition,
                    observation.attestation.is_some(),
                    observation.cache_update,
                    result
                        .as_ref()
                        .and_then(|result| result["exit_code"].as_i64()),
                    result
                        .as_ref()
                        .and_then(|result| result["changeset"]["files"].as_array())
                        .map(Vec::len),
                    observation.output.len(),
                )
            })
        })
        .collect::<Vec<_>>();
    format!("agents={agents:?}; tools={tools:?}")
}

use arany::semantic;

pub use arany::test_support::process;

pub use arany::test_support::engine as engine_support;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use arany::test_support::git;

#[cfg(target_os = "linux")]
#[path = "session_run/isolation.rs"]
mod isolation;

#[cfg(target_os = "linux")]
#[path = "session_run/tools.rs"]
mod tools;

#[cfg(all(target_os = "macos", debug_assertions))]
#[path = "session_run/tools_macos.rs"]
mod tools;

#[cfg(target_os = "macos")]
#[path = "session_run/tools_macos_git.rs"]
mod tools_git;

#[cfg(target_os = "macos")]
#[path = "session_run/tools_macos_researcher.rs"]
mod tools_researcher;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "session_run/tools/replay.rs"]
mod tool_replay;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "session_run/supervisor.rs"]
mod supervisor;

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", any(debug_assertions, arany_ci))
))]
fn private_file(path: &std::path::Path, bytes: &[u8]) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
fn saved_ui_preferences(completion: &str, view: &str) -> Vec<u8> {
    format!(
        r#"{{"version":5,"preferences":{{"completion":"{completion}","view":"{view}","goal_budget":{{"max_provider_calls":null,"max_tokens":null,"max_active_ms":null,"max_runs":16}},"resources":{{"default_command_timeout_ms":120000,"max_command_timeout_ms":600000,"build_memory_bytes":4294967296,"build_processes":256,"build_project_bytes":1073741824,"build_cache_bytes":8589934592}},"mouse":"wheel_and_clicks","copy_on_select":true,"newline":"shift_enter"}}}}"#
    )
    .into_bytes()
}

#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
fn seed_classic(state: &std::path::Path) {
    arany::StateRoot::admit(state)
        .unwrap()
        .record(arany::RecordKind::UiPreferences)
        .replace(&saved_ui_preferences("tabs", "classic"))
        .unwrap();
}

#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
fn seed_transcript_session(state: &std::path::Path, workspace: &std::path::Path) -> SessionId {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let engine =
            engine_support::open(StateRoot::admit(state).unwrap(), Default::default()).unwrap();
        let mut provider = arany::test_support::scenario::ScriptedProvider::new(
            (0..17)
                .map(|_| arany::test_support::goals::finish_with("Synthetic answer"))
                .collect(),
        )
        .with_profile("openai", "gpt-5.4");
        provider.preview = Some(arany::PreviewText {
            commentary: vec!["DETAIL_CANARY \u{1b}]52;c;inert\u{7}".into()],
            ..Default::default()
        });
        let provider = std::sync::Arc::new(provider);
        let mut id = None;
        for _ in 0..17 {
            let mut request =
                arany::test_support::scenario::request(workspace, "Synthetic objective");
            request.include_paths.clear();
            request.session_id = id;
            request.title = None;
            let result = engine_support::run_with(
                &engine,
                &provider,
                request,
                arany::RunControls {
                    hints: Some(arany::RunHints::with_previews()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            assert_eq!(result.run.status, RunStatus::Finished);
            id = Some(result.session_id);
        }
        assert_eq!(provider.requests.lock().unwrap().len(), 17);
        engine.close().await.unwrap();
        id.unwrap()
    })
}

#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
fn seed_answer_session(
    state: &std::path::Path,
    workspace: &std::path::Path,
    answer: &str,
) -> SessionId {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let engine =
            engine_support::open(StateRoot::admit(state).unwrap(), Default::default()).unwrap();
        let provider = std::sync::Arc::new(
            arany::test_support::scenario::ScriptedProvider::new(vec![
                arany::test_support::goals::finish_with(answer),
            ])
            .with_profile("openai", "gpt-5.4"),
        );
        let mut request = arany::test_support::scenario::request(workspace, "Copy objective");
        request.include_paths.clear();
        request.title = None;
        let result = engine_support::run_with(&engine, &provider, request, Default::default())
            .await
            .unwrap();
        assert_eq!(result.run.status, RunStatus::Finished);
        assert_eq!(result.run.assistant_message.as_deref(), Some(answer));
        engine.close().await.unwrap();
        result.session_id
    })
}

#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
#[path = "session_run/roster.rs"]
mod roster;

#[cfg(unix)]
#[path = "session_run/custom.rs"]
mod custom;

#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
#[path = "session_run/exec_interrupt.rs"]
mod exec_interrupt;

#[cfg(unix)]
#[path = "session_run/live.rs"]
mod live;

#[cfg(target_os = "linux")]
#[path = "session_run/agent_inspector.rs"]
mod agent_inspector;

#[cfg(target_os = "linux")]
#[path = "session_run/model_catalog.rs"]
mod model_catalog;

#[cfg(target_os = "linux")]
#[path = "session_run/slash_completion.rs"]
mod slash_completion;

#[cfg(target_os = "linux")]
#[path = "session_run/rewind.rs"]
mod rewind;

#[cfg(target_os = "linux")]
#[path = "session_run/account_lock.rs"]
mod account_lock;
#[cfg(target_os = "linux")]
#[path = "session_run/setup.rs"]
mod setup;

#[cfg(unix)]
#[path = "session_run/loopback.rs"]
mod loopback;

#[cfg(target_os = "linux")]
#[path = "session_run/tls_fixture.rs"]
mod tls_fixture;

#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
#[path = "session_run/startup_trust.rs"]
mod startup_trust;

#[cfg(target_os = "linux")]
#[path = "session_run/active_terminal.rs"]
mod active_terminal;

#[cfg(all(target_os = "macos", debug_assertions))]
#[path = "session_run/active_terminal_macos.rs"]
mod active_terminal;

#[cfg(target_os = "linux")]
#[path = "session_run/chat_mouse.rs"]
mod chat_mouse;
#[cfg(target_os = "linux")]
#[path = "session_run/session_picker.rs"]
mod session_picker;

#[cfg(target_os = "linux")]
#[path = "session_run/auto_compaction.rs"]
mod auto_compaction;

#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
#[path = "session_run/telemetry.rs"]
mod telemetry;

#[cfg(target_os = "linux")]
#[path = "session_run/disk_fault.rs"]
mod disk_fault;

#[cfg(target_os = "linux")]
#[path = "session_run/performance.rs"]
mod performance;

#[cfg(unix)]
#[test]
fn terminal_readers_preserve_shared_output_and_setup_redraw() {
    use arany_cli::terminal::{AttachedTerminal, Composer, TerminalInput, TerminalMode};
    use rustix::fs::{OFlags, fcntl_getfl};
    use std::{io::Write, process::Stdio, time::Duration};

    const CASE: &str = "ARANY_TEST_TERMINAL_READER_CASE";
    const TEST: &str = "terminal_readers_preserve_shared_output_and_setup_redraw";
    if let Some(case) = std::env::var_os(CASE) {
        let linear = case == "linear" || case == "linear-panic" || case == "cursor-timeout";
        let unwind = case == "inline-panic" || case == "linear-panic" || case == "fullscreen-panic";
        let flags = fcntl_getfl(std::io::stdin()).expect("inherited input flags");
        assert!(!flags.contains(OFlags::NONBLOCK));
        assert_eq!(fcntl_getfl(std::io::stderr()).unwrap(), flags);
        let settings = process::terminal_settings(std::io::stdin());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mode = if case == "fullscreen" || case == "fullscreen-panic" {
                TerminalMode::Fullscreen
            } else {
                TerminalMode::Classic
            };
            let mut terminal = AttachedTerminal::acquire_with_mode(
                linear && case != "cursor-timeout",
                mode,
                Default::default(),
            )
            .unwrap();
            assert_eq!(terminal.is_linear(), linear);
            for cycle in 0..2 {
                assert_eq!(
                    fcntl_getfl(std::io::stderr()).unwrap(),
                    flags,
                    "input ownership must not make shared terminal output nonblocking"
                );
                assert_eq!(fcntl_getfl(std::io::stdin()).unwrap(), flags);
                terminal
                    .draw_setup(4, "Synthetic setup", "Returning to chat", 0, None)
                    .unwrap();
                let mut view = SessionView {
                    usage: Default::default(),
                    rewinds: Default::default(),
                    id: SessionId::new(),
                    title: "New Session".into(),
                    title_is_explicit: false,
                    inherited_title: None,
                    workspace_identity: None,
                    defaults: SessionDefaults::default(),
                    host_epoch_floor: 0,
                    created_sequence: 0,
                    last_sequence: 0,
                    lineage: None,
                    goals: Vec::new(),
                    runs: Vec::new(),
                    compactions: Vec::new(),
                };
                if case == "inline-long" && cycle == 0 {
                    let answer = (0..200)
                        .map(|line| format!("NATIVE_LINE_{line:03}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    let run_id = RunId::new();
                    view.runs.push(arany::RunView {
                        skill: None,
                        supervisor: Default::default(),
                        workers: Default::default(),
                        id: run_id,
                        objective: "Synthetic long response".into(),
                        images: Vec::new(),
                        config: None,
                        agents: Vec::new(),
                        assistant_message: None,
                        status: RunStatus::Active,
                        accepted_sequence: 1,
                        finished_sequence: None,
                        failure: None,
                        tools: Vec::new(),
                        tool_compaction_summary: None,
                        tool_compaction_covered_count: 0,
                        tool_results_cleared_count: 0,
                        definitions: Default::default(),
                        display: Vec::new(),
                        steering_notes: Vec::new(),
                        lifecycle_hooks: Vec::new(),
                        lifecycle_hook_sequence: None,
                        provider_sequences: Default::default(),
                        provider_fallbacks: Vec::new(),
                        provider_retries: Vec::new(),
                    });
                    terminal.record_user_run(&view.runs[0]).unwrap();
                    for lines in [50, 100, 200] {
                        terminal.set_preview(Some(arany::DisplayUpdate {
                            run_id,
                            agent_run_id: AgentRunId::new(),
                            call_index: 0,
                            text: std::sync::Arc::new(arany::PreviewText {
                                answer: answer.lines().take(lines).collect::<Vec<_>>().join("\n"),
                                ..Default::default()
                            }),
                        }));
                        terminal.draw(&view, &Composer::default(), None).unwrap();
                    }
                    println!("LONG_CONFIRMED");
                    view.runs[0].assistant_message = Some(answer);
                    view.runs[0].status = RunStatus::Finished;
                    view.last_sequence = 2;
                    terminal.finish_run_display();
                    for _ in 0..3 {
                        terminal.draw(&view, &Composer::default(), None).unwrap();
                    }
                    let mut draft = Composer::default();
                    for character in "retained 🚀 draft".chars() {
                        draft.apply(TerminalInput::Character(character));
                    }
                    assert!(
                        terminal
                            .handle_transcript_input(TerminalInput::HistoryFind, &view)
                            .unwrap()
                    );
                    let mut details = view.clone();
                    let mut run = view.runs[0].clone();
                    run.display.push(arany::DisplayChunk {
                        call_index: 0,
                        phase: arany::AgentPhase::RootPlan,
                        category: arany::DisplayCategory::Commentary,
                        item: 0,
                        offset: 0,
                        text: "NATIVE_DETAIL \u{1b}]52;c;inert\u{7}".into(),
                    });
                    details.runs = vec![run; 17];
                    assert!(
                        terminal
                            .handle_transcript_input(TerminalInput::Transcript, &details)
                            .unwrap()
                    );
                    terminal.draw(&view, &draft, None).unwrap();
                    terminal.toggle_transcript(&details).unwrap();
                    terminal
                        .set_mode(TerminalMode::Fullscreen, draft.input_capacity())
                        .unwrap();
                    terminal.draw(&view, &draft, None).unwrap();
                    terminal
                        .handle_history_input(TerminalInput::PageUp, &view, &draft, false)
                        .await;
                    terminal
                        .set_mode(TerminalMode::Classic, draft.input_capacity())
                        .unwrap();
                    terminal.draw(&view, &draft, None).unwrap();
                    assert_eq!(draft.text(), "retained 🚀 draft");
                    for _ in 0..2 {
                        terminal
                            .draw(&view, &draft, Some("RepeatedFeedback"))
                            .unwrap();
                    }
                    let mut next = view.runs[0].clone();
                    next.id = RunId::new();
                    next.objective = "Next task".into();
                    next.assistant_message = None;
                    next.status = RunStatus::Cancelled;
                    view.runs.push(next);
                    view.last_sequence += 1;
                    terminal
                        .draw(&view, &draft, Some("RepeatedFeedback"))
                        .unwrap();
                }
                terminal
                    .draw(&view, &Composer::default(), Some("Synthetic setup ready"))
                    .unwrap();
                println!("INPUT_READY:{cycle}");
                if unwind {
                    let observed =
                        tokio::time::timeout(Duration::from_secs(5), terminal.next_input())
                            .await
                            .expect("bounded unwind release")
                            .unwrap();
                    assert_eq!(observed, TerminalInput::Character('p'));
                    panic!("TEST_ONLY_TERMINAL_OWNER_PANIC");
                }
                for expected in [TerminalInput::Character('x'), TerminalInput::Submit] {
                    let observed =
                        tokio::time::timeout(Duration::from_secs(5), terminal.next_input())
                            .await
                            .expect("bounded terminal input")
                            .unwrap();
                    assert_eq!(observed, expected);
                }
                terminal.restore().unwrap();
                assert_eq!(process::terminal_settings(std::io::stdin()), settings);
                assert_eq!(fcntl_getfl(std::io::stdin()).unwrap(), flags);
                assert_eq!(fcntl_getfl(std::io::stderr()).unwrap(), flags);
                if cycle == 0 {
                    terminal.reacquire_after_output().unwrap();
                }
            }
        });
        return;
    }

    for case in [
        "inline",
        "inline-long",
        "inline-query-key",
        "cursor-timeout",
        "fullscreen",
        "fullscreen-panic",
        "linear",
        "inline-panic",
        "linear-panic",
    ] {
        let unwind = case.ends_with("-panic");
        let temp = tempfile::tempdir().unwrap();
        let geometry = nix::pty::Winsize {
            ws_row: 50,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let (master, slave) = process::cloexec_pty(Some(&geometry));
        let before = process::terminal_settings(&master);
        let probe = master.try_clone().unwrap();
        let input = master.try_clone().unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .env_clear()
            .env(CASE, case)
            .env("HOME", temp.path().join("fixture-home"))
            .env("XDG_STATE_HOME", temp.path().join("fixture-state"))
            .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
            .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account"))
            .env("TERM", "xterm")
            .current_dir(temp.path())
            .args(["--exact", TEST, "--nocapture", "--test-threads", "1"])
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // Close fixture PTYs before reaping a child on unwind.
        let (mut child, mut input, probe) = (
            loopback::ChildGuard::new(command.spawn().unwrap()),
            input,
            probe,
        );
        drop(command);
        let mut queries = 0;
        let mut submissions = 0;
        let mut injected = 0;
        let mut observed_tail = String::new();
        let mut observed_bytes = 0;
        let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            process::capture_terminal(
                child.child(),
                master,
                Duration::from_secs(20),
                256 * 1024,
                |bytes| {
                    if observed_bytes != bytes.len() {
                        observed_tail =
                            String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(4096)..])
                                .chars()
                                .flat_map(char::escape_default)
                                .collect();
                        observed_bytes = bytes.len();
                    }
                    let count = bytes.windows(4).filter(|part| *part == b"\x1b[6n").count();
                    if queries < count {
                        assert!(
                            case.starts_with("inline") || case == "cursor-timeout",
                            "only classic queries the admitted terminal"
                        );
                        if case == "cursor-timeout" {
                            queries = count;
                        } else {
                            if case == "inline-query-key" && injected == submissions {
                                input.write_all(b"x").unwrap();
                                injected += 1;
                            }
                            reply_positioned_cursor_queries(&mut input, bytes, &mut queries);
                        }
                    }
                    let marker = format!("INPUT_READY:{submissions}");
                    if submissions < if unwind { 1 } else { 2 }
                        && bytes
                            .windows(marker.len())
                            .enumerate()
                            .any(|(offset, part)| {
                                part == marker.as_bytes()
                                    && (bytes[offset + marker.len()..].starts_with(b"\n")
                                        || bytes[offset + marker.len()..].starts_with(b"\r\n"))
                            })
                    {
                        if unwind {
                            let acquired = process::terminal_settings(&probe);
                            if case.starts_with("linear") {
                                assert_eq!(acquired, before, "linear mode remains canonical");
                            } else {
                                assert_ne!(acquired, before, "raw mode acquired before unwind");
                            }
                        }
                        input
                            .write_all(if unwind {
                                b"p\r"
                            } else if case == "inline-query-key" {
                                b"\r"
                            } else {
                                b"x\r"
                            })
                            .unwrap();
                        submissions += 1;
                    }
                },
            )
        }));
        let output = observation.unwrap_or_else(|_| {
            panic!("{case} terminal owner observation failed after {submissions} submissions: {observed_tail}")
        });
        assert!(
            if unwind {
                output.status.code() == Some(101)
            } else {
                output.status.success()
            },
            "{case} terminal owner failed: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(submissions, if unwind { 1 } else { 2 }, "{case} input gate");
        assert_eq!(
            process::terminal_settings(&probe),
            before,
            "{case} restoration"
        );
        if case.starts_with("fullscreen") {
            // Cursor visibility is global, so the final show may precede the leave of the alternate screen.
            let text = String::from_utf8_lossy(&output.stderr);
            let last_show = text.rfind("\x1b[?25h").expect("cursor shown on exit");
            let last_hide = text.rfind("\x1b[?25l").unwrap_or(0);
            let leave = text.rfind("\x1b[?1049l").expect("alternate screen left");
            assert!(last_show > last_hide, "{case} leaves cursor hidden");
            assert!(leave > last_hide, "{case} hides cursor after leaving");
        }
        if case == "inline-long" {
            let text = String::from_utf8_lossy(&output.stderr);
            let before_fullscreen = text
                .split_once("\x1b[?1049h")
                .expect("explicit Fullscreen switch")
                .0;
            for word in ["terminal's", "find;", "Ctrl+T", "scrollback"] {
                assert!(
                    before_fullscreen.contains(word),
                    "terminal find notice is missing {word}"
                );
            }
            assert_eq!(before_fullscreen.matches("NATIVE_DETAIL").count(), 32);
            assert_eq!(before_fullscreen.matches("reached;").count(), 2);
            assert!(!before_fullscreen.contains("\x1b]52;"));
            let mut primary = String::new();
            let mut rest = text.as_ref();
            while let Some((before, alternate)) = rest.split_once("\x1b[?1049h") {
                primary.push_str(before);
                rest = alternate
                    .split_once("\x1b[?1049l")
                    .expect("alternate-screen restoration")
                    .1;
            }
            primary.push_str(rest);
            let confirmed = primary
                .split_once("LONG_CONFIRMED")
                .expect("confirmation boundary")
                .1;
            assert_eq!(
                confirmed.matches("RepeatedFeedback").count(),
                2,
                "feedback belongs to its committed boundary"
            );
            for line in 0..200 {
                let expected = format!("NATIVE_LINE_{line:03}");
                assert_eq!(
                    confirmed.matches(&expected).count(),
                    1,
                    "native final transcript {expected}"
                );
            }
        }
        assert!(output.stdout.is_empty());
        let completion: &[u8] = if unwind {
            b"0 passed; 1 failed; 0 ignored"
        } else {
            b"1 passed; 0 failed; 0 ignored"
        };
        assert!(
            output
                .stderr
                .windows(completion.len())
                .any(|part| part == completion),
            "exact child must complete one test"
        );
        if unwind {
            assert!(
                output
                    .stderr
                    .windows(b"TEST_ONLY_TERMINAL_OWNER_PANIC".len())
                    .any(|part| part == b"TEST_ONLY_TERMINAL_OWNER_PANIC"),
                "{case} deliberate unwind reached"
            );
        }
        assert!(
            !temp.path().join("account").exists(),
            "terminal evidence never opens an account"
        );
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn account_path_ignores_session_state_environment() {
    use nix::unistd::{User, geteuid};

    const CASE: &str = "ARANY_TEST_ACCOUNT_PATH_CASE";
    const TEST: &str = "account_path_ignores_session_state_environment";
    if let Some(case) = std::env::var_os(CASE) {
        match case.to_str() {
            Some("system") => {
                let home = User::from_uid(geteuid())
                    .expect("OS user lookup")
                    .expect("OS user")
                    .dir;
                #[cfg(target_os = "linux")]
                let expected = home.join(".local/state/arany");
                #[cfg(target_os = "macos")]
                let expected = home.join("Library/Application Support/dev.Arany.arany");
                assert_eq!(
                    StateRoot::account_path().expect("OS user account path"),
                    expected
                );
            }
            #[cfg(debug_assertions)]
            Some("fixture") => {
                let fixture =
                    std::path::PathBuf::from(std::env::var_os("ARANY_TEST_ACCOUNT_ROOT").unwrap());
                assert_eq!(
                    StateRoot::account_path().expect("absolute debug fixture"),
                    fixture
                );
                let parent = fixture.parent().expect("fixture-owned parent");
                let default_state = StateRoot::default_path().expect("fixture Session path");
                assert!(
                    default_state.starts_with(parent),
                    "environment-selected state must remain inside the fixture"
                );
                assert_ne!(
                    default_state, fixture,
                    "account and Session fixture roots remain separate"
                );
            }
            #[cfg(debug_assertions)]
            Some("reject-ambient-state") => {
                assert!(
                    StateRoot::account_path().is_ok(),
                    "stable fixture root is admitted"
                );
                assert!(
                    matches!(
                        StateRoot::default_path(),
                        Err(StoreError::InvalidStateDirectory)
                    ),
                    "ambient default state must reject before it can be opened"
                );
            }
            Some("reject") => assert!(matches!(
                StateRoot::account_path(),
                Err(StoreError::InvalidStateDirectory)
            )),
            _ => panic!("unknown account-path fixture"),
        }
        return;
    }

    let temp = tempfile::tempdir().expect("temporary environment root");
    #[cfg(debug_assertions)]
    let absolute_case = "fixture";
    #[cfg(not(debug_assertions))]
    let absolute_case = "reject";
    for (case, fixture) in [
        ("system", None),
        (absolute_case, Some(temp.path().join("account-root"))),
        (
            "reject",
            Some(std::path::PathBuf::from("relative-account-root")),
        ),
        #[cfg(debug_assertions)]
        (
            "reject-ambient-state",
            Some(temp.path().join("account-root")),
        ),
    ] {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args(["--exact", TEST])
            .env_clear()
            .env(CASE, case)
            .current_dir(temp.path());
        if case != "reject-ambient-state" {
            command
                .env("HOME", temp.path().join("different-home"))
                .env("XDG_STATE_HOME", temp.path().join("different-state"))
                .env("XDG_DATA_HOME", temp.path().join("different-data"));
        }
        if let Some(fixture) = fixture {
            command.env("ARANY_TEST_ACCOUNT_ROOT", fixture);
        }
        let output = command
            .bounded_output_for(std::time::Duration::from_secs(10), 64 * 1024)
            .expect("bounded isolated account-path child");
        assert!(output.status.success(), "account-path case {case} failed");
        assert_eq!(output.stderr, b"");
        process::assert_single_test_completed(&output.stdout, &format!("account-path case {case}"));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn native_keychain_fixture_cannot_fall_back_to_default_store() {
    let temp = tempfile::tempdir().unwrap();
    let slot = format!("native-test-{}", uuid::Uuid::now_v7());
    for root in [
        std::path::PathBuf::from("relative"),
        temp.path().join("missing"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_arany"))
            .args(["--internal-credential-helper", "read", &slot])
            .env_clear()
            .env("ARANY_TEST_KEYCHAIN_ROOT", root)
            .current_dir(temp.path())
            .bounded_output_for(std::time::Duration::from_secs(10), 4096)
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "fixture rejection must precede native store access"
        );
        assert!(output.stdout.is_empty() && output.stderr.is_empty());
    }
}

#[test]
fn unsupported_journal_versions_reject_without_changing_existing_data() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let current = tempfile::tempdir().unwrap();
    let root = StateRoot::admit(&current.path().join("state")).unwrap();
    let database = root.path().join("events.sqlite3");
    runtime
        .block_on(Store::open(root).unwrap().close())
        .unwrap();
    let current_version: i64 = rusqlite::Connection::open(database)
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    let migration_floor = 4;
    for version in (0..migration_floor).chain(std::iter::once(current_version + 1)) {
        let temp = tempfile::tempdir().expect("private test root");
        let state_path = temp.path().join("state");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        StateRoot::admit(&state_path).expect("state root");
        let database = state_path.join("events.sqlite3");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE canary (value TEXT); INSERT INTO canary VALUES ('preserved');",
            )
            .unwrap();
        connection
            .pragma_update(None, "user_version", version)
            .unwrap();
        drop(connection);
        let original = std::fs::read(&database).unwrap();
        for read_only in [true, false] {
            let state = StateRoot::open_existing(&state_path).unwrap();
            let result = if read_only {
                Store::open_read_only(state)
            } else {
                Store::open(state)
            };
            assert!(
                matches!(result, Err(StoreError::InvalidHistory)),
                "version {version}, read_only {read_only}"
            );
            assert_eq!(std::fs::read(&database).unwrap(), original);
        }
        let reset = StateRoot::open_existing(&state_path)
            .unwrap()
            .prepare_history_reset(&workspace)
            .unwrap();
        for read_only in [true, false] {
            let state = StateRoot::open_existing(&state_path).unwrap();
            let result = if read_only {
                Store::open_read_only(state)
            } else {
                Store::open(state)
            };
            assert!(matches!(result, Err(StoreError::HistoryBusy)));
        }
        drop(reset);
        assert_eq!(std::fs::read(&database).unwrap(), original);

        let reset = StateRoot::open_existing(&state_path)
            .unwrap()
            .prepare_history_reset(&workspace)
            .unwrap();
        let mut changed = original;
        changed.push(0);
        std::fs::write(&database, &changed).unwrap();
        assert!(matches!(reset.apply(), Err(StoreError::HistoryChanged)));
        assert_eq!(std::fs::read(&database).unwrap(), changed);

        StateRoot::open_existing(&state_path)
            .unwrap()
            .prepare_history_reset(&workspace)
            .unwrap()
            .apply()
            .unwrap();
        let store = Store::open(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        assert!(matches!(
            StateRoot::open_existing(&state_path)
                .unwrap()
                .prepare_history_reset(&workspace),
            Err(StoreError::HistoryBusy)
        ));
        runtime.block_on(store.close()).unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        assert!(matches!(
            StateRoot::open_existing(&state_path)
                .unwrap()
                .prepare_history_reset(&workspace),
            Err(StoreError::HistoryBusy)
        ));
        assert!(
            runtime
                .block_on(store.load_session(SessionId::new()))
                .unwrap()
                .is_empty()
        );
        runtime.block_on(store.close()).unwrap();
    }
}

#[test]
fn attached_command_rejects_pipes_before_creating_state() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state_dir = temp.path().join("state");
    let output = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(temp.path())
        .arg("--state-dir")
        .arg(&state_dir)
        .arg("--workspace")
        .arg(&workspace)
        .args(["--provider", "openai", "--model", "gpt-5.4", "hello"])
        .bounded_output()
        .expect("attached process");
    assert!(!output.status.success());
    assert_eq!(output.stdout, b"");
    assert_eq!(
        output.stderr,
        b"error: attached mode requires terminal stdin and stderr; use arany exec for pipes\n"
    );
    assert!(!state_dir.exists());

    let picker = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(temp.path())
        .arg("--state-dir")
        .arg(&state_dir)
        .arg("--workspace")
        .arg(&workspace)
        .arg("--resume")
        .bounded_output()
        .expect("picker process");
    assert!(!picker.status.success());
    assert_eq!(picker.stdout, b"");
    assert_eq!(picker.stderr, output.stderr);
    assert!(!state_dir.exists());

    for flag in ["--screen-reader", "--no-color"] {
        let flagged = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .current_dir(temp.path())
            .arg("--state-dir")
            .arg(&state_dir)
            .arg("--workspace")
            .arg(&workspace)
            .arg(flag)
            .bounded_output()
            .expect("flagged attached process");
        assert!(!flagged.status.success(), "{flag}");
        assert_eq!(flagged.stdout, b"", "{flag}");
        assert_eq!(flagged.stderr, output.stderr, "{flag}");
        assert!(!state_dir.exists(), "{flag}");
    }
}

#[test]
fn model_catalog_command_reads_only_selected_configuration() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state_dir = temp.path().join("state");
    let missing_key = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(&workspace)
        .args(["provider", "models", "openai"])
        .bounded_output()
        .expect("native catalog process");
    assert!(!missing_key.status.success());
    assert_eq!(missing_key.stdout, b"");
    assert_eq!(missing_key.stderr, b"error: Provider unavailable\n");
    assert!(!state_dir.exists());

    #[cfg(debug_assertions)]
    {
        let account_root = temp.path().join("missing-chatgpt-account");
        let fixture_home = temp.path().join("fixture-home");
        let fixture_state = temp.path().join("fixture-state");
        let chatgpt_missing = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .env("HOME", &fixture_home)
            .env("XDG_STATE_HOME", &fixture_state)
            .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
            .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
            .env("OPENAI_API_KEY", "synthetic-unselected-api-key")
            .current_dir(&workspace)
            .args(["provider", "models", "chatgpt"])
            .bounded_output()
            .expect("missing ChatGPT account process");
        assert!(!chatgpt_missing.status.success());
        assert_eq!(chatgpt_missing.stdout, b"");
        assert_eq!(
            chatgpt_missing.stderr,
            b"error: no selected ChatGPT account; run arany --setup\n"
        );
        assert!(!account_root.exists());
        assert!(!state_dir.exists());

        let missing_effort = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .env("HOME", &fixture_home)
            .env("XDG_STATE_HOME", &fixture_state)
            .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
            .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
            .current_dir(&workspace)
            .args([
                "exec",
                "--state-dir",
                state_dir.to_str().expect("test path"),
                "--provider",
                "chatgpt",
                "--model",
                "model-one",
                "objective",
            ])
            .bounded_output()
            .expect("ChatGPT Run requires effort before account access");
        assert!(!missing_effort.status.success());
        assert_eq!(missing_effort.stdout, b"");
        assert_eq!(
            missing_effort.stderr,
            b"error: ChatGPT requires --effort LEVEL\n"
        );
        assert!(!account_root.exists());
        assert!(!state_dir.exists());

        let unselected_run = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .env("HOME", &fixture_home)
            .env("XDG_STATE_HOME", &fixture_state)
            .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
            .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
            .env("OPENAI_API_KEY", "synthetic-unselected-api-key")
            .current_dir(&workspace)
            .args([
                "exec",
                "--state-dir",
                state_dir.to_str().expect("test path"),
                "--provider",
                "chatgpt",
                "--model",
                "model-one",
                "--effort",
                "high",
                "objective",
            ])
            .bounded_output()
            .expect("ChatGPT Run cannot fall back to API key");
        assert!(!unselected_run.status.success());
        assert_eq!(unselected_run.stdout, b"");
        assert_eq!(
            unselected_run.stderr,
            b"error: no selected ChatGPT account; run arany --setup\n"
        );
        assert!(!account_root.exists());
        assert!(!state_dir.exists());

        let unchecked = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .env("HOME", &fixture_home)
            .env("XDG_STATE_HOME", &fixture_state)
            .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
            .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
            .env("OPENAI_API_KEY", "synthetic-unselected-api-key")
            .current_dir(&workspace)
            .args([
                "provider",
                "check",
                "chatgpt",
                "model-one",
                "--effort",
                "high",
            ])
            .bounded_output()
            .expect("ChatGPT check requires explicit cost acceptance");
        assert!(!unchecked.status.success());
        assert_eq!(unchecked.stdout, b"");
        assert!(String::from_utf8_lossy(&unchecked.stderr).contains("no remote output-token cap"));
        assert!(!account_root.exists());

        let check_missing = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .env("HOME", &fixture_home)
            .env("XDG_STATE_HOME", &fixture_state)
            .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
            .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
            .env("OPENAI_API_KEY", "synthetic-unselected-api-key")
            .current_dir(&workspace)
            .args([
                "provider",
                "check",
                "chatgpt",
                "model-one",
                "--effort",
                "high",
                "--accept-cost",
            ])
            .bounded_output()
            .expect("ChatGPT check with missing account");
        assert!(!check_missing.status.success());
        assert_eq!(check_missing.stdout, b"");
        assert_eq!(
            check_missing.stderr,
            b"error: no selected ChatGPT account; run arany --setup\n"
        );
        assert!(!account_root.exists());
        assert!(!state_dir.exists());

        for flag in ["--saved-account", "--state-dir"] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
            command
                .env_clear()
                .env("HOME", &fixture_home)
                .env("XDG_STATE_HOME", &fixture_state)
                .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
                .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
                .current_dir(&workspace)
                .args(["provider", "models", flag]);
            if flag == "--state-dir" {
                command.arg(&state_dir);
            }
            let result = command
                .arg("chatgpt")
                .bounded_output()
                .expect("invalid catalog flag");
            assert!(!result.status.success(), "{flag}");
            assert_eq!(result.stdout, b"", "{flag}");
            assert!(
                String::from_utf8_lossy(&result.stderr)
                    .contains("omit --saved-account and --state-dir"),
                "{flag}"
            );
            assert!(!account_root.exists(), "{flag}");
        }

        for flag in ["--saved-account", "--state-dir"] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
            command
                .env_clear()
                .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
                .env("HOME", &fixture_home)
                .env("XDG_STATE_HOME", &fixture_state)
                .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
                .current_dir(&workspace)
                .args(["provider", "check", flag]);
            if flag == "--state-dir" {
                command.arg(&state_dir);
            }
            let result = command
                .args(["chatgpt", "model-one", "--effort", "high", "--accept-cost"])
                .bounded_output()
                .expect("invalid ChatGPT check flag");
            assert!(!result.status.success(), "{flag}");
            assert_eq!(result.stdout, b"", "{flag}");
            assert!(
                String::from_utf8_lossy(&result.stderr)
                    .contains("omit --saved-account and --state-dir"),
                "{flag}"
            );
            assert!(!account_root.exists(), "{flag}");
            assert!(!state_dir.exists(), "{flag}");
        }
    }

    let custom_saved = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(&workspace)
        .args(["provider", "models", "--saved-account", "custom:local"])
        .bounded_output()
        .expect("invalid saved custom catalog process");
    assert!(!custom_saved.status.success());
    assert_eq!(custom_saved.stdout, b"");
    assert_eq!(
        custom_saved.stderr,
        b"error: --saved-account requires a native Provider\n"
    );
    assert!(!state_dir.exists());

    #[cfg(all(target_os = "linux", debug_assertions))]
    {
        let marker = temp.path().join("credential-command-marker");
        let address = format!("unixexec:path=/usr/bin/touch,argv1={}", marker.display());
        let unsafe_saved = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
            .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
            .env("DBUS_SESSION_BUS_ADDRESS", address)
            .current_dir(&workspace)
            .args(["provider", "models", "--saved-account", "openai"])
            .bounded_output()
            .expect("saved native catalog with unsafe transport");
        assert!(!unsafe_saved.status.success());
        assert_eq!(unsafe_saved.stdout, b"");
        assert_eq!(
            unsafe_saved.stderr,
            b"error: unsafe or unsupported D-Bus session address for the OS credential store\n"
        );
        assert!(!marker.exists());
    }

    let root = StateRoot::admit(&state_dir).expect("private state root");
    let profile_file = root.path().join("provider-profiles.json");
    std::fs::write(&profile_file, r#"{"version":1,"profiles":[{"name":"local","protocol":"openai-responses","endpoint":"http://127.0.0.1:9321/v1/responses","model":"model-1","credential_env":"ARANY_PROVIDER_LOCAL_KEY","outcome_encoding":"json_schema","privacy":"user_authorized","max_output_tokens":4096,"capability_evidence_version":2,"efforts":[]}]}"#).expect("profile file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&profile_file, std::fs::Permissions::from_mode(0o600))
            .expect("private profile file");
    }
    let custom = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(&workspace)
        .args(["provider", "models", "--state-dir"])
        .arg(&state_dir)
        .arg("custom:local")
        .bounded_output()
        .expect("custom catalog process");
    assert!(
        custom.status.success(),
        "{}",
        String::from_utf8_lossy(&custom.stderr)
    );
    assert_eq!(
        custom.stdout,
        b"Provider: custom:local\nModels: 1\nmodel-1 \xC2\xB7 exact profile; Run requires current conformance; effort provider default\n"
    );
    assert_eq!(custom.stderr, b"");
}

#[test]
fn native_model_check_requires_explicit_cost_consent_before_state_or_credentials() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state_dir = temp.path().join("state");
    let without_consent = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(&workspace)
        .args(["provider", "check", "--state-dir"])
        .arg(&state_dir)
        .args(["openai", "example-model", "--effort", "high"])
        .bounded_output()
        .expect("native check without consent");
    assert!(!without_consent.status.success());
    assert_eq!(without_consent.stdout, b"");
    assert_eq!(
        without_consent.stderr,
        b"error: native checks may bill up to 3 inference calls and 3,072 generated tokens, plus bounded catalog/input usage; rerun with --accept-cost\n"
    );
    assert!(!state_dir.exists());

    let saved_without_consent = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .env("HOME", temp.path().join("fixture-home"))
        .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
        .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
        .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
        .current_dir(&workspace)
        .args(["provider", "check", "--state-dir"])
        .arg(&state_dir)
        .args([
            "openai",
            "example-model",
            "--effort",
            "high",
            "--saved-account",
        ])
        .bounded_output()
        .expect("saved native check without consent");
    assert!(!saved_without_consent.status.success());
    assert_eq!(saved_without_consent.stdout, b"");
    assert_eq!(saved_without_consent.stderr, without_consent.stderr);
    assert!(!state_dir.exists());

    let invalid_model = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(&workspace)
        .args(["provider", "check", "--state-dir"])
        .arg(&state_dir)
        .args(["openai", "bad\nmodel", "--effort", "high", "--accept-cost"])
        .bounded_output()
        .expect("native check with invalid model");
    assert!(!invalid_model.status.success());
    assert_eq!(invalid_model.stdout, b"");
    assert_eq!(
        invalid_model.stderr,
        b"error: native Provider, model, or effort is invalid\n"
    );
    assert!(!state_dir.exists());

    let without_key = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(&workspace)
        .args(["provider", "check", "--state-dir"])
        .arg(&state_dir)
        .args([
            "openai",
            "example-model",
            "--effort",
            "high",
            "--accept-cost",
        ])
        .bounded_output()
        .expect("native check without key");
    assert!(!without_key.status.success());
    assert_eq!(without_key.stdout, b"");
    assert_eq!(
        without_key.stderr,
        b"error: selected native API key is unavailable or invalid\n"
    );
    assert!(state_dir.exists());

    #[cfg(all(target_os = "linux", debug_assertions))]
    {
        let marker = temp.path().join("credential-command-marker");
        let address = format!("unixexec:path=/usr/bin/touch,argv1={}", marker.display());
        let unsafe_saved_account = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
            .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account-root"))
            .env("DBUS_SESSION_BUS_ADDRESS", address)
            .current_dir(&workspace)
            .args(["provider", "check", "--state-dir"])
            .arg(&state_dir)
            .args([
                "openai",
                "example-model",
                "--effort",
                "high",
                "--saved-account",
                "--accept-cost",
            ])
            .bounded_output()
            .expect("saved native check with unsafe transport");
        assert!(!unsafe_saved_account.status.success());
        assert_eq!(unsafe_saved_account.stdout, b"");
        assert_eq!(
            unsafe_saved_account.stderr,
            b"error: unsafe or unsupported D-Bus session address for the OS credential store\n"
        );
        assert!(!marker.exists());
    }
}

#[test]
fn exact_resume_and_rename_survive_file_backed_reopen() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("private temporary directory");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let state_path = temp.path().join("state");
        let id = create_session(
            StateRoot::admit(&state_path).expect("state"),
            workspace.clone(),
            None,
        )
        .await
        .expect("empty Session");
        let other_workspace = temp.path().join("other-workspace");
        std::fs::create_dir(&other_workspace).expect("other Workspace");
        assert!(matches!(
            resume_session(
                StateRoot::open_existing(&state_path).expect("existing state"),
                other_workspace.clone(),
                id,
            )
            .await,
            Err(EngineError::WorkspaceMismatch)
        ));
        assert!(matches!(
            rename_session(
                StateRoot::admit(&state_path).expect("state"),
                other_workspace,
                id,
                "Wrong Workspace".into(),
            )
            .await,
            Err(EngineError::WorkspaceMismatch)
        ));
        assert!(matches!(
            rename_session(
                StateRoot::admit(&state_path).expect("state"),
                workspace.clone(),
                id,
                String::new(),
            )
            .await,
            Err(EngineError::InvalidRequest)
        ));
        rename_session(
            StateRoot::admit(&state_path).expect("state"),
            workspace.clone(),
            id,
            "Research thread".into(),
        )
        .await
        .expect("renamed Session");
        let view = resume_session(
            StateRoot::open_existing(&state_path).expect("existing state"),
            workspace,
            id,
        )
        .await
        .expect("exact Session resume");
        assert_eq!(view.id, id);
        assert_eq!(view.title, "Research thread");
        assert!(view.runs.is_empty());
        assert_eq!(view.last_sequence, 2);
        assert!(view.workspace_identity.is_some());
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
            .expect("read-only reopen");
        let events = store.load_session(id).await.expect("committed history");
        assert_eq!(events.len(), 2);
        assert!(matches!(events[1].event, Event::SessionRenamed { .. }));
        store.close().await.expect("store shutdown");
        let WorkspaceIdentity { device, inode } = view.workspace_identity.expect("bound Workspace");
        let shown = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .current_dir(temp.path())
            .args(["show", "--state-dir"])
            .arg(&state_path)
            .args(["--output", "jsonl", &id.to_string()])
            .bounded_output()
            .expect("show process");
        assert!(shown.status.success());
        assert_eq!(shown.stderr, b"");
        let expected = format!(
            "{{\"sequence\":1,\"session_id\":\"{id}\",\"run_id\":null,\"agent_run_id\":null,\"kind\":\"SessionStarted\",\"event_version\":{},\"payload\":{{\"title\":\"New Session\",\"workspace_device\":{device},\"workspace_inode\":{inode}}},\"created_at_ms\":{}}}\n\
             {{\"sequence\":2,\"session_id\":\"{id}\",\"run_id\":null,\"agent_run_id\":null,\"kind\":\"SessionRenamed\",\"event_version\":{},\"payload\":{{\"title\":\"Research thread\"}},\"created_at_ms\":{}}}\n",
            events[0].event.version(), events[0].created_at_ms, events[1].event.version(), events[1].created_at_ms
        );
        assert_eq!(shown.stdout, expected.as_bytes());
        let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
            .expect("version inspection");
        let version: i64 = connection
            .query_row("SELECT event_version FROM events WHERE sequence=1", [], |row| {
                row.get(0)
            })
            .expect("start Event version");
        assert_eq!(version, events[0].event.version());
        connection
            .execute("UPDATE events SET event_version=1 WHERE sequence=1", [])
            .expect("version fault injection");
        drop(connection);
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
            .expect("read-only reopen after corruption");
        assert!(matches!(
            store.load_view(id).await,
            Err(StoreError::InvalidHistory)
        ));
        store.close().await.expect("store shutdown after corruption");
    });
}

#[test]
fn continue_selects_latest_committed_activity_in_the_admitted_workspace() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("private temporary directory");
        let workspace = temp.path().join("workspace");
        let other_workspace = temp.path().join("other-workspace");
        let unused_workspace = temp.path().join("unused-workspace");
        for path in [&workspace, &other_workspace, &unused_workspace] {
            std::fs::create_dir(path).expect("Workspace");
        }
        let state_path = temp.path().join("state");
        let first = create_session(
            StateRoot::admit(&state_path).expect("state"),
            workspace.clone(),
            Some("First".into()),
        )
        .await
        .expect("first Session");
        let other = create_session(
            StateRoot::admit(&state_path).expect("state"),
            other_workspace.clone(),
            Some("Other Workspace".into()),
        )
        .await
        .expect("other Workspace Session");
        let second = create_session(
            StateRoot::admit(&state_path).expect("state"),
            workspace.clone(),
            Some("Second".into()),
        )
        .await
        .expect("second Session");
        let listed = list_sessions(
            StateRoot::open_existing(&state_path).expect("existing state"),
            workspace.clone(),
        )
        .await
        .expect("Workspace Session list");
        assert_eq!(
            listed.iter().map(|item| item.id).collect::<Vec<_>>(),
            [second, first]
        );
        assert_eq!(listed[0].title, "Second");
        assert!(listed[0].last_sequence > listed[1].last_sequence);
        assert!(
            list_sessions(
                StateRoot::open_existing(&state_path).expect("existing state"),
                unused_workspace.clone(),
            )
            .await
            .expect("empty Workspace list")
            .is_empty()
        );

        let selected = continue_session(
            StateRoot::open_existing(&state_path).expect("existing state"),
            workspace.clone(),
        )
        .await
        .expect("latest Workspace Session");
        assert_eq!(selected.id, second);
        let selected = continue_session(
            StateRoot::open_existing(&state_path).expect("existing state"),
            other_workspace.clone(),
        )
        .await
        .expect("other Workspace Session");
        assert_eq!(selected.id, other);
        assert!(matches!(
            continue_session(
                StateRoot::open_existing(&state_path).expect("existing state"),
                unused_workspace,
            )
            .await,
            Err(EngineError::NoSessionForWorkspace)
        ));

        rename_session(
            StateRoot::admit(&state_path).expect("state"),
            workspace.clone(),
            first,
            "First, recently used".into(),
        )
        .await
        .expect("newest committed activity");
        let selected = continue_session(
            StateRoot::open_existing(&state_path).expect("existing state"),
            workspace.clone(),
        )
        .await
        .expect("latest by activity");
        assert_eq!(selected.id, first);
        assert_eq!(selected.title, "First, recently used");
        let listed = list_sessions(
            StateRoot::open_existing(&state_path).expect("existing state"),
            workspace.clone(),
        )
        .await
        .expect("renamed Session list");
        assert_eq!(
            listed.iter().map(|item| item.id).collect::<Vec<_>>(),
            [first, second]
        );
        assert_eq!(listed[0].title, "First, recently used");

        let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
            .expect("test fault injection");
        connection
            .execute(
                "UPDATE events SET event_version=9 WHERE session_id=?1",
                [other.to_string()],
            )
            .expect("malformed unrelated Session");
        drop(connection);
        assert!(matches!(
            continue_session(
                StateRoot::open_existing(&state_path).expect("existing state"),
                workspace.clone(),
            )
            .await,
            Err(EngineError::Store(StoreError::InvalidHistory))
        ));
        assert!(matches!(
            list_sessions(
                StateRoot::open_existing(&state_path).expect("existing state"),
                workspace,
            )
            .await,
            Err(EngineError::Store(StoreError::InvalidHistory))
        ));
    });
}

#[test]
fn an_empty_session_and_defaults_are_durable_without_workspace_overlap() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("private temporary directory");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let state_path = temp.path().join("state");
        let id = create_session(
            StateRoot::admit(&state_path).expect("state"),
            workspace.clone(),
            None,
        )
        .await
        .expect("empty Session");
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
            .expect("read-only reopen");
        let events = store.load_session(id).await.expect("committed history");
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].event, Event::SessionStarted { .. }));
        let view = store
            .load_view(id)
            .await
            .expect("resolved view")
            .expect("Session");
        assert_eq!(view.title, "New Session");
        assert!(view.runs.is_empty());
        assert!(view.workspace_identity.is_some());
        store.close().await.expect("closed read-only store");

        let defaults = SessionDefaults {
            provider: Some("openai".into()),
            model: Some("gpt-5.4".into()),
            effort: Some(arany::Effort::Medium),
            account_id: None,
            policy: CollaborationPolicy::Single,
        };
        set_session_defaults(
            StateRoot::admit(&state_path).expect("state"),
            workspace.clone(),
            id,
            defaults.clone(),
        )
        .await
        .expect("committed Session defaults");
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
            .expect("read-only reopen");
        let events = store.load_session(id).await.expect("committed history");
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[1].event,
            Event::SessionDefaultChanged { .. }
        ));
        let view = store
            .load_view(id)
            .await
            .expect("resolved view")
            .expect("Session");
        assert_eq!(view.defaults, defaults);
        let WorkspaceIdentity { device: workspace_device, inode: workspace_inode } =
            view.workspace_identity.expect("pinned Workspace identity");
        store.close().await.expect("closed read-only store");
        let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
        connection.execute("UPDATE events SET created_at_ms=CASE WHEN sequence=1 THEN 1791198000000 ELSE 1791288000000 END", []).unwrap();
        drop(connection);
        let listed = list_sessions(StateRoot::open_existing(&state_path).unwrap(), workspace.clone()).await.unwrap();
        assert_eq!(listed[0].title, "Empty conversation");
        assert_eq!(listed[0].created_at, "2026-10-05 11:00:00");
        assert_eq!(listed[0].last_activity_at, "2026-10-06 12:00:00");
        assert_eq!(listed[0].defaults, defaults);

        let invalid = set_session_defaults(
            StateRoot::admit(&state_path).expect("state"),
            workspace.clone(),
            id,
            SessionDefaults {
                provider: None,
                model: Some("gpt-5.4".into()),
                effort: None,
                account_id: None,
                policy: CollaborationPolicy::Single,
            },
        )
        .await;
        assert!(matches!(invalid, Err(EngineError::InvalidRequest)));
        let store =
            Store::open(StateRoot::admit(&state_path).expect("state")).expect("writable reopen");
        let pending_run_id = RunId::new();
        let pending_goal_id = arany::GoalId::new();
        store
            .append(
                id,
                Event::GoalStarted {
                    include_paths: Some(Vec::new()),
                    goal_id: pending_goal_id,
                    objective: "pending objective".into(),
                    workspace_identity: WorkspaceIdentity { device: workspace_device, inode: workspace_inode },
                    budget: arany::GoalBudget::default(),
                    phase: arany::GoalPhase::Research,
                    autonomous: false,
                    resource_settings: arany::ToolResourceSettings::default(),
                    continues: None,
                },
            )
            .await
            .expect("pending Goal");
        store
            .append(
                id,
                Event::GoalRunLinked {
                    goal_id: pending_goal_id,
                    run_id: pending_run_id,
                    phase: arany::GoalPhase::Research,
                },
            )
            .await
            .expect("pending Goal Run link");
        store
            .append(
                id,
                Event::MessageAccepted {
                    skill: None,
                    run_id: pending_run_id,
                    text: "pending objective".into(),
                    images: Vec::new(),
                },
            )
            .await
            .expect("pending objective");
        let mut config = RunConfig {
            supervisor: None,
            worker_policy: None,
            agent_definitions: None,
            roster: None,
            providers: None,
            provider: "scripted".into(),
            model: "test-model".into(),
            effort: None,
            custom_profile_provenance: None,
            saved_api_account_id: None,
            chatgpt_provenance: None,
            output_token_bound: arany::OutputTokenBound::ProviderEnforced,
            context_window_tokens: None,
            policy: CollaborationPolicy::Single,
            output_token_cap: 4096,
            provider_concurrency: 1,
            workspace_device,
            workspace_inode,
            user_instruction_digest: None,
            instruction_sources: Vec::new(),
            launch_directory: String::new(),
            include_digests: Vec::new(),
            history_run_ids: Vec::new(),
            excluded_history_runs: 0,
            context_usage: Some(ContextUsage {
                used_bytes: 101,
                budget_bytes: 100,
                compactable_bytes: 10,
                tool_history_bytes: 0,
            }),
            compaction_event_sequence: None,
            compaction_content_digest: None,
            tool_policy: None,
        };
        let encoded_config = serde_json::to_value(&config).expect("current Run config JSON");
        assert!(encoded_config["effort"].is_null());
        assert_eq!(
            serde_json::from_value::<RunConfig>(encoded_config)
                .expect("current Run config")
                .effort,
            None
        );
        assert!(matches!(
            store
                .append(
                    id,
                    Event::RunStarted {
                        run_id: pending_run_id,
                        config: config.clone(),
                        active_deadline_ms: None,
                    },
                )
                .await,
            Err(StoreError::InvalidHistory)
        ));
        config.context_usage = None;
        config.workspace_device = workspace_device.wrapping_add(1);
        assert!(matches!(
            store
                .append(
                    id,
                    Event::RunStarted {
                        run_id: pending_run_id,
                        config,
                        active_deadline_ms: None,
                    },
                )
                .await,
            Err(StoreError::InvalidHistory)
        ));
        store.close().await.expect("closed writable store");
        let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
            .expect("test fault injection");
        connection
            .execute(
                "UPDATE events SET payload=?1 WHERE sequence=2",
                [r#"{"provider":null,"model":"gpt-5.4","policy":{"mode":"single"}}"#],
            )
            .expect("invalid persisted defaults");
        drop(connection);
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
            .expect("read-only reopen");
        assert!(matches!(
            store.load_view(id).await,
            Err(StoreError::InvalidHistory)
        ));
        store.close().await.expect("closed read-only store");

        let overlap_path = workspace.join("state");
        let overlap = create_session(
            StateRoot::admit(&overlap_path).expect("overlap state"),
            workspace,
            None,
        )
        .await;
        assert!(matches!(overlap, Err(EngineError::StateOverlap)));
        let connection = rusqlite::Connection::open(overlap_path.join("events.sqlite3"))
            .expect("overlap database");
        let tables: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name='events'",
                [],
                |row| row.get(0),
            )
            .expect("journal table count");
        assert_eq!(tables, 0);
    });
}

#[test]
fn session_journal_survives_reopen_and_rejects_invalid_history() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("private temporary directory");
        let state_path = temp.path().join("state");
        let hostile_title = "Continued\u{1b}[31m\u{202e} Session";
        let id = SessionId::new();
        let store = Store::open(StateRoot::admit(&state_path).expect("state admission"))
            .expect("new store");
        let start = store
            .append(
                id,
                Event::SessionStarted {
                    title: "First Session".into(),
                    workspace_identity: WorkspaceIdentity { device: 1, inode: 1 },
                },
            )
            .await
            .expect("committed start");
        assert_eq!(start.sequence, 1);
        let rename = store
            .append(
                id,
                Event::SessionRenamed {
                    title: hostile_title.into(),
                },
            )
            .await
            .expect("committed rename");
        assert_eq!(rename.sequence, 2);
        assert!(matches!(
            store
                .append(
                    id,
                    Event::SessionStarted {
                        title: "Invalid duplicate".into(),
                        workspace_identity: WorkspaceIdentity { device: 1, inode: 1 },
                    },
                )
                .await,
            Err(StoreError::InvalidHistory)
        ));
        store.close().await.expect("closed store");

        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let events = store.load_session(id).await.expect("replayed Events");
        assert_eq!(events.len(), 2);
        let view = SessionView::replay(id, &events)
            .expect("valid reduction")
            .expect("Session exists");
        assert_eq!(view.title, hostile_title);
        assert_eq!(view.last_sequence, 2);
        assert_eq!(view.workspace_identity, Some(WorkspaceIdentity { device: 1, inode: 1 }));
        store.close().await.expect("closed reopened store");

        let text = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .current_dir(temp.path())
            .args(["show", "--state-dir"])
            .arg(&state_path)
            .args(["--output", "text", &id.to_string()])
            .bounded_output()
            .expect("show process");
        assert!(text.status.success());
        assert_eq!(text.stderr, b"");
        assert_eq!(
            String::from_utf8(text.stdout).expect("UTF-8 output"),
            format!(
                "Session: {id}\nTitle: Continued\\u{{001b}}[31m\\u{{202e}} Session\n\
                 1 SessionStarted First Session\n\
                 2 SessionRenamed Continued\\u{{001b}}[31m\\u{{202e}} Session\n"
            )
        );
        let jsonl = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .current_dir(temp.path())
            .args(["show", "--state-dir"])
            .arg(&state_path)
            .args(["--output", "jsonl", &id.to_string()])
            .bounded_output()
            .expect("JSONL show process");
        assert!(jsonl.status.success());
        assert_eq!(jsonl.stderr, b"");
        assert!(jsonl.stdout.ends_with(b"\n"));
        assert!(!jsonl.stdout.contains(&0x1b));
        assert!(!String::from_utf8_lossy(&jsonl.stdout).contains('\u{202e}'));
        let lines: Vec<_> = jsonl
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<serde_json::Value>(line).expect("valid JSONL"))
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["kind"], "SessionStarted");
        assert_eq!(lines[1]["payload"]["title"], hostile_title);
        assert_eq!(jsonl.stdout.iter().filter(|byte| **byte == b'\n').count(), 2);
        for (line, event) in lines.iter().zip(&events) {
            assert_eq!(line["sequence"], event.sequence);
            assert_eq!(line["session_id"], id.to_string());
            assert!(line["run_id"].is_null());
            assert!(line["agent_run_id"].is_null());
            assert_eq!(line["event_version"], event.event.version());
            assert_eq!(line["created_at_ms"], event.created_at_ms);
        }

        let missing_path = temp.path().join("missing");
        let missing = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .current_dir(temp.path())
            .args(["show", "--state-dir"])
            .arg(&missing_path)
            .args(["--output", "text", &id.to_string()])
            .bounded_output()
            .expect("missing-state show process");
        assert!(!missing.status.success());
        assert_eq!(missing.stdout, b"");
        assert_eq!(
            missing.stderr,
            b"error: state directory unavailable or unsafe\n"
        );
        assert!(!missing_path.exists());

        let invalid_argument = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .current_dir(temp.path())
            .arg("--\u{1b}[31m")
            .bounded_output()
            .expect("invalid-argument process");
        assert!(!invalid_argument.status.success());
        assert_eq!(invalid_argument.stdout, b"");
        assert_eq!(
            invalid_argument.stderr,
            b"error: invalid arguments; run arany --help\n"
        );

        let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
            .expect("test fault injection");
        connection
            .execute(
                "INSERT INTO events (sequence, session_id, kind, event_version, payload, created_at_ms)
                 VALUES (3, ?1, 'UnknownEvent', 1, '{}', 1)",
                [id.to_string()],
            )
            .expect("injected unsupported Event");
        drop(connection);
        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store after injection");
        assert!(matches!(
            store.load_session(id).await,
            Err(StoreError::InvalidHistory)
        ));
        store.close().await.expect("closed store after rejected replay");

        let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
            .expect("test fault injection");
        connection
            .execute(
                "UPDATE events SET kind='SessionRenamed', payload=?1 WHERE sequence=3",
                [format!("{{\"title\":\"{}\"}}", "x".repeat(65_536))],
            )
            .expect("injected oversized Event");
        drop(connection);
        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store after oversized Event");
        assert!(matches!(
            store.load_session(id).await,
            Err(StoreError::InvalidHistory)
        ));
        store.close().await.expect("closed store after size rejection");
    });
}

#[test]
fn exec_process_rejects_unselected_or_unavailable_provider_before_workspace_admission() {
    let temp = tempfile::tempdir().expect("private temporary directory");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace directory");
    let state = temp.path().join("state");
    let missing_key = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .env("ANTHROPIC_API_KEY", "test-placeholder-key")
        .current_dir(temp.path())
        .args(["exec", "--state-dir"])
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .args(["--provider", "openai", "--model", "gpt-5.4", "/literal"])
        .bounded_output()
        .expect("exec process");
    assert!(!missing_key.status.success());
    assert_eq!(missing_key.stdout, b"");
    assert_eq!(missing_key.stderr, b"error: OPENAI_API_KEY unavailable\n");
    assert!(!state.exists());

    #[cfg(unix)]
    for workspace_id in ["", "default", "wrkspc_a\r\nx-header:value"] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
        command
            .env_clear()
            .env("ANTHROPIC_API_KEY", "test-placeholder-key")
            .env("ANTHROPIC_WORKSPACE_ID", workspace_id)
            .current_dir(temp.path())
            .args(["exec", "--state-dir"])
            .arg(&state)
            .arg("--workspace")
            .arg(&workspace)
            .args([
                "--provider",
                "anthropic",
                "--model",
                "claude-opus-5-5",
                "prompt",
            ]);
        let rejected = process_output_before_deadline(command);
        assert!(!rejected.status.success());
        assert_eq!(rejected.stdout, b"");
        assert_eq!(rejected.stderr, b"error: invalid ANTHROPIC_WORKSPACE_ID\n");
        assert!(!state.exists(), "malformed API workspace created State");
    }

    let malformed_model = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(temp.path())
        .args(["exec", "--state-dir"])
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .args([
            "--provider",
            "openai",
            "--model",
            "bad\nmodel",
            "--effort",
            "high",
            "prompt",
        ])
        .bounded_output()
        .expect("malformed native model process");
    assert!(!malformed_model.status.success());
    assert_eq!(malformed_model.stdout, b"");
    assert_eq!(malformed_model.stderr, b"error: invalid native model ID\n");
    assert!(!state.exists());

    for (profile, model, effort, expected_error) in [
        (
            "openai",
            "gpt-6.1-sol",
            "max",
            b"error: OPENAI_API_KEY unavailable\n".as_slice(),
        ),
        (
            "anthropic",
            "claude-opus-5-5",
            "medium",
            b"error: ANTHROPIC_API_KEY unavailable\n".as_slice(),
        ),
        (
            "openai",
            "future-model",
            "low",
            b"error: OPENAI_API_KEY unavailable\n".as_slice(),
        ),
        (
            "anthropic",
            "future-model",
            "low",
            b"error: ANTHROPIC_API_KEY unavailable\n".as_slice(),
        ),
    ] {
        let selection = Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .current_dir(temp.path())
            .args(["exec", "--state-dir"])
            .arg(&state)
            .arg("--workspace")
            .arg(&workspace)
            .args([
                "--provider",
                profile,
                "--model",
                model,
                "--effort",
                effort,
                "prompt",
            ])
            .bounded_output()
            .expect("reviewed native model process");
        assert!(!selection.status.success(), "{profile}/{model}");
        assert_eq!(selection.stdout, b"", "{profile}/{model}");
        assert_eq!(selection.stderr, expected_error, "{profile}/{model}");
        assert!(!state.exists(), "{profile}/{model}");
    }

    let overlapping_state = workspace.join("state");
    let overlap = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(temp.path())
        .args(["exec", "--state-dir"])
        .arg(&overlapping_state)
        .arg("--workspace")
        .arg(&workspace)
        .args(["--provider", "openai", "--model", "gpt-5.4", "prompt"])
        .bounded_output()
        .expect("overlap process");
    assert!(!overlap.status.success());
    assert_eq!(overlap.stdout, b"");
    assert_eq!(
        overlap.stderr,
        b"error: state directory overlaps the Workspace\n"
    );
    assert!(!overlapping_state.exists());

    let unsupported_model = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(temp.path())
        .args(["exec", "--state-dir"])
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .args(["--provider", "openai", "--model", "unsupported", "prompt"])
        .bounded_output()
        .expect("unsupported model process");
    assert!(!unsupported_model.status.success());
    assert_eq!(unsupported_model.stdout, b"");
    assert_eq!(
        unsupported_model.stderr,
        b"error: invalid native model/effort; unknown models require --effort LEVEL\n"
    );
    assert!(!state.exists());

    let unsupported_effort = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(temp.path())
        .args(["exec", "--state-dir"])
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .args([
            "--provider",
            "openai",
            "--model",
            "gpt-5.4",
            "--effort",
            "max",
            "prompt",
        ])
        .bounded_output()
        .expect("unsupported effort process");
    assert!(!unsupported_effort.status.success());
    assert_eq!(unsupported_effort.stdout, b"");
    assert_eq!(
        unsupported_effort.stderr,
        b"error: invalid native model/effort; unknown models require --effort LEVEL\n"
    );
    assert!(!state.exists());

    let missing_anthropic_key = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .env("OPENAI_API_KEY", "test-placeholder-key")
        .current_dir(temp.path())
        .args(["exec", "--state-dir"])
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .args([
            "--provider",
            "anthropic",
            "--model",
            "claude-sonnet-5",
            "prompt",
        ])
        .bounded_output()
        .expect("Anthropic exec process");
    assert!(!missing_anthropic_key.status.success());
    assert_eq!(missing_anthropic_key.stdout, b"");
    assert_eq!(
        missing_anthropic_key.stderr,
        b"error: ANTHROPIC_API_KEY unavailable\n"
    );
    assert!(!state.exists());

    let unsupported_anthropic_model = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(temp.path())
        .args(["exec", "--state-dir"])
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .args([
            "--provider",
            "anthropic",
            "--model",
            "unsupported",
            "prompt",
        ])
        .bounded_output()
        .expect("unsupported Anthropic process");
    assert!(!unsupported_anthropic_model.status.success());
    assert_eq!(unsupported_anthropic_model.stdout, b"");
    assert_eq!(
        unsupported_anthropic_model.stderr,
        b"error: invalid native model/effort; unknown models require --effort LEVEL\n"
    );
    assert!(!state.exists());

    let no_provider = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(temp.path())
        .args(["exec", "--model", "gpt-5.4", "prompt"])
        .bounded_output()
        .expect("unselected provider process");
    assert!(!no_provider.status.success());
    assert_eq!(no_provider.stdout, b"");
    assert_eq!(
        no_provider.stderr,
        b"error: invalid arguments; run arany --help\n"
    );
    assert!(!state.exists());
}

#[cfg(unix)]
#[test]
fn private_state_admission_rejects_unsafe_path_objects() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let temp = tempfile::tempdir().expect("temporary root");
    let state = temp.path().join("state");
    StateRoot::admit(&state).expect("initial private state");

    let alias = temp.path().join("alias");
    symlink(&state, &alias).expect("test symlink");
    assert!(StateRoot::open_existing(&alias).is_err());

    let unnormalized = state.join("..").join("state");
    assert!(matches!(
        StateRoot::admit(&unnormalized),
        Err(StoreError::InvalidStateDirectory)
    ));

    let file = temp.path().join("ordinary-file");
    std::fs::write(&file, b"data").expect("test file");
    assert!(StateRoot::admit(&file.join("state")).is_err());

    let database = state.join("events.sqlite3");
    std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o644))
        .expect("weak test permissions");
    assert!(matches!(
        StateRoot::open_existing(&state),
        Err(StoreError::StateNotPrivate)
    ));
}

#[test]
fn incomplete_run_replays_as_interrupted_and_a_new_run_can_start() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let identity_state = temp.path().join("identity");
        let identity_id = create_session(
            StateRoot::admit(&identity_state).unwrap(),
            workspace.clone(),
            None,
        )
        .await
        .unwrap();
        let identity = resume_session(
            StateRoot::open_existing(&identity_state).unwrap(),
            workspace.clone(),
            identity_id,
        )
        .await
        .unwrap()
        .workspace_identity
        .unwrap();
        for (prefix_len, cold, defaults_first) in [
            (2, false, false),
            (2, true, true),
            (5, false, true),
            (5, true, false),
            (7, false, false),
            (7, true, true),
        ] {
            let state_path = temp.path().join(format!("state-{prefix_len}-{cold}"));
            let session_id = SessionId::new();
            let goal_id = arany::GoalId::new();
            let interrupted_run = RunId::new();
            let primary = AgentRunId::new();
            let config = RunConfig {
                supervisor: None,
                worker_policy: None,
                agent_definitions: None,
                roster: None,
                providers: None,
                provider: "scripted".into(),
                model: "test-model".into(),
                effort: None,
                custom_profile_provenance: None,
                saved_api_account_id: None,
                chatgpt_provenance: None,
                output_token_bound: arany::OutputTokenBound::ProviderEnforced,
                context_window_tokens: None,
                policy: CollaborationPolicy::Single,
                output_token_cap: 4096,
                provider_concurrency: 1,
                workspace_device: identity.device,
                workspace_inode: identity.inode,
                user_instruction_digest: None,
                instruction_sources: Vec::new(),
                launch_directory: String::new(),
                include_digests: Vec::new(),
                history_run_ids: Vec::new(),
                excluded_history_runs: 0,
                context_usage: None,
                compaction_event_sequence: None,
                compaction_content_digest: None,
                tool_policy: None,
            };
            let mut store =
                Store::open(StateRoot::admit(&state_path).expect("private state")).expect("store");
            let prefix = [
                Event::SessionStarted {
                    title: "New Session".into(),
                    workspace_identity: identity,
                },
                Event::GoalStarted {
                    include_paths: Some(Vec::new()),
                    goal_id,
                    objective: "Question".into(),
                    workspace_identity: identity,
                    budget: arany::GoalBudget::default(),
                    phase: arany::GoalPhase::Research,
                    autonomous: false,
                    resource_settings: arany::ToolResourceSettings::default(),
                    continues: None,
                },
                Event::GoalRunLinked {
                    goal_id,
                    run_id: interrupted_run,
                    phase: arany::GoalPhase::Research,
                },
                Event::MessageAccepted {
                    skill: None,
                    run_id: interrupted_run,
                    text: "Question".into(),
                    images: Vec::new(),
                },
                Event::RunStarted {
                    run_id: interrupted_run,
                    config: config.clone(),
                    active_deadline_ms: None,
                },
                Event::AgentSpawned {
                    profile: None,
                    assignment: None,
                    definition: None,
                    run_id: interrupted_run,
                    agent_run_id: primary,
                    role: AgentRole::Primary,
                    ordinal: 0,
                    objective: None,
                    grant: None,
                },
                Event::GoalProviderCallReserved {
                    goal_id,
                    run_id: interrupted_run,
                    agent_run_id: primary,
                    active_ceiling_ms: 0,
                },
                Event::ProviderCallRecorded {
                    run_id: interrupted_run,
                    agent_run_id: primary,
                    record: arany::ProviderCallRecord {
                        rate_limits: None,
                        delegation: None,
                        phase: arany::AgentPhase::RootPlan,
                        disposition: arany::ProviderCallDisposition::Finished,
                        response_id: Some("synthetic-response".into()),
                        input_tokens: Some(7),
                        output_tokens: Some(3),
                        wire_provenance: None,
                        refusal: None,
                        failure_reason: None,
                        requested_tools: 0,
                        cache_read_input_tokens: None,
                        cache_creation_input_tokens: None,
                    },
                },
                Event::AgentFinished {
                    worker: None,
                    run_id: interrupted_run,
                    agent_run_id: primary,
                    disposition: arany::AgentDisposition::Finished,
                    summary: Some("Observed answer".into()),
                    result: Some("Observed answer".into()),
                },
                Event::MessageCommitted {
                    run_id: interrupted_run,
                    text: "Observed answer".into(),
                },
            ];
            let event_count = match prefix_len {
                2 => 4,
                5 => 8,
                7 => prefix.len(),
                _ => unreachable!("declared crash point"),
            };
            for event in &prefix[..event_count] {
                store
                    .append(session_id, event.clone())
                    .await
                    .expect("valid unfinished prefix");
            }
            if cold {
                store.close().await.expect("store shutdown");
                store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
                    .expect("reopened store");
            }
            let events = store
                .load_session(session_id)
                .await
                .expect("durable prefix");
            let view = SessionView::replay(session_id, &events)
                .expect("valid prefix")
                .expect("Session");
            assert_eq!(view.runs[0].status, RunStatus::Interrupted);
            assert_eq!(view.workspace_identity, Some(identity));
            if prefix_len > 2 {
                let listed = list_sessions(
                    StateRoot::open_existing(&state_path).unwrap(),
                    workspace.clone(),
                )
                .await
                .unwrap();
                assert_eq!(
                    listed[0].title, "Question",
                    "an unfinished Run must not change the objective title"
                );
            }
            let interrupted = view.runs[0].clone();
            if prefix_len > 2 {
                assert_eq!(
                    interrupted.agents[0].status,
                    if prefix_len == 7 {
                        AgentStatus::Finished
                    } else {
                        AgentStatus::Interrupted
                    }
                );
                assert_eq!(interrupted.agents[0].provider_calls.len(), 1);
            }
            assert_eq!(
                interrupted.assistant_message.as_deref(),
                (prefix_len == 7).then_some("Observed answer")
            );
            assert!(interrupted.finished_sequence.is_none());
            let defaults = SessionDefaults {
                provider: Some("openai".into()),
                model: Some("gpt-5.4".into()),
                effort: Some(arany::Effort::High),
                account_id: None,
                policy: CollaborationPolicy::Single,
            };
            let mut metadata = [
                Event::SessionRenamed {
                    title: "Recovered conversation".into(),
                },
                Event::SessionDefaultChanged {
                    defaults: defaults.clone(),
                },
            ];
            if defaults_first {
                metadata.swap(0, 1);
            }
            for event in metadata {
                store
                    .append(session_id, event)
                    .await
                    .unwrap_or_else(|error| {
                        panic!("idle metadata after prefix {prefix_len}, cold={cold}: {error}")
                    });
            }
            let committed = store
                .load_session(session_id)
                .await
                .expect("metadata prefix");
            assert_eq!(&committed[..event_count], events.as_slice());
            assert_eq!(committed.len(), event_count + 2);
            assert!(
                committed[event_count..]
                    .iter()
                    .all(|event| event.run_id.is_none() && event.agent_run_id.is_none())
            );
            let recovered = store
                .load_view(session_id)
                .await
                .expect("recovered view")
                .expect("Session");
            assert_eq!(recovered.title, "Recovered conversation");
            assert_eq!(recovered.defaults, defaults);
            assert_eq!(
                recovered.runs.as_slice(),
                std::slice::from_ref(&interrupted)
            );
            let late = match prefix_len {
                2 => Event::RunStarted {
                    run_id: interrupted_run,
                    config: config.clone(),
                    active_deadline_ms: None,
                },
                5 => prefix[event_count].clone(),
                7 => Event::RunFinished {
                    run_id: interrupted_run,
                    disposition: arany::RunDisposition::Finished,
                    failure: None,
                    active_ms: 0,
                },
                _ => unreachable!("declared crash point"),
            };
            assert!(matches!(
                store.append(session_id, late).await,
                Err(StoreError::InvalidHistory)
            ));
            assert_eq!(
                store
                    .load_session(session_id)
                    .await
                    .expect("unchanged prefix"),
                committed
            );
            let next_run = RunId::new();
            store
                .append(session_id, Event::GoalResumed { goal_id })
                .await
                .expect("explicitly resumed interrupted Goal");
            store
                .append(
                    session_id,
                    Event::GoalRunLinked {
                        goal_id,
                        run_id: next_run,
                        phase: arany::GoalPhase::Research,
                    },
                )
                .await
                .expect("new Goal Run link");
            store
                .append(
                    session_id,
                    Event::MessageAccepted {
                        skill: None,
                        run_id: next_run,
                        text: "Try again".into(),
                        images: Vec::new(),
                    },
                )
                .await
                .expect("new Run after interrupted prefix");
            store
                .append(
                    session_id,
                    Event::RunStarted {
                        run_id: next_run,
                        config,
                        active_deadline_ms: None,
                    },
                )
                .await
                .expect("fresh Run starts");
            store
                .append(
                    session_id,
                    Event::AgentSpawned {
                        profile: None,
                        assignment: None,
                        definition: None,
                        run_id: next_run,
                        agent_run_id: AgentRunId::new(),
                        role: AgentRole::Primary,
                        ordinal: 0,
                        objective: None,
                        grant: None,
                    },
                )
                .await
                .expect("ordinary Run progress stays active");
            store.close().await.expect("store shutdown");
            let store = Store::open_read_only(
                StateRoot::open_existing(&state_path).expect("existing state"),
            )
            .expect("closed-history reopen");
            let events = store.load_session(session_id).await.expect("history");
            let view = SessionView::replay(session_id, &events)
                .expect("valid history")
                .expect("Session");
            assert_eq!(view.runs.len(), 2);
            assert_eq!(view.runs[0], interrupted);
            assert_eq!(view.runs[1].status, RunStatus::Interrupted);
            assert_eq!(view.defaults, defaults);
            assert_eq!(view.title, "Recovered conversation");
            store.close().await.expect("store shutdown");
        }
    });
}

#[cfg(unix)]
struct SupervisedChild(Option<std::process::Child>);

#[cfg(unix)]
impl SupervisedChild {
    fn child(&mut self) -> &mut std::process::Child {
        self.0.as_mut().expect("live child")
    }

    fn finish(&mut self) -> std::process::Output {
        process::capture(
            self.child(),
            std::time::Duration::from_secs(5),
            32 * 1024 * 1024,
        )
    }
}

#[cfg(unix)]
impl Drop for SupervisedChild {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(unix)]
fn process_output_before_deadline(mut command: Command) -> std::process::Output {
    use std::process::Stdio;
    use std::time::Duration;

    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut process = SupervisedChild(Some(command.spawn().expect("product process")));
    process::capture(process.child(), Duration::from_secs(5), 32 * 1024 * 1024)
}

#[cfg(unix)]
struct HeldProvider {
    ready_path: std::path::PathBuf,
}

#[cfg(unix)]
#[async_trait::async_trait]
impl arany::Provider for HeldProvider {
    fn identity(&self) -> &arany::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                arany::ProviderIdentity::new("scripted", "held-model", arany::Provenance::native())
            });
        &IDENTITY
    }

    async fn invoke(
        &self,
        request: arany::ProviderRequest,
    ) -> Result<arany::ProviderResponse, arany::ProviderError> {
        let ready_path = self.ready_path.clone();
        {
            assert_eq!(
                request.phase,
                arany::AgentPhase::RootPlan,
                "holder call phase"
            );
            assert!(request.model == "held-model", "holder call model");
            assert!(request.input.images.is_empty(), "holder call images");
            assert!(
                request.input.objective == "Held in another process",
                "holder call objective"
            );
            assert!(
                request.input.workspace_guidance.is_empty(),
                "holder call instructions"
            );
            assert_eq!(request.input.includes.len(), 1, "holder call includes");
            assert!(request.input.includes[0].contains("Pinned lock fixture"));
            assert!(request.input.history.is_empty(), "holder call history");
            assert!(
                request.input.context_summary.is_none(),
                "holder call summary"
            );
            assert!(
                request.input.child_results.is_empty(),
                "holder call children"
            );
            assert_eq!(request.max_output_tokens, 4096, "holder call output cap");
            std::fs::write(ready_path, b"ready").expect("Provider readiness marker");
            std::future::pending().await
        }
    }
}

#[cfg(unix)]
#[test]
#[ignore = "supervised process helper"]
fn process_lock_holder() {
    use arany::{CollaborationPolicy, EngineConfig, RunRequest};
    use engine_support::run_to_end;
    use std::sync::Arc;

    let state_path = std::env::var_os("ARANY_TEST_STATE_PATH").expect("state path");
    let workspace = std::env::var_os("ARANY_TEST_WORKSPACE_PATH").expect("Workspace path");
    let session_id = std::env::var("ARANY_TEST_SESSION_ID")
        .expect("Session ID")
        .parse::<SessionId>()
        .expect("valid Session ID");
    let ready_path = std::env::var_os("ARANY_TEST_READY_PATH").expect("ready path");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let provider = Arc::new(HeldProvider {
            ready_path: ready_path.into(),
        });
        let engine = arany::test_support::engine::open(
            StateRoot::admit(std::path::Path::new(&state_path)).expect("state admission"),
            EngineConfig::default(),
        )
        .expect("held Engine");
        run_to_end(
            &engine,
            &provider,
            RunRequest {
                skill: None,
                session_id: Some(session_id),
                title: None,
                objective: "Held in another process".into(),
                images: Vec::new(),
                workspace: workspace.into(),
                launch_directory: None,
                include_paths: vec!["held-include.txt".into()],
                policy: CollaborationPolicy::Single,
                goal: arany::GoalRequest::default(),
            },
        )
        .await
        .expect("Provider remains held");
    });
}

#[cfg(unix)]
#[test]
fn exec_process_rejects_a_busy_session_and_recovers_after_holder_death() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("private temporary directory");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace directory");
        let pinned_include = workspace.join("held-include.txt");
        std::fs::write(&pinned_include, "Pinned lock fixture").expect("pinned include");
        let state_path = temp.path().join("state");
        let session_id = create_session(
            StateRoot::admit(&state_path).expect("state admission"),
            workspace.clone(),
            Some("Locked Session".into()),
        )
        .await
        .expect("committed pinned Session");

        let ready_path = temp.path().join("holder-ready");
        let mut holder_command = Command::new(std::env::current_exe().expect("test executable"));
        holder_command.env_clear();
        #[cfg(debug_assertions)]
        holder_command.env(
            "ARANY_TEST_ACCOUNT_ROOT",
            arany::test_support::isolate_account_root(),
        );
        let mut holder = SupervisedChild(Some(
            holder_command
                .env("ARANY_TEST_STATE_PATH", &state_path)
                .env("ARANY_TEST_WORKSPACE_PATH", &workspace)
                .env("ARANY_TEST_SESSION_ID", session_id.to_string())
                .env("ARANY_TEST_READY_PATH", &ready_path)
                .args(["--ignored", "--exact", "process_lock_holder"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("lock holder process"),
        ));
        let ready_deadline = Instant::now() + Duration::from_secs(5);
        while !ready_path.exists() {
            assert!(
                holder.child().try_wait().expect("holder status").is_none(),
                "lock holder exited before readiness"
            );
            assert!(Instant::now() < ready_deadline, "lock holder deadline");
            std::thread::yield_now();
        }

        let contender = |resume_goal: Option<arany::GoalId>| {
            let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
            command.env_clear();
            #[cfg(debug_assertions)]
            command.env(
                "ARANY_TEST_ACCOUNT_ROOT",
                arany::test_support::isolate_account_root(),
            );
            command
                .env("OPENAI_API_KEY", "test-placeholder-key")
                .current_dir(temp.path())
                .args(["exec", "--state-dir"])
                .arg(&state_path)
                .arg("--workspace")
                .arg(&workspace)
                .args(["--provider", "openai", "--model", "gpt-5.4"])
                .args(["--session-id", &session_id.to_string()]);
            if let Some(goal_id) = resume_goal {
                command.arg("--resume-goal").arg(goal_id.to_string());
            }
            command.args(["--include", "missing.txt", "prompt"]);
            process_output_before_deadline(command)
        };
        let busy = contender(None);
        assert!(!busy.status.success());
        assert_eq!(busy.stdout, b"");
        assert_eq!(busy.stderr, b"error: Session has an active operation\n");

        holder.child().kill().expect("kill holder");
        let killed = holder.finish();
        assert!(!killed.status.success());
        let store =
            Store::open_read_only(StateRoot::open_existing(&state_path).expect("read-only state"))
                .expect("read-only Store after holder exit");
        let interrupted_events = store
            .load_session(session_id)
            .await
            .expect("durable interrupted Goal facts");
        let view = SessionView::replay(session_id, &interrupted_events)
            .expect("valid interrupted Goal history")
            .expect("Session");
        let goal_id = view
            .interrupted_goal_id()
            .expect("held Goal became interrupted");
        assert_eq!(
            view.goal(goal_id).unwrap().include_paths,
            Some(vec!["held-include.txt".into()])
        );
        store.close().await.expect("close interrupted Store");

        std::fs::remove_file(&pinned_include).expect("remove the originally admitted input");
        // Reading the replacement must fail locally with a different error, before Provider dispatch.
        std::fs::write(workspace.join("missing.txt"), [0xff]).expect("unrecorded include");
        let released = contender(Some(goal_id));
        assert!(!released.status.success());
        assert_eq!(released.stdout, b"");
        assert_eq!(
            released.stderr,
            b"error: \"held-include.txt\": explicit include does not exist\n"
        );

        let store =
            Store::open_read_only(StateRoot::open_existing(&state_path).expect("read-only state"))
                .expect("read-only store after explicit resume preflight");
        let events = store
            .load_session(session_id)
            .await
            .expect("unchanged interrupted Session");
        assert_eq!(
            events, interrupted_events,
            "preflight failure does not resume"
        );
        let view = SessionView::replay(session_id, &events)
            .expect("valid interrupted prefix after refusal")
            .expect("Session");
        assert_eq!(view.runs.len(), 1);
        assert_eq!(view.runs[0].status, RunStatus::Interrupted);
        assert!(
            state_path
                .join(format!("session-{session_id}.lock"))
                .exists()
        );
        store.close().await.expect("read-only shutdown");
    });
}

#[test]
fn serve_stdio_initializes_refuses_missing_credentials_and_exits_cleanly() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    let input = format!(
        "{}\n{}\n",
        serde_json::json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"process","version":"1"}}}),
        serde_json::json!({"id":2,"method":"thread/start","params":{"cwd":workspace,"provider":"openai","model":"gpt-5.4"}})
    );
    let output = process::file_output(
        Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .current_dir(&workspace)
            .args(["serve", "--stdio", "--state-dir"])
            .arg(&state),
        Some(input.as_bytes()),
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let schema = arany_cli::protocol::protocol_schema();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let messages: Vec<serde_json::Value> = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(messages.len(), 2);
    assert!(messages.iter().all(|message| validator.is_valid(message)));
    assert_eq!(messages[0]["id"], 1);
    assert_eq!(
        messages[0]["result"]["capabilities"],
        serde_json::json!(arany_cli::protocol::methods::CAPABILITIES)
    );
    assert_eq!(messages[1]["error"]["code"], -32001);
    assert_eq!(
        messages[1]["error"]["data"]["reason"],
        "providerUnavailable"
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            assert!(
                arany::list_sessions(StateRoot::open_existing(&state).unwrap(), workspace.clone())
                    .await
                    .unwrap()
                    .is_empty()
            );
        });
}

#[test]
fn serve_rejects_unsupported_transport_and_terminal_input_before_state_admission() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path();
    let output = process::file_output(
        Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .current_dir(workspace)
            .arg("serve"),
        None,
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"error: unsupported transport; pass --stdio\n"
    );
    let (_master, slave) = process::cloexec_pty(None);
    let state = root.path().join("state");
    let output = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(workspace)
        .args(["serve", "--stdio", "--state-dir"])
        .arg(&state)
        .stdin(slave)
        .bounded_output_with_input()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"error: serve requires piped stdin and stdout\n"
    );
    assert!(!state.exists());
}

#[test]
fn headless_startup_archives_incompatible_history_and_fails_closed_on_unsafe_history() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for surface in [
        &["serve", "--stdio"][..],
        &["acp", "--provider", "openai", "--model", "gpt-5.4"],
    ] {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = root.path().join("state");
        runtime.block_on(async {
            create_session(StateRoot::admit(&state).unwrap(), workspace.clone(), None)
                .await
                .unwrap()
        });
        StateRoot::open_existing(&state)
            .unwrap()
            .record(arany::RecordKind::UiPreferences)
            .replace(b"synthetic-preferences")
            .unwrap();
        rusqlite::Connection::open(state.join("events.sqlite3"))
            .unwrap()
            .execute_batch(
                "UPDATE events SET event_version = event_version - 1 WHERE kind = 'SessionStarted'",
            )
            .unwrap();
        let history = std::fs::read(state.join("events.sqlite3")).unwrap();
        let run = || {
            process::file_output(
                Command::new(env!("CARGO_BIN_EXE_arany"))
                    .env_clear()
                    .current_dir(&workspace)
                    .args(surface)
                    .arg("--state-dir")
                    .arg(&state),
                Some(b""),
            )
        };

        let output = run();
        assert_eq!(output.status.code(), Some(0), "{surface:?}: {output:?}");
        let archived = std::fs::read_dir(state.join("archive"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(archived.len(), 1, "{surface:?}");
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            format!(
                "warning: History from an incompatible version archived intact to {}; started a new history\n",
                archived[0].display()
            )
        );
        assert_eq!(
            std::fs::read(archived[0].join("events.sqlite3")).unwrap(),
            history
        );
        assert_eq!(
            StateRoot::open_existing(&state)
                .unwrap()
                .record(arany::RecordKind::UiPreferences)
                .read()
                .unwrap()
                .as_deref(),
            Some(&b"synthetic-preferences"[..])
        );
        assert!(runtime.block_on(async {
            list_sessions(StateRoot::open_existing(&state).unwrap(), workspace.clone())
                .await
                .unwrap()
                .is_empty()
        }));

        let outside = root.path().join("outside");
        std::fs::write(&outside, b"outside-stays").unwrap();
        std::os::unix::fs::symlink(&outside, state.join("events.sqlite3-journal")).unwrap();
        let fresh = std::fs::read(state.join("events.sqlite3")).unwrap();
        let output = run();
        assert_eq!(output.status.code(), Some(1), "{surface:?}: {output:?}");
        assert_eq!(
            output.stderr,
            b"error: history files are unsafe or inaccessible; check permissions and retry\n"
        );
        assert_eq!(std::fs::read(state.join("events.sqlite3")).unwrap(), fresh);
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside-stays");
        assert_eq!(std::fs::read_dir(state.join("archive")).unwrap().count(), 1);
    }
}

#[test]
fn acp_stdio_negotiates_reports_auth_required_and_exits_cleanly() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    let input = format!(
        "{}\n{}\n{}\n",
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"authenticate","params":{"methodId":"arany-setup"}}),
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd":workspace,"mcpServers":[]}})
    );
    let output = process::file_output(
        Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .current_dir(&workspace)
            .args([
                "acp",
                "--provider",
                "openai",
                "--model",
                "gpt-5.4",
                "--state-dir",
            ])
            .arg(&state),
        Some(input.as_bytes()),
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/acp/schema.json")).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let mut messages: Vec<serde_json::Value> = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    messages.sort_by_key(|message| message["id"].as_i64());
    assert_eq!(messages.len(), 3);
    assert!(messages.iter().all(|message| validator.is_valid(message)));
    assert_eq!(messages[0]["result"]["protocolVersion"], 1);
    for message in &messages[1..] {
        assert_eq!(message["error"]["code"], -32000);
        assert_eq!(message["error"]["data"]["reason"], "auth_required");
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            assert!(
                arany::list_sessions(StateRoot::open_existing(&state).unwrap(), workspace)
                    .await
                    .unwrap()
                    .is_empty()
            );
        });
}

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", any(debug_assertions, arany_ci))
))]
#[test]
fn acp_profile_admits_session_cwd_instead_of_launch_directory() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    StateRoot::admit(&state).unwrap();
    private_file(
        &state.join("provider-roster.json"),
        &serde_json::to_vec(&serde_json::json!({
            "version":1,
            "profiles":[{"name":"main","routes":[{"provider":"openai","model":"gpt-5.4","effort":"low","account_id":null}]}],
            "presets":[{"name":"coding","providers":{"primary":"main","child":null,"review":null,"compaction":null},"child_profiles":["main"]}]
        })).unwrap(),
    );
    let input = format!(
        "{}\n{}\n{}\n{}\n",
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"authenticate","params":{"methodId":"arany-setup"}}),
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd":root.path(),"mcpServers":[]}}),
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"session/new","params":{"cwd":workspace,"mcpServers":[]}}),
    );
    let output = process::file_output(
        Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .env("OPENAI_API_KEY", "synthetic-acp-key")
            .current_dir(root.path())
            .args(["acp", "--profile", "coding", "--state-dir"])
            .arg(&state),
        Some(input.as_bytes()),
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(output.stderr, b"Roster preset: coding\nRoles: primary=main, child=main, review=main, compaction=main\nEligible child profiles: main\n  main[0]: openai/gpt-5.4 effort=low (native API)\n");
    let mut messages: Vec<serde_json::Value> = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    messages.sort_by_key(|message| message["id"].as_i64());
    assert_eq!(messages.len(), 4, "{messages:?}");
    assert_eq!(messages[0]["result"]["protocolVersion"], 1);
    assert_eq!(messages[1]["result"], serde_json::json!({}));
    assert_eq!(messages[2]["error"]["data"]["reason"], "invalidParams");
    let id: SessionId = serde_json::from_value(messages[3]["result"]["sessionId"].clone()).unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
            let events = store.load_session(id).await.unwrap();
            let view = SessionView::replay(id, &events).unwrap().unwrap();
            use std::os::unix::fs::MetadataExt;
            let metadata = std::fs::metadata(&workspace).unwrap();
            assert_eq!(
                view.workspace_identity,
                Some(WorkspaceIdentity {
                    device: metadata.dev(),
                    inode: metadata.ino()
                })
            );
            assert!(view.runs.is_empty());
            store.close().await.unwrap();
            assert_eq!(
                arany::list_sessions(StateRoot::open_existing(&state).unwrap(), workspace)
                    .await
                    .unwrap()
                    .len(),
                1
            );
        });
}

#[test]
fn acp_refuses_terminal_input_before_state_admission() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let (_master, slave) = process::cloexec_pty(None);
    let output = Command::new(env!("CARGO_BIN_EXE_arany"))
        .env_clear()
        .current_dir(root.path())
        .args([
            "acp",
            "--provider",
            "openai",
            "--model",
            "gpt-5.4",
            "--tools",
            "--state-dir",
        ])
        .arg(&state)
        .stdin(slave)
        .bounded_output_with_input()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"error: acp requires piped stdin and stdout\n"
    );
    assert!(!state.exists());
}

fn check_output(
    bytes: &[u8],
    state: &std::path::Path,
    disposition: arany::CheckDisposition,
) -> Vec<u8> {
    let text = std::str::from_utf8(bytes).expect("check output UTF-8");
    let (prefix, receipt) = text.rsplit_once("Check: ").expect("durable check receipt");
    let (id, status) = receipt.split_once('\n').expect("check disposition");
    let id: arany::CheckId = id.parse().expect("check UUID");
    assert_eq!(status, format!("Disposition: {disposition}\n"));
    let database = rusqlite::Connection::open_with_flags(
        state.join("events.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .expect("closed check database");
    let mut statement = database.prepare("SELECT kind, event_version, payload FROM provider_check_events WHERE check_id=?1 ORDER BY sequence").expect("check events");
    let events: Vec<arany::CheckEvent> = statement
        .query_map([id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .expect("query")
        .map(|row| {
            let (kind, version, payload) = row.expect("event row");
            let event: arany::CheckEvent =
                serde_json::from_str(&payload).expect("typed check event");
            assert_eq!(kind, event.kind());
            assert_eq!(version, event.version());
            event
        })
        .collect();
    let view = arany::CheckView::replay(id, &events)
        .expect("strict check replay")
        .expect("check");
    assert_eq!(view.disposition, Some(disposition));
    if disposition == arany::CheckDisposition::Succeeded {
        let catalog_pages = events
            .windows(2)
            .filter(|pair| {
                matches!(
                    *pair,
                    [
                        arany::CheckEvent::ProviderCheckCallStarted {
                            kind: arany::CheckCallKind::Catalog,
                            ..
                        },
                        arany::CheckEvent::ProviderCheckCallFinished {
                            disposition: arany::CheckDisposition::Succeeded,
                            ..
                        }
                    ]
                )
            })
            .count();
        assert_eq!(
            view.completed_calls as usize,
            view.start.probes.len() + catalog_pages
        );
    }
    prefix.as_bytes().to_vec()
}

#[cfg(unix)]
fn assert_check_privacy(output: &std::process::Output, state: &std::path::Path, canaries: &[&str]) {
    let database = std::fs::read(state.join("events.sqlite3")).expect("whole check database");
    let diagnostics = match std::fs::read(state.join("development.log")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("diagnostic inspection failed: {error}"),
    };
    for (channel, bytes) in [
        ("stdout", output.stdout.as_slice()),
        ("stderr", output.stderr.as_slice()),
        ("database", database.as_slice()),
        ("diagnostics", diagnostics.as_slice()),
    ] {
        for canary in canaries {
            assert!(
                !bytes
                    .windows(canary.len())
                    .any(|part| part == canary.as_bytes()),
                "check leaked a private canary into {channel}"
            );
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn attached_usage_reopens_complete_token_facts_without_credentials_or_events() {
    use arany::test_support::scenario::{ScriptedProvider, request};
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    use std::io::Write;
    use std::process::Stdio;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (id, before, view) = runtime.block_on(async {
        let engine =
            engine_support::open(StateRoot::admit(&state).unwrap(), Default::default()).unwrap();
        let mut provider =
            ScriptedProvider::new(vec![arany::test_support::goals::finish_with("Done")])
                .with_profile("openai", "gpt-5.4");
        provider.usage_override = Some(arany::Usage {
            input_tokens: Some(12_340),
            output_tokens: Some(8),
            cache_read_input_tokens: Some(7),
            cache_creation_input_tokens: Some(3),
        });
        let provider = Arc::new(provider);
        let mut request = request(&workspace, "Inspect usage");
        request.include_paths.clear();
        let result = engine_support::run_to_end(&engine, &provider, request)
            .await
            .unwrap();
        engine.close().await.unwrap();
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        set_session_defaults(
            StateRoot::open_existing(&state).unwrap(),
            workspace.clone(),
            result.session_id,
            SessionDefaults {
                provider: Some("openai".into()),
                model: Some("gpt-5.4".into()),
                effort: Some(arany::Effort::Low),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
        let events = store.load_session(result.session_id).await.unwrap();
        let view = store.load_view(result.session_id).await.unwrap().unwrap();
        store.close().await.unwrap();
        (result.session_id, events, view)
    });
    let expected = format!(
        "Usage page 1 of 1\nUsage: Provider-reported tokens\nCommitted through Event {}\nCache reads/writes are included in input\nOutput includes reported reasoning tokens\n\nCurrent/latest Run\nProvider: openai\nModel: gpt-5.4\nInput: 12,340\nOutput: 8\nCache read (included in input): 7\nCache write (included in input): 3\nAPI rate limits: unknown (not reported)\n\nSession totals\nProvider: openai\nModel: gpt-5.4\nInput: 12,340\nOutput: 8\nCache read (included in input): 7\nCache write (included in input): 3\nAPI rate limits: unknown (not reported)\n",
        view.last_sequence
    );
    let report_ready = format!("{expected}Input:\n");
    for _ in 0..2 {
        let mut command = isolation::account_isolated_script(temp.path());
        command.env_clear().current_dir(&workspace)
            .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
            .env("ARANY_TEST_STATE", &state).env("ARANY_TEST_WORKSPACE", &workspace)
            .env("ARANY_TEST_SESSION", id.to_string()).env("TERM", "dumb")
            .args(["-q", "-e", "-c", "exec \"$ARANY_TEST_EXE\" --screen-reader --no-color --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\"", "/dev/null"])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = loopback::ChildGuard::new(command.spawn().unwrap());
        let mut input = child.child().stdin.take().unwrap();
        let mut output = child.child().stdout.take().unwrap();
        let flags = fcntl_getfl(&output).unwrap();
        fcntl_setfl(&output, flags | OFlags::NONBLOCK).unwrap();
        let mut bytes = Vec::new();
        let mut answered = session_picker::PtyResponses::default();
        for (needle, action) in [("Input:\n", "/usage\n"), (report_ready.as_str(), "/exit\n")] {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !String::from_utf8_lossy(&bytes)
                .replace('\r', "")
                .contains(needle)
            {
                session_picker::pump(&mut output, &mut input, &mut bytes, &mut answered);
                assert!(
                    Instant::now() < deadline,
                    "usage PTY stage: waiting for {needle:?}"
                );
                std::thread::yield_now();
            }
            input.write_all(action.as_bytes()).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            session_picker::pump(&mut output, &mut input, &mut bytes, &mut answered);
            if let Some(status) = child.child().try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < deadline, "usage PTY exit");
            std::thread::yield_now();
        }
        let text = String::from_utf8(bytes).unwrap().replace('\r', "");
        let start = text.find("Usage page 1 of 1\n").unwrap();
        assert_eq!(&text[start..start + expected.len()], expected);
        assert!(!text.contains('\u{1b}'));
        runtime.block_on(async {
            let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
            assert_eq!(store.load_session(id).await.unwrap(), before);
            assert_eq!(store.load_view(id).await.unwrap().unwrap(), view);
            store.close().await.unwrap();
        });
    }
}
#[cfg(all(unix, any(debug_assertions, target_os = "linux")))]
#[path = "session_run/storage.rs"]
mod storage;
