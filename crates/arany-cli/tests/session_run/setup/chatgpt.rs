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
        let (temp, workspace, state) = workspace_root();
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
        let mut pty = Pty::new(
            crate::product::PtyLaunch::new(
                crate::product::Isolation::Fixture(temp.path()),
                SETUP_SHELL,
            )
            .session(&state, &workspace)
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", temp.path().join("missing-bus").display()),
            )
            .spawn(),
        );
        pty.wait_for(b"SCRIPT_INPUT_GATE");
        pty.send(b"go\r");
        pty.wait_for(SETUP_MENU_PROMPT);
        pty.adopt_product(&state);
        pty.send(b"API accounts\r");
        pty.acknowledge_linear_pages((0, API_ACCOUNTS_PROMPT));
        pty.send(b"Manage ChatGPT account\r");
        pty.wait_for(
            if disconnected && !other_saved {
                b"type a choice name: 1. Reconnect or 2. Connect new or 3. Inspect; empty Enter selects 1. Reconnect\r\nInput:\r\n"
            } else {
                b"type a choice name: 1. Use saved or 2. Reconnect or 3. Connect new or 4. Inspect; empty Enter selects 1. Use saved\r\nInput:\r\n"
            },
        );
        let chosen_at = pty.send(format!("{choice}\r").as_bytes());
        // Failed setup returns to the unconfigured composer; empty Ctrl+D then exits.
        pty.pump_until("failed setup notice missing", |transcript| {
            transcript[chosen_at..]
                .split(|byte| *byte == b'\n')
                .skip_while(|line| !line.starts_with(b"Notice: "))
                .any(|line| line.starts_with(b"Input:"))
        });
        pty.send(b"\x04");
        let result = pty.finish();
        assert!(result.status.success(), "unconfigured chat exit");
        assert_eq!(
            pty.answered.cursor_queries, 0,
            "linear setup queried the cursor"
        );
        let text = String::from_utf8(pty.transcript).expect("screen-reader UTF-8");
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
        assert_tty_restored(&text);
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
    let (temp, workspace, state) = workspace_root();
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
    let mut pty = Pty::new(
        crate::product::PtyLaunch::new(
            crate::product::Isolation::Fixture(temp.path()),
            SETUP_SHELL,
        )
        .session(&state, &workspace)
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", temp.path().join("missing-bus").display()),
        )
        .spawn(),
    );
    pty.wait_for(b"SCRIPT_INPUT_GATE");
    pty.send(b"go\r");
    pty.wait_for(SETUP_MENU_PROMPT);
    pty.send(b"API accounts\r");
    pty.acknowledge_linear_pages((0, API_ACCOUNTS_PROMPT));
    pty.send(b"Manage ChatGPT account\r");
    pty.wait_for(
        b"type a choice name: 1. Use saved or 2. Reconnect or 3. Connect new or 4. Inspect; empty Enter selects 1. Use saved\r\nInput:\r\n",
    );
    pty.send(b"Use saved\r");
    pty.wait_for(b"* is current; choose account");
    pty.send(b"\x03");
    let result = pty.finish();
    assert!(result.status.success(), "cancelled picker failed");
    assert!(result.stderr.is_empty(), "script wrapper stderr");
    assert_eq!(
        pty.answered.cursor_queries, 0,
        "screen-reader picker queried cursor"
    );
    let text = String::from_utf8(pty.transcript).expect("screen-reader UTF-8");
    assert!(text.contains(&format!("1* {}", &selected.simple().to_string()[24..])));
    assert!(text.contains(&format!("2  {}", &other.simple().to_string()[24..])));
    assert!(!text.contains("synthetic-subject"));
    assert!(!text.contains("synthetic-other-subject"));
    assert!(!text.contains('\x1b'), "screen-reader output has escapes");
    assert_tty_restored(&text);
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
        let (temp, workspace, state) = workspace_root();
        let account_root = crate::product::fixture_account(temp.path());
        let mut pty = Pty::new(
            crate::product::PtyLaunch::new(
                crate::product::Isolation::Fixture(temp.path()),
                SETUP_SHELL,
            )
            .session(&state, &workspace)
            .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", temp.path().join("missing-bus").display()),
            )
            .spawn(),
        );
        pty.wait_for(b"SCRIPT_INPUT_GATE");
        pty.send(b"go\r");
        pty.wait_for(SETUP_MENU_PROMPT);
        pty.adopt_product(&state);
        pty.send(b"API accounts\r");
        pty.acknowledge_linear_pages((0, API_ACCOUNTS_PROMPT));
        pty.send(b"Manage ChatGPT account\r");
        pty.wait_for(b"empty Enter selects 1. Cancel\r\nInput:\r\n");
        if accept_file {
            let warning_at = pty.send(b"Use private file\r");
            pty.acknowledge_linear_pages(
                (
                    warning_at,
                    b"type a choice name: 1. Back or 2. Accept; empty Enter selects 1. Back\r\nInput:\r\n",
                ),
            );
            pty.send(b"\r");
        } else {
            pty.send(b"\r");
        }
        let result = pty.finish();
        assert!(result.status.success(), "cancelled setup failed");
        assert_eq!(
            pty.answered.cursor_queries, 0,
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
        let text = String::from_utf8(pty.transcript).expect("screen-reader UTF-8");
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
        assert_tty_restored(&text);
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
    let (temp, workspace, state) = workspace_root();
    let account_root = crate::product::fixture_account(temp.path());
    let mut pty = Pty::new(crate::product::PtyLaunch::new(
        crate::product::Isolation::Fixture(temp.path()),
        r#"stty rows 8 cols 16; printf 'SHELL_PID:%s\n' "$$"; before=$(stty -g); printf 'TTY_BEFORE:%s\n' "$before"; "$ARANY_TEST_EXE" --no-color --setup --state-dir "$ARANY_TEST_STATE" --workspace "$ARANY_TEST_WORKSPACE"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' "$after"; exit "$exit_code""#,
    )
    .session(&state, &workspace)
    .env("ARANY_TEST_ACCOUNT_ROOT", &account_root)
    .env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={}", temp.path().join("missing-bus").display()))
    .term("xterm")
    .spawn());
    pty.wait_for(b"Provider setup");
    let root_frame = pty
        .transcript
        .windows(b"Provider setup".len())
        .position(|part| part == b"Provider setup")
        .unwrap();
    pty.wait_for_after(root_frame, b"\x1b[?25l");
    pty.adopt_product(&state);
    let accounts_at = pty.send_and_wait(b"\r", b"API accounts");
    let frame_start = pty.transcript[accounts_at..]
        .windows(b"API accounts".len())
        .position(|part| part == b"API accounts")
        .unwrap()
        + accounts_at;
    pty.wait_for_after(frame_start, b"\x1b[?25l");
    pty.send(b"\x1b[B\x1b[B\r");
    pty.wait_for(b"\x1b[7;13Hfile");
    pty.send(b"\x1b[B\r");
    pty.wait_for("ChatGPT plan co\u{2026}".as_bytes());
    assert!(!state.exists(), "pre-consent flow created a Session");
    pty.send_and_wait(b"\r", b"\x1b[5;6H2");
    pty.send_and_wait(b"Accept", b"Read every");
    pty.send(b"\x1b");
    let result = wait_product(pty.child.take());
    let mut remaining = [0u8; 4096];
    loop {
        match pty.output.read(&mut remaining) {
            Ok(0) | Err(_) => break,
            Ok(count) => pty.transcript.extend_from_slice(&remaining[..count]),
        }
    }
    assert!(result.status.success(), "cancelled narrow setup failed");
    assert!(!state.exists(), "cancelled setup created a Session");
    assert!(!account_root.exists(), "warning created account State");
    let text = String::from_utf8_lossy(&pty.transcript);
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
    let (temp, workspace, state) = workspace_root();
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
    let mut pty = Pty::new(child);
    pty.wait_for(b"Input:\r\n");
    assert!(
        String::from_utf8_lossy(&pty.transcript)
            .contains("unsafe or unsupported D-Bus session address"),
        "unsafe transport rejection missing: {}",
        tail(&pty.transcript)
    );
    pty.send(b"/exit\r");
    pty.child.child().stdout = Some(pty.output);
    let result = wait_product(pty.child.take());
    pty.transcript.extend_from_slice(&result.stdout);
    assert!(
        result.status.success(),
        "unsafe transport startup exit: {:?}; stdout: {}; stderr: {}",
        result.status,
        tail(&pty.transcript),
        tail(&result.stderr)
    );
    let text = String::from_utf8_lossy(&pty.transcript);
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
