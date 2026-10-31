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
    let other_account = Uuid::now_v7();
    index.accounts.push(AccountEntry {
        id: other_account,
        host_id: super::super::fixture_host_id(),
        client_id: super::super::CLIENT_ID.into(),
        subject: "same-subject".into(),
        chatgpt_account_id: "remote-other".into(),
        keyring_slot: None,
        storage: AccountStorage::PrivateFile,
        renewal_pending: false,
        signout_pending: false,
        disconnected: true,
        token: None,
    });
    index.selected = Some(other_account);
    index.write(&state).unwrap();
    let before = state
        .record(RecordKind::ChatgptAccounts)
        .read()
        .unwrap()
        .unwrap();
    let replacement = fixture("remote-one");
    let consent = receipt(&replacement, AccountStorage::PrivateFile);
    assert!(matches!(
        save_private_file_at(
            &state,
            &workspace,
            replacement,
            consent,
            Some(other_account)
        ),
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
}
