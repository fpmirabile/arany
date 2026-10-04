use super::*;
use crate::cli::chatgpt::consent::RiskPrompt;

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
            1_799_999_000,
        )
        .unwrap();
        let outcome = sign_out_at_with(
            &state,
            &workspace,
            |_, _, _| panic!("file account must not read keyring"),
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
                "model-one",
                Effort::Medium,
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
fn keyring_disconnection_blocks_use_even_when_deletion_fails() {
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
        schema: 1,
        credentials,
        consent,
    };
    let outcome = sign_out_at_with(
        &state,
        &workspace,
        |client, subject, host| {
            assert_eq!(client, token.credentials.client_id);
            assert_eq!(subject, token.credentials.subject);
            assert_eq!(host, token.credentials.host_id);
            Ok(token.clone())
        },
        |_| Ok(()),
        |slot| {
            assert_eq!(slot, keyring_slot("oaiapp_keyring_signout"));
            Err(AuthorizationError::Unavailable)
        },
    )
    .unwrap();
    assert!(outcome.remote_confirmed);
    assert!(!outcome.local_cleared);
    let index = AccountIndex::read(&state).unwrap();
    assert!(index.accounts[0].disconnected);
    assert!(index.model_checks.is_empty());
    assert!(matches!(
        load_selected_keyring_at(&state, id),
        Err(AuthorizationError::NoSelectedAccount)
    ));
    let cleanup = sign_out_at_with(
        &state,
        &workspace,
        |_, _, _| panic!("disconnected account must not load token"),
        |_| panic!("disconnected account must not claim remote revocation"),
        |_| Ok(()),
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
                assert_eq!(slot, keyring_slot("oaiapp_keyring_signout"));
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
        |_, _, _| {
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
    let record = reopened.read_chatgpt_accounts_record().unwrap().unwrap();
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
        schema: 1,
        selected: Some(id),
        accounts: vec![AccountEntry {
            id,
            host_id: credentials.host_id,
            client_id: credentials.client_id,
            subject: credentials.subject,
            storage: AccountStorage::Keyring,
            renewal_pending: true,
            signout_pending: false,
            disconnected: false,
            plan_permission_missing: false,
            token: None,
        }],
        model_checks: vec![ModelCheck {
            account_id: id,
            fingerprint: [7; 32],
            checked_at_sec: 1_799_999_000,
            expires_at_sec: 1_799_999_000 + MODEL_CHECK_AGE_SECONDS,
        }],
    }
    .write(&state)
    .unwrap();
    let mut index = AccountIndex::read(&state).unwrap();
    assert!(matches!(
        disconnect_unusable_refresh_with(&state, &mut index, 0, |slot| {
            assert_eq!(slot, keyring_slot("oaiapp_terminal_keyring"));
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
            if let Ok(entry) = keyring::Entry::new(crate::cli::credentials::SERVICE, &self.0) {
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
        let output = crate::cli::credentials::native_test_output(&mut command, input);
        assert!(output.stdout.len() <= MAX_TOKEN_RECORD_BYTES);
        assert!(output.stderr.is_empty(), "product helper emitted stderr");
        output
    }

    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state = StateRoot::admit(&temp.path().join("state")).expect("private account state");
    let client_id = format!("oaiapp_native_{}", Uuid::now_v7().simple());
    let slot = keyring_slot(&client_id);
    let _cleanup = Cleanup(slot.clone());
    let executable = std::env::var_os("ARANY_TEST_EXE").expect("set ARANY_TEST_EXE to built arany");
    let first = credentials(&client_id, "synthetic-subject");
    let consent = RiskPrompt::new(AccountStorage::Keyring)
        .accept("Accept")
        .expect("test risk acceptance")
        .bind(&first)
        .expect("test consent");
    let record = serde_json::to_vec(&TokenRecord {
        schema: 1,
        credentials: first.clone(),
        consent: consent.clone(),
    })
    .expect("synthetic token record");
    let id = save_keyring_at(&state, &workspace, first.clone(), consent, None)
        .expect("save synthetic subscription record");
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
        schema: 1,
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

#[test]
fn keyring_slot_and_consent_bind_token_to_its_registration() {
    let credentials = credentials("oaiapp_first", "subject-one");
    let consent = RiskPrompt::new(AccountStorage::Keyring)
        .accept("Accept")
        .unwrap()
        .bind(&credentials)
        .unwrap();
    let record = serde_json::to_vec(&TokenRecord {
        schema: 1,
        credentials,
        consent,
    })
    .unwrap();
    let slot = keyring_slot("oaiapp_first");
    assert!(valid_keyring_record(&slot, &record));
    assert!(!valid_keyring_record(
        &keyring_slot("oaiapp_other"),
        &record
    ));
    assert!(!valid_keyring_record(
        &slot,
        &vec![b'x'; MAX_TOKEN_RECORD_BYTES + 1]
    ));

    let mut changed: serde_json::Value = serde_json::from_slice(&record).unwrap();
    changed["credentials"]["subject"] = "other-subject".into();
    assert!(!valid_keyring_record(
        &slot,
        &serde_json::to_vec(&changed).unwrap()
    ));
}

#[test]
fn pending_keyring_renewal_blocks_load_before_os_store_access() {
    let temp = tempfile::tempdir().unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let id = Uuid::now_v7();
    AccountIndex {
        schema: 1,
        selected: Some(id),
        accounts: vec![AccountEntry {
            id,
            host_id: credentials("oaiapp_pending", "subject-one").host_id,
            client_id: "oaiapp_pending".into(),
            subject: "subject-one".into(),
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
        account_id: id,
        fingerprint: [7; 32],
        checked_at_sec: 1_800_000_000,
        expires_at_sec: 1_800_000_000 + MODEL_CHECK_AGE_SECONDS,
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
        Err(AuthorizationError::Unavailable)
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
    let admitted = admitted_model_at(&state, &workspace, &selected, "model-one", Effort::High)
        .expect("consented account needs no synthetic check");
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
            admitted_model_at(&state, &workspace, &selected, "model-one", Effort::High),
            Err(expected)
        );
    }
    baseline.write(&state).unwrap();
    assert!(matches!(
        admitted_model_at(&state, &workspace, &selected, "bad model", Effort::High),
        Err(AuthorizationError::InvalidSelection)
    ));
    record_model_check_at(
        &state,
        &workspace,
        &selected,
        "model-one",
        Effort::High,
        now,
    )
    .unwrap();
    let index = AccountIndex::read(&state).unwrap();
    let fingerprint = model_check_fingerprint(&selected, "model-one", Effort::High).unwrap();
    assert_ne!(admitted, fingerprint);
    assert!(model_check_valid(&index, id, fingerprint, now));
    assert_eq!(
        admitted_model_at(&state, &workspace, &selected, "model-one", Effort::High,).unwrap(),
        admitted
    );
    for (model, effort) in [("model-two", Effort::High), ("model-one", Effort::Low)] {
        let other = admitted_model_at(&state, &workspace, &selected, model, effort).unwrap();
        assert_ne!(admitted, other);
    }
    assert!(!model_check_valid(&index, id, fingerprint, now - 1));
    assert!(!model_check_valid(
        &index,
        id,
        fingerprint,
        now + MODEL_CHECK_AGE_SECONDS
    ));
    for (model, effort) in [("model-two", Effort::High), ("model-one", Effort::Low)] {
        let other = model_check_fingerprint(&selected, model, effort).unwrap();
        assert!(!model_check_valid(&index, id, other, now));
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
    assert!(model_check_valid(
        &AccountIndex::read(&state).unwrap(),
        id,
        fingerprint,
        1_799_999_900
    ));
    assert_eq!(
        admitted_model_at(&state, &workspace, &renewed, "model-one", Effort::High,).unwrap(),
        admitted
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
        admitted_model_at(&state, &workspace, &altered, "model-one", Effort::High),
        Err(AuthorizationError::InvalidIdentity)
    ));

    clear_model_check_at(&state, &workspace, &renewed, "model-one", Effort::High).unwrap();
    assert!(AccountIndex::read(&state).unwrap().model_checks.is_empty());
    assert_eq!(
        admitted_model_at(&state, &workspace, &renewed, "model-one", Effort::High).unwrap(),
        admitted
    );
    record_model_check_at(&state, &workspace, &renewed, "model-one", Effort::High, now).unwrap();
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
            1_799_000_000,
        ),
        Err(AuthorizationError::InvalidIdentity)
    ));
    assert!(AccountIndex::read(&state).unwrap().model_checks.is_empty());
    assert!(matches!(
        admitted_model_at(&state, &workspace, &selected, "model-one", Effort::High,),
        Err(AuthorizationError::InvalidIdentity)
    ));

    let mut stale = selected;
    stale.credentials.subject = "changed-subject".into();
    assert!(matches!(
        clear_model_check_at(&state, &workspace, &stale, "model-one", Effort::High),
        Err(AuthorizationError::ConsentRequired)
    ));
    assert!(matches!(
        admitted_model_at(&state, &workspace, &stale, "model-one", Effort::High,),
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
            account_id: id,
            fingerprint: [byte as u8; 32],
            checked_at_sec: now,
            expires_at_sec: now + MODEL_CHECK_AGE_SECONDS,
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
        save_private_file_at(&reopened, &workspace, changed, changed_receipt, None),
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
        .replace_chatgpt_accounts_record(&serde_json::to_vec(&stale).unwrap())
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
            "model-one",
            Effort::Medium,
        ),
        Err(AuthorizationError::PermissionMissing)
    ));
    let before = reopened.read_chatgpt_accounts_record().unwrap().unwrap();
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
            before == reopened.read_chatgpt_accounts_record().unwrap().unwrap(),
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
    assert!(
        encoded["accounts"][1]
            .get("plan_permission_missing")
            .is_none(),
        "enabled record must keep its old format"
    );

    let valid = serde_json::to_value(AccountIndex::read(&reopened).unwrap()).unwrap();
    let mut contradictory = valid.clone();
    contradictory["accounts"][1]["plan_permission_missing"] = true.into();
    reopened
        .replace_chatgpt_accounts_record(&serde_json::to_vec(&contradictory).unwrap())
        .unwrap();
    assert!(matches!(
        load_selected_private_file_at(&reopened, second_id),
        Err(AuthorizationError::InvalidIdentity)
    ));
    let mut wrong_host = valid.clone();
    wrong_host["accounts"][0]["host_id"] = "123e4567-e89b-42d3-a456-426614174001".into();
    reopened
        .replace_chatgpt_accounts_record(&serde_json::to_vec(&wrong_host).unwrap())
        .unwrap();
    assert!(matches!(
        load_selected_private_file_at(&reopened, second_id),
        Err(AuthorizationError::InvalidIdentity)
    ));

    let mut malformed = valid;
    malformed["accounts"][0]["client_id"] = "oaiapp_second".into();
    reopened
        .replace_chatgpt_accounts_record(&serde_json::to_vec(&malformed).unwrap())
        .unwrap();
    assert!(matches!(
        load_selected_private_file_at(&reopened, second_id),
        Err(AuthorizationError::InvalidIdentity)
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
        .read_chatgpt_accounts_record()
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
