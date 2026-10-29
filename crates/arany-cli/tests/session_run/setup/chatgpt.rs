use super::*;

#[cfg(debug_assertions)]
#[test]
fn setup_reuse_and_reconnect_choices_preserve_selected_chatgpt_account_on_failure() {
    for (choice, disconnected, other_saved) in [
        ("Use saved", false, false),
        ("Reconnect", false, false),
        ("Reconnect", true, false),
        ("Use saved", true, true),
    ] {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let state = temp.path().join("state");
        let selected = prepare_selected_chatgpt_account(temp.path());
        let other = Uuid::now_v7();
        if disconnected || other_saved {
            let account_root =
                StateRoot::open_existing(&crate::product::fixture_account(temp.path()))
                    .expect("account root");
            let bytes = account_root
                .record(RecordKind::ChatgptAccounts)
                .read()
                .expect("account index")
                .expect("selected account metadata");
            let mut index: serde_json::Value =
                serde_json::from_slice(&bytes).expect("account index JSON");
            index["accounts"][0]["disconnected"] = disconnected.into();
            if other_saved {
                index["accounts"]
                    .as_array_mut()
                    .expect("accounts")
                    .push(serde_json::json!({
                        "id": other,
                        "host_id": "123e4567-e89b-42d3-a456-426614174000",
                        "client_id": "oaiapp_synthetic_other",
                        "subject": "synthetic-other-subject",
                        "chatgpt_account_id": null,
                        "keyring_slot": chatgpt_slot("oaiapp_synthetic_other", other),
                        "storage": "keyring",
                        "renewal_pending": false,
                        "signout_pending": false,
                        "disconnected": false,
                        "token": null
                    }));
            }
            account_root
                .record(RecordKind::ChatgptAccounts)
                .replace(&serde_json::to_vec(&index).expect("account index"))
                .expect("disconnected account index");
        }
        let mut child = crate::product::PtyLaunch::new(
            crate::product::Isolation::Fixture(temp.path()),
            SETUP_SHELL,
        )
        .session(&state, &workspace)
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", temp.path().join("missing-bus").display()),
        )
        .spawn();
        let mut input = child.child().stdin.take().expect("PTY input");
        let mut output = child.child().stdout.take().expect("PTY output");
        let flags = fcntl_getfl(&output).expect("stdout flags");
        fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking PTY output");
        let mut transcript = Vec::new();
        let mut answered = crate::session_picker::PtyResponses::default();
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"SCRIPT_INPUT_GATE",
        );
        input.write_all(b"go\r").expect("start product");
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"type a choice name: 1. API accounts or 2. Custom routes or 3. Role routing or 4. Roster profiles; empty Enter selects 1. API accounts\r\nInput:\r\n",
        );
        let text = String::from_utf8_lossy(&transcript);
        let shell_pid = text
            .lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix("SHELL_PID:"))
            .expect("shell PID")
            .parse::<u32>()
            .expect("shell PID number");
        let product_pid = product_child_of(shell_pid);
        let _product = ProductGuard::new(product_pid, state.clone());
        input.write_all(b"API accounts\r").expect("open accounts");
        acknowledge_linear_pages(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            (0, API_ACCOUNTS_PROMPT),
        );
        input
            .write_all(b"Manage ChatGPT account\r")
            .expect("manage ChatGPT account");
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            if disconnected && !other_saved {
                b"type a choice name: 1. Reconnect or 2. Connect new or 3. Inspect; empty Enter selects 1. Reconnect\r\nInput:\r\n"
            } else {
                b"type a choice name: 1. Use saved or 2. Reconnect or 3. Connect new or 4. Inspect; empty Enter selects 1. Use saved\r\nInput:\r\n"
            },
        );
        let chosen_at = transcript.len();
        input
            .write_all(format!("{choice}\r").as_bytes())
            .expect("choose selected account action");
        // Failed setup returns to the unconfigured composer; empty Ctrl+D then exits.
        crate::session_picker::pump_until(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            "failed setup notice missing",
            |transcript| {
                transcript[chosen_at..]
                    .split(|byte| *byte == b'\n')
                    .skip_while(|line| !line.starts_with(b"Notice: "))
                    .any(|line| line.starts_with(b"Input:"))
            },
        );
        input.write_all(b"\x04").expect("exit unconfigured chat");
        let result = wait_product(child.take());
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(result.status.success(), "unconfigured chat exit");
        assert_eq!(
            answered.cursor_queries, 0,
            "linear setup queried the cursor"
        );
        let text = String::from_utf8(transcript).expect("screen-reader UTF-8");
        if choice == "Reconnect" {
            assert!(text.contains("Saved ChatGPT keyring is unavailable"));
            assert!(text.contains("Unlock your desktop password store"));
            assert!(text.contains("Connect new, then Use private file (not encrypted)"));
            assert!(!text.contains("Choose Accept to continue"));
            if disconnected {
                assert!(text.contains("ChatGPT account disconnected"));
                assert!(!text.contains("type a choice name: 1. Use saved"));
            }
        } else if other_saved {
            assert!(text.contains("ChatGPT account disconnected"));
            assert!(text.contains("authorization is unavailable"));
            assert!(text.contains("unlock your desktop password store and retry"));
            assert!(!text.contains("no selected ChatGPT account"));
        } else {
            assert!(text.contains("Model catalog failed"));
            assert!(text.contains("saved ChatGPT account remains selected"));
        }
        assert!(!text.contains("new ChatGPT account was saved"));
        assert!(!text.contains("Waiting for ChatGPT"));
        assert!(!text.contains("Choose Accept to continue"));
        if other_saved {
            assert_failed_chatgpt_selection(&state, &workspace, other);
        } else if let Ok(root) = StateRoot::open_existing(&state) {
            let sessions = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("fixture runtime")
                .block_on(arany::list_sessions(root, workspace.clone()))
                .expect("Session list");
            assert!(sessions.is_empty(), "failed reuse created a Session");
        }
        assert!(!text.contains('\x1b'), "screen-reader output has escapes");
        let marker = |name: &str| {
            text.lines()
                .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
                .expect("terminal marker")
                .to_owned()
        };
        assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
        let index = StateRoot::open_existing(&crate::product::fixture_account(temp.path()))
            .expect("account index remains")
            .record(RecordKind::ChatgptAccounts)
            .read()
            .expect("account index read")
            .expect("selected account metadata");
        let index: serde_json::Value = serde_json::from_slice(&index).expect("account index JSON");
        assert_eq!(index["selected"], selected.to_string());
        assert_eq!(
            index["accounts"].as_array().map(Vec::len),
            Some(1 + usize::from(other_saved))
        );
        assert_eq!(
            index["accounts"][0]["disconnected"]
                .as_bool()
                .unwrap_or(false),
            disconnected
        );
    }
}

#[cfg(debug_assertions)]
fn assert_failed_chatgpt_selection(state: &Path, workspace: &Path, target_id: Uuid) {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("replay runtime")
        .block_on(async {
            let sessions = arany::list_sessions(
                StateRoot::open_existing(state).expect("setup StateRoot"),
                workspace.to_owned(),
            )
            .await
            .expect("setup Session list");
            assert_eq!(
                sessions.len(),
                1,
                "confirmed selection materializes a Session"
            );
            let store = Store::open_read_only(StateRoot::open_existing(state).unwrap()).unwrap();
            let events = store.load_session(sessions[0].id).await.unwrap();
            assert_eq!(
                events.len(),
                3,
                "only Session start and the failed edit pair"
            );
            assert!(matches!(
                events[0].event,
                arany::Event::SessionStarted { .. }
            ));
            let operation_id = match &events[1].event {
                arany::Event::ProviderSetupEditStarted {
                    operation_id,
                    target,
                    expected_digest,
                } => {
                    assert_eq!(target.kind, arany::ProviderSetupTargetKind::ChatGptAccount);
                    assert_eq!(target.identity, target_id.to_string());
                    assert_eq!(*expected_digest, None);
                    *operation_id
                }
                event => panic!("unexpected selection Event: {}", event.kind()),
            };
            assert!(matches!(
                &events[2].event,
                arany::Event::ProviderSetupEditFinished {
                    operation_id: finished,
                    disposition: arany::ProviderSetupDisposition::Failed,
                    resulting_digest: None,
                } if *finished == operation_id
            ));
            let view = SessionView::replay(sessions[0].id, &events)
                .expect("strict replay")
                .expect("setup Session");
            assert_eq!(view.defaults, SessionDefaults::default());
            assert!(view.runs.is_empty(), "failed selection creates no Run");
            assert_eq!(view.pending_provider_setup_edit, None);
            assert!(!view.provider_setup_uncertain);
            store.close().await.expect("close setup Store");
        });
}

#[cfg(debug_assertions)]
#[test]
fn saved_chatgpt_picker_lists_account_ids_and_cancels_cleanly() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let selected = prepare_selected_chatgpt_account(temp.path());
    let account_root = StateRoot::open_existing(&crate::product::fixture_account(temp.path()))
        .expect("account root");
    let bytes = account_root
        .record(RecordKind::ChatgptAccounts)
        .read()
        .expect("account index")
        .expect("selected account metadata");
    let mut index: serde_json::Value = serde_json::from_slice(&bytes).expect("account index JSON");
    let other = Uuid::now_v7();
    index["accounts"]
        .as_array_mut()
        .expect("accounts")
        .push(serde_json::json!({
            "id": other,
            "host_id": "123e4567-e89b-42d3-a456-426614174000",
            "client_id": "oaiapp_synthetic_other",
            "subject": "synthetic-other-subject",
            "chatgpt_account_id": null,
            "keyring_slot": chatgpt_slot("oaiapp_synthetic_other", other),
            "storage": "keyring",
            "renewal_pending": false,
            "signout_pending": false,
            "disconnected": false,
            "token": null
        }));
    account_root
        .record(RecordKind::ChatgptAccounts)
        .replace(&serde_json::to_vec(&index).expect("two accounts"))
        .expect("save account index");
    let mut child = crate::product::PtyLaunch::new(
        crate::product::Isolation::Fixture(temp.path()),
        SETUP_SHELL,
    )
    .session(&state, &workspace)
    .env(
        "DBUS_SESSION_BUS_ADDRESS",
        format!("unix:path={}", temp.path().join("missing-bus").display()),
    )
    .spawn();
    let mut input = child.child().stdin.take().expect("PTY input");
    let mut output = child.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking PTY output");
    let mut transcript = Vec::new();
    let mut answered = crate::session_picker::PtyResponses::default();
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"SCRIPT_INPUT_GATE",
    );
    input.write_all(b"go\r").expect("start setup");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"type a choice name: 1. API accounts or 2. Custom routes or 3. Role routing or 4. Roster profiles; empty Enter selects 1. API accounts\r\nInput:\r\n",
    );
    input.write_all(b"API accounts\r").expect("open accounts");
    acknowledge_linear_pages(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        (0, API_ACCOUNTS_PROMPT),
    );
    input
        .write_all(b"Manage ChatGPT account\r")
        .expect("manage ChatGPT account");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"type a choice name: 1. Use saved or 2. Reconnect or 3. Connect new or 4. Inspect; empty Enter selects 1. Use saved\r\nInput:\r\n",
    );
    input
        .write_all(b"Use saved\r")
        .expect("open saved accounts");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"* is current; choose account",
    );
    input.write_all(b"\x03").expect("cancel account choice");
    let result = wait_product(child.take());
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    assert!(result.status.success(), "cancelled picker failed");
    assert!(result.stderr.is_empty(), "script wrapper stderr");
    assert_eq!(
        answered.cursor_queries, 0,
        "screen-reader picker queried cursor"
    );
    let text = String::from_utf8(transcript).expect("screen-reader UTF-8");
    assert!(text.contains(&format!("1* {}", &selected.simple().to_string()[24..])));
    assert!(text.contains(&format!("2  {}", &other.simple().to_string()[24..])));
    assert!(!text.contains("synthetic-subject"));
    assert!(!text.contains("synthetic-other-subject"));
    assert!(!text.contains('\x1b'), "screen-reader output has escapes");
    let marker = |name: &str| {
        text.lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
            .expect("terminal marker")
            .to_owned()
    };
    assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
    assert!(!state.exists(), "cancelled picker created Session State");
    let updated = account_root
        .record(RecordKind::ChatgptAccounts)
        .read()
        .expect("account index")
        .expect("saved accounts");
    let updated: serde_json::Value = serde_json::from_slice(&updated).expect("index JSON");
    assert_eq!(updated["selected"], selected.to_string());
}

#[cfg(debug_assertions)]
#[test]
fn screen_reader_chatgpt_consent_defaults_to_back_before_browser_or_account_state() {
    for accept_file in [false, true] {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let state = temp.path().join("state");
        let account_root = crate::product::fixture_account(temp.path());
        let mut child = crate::product::PtyLaunch::new(
            crate::product::Isolation::Fixture(temp.path()),
            SETUP_SHELL,
        )
        .session(&state, &workspace)
        .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", temp.path().join("missing-bus").display()),
        )
        .spawn();
        let mut input = child.child().stdin.take().expect("PTY input");
        let mut output = child.child().stdout.take().expect("PTY output");
        let flags = fcntl_getfl(&output).expect("stdout flags");
        fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking PTY output");
        let mut transcript = Vec::new();
        let mut answered = crate::session_picker::PtyResponses::default();
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"SCRIPT_INPUT_GATE",
        );
        input.write_all(b"go\r").expect("start product");
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"type a choice name: 1. API accounts or 2. Custom routes or 3. Role routing or 4. Roster profiles; empty Enter selects 1. API accounts\r\nInput:\r\n",
        );
        let shell_pid = String::from_utf8_lossy(&transcript)
            .lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix("SHELL_PID:"))
            .expect("shell PID")
            .parse::<u32>()
            .expect("shell PID number");
        let _product = ProductGuard::new(product_child_of(shell_pid), state.clone());
        input.write_all(b"API accounts\r").expect("open accounts");
        acknowledge_linear_pages(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            (0, API_ACCOUNTS_PROMPT),
        );
        input
            .write_all(b"Manage ChatGPT account\r")
            .expect("manage ChatGPT account");
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"empty Enter selects 1. Cancel\r\nInput:\r\n",
        );
        if accept_file {
            let warning_at = transcript.len();
            input
                .write_all(b"Use private file\r")
                .expect("select explicit fallback");
            acknowledge_linear_pages(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                (
                    warning_at,
                    b"type a choice name: 1. Back or 2. Accept; empty Enter selects 1. Back\r\nInput:\r\n",
                ),
            );
            input
                .write_all(b"\r")
                .expect("default declines plan consent");
        } else {
            input
                .write_all(b"\r")
                .expect("default declines file storage");
        }
        let result = wait_product(child.take());
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(result.status.success(), "cancelled setup failed");
        assert_eq!(
            answered.cursor_queries, 0,
            "linear setup must not query the cursor"
        );
        assert!(!state.exists(), "cancelled setup created Session State");
        // The harness's Enter at folder consent is an explicit "read only" save, which creates the root.
        if let Ok(root) = StateRoot::open_existing(&account_root) {
            assert!(
                root.record(RecordKind::SavedAccount)
                    .read()
                    .unwrap()
                    .is_none()
                    && root
                        .record(RecordKind::ChatgptRegistration)
                        .read()
                        .unwrap()
                        .is_none()
                    && root
                        .record(RecordKind::ChatgptAccounts)
                        .read()
                        .unwrap()
                        .is_none(),
                "pre-consent setup created account State"
            );
        }
        let text = String::from_utf8(transcript).expect("screen-reader UTF-8");
        let reviewed = reviewed_setup_text(&text);
        assert!(text.contains("Password store unavailable. Unencrypted file"));
        assert_eq!(reviewed.contains("remote output-token limit"), accept_file);
        assert_eq!(
            reviewed.contains("private account file is not encrypted"),
            accept_file
        );
        assert_eq!(
            reviewed.contains("Choose Accept to continue, or Back to cancel."),
            accept_file
        );
        assert!(!text.contains('\x1b'), "screen-reader output has escapes");
        let marker = |name: &str| {
            text.lines()
                .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
                .expect("terminal marker")
                .to_owned()
        };
        assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
    }
}

#[cfg(all(target_os = "linux", not(debug_assertions)))]
#[test]
#[ignore = "native Linux Bubblewrap ChatGPT account-only cross-StateRoot gate"]
fn release_selected_chatgpt_account_starts_without_keyring_or_run() {
    let temp = tempfile::tempdir().expect("private test root");
    let private_home = temp.path().join("home");
    std::fs::create_dir_all(private_home.join("workspace")).expect("private Workspace");
    prepare_selected_chatgpt_account_at(&private_home.join(".local/state/arany"));
    let passwd_home = release_passwd_home();
    for name in ["state-one", "state-two"] {
        let view = release_bare_session(
            &private_home,
            &passwd_home,
            name,
            None,
            &["synthetic-subject"],
        )
        .expect("ChatGPT Session");
        assert!(view.contains("Provider: chatgpt\r\nModel: unset\r\n"));
    }
}

#[cfg(debug_assertions)]
#[test]
fn narrow_no_color_chatgpt_warning_pages_before_acceptance_and_restores_terminal() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let account_root = crate::product::fixture_account(temp.path());
    let mut child = crate::product::PtyLaunch::new(
        crate::product::Isolation::Fixture(temp.path()),
        r#"stty rows 8 cols 16; printf 'SHELL_PID:%s\n' "$$"; before=$(stty -g); printf 'TTY_BEFORE:%s\n' "$before"; "$ARANY_TEST_EXE" --no-color --setup --state-dir "$ARANY_TEST_STATE" --workspace "$ARANY_TEST_WORKSPACE"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' "$after"; exit "$exit_code""#,
    )
    .session(&state, &workspace)
    .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
    .env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={}", temp.path().join("missing-bus").display()))
    .term("xterm")
    .spawn();
    let mut input = child.child().stdin.take().expect("PTY input");
    let mut output = child.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking PTY output");
    let mut transcript = Vec::new();
    let mut answered = crate::session_picker::PtyResponses::default();
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Provider setup",
    );
    let root_frame = transcript
        .windows(b"Provider setup".len())
        .position(|part| part == b"Provider setup")
        .unwrap();
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        root_frame,
        b"\x1b[?25l",
    );
    let text = String::from_utf8_lossy(&transcript);
    let shell_pid = text
        .lines()
        .find_map(|line| line.trim_end_matches('\r').strip_prefix("SHELL_PID:"))
        .expect("shell PID")
        .parse::<u32>()
        .expect("shell PID number");
    let product_pid = product_child_of(shell_pid);
    let _product = ProductGuard::new(product_pid, state.clone());
    let accounts_at = transcript.len();
    input.write_all(b"\r").expect("open API accounts");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        accounts_at,
        b"API accounts",
    );
    let frame_start = transcript[accounts_at..]
        .windows(b"API accounts".len())
        .position(|part| part == b"API accounts")
        .unwrap()
        + accounts_at;
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        frame_start,
        b"\x1b[?25l",
    );
    input
        .write_all(b"\x1b[B\x1b[B\r")
        .expect("select ChatGPT account manager");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"\x1b[7;13Hfile",
    );
    input
        .write_all(b"\x1b[B\r")
        .expect("choose and confirm file fallback");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "ChatGPT plan co\u{2026}".as_bytes(),
    );
    assert!(!state.exists(), "pre-consent flow created a Session");
    let next_page = transcript.len();
    input.write_all(b"\r").expect("read next page");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        next_page,
        b"\x1b[5;6H2",
    );
    let ignored = transcript.len();
    input
        .write_all(b"Accept")
        .expect("attempt early acceptance");
    wait_for_after(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        ignored,
        b"Read every",
    );
    input.write_all(b"\x1b").expect("cancel before final page");
    let result = wait_product(child.take());
    let mut remaining = [0u8; 4096];
    loop {
        match output.read(&mut remaining) {
            Ok(0) | Err(_) => break,
            Ok(count) => transcript.extend_from_slice(&remaining[..count]),
        }
    }
    assert!(result.status.success(), "cancelled narrow setup failed");
    assert!(!state.exists(), "cancelled setup created a Session");
    assert!(!account_root.exists(), "warning created account State");
    let text = String::from_utf8_lossy(&transcript);
    let marker = |name: &str| {
        text.lines()
            .find_map(|line| {
                line.trim_end_matches('\r')
                    .split_once(name)
                    .map(|(_, value)| value)
            })
            .expect("terminal marker")
            .to_owned()
    };
    assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
}

#[cfg(debug_assertions)]
#[test]
fn unsafe_dbus_address_cannot_launch_a_credential_command() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let marker = temp.path().join("credential-command-marker");
    let address = format!("unixexec:path=/usr/bin/touch,argv1={}", marker.display());
    crate::isolation::remember_workspace_trust(
        &crate::product::fixture_account(temp.path()),
        &workspace,
        &workspace,
    );
    prepare_selected_chatgpt_account(temp.path());
    let mut child = crate::product::PtyLaunch::new(
        crate::product::Isolation::Fixture(temp.path()),
        "exec \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\"",
    )
    .session(&state, &workspace)
    .env("DBUS_SESSION_BUS_ADDRESS", address)
    .piped_stderr()
    .spawn();
    let _product = ProductGuard::new(product_child_of(child.child().id()), state.clone());
    let mut input = child.child().stdin.take().expect("PTY input");
    let mut output = child.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking PTY output");
    let mut transcript = Vec::new();
    let mut answered = crate::session_picker::PtyResponses::default();
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        b"Input:\r\n",
    );
    assert!(
        String::from_utf8_lossy(&transcript)
            .contains("unsafe or unsupported D-Bus session address"),
        "unsafe transport rejection missing: {}",
        tail(&transcript)
    );
    input.write_all(b"/exit\r").expect("exit unconfigured chat");
    child.child().stdout = Some(output);
    let result = wait_product(child.take());
    transcript.extend_from_slice(&result.stdout);
    assert!(
        result.status.success(),
        "unsafe transport startup exit: {:?}; stdout: {}; stderr: {}",
        result.status,
        tail(&transcript),
        tail(&result.stderr)
    );
    let text = String::from_utf8_lossy(&transcript);
    assert!(!marker.exists(), "credential command was not executed");
    // Rejected startup authorization keeps unconfigured chat; it saves no conversation.
    assert!(text.contains("No conversation saved."));
    if let Ok(root) = StateRoot::open_existing(&state) {
        let sessions = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("fixture runtime")
            .block_on(arany::list_sessions(root, workspace.clone()))
            .expect("Session list");
        assert!(sessions.is_empty(), "unsafe transport created a Session");
    }
}
