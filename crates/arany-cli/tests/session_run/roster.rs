use super::*;
use serde_json::json;
use std::{os::unix::fs::PermissionsExt, path::Path, process::Stdio};

const TERMINAL_REQUIRED: &[u8] =
    b"error: attached mode requires terminal stdin and stderr; use arany exec for pipes\n";

fn record() -> serde_json::Value {
    json!({"version":1,"profiles":[
        {"name":"main","routes":[{"provider":"openai","model":"gpt-5.4","effort":"low","account_id":null}]},
        {"name":"unused","routes":[{"provider":"anthropic","model":"claude-sonnet-5","effort":"high","account_id":null}]}
    ],"presets":[{"name":"coding","providers":{"primary":"main","child":null,"review":null,"compaction":null},"child_profiles":["main","unused"]}]})
}

fn command(workspace: &Path, state: &Path, extra: &[&str]) -> std::process::Output {
    let run = |attached| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
        command.env_clear().current_dir(workspace);
        if attached {
            command.args(["--screen-reader"]);
        } else {
            command.arg("exec");
        }
        command
            .arg("answer")
            .args(["--profile", "coding", "--state-dir"])
            .arg(state)
            .arg("--workspace")
            .arg(workspace)
            .args(extra)
            .stdin(Stdio::null())
            .bounded_output()
            .unwrap()
    };
    let output = run(false);
    if !extra.contains(&"--output") {
        let attached = run(true);
        assert_eq!(
            attached.status.code(),
            output.status.code(),
            "attached refusal exit"
        );
        assert_eq!(attached.stdout, output.stdout, "attached refusal stdout");
        let mut expected = output.stderr.clone();
        if expected.windows(7).any(|part| part == b"refused") {
            // Credential admission follows folder consent, which needs a terminal.
            let error = expected
                .windows(7)
                .rposition(|part| part == b"error: ")
                .unwrap();
            expected.truncate(error);
            expected.extend_from_slice(TERMINAL_REQUIRED);
        }
        assert_eq!(attached.stderr, expected, "attached refusal stderr");
    }
    output
}

#[test]
fn profile_refusals_precede_workspace_input_and_keep_stdout_empty() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    nix::unistd::mkfifo(
        &workspace.join("AGENTS.md"),
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .unwrap();
    let state = temp.path().join("state");
    StateRoot::admit(&state).unwrap();
    let roster = state.join("provider-roster.json");
    let value = record();
    private_file(&roster, &serde_json::to_vec(&value).unwrap());
    let manifest = "Roster preset: coding\nRoles: primary=main, child=main, review=main, compaction=main\nEligible child profiles: main, unused\n  main[0]: openai/gpt-5.4 effort=low (native API)\n  unused[0]: anthropic/claude-sonnet-5 effort=high (native API)\n";
    for (extra, expected) in [
        (
            vec!["--provider", "anthropic"],
            "error: explicit Provider/model/effort differs from --profile primary\n".to_owned(),
        ),
        (
            vec!["--model", "gpt-6-luna"],
            "error: explicit Provider/model/effort differs from --profile primary\n".to_owned(),
        ),
        (
            vec!["--effort", "high"],
            "error: explicit Provider/model/effort differs from --profile primary\n".to_owned(),
        ),
        (
            vec![],
            format!(
                "{manifest}error: roster profile main route 0 refused: Provider role route openai/gpt-5.4 refused: selected native API credentials or workspace configuration unavailable\n"
            ),
        ),
        (
            vec!["--output", "jsonl"],
            format!(
                "{manifest}error: roster profile main route 0 refused: Provider role route openai/gpt-5.4 refused: selected native API credentials or workspace configuration unavailable\n"
            ),
        ),
    ] {
        let output = command(&workspace, &state, &extra);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(output.stdout, b"");
        assert_eq!(output.stderr, expected.as_bytes());
    }
    let mut unused = Command::new(env!("CARGO_BIN_EXE_arany"));
    let output = unused
        .env_clear()
        .env("OPENAI_API_KEY", "synthetic-roster-key")
        .current_dir(&workspace)
        .args(["exec", "--profile", "coding", "--state-dir"])
        .arg(&state)
        .arg("answer")
        .stdin(Stdio::null())
        .bounded_output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"");
    assert_eq!(output.stderr, format!("{manifest}error: roster profile unused route 0 refused: Provider role route anthropic/claude-sonnet-5 refused: selected native API credentials or workspace configuration unavailable\n").as_bytes());
    private_file(&state.join("provider-routing.json"), b"{}");
    let conflict = command(&workspace, &state, &[]);
    assert_eq!(conflict.status.code(), Some(1));
    assert_eq!(conflict.stdout, b"");
    assert_eq!(
        conflict.stderr,
        b"error: --profile conflicts with provider-routing.json; choose one routing source\n"
    );
    std::fs::remove_file(state.join("provider-routing.json")).unwrap();
    std::fs::set_permissions(&roster, std::fs::Permissions::from_mode(0o644)).unwrap();
    let unsafe_file = command(&workspace, &state, &[]);
    assert_eq!(unsafe_file.status.code(), Some(1));
    assert_eq!(unsafe_file.stdout, b"");
    assert_eq!(
        unsafe_file.stderr,
        b"error: Provider roster unavailable or unsafe\n"
    );
    std::fs::remove_file(&roster).unwrap();
    let missing = command(&workspace, &state, &[]);
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(missing.stdout, b"");
    assert_eq!(
        missing.stderr,
        b"error: provider-roster.json is unavailable\n"
    );
    assert_eq!(
        std::fs::metadata(state.join("events.sqlite3"))
            .unwrap()
            .len(),
        0,
        "refusal opens no journal"
    );
    let fresh = temp.path().join("fresh");
    let absent = command(&workspace, &fresh, &[]);
    assert_eq!(absent.status.code(), Some(1));
    assert!(!fresh.exists(), "refusal creates no State");
}

#[cfg(all(target_os = "linux", debug_assertions))]
#[test]
fn profile_saved_models_share_one_selected_identity_and_refuse_another() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    nix::unistd::mkfifo(
        &workspace.join("AGENTS.md"),
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .unwrap();
    let state = temp.path().join("state");
    StateRoot::admit(&state).unwrap();
    let account_root = temp.path().join("account-root");
    let account = super::setup::prepare_test_file_account_at(&account_root);
    let mut roster = record();
    roster["profiles"][0]["routes"][0]["account_id"] = json!(account);
    let same = json!({"name":"same","routes":[{"provider":"openai","model":"gpt-6-luna","effort":"low","account_id":account}]});
    roster["profiles"].as_array_mut().unwrap().push(same);
    roster["presets"][0]["child_profiles"] = json!(["main", "same", "unused"]);
    for other in [false, true] {
        if other {
            roster["profiles"][2]["routes"][0]["account_id"] = json!(uuid::Uuid::now_v7());
        }
        private_file(
            &state.join("provider-roster.json"),
            &serde_json::to_vec(&roster).unwrap(),
        );
        for entry in [vec!["exec", "answer"], vec!["--screen-reader", "answer"]] {
            let exec = entry[0] == "exec";
            let output = Command::new(env!("CARGO_BIN_EXE_arany"))
                .env_clear()
                .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
                .env("HOME", temp.path().join("home"))
                .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
                .current_dir(&workspace)
                .args(entry)
                .args(["--profile", "coding", "--state-dir"])
                .arg(&state)
                .stdin(Stdio::null())
                .bounded_output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1));
            assert_eq!(output.stdout, b"");
            let error = if other {
                "error: roster profile same route 0 refused: Provider role route openai/gpt-6-luna refused: invalid saved account; use arany --setup to choose or reset access\n"
            } else {
                "error: roster profile unused route 0 refused: Provider role route anthropic/claude-sonnet-5 refused: selected native API credentials or workspace configuration unavailable\n"
            };
            let expected = format!(
                "Roster preset: coding\nRoles: primary=main, child=main, review=main, compaction=main\nEligible child profiles: main, same, unused\n  main[0]: openai/gpt-5.4 effort=low (native API)\n  same[0]: openai/gpt-6-luna effort=low (native API)\n  unused[0]: anthropic/claude-sonnet-5 effort=high (native API)\n{error}"
            );
            if exec {
                assert_eq!(output.stderr, expected.as_bytes());
            } else {
                let manifest = expected.rfind("error: ").unwrap();
                assert_eq!(
                    output.stderr,
                    [&expected.as_bytes()[..manifest], TERMINAL_REQUIRED].concat()
                );
            }
            assert_eq!(
                std::fs::metadata(state.join("events.sqlite3"))
                    .unwrap()
                    .len(),
                0
            );
        }
    }
}

#[test]
#[cfg(target_os = "linux")]
fn attached_profile_admission_waits_for_folder_consent() {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    use std::io::Write;
    use std::time::{Duration, Instant};

    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    StateRoot::admit(&state).unwrap();
    private_file(
        &state.join("provider-roster.json"),
        &serde_json::to_vec(&record()).unwrap(),
    );
    let shell = "stty rows 24 cols 80; /arany --screen-reader --no-color --profile coding --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" answer";
    let mut command = super::isolation::isolated_script(temp.path());
    command
        .env_clear()
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("SHELL", "/bin/sh")
        .env("TERM", "dumb")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", shell, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = super::loopback::ChildGuard::new(command.spawn().unwrap());
    let mut input = child.child().stdin.take().unwrap();
    let mut output = child.child().stdout.take().unwrap();
    let flags = fcntl_getfl(&output).unwrap();
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).unwrap();
    let mut transcript = Vec::new();
    let mut answered = super::session_picker::PtyResponses::manual_consent();
    fn wait(
        output: &mut std::process::ChildStdout,
        input: &mut std::process::ChildStdin,
        transcript: &mut Vec<u8>,
        answered: &mut super::session_picker::PtyResponses,
        needle: &[u8],
    ) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !transcript.windows(needle.len()).any(|part| part == needle) {
            super::session_picker::pump(output, input, transcript, answered);
            assert!(
                Instant::now() < deadline,
                "missing {}: {}",
                String::from_utf8_lossy(needle),
                super::session_picker::tail(transcript)
            );
            std::thread::yield_now();
        }
    }
    wait(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Do you trust this folder?",
    );
    assert!(
        !transcript
            .windows(b"refused".len())
            .any(|part| part == b"refused"),
        "roster admission must wait for folder consent"
    );
    input.write_all(b"read only\r").unwrap();
    wait(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"roster profile main route 0 refused",
    );
    assert_eq!(
        std::fs::metadata(state.join("events.sqlite3"))
            .unwrap()
            .len(),
        0
    );
}
