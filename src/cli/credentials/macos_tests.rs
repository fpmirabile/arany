use super::*;
use security_framework::{
    item::{ItemClass, ItemSearchOptions},
    os::macos::keychain::{CreateOptions, SecKeychain},
};
use std::{os::unix::fs::PermissionsExt, process::Command};

const PASSWORD: &str = "synthetic-keychain-password";
const TEST: &str =
    "cli::credentials::tests::native_store_round_trips_an_isolated_synthetic_account";

fn preferences() -> [Vec<u8>; 2] {
    ["list-keychains", "default-keychain"].map(|operation| {
        let output = native_test_output(
            Command::new("/usr/bin/security")
                .args([operation, "-d", "user"])
                .env_clear(),
            None,
        );
        assert!(
            output.status.success(),
            "read-only Keychain preference observation"
        );
        output.stdout
    })
}

pub(super) async fn roundtrip() {
    if let Some(root) = std::env::var_os("ARANY_TEST_KEYCHAIN_ROOT") {
        let root = StateRoot::open_existing(Path::new(&root)).unwrap();
        let _interaction = SecKeychain::disable_user_interaction().unwrap();
        let mut keychain = SecKeychain::open(root.path().join("arany-test.keychain-db")).unwrap();
        let workspace = root.path().parent().unwrap().join("project");
        std::fs::create_dir(&workspace).unwrap();
        let account_root = root.path().parent().unwrap().join("account");
        let account = SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            Some(Effort::Low),
            "synthetic-native-key".into(),
        )
        .unwrap();
        let id = account.id;
        save_at(DEFAULT_ACCOUNT, &account_root, &workspace, account)
            .await
            .unwrap();
        let loaded = load_at(DEFAULT_ACCOUNT).await.unwrap().unwrap();
        assert_eq!(loaded.id, id);
        assert!(
            loaded.api_key() == "synthetic-native-key",
            "native save must reach the isolated store"
        );
        let state = StateRoot::open_existing(&account_root).unwrap();
        let marker = state.read_saved_account_record().unwrap().unwrap();
        assert_eq!(
            StoredAccountFile::parse(&marker).unwrap().storage,
            AccountStorage::Keyring
        );
        assert!(
            !marker
                .windows(b"synthetic-native-key".len())
                .any(|part| part == b"synthetic-native-key")
        );
        assert!(
            keyring_helper::authorize(DEFAULT_ACCOUNT.into(), Some((id, "openai".into())))
                .await
                .unwrap()
        );
        assert!(matches!(
            keyring_helper::authorize(
                DEFAULT_ACCOUNT.into(),
                Some((Uuid::now_v7(), "openai".into()))
            )
            .await,
            Err(CredentialError::InvalidAccount)
        ));
        let replacement = SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            Some(Effort::Low),
            "synthetic-replacement-key".into(),
        )
        .unwrap();
        let replacement_id = replacement.id;
        save_at(DEFAULT_ACCOUNT, &account_root, &workspace, replacement)
            .await
            .unwrap();
        let loaded = load_at(DEFAULT_ACCOUNT).await.unwrap().unwrap();
        assert_eq!(loaded.id, replacement_id);
        assert!(
            loaded.api_key() == "synthetic-replacement-key",
            "native replacement must reach the isolated store"
        );
        assert!(matches!(
            keyring_helper::authorize(DEFAULT_ACCOUNT.into(), Some((id, "openai".into()))).await,
            Err(CredentialError::InvalidAccount)
        ));

        let output = native_test_output(
            Command::new("/usr/bin/security")
                .arg("lock-keychain")
                .arg(root.path().join("arany-test.keychain-db"))
                .env_clear(),
            None,
        );
        assert!(output.status.success(), "lock only the synthetic Keychain");
        assert!(matches!(
            load_at(DEFAULT_ACCOUNT).await,
            Err(CredentialError::Locked)
        ));
        let rejected = SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            Some(Effort::Low),
            "rejected-locked-key".into(),
        )
        .unwrap();
        assert!(matches!(
            save_at(DEFAULT_ACCOUNT, &account_root, &workspace, rejected).await,
            Err(CredentialError::Locked)
        ));
        assert_eq!(state.read_saved_account_record().unwrap(), Some(marker));
        keychain.unlock(Some(PASSWORD)).unwrap();
        let loaded = load_at(DEFAULT_ACCOUNT).await.unwrap().unwrap();
        assert_eq!(loaded.id, replacement_id);
        assert!(
            loaded.api_key() == "synthetic-replacement-key",
            "locked replacement must preserve the original item"
        );

        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        use sha2::{Digest, Sha256};
        let credentials = crate::cli::chatgpt::VerifiedCredentials {
            client_id: "oaiapp_native_synthetic".into(),
            host_id: Uuid::parse_str("123e4567-e89b-42d3-a456-426614174000").unwrap(),
            subject: "synthetic-subject".into(),
            id_token: "synthetic-id".into(),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at_unix: 1_900_000_000,
        };
        let consent = crate::cli::chatgpt::RiskPrompt::new(AccountStorage::Keyring)
            .accept("Accept")
            .unwrap()
            .bind(&credentials)
            .unwrap();
        let slot = format!(
            "chatgpt-{}",
            URL_SAFE_NO_PAD.encode(Sha256::digest(credentials.client_id.as_bytes()))
        );
        let record = serde_json::to_string(
            &serde_json::json!({"schema":1,"credentials":credentials,"consent":consent}),
        )
        .unwrap();
        keyring_helper::write(&slot, &record).unwrap();
        assert!(
            keyring_helper::read(&slot)
                .unwrap()
                .is_some_and(|bytes| bytes == record.as_bytes()),
            "synthetic token save/read"
        );
        keyring_helper::delete(&slot).unwrap();
        assert!(keyring_helper::read(&slot).unwrap().is_none());
        let denied = ItemSearchOptions::new()
            .keychains(&[keychain])
            .class(ItemClass::generic_password())
            .service(SERVICE)
            .account(DEFAULT_ACCOUNT)
            .delete();
        assert!(
            denied.is_err_and(|error| matches!(error.code(), -25244 | -25308)),
            "another test executable must not delete the item's native authorization"
        );
        assert_eq!(
            load_at(DEFAULT_ACCOUNT).await.unwrap().unwrap().id,
            replacement_id
        );
        return;
    }
    let executable =
        std::env::var_os("ARANY_TEST_EXE").expect("set ARANY_TEST_EXE to built debug arany");
    assert!(Path::new(&executable).is_absolute());
    let before = preferences();
    let temp = tempfile::tempdir().unwrap();
    let root = StateRoot::admit(&temp.path().join("keychain")).unwrap();
    let path = root.path().join("arany-test.keychain-db");
    let _interaction = SecKeychain::disable_user_interaction().unwrap();
    let _keychain = CreateOptions::new()
        .password(PASSWORD)
        .prompt_user(false)
        .create(&path)
        .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        preferences(),
        before,
        "private creation must preserve user Keychain preferences"
    );
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", TEST, "--ignored", "--nocapture"])
        .env_clear()
        .env("ARANY_TEST_EXE", executable)
        .env("ARANY_TEST_KEYCHAIN_ROOT", root.path())
        .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account"))
        .env("HOME", temp.path().join("legacy-home"))
        .env("XDG_STATE_HOME", temp.path().join("legacy-state"))
        .env("XDG_DATA_HOME", temp.path().join("legacy-data"))
        .env("TMPDIR", "/private/tmp")
        .current_dir(temp.path());
    let output = native_test_output(&mut command, None);
    assert_eq!(
        preferences(),
        before,
        "native lifecycle must preserve user Keychain preferences"
    );
    for secret in [
        b"synthetic-native-key".as_slice(),
        b"synthetic-replacement-key".as_slice(),
        b"synthetic-access".as_slice(),
        b"synthetic-refresh".as_slice(),
        PASSWORD.as_bytes(),
    ] {
        assert!(
            !output
                .stdout
                .windows(secret.len())
                .any(|part| part == secret)
                && !output
                    .stderr
                    .windows(secret.len())
                    .any(|part| part == secret),
            "native secrets must stay in bounded helper pipes"
        );
    }
    assert!(
        output.status.success(),
        "isolated native Keychain child: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output
            .stdout
            .windows(b"test result: ok. 1 passed; 0 failed; 0 ignored;".len())
            .any(|part| part == b"test result: ok. 1 passed; 0 failed; 0 ignored;"),
        "native child must complete exactly one test"
    );
}
