use super::super::consent::RiskPrompt;
use super::*;
use std::cell::RefCell;
use std::collections::HashMap;

fn fixture(account_id: &str) -> VerifiedCredentials {
    VerifiedCredentials {
        client_id: super::super::CLIENT_ID.into(),
        host_id: super::super::fixture_host_id(),
        subject: "same-subject".into(),
        chatgpt_account_id: Some(account_id.into()),
        id_token: "synthetic-verified-id".into(),
        access_token: "synthetic-access".into(),
        refresh_token: "synthetic-refresh".into(),
        access_expires_at_unix: 1_900_000_000,
    }
}

fn receipt(credentials: &VerifiedCredentials, storage: AccountStorage) -> ConsentReceipt {
    RiskPrompt::new(storage)
        .accept("Accept")
        .unwrap()
        .bind(credentials)
        .unwrap()
}

fn root() -> (tempfile::TempDir, PathBuf, StateRoot) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    (temp, workspace, state)
}

#[test]
fn public_client_accounts_keep_separate_slots_and_logout_deletes_only_the_selected_slot() {
    for disconnected in [false, true] {
        let (_temp, workspace, state) = root();
        let records = RefCell::new(HashMap::new());
        let mut ids = Vec::new();
        for account_id in ["remote-one", "remote-two"] {
            let credentials = fixture(account_id);
            let consent = receipt(&credentials, AccountStorage::Keyring);
            let id = save_keyring_at_with(
                &state,
                &workspace,
                credentials,
                consent,
                None,
                |slot, record| {
                    records
                        .borrow_mut()
                        .insert(slot.to_owned(), record.as_bytes().to_vec());
                    Ok(())
                },
            )
            .unwrap();
            ids.push(id);
        }
        assert_ne!(ids[0], ids[1]);
        assert!(matches!(
            refuse_backend_route_at(&state, &workspace, ids[1]),
            Err(AuthorizationError::BackendRouteNotAdmitted)
        ));
        let mut index = AccountIndex::read(&state).unwrap();
        index.accounts[1].disconnected = disconnected;
        index.write(&state).unwrap();
        let first_slot = index.accounts[0].keyring_slot.clone().unwrap();
        let second_slot = index.accounts[1].keyring_slot.clone().unwrap();
        assert_ne!(first_slot, second_slot);
        let first_bytes = records.borrow()[&first_slot].clone();
        let result = sign_out_at_with(
            &state,
            &workspace,
            |entry| {
                admit_keyring_record(
                    entry,
                    &records.borrow()[entry.keyring_slot.as_ref().unwrap()],
                )
            },
            |_| Ok(()),
            |slot| {
                assert_eq!(slot, second_slot);
                records.borrow_mut().remove(slot);
                Ok(())
            },
        )
        .unwrap();
        assert!(result.remote_confirmed && result.local_cleared);
        assert!(records.borrow()[&first_slot] == first_bytes);
        assert!(!records.borrow().contains_key(&second_slot));
        let reopened = StateRoot::open_existing(state.path()).unwrap();
        let index = AccountIndex::read(&reopened).unwrap();
        assert!(!index.accounts[0].disconnected);
        assert!(index.accounts[1].disconnected);
    }
}

#[test]
fn migrated_file_token_is_retained_until_reauthorization_and_routes_refuse_before_loading() {
    let (_temp, workspace, state) = root();
    let credentials = fixture("remote-one");
    let consent = receipt(&credentials, AccountStorage::PrivateFile);
    let id = save_private_file_at(&state, &workspace, credentials, consent, None).unwrap();
    let current = state
        .record(RecordKind::ChatgptAccounts)
        .read()
        .unwrap()
        .unwrap();
    let mut legacy: serde_json::Value = serde_json::from_slice(&current).unwrap();
    legacy["schema"] = 3.into();
    let entry = &mut legacy["accounts"][0];
    entry["client_id"] = "oaiapp_legacy".into();
    entry["chatgpt_account_id"] = serde_json::Value::Null;
    entry["plan_permission_missing"] = false.into();
    entry.as_object_mut().unwrap().remove("slot_client_id");
    entry["token"]["credentials"]["client_id"] = "oaiapp_legacy".into();
    entry["token"]["credentials"]
        .as_object_mut()
        .unwrap()
        .remove("chatgpt_account_id");
    let token = entry["token"].clone();
    state
        .record(RecordKind::ChatgptAccounts)
        .replace(&serde_json::to_vec(&legacy).unwrap())
        .unwrap();
    let reopened = StateRoot::open_existing(state.path()).unwrap();
    let index = AccountIndex::read(&reopened).unwrap();
    assert!(index.accounts[0].disconnected);
    assert!(serde_json::to_value(index.accounts[0].token.as_ref().unwrap()).unwrap() == token);
    assert!(matches!(
        load_selected_private_file_at(&reopened, id),
        Err(AuthorizationError::NoSelectedAccount)
    ));
    let replacement = fixture("remote-one");
    let consent = receipt(&replacement, AccountStorage::PrivateFile);
    assert_eq!(
        save_private_file_at(&reopened, &workspace, replacement, consent, Some(id)).unwrap(),
        id
    );
    assert!(matches!(
        refuse_backend_route_at(&reopened, &workspace, id),
        Err(AuthorizationError::BackendRouteNotAdmitted)
    ));
    let index = AccountIndex::read(&reopened).unwrap();
    assert!(!index.accounts[0].renewal_pending);
    assert_eq!(
        index.accounts[0].chatgpt_account_id.as_deref(),
        Some("remote-one")
    );
}

#[test]
fn reauthorization_rejects_account_drift_and_duplicate_identity_without_writes() {
    let (_temp, workspace, state) = root();
    let credentials = fixture("remote-one");
    let consent = receipt(&credentials, AccountStorage::PrivateFile);
    let id = save_private_file_at(&state, &workspace, credentials, consent, None).unwrap();
    let before = state
        .record(RecordKind::ChatgptAccounts)
        .read()
        .unwrap()
        .unwrap();
    let changed = fixture("remote-two");
    let consent = receipt(&changed, AccountStorage::PrivateFile);
    assert!(matches!(
        save_private_file_at(&state, &workspace, changed, consent, Some(id)),
        Err(AuthorizationError::RegistrationConflict)
    ));
    assert!(
        state
            .record(RecordKind::ChatgptAccounts)
            .read()
            .unwrap()
            .unwrap()
            == before
    );
    let mut index = AccountIndex::read(&state).unwrap();
    let legacy_id = Uuid::now_v7();
    index.accounts.push(AccountEntry {
        id: legacy_id,
        host_id: super::super::fixture_host_id(),
        client_id: "oaiapp_legacy".into(),
        subject: "same-subject".into(),
        chatgpt_account_id: None,
        keyring_slot: None,
        slot_client_id: None,
        storage: AccountStorage::PrivateFile,
        renewal_pending: false,
        signout_pending: false,
        disconnected: true,
        token: None,
    });
    index.selected = Some(legacy_id);
    index.write(&state).unwrap();
    let before = state
        .record(RecordKind::ChatgptAccounts)
        .read()
        .unwrap()
        .unwrap();
    let replacement = fixture("remote-one");
    let consent = receipt(&replacement, AccountStorage::PrivateFile);
    assert!(matches!(
        save_private_file_at(&state, &workspace, replacement, consent, Some(legacy_id)),
        Err(AuthorizationError::RegistrationConflict)
    ));
    assert!(
        state
            .record(RecordKind::ChatgptAccounts)
            .read()
            .unwrap()
            .unwrap()
            == before
    );
    let removed = sign_out_target_at_with(
        &state,
        &workspace,
        SignOutTarget {
            account_id: Some(id),
            expected_index: Some(&before),
        },
        SignOutActions {
            read_keyring: |_: &AccountEntry| panic!("file removal must not read the keyring"),
            revoke: None::<fn(&VerifiedCredentials) -> Result<(), AuthorizationError>>,
            delete_keyring: |_: &str| panic!("file removal must not delete a keyring item"),
        },
    )
    .unwrap();
    assert!(removed.local_cleared);
    let replacement = fixture("remote-one");
    let consent = receipt(&replacement, AccountStorage::PrivateFile);
    assert_eq!(
        save_private_file_at(&state, &workspace, replacement, consent, Some(legacy_id)).unwrap(),
        legacy_id
    );
}

#[test]
fn reauthorization_keeps_the_original_keyring_slot_after_the_client_changes() {
    let (_temp, workspace, state) = root();
    let legacy_client = "oaiapp_legacy_slot";
    let id = Uuid::now_v7();
    let slot = legacy_keyring_slot(legacy_client);
    let source = serde_json::json!({
        "schema": 3, "selected": id, "model_checks": [],
        "accounts": [{
            "id": id, "host_id": super::super::fixture_host_id(),
            "client_id": legacy_client, "subject": "same-subject",
            "chatgpt_account_id": null, "keyring_slot": slot,
            "storage": "keyring", "renewal_pending": false,
            "signout_pending": true, "disconnected": false,
            "plan_permission_missing": false, "token": null
        }]
    });
    state
        .record(RecordKind::ChatgptAccounts)
        .replace(&serde_json::to_vec(&source).unwrap())
        .unwrap();
    let migrated = AccountIndex::read(&state).unwrap();
    assert!(migrated.accounts[0].disconnected);
    assert!(migrated.accounts[0].signout_pending);
    assert_eq!(
        migrated.accounts[0].keyring_slot.as_deref(),
        Some(slot.as_str())
    );
    let replacement = fixture("remote-one");
    let consent = receipt(&replacement, AccountStorage::Keyring);
    save_keyring_at_with(
        &state,
        &workspace,
        replacement,
        consent,
        Some(id),
        |addressed, bytes| {
            assert_eq!(addressed, slot);
            assert!(valid_written_keyring_record(addressed, bytes.as_bytes()));
            assert!(AccountIndex::read(&state).unwrap().accounts[0].renewal_pending);
            Ok(())
        },
    )
    .unwrap();
    let reopened = StateRoot::open_existing(state.path()).unwrap();
    let index = AccountIndex::read(&reopened).unwrap();
    assert_eq!(
        index.accounts[0].slot_client_id.as_deref(),
        Some(legacy_client)
    );
    assert_eq!(index.accounts[0].client_id, super::super::CLIENT_ID);
    assert_eq!(
        index.accounts[0].keyring_slot.as_deref(),
        Some(slot.as_str())
    );
    assert!(!index.accounts[0].signout_pending);
    assert!(!index.accounts[0].renewal_pending);
}

#[test]
fn profile_migration_preserves_pending_renewal_and_signout_markers() {
    for (disconnected, renewal_pending, signout_pending) in [
        (false, false, false),
        (false, true, false),
        (false, false, true),
        (false, true, true),
        (true, false, true),
    ] {
        let id = Uuid::now_v7();
        let client = "oaiapp_pending_migration";
        let slot = legacy_keyring_slot(client);
        let source = serde_json::json!({
            "schema": 3, "selected": id, "model_checks": [],
            "accounts": [{
                "id": id, "host_id": super::super::fixture_host_id(),
                "client_id": client, "subject": "same-subject",
                "chatgpt_account_id": null, "keyring_slot": slot,
                "storage": "keyring", "renewal_pending": renewal_pending,
                "signout_pending": signout_pending, "disconnected": disconnected,
                "plan_permission_missing": false, "token": null
            }]
        });
        let upgraded = CHATGPT_ACCOUNTS.upgrade(&serde_json::to_vec(&source).unwrap());
        if signout_pending && (renewal_pending || disconnected) {
            assert!(upgraded.is_none());
            continue;
        }
        let index = AccountIndex::decode(&upgraded.unwrap()).unwrap();
        let entry = &index.accounts[0];
        assert!(entry.disconnected);
        assert_eq!(entry.renewal_pending, renewal_pending);
        assert_eq!(entry.signout_pending, signout_pending);
        assert_eq!(entry.keyring_slot.as_deref(), Some(slot.as_str()));
        assert!(entry.ensure_usable().is_err());
    }
}
