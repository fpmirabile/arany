use crate::cli::{chatgpt, credentials};
use arany::{
    AnthropicProvider, ChatGptProvider, CompactionRecord, CustomProvider, Effort, Engine,
    NativeApiCredentials, OpenAiProvider, Provider, RunCancellation, RunId, RunOutcome,
    RunProgress, RunRequest, SessionId, StateRoot, Telemetry, native_protection_supported,
    resolve_native_effort_for_run,
};
use std::path::Path;
use uuid::Uuid;

#[derive(Clone, Copy)]
pub(super) struct ToolAccess<'a> {
    pub configured: bool,
    pub permissions: Option<&'a arany::WorkspacePermissions>,
    pub approvals: &'a arany::ToolApprovals,
}

enum SelectedProvider {
    OpenAi(OpenAiProvider),
    Anthropic(AnthropicProvider),
    ChatGpt(ChatGptProvider),
    Custom(CustomProvider),
}

pub(super) enum RunAttemptError {
    InterruptedBeforeRun,
    Failed(String),
}

#[derive(Clone, Copy)]
pub(super) struct Selection<'a> {
    pub workspace: &'a Path,
    pub profile: &'a str,
    pub model: &'a str,
    pub effort: Option<Effort>,
    pub account_id: Option<Uuid>,
}

pub(super) fn validate_local_selection(selection: Selection<'_>) -> Result<(), &'static str> {
    match selection.profile {
        "openai" | "anthropic" => {
            resolve_native_effort_for_run(selection.profile, selection.model, selection.effort)
                .map_err(
                    |_| "invalid native model/effort; choose a model and effort with /model",
                )?;
        }
        "chatgpt" => {
            if selection.account_id.is_none() {
                return Err("select a ChatGPT account with /setup first");
            }
            if selection.effort.is_none() {
                return Err("ChatGPT requires a model effort; use /model");
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) async fn preflight_selected(
    state_dir: &Path,
    selection: Selection<'_>,
) -> Result<(), String> {
    select_provider(state_dir, selection).await.map(|_| ())
}

pub(super) async fn run_selected(
    state_dir: &Path,
    telemetry: &Telemetry,
    selection: Selection<'_>,
    request: RunRequest,
    cancellation: RunCancellation,
    progress: RunProgress,
    tools: ToolAccess<'_>,
) -> Result<RunOutcome, RunAttemptError> {
    let mut admission_cancellation = cancellation.clone();
    let provider = tokio::select! {
        biased;
        () = admission_cancellation.cancelled() => return Err(RunAttemptError::InterruptedBeforeRun),
        result = select_provider(state_dir, selection) => {
            result.map_err(RunAttemptError::Failed)?
        }
    };
    match provider {
        SelectedProvider::OpenAi(provider) => {
            run_with_provider(
                state_dir,
                telemetry,
                provider,
                request,
                cancellation,
                progress,
                tools,
            )
            .await
        }
        SelectedProvider::Anthropic(provider) => {
            run_with_provider(
                state_dir,
                telemetry,
                provider,
                request,
                cancellation,
                progress,
                tools,
            )
            .await
        }
        SelectedProvider::ChatGpt(provider) => {
            run_with_provider(
                state_dir,
                telemetry,
                provider,
                request,
                cancellation,
                progress,
                tools,
            )
            .await
        }
        SelectedProvider::Custom(provider) => {
            run_with_provider(
                state_dir,
                telemetry,
                provider,
                request,
                cancellation,
                progress,
                tools,
            )
            .await
        }
    }
    .map_err(RunAttemptError::Failed)
}

pub(super) async fn compact_selected(
    state_dir: &Path,
    telemetry: &Telemetry,
    selection: Selection<'_>,
    session_id: SessionId,
    workspace: &Path,
    expected_run_id: Option<RunId>,
) -> Result<Option<CompactionRecord>, String> {
    match select_provider(state_dir, selection).await? {
        SelectedProvider::OpenAi(provider) => {
            compact_with_provider(
                state_dir,
                telemetry,
                provider,
                session_id,
                workspace,
                expected_run_id,
            )
            .await
        }
        SelectedProvider::Anthropic(provider) => {
            compact_with_provider(
                state_dir,
                telemetry,
                provider,
                session_id,
                workspace,
                expected_run_id,
            )
            .await
        }
        SelectedProvider::ChatGpt(provider) => {
            compact_with_provider(
                state_dir,
                telemetry,
                provider,
                session_id,
                workspace,
                expected_run_id,
            )
            .await
        }
        SelectedProvider::Custom(provider) => {
            compact_with_provider(
                state_dir,
                telemetry,
                provider,
                session_id,
                workspace,
                expected_run_id,
            )
            .await
        }
    }
}

async fn select_provider(
    state_dir: &Path,
    selection: Selection<'_>,
) -> Result<SelectedProvider, String> {
    select_provider_inner(state_dir, selection)
        .await
        .inspect_err(|_| {
            arany::record_development_failure(arany::DevelopmentFailure::ProviderAdmission);
        })
}

async fn select_provider_inner(
    state_dir: &Path,
    selection: Selection<'_>,
) -> Result<SelectedProvider, String> {
    validate_local_selection(selection)?;
    match selection.profile {
        "openai" => {
            let key = native_key(selection).await?.into_api_key();
            let provider = match selection.account_id {
                Some(id) => OpenAiProvider::from_saved_api_key_with_effort(
                    selection.model.to_owned(),
                    selection.effort,
                    key,
                    id,
                ),
                None => OpenAiProvider::from_api_key_with_effort(
                    selection.model.to_owned(),
                    selection.effort,
                    key,
                ),
            }
            .map_err(|_| "selected OpenAI API key unavailable".to_owned())?;
            Ok(SelectedProvider::OpenAi(provider))
        }
        "anthropic" => {
            let credentials = native_key(selection).await?;
            let provider = AnthropicProvider::from_credentials_with_effort(
                selection.model.to_owned(),
                selection.effort,
                credentials,
                selection.account_id,
            )
            .map_err(|_| "selected Anthropic API key unavailable".to_owned())?;
            Ok(SelectedProvider::Anthropic(provider))
        }
        "chatgpt" => {
            let account_id = selection
                .account_id
                .ok_or("select a ChatGPT account with /setup first")?;
            let effort = selection
                .effort
                .expect("local selection requires explicit ChatGPT effort");
            let provider = chatgpt::provider_for_run(
                selection.workspace.to_path_buf(),
                Some(account_id),
                selection.model.to_owned(),
                effort,
            )
            .await
            .map_err(|error| error.to_string())?;
            Ok(SelectedProvider::ChatGpt(provider))
        }
        _ => {
            if selection.account_id.is_some() {
                return Err("saved account requires a native Provider".into());
            }
            let name = selection
                .profile
                .strip_prefix("custom:")
                .ok_or_else(|| "unsupported Provider profile".to_owned())?;
            let root = StateRoot::open_existing(state_dir)
                .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
            let provider = CustomProvider::admit(&root, name, selection.model, selection.effort)
                .await
                .map_err(|error| error.to_string())?;
            Ok(SelectedProvider::Custom(provider))
        }
    }
}

async fn native_key(selection: Selection<'_>) -> Result<NativeApiCredentials, String> {
    match selection.account_id {
        Some(id) => credentials::load_selected(selection.workspace, id, selection.profile)
            .await
            .map_err(|error| error.to_string()),
        None => NativeApiCredentials::from_env(selection.profile).map_err(|_| {
            "selected native API credentials or workspace configuration unavailable".to_owned()
        }),
    }
}

async fn run_with_provider<P: Provider + 'static>(
    state_dir: &Path,
    telemetry: &Telemetry,
    provider: P,
    request: RunRequest,
    cancellation: RunCancellation,
    progress: RunProgress,
    tools: ToolAccess<'_>,
) -> Result<RunOutcome, String> {
    let trusted_tools =
        !tools.configured && super::permissions::supports_tools(Some(provider.profile_name()));
    let root = StateRoot::admit(state_dir)
        .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
    let mut engine = Engine::open_with_telemetry(root, provider, telemetry.clone())
        .map_err(|_| "state store unavailable".to_owned())?;
    if tools.configured {
        if !native_protection_supported() {
            return Err(super::permissions::UNAVAILABLE_NOTICE.to_owned());
        }
        engine
            .enable_tools(
                std::env::current_exe().map_err(|_| "Guard executable unavailable".to_owned())?,
            )
            .map_err(|error| {
                arany::record_development_failure(arany::DevelopmentFailure::ToolAdmission);
                error.to_string()
            })?;
    }
    if let Some(permissions) = tools
        .permissions
        .filter(|permissions| trusted_tools && permissions.is_trusted())
    {
        engine
            .enable_trusted_workspace(
                permissions.clone(),
                &request.workspace,
                std::env::current_exe().map_err(|_| "Guard executable unavailable".to_owned())?,
                tools.approvals.clone(),
            )
            .map_err(|error| {
                arany::record_development_failure(arany::DevelopmentFailure::ToolAdmission);
                error.to_string()
            })?;
    }
    engine.set_tool_approvals(tools.approvals.clone());
    let result = engine
        .run_with_progress(request, cancellation, progress)
        .await;
    let closed = engine.close().await;
    let outcome = result.map_err(|error| error.to_string())?;
    closed.map_err(|_| "store shutdown failed".to_owned())?;
    Ok(outcome)
}

async fn compact_with_provider<P: Provider + 'static>(
    state_dir: &Path,
    telemetry: &Telemetry,
    provider: P,
    session_id: SessionId,
    workspace: &Path,
    expected_run_id: Option<RunId>,
) -> Result<Option<CompactionRecord>, String> {
    let root = StateRoot::open_existing(state_dir)
        .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
    let mut engine = Engine::open_with_telemetry(root, provider, telemetry.clone())
        .map_err(|_| "state store unavailable".to_owned())?;
    let result = if let Some(expected_run_id) = expected_run_id {
        engine
            .auto_compact_session(session_id, workspace.to_path_buf(), expected_run_id)
            .await
    } else {
        engine
            .compact_session(session_id, workspace.to_path_buf())
            .await
            .map(Some)
    };
    let closed = engine.close().await;
    let record = result.map_err(|error| error.to_string())?;
    closed.map_err(|_| "store shutdown failed".to_owned())?;
    Ok(record)
}

#[cfg(all(test, target_os = "macos"))]
#[allow(dead_code)]
#[path = "../../../tests/common/process.rs"]
mod test_process;

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use arany::{
        AgentPhase, ApprovalMode, AttachedTerminal, ChatGptAdmission, ChatGptProvenance,
        CollaborationPolicy, Composer, Finish, OutputTokenBound, ProviderError, ProviderOutcome,
        ProviderRequest, ProviderResponse, RunStatus, SessionView, Store,
    };
    use std::{
        io::Write,
        os::unix::fs::{MetadataExt, PermissionsExt},
        os::unix::process::CommandExt,
        process::{Child, Command, Stdio},
        sync::{Arc, Mutex},
        time::Duration,
    };

    struct OwnedChild(Child);

    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(debug_assertions)]
    #[tokio::test]
    async fn saved_access_is_authorized_before_chat_without_exposing_secrets() {
        const ROOT: &str = "ARANY_TEST_SAVED_ACCESS_ROOT";
        const CASE: &str = "ARANY_TEST_SAVED_ACCESS_CASE";
        const TEST: &str = "cli::attached::run::tests::saved_access_is_authorized_before_chat_without_exposing_secrets";
        let Some(root) = std::env::var_os(ROOT) else {
            for case in [
                "chatgpt",
                "locked",
                "missing",
                "secret",
                "native",
                "native_cached",
                "replaced",
                "private_file",
                "unconfigured",
                "resume_locked",
                "continue_missing",
                "cancel",
            ] {
                let temp = tempfile::tempdir().unwrap();
                let pty = nix::pty::openpty(None, None).unwrap();
                let slave = std::fs::File::from(pty.slave);
                let master = std::fs::File::from(pty.master);
                let settings = nix::sys::termios::tcgetattr(&master).unwrap();
                let mut input = master.try_clone().unwrap();
                let mut quit = false;
                let listener = if case == "cancel" {
                    let listener =
                        std::os::unix::net::UnixListener::bind(temp.path().join("authorization"))
                            .unwrap();
                    listener.set_nonblocking(true).unwrap();
                    Some(listener)
                } else {
                    None
                };
                let mut connection = None;
                let mut helper_pid = None;
                let mut interrupted = false;
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .env_clear()
                    .env(ROOT, temp.path())
                    .env(CASE, case)
                    .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account"))
                    .env("ARANY_TEST_EXE", temp.path().join("credential-helper"))
                    .env("HOME", temp.path().join("legacy-home"))
                    .env("XDG_STATE_HOME", temp.path().join("legacy-state"))
                    .env("XDG_DATA_HOME", temp.path().join("legacy-data"))
                    .env("TERM", "dumb")
                    .args(["--exact", TEST, "--nocapture"])
                    .current_dir(temp.path())
                    .process_group(0)
                    .stdin(Stdio::from(slave.try_clone().unwrap()))
                    .stdout(Stdio::piped())
                    .stderr(Stdio::from(slave));
                let mut child = OwnedChild(command.spawn().unwrap());
                let product_pid =
                    rustix::process::Pid::from_raw(child.0.id().try_into().unwrap()).unwrap();
                drop(command);
                let output = test_process::capture_terminal(
                    &mut child.0,
                    master,
                    Duration::from_secs(20),
                    64 * 1024,
                    |bytes| {
                        if !interrupted && let Some(listener) = &listener {
                            match listener.accept() {
                                Ok((mut ready, _)) => {
                                    use std::io::Read;
                                    ready.set_nonblocking(false).unwrap();
                                    ready
                                        .set_read_timeout(Some(Duration::from_secs(4)))
                                        .unwrap();
                                    let mut pid = [0; 4];
                                    ready.read_exact(&mut pid).unwrap();
                                    helper_pid = rustix::process::Pid::from_raw(
                                        u32::from_be_bytes(pid).try_into().unwrap(),
                                    );
                                    connection = Some(ready);
                                    rustix::process::kill_process(
                                        product_pid,
                                        rustix::process::Signal::INT,
                                    )
                                    .unwrap();
                                    interrupted = true;
                                }
                                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                                Err(error) => panic!("authorization readiness: {error}"),
                            }
                        }
                        if !quit
                            && String::from_utf8_lossy(bytes)
                                .replace("\r\n", "\n")
                                .contains("Input:\n")
                        {
                            input.write_all(b"/quit\n").unwrap();
                            quit = true;
                        }
                    },
                );
                let transcript = String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n");
                assert!(
                    output.status.success(),
                    "saved access case {case}: {}",
                    String::from_utf8_lossy(&output.stdout)
                );
                assert!(
                    String::from_utf8_lossy(&output.stdout)
                        .contains("1 passed; 0 failed; 0 ignored"),
                    "exact child must execute one test"
                );
                let submitted = matches!(case, "resume_locked" | "continue_missing" | "cancel");
                assert_eq!(quit, !submitted, "startup input ownership for {case}");
                assert_eq!(
                    nix::sys::termios::tcgetattr(&input).unwrap(),
                    settings,
                    "startup must restore exact terminal settings"
                );
                if case == "cancel" {
                    use std::io::Read;
                    assert!(interrupted);
                    let mut byte = [0];
                    assert_eq!(
                        connection.as_mut().unwrap().read(&mut byte).unwrap(),
                        0,
                        "cancelled credential helper EOF"
                    );
                    assert_eq!(
                        rustix::process::test_kill_process(helper_pid.unwrap()),
                        Err(rustix::io::Errno::SRCH),
                        "credential helper must be reaped before exit"
                    );
                }
                assert!(
                    !transcript.contains("synthetic-startup-secret"),
                    "credential escaped to presentation"
                );
                let marker = temp.path().join("helper-access");
                if matches!(case, "private_file" | "unconfigured") {
                    assert!(
                        !marker.exists(),
                        "unconfigured/private file must not access the OS store"
                    );
                    assert!(!transcript.contains("Authorize saved access"));
                } else {
                    let phase = transcript.find("Approve this Arany credential in your OS password store; Esc or Ctrl+C exits\n").expect("authorization phase");
                    if !submitted {
                        assert!(phase < transcript.find("Input:\n").expect("composer"));
                    }
                    let calls = std::fs::read_to_string(marker).unwrap();
                    assert_eq!(
                        calls.lines().count(),
                        1,
                        "startup must not retry or inspect another item"
                    );
                    match case {
                        "locked" | "resume_locked" => {
                            assert!(transcript.contains("OS credential store is locked"))
                        }
                        "missing" | "continue_missing" => assert!(
                            transcript
                                .contains("Saved sign-in is missing from the OS password store")
                        ),
                        "secret" => assert!(transcript.contains("OS credential store unavailable")),
                        "replaced" => assert!(transcript.contains("invalid saved account")),
                        "cancel" => assert!(transcript.contains("Startup authorization cancelled")),
                        _ => assert!(
                            !transcript.contains("Error:"),
                            "authorization failed in {case}"
                        ),
                    }
                }
            }
            return;
        };
        let root = std::path::PathBuf::from(root);
        let case = std::env::var(CASE).unwrap();
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = StateRoot::admit(&root.join("account")).unwrap();
        let metadata = std::fs::metadata(&workspace).unwrap();
        let trust = serde_json::to_vec(&serde_json::json!({
            "version": 1, "workspaces": [{ "path": workspace,
                "device": metadata.dev(), "inode": metadata.ino(), "mode": "auto_edits" }]
        }))
        .unwrap();
        let trust_path = state.path().join("workspace-permissions.json");
        std::fs::write(&trust_path, trust).unwrap();
        std::fs::set_permissions(&trust_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let native = matches!(
            case.as_str(),
            "native" | "native_cached" | "replaced" | "private_file"
        );
        let account = crate::cli::credentials::SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            Some(Effort::Low),
            "synthetic-startup-secret".into(),
        )
        .unwrap();
        let id = if native { account.id } else { Uuid::now_v7() };
        let profile = if native { "openai" } else { "chatgpt" };
        let (operation, slot, response) = if case == "unconfigured" {
            ("forbidden", "forbidden".into(), "exit 1".into())
        } else if native {
            if case == "private_file" {
                crate::cli::credentials::save(
                    &workspace,
                    account,
                    crate::cli::credentials::AccountStorage::PrivateFile,
                )
                .await
                .unwrap();
                ("forbidden", "forbidden".into(), "exit 1".into())
            } else {
                state
                    .replace_saved_account_record(
                        br#"{"schema":1,"storage":"keyring","account":null}"#,
                    )
                    .unwrap();
                let record = serde_json::to_string(&account)
                    .unwrap()
                    .replace('\'', "'\\''");
                if case == "native" {
                    (
                        "read",
                        "default-native-api-account".into(),
                        format!("printf '%s' '{record}'"),
                    )
                } else {
                    (
                        "authorize",
                        "default-native-api-account".into(),
                        if case == "replaced" {
                            "exit 4"
                        } else {
                            "exit 0"
                        }
                        .into(),
                    )
                }
            }
        } else {
            state
                .replace_chatgpt_accounts_record(
                    &serde_json::to_vec(&serde_json::json!({
                        "schema": 1, "selected": id, "accounts": [{
                            "id": id, "host_id": "11111111-1111-4111-8111-111111111111",
                            "client_id": "synthetic-client", "subject": "synthetic-subject",
                            "storage": "keyring", "renewal_pending": false, "token": null
                        }], "model_checks": []
                    }))
                    .unwrap(),
                )
                .unwrap();
            let slot = crate::cli::chatgpt::selected_keyring_slot(&workspace, id)
                .unwrap()
                .unwrap();
            let response = match case.as_str() {
                "chatgpt" => "exit 0",
                "locked" | "resume_locked" => "exit 3",
                "missing" | "continue_missing" => "exit 2",
                "secret" => "printf synthetic-startup-secret",
                "cancel" => "held-helper",
                _ => panic!("invalid synthetic startup case"),
            }
            .into();
            ("authorize", slot, response)
        };
        if !matches!(case.as_str(), "native" | "unconfigured") {
            state
                .replace_model_preferences_record(
                    &serde_json::to_vec(&serde_json::json!({
                        "version": 1, "selected": { "profile": profile, "account_id": id },
                        "sources": [{ "profile": profile, "account_id": id, "model": "gpt-5.4",
                            "effort": "low", "catalog": ["gpt-5.4"] }]
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        let marker = root
            .join("helper-access")
            .to_str()
            .unwrap()
            .replace('\'', "'\\''");
        let identity = if native && operation == "authorize" {
            format!("[ \"$#\" -eq 5 ] && [ \"$4\" = '{id}' ] && [ \"$5\" = 'openai' ] || exit 1")
        } else {
            "[ \"$#\" -eq 3 ] || exit 1".into()
        };
        let response = if case == "cancel" {
            let socket = root
                .join("authorization")
                .to_str()
                .unwrap()
                .replace('\'', "'\\''");
            let executable = std::env::current_exe()
                .unwrap()
                .to_str()
                .unwrap()
                .replace('\'', "'\\''");
            format!(
                "export ARANY_TEST_AUTHORIZATION_SOCKET='{socket}'\nexec '{executable}' --exact cli::credentials::keyring_helper::tests::held_authorization_helper --ignored --nocapture"
            )
        } else {
            response
        };
        let script = format!(
            "#!/bin/sh\n[ \"$1\" = --internal-credential-helper ] || exit 1\nprintf '%s:%s\\n' \"$2\" \"$3\" >> '{marker}'\n{identity}\n[ \"$2\" = '{operation}' ] && [ \"$3\" = '{slot}' ] || exit 1\n{response}\n"
        );
        let helper = root.join("credential-helper");
        std::fs::write(&helper, script).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut args = super::super::AttachedArgs {
            workspace: Some(workspace.clone()),
            state_dir: Some(root.join("session")),
            screen_reader: true,
            ..Default::default()
        };
        if matches!(case.as_str(), "resume_locked" | "continue_missing") {
            let session = arany::create_session(
                StateRoot::admit(&root.join("session")).unwrap(),
                workspace.clone(),
                None,
            )
            .await
            .unwrap();
            arany::set_session_defaults(
                StateRoot::open_existing(&root.join("session")).unwrap(),
                workspace.clone(),
                session,
                arany::SessionDefaults {
                    provider: Some("chatgpt".into()),
                    model: Some("gpt-5.4".into()),
                    effort: Some(Effort::Low),
                    account_id: Some(id),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            let store =
                Store::open_read_only(StateRoot::open_existing(&root.join("session")).unwrap())
                    .unwrap();
            let before = store.load_session(session).await.unwrap();
            store.close().await.unwrap();
            args.prompt = Some("synthetic task must not be submitted".into());
            if case == "resume_locked" {
                args.resume = Some(Some(session.to_string()));
            } else {
                args.continue_session = true;
            }
            let error = super::super::run(args, Telemetry::disabled())
                .await
                .unwrap_err();
            assert!(error.contains(if case == "resume_locked" {
                "OS credential store is locked"
            } else {
                "Saved sign-in is missing"
            }));
            eprintln!("Startup rejected: {error}");
            let store =
                Store::open_read_only(StateRoot::open_existing(&root.join("session")).unwrap())
                    .unwrap();
            assert_eq!(
                store.load_session(session).await.unwrap(),
                before,
                "denied startup must preserve the closed Event prefix"
            );
            store.close().await.unwrap();
            return;
        }
        super::super::run(args, Telemetry::disabled())
            .await
            .unwrap();
        if case == "cancel" {
            assert!(
                !root.join("session").exists(),
                "cancelled startup must not create Session state"
            );
            eprintln!("Startup authorization cancelled");
            return;
        }
        assert!(
            arany::list_sessions(
                StateRoot::open_existing(&root.join("session")).unwrap(),
                workspace,
            )
            .await
            .unwrap()
            .is_empty(),
            "startup and quit must create no canonical Session or Run"
        );
    }

    struct Greeting {
        profile: &'static str,
        account_id: Uuid,
        observed: Arc<Mutex<Vec<ProviderRequest>>>,
    }

    impl Provider for Greeting {
        fn profile_name(&self) -> &str {
            self.profile
        }
        fn model_name(&self) -> &str {
            "synthetic-model"
        }
        fn reasoning_effort(&self) -> Option<Effort> {
            Some(Effort::Low)
        }
        fn max_concurrent_calls(&self) -> u8 {
            1
        }
        fn chatgpt_provenance(&self) -> Option<ChatGptProvenance> {
            (self.profile == "chatgpt").then_some(ChatGptProvenance {
                account_id: self.account_id,
                evidence_fingerprint: [1; 32],
                admission: ChatGptAdmission::AccountConsent,
            })
        }
        fn output_token_bound(&self) -> OutputTokenBound {
            if self.profile == "chatgpt" {
                OutputTokenBound::LocalAcceptanceOnly
            } else {
                OutputTokenBound::ProviderEnforced
            }
        }
        async fn invoke(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            self.observed.lock().unwrap().push(request);
            Ok(ProviderResponse {
                outcome: ProviderOutcome::Finish(Finish {
                    summary: "Greeting complete".into(),
                    result: "Hola".into(),
                }),
                response_id: None,
                input_tokens: Some(1),
                output_tokens: Some(1),
                wire_provenance: None,
            })
        }
    }

    #[tokio::test]
    async fn native_chat_preserves_turns_with_file_tools_and_rejects_unsupported_grants() {
        const FIXTURE: &str = "ARANY_TEST_NATIVE_CHAT_ROOT";
        const TEST: &str = "cli::attached::run::tests::native_chat_preserves_turns_with_file_tools_and_rejects_unsupported_grants";
        let Some(root) = std::env::var_os(FIXTURE) else {
            let temp = tempfile::tempdir().unwrap();
            let pty = nix::pty::openpty(None, None).unwrap();
            let slave = std::fs::File::from(pty.slave);
            let master = std::fs::File::from(pty.master);
            let mut input = master.try_clone().unwrap();
            let mut cancelled = false;
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .env_clear()
                .env(FIXTURE, temp.path())
                .env("HOME", temp.path().join("legacy-home"))
                .env("XDG_STATE_HOME", temp.path().join("legacy-state"))
                .env("XDG_DATA_HOME", temp.path().join("legacy-data"))
                .env("TERM", "dumb")
                .current_dir(temp.path())
                .args(["--exact", TEST, "--nocapture"])
                .process_group(0)
                .stdin(Stdio::from(slave.try_clone().unwrap()))
                .stdout(Stdio::piped())
                .stderr(Stdio::from(slave));
            command.env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account"));
            let mut child = OwnedChild(command.spawn().unwrap());
            drop(command);
            let output = test_process::capture_terminal(
                &mut child.0,
                master,
                Duration::from_secs(30),
                64 * 1024,
                |bytes| {
                    if !cancelled && String::from_utf8_lossy(bytes).replace("\r\n", "\n")
                        .contains("Type trust or read only; empty Enter selects read only. Ctrl+C exits:\n") {
                        input.write_all(b"\x04").unwrap();
                        cancelled = true;
                    }
                },
            );
            assert!(
                output.status.success(),
                "native chat regression: stdout {}; terminal {}",
                arany::escape_terminal(&String::from_utf8_lossy(&output.stdout)),
                arany::escape_terminal(&String::from_utf8_lossy(&output.stderr))
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed; 0 ignored"),
                "exact child must execute one test"
            );
            assert_eq!(cancelled, cfg!(debug_assertions));
            return;
        };
        let root = std::path::PathBuf::from(root);
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let account = StateRoot::admit(&root.join("account")).unwrap();
        let metadata = std::fs::metadata(&workspace).unwrap();
        let remembered = serde_json::to_vec(&serde_json::json!({
            "version": 1, "workspaces": [{ "path": workspace,
                "device": metadata.dev(), "inode": metadata.ino(), "mode": "auto_edits" }]
        }))
        .unwrap();
        let trust_record = account.path().join("workspace-permissions.json");
        std::fs::write(&trust_record, &remembered).unwrap();
        std::fs::set_permissions(&trust_record, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut terminal = AttachedTerminal::acquire_with_preference(true).unwrap();
        let mut composer = Composer::default();
        composer.apply(arany::TerminalInput::Character('x'));
        composer.set_approval_mode(Some(ApprovalMode::AutoEdits));
        let choice =
            super::super::permissions::admit(&mut terminal, &workspace, &mut composer, false).await;
        #[cfg(debug_assertions)]
        assert!(
            matches!(&choice, Ok(super::super::permissions::PermissionChoice::Selected(access)) if access.is_trusted())
        );
        #[cfg(not(debug_assertions))]
        assert!(
            matches!(
                &choice,
                Err(super::super::setup::SetupError::Recoverable(_))
            ),
            "optimized account override must reject before host access"
        );
        let permissions = match &choice {
            Ok(super::super::permissions::PermissionChoice::Selected(access)) => Some(access),
            _ => None,
        };
        let (approvals, _inbox) = arany::ToolApprovals::new(ApprovalMode::AutoEdits);
        for profile in ["chatgpt", "openai", "anthropic"] {
            let state = root.join(profile);
            let account_id = Uuid::now_v7();
            let observed = Arc::new(Mutex::new(Vec::new()));
            let mut session = None;
            for _ in 0..2 {
                let outcome = run_with_provider(
                    &state,
                    &Telemetry::disabled(),
                    Greeting {
                        profile,
                        account_id,
                        observed: observed.clone(),
                    },
                    RunRequest {
                        session_id: session,
                        title: None,
                        objective: "hola hola".into(),
                        images: Vec::new(),
                        workspace: workspace.clone(),
                        include_paths: Vec::new(),
                        policy: CollaborationPolicy::Single,
                    },
                    RunCancellation::new(),
                    RunProgress::new(),
                    ToolAccess {
                        configured: false,
                        permissions,
                        approvals: &approvals,
                    },
                )
                .await
                .expect("ordinary greeting retains the admitted native file capability");
                session = Some(outcome.session_id);
                assert_eq!(outcome.run.status, RunStatus::Finished);
            }
            {
                let requests = observed.lock().unwrap();
                assert_eq!(requests.len(), 2, "exact greeting call count");
                assert!(
                    requests
                        .iter()
                        .all(|request| request.phase == AgentPhase::RootPlan
                            && request.objective == "hola hola"
                            && request.tools.is_some() == permissions.is_some()
                            && request.collaboration == CollaborationPolicy::Single
                            && request.model == "synthetic-model"
                            && request.instructions.is_none()
                            && request.includes.is_empty()
                            && request.images.is_empty()
                            && request.child_results.is_empty()
                            && request.context_summary.is_none())
                );
                assert!(requests[0].history.is_empty());
                assert_eq!(
                    requests[1].history.len(),
                    1,
                    "resumed greeting retains its accepted turn"
                );
                assert_eq!(requests[1].history[0].user, "hola hola");
                assert_eq!(requests[1].history[0].assistant, "Hola");
            }
            let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
            let events = store.load_session(session.unwrap()).await.unwrap();
            let view = SessionView::replay(session.unwrap(), &events)
                .unwrap()
                .unwrap();
            assert_eq!(view.runs.len(), 2);
            assert!(view.runs.iter().all(|run| run.status == RunStatus::Finished
                && run.objective == "hola hola"
                && run.assistant_message.as_deref() == Some("Hola")
                && run.tools.is_empty()));
            store.close().await.unwrap();
            let private = state.join("tools.json");
            std::fs::write(&private, serde_json::to_vec(&serde_json::json!({"version":1,"workspace_paths":["."],"write":true,
                "commands":[{"name":"sh","executable":"/bin/sh","sha256":"0".repeat(64),"interpreter":true,"inputs":[]}],"skills":[],"mcp":[]})).unwrap()).unwrap();
            std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o600)).unwrap();
            let error = run_with_provider(
                &state,
                &Telemetry::disabled(),
                Greeting {
                    profile,
                    account_id,
                    observed: observed.clone(),
                },
                RunRequest {
                    session_id: session,
                    title: None,
                    objective: "explicit Tools".into(),
                    images: Vec::new(),
                    workspace: workspace.clone(),
                    include_paths: Vec::new(),
                    policy: CollaborationPolicy::Single,
                },
                RunCancellation::new(),
                RunProgress::new(),
                ToolAccess {
                    configured: true,
                    permissions: None,
                    approvals: &approvals,
                },
            )
            .await
            .err()
            .expect("explicit unsupported command grant must reject");
            assert_eq!(error, arany::ToolError::ProtectionUnavailable.to_string());
            assert_eq!(
                observed.lock().unwrap().len(),
                2,
                "rejection must make no Provider call"
            );
            let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
            assert_eq!(
                store.load_session(session.unwrap()).await.unwrap(),
                events,
                "explicit rejection preserves the committed prefix"
            );
            store.close().await.unwrap();
        }
        let choice = tokio::time::timeout(
            Duration::from_secs(3),
            super::super::permissions::choose(&mut terminal, &workspace, &mut composer, false),
        )
        .await
        .unwrap();
        #[cfg(debug_assertions)]
        assert!(matches!(
            choice,
            Ok(super::super::permissions::PermissionChoice::Exit)
        ));
        #[cfg(not(debug_assertions))]
        assert!(matches!(
            choice,
            Err(super::super::setup::SetupError::Recoverable(_))
        ));
        assert_eq!(
            super::super::permissions::cycle(&mut composer),
            if permissions.is_some() {
                "Request approvals · Shift+Tab changes mode"
            } else {
                "Read only · use /permissions to trust this folder"
            }
        );
        assert_eq!(composer.text(), "x");
        assert_eq!(
            composer.approval_mode(),
            permissions.map(|_| ApprovalMode::Request)
        );
        assert_eq!(std::fs::read(&trust_record).unwrap(), remembered);
        drop(terminal);
    }
}
