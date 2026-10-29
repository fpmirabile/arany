use super::*;
use crate::accounts::chatgpt::consent::RiskPrompt;
use arany::RecordKind;

fn credentials(client_id: &str, subject: &str) -> VerifiedCredentials {
    VerifiedCredentials {
        client_id: client_id.into(),
        host_id: Uuid::parse_str("123e4567-e89b-42d3-a456-426614174000").unwrap(),
        subject: subject.into(),
        id_token: "synthetic-id".into(),
        access_token: "synthetic-access".into(),
        refresh_token: "synthetic-refresh".into(),
        access_expires_at_unix: 1_800_000_000,
    }
}

fn accepted(credentials: &VerifiedCredentials) -> ConsentReceipt {
    RiskPrompt::new(AccountStorage::PrivateFile)
        .accept("Accept")
        .unwrap()
        .bind(credentials)
        .unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn disappearing_keyring_saves_block_new_and_existing_setup_sessions() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    const ROOT: &str = "ARANY_TEST_SAVE_ROOT";
    const SCRIPT: &str = "#!/bin/sh\n/bin/cat > \"$HOME/accepted\"\nkill -TERM $$\n";
    if let Some(root) = std::env::var_os(ROOT) {
        let root = PathBuf::from(root);
        assert_eq!(
            std::env::var_os("ARANY_TEST_EXE"),
            Some(root.join("helper").into())
        );
        assert_eq!(
            std::fs::read_to_string(root.join("helper")).unwrap(),
            SCRIPT
        );
        assert_eq!(std::env::var_os("HOME"), Some(root.clone().into()));
        assert_eq!(std::env::temp_dir(), root);
        for existing in [false, true] {
            assert_disappearing_keyring_save(&root, existing).await;
            assert!(std::fs::metadata(root.join("accepted")).unwrap().len() > 0);
        }
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let helper = root.join("helper");
    std::fs::write(&helper, SCRIPT).unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let thread = std::thread::current();
    let test = thread.name().unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test, "--test-threads=1", "--nocapture"])
        .env_clear()
        .env(ROOT, &root)
        .env("HOME", &root)
        .env("TMPDIR", &root)
        .env("ARANY_TEST_EXE", &helper)
        .current_dir(&root);
    let output = crate::test_process::file_output(&mut command, None);
    assert!(output.status.success(), "isolated save lifecycle failed");
    crate::test_process::assert_single_test_completed(&output.stdout, test);
}

#[cfg(unix)]
async fn assert_disappearing_keyring_save(root: &Path, existing: bool) {
    let workspace = root.join(if existing {
        "existing-workspace"
    } else {
        "new-workspace"
    });
    std::fs::create_dir(&workspace).unwrap();
    let state = StateRoot::admit(&root.join(if existing {
        "existing-state"
    } else {
        "new-state"
    }))
    .unwrap();
    let credentials = credentials("oaiapp_disappearing_helper", "synthetic-subject");
    let consent = RiskPrompt::new(AccountStorage::Keyring)
        .accept("Accept")
        .unwrap()
        .bind(&credentials)
        .unwrap();
    let selected = existing.then(|| {
        save_keyring_at_with(
            &state,
            &workspace,
            credentials.clone(),
            consent.clone(),
            None,
            |_, _| Ok(()),
        )
        .unwrap()
    });
    crate::cli::attached::assert_uncertain_publication(
        crate::cli::attached::SetupPublication::ChatGpt,
        || {
            let error =
                save_keyring_at(&state, &workspace, credentials, consent, selected).unwrap_err();
            assert!(matches!(
                error,
                AuthorizationError::CredentialStore(
                    crate::accounts::credentials::CredentialError::WriteOutcomeUnknown
                )
            ));
            let index = AccountIndex::read(&state).unwrap();
            assert_eq!(index.selected, selected);
            if existing {
                assert!(index.accounts[0].renewal_pending);
            }
            error
        },
    )
    .await;
}

#[tokio::test]
async fn chatgpt_partial_publication_faults_block_setup_continuation() {
    enum Permission {
        Enabled,
        Disabled,
    }
    for (storage, permission) in [
        (AccountStorage::Keyring, Permission::Enabled),
        (AccountStorage::Keyring, Permission::Disabled),
        (AccountStorage::PrivateFile, Permission::Enabled),
        (AccountStorage::PrivateFile, Permission::Disabled),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = StateRoot::admit(&temp.path().join("state")).unwrap();
        let credentials = credentials("oaiapp_partial_save", "synthetic-subject");
        let registration = serde_json::json!({
            "schema": 1, "host_id": credentials.host_id,
            "issued_client_id": credentials.client_id,
        });
        state
            .record(RecordKind::ChatgptRegistration)
            .replace(&serde_json::to_vec(&registration).unwrap())
            .unwrap();
        arany::FaultPlan::fail_record_publication(
            state.path().to_owned(),
            "chatgpt-registration.json",
        );
        crate::cli::attached::assert_uncertain_publication(
            crate::cli::attached::SetupPublication::ChatGpt,
            || {
                let result = if matches!(permission, Permission::Disabled) {
                    save_without_plan_permission_at(
                        &state,
                        &workspace,
                        VerifiedIdentity {
                            client_id: credentials.client_id.clone(),
                            host_id: credentials.host_id,
                            subject: credentials.subject.clone(),
                        },
                        storage,
                        None,
                        |_| panic!("new disabled account has no keyring item"),
                    )
                    .map(|outcome| outcome.id)
                } else {
                    let consent = RiskPrompt::new(storage)
                        .accept("Accept")
                        .unwrap()
                        .bind(&credentials)
                        .unwrap();
                    match storage {
                        AccountStorage::PrivateFile => {
                            save_private_file_at(&state, &workspace, credentials, consent, None)
                        }
                        AccountStorage::Keyring => save_keyring_at_with(
                            &state,
                            &workspace,
                            credentials,
                            consent,
                            None,
                            |_, _| Ok(()),
                        ),
                    }
                };
                assert_eq!(AccountIndex::read(&state).unwrap().accounts.len(), 1);
                let error = result.unwrap_err();
                assert!(matches!(
                    error,
                    AuthorizationError::CredentialStore(
                        crate::accounts::credentials::CredentialError::WriteOutcomeUnknown
                    )
                ));
                error
            },
        )
        .await;
    }
}

#[tokio::test]
async fn chatgpt_keyring_partial_save_blocks_setup_continuation() {
    #[derive(Clone, Copy)]
    enum Fault {
        NewIndex,
        RenewalIndex,
        RenewalWrite,
    }
    for fault in [Fault::NewIndex, Fault::RenewalIndex, Fault::RenewalWrite] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = StateRoot::admit(&temp.path().join("state")).unwrap();
        let credentials = credentials("oaiapp_partial_keyring", "synthetic-subject");
        let consent = RiskPrompt::new(AccountStorage::Keyring)
            .accept("Accept")
            .unwrap()
            .bind(&credentials)
            .unwrap();
        let selected = if matches!(fault, Fault::NewIndex) {
            None
        } else {
            Some(
                save_keyring_at_with(
                    &state,
                    &workspace,
                    credentials.clone(),
                    consent.clone(),
                    None,
                    |_, _| Ok(()),
                )
                .unwrap(),
            )
        };
        crate::cli::attached::assert_uncertain_publication(
            crate::cli::attached::SetupPublication::ChatGpt,
            || {
                let error = save_keyring_at_with(
                    &state,
                    &workspace,
                    credentials,
                    consent,
                    selected,
                    |_, _| {
                        if matches!(fault, Fault::RenewalWrite) {
                            Err(AuthorizationError::Unavailable)
                        } else {
                            arany::FaultPlan::fail_record_publication(
                                state.path().to_owned(),
                                "chatgpt-accounts.json",
                            );
                            Ok(())
                        }
                    },
                )
                .unwrap_err();
                let index = AccountIndex::read(&state).unwrap();
                assert_eq!(index.accounts.len(), usize::from(selected.is_some()));
                if let Some(entry) = index.accounts.first() {
                    assert!(entry.renewal_pending);
                }
                error
            },
        )
        .await;
    }
}

#[test]
fn chatgpt_index_publication_fault_retains_uncertainty() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let credentials = credentials("oaiapp_publication", "synthetic-subject");
    let consent = accepted(&credentials);
    arany::FaultPlan::fail_record_sync(state.path().to_owned(), "chatgpt-accounts.json");
    assert!(matches!(
        save_private_file_at(&state, &workspace, credentials, consent, None),
        Err(AuthorizationError::CredentialStore(
            crate::accounts::credentials::CredentialError::WriteOutcomeUnknown
        ))
    ));
    let reopened = StateRoot::open_existing(state.path()).unwrap();
    assert_eq!(AccountIndex::read(&reopened).unwrap().accounts.len(), 1);
}

#[test]
fn sign_out_blocks_use_clears_file_token_and_keeps_registration() {
    for confirmed in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = StateRoot::admit(&temp.path().join("state")).unwrap();
        let credentials = credentials("oaiapp_signout", "subject-one");
        let id = save_private_file_at(
            &state,
            &workspace,
            credentials.clone(),
            accepted(&credentials),
            None,
        )
        .unwrap();
        let selected = refresh_selected_with(&state, &workspace, id, 1_799_000_000, |_| {
            panic!("fresh token must not rotate")
        })
        .unwrap();
        record_model_check_at(
            &state,
            &workspace,
            &selected,
            "model-one",
            Effort::Medium,
            None,
            1_799_999_000,
        )
        .unwrap();
        let outcome = sign_out_at_with(
            &state,
            &workspace,
            |_| panic!("file account must not read keyring"),
            |saved| {
                assert_eq!(saved.refresh_token, "synthetic-refresh");
                let pending = AccountIndex::read(&state).unwrap();
                assert!(pending.accounts[0].signout_pending);
                assert!(pending.model_checks.is_empty());
                assert!(matches!(
                    selected_id_at(&state),
                    Err(AuthorizationError::NoSelectedAccount)
                ));
                if confirmed {
                    Ok(())
                } else {
                    Err(AuthorizationError::Unavailable)
                }
            },
            |_| panic!("file account must not delete keyring"),
        )
        .unwrap();
        assert_eq!(outcome.remote_confirmed, confirmed);
        assert!(outcome.local_cleared);
        let reopened = StateRoot::open_existing(&temp.path().join("state")).unwrap();
        let index = AccountIndex::read(&reopened).unwrap();
        assert_eq!(index.selected, Some(id));
        assert!(index.accounts[0].disconnected);
        assert!(!index.accounts[0].signout_pending);
        assert!(index.accounts[0].token.is_none());
        assert!(index.model_checks.is_empty());
        assert_eq!(selected_registration_at(&reopened).unwrap(), (id, false));
        assert_eq!(
            selected_reauthorization_target_at(&reopened, id)
                .unwrap()
                .client_id,
            "oaiapp_signout"
        );
        assert!(matches!(
            load_selected_private_file_at(&reopened, id),
            Err(AuthorizationError::NoSelectedAccount)
        ));
        assert!(matches!(
            admitted_model_at(
                &reopened,
                &workspace,
                &selected,
                RunModel {
                    slug: "model-one",
                    effort: Effort::Medium,
                    now: 1_799_999_010,
                },
                Ok(&[])
            ),
            Err(AuthorizationError::NoSelectedAccount)
        ));
        assert_eq!(
            save_private_file_at(
                &reopened,
                &workspace,
                credentials.clone(),
                accepted(&credentials),
                Some(id),
            )
            .unwrap(),
            id
        );
        assert_eq!(selected_id_at(&reopened).unwrap(), id);
        assert!(!AccountIndex::read(&reopened).unwrap().accounts[0].disconnected);
    }
}

#[test]
fn timed_out_chatgpt_keyring_deletion_blocks_account_until_cleanup_succeeds() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let credentials = credentials("oaiapp_keyring_signout", "subject-one");
    let consent = RiskPrompt::new(AccountStorage::Keyring)
        .accept("Accept")
        .unwrap()
        .bind(&credentials)
        .unwrap();
    let id = save_keyring_at_with(
        &state,
        &workspace,
        credentials.clone(),
        consent.clone(),
        None,
        |_, _| Ok(()),
    )
    .unwrap();
    let token = TokenRecord {
        schema: TOKEN_RECORD_SCHEMA,
        slot: Some(account_keyring_slot("oaiapp_keyring_signout", id)),
        credentials,
        consent,
    };
    let expected_index = state
        .record(RecordKind::ChatgptAccounts)
        .read()
        .unwrap()
        .unwrap();
    let outcome = sign_out_target_at_with(
        &state,
        &workspace,
        SignOutTarget {
            account_id: Some(id),
            expected_index: Some(&expected_index),
        },
        SignOutActions {
            read_keyring: |entry: &AccountEntry| {
                assert_eq!(entry.client_id, token.credentials.client_id);
                assert_eq!(entry.subject, token.credentials.subject);
                assert_eq!(entry.host_id, token.credentials.host_id);
                Ok(token.clone())
            },
            revoke: None::<fn(&VerifiedCredentials) -> Result<(), AuthorizationError>>,
            delete_keyring: |slot: &str| {
                assert_eq!(slot, account_keyring_slot("oaiapp_keyring_signout", id));
                Err(AuthorizationError::Unavailable)
            },
        },
    );
    assert!(matches!(
        outcome,
        Err(AuthorizationError::SignOutStorageUncertain)
    ));
    let index = AccountIndex::read(&state).unwrap();
    assert!(index.accounts[0].disconnected);
    assert!(index.model_checks.is_empty());
    assert!(matches!(
        load_selected_keyring_at(&state, id),
        Err(AuthorizationError::NoSelectedAccount)
    ));
    let cleanup = sign_out_target_at_with(
        &state,
        &workspace,
        SignOutTarget {
            account_id: Some(id),
            expected_index: None,
        },
        SignOutActions {
            read_keyring: |_: &AccountEntry| panic!("disconnected account must not load token"),
            revoke: None::<fn(&VerifiedCredentials) -> Result<(), AuthorizationError>>,
            delete_keyring: |_: &str| Ok(()),
        },
    )
    .unwrap();
    assert!(!cleanup.remote_confirmed);
    assert!(cleanup.local_cleared);
    for confirmed in [true, false] {
        assert_eq!(
            save_keyring_at_with(
                &state,
                &workspace,
                token.credentials.clone(),
                token.consent.clone(),
                Some(id),
                |_, _| Ok(()),
            )
            .unwrap(),
            id
        );
        let outcome = save_without_plan_permission_at(
            &state,
            &workspace,
            VerifiedIdentity {
                client_id: token.credentials.client_id.clone(),
                host_id: token.credentials.host_id,
                subject: token.credentials.subject.clone(),
            },
            AccountStorage::Keyring,
            Some(id),
            |slot| {
                assert_eq!(slot, account_keyring_slot("oaiapp_keyring_signout", id));
                let blocked = AccountIndex::read(&state).unwrap();
                assert!(blocked.accounts[0].plan_permission_missing);
                assert!(blocked.accounts[0].disconnected);
                assert!(blocked.accounts[0].token.is_none());
                assert!(matches!(
                    load_selected_keyring_at(&state, id),
                    Err(AuthorizationError::PermissionMissing)
                ));
                if confirmed {
                    Ok(())
                } else {
                    Err(AuthorizationError::Unavailable)
                }
            },
        )
        .unwrap();
        assert_eq!(outcome.id, id);
        assert_eq!(outcome.local_cleared, confirmed);
        assert!(matches!(
            load_selected_keyring_at(&state, id),
            Err(AuthorizationError::PermissionMissing)
        ));
        assert_eq!(selected_registration_at(&state).unwrap(), (id, false));
        assert!(
            selected_reauthorization_target_at(&state, id)
                .unwrap()
                .plan_permission_missing
        );
    }
    assert_eq!(
        save_keyring_at_with(
            &state,
            &workspace,
            token.credentials.clone(),
            token.consent.clone(),
            Some(id),
            |_, _| {
                assert!(matches!(
                    load_selected_keyring_at(&state, id),
                    Err(AuthorizationError::PermissionMissing)
                ));
                Ok(())
            },
        )
        .unwrap(),
        id
    );
    let enabled = AccountIndex::read(&state).unwrap();
    assert!(!enabled.accounts[0].plan_permission_missing);
    assert!(!enabled.accounts[0].disconnected);
    assert!(!enabled.accounts[0].renewal_pending);
    assert_eq!(selected_registration_at(&state).unwrap(), (id, true));
}

#[test]
fn unreadable_keyring_still_disconnects_before_cleanup() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let credentials = credentials("oaiapp_locked_signout", "subject-one");
    let consent = RiskPrompt::new(AccountStorage::Keyring)
        .accept("Accept")
        .unwrap()
        .bind(&credentials)
        .unwrap();
    let id = save_keyring_at_with(
        &state,
        &workspace,
        credentials,
        consent,
        None,
        |_, _| Ok(()),
    )
    .unwrap();
    let mut index = AccountIndex::read(&state).unwrap();
    index.accounts[0].renewal_pending = true;
    index.write(&state).unwrap();

    let outcome = sign_out_at_with(
        &state,
        &workspace,
        |_| {
            let pending = AccountIndex::read(&state).unwrap();
            assert!(pending.accounts[0].signout_pending);
            assert!(!pending.accounts[0].renewal_pending);
            assert!(matches!(
                selected_id_at(&state),
                Err(AuthorizationError::NoSelectedAccount)
            ));
            Err(AuthorizationError::Unavailable)
        },
        |_| panic!("unreadable token cannot be revoked remotely"),
        |_| Ok(()),
    )
    .unwrap();
    assert!(!outcome.remote_confirmed);
    assert!(outcome.local_cleared);
    let index = AccountIndex::read(&state).unwrap();
    assert_eq!(index.selected, Some(id));
    assert!(index.accounts[0].disconnected);
    assert!(!index.accounts[0].renewal_pending);
}

#[test]
fn terminal_refresh_error_clears_private_token_and_keeps_registration() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let credentials = credentials("oaiapp_unusable_refresh", "subject-one");
    let id = save_private_file_at(
        &state,
        &workspace,
        credentials.clone(),
        accepted(&credentials),
        None,
    )
    .unwrap();
    let selected = refresh_selected_with(&state, &workspace, id, 1_799_000_000, |_| {
        panic!("fresh token must not rotate")
    })
    .unwrap();
    record_model_check_at(
        &state,
        &workspace,
        &selected,
        "model-one",
        Effort::Medium,
        None,
        1_799_999_000,
    )
    .unwrap();

    assert!(matches!(
        refresh_selected_with(&state, &workspace, id, 1_799_999_900, |saved| {
            assert_eq!(saved.refresh_token, "synthetic-refresh");
            assert!(AccountIndex::read(&state).unwrap().accounts[0].renewal_pending);
            Err(AuthorizationError::RefreshTokenUnusable)
        }),
        Err(AuthorizationError::RefreshTokenUnusable)
    ));
    let reopened = StateRoot::open_existing(state.path()).unwrap();
    let index = AccountIndex::read(&reopened).unwrap();
    assert_eq!(index.selected, Some(id));
    assert!(index.accounts[0].disconnected);
    assert!(!index.accounts[0].renewal_pending);
    assert!(index.accounts[0].token.is_none());
    assert!(index.model_checks.is_empty());
    assert_eq!(selected_registration_at(&reopened).unwrap(), (id, false));
    assert!(matches!(
        load_selected_private_file_at(&reopened, id),
        Err(AuthorizationError::NoSelectedAccount)
    ));
    let record = reopened
        .record(RecordKind::ChatgptAccounts)
        .read()
        .unwrap()
        .unwrap();
    assert!(
        !record
            .windows(b"synthetic-refresh".len())
            .any(|part| part == b"synthetic-refresh")
    );
    assert_eq!(
        save_private_file_at(
            &reopened,
            &workspace,
            credentials.clone(),
            accepted(&credentials),
            Some(id),
        )
        .unwrap(),
        id
    );
}

#[test]
fn terminal_refresh_keyring_cleanup_blocks_use_before_delete_result() {
    let temp = tempfile::tempdir().unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let credentials = credentials("oaiapp_terminal_keyring", "subject-one");
    let id = Uuid::now_v7();
    AccountIndex {
        schema: CHATGPT_ACCOUNTS.current as u8,
        selected: Some(id),
        accounts: vec![AccountEntry {
            id,
            host_id: credentials.host_id,
            client_id: credentials.client_id,
            subject: credentials.subject,
            chatgpt_account_id: None,
            keyring_slot: Some(account_keyring_slot("oaiapp_terminal_keyring", id)),
            storage: AccountStorage::Keyring,
            renewal_pending: true,
            signout_pending: false,
            disconnected: false,
            plan_permission_missing: false,
            token: None,
        }],
        model_checks: vec![ModelCheck {
            check_id: CheckId::new(),
            account_id: id,
            fingerprint: [7; 32],
            checked_at_sec: 1_799_999_000,
            expires_at_sec: 1_799_999_000 + MODEL_CHECK_AGE_SECONDS,
            context_window_tokens: None,
        }],
    }
    .write(&state)
    .unwrap();
    let mut index = AccountIndex::read(&state).unwrap();
    assert!(matches!(
        disconnect_unusable_refresh_with(&state, &mut index, 0, |slot| {
            assert_eq!(slot, account_keyring_slot("oaiapp_terminal_keyring", id));
            let blocked = AccountIndex::read(&state).unwrap();
            assert!(blocked.accounts[0].disconnected);
            assert!(blocked.model_checks.is_empty());
            assert!(matches!(
                load_selected_keyring_at(&state, id),
                Err(AuthorizationError::NoSelectedAccount)
            ));
            Err(AuthorizationError::Unavailable)
        }),
        Err(AuthorizationError::RefreshCleanupUncertain)
    ));
    assert!(AccountIndex::read(&state).unwrap().accounts[0].disconnected);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires a native unlocked Linux Secret Service"]
fn native_keyring_round_trips_and_replaces_a_synthetic_chatgpt_account() {
    use std::process::Command;

    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Ok(entry) = keyring::Entry::new(crate::accounts::credentials::SERVICE, &self.0) {
                let _ = entry.delete_credential();
            }
        }
    }

    fn product_helper(
        executable: &std::ffi::OsStr,
        operation: &str,
        slot: &str,
        input: Option<&[u8]>,
    ) -> std::process::Output {
        let mut command = Command::new("/usr/bin/timeout");
        command
            .args(["-k", "1s", "8s"])
            .arg(executable)
            .args(["--internal-credential-helper", operation, slot])
            .env_clear()
            .current_dir("/");
        for name in [
            "DBUS_SESSION_BUS_ADDRESS",
            "XDG_RUNTIME_DIR",
            "HOME",
            "USER",
            "LOGNAME",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let output = crate::test_process::file_output(&mut command, input);
        assert!(output.stdout.len() <= MAX_TOKEN_RECORD_BYTES);
        assert!(output.stderr.is_empty(), "product helper emitted stderr");
        output
    }

    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state = StateRoot::admit(&temp.path().join("state")).expect("private account state");
    let client_id = format!("oaiapp_native_{}", Uuid::now_v7().simple());
    let executable = std::env::var_os("ARANY_TEST_EXE").expect("set ARANY_TEST_EXE to built arany");
    let first = credentials(&client_id, "synthetic-subject");
    let consent = RiskPrompt::new(AccountStorage::Keyring)
        .accept("Accept")
        .expect("test risk acceptance")
        .bind(&first)
        .expect("test consent");
    let id = save_keyring_at(&state, &workspace, first.clone(), consent.clone(), None)
        .expect("save synthetic subscription record");
    let slot = account_keyring_slot(&client_id, id);
    let _cleanup = Cleanup(slot.clone());
    let record = serde_json::to_vec(&TokenRecord {
        schema: TOKEN_RECORD_SCHEMA,
        slot: Some(slot.clone()),
        credentials: first.clone(),
        consent,
    })
    .expect("synthetic token record");
    let read = product_helper(&executable, "read", &slot, None);
    assert!(read.status.success() && read.stdout == record);
    let rejected = product_helper(&executable, "write", &slot, Some(b"invalid record"));
    assert!(!rejected.status.success() && rejected.stdout.is_empty());
    let read = product_helper(&executable, "read", &slot, None);
    assert!(read.status.success() && read.stdout == record);
    let loaded = load_selected_keyring_at(&state, id).expect("load saved subscription record");
    assert!(loaded.access_token == first.access_token);
    assert!(loaded.refresh_token == first.refresh_token);

    let mut replacement = first;
    replacement.access_token = "synthetic-replacement-access".into();
    replacement.refresh_token = "synthetic-replacement-refresh".into();
    let consent = RiskPrompt::new(AccountStorage::Keyring)
        .accept("Accept")
        .expect("test risk acceptance")
        .bind(&replacement)
        .expect("replacement consent");
    let replacement_record = serde_json::to_vec(&TokenRecord {
        schema: TOKEN_RECORD_SCHEMA,
        slot: Some(slot.clone()),
        credentials: replacement.clone(),
        consent: consent.clone(),
    })
    .expect("synthetic replacement record");
    assert_eq!(
        save_keyring_at(&state, &workspace, replacement.clone(), consent, Some(id))
            .expect("replace synthetic subscription record"),
        id
    );
    let read = product_helper(&executable, "read", &slot, None);
    assert!(read.status.success() && read.stdout == replacement_record);
    let loaded = load_selected_keyring_at(&state, id).expect("load replacement record");
    assert!(loaded.access_token == replacement.access_token);
    assert!(loaded.refresh_token == replacement.refresh_token);
    let renewed = refresh_selected_with(&state, &workspace, id, 1_799_999_900, |previous| {
        assert_eq!(previous.refresh_token, "synthetic-replacement-refresh");
        let mut next = previous.clone();
        next.access_token = "synthetic-rotated-access".into();
        next.refresh_token = "synthetic-rotated-refresh".into();
        next.access_expires_at_unix = 1_800_003_600;
        Ok(next)
    })
    .expect("rotate selected synthetic keyring token");
    assert_eq!(renewed.id, id);
    assert_eq!(
        renewed.credentials.refresh_token,
        "synthetic-rotated-refresh"
    );
    assert!(!AccountIndex::read(&state).unwrap().accounts[0].renewal_pending);
    let read = product_helper(&executable, "read", &slot, None);
    assert!(read.status.success() && read.stderr.is_empty());
    let stored: TokenRecord = serde_json::from_slice(&read.stdout).expect("rotated token record");
    assert_eq!(stored.credentials.access_token, "synthetic-rotated-access");
    assert_eq!(
        stored.credentials.refresh_token,
        "synthetic-rotated-refresh"
    );
    assert_eq!(
        load_selected_keyring_at(&state, id)
            .expect("load rotated keyring token")
            .refresh_token,
        "synthetic-rotated-refresh"
    );
    assert!(matches!(
        load_selected_keyring_at(&state, Uuid::now_v7()),
        Err(AuthorizationError::InvalidIdentity)
    ));
    delete_chatgpt_keyring_record(&slot)
        .expect("delete through the production credential supervisor");
    assert!(
        read_chatgpt_keyring_record(&slot)
            .expect("verify synthetic credential deletion")
            .is_none()
    );
}

fn keyring_consent(credentials: &VerifiedCredentials) -> ConsentReceipt {
    RiskPrompt::new(AccountStorage::Keyring)
        .accept("Accept")
        .unwrap()
        .bind(credentials)
        .unwrap()
}

fn token_record_json(
    schema: u8,
    slot: Option<&str>,
    credentials: &VerifiedCredentials,
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "schema": schema,
        "credentials": credentials,
        "consent": keyring_consent(credentials),
    });
    if let Some(slot) = slot {
        value["slot"] = slot.into();
    }
    value
}

fn keyring_entry(id: Uuid, credentials: &VerifiedCredentials, slot: String) -> AccountEntry {
    AccountEntry {
        id,
        host_id: credentials.host_id,
        client_id: credentials.client_id.clone(),
        subject: credentials.subject.clone(),
        chatgpt_account_id: None,
        keyring_slot: Some(slot),
        storage: AccountStorage::Keyring,
        renewal_pending: false,
        signout_pending: false,
        disconnected: false,
        plan_permission_missing: false,
        token: None,
    }
}

fn to_version_two_shape(value: &mut serde_json::Value) {
    for entry in value["accounts"].as_array_mut().unwrap() {
        let entry = entry.as_object_mut().unwrap();
        entry.remove("keyring_slot");
        entry.remove("chatgpt_account_id");
        if let Some(token) = entry.get_mut("token").and_then(|t| t.as_object_mut()) {
            token.insert("schema".into(), LEGACY_TOKEN_RECORD_SCHEMA.into());
            token.remove("slot");
        }
    }
}

fn version_two_value(index: &AccountIndex) -> serde_json::Value {
    let mut value = serde_json::to_value(index).unwrap();
    value["schema"] = 2.into();
    to_version_two_shape(&mut value);
    value
}

#[test]
fn keyring_slot_and_consent_bind_token_to_its_record() {
    let id = Uuid::now_v7();
    let creds = credentials("oaiapp_first", "subject-one");
    let slot = account_keyring_slot("oaiapp_first", id);
    let value = token_record_json(TOKEN_RECORD_SCHEMA, Some(&slot), &creds);
    let record = serde_json::to_vec(&value).unwrap();
    assert!(valid_keyring_record(&slot, &record));
    assert!(valid_written_keyring_record(&slot, &record));
    let foreign = account_keyring_slot("oaiapp_first", Uuid::now_v7());
    assert!(!valid_keyring_record(&foreign, &record));
    assert!(!valid_keyring_record(
        &legacy_keyring_slot("oaiapp_first"),
        &record
    ));
    assert!(!valid_keyring_record(
        &slot,
        &vec![b'x'; MAX_TOKEN_RECORD_BYTES + 1]
    ));
    assert!(!valid_written_keyring_record(
        &slot,
        &vec![b'x'; MAX_TOKEN_RECORD_BYTES + 1]
    ));
    let mut changed = value.clone();
    changed["credentials"]["subject"] = "other-subject".into();
    assert!(!valid_keyring_record(
        &slot,
        &serde_json::to_vec(&changed).unwrap()
    ));
    let mut unslotted = value;
    unslotted["slot"] = serde_json::Value::Null;
    assert!(!valid_keyring_record(
        &slot,
        &serde_json::to_vec(&unslotted).unwrap()
    ));
}

#[test]
fn version_one_keyring_records_upgrade_in_memory_at_their_client_derived_slot() {
    let creds = credentials("oaiapp_legacy", "subject-one");
    let legacy_slot = legacy_keyring_slot("oaiapp_legacy");
    let mut value = token_record_json(LEGACY_TOKEN_RECORD_SCHEMA, None, &creds);
    let record = serde_json::to_vec(&value).unwrap();
    assert!(valid_keyring_record(&legacy_slot, &record));
    assert!(!valid_written_keyring_record(&legacy_slot, &record));
    let moved = account_keyring_slot("oaiapp_legacy", Uuid::now_v7());
    assert!(!valid_keyring_record(&moved, &record));
    let upgraded = decode_keyring_record(&record).unwrap();
    assert_eq!(upgraded.schema, TOKEN_RECORD_SCHEMA);
    assert_eq!(upgraded.slot.as_deref(), Some(legacy_slot.as_str()));
    value["slot"] = legacy_slot.into();
    assert!(decode_keyring_record(&serde_json::to_vec(&value).unwrap()).is_none());
}

#[test]
fn keyring_read_admits_only_the_record_written_at_the_entry_slot() {
    let id = Uuid::now_v7();
    let creds = credentials("oaiapp_admit", "subject-one");
    let slot = account_keyring_slot("oaiapp_admit", id);
    let entry = keyring_entry(id, &creds, slot.clone());
    let bytes = |schema: u8, slot: Option<&str>, credentials: &VerifiedCredentials| {
        serde_json::to_vec(&token_record_json(schema, slot, credentials)).unwrap()
    };
    let admitted =
        admit_keyring_record(&entry, &bytes(TOKEN_RECORD_SCHEMA, Some(&slot), &creds)).unwrap();
    assert_eq!(admitted.slot.as_deref(), Some(slot.as_str()));
    let foreign = account_keyring_slot("oaiapp_admit", Uuid::now_v7());
    for record in [
        bytes(TOKEN_RECORD_SCHEMA, Some(&foreign), &creds),
        bytes(TOKEN_RECORD_SCHEMA, None, &creds),
        bytes(LEGACY_TOKEN_RECORD_SCHEMA, None, &creds),
        bytes(
            TOKEN_RECORD_SCHEMA,
            Some(&slot),
            &credentials("oaiapp_other", "subject-one"),
        ),
        bytes(
            TOKEN_RECORD_SCHEMA,
            Some(&slot),
            &credentials("oaiapp_admit", "subject-two"),
        ),
    ] {
        assert!(matches!(
            admit_keyring_record(&entry, &record),
            Err(AuthorizationError::InvalidIdentity)
        ));
    }
    let migrated = keyring_entry(id, &creds, legacy_keyring_slot("oaiapp_admit"));
    assert!(
        admit_keyring_record(&migrated, &bytes(LEGACY_TOKEN_RECORD_SCHEMA, None, &creds)).is_ok()
    );
}

#[test]
fn accounts_of_one_client_keep_distinct_slots_and_identities() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let first = credentials("oaiapp_shared", "subject-one");
    let second = credentials("oaiapp_shared", "subject-two");
    let mut written = Vec::new();
    let mut save = |credentials: &VerifiedCredentials| {
        save_keyring_at_with(
            &state,
            &workspace,
            credentials.clone(),
            keyring_consent(credentials),
            None,
            |slot, body| {
                assert!(valid_written_keyring_record(slot, body.as_bytes()));
                written.push(slot.to_owned());
                Ok(())
            },
        )
        .unwrap()
    };
    let first_id = save(&first);
    let second_id = save(&second);
    assert_ne!(first_id, second_id);
    assert_eq!(save(&first), first_id);
    assert_eq!(written[0], account_keyring_slot("oaiapp_shared", first_id));
    assert_eq!(written[1], account_keyring_slot("oaiapp_shared", second_id));
    assert_ne!(written[0], written[1]);
    assert_eq!(written[2], written[0]);

    let index = AccountIndex::read(&state).unwrap();
    assert_eq!(index.accounts.len(), 2);
    assert_eq!(
        index.accounts[0].keyring_slot.as_deref(),
        Some(written[0].as_str())
    );
    assert_eq!(
        index.accounts[1].keyring_slot.as_deref(),
        Some(written[1].as_str())
    );

    let mut duplicate = serde_json::to_value(&index).unwrap();
    duplicate["accounts"][1]["subject"] = "subject-one".into();
    assert!(
        serde_json::from_value::<AccountIndex>(duplicate)
            .unwrap()
            .validate()
            .is_err()
    );
    let mut shared = serde_json::to_value(&index).unwrap();
    for account in 0..2 {
        shared["accounts"][account]["keyring_slot"] = legacy_keyring_slot("oaiapp_shared").into();
    }
    assert!(
        serde_json::from_value::<AccountIndex>(shared)
            .unwrap()
            .validate()
            .is_err()
    );
    let mut foreign = serde_json::to_value(&index).unwrap();
    foreign["accounts"][0]["keyring_slot"] = legacy_keyring_slot("oaiapp_other").into();
    assert!(
        serde_json::from_value::<AccountIndex>(foreign)
            .unwrap()
            .validate()
            .is_err()
    );

    let mut deleted = None;
    let outcome = sign_out_target_at_with(
        &state,
        &workspace,
        SignOutTarget {
            account_id: Some(first_id),
            expected_index: None,
        },
        SignOutActions {
            read_keyring: |_: &AccountEntry| Err(AuthorizationError::Unavailable),
            revoke: None::<fn(&VerifiedCredentials) -> Result<(), AuthorizationError>>,
            delete_keyring: |slot: &str| {
                deleted = Some(slot.to_owned());
                Ok(())
            },
        },
    )
    .unwrap();
    assert!(outcome.local_cleared);
    assert_eq!(deleted.as_deref(), Some(written[0].as_str()));
    let index = AccountIndex::read(&state).unwrap();
    assert!(index.accounts[0].disconnected);
    assert!(!index.accounts[1].disconnected);
    assert_eq!(
        index.accounts[1].keyring_slot.as_deref(),
        Some(written[1].as_str())
    );
}

#[test]
fn private_file_entries_carry_no_slot() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let creds = credentials("oaiapp_file", "subject-one");
    save_private_file_at(&state, &workspace, creds.clone(), accepted(&creds), None).unwrap();
    let index = AccountIndex::read(&state).unwrap();
    assert!(index.accounts[0].keyring_slot.is_none());
    assert!(index.accounts[0].chatgpt_account_id.is_none());
    assert!(index.accounts[0].token.as_ref().unwrap().slot.is_none());
    let mut slotted = serde_json::to_value(&index).unwrap();
    slotted["accounts"][0]["keyring_slot"] = legacy_keyring_slot("oaiapp_file").into();
    assert!(
        serde_json::from_value::<AccountIndex>(slotted)
            .unwrap()
            .validate()
            .is_err()
    );
    for field in ["keyring_slot", "chatgpt_account_id"] {
        let mut missing = serde_json::to_value(&index).unwrap();
        missing["accounts"][0]
            .as_object_mut()
            .unwrap()
            .remove(field);
        assert!(serde_json::from_value::<AccountIndex>(missing).is_err());
    }
}

#[test]
fn unsupported_or_duplicated_token_record_bytes_are_refused_on_every_read_path() {
    let id = Uuid::now_v7();
    let creds = credentials("oaiapp_dup", "subject-one");
    let slot = account_keyring_slot("oaiapp_dup", id);
    let entry = keyring_entry(id, &creds, slot.clone());
    let refused = |slot: &str, bytes: &[u8]| {
        assert!(decode_keyring_record(bytes).is_none());
        assert!(!valid_keyring_record(slot, bytes));
        assert!(!valid_written_keyring_record(slot, bytes));
        assert!(matches!(
            admit_keyring_record(&entry, bytes),
            Err(AuthorizationError::InvalidIdentity)
        ));
    };
    for schema in [0, 3, 255] {
        let value = token_record_json(schema, Some(&slot), &creds);
        refused(&slot, &serde_json::to_vec(&value).unwrap());
    }
    let current = serde_json::to_string(&TokenRecord {
        schema: TOKEN_RECORD_SCHEMA,
        slot: Some(slot.clone()),
        credentials: creds.clone(),
        consent: keyring_consent(&creds),
    })
    .unwrap();
    assert!(valid_keyring_record(&slot, current.as_bytes()));
    for (from, to) in [
        ("{\"schema\":2,", "{\"schema\":2,\"schema\":2,"),
        (
            "\"client_id\":\"oaiapp_dup\"",
            "\"client_id\":\"oaiapp_dup\",\"client_id\":\"oaiapp_dup\"",
        ),
        (
            "\"warning_version\":",
            "\"warning_version\":0,\"warning_version\":",
        ),
    ] {
        let duplicated = current.replacen(from, to, 1);
        assert_ne!(duplicated, current);
        refused(&slot, duplicated.as_bytes());
    }
    let legacy_slot = legacy_keyring_slot("oaiapp_dup");
    let legacy =
        serde_json::to_string(&token_record_json(LEGACY_TOKEN_RECORD_SCHEMA, None, &creds))
            .unwrap();
    assert!(valid_keyring_record(&legacy_slot, legacy.as_bytes()));
    let legacy_entry = keyring_entry(id, &creds, legacy_slot.clone());
    for (from, to) in [
        ("\"schema\":1", "\"schema\":1,\"schema\":1"),
        (
            "\"client_id\":\"oaiapp_dup\"",
            "\"client_id\":\"oaiapp_dup\",\"client_id\":\"oaiapp_dup\"",
        ),
        (
            "\"warning_version\":",
            "\"warning_version\":0,\"warning_version\":",
        ),
    ] {
        let duplicated = legacy.replacen(from, to, 1);
        assert_ne!(duplicated, legacy);
        assert!(!valid_keyring_record(&legacy_slot, duplicated.as_bytes()));
        assert!(matches!(
            admit_keyring_record(&legacy_entry, duplicated.as_bytes()),
            Err(AuthorizationError::InvalidIdentity)
        ));
    }
}

#[test]
fn older_account_indexes_are_refused_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    for subject in ["subject-one", "subject-two"] {
        let creds = credentials("oaiapp_twin", subject);
        save_private_file_at(&state, &workspace, creds.clone(), accepted(&creds), None).unwrap();
    }
    let current = AccountIndex::read(&state).unwrap();
    assert_eq!(current.accounts.len(), 2);
    assert!(current.validate().is_ok());

    for version in [1, 2] {
        let mut source = version_two_value(&current);
        source["schema"] = version.into();
        let bytes = serde_json::to_vec(&source).unwrap();
        assert!(CHATGPT_ACCOUNTS.mismatched(&bytes));
        let record = state.record(RecordKind::ChatgptAccounts);
        record.replace(&bytes).unwrap();
        assert!(AccountIndex::read(&state).is_err());
        assert_eq!(record.read().unwrap().unwrap(), bytes);
    }
}

#[cfg(unix)]
const FAKE_HELPER: &str = r#"#!/bin/sh
d="${0%/*}/store"
case "$2" in
read) [ -f "$d/$3" ] || exit 2; /bin/cat "$d/$3" ;;
write) [ ! -f "$d/fail-write" ] || exit 1; /bin/cat > "$d/$3.tmp" && /bin/mv "$d/$3.tmp" "$d/$3" ;;
delete) /bin/rm -f "$d/$3" ;;
*) exit 1 ;;
esac
"#;

#[cfg(unix)]
struct FakeHelper {
    store: PathBuf,
    _gate: std::sync::MutexGuard<'static, ()>,
}

#[cfg(unix)]
impl FakeHelper {
    fn install(dir: &Path) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let gate = crate::test_process::NATIVE_GATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let store = dir.join("store");
        std::fs::create_dir(&store).unwrap();
        let helper = dir.join("helper");
        std::fs::write(&helper, FAKE_HELPER).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        *crate::accounts::credentials::TEST_HELPER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(helper);
        Self { store, _gate: gate }
    }

    fn put(&self, slot: &str, bytes: &[u8]) {
        std::fs::write(self.store.join(slot), bytes).unwrap();
    }

    fn get(&self, slot: &str) -> Vec<u8> {
        std::fs::read(self.store.join(slot)).unwrap()
    }

    fn holds(&self, slot: &str) -> bool {
        self.store.join(slot).exists()
    }

    fn fail_writes(&self, fail: bool) {
        let marker = self.store.join("fail-write");
        if fail {
            std::fs::write(marker, b"").unwrap();
        } else {
            std::fs::remove_file(marker).unwrap();
        }
    }
}

#[cfg(unix)]
impl Drop for FakeHelper {
    fn drop(&mut self) {
        *crate::accounts::credentials::TEST_HELPER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

#[cfg(unix)]
fn rotate(previous: &VerifiedCredentials) -> Result<VerifiedCredentials, AuthorizationError> {
    let mut next = previous.clone();
    next.access_token = "synthetic-rotated-access".into();
    next.refresh_token = "synthetic-rotated-refresh".into();
    next.access_expires_at_unix = 1_800_003_600;
    Ok(next)
}

#[cfg(unix)]
#[test]
fn legacy_slot_keyring_accounts_load_refresh_and_block_through_their_retained_slots() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let first = credentials("oaiapp_first_issued", "shared-subject");
    let second = credentials("oaiapp_second_issued", "shared-subject");
    let mut ids = Vec::new();
    for creds in [&first, &second] {
        ids.push(
            save_keyring_at_with(
                &state,
                &workspace,
                creds.clone(),
                keyring_consent(creds),
                None,
                |_, _| Ok(()),
            )
            .unwrap(),
        );
    }
    let (first_id, second_id) = (ids[0], ids[1]);
    let helper = FakeHelper::install(temp.path());
    let legacy_first = legacy_keyring_slot("oaiapp_first_issued");
    let legacy_second = legacy_keyring_slot("oaiapp_second_issued");
    // Current indexes keep the client-derived slot an earlier release wrote tokens at.
    let mut index = AccountIndex::read(&state).unwrap();
    index.accounts[0].keyring_slot = Some(legacy_first.clone());
    index.accounts[1].keyring_slot = Some(legacy_second.clone());
    index.write(&state).unwrap();
    let version_one = |creds: &VerifiedCredentials| {
        serde_json::to_vec(&token_record_json(LEGACY_TOKEN_RECORD_SCHEMA, None, creds)).unwrap()
    };
    helper.put(&legacy_first, &version_one(&first));
    let second_bytes = version_one(&second);
    helper.put(&legacy_second, &second_bytes);
    let first_bytes = helper.get(&legacy_first);

    let mut index = AccountIndex::read(&state).unwrap();
    index.selected = Some(second_id);
    index.write(&state).unwrap();
    assert_eq!(
        load_selected_keyring_at(&state, second_id)
            .unwrap()
            .refresh_token,
        "synthetic-refresh"
    );
    assert_eq!(helper.get(&legacy_second), second_bytes);

    let renewed =
        refresh_selected_with(&state, &workspace, second_id, 1_799_999_900, rotate).unwrap();
    assert_eq!(
        renewed.credentials.refresh_token,
        "synthetic-rotated-refresh"
    );
    let stored: TokenRecord = serde_json::from_slice(&helper.get(&legacy_second)).unwrap();
    assert_eq!(stored.schema, TOKEN_RECORD_SCHEMA);
    assert_eq!(stored.slot.as_deref(), Some(legacy_second.as_str()));
    assert_eq!(
        stored.credentials.refresh_token,
        "synthetic-rotated-refresh"
    );
    assert_eq!(helper.get(&legacy_first), first_bytes);
    assert!(!AccountIndex::read(&state).unwrap().accounts[1].renewal_pending);

    let mut index = AccountIndex::read(&state).unwrap();
    index.selected = Some(first_id);
    index.write(&state).unwrap();
    helper.fail_writes(true);
    assert!(matches!(
        refresh_selected_with(&state, &workspace, first_id, 1_799_999_900, rotate),
        Err(AuthorizationError::RenewalStorageUncertain)
    ));
    helper.fail_writes(false);
    let reopened = StateRoot::open_existing(state.path()).unwrap();
    assert!(AccountIndex::read(&reopened).unwrap().accounts[0].renewal_pending);
    assert!(matches!(
        load_selected_keyring_at(&reopened, first_id),
        Err(AuthorizationError::RenewalStorageUncertain)
    ));
    assert_eq!(helper.get(&legacy_first), first_bytes);
    assert_eq!(
        serde_json::from_slice::<TokenRecord>(&helper.get(&legacy_second))
            .unwrap()
            .credentials
            .refresh_token,
        "synthetic-rotated-refresh"
    );

    assert_eq!(
        save_keyring_at(
            &reopened,
            &workspace,
            first.clone(),
            keyring_consent(&first),
            Some(first_id),
        )
        .unwrap(),
        first_id
    );
    let repaired: TokenRecord = serde_json::from_slice(&helper.get(&legacy_first)).unwrap();
    assert_eq!(repaired.schema, TOKEN_RECORD_SCHEMA);
    assert_eq!(repaired.slot.as_deref(), Some(legacy_first.as_str()));
    assert!(!AccountIndex::read(&reopened).unwrap().accounts[0].renewal_pending);

    let removed = sign_out_target_at_with(
        &reopened,
        &workspace,
        SignOutTarget {
            account_id: Some(second_id),
            expected_index: None,
        },
        SignOutActions {
            read_keyring: read_keyring_token,
            revoke: None::<fn(&VerifiedCredentials) -> Result<(), AuthorizationError>>,
            delete_keyring: |slot: &str| {
                delete_chatgpt_keyring_record(slot).map_err(|_| AuthorizationError::Unavailable)
            },
        },
    )
    .unwrap();
    assert!(removed.local_cleared);
    assert!(!helper.holds(&legacy_second));
    assert!(helper.holds(&legacy_first));
}

#[cfg(unix)]
#[test]
fn same_client_keyring_accounts_refresh_and_sign_out_in_isolation() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let helper = FakeHelper::install(temp.path());
    let first = credentials("oaiapp_shared_pair", "subject-one");
    let second = credentials("oaiapp_shared_pair", "subject-two");
    let mut ids = Vec::new();
    for creds in [&first, &second] {
        ids.push(
            save_keyring_at(
                &state,
                &workspace,
                creds.clone(),
                keyring_consent(creds),
                None,
            )
            .unwrap(),
        );
    }
    let (first_id, second_id) = (ids[0], ids[1]);
    let index = AccountIndex::read(&state).unwrap();
    let first_slot = index.accounts[0].keyring_slot.clone().unwrap();
    let second_slot = index.accounts[1].keyring_slot.clone().unwrap();
    assert_eq!(
        first_slot,
        account_keyring_slot("oaiapp_shared_pair", first_id)
    );
    assert_eq!(
        second_slot,
        account_keyring_slot("oaiapp_shared_pair", second_id)
    );
    assert_ne!(first_slot, second_slot);
    let second_bytes = helper.get(&second_slot);

    let mut selected = index;
    selected.selected = Some(first_id);
    selected.write(&state).unwrap();
    let renewed = refresh_selected_with(&state, &workspace, first_id, 1_799_999_900, |previous| {
        assert!(AccountIndex::read(&state).unwrap().accounts[0].renewal_pending);
        rotate(previous)
    })
    .unwrap();
    assert_eq!(
        renewed.credentials.refresh_token,
        "synthetic-rotated-refresh"
    );
    let stored: TokenRecord = serde_json::from_slice(&helper.get(&first_slot)).unwrap();
    assert_eq!(stored.slot.as_deref(), Some(first_slot.as_str()));
    assert_eq!(
        stored.credentials.refresh_token,
        "synthetic-rotated-refresh"
    );
    assert_eq!(helper.get(&second_slot), second_bytes);
    assert!(!AccountIndex::read(&state).unwrap().accounts[0].renewal_pending);

    let removed = sign_out_target_at_with(
        &state,
        &workspace,
        SignOutTarget {
            account_id: Some(first_id),
            expected_index: None,
        },
        SignOutActions {
            read_keyring: read_keyring_token,
            revoke: None::<fn(&VerifiedCredentials) -> Result<(), AuthorizationError>>,
            delete_keyring: |slot: &str| {
                delete_chatgpt_keyring_record(slot).map_err(|_| AuthorizationError::Unavailable)
            },
        },
    )
    .unwrap();
    assert!(removed.local_cleared);
    assert!(!helper.holds(&first_slot));
    assert_eq!(helper.get(&second_slot), second_bytes);
    let mut index = AccountIndex::read(&state).unwrap();
    index.selected = Some(second_id);
    index.write(&state).unwrap();
    assert_eq!(
        load_selected_keyring_at(&state, second_id)
            .unwrap()
            .refresh_token,
        "synthetic-refresh"
    );
}

#[test]
fn pending_keyring_renewal_blocks_load_before_os_store_access() {
    let temp = tempfile::tempdir().unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let id = Uuid::now_v7();
    AccountIndex {
        schema: CHATGPT_ACCOUNTS.current as u8,
        selected: Some(id),
        accounts: vec![AccountEntry {
            id,
            host_id: credentials("oaiapp_pending", "subject-one").host_id,
            client_id: "oaiapp_pending".into(),
            subject: "subject-one".into(),
            chatgpt_account_id: None,
            keyring_slot: Some(account_keyring_slot("oaiapp_pending", id)),
            storage: AccountStorage::Keyring,
            renewal_pending: true,
            signout_pending: false,
            disconnected: false,
            plan_permission_missing: false,
            token: None,
        }],
        model_checks: Vec::new(),
    }
    .write(&state)
    .unwrap();
    assert_eq!(selected_id_at(&state).unwrap(), id);
    let target = selected_reauthorization_target_at(&state, id).unwrap();
    assert_eq!(target.id, id);
    assert_eq!(target.client_id, "oaiapp_pending");
    assert_eq!(target.subject, "subject-one");
    assert_eq!(target.storage, AccountStorage::Keyring);
    assert!(matches!(
        selected_reauthorization_target_at(&state, Uuid::now_v7()),
        Err(AuthorizationError::SelectedAccountChanged)
    ));
    assert!(matches!(
        load_selected_keyring_at(&state, id),
        Err(AuthorizationError::RenewalStorageUncertain)
    ));
}

#[test]
fn keyring_reauthorization_fails_closed_after_an_uncertain_write() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let first = credentials("oaiapp_reconnect", "subject-one");
    let consent = RiskPrompt::new(AccountStorage::Keyring)
        .accept("Accept")
        .unwrap()
        .bind(&first)
        .unwrap();
    let id = save_keyring_at_with(
        &state,
        &workspace,
        first.clone(),
        consent,
        None,
        |slot, body| {
            assert!(valid_keyring_record(slot, body.as_bytes()));
            Ok(())
        },
    )
    .unwrap();
    let mut index = AccountIndex::read(&state).unwrap();
    index.model_checks.push(ModelCheck {
        check_id: CheckId::new(),
        account_id: id,
        fingerprint: [7; 32],
        checked_at_sec: 1_800_000_000,
        expires_at_sec: 1_800_000_000 + MODEL_CHECK_AGE_SECONDS,
        context_window_tokens: None,
    });
    index.write(&state).unwrap();

    let mut replacement = first;
    replacement.refresh_token = "synthetic-reconnected-refresh".into();
    let consent = RiskPrompt::new(AccountStorage::Keyring)
        .accept("Accept")
        .unwrap()
        .bind(&replacement)
        .unwrap();
    assert!(matches!(
        save_keyring_at_with(
            &state,
            &workspace,
            replacement.clone(),
            consent.clone(),
            Some(Uuid::now_v7()),
            |_, _| panic!("selection drift must not write a token"),
        ),
        Err(AuthorizationError::SelectedAccountChanged)
    ));
    assert!(!AccountIndex::read(&state).unwrap().accounts[0].renewal_pending);
    assert!(matches!(
        save_keyring_at_with(
            &state,
            &workspace,
            replacement.clone(),
            consent.clone(),
            Some(id),
            |_, _| Err(AuthorizationError::Unavailable),
        ),
        Err(AuthorizationError::CredentialStore(
            crate::accounts::credentials::CredentialError::WriteOutcomeUnknown
        ))
    ));
    let pending = AccountIndex::read(&state).unwrap();
    assert_eq!(pending.selected, Some(id));
    assert!(pending.accounts[0].renewal_pending);
    assert_eq!(pending.model_checks.len(), 1);
    assert!(matches!(
        load_selected_keyring_at(&state, id),
        Err(AuthorizationError::RenewalStorageUncertain)
    ));
    assert_eq!(
        selected_reauthorization_target_at(&state, id)
            .unwrap()
            .client_id,
        "oaiapp_reconnect"
    );

    assert_eq!(
        save_keyring_at_with(
            &state,
            &workspace,
            replacement,
            consent,
            Some(id),
            |slot, body| {
                assert!(valid_keyring_record(slot, body.as_bytes()));
                Ok(())
            },
        )
        .unwrap(),
        id
    );
    let restored = AccountIndex::read(&state).unwrap();
    assert_eq!(restored.selected, Some(id));
    assert!(!restored.accounts[0].renewal_pending);
    assert!(restored.model_checks.is_empty());
}

fn model_account() -> (tempfile::TempDir, StateRoot, PathBuf, SelectedAccount) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let credentials = credentials("oaiapp_checked", "subject-one");
    let id = save_private_file_at(
        &state,
        &workspace,
        credentials.clone(),
        accepted(&credentials),
        None,
    )
    .unwrap();
    let selected = refresh_selected_with(&state, &workspace, id, 1_799_000_000, |_| {
        panic!("fresh token must not rotate")
    })
    .unwrap();
    (temp, state, workspace, selected)
}

#[test]
fn catalog_admission_uses_matching_metadata_without_a_check() {
    for (slug, catalog_window, expected) in [
        ("model-one", Some(272_000), Some(272_000)),
        ("model-one", Some(32_768), Some(32_768)),
        ("model-one", Some(2_000_000), Some(2_000_000)),
        ("model-one", None, None),
        ("model-two", Some(272_000), None),
    ] {
        let (_temp, state, workspace, selected) = model_account();
        let catalog = [super::super::ChatGptModel {
            slug: slug.into(),
            display_name: "First model".into(),
            context_window_tokens: catalog_window,
        }];
        let (_, window) = admitted_model_at(
            &state,
            &workspace,
            &selected,
            RunModel {
                slug: "model-one",
                effort: Effort::High,
                now: 1_799_999_010,
            },
            Ok(&catalog),
        )
        .unwrap();
        assert_eq!(window, expected);
        assert!(AccountIndex::read(&state).unwrap().model_checks.is_empty());
    }
}

#[test]
fn catalog_failure_uses_the_latest_same_slug_check_across_efforts() {
    let (_temp, state, workspace, selected) = model_account();
    for (slug, effort, window, now) in [
        ("model-one", Effort::High, Some(272_000), 1_799_999_001),
        ("model-one", Effort::Low, Some(320_000), 1_799_999_000),
        ("model-two", Effort::High, Some(500_000), 1_799_999_002),
    ] {
        record_model_check_at(&state, &workspace, &selected, slug, effort, window, now).unwrap();
    }
    let catalog = [super::super::ChatGptModel {
        slug: "model-one".into(),
        display_name: "First model".into(),
        context_window_tokens: Some(400_000),
    }];
    let without_window = [super::super::ChatGptModel {
        slug: "model-one".into(),
        display_name: "First model".into(),
        context_window_tokens: None,
    }];
    for (slug, live, expected) in [
        (
            "model-one",
            Err(&AuthorizationError::Unavailable),
            Some(272_000),
        ),
        ("model-one", Ok(catalog.as_slice()), Some(400_000)),
        ("model-one", Ok(without_window.as_slice()), None),
        ("model-one", Ok([].as_slice()), None),
        ("model-three", Err(&AuthorizationError::Unavailable), None),
    ] {
        let (_, window) = admitted_model_at(
            &state,
            &workspace,
            &selected,
            RunModel {
                slug,
                effort: Effort::High,
                now: 1_799_999_010,
            },
            live,
        )
        .unwrap();
        assert_eq!(window, expected);
    }
    record_model_check_at(
        &state,
        &workspace,
        &selected,
        "model-one",
        Effort::Medium,
        None,
        1_799_999_003,
    )
    .unwrap();
    let (_, window) = admitted_model_at(
        &state,
        &workspace,
        &selected,
        RunModel {
            slug: "model-one",
            effort: Effort::High,
            now: 1_799_999_010,
        },
        Err(&AuthorizationError::Unavailable),
    )
    .unwrap();
    assert_eq!(
        window, None,
        "the newest check must not revive older metadata"
    );
}

#[test]
fn catalog_failure_ignores_stale_and_future_checks() {
    let (_temp, state, workspace, selected) = model_account();
    let checked_at = 1_799_999_000;
    record_model_check_at(
        &state,
        &workspace,
        &selected,
        "model-one",
        Effort::High,
        Some(272_000),
        checked_at,
    )
    .unwrap();
    for (now, expected) in [
        (checked_at - 1, None),
        (checked_at + MODEL_CHECK_AGE_SECONDS - 1, Some(272_000)),
        (checked_at + MODEL_CHECK_AGE_SECONDS, None),
    ] {
        let (_, window) = admitted_model_at(
            &state,
            &workspace,
            &selected,
            RunModel {
                slug: "model-one",
                effort: Effort::High,
                now,
            },
            Err(&AuthorizationError::Unavailable),
        )
        .unwrap();
        assert_eq!(window, expected);
    }
}

#[tokio::test(start_paused = true)]
async fn slow_admission_catalog_falls_back_after_five_seconds() {
    use std::time::Duration;
    const DELAYED_REPLY: Duration = Duration::from_secs(6);

    let (_temp, state, workspace, selected) = model_account();
    let checked_at = 1_799_999_000;
    record_model_check_at(
        &state,
        &workspace,
        &selected,
        "model-one",
        Effort::High,
        Some(272_000),
        checked_at,
    )
    .unwrap();
    let started = tokio::time::Instant::now();
    let catalog = super::super::catalog::read_for_run(async {
        tokio::time::sleep(DELAYED_REPLY).await;
        Ok(Vec::new())
    })
    .await;
    assert!(matches!(&catalog, Err(AuthorizationError::Unavailable)));
    assert_eq!(started.elapsed(), Duration::from_secs(5));
    let (_, window) = admitted_model_at(
        &state,
        &workspace,
        &selected,
        RunModel {
            slug: "model-one",
            effort: Effort::High,
            now: checked_at + 1,
        },
        catalog.as_deref(),
    )
    .unwrap();
    assert_eq!(window, Some(272_000));
}

#[test]
fn model_check_evidence_is_exact_temporary_and_cleared_by_new_sign_in() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let credentials = credentials("oaiapp_checked", "subject-one");
    let id = save_private_file_at(
        &state,
        &workspace,
        credentials.clone(),
        accepted(&credentials),
        None,
    )
    .unwrap();
    let selected = refresh_selected_with(&state, &workspace, id, 1_799_000_000, |_| {
        panic!("fresh token must not rotate")
    })
    .unwrap();
    let now = 1_799_999_000;
    let (admitted, window) = admitted_model_at(
        &state,
        &workspace,
        &selected,
        RunModel {
            slug: "model-one",
            effort: Effort::High,
            now: 1_799_999_010,
        },
        Err(&AuthorizationError::Unavailable),
    )
    .expect("consented account needs no synthetic check");
    assert_eq!(window, None);
    assert!(AccountIndex::read(&state).unwrap().model_checks.is_empty());
    let baseline = AccountIndex::read(&state).unwrap();
    for (field, expected) in [
        (
            "renewal_pending",
            AuthorizationError::RenewalStorageUncertain,
        ),
        ("signout_pending", AuthorizationError::SignOutPending),
        ("disconnected", AuthorizationError::NoSelectedAccount),
    ] {
        let mut blocked = serde_json::to_value(&baseline).unwrap();
        blocked["accounts"][0][field] = true.into();
        if field == "disconnected" {
            blocked["accounts"][0]["token"] = serde_json::Value::Null;
        }
        let blocked: AccountIndex = serde_json::from_value(blocked).unwrap();
        blocked.write(&state).unwrap();
        assert_eq!(
            admitted_model_at(
                &state,
                &workspace,
                &selected,
                RunModel {
                    slug: "model-one",
                    effort: Effort::High,
                    now: 1_799_999_010,
                },
                Err(&AuthorizationError::Unavailable)
            ),
            Err(expected)
        );
    }
    baseline.write(&state).unwrap();
    assert!(matches!(
        admitted_model_at(
            &state,
            &workspace,
            &selected,
            RunModel {
                slug: "bad model",
                effort: Effort::High,
                now: 1_799_999_010,
            },
            Err(&AuthorizationError::Unavailable)
        ),
        Err(AuthorizationError::InvalidSelection)
    ));
    record_model_check_at(
        &state,
        &workspace,
        &selected,
        "model-one",
        Effort::High,
        Some(272_000),
        now,
    )
    .unwrap();
    let index = AccountIndex::read(&state).unwrap();
    let fingerprint = model_check_fingerprint(&selected, "model-one", Effort::High).unwrap();
    assert_ne!(admitted, fingerprint);
    let proof = &index.model_checks[0];
    assert!(
        state
            .check_evidence_valid(proof.check_id, fingerprint, proof.digest().unwrap())
            .unwrap()
    );
    assert_superseded_account_versions(&state, &selected, admitted, &index, now);
    for (model, effort, window) in [
        ("model-one", Effort::High, Some(272_000)),
        ("model-two", Effort::High, None),
        ("model-one", Effort::Low, Some(272_000)),
    ] {
        assert_eq!(
            admitted_model_at(
                &state,
                &workspace,
                &selected,
                RunModel {
                    slug: model,
                    effort,
                    now: 1_799_999_010,
                },
                Err(&AuthorizationError::Unavailable)
            )
            .map(|(_, window)| window),
            Ok(window),
            "the last same-slug check supplies the cached window across efforts"
        );
    }
    let mut out_of_range = serde_json::to_value(&index).unwrap();
    out_of_range["model_checks"][0]["context_window_tokens"] = 2_000_001.into();
    assert!(AccountIndex::decode(&serde_json::to_vec(&out_of_range).unwrap()).is_err());
    assert_eq!(
        admitted_model_at(
            &state,
            &workspace,
            &selected,
            RunModel {
                slug: "model-one",
                effort: Effort::High,
                now: 1_799_999_010,
            },
            Err(&AuthorizationError::Unavailable)
        )
        .unwrap(),
        (admitted, Some(272_000))
    );
    for (model, effort) in [("model-two", Effort::High), ("model-one", Effort::Low)] {
        let (other, window) = admitted_model_at(
            &state,
            &workspace,
            &selected,
            RunModel {
                slug: model,
                effort,
                now: 1_799_999_010,
            },
            Err(&AuthorizationError::Unavailable),
        )
        .unwrap();
        assert_ne!(admitted, other);
        assert_eq!(window, (model == "model-one").then_some(272_000));
    }

    let renewed = refresh_selected_with(&state, &workspace, id, 1_799_999_900, |_| {
        let mut replacement = credentials.clone();
        replacement.access_token = "rotated-access".into();
        replacement.refresh_token = "rotated-refresh".into();
        replacement.access_expires_at_unix = 1_800_003_600;
        Ok(replacement)
    })
    .unwrap();
    assert_eq!(
        model_check_fingerprint(&renewed, "model-one", Effort::High).unwrap(),
        fingerprint
    );
    assert_eq!(
        admitted_model_at(
            &state,
            &workspace,
            &renewed,
            RunModel {
                slug: "model-one",
                effort: Effort::High,
                now: 1_799_999_010,
            },
            Err(&AuthorizationError::Unavailable)
        )
        .unwrap(),
        (admitted, Some(272_000))
    );

    let mut altered = renewed.clone();
    let mut consent = serde_json::to_value(&altered.consent).unwrap();
    consent["accepted_at_sec"] = 1_u64.into();
    altered.consent = serde_json::from_value(consent).unwrap();
    assert_ne!(
        model_check_fingerprint(&altered, "model-one", Effort::High).unwrap(),
        fingerprint
    );
    assert!(matches!(
        admitted_model_at(
            &state,
            &workspace,
            &altered,
            RunModel {
                slug: "model-one",
                effort: Effort::High,
                now: 1_799_999_010,
            },
            Err(&AuthorizationError::Unavailable)
        ),
        Err(AuthorizationError::InvalidIdentity)
    ));

    clear_model_check_at(&state, &workspace, &renewed, "model-one", Effort::High).unwrap();
    assert!(AccountIndex::read(&state).unwrap().model_checks.is_empty());
    assert_eq!(
        admitted_model_at(
            &state,
            &workspace,
            &renewed,
            RunModel {
                slug: "model-one",
                effort: Effort::High,
                now: 1_799_999_010,
            },
            Err(&AuthorizationError::Unavailable)
        )
        .unwrap(),
        (admitted, None)
    );
    record_model_check_at(
        &state,
        &workspace,
        &renewed,
        "model-one",
        Effort::High,
        None,
        now,
    )
    .unwrap();
    assert_eq!(
        save_private_file_at(
            &state,
            &workspace,
            credentials.clone(),
            accepted(&credentials),
            Some(id),
        )
        .unwrap(),
        id
    );
    assert!(AccountIndex::read(&state).unwrap().model_checks.is_empty());
}

fn assert_superseded_account_versions(
    state: &StateRoot,
    selected: &SelectedAccount,
    admitted: [u8; 32],
    index: &AccountIndex,
    now: u64,
) {
    let previous_admission = account_fingerprint(
        selected,
        "model-one",
        Effort::High,
        "chatgpt-consented-account-admission-v38",
        "no-preflight-inference;strict-stream-outcomes;local-output-bound",
    )
    .unwrap();
    assert_ne!(admitted, previous_admission);
    assert!(
        selected
            .consent
            .matches(&selected.credentials, selected.storage)
    );
    let previous_check = account_fingerprint(
        selected,
        "model-one",
        Effort::High,
        "chatgpt-goal-contract-strict-stream-conformance-journal-v39",
        "three-probes-1024-local-output-tokens",
    )
    .unwrap();
    let current = model_check_fingerprint(selected, "model-one", Effort::High).unwrap();
    assert_ne!(current, previous_check);
    let mut superseded = AccountIndex::decode(&serde_json::to_vec(index).unwrap()).unwrap();
    assert_eq!(superseded.model_checks.len(), 1);
    superseded.model_checks[0].fingerprint = previous_check;
    assert_eq!(
        checked_model_window(state, &superseded, selected, "model-one", now).unwrap(),
        None
    );
}

#[test]
fn replaced_or_stale_account_cannot_publish_model_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let first = credentials("oaiapp_first", "subject-one");
    let first_id =
        save_private_file_at(&state, &workspace, first.clone(), accepted(&first), None).unwrap();
    let selected = refresh_selected_with(&state, &workspace, first_id, 1_799_000_000, |_| {
        panic!("fresh token must not rotate")
    })
    .unwrap();
    let second = credentials("oaiapp_second", "subject-two");
    save_private_file_at(&state, &workspace, second.clone(), accepted(&second), None).unwrap();
    assert!(matches!(
        record_model_check_at(
            &state,
            &workspace,
            &selected,
            "model-one",
            Effort::High,
            None,
            1_799_000_000,
        ),
        Err(AuthorizationError::InvalidIdentity)
    ));
    assert!(AccountIndex::read(&state).unwrap().model_checks.is_empty());
    assert!(matches!(
        admitted_model_at(
            &state,
            &workspace,
            &selected,
            RunModel {
                slug: "model-one",
                effort: Effort::High,
                now: 1_799_999_010,
            },
            Ok(&[])
        ),
        Err(AuthorizationError::InvalidIdentity)
    ));

    let mut stale = selected;
    stale.credentials.subject = "changed-subject".into();
    assert!(matches!(
        clear_model_check_at(&state, &workspace, &stale, "model-one", Effort::High),
        Err(AuthorizationError::ConsentRequired)
    ));
    assert!(matches!(
        admitted_model_at(
            &state,
            &workspace,
            &stale,
            RunModel {
                slug: "model-one",
                effort: Effort::High,
                now: 1_799_999_010,
            },
            Ok(&[])
        ),
        Err(AuthorizationError::ConsentRequired)
    ));
}

#[test]
fn model_check_record_limit_fails_without_replacing_account() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let credentials = credentials("oaiapp_checked", "subject-one");
    let id = save_private_file_at(
        &state,
        &workspace,
        credentials.clone(),
        accepted(&credentials),
        None,
    )
    .unwrap();
    let selected = refresh_selected_with(&state, &workspace, id, 1_799_000_000, |_| {
        panic!("fresh token must not rotate")
    })
    .unwrap();
    let now = 1_799_999_000;
    let mut index = AccountIndex::read(&state).unwrap();
    for byte in 0..MAX_MODEL_CHECKS {
        index.model_checks.push(ModelCheck {
            check_id: CheckId::new(),
            account_id: id,
            fingerprint: [byte as u8; 32],
            checked_at_sec: now,
            expires_at_sec: now + MODEL_CHECK_AGE_SECONDS,
            context_window_tokens: None,
        });
    }
    index.write(&state).unwrap();
    let mut oversized = serde_json::to_value(AccountIndex::read(&state).unwrap()).unwrap();
    oversized["model_checks"] = serde_json::Value::Array(vec![
        oversized["model_checks"][0].clone();
        MAX_MODEL_CHECKS + 1
    ]);
    assert!(serde_json::from_value::<AccountIndex>(oversized).is_err());
    assert!(matches!(
        record_model_check_at(
            &state,
            &workspace,
            &selected,
            "new-model",
            Effort::High,
            None,
            now,
        ),
        Err(AuthorizationError::EvidenceUnavailable)
    ));
    assert_eq!(
        AccountIndex::read(&state).unwrap().model_checks.len(),
        MAX_MODEL_CHECKS
    );
    assert_eq!(AccountIndex::read(&state).unwrap().selected, Some(id));
}

#[test]
fn reuse_picker_excludes_every_unusable_nonselected_account() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let mut ids = Vec::new();
    for client in ["oaiapp_first_picker", "oaiapp_selected_picker"] {
        let credentials = credentials(client, "synthetic-subject");
        let consent = RiskPrompt::new(AccountStorage::Keyring)
            .accept("Accept")
            .unwrap()
            .bind(&credentials)
            .unwrap();
        ids.push(
            save_keyring_at_with(
                &state,
                &workspace,
                credentials,
                consent,
                None,
                |_, _| Ok(()),
            )
            .unwrap(),
        );
    }
    for (disconnected, signout_pending, renewal_pending, plan_permission_missing, usable) in [
        (false, false, false, false, true),
        (true, false, false, false, false),
        (false, true, false, false, false),
        (false, false, true, false, false),
        (true, false, false, true, false),
    ] {
        let mut index = AccountIndex::read(&state).unwrap();
        let entry = &mut index.accounts[0];
        entry.disconnected = disconnected;
        entry.signout_pending = signout_pending;
        entry.renewal_pending = renewal_pending;
        entry.plan_permission_missing = plan_permission_missing;
        index.write(&state).unwrap();
        assert_eq!(
            saved_ids_at(&state).unwrap(),
            (
                ids[1],
                if usable {
                    vec![ids[1], ids[0]]
                } else {
                    vec![ids[1]]
                }
            )
        );
    }
}

#[test]
fn saved_chatgpt_selection_requires_a_current_account_and_preserves_tokens() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let first = credentials("oaiapp_first", "subject-one");
    let first_id =
        save_private_file_at(&state, &workspace, first.clone(), accepted(&first), None).unwrap();
    let second = credentials("oaiapp_second", "subject-two");
    let second_id =
        save_private_file_at(&state, &workspace, second.clone(), accepted(&second), None).unwrap();

    assert_eq!(
        saved_ids_at(&state).unwrap(),
        (second_id, vec![second_id, first_id])
    );
    assert!(matches!(
        select_saved_at(&state, &workspace, first_id, first_id),
        Err(AuthorizationError::SelectedAccountChanged)
    ));
    assert!(matches!(
        select_saved_at(&state, &workspace, second_id, Uuid::now_v7()),
        Err(AuthorizationError::SelectedAccountChanged)
    ));
    assert_eq!(selected_id_at(&state).unwrap(), second_id);

    let mut index = AccountIndex::read(&state).unwrap();
    index.accounts[0].renewal_pending = true;
    index.write(&state).unwrap();
    assert!(matches!(
        select_saved_at(&state, &workspace, second_id, first_id),
        Err(AuthorizationError::RenewalStorageUncertain)
    ));
    index.accounts[0].renewal_pending = false;
    index.write(&state).unwrap();
    let token = index.accounts[0].token.as_mut().unwrap();
    let consent = token.consent.clone();
    let mut stale = serde_json::to_value(&consent).unwrap();
    stale["warning_version"] = 0.into();
    token.consent = serde_json::from_value(stale).unwrap();
    index.write(&state).unwrap();
    assert!(matches!(
        select_saved_at(&state, &workspace, second_id, first_id),
        Err(AuthorizationError::ConsentRequired)
    ));
    index.accounts[0].token.as_mut().unwrap().consent = consent;
    index.write(&state).unwrap();

    select_saved_at(&state, &workspace, second_id, first_id).unwrap();
    let reopened = StateRoot::open_existing(state.path()).unwrap();
    assert_eq!(
        saved_ids_at(&reopened).unwrap(),
        (first_id, vec![first_id, second_id])
    );
    assert_eq!(selected_id_at(&reopened).unwrap(), first_id);
    assert_eq!(
        load_selected_private_file_at(&state, first_id)
            .unwrap()
            .refresh_token,
        first.refresh_token
    );
    assert_eq!(
        AccountIndex::read(&state).unwrap().accounts[1]
            .token
            .as_ref()
            .unwrap()
            .credentials
            .refresh_token,
        second.refresh_token
    );
}

#[test]
fn setup_signout_removes_the_confirmed_identity_and_rejects_a_stale_index() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let first = credentials("oaiapp_first", "subject-one");
    let first_id =
        save_private_file_at(&state, &workspace, first.clone(), accepted(&first), None).unwrap();
    let second = credentials("oaiapp_second", "subject-two");
    let second_id =
        save_private_file_at(&state, &workspace, second.clone(), accepted(&second), None).unwrap();
    let snapshot = state
        .record(RecordKind::ChatgptAccounts)
        .read()
        .unwrap()
        .unwrap();
    assert_eq!(
        AccountIndex::read(&state).unwrap().selected,
        Some(second_id)
    );
    let outcome = sign_out_target_at_with(
        &state,
        &workspace,
        SignOutTarget {
            account_id: Some(first_id),
            expected_index: Some(&snapshot),
        },
        SignOutActions {
            read_keyring: |_: &AccountEntry| panic!("private account must not read keyring"),
            revoke: Some(|token: &VerifiedCredentials| {
                assert_eq!(token.refresh_token, first.refresh_token);
                Ok(())
            }),
            delete_keyring: |_: &str| panic!("private account must not delete keyring"),
        },
    )
    .unwrap();
    assert!(outcome.remote_confirmed && outcome.local_cleared);
    let index = AccountIndex::read(&state).unwrap();
    assert_eq!(index.selected, Some(second_id));
    assert!(
        index
            .accounts
            .iter()
            .find(|entry| entry.id == first_id)
            .unwrap()
            .disconnected
    );

    let stale = state
        .record(RecordKind::ChatgptAccounts)
        .read()
        .unwrap()
        .unwrap();
    let index = AccountIndex::read(&state).unwrap();
    let changed_id = Uuid::now_v7();
    let mut changed = index;
    changed.selected = Some(changed_id);
    changed.accounts[1].id = changed_id;
    changed.write(&state).unwrap();
    assert!(matches!(
        sign_out_target_at_with(
            &state,
            &workspace,
            SignOutTarget {
                account_id: Some(second_id),
                expected_index: Some(&stale),
            },
            SignOutActions {
                read_keyring: |_: &AccountEntry| { panic!("stale account must not read keyring") },
                revoke: Some(|_: &VerifiedCredentials| panic!("stale account must not revoke")),
                delete_keyring: |_: &str| panic!("stale account must not delete keyring"),
            },
        ),
        Err(AuthorizationError::SetupChanged)
    ));
}

#[test]
fn local_account_removal_disconnects_without_remote_revocation() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let account = credentials("oaiapp_local_remove", "local-remove-subject");
    let id = save_private_file_at(
        &state,
        &workspace,
        account.clone(),
        accepted(&account),
        None,
    )
    .unwrap();
    let snapshot = state
        .record(RecordKind::ChatgptAccounts)
        .read()
        .unwrap()
        .unwrap();
    let outcome = sign_out_target_at_with(
        &state,
        &workspace,
        SignOutTarget {
            account_id: Some(id),
            expected_index: Some(&snapshot),
        },
        SignOutActions {
            read_keyring: |_: &AccountEntry| panic!("private account must not read keyring"),
            revoke: None::<fn(&VerifiedCredentials) -> Result<(), AuthorizationError>>,
            delete_keyring: |_: &str| panic!("private account must not delete keyring"),
        },
    )
    .unwrap();
    assert!(!outcome.remote_confirmed);
    assert!(outcome.local_cleared);
    assert!(AccountIndex::read(&state).unwrap().accounts[0].disconnected);
}

#[test]
fn private_file_renewal_is_selected_serialized_and_atomic() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let first = credentials("oaiapp_first", "subject-one");
    let first_id = save_private_file_at(
        &state,
        &workspace,
        first,
        accepted(&credentials("oaiapp_first", "subject-one")),
        None,
    )
    .unwrap();
    let second = credentials("oaiapp_second", "subject-two");
    let second_id = save_private_file_at(
        &state,
        &workspace,
        second,
        accepted(&credentials("oaiapp_second", "subject-two")),
        None,
    )
    .unwrap();
    let stale = credentials("oaiapp_first", "subject-one");
    assert!(matches!(
        save_private_file_at(
            &state,
            &workspace,
            stale.clone(),
            accepted(&stale),
            Some(first_id),
        ),
        Err(AuthorizationError::SelectedAccountChanged)
    ));
    assert_eq!(
        AccountIndex::read(&state).unwrap().selected,
        Some(second_id)
    );

    assert!(matches!(
        refresh_selected_with(&state, &workspace, first_id, 1_799_999_900, |_| {
            panic!("unselected account must not refresh")
        }),
        Err(AuthorizationError::InvalidIdentity)
    ));
    let current = refresh_selected_with(&state, &workspace, second_id, 1_799_999_000, |_| {
        panic!("fresh access token must not refresh")
    })
    .unwrap();
    assert_eq!(current.id, second_id);
    assert_eq!(current.storage, AccountStorage::PrivateFile);
    assert!(
        current
            .consent
            .matches(&current.credentials, current.storage)
    );
    assert_eq!(current.credentials.access_token, "synthetic-access");

    let other = StateRoot::open_existing(state.path()).unwrap();
    let renewed = refresh_selected_with(&state, &workspace, second_id, 1_799_999_900, |old| {
        assert_eq!(old.refresh_token, "synthetic-refresh");
        assert!(
            other
                .with_account_replacement_lock(&workspace, || ())
                .is_err()
        );
        let mut next = credentials("oaiapp_second", "subject-two");
        next.access_token = "new-access".into();
        next.refresh_token = "new-refresh".into();
        next.access_expires_at_unix = 1_800_003_600;
        Ok(next)
    })
    .unwrap();
    assert_eq!(renewed.id, second_id);
    assert!(
        renewed
            .consent
            .matches(&renewed.credentials, renewed.storage)
    );
    assert_eq!(renewed.credentials.access_token, "new-access");
    let reopened = StateRoot::open_existing(state.path()).unwrap();
    let reauthorize = || {
        let mut restored = credentials("oaiapp_second", "subject-two");
        restored.access_token = "new-access".into();
        restored.refresh_token = "new-refresh".into();
        restored.access_expires_at_unix = 1_800_003_600;
        let receipt = accepted(&restored);
        assert_eq!(
            save_private_file_at(&reopened, &workspace, restored, receipt, Some(second_id))
                .unwrap(),
            second_id
        );
    };
    let index = AccountIndex::read(&reopened).unwrap();
    assert_eq!(index.selected, Some(second_id));
    assert_eq!(index.accounts.len(), 2);
    assert_eq!(index.accounts[0].id, first_id);
    assert_eq!(
        index.accounts[0]
            .token
            .as_ref()
            .unwrap()
            .credentials
            .access_token,
        "synthetic-access"
    );
    assert_eq!(
        index.accounts[1]
            .token
            .as_ref()
            .unwrap()
            .credentials
            .refresh_token,
        "new-refresh"
    );

    assert!(matches!(
        refresh_selected_with(&reopened, &workspace, second_id, 1_800_003_500, |_| {
            Err(AuthorizationError::Unavailable)
        }),
        Err(AuthorizationError::Unavailable)
    ));
    assert_eq!(
        load_selected_private_file_at(&reopened, second_id)
            .unwrap()
            .refresh_token,
        "new-refresh"
    );

    assert!(matches!(
        refresh_selected_with(&reopened, &workspace, second_id, 1_800_003_500, |_| {
            let mut wrong = credentials("oaiapp_second", "other-subject");
            wrong.access_expires_at_unix = 1_800_004_000;
            Ok(wrong)
        }),
        Err(AuthorizationError::InvalidIdentity)
    ));
    assert!(matches!(
        load_selected_private_file_at(&reopened, second_id),
        Err(AuthorizationError::RenewalStorageUncertain)
    ));
    assert_eq!(
        AccountIndex::read(&reopened).unwrap().accounts[1]
            .token
            .as_ref()
            .unwrap()
            .credentials
            .refresh_token,
        "new-refresh"
    );
    reauthorize();

    for drift in ["reused_tokens", "id_hint"] {
        assert!(
            matches!(
                refresh_selected_with(&reopened, &workspace, second_id, 1_800_003_500, |_| {
                    let mut next = credentials("oaiapp_second", "subject-two");
                    next.access_expires_at_unix = 1_800_004_000;
                    if drift == "reused_tokens" {
                        next.access_token = "new-access".into();
                        next.refresh_token = "new-refresh".into();
                    } else {
                        next.id_token = "different-id-hint".into();
                    }
                    Ok(next)
                }),
                Err(AuthorizationError::InvalidIdentity)
            ),
            "{drift}"
        );
        assert!(matches!(
            load_selected_private_file_at(&reopened, second_id),
            Err(AuthorizationError::RenewalStorageUncertain)
        ));
        reauthorize();
    }

    let outside = temp.path().join("outside");
    std::fs::write(&outside, b"unchanged").unwrap();
    let uncertain = refresh_selected_with(&reopened, &workspace, second_id, 1_800_003_500, |_| {
        std::os::unix::fs::symlink(&outside, state.path().join("chatgpt-accounts.pending"))
            .unwrap();
        let mut next = credentials("oaiapp_second", "subject-two");
        next.access_token = "latest-access".into();
        next.refresh_token = "latest-refresh".into();
        next.access_expires_at_unix = 1_800_004_000;
        Ok(next)
    });
    assert!(
        matches!(&uncertain, Err(AuthorizationError::RenewalStorageUncertain)),
        "renewal outcome: {}",
        uncertain
            .err()
            .map(|error| error.to_string())
            .unwrap_or("success".into())
    );
    assert_eq!(std::fs::read(&outside).unwrap(), b"unchanged");
    std::fs::remove_file(state.path().join("chatgpt-accounts.pending")).unwrap();
    assert!(matches!(
        load_selected_private_file_at(&reopened, second_id),
        Err(AuthorizationError::RenewalStorageUncertain)
    ));
    let pending = AccountIndex::read(&reopened).unwrap();
    assert!(pending.accounts[1].renewal_pending);
    assert_eq!(
        pending.accounts[1]
            .token
            .as_ref()
            .unwrap()
            .credentials
            .refresh_token,
        "new-refresh"
    );

    assert!(matches!(
        state
            .with_account_replacement_lock(&workspace, || refresh_selected_with(
                &reopened,
                &workspace,
                second_id,
                1_800_003_500,
                |_| { panic!("busy account lock must refuse before exchange") }
            ))
            .unwrap(),
        Err(AuthorizationError::Unavailable)
    ));
}

#[tokio::test]
async fn async_renewal_owner_returns_fresh_private_file_token_without_network() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let mut account = credentials("oaiapp_async", "subject-one");
    account.access_expires_at_unix = u64::MAX;
    let receipt = accepted(&account);
    let id = save_private_file_at(&state, &workspace, account, receipt, None).unwrap();
    let result = refresh_selected_at(state, workspace, id).await.unwrap();
    assert_eq!(result.credentials.client_id, "oaiapp_async");
}

#[test]
fn private_file_accounts_reopen_and_reject_cross_account_drift() {
    let temp = tempfile::tempdir().expect("private root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let first = credentials("oaiapp_first", "subject-one");
    let first_receipt = accepted(&first);
    let first_id = save_private_file_at(&state, &workspace, first, first_receipt, None).unwrap();
    let second = credentials("oaiapp_second", "subject-two");
    let second_receipt = accepted(&second);
    let second_id = save_private_file_at(&state, &workspace, second, second_receipt, None).unwrap();
    assert_ne!(first_id, second_id);

    let reopened = StateRoot::open_existing(state.path()).unwrap();
    assert_eq!(
        load_selected_private_file_at(&reopened, second_id)
            .unwrap()
            .subject,
        "subject-two"
    );
    assert!(matches!(
        load_selected_private_file_at(&reopened, first_id),
        Err(AuthorizationError::InvalidIdentity)
    ));
    let changed = credentials("oaiapp_second", "subject-other");
    let changed_receipt = accepted(&changed);
    assert!(matches!(
        save_private_file_at(
            &reopened,
            &workspace,
            changed,
            changed_receipt,
            Some(second_id)
        ),
        Err(AuthorizationError::RegistrationConflict)
    ));
    assert_eq!(AccountIndex::read(&reopened).unwrap().accounts.len(), 2);

    let mut different_host = credentials("oaiapp_third", "subject-three");
    different_host.host_id = Uuid::parse_str("123e4567-e89b-42d3-a456-426614174001").unwrap();
    let different_host_receipt = accepted(&different_host);
    assert!(matches!(
        save_private_file_at(
            &reopened,
            &workspace,
            different_host,
            different_host_receipt,
            None,
        ),
        Err(AuthorizationError::RegistrationConflict)
    ));
    assert_eq!(AccountIndex::read(&reopened).unwrap().accounts.len(), 2);

    let replacement = credentials("oaiapp_second", "subject-two");
    let replacement_receipt = accepted(&replacement);
    assert_eq!(
        save_private_file_at(
            &reopened,
            &workspace,
            replacement,
            replacement_receipt,
            None
        )
        .unwrap(),
        second_id
    );
    assert_eq!(AccountIndex::read(&reopened).unwrap().accounts.len(), 2);

    let rejected = credentials("oaiapp_third", "subject-three");
    let wrong_backend_receipt = RiskPrompt::new(AccountStorage::Keyring)
        .accept("Accept")
        .unwrap()
        .bind(&rejected)
        .unwrap();
    assert!(matches!(
        save_private_file_at(&reopened, &workspace, rejected, wrong_backend_receipt, None),
        Err(AuthorizationError::ConsentRequired)
    ));
    assert_eq!(AccountIndex::read(&reopened).unwrap().accounts.len(), 2);

    let mut stale = serde_json::to_value(AccountIndex::read(&reopened).unwrap()).unwrap();
    stale["accounts"][1]["token"]["consent"]["warning_version"] = 1.into();
    reopened
        .record(RecordKind::ChatgptAccounts)
        .replace(&serde_json::to_vec(&stale).unwrap())
        .unwrap();
    assert!(matches!(
        load_selected_private_file_at(&reopened, second_id),
        Err(AuthorizationError::ConsentRequired)
    ));
    assert!(matches!(
        refresh_selected_with(&reopened, &workspace, second_id, 1_799_999_000, |_| {
            panic!("stale consent must not refresh")
        }),
        Err(AuthorizationError::ConsentRequired)
    ));
    let renewed_consent = credentials("oaiapp_second", "subject-two");
    let receipt = accepted(&renewed_consent);
    assert_eq!(
        save_private_file_at(&reopened, &workspace, renewed_consent, receipt, None).unwrap(),
        second_id
    );
    assert_eq!(
        load_selected_private_file_at(&reopened, second_id)
            .unwrap()
            .subject,
        "subject-two"
    );

    let selected = refresh_selected_with(&reopened, &workspace, second_id, 1_799_000_000, |_| {
        panic!("fresh token must not rotate")
    })
    .unwrap();
    record_model_check_at(
        &reopened,
        &workspace,
        &selected,
        "model-one",
        Effort::Medium,
        None,
        1_799_999_000,
    )
    .unwrap();
    let outcome = save_without_plan_permission_at(
        &reopened,
        &workspace,
        VerifiedIdentity {
            client_id: "oaiapp_second".into(),
            host_id: selected.credentials.host_id,
            subject: "subject-two".into(),
        },
        AccountStorage::PrivateFile,
        Some(second_id),
        |_| panic!("private disabled sign-in must not access keyring"),
    )
    .unwrap();
    assert_eq!(outcome.id, second_id);
    assert!(outcome.local_cleared);
    let disabled = AccountIndex::read(&reopened).unwrap();
    assert!(disabled.accounts[1].plan_permission_missing);
    assert!(disabled.accounts[1].token.is_none());
    assert!(disabled.model_checks.is_empty());
    assert_eq!(
        selected_registration_at(&reopened).unwrap(),
        (second_id, false)
    );
    assert!(
        selected_reauthorization_target_at(&reopened, second_id)
            .unwrap()
            .plan_permission_missing
    );
    assert!(matches!(
        load_selected_private_file_at(&reopened, second_id),
        Err(AuthorizationError::PermissionMissing)
    ));
    assert!(matches!(
        refresh_selected_with(&reopened, &workspace, second_id, 1_799_999_000, |_| {
            panic!("disabled sign-in must not refresh")
        }),
        Err(AuthorizationError::PermissionMissing)
    ));
    assert!(matches!(
        admitted_model_at(
            &reopened,
            &workspace,
            &selected,
            RunModel {
                slug: "model-one",
                effort: Effort::Medium,
                now: 1_799_999_010,
            },
            Ok(&[])
        ),
        Err(AuthorizationError::PermissionMissing)
    ));
    let before = reopened
        .record(RecordKind::ChatgptAccounts)
        .read()
        .unwrap()
        .unwrap();
    for (client, subject, host, storage, expected) in [
        (
            "oaiapp_second",
            "different-subject",
            selected.credentials.host_id,
            AccountStorage::PrivateFile,
            Some(second_id),
        ),
        (
            "oaiapp_second",
            "subject-two",
            selected.credentials.host_id,
            AccountStorage::Keyring,
            Some(second_id),
        ),
        (
            "oaiapp_second",
            "subject-two",
            Uuid::parse_str("123e4567-e89b-42d3-a456-426614174001").unwrap(),
            AccountStorage::PrivateFile,
            Some(second_id),
        ),
        (
            "oaiapp_first",
            "subject-one",
            selected.credentials.host_id,
            AccountStorage::PrivateFile,
            Some(second_id),
        ),
    ] {
        assert!(
            save_without_plan_permission_at(
                &reopened,
                &workspace,
                VerifiedIdentity {
                    client_id: client.into(),
                    host_id: host,
                    subject: subject.into()
                },
                storage,
                expected,
                |_| panic!("rejected disabled sign-in must not access keyring"),
            )
            .is_err()
        );
        assert!(
            before
                == reopened
                    .record(RecordKind::ChatgptAccounts)
                    .read()
                    .unwrap()
                    .unwrap(),
            "rejected identity replaced the account"
        );
    }
    let repaired = credentials("oaiapp_second", "subject-two");
    assert_eq!(
        save_private_file_at(
            &reopened,
            &workspace,
            repaired.clone(),
            accepted(&repaired),
            Some(second_id)
        )
        .unwrap(),
        second_id
    );
    let enabled = AccountIndex::read(&reopened).unwrap();
    assert!(!enabled.accounts[1].plan_permission_missing);
    assert!(enabled.model_checks.is_empty());
    assert_eq!(
        load_selected_private_file_at(&reopened, second_id)
            .unwrap()
            .subject,
        "subject-two"
    );
    let encoded = serde_json::to_value(&enabled).unwrap();
    assert_eq!(encoded["accounts"][1]["plan_permission_missing"], false);
    for field in [
        "signout_pending",
        "disconnected",
        "plan_permission_missing",
        "keyring_slot",
        "chatgpt_account_id",
        "token",
    ] {
        let mut missing = encoded.clone();
        missing["accounts"][1]
            .as_object_mut()
            .unwrap()
            .remove(field);
        assert!(serde_json::from_value::<AccountIndex>(missing).is_err());
    }
    let mut missing = encoded.clone();
    missing.as_object_mut().unwrap().remove("model_checks");
    assert!(serde_json::from_value::<AccountIndex>(missing).is_err());

    let valid = serde_json::to_value(AccountIndex::read(&reopened).unwrap()).unwrap();
    let mut contradictory = valid.clone();
    contradictory["accounts"][1]["plan_permission_missing"] = true.into();
    reopened
        .record(RecordKind::ChatgptAccounts)
        .replace(&serde_json::to_vec(&contradictory).unwrap())
        .unwrap();
    assert!(matches!(
        load_selected_private_file_at(&reopened, second_id),
        Err(AuthorizationError::InvalidSavedState)
    ));
    let mut wrong_host = valid.clone();
    wrong_host["accounts"][0]["host_id"] = "123e4567-e89b-42d3-a456-426614174001".into();
    reopened
        .record(RecordKind::ChatgptAccounts)
        .replace(&serde_json::to_vec(&wrong_host).unwrap())
        .unwrap();
    assert!(matches!(
        load_selected_private_file_at(&reopened, second_id),
        Err(AuthorizationError::InvalidSavedState)
    ));

    let mut malformed = valid;
    malformed["accounts"][0]["client_id"] = "oaiapp_second".into();
    reopened
        .record(RecordKind::ChatgptAccounts)
        .replace(&serde_json::to_vec(&malformed).unwrap())
        .unwrap();
    assert!(matches!(
        load_selected_private_file_at(&reopened, second_id),
        Err(AuthorizationError::InvalidSavedState)
    ));

    let metadata_state = StateRoot::admit(&temp.path().join("metadata-only")).unwrap();
    let identity = VerifiedIdentity {
        client_id: "oaiapp_disabled".into(),
        host_id: selected.credentials.host_id,
        subject: "verified-subject".into(),
    };
    let outcome = save_without_plan_permission_at(
        &metadata_state,
        &workspace,
        identity,
        AccountStorage::PrivateFile,
        None,
        |_| panic!("new metadata-only account must not access keyring"),
    )
    .unwrap();
    let metadata_reopened = StateRoot::open_existing(metadata_state.path()).unwrap();
    let record = metadata_reopened
        .record(RecordKind::ChatgptAccounts)
        .read()
        .unwrap()
        .unwrap();
    for omitted in [
        b"synthetic-access".as_slice(),
        b"synthetic-refresh",
        b"synthetic-id",
    ] {
        assert!(
            !record.windows(omitted.len()).any(|part| part == omitted),
            "metadata-only record contains a token canary"
        );
    }
    let index = AccountIndex::read(&metadata_reopened).unwrap();
    assert_eq!(index.accounts.len(), 1);
    assert_eq!(index.selected, Some(outcome.id));
    assert!(index.accounts[0].plan_permission_missing);
    assert!(index.accounts[0].token.is_none());
    assert!(index.model_checks.is_empty());
    assert!(outcome.local_cleared);
    assert!(matches!(
        load_selected_private_file_at(&metadata_reopened, outcome.id),
        Err(AuthorizationError::PermissionMissing)
    ));
}

fn record_model_check_at(
    state: &StateRoot,
    workspace: &Path,
    selected: &SelectedAccount,
    model: &str,
    effort: Effort,
    context_window_tokens: Option<u32>,
    now: u64,
) -> Result<(), AuthorizationError> {
    let (state, workspace, selected, model) = (
        state.path().to_owned(),
        workspace.to_owned(),
        selected.clone(),
        model.to_owned(),
    );
    std::thread::spawn(move || {
        let state = StateRoot::open_existing(&state).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut check = model_check_fixture(&state, &selected, (&model, effort)).await;
                let record = new_model_check(
                    &selected,
                    (&model, effort),
                    context_window_tokens,
                    check.id(),
                    now,
                )?;
                let digest = record.digest()?;
                check.evidence_started(digest).await.unwrap();
                publish_model_check_at(&state, &workspace, &selected, record)?;
                check.evidence_published(digest).await.unwrap();
                check.close().await.unwrap();
                Ok(())
            })
    })
    .join()
    .unwrap()
}

#[tokio::test]
async fn an_unfinished_attempt_suppresses_cached_metadata_without_revoking_account_admission() {
    let (_temp, state, workspace, selected) = model_account();
    let now = 1_799_999_000;
    record_model_check_at(
        &state,
        &workspace,
        &selected,
        "model-one",
        Effort::High,
        Some(272_000),
        now,
    )
    .unwrap();
    let index = AccountIndex::read(&state).unwrap();
    assert_eq!(
        checked_model_window(&state, &index, &selected, "model-one", now).unwrap(),
        Some(272_000)
    );
    let mut check = model_check_fixture(&state, &selected, ("model-one", Effort::High)).await;
    assert_eq!(
        checked_model_window(&state, &index, &selected, "model-one", now).unwrap(),
        None
    );
    let record = new_model_check(
        &selected,
        ("model-one", Effort::High),
        Some(320_000),
        check.id(),
        now,
    )
    .unwrap();
    let digest = record.digest().unwrap();
    let payload = serde_json::to_vec(&record).unwrap();
    check.evidence_started(digest).await.unwrap();
    let mut changed = selected.clone();
    changed.credentials.access_token = "changed-access".into();
    let refused = serde_json::from_slice(&payload).unwrap();
    assert!(matches!(
        publish_model_check_at(&state, &workspace, &changed, refused),
        Err(AuthorizationError::SelectedAccountChanged)
    ));
    publish_model_check_at(&state, &workspace, &selected, record).unwrap();
    assert_eq!(
        checked_model_window(
            &state,
            &AccountIndex::read(&state).unwrap(),
            &selected,
            "model-one",
            now
        )
        .unwrap(),
        None
    );
    check
        .finish(arany::CheckDisposition::Cancelled)
        .await
        .unwrap();
    let delayed = serde_json::from_slice(&payload).unwrap();
    assert!(publish_model_check_at(&state, &workspace, &selected, delayed).is_err());
    assert_eq!(
        check.close().await.unwrap().replay_disposition(),
        arany::CheckDisposition::Uncertain
    );
    let (_, window) = admitted_model_at(
        &state,
        &workspace,
        &selected,
        RunModel {
            slug: "model-one",
            effort: Effort::High,
            now,
        },
        Err(&AuthorizationError::Unavailable),
    )
    .unwrap();
    assert_eq!(window, None);
}

async fn model_check_fixture(
    state: &StateRoot,
    selected: &SelectedAccount,
    selection: (&str, Effort),
) -> arany::ChatGptCheck {
    let mut check = arany::ChatGptCheck::start(
        StateRoot::open_existing(state.path()).unwrap(),
        arany::ChatGptCheckRequest {
            account: selected.id,
            model: selection.0.into(),
            effort: selection.1,
            fingerprint: model_check_fingerprint(selected, selection.0, selection.1).unwrap(),
        },
        retained_checks,
        arany::RunCancellation::new(),
    )
    .await
    .unwrap();
    check.evidence_cleared().await.unwrap();
    let token = check.begin_catalog().await.unwrap();
    check
        .record_catalog(token, arany::CheckDisposition::Succeeded)
        .await
        .unwrap();
    arany::test_support::complete_chatgpt_probe_fixture(&mut check)
        .await
        .unwrap();
    check
}
