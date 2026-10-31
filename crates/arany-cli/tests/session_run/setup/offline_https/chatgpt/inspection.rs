use super::*;
use crate::setup::manager::{Presentation, acknowledge_result, assert_result_contains};
use crate::setup::{SetupPtyMode, setup_pty_command};
use std::{net::TcpListener, os::unix::net::UnixListener};

const STAGE_TEST: &str = "setup::offline_https::chatgpt::inspection::metadata_inspection_stage";

#[derive(Clone, Copy)]
enum Access {
    Connected,
    Disconnected,
    RenewalPending,
    SignOutPending,
}

impl Access {
    fn menu_title(self) -> &'static str {
        match self {
            Self::Connected | Self::RenewalPending => "ChatGPT account",
            Self::Disconnected | Self::SignOutPending => "ChatGPT account disconnected",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Connected => "connected; saved metadata only, not checked",
            Self::Disconnected => "unavailable; disconnected",
            Self::RenewalPending => "unavailable; renewal incomplete or uncertain",
            Self::SignOutPending => "unavailable; sign-out incomplete or uncertain",
        }
    }

    fn inspect_choice(self) -> usize {
        match self {
            Self::Connected | Self::RenewalPending => 4,
            Self::Disconnected | Self::SignOutPending => 3,
        }
    }
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "isolated Linux metadata inspection; requires OpenSSL, ip, unshare, bwrap, and script"
)]
fn manage_chatgpt_inspection_preserves_selection_and_defaults_without_access() {
    let _gate = crate::process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    run_isolated(STAGE_TEST, "inspection");
}

#[test]
#[ignore = "isolated helper; launched only by the metadata inspection journey"]
fn metadata_inspection_stage() {
    assert_eq!(std::env::var(STAGE).as_deref(), Ok("inspection"));
    assert_eq!(nix::unistd::geteuid().as_raw(), 0);
    assert_eq!(std::env::current_exe().unwrap(), Path::new("/test"));
    assert!(
        Command::new("/usr/bin/ip")
            .args(["link", "set", "lo", "up"])
            .status()
            .unwrap()
            .success()
    );
    let api = TcpListener::bind("127.0.0.1:443").unwrap();
    let issuer = TcpListener::bind("127.0.0.2:443").unwrap();
    api.set_nonblocking(true).unwrap();
    issuer.set_nonblocking(true).unwrap();
    for presentation in [Presentation::ScreenReader, Presentation::Inline] {
        for (storage, access) in [
            ("keyring", Access::Connected),
            ("private_file", Access::Connected),
            ("keyring", Access::Disconnected),
            ("keyring", Access::RenewalPending),
            ("keyring", Access::SignOutPending),
        ] {
            inspection_case(presentation, storage, access);
            assert!(
                api.accept()
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock),
                "inspection contacted the Provider"
            );
            assert!(
                issuer
                    .accept()
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock),
                "inspection contacted the issuer"
            );
        }
    }
}

fn seed_accounts(root: &StateRoot, storage: &str, access: Access) -> Uuid {
    let selected = Uuid::now_v7();
    let inspected = Uuid::now_v7();
    let host = "123e4567-e89b-42d3-a456-426614174000";
    let entry = |id: Uuid, client: &str| {
        let disconnected = matches!(access, Access::Disconnected);
        let token = (storage == "private_file" && !disconnected).then(|| {
            serde_json::json!({
                "slot": null,
                "credentials": {
                    "client_id": client, "host_id": host, "subject": "synthetic-subject",
                    "chatgpt_account_id": "synthetic-account", "id_token": ACCESS, "access_token": ACCESS,
                    "refresh_token": ACCESS, "access_expires_at_unix": 4_000_000_000_u64
                },
                "consent": {
                    "warning_version": 4, "warning_digest": ([0; 32].to_vec()), "storage": storage,
                    "client_id": client, "subject": "synthetic-subject", "host_id": host,
                    "accepted_at_sec": 1
                }
            })
        });
        let keyring_slot = (storage == "keyring").then(|| crate::setup::chatgpt_slot(client, id));
        serde_json::json!({
            "id": id, "host_id": host, "client_id": client, "subject": "synthetic-subject",
            "chatgpt_account_id": "synthetic-account", "keyring_slot": keyring_slot,
            "storage": storage, "renewal_pending": matches!(access, Access::RenewalPending),
            "signout_pending": matches!(access, Access::SignOutPending), "disconnected": disconnected,
            "token": token
        })
    };
    root.record(RecordKind::ChatgptAccounts).replace(&serde_json::to_vec(&serde_json::json!({
        "selected": selected,
        "accounts": [entry(selected, "oaiapp_selected_inspection"), entry(inspected, "oaiapp_other_inspection")]
    })).unwrap()).unwrap();
    root.record(RecordKind::ChatgptRegistration)
        .replace(
            &serde_json::to_vec(&serde_json::json!({
                "host_id": host
            }))
            .unwrap(),
        )
        .unwrap();
    inspected
}

fn seed_session(state: &Path, workspace: &Path, port: u16) -> arany::SessionId {
    crate::loopback::write_profile(state, port);
    runtime().block_on(async {
        let id = create_session(
            StateRoot::open_existing(state).unwrap(),
            workspace.to_owned(),
            Some("Inspection preserved".into()),
        )
        .await
        .unwrap();
        set_session_defaults(
            StateRoot::open_existing(state).unwrap(),
            workspace.to_owned(),
            id,
            SessionDefaults {
                provider: Some("custom:local".into()),
                model: Some("model-1".into()),
                effort: None,
                account_id: None,
                policy: arany::CollaborationPolicy::Single,
            },
        )
        .await
        .unwrap();
        id
    })
}

fn inspection_case(presentation: Presentation, storage: &str, access: Access) {
    let temp = tempfile::TempDir::new_in("/root").unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let account_root = StateRoot::admit(&temp.path().join("account-root")).unwrap();
    let inspected = seed_accounts(&account_root, storage, access);
    let bus_path = temp.path().join("bus");
    let bus = UnixListener::bind(&bus_path).unwrap();
    bus.set_nonblocking(true).unwrap();
    let provider = TcpListener::bind("127.0.0.1:0").unwrap();
    provider.set_nonblocking(true).unwrap();
    let id = seed_session(&state, &workspace, provider.local_addr().unwrap().port());
    let mut pty = start_inspection(&temp, &workspace, id, presentation);
    let before = session_snapshot(&state, id);
    let account_before = account_root
        .record(RecordKind::ChatgptAccounts)
        .read()
        .unwrap();
    let models_before = account_root
        .record(RecordKind::ModelPreferences)
        .read()
        .unwrap();
    perform_inspection(&mut pty, presentation, access, inspected, storage);
    assert_eq!(session_snapshot(&state, id), before);
    pty.send(b"/exit\r");
    let (result, transcript) = crate::setup::finish_setup_pty_with_transcript(pty);
    assert!(result.status.success());
    assert!(
        account_root
            .record(RecordKind::ChatgptAccounts)
            .read()
            .unwrap()
            == account_before,
        "inspection changed account selection or metadata"
    );
    assert_eq!(
        account_root
            .record(RecordKind::ModelPreferences)
            .read()
            .unwrap(),
        models_before
    );
    assert_eq!(session_snapshot(&state, id), before);
    assert!(
        bus.accept()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock),
        "inspection accessed credentials"
    );
    assert!(
        provider
            .accept()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock),
        "inspection called the conversation Provider"
    );
    assert!(
        !transcript
            .windows(ACCESS.len())
            .any(|bytes| bytes == ACCESS.as_bytes()),
        "token disclosed"
    );
}

fn perform_inspection(
    pty: &mut Pty,
    presentation: Presentation,
    access: Access,
    inspected: Uuid,
    storage: &str,
) {
    let from = pty.send(b"/setup\r");
    presentation.menu(pty, from, "Provider setup");
    let accounts = presentation.choose(pty, 1);
    presentation.menu(pty, accounts, "API accounts");
    let manage = presentation.choose(pty, 3);
    presentation.menu(pty, manage, access.menu_title());
    let picker = presentation.choose(pty, access.inspect_choice());
    presentation.menu(pty, picker, "Inspect ChatGPT account");
    let details = presentation.choose(pty, 2);
    let (returned, content) = acknowledge_result(pty, presentation, details);
    assert_details(&content, inspected, storage, access);
    presentation.menu(pty, returned, access.menu_title());
    let back = pty.send(match presentation {
        Presentation::ScreenReader => b"\x03",
        Presentation::Inline => b"\x1b",
    });
    presentation.composer(pty, back);
}

fn start_inspection(
    temp: &tempfile::TempDir,
    workspace: &Path,
    id: arany::SessionId,
    presentation: Presentation,
) -> Pty {
    let mode = match presentation {
        Presentation::ScreenReader => SetupPtyMode::Resumed(id),
        Presentation::Inline => SetupPtyMode::ResumedInline(id),
    };
    let state = temp.path().join("state");
    let mut pty = Pty::new(
        setup_pty_command(temp, workspace, &state, &mode, None)
            .executable("/arany")
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", temp.path().join("bus").display()),
            )
            .spawn(),
    );
    let product = product_child_of_executable(pty.child.child().id(), Path::new("/arany"));
    pty.guard_product(ProductGuard::new(product, state));
    presentation.composer(&mut pty, 0);
    pty
}

fn session_snapshot(state: &Path, id: arany::SessionId) -> (SessionDefaults, String, u64) {
    runtime().block_on(async {
        let store = Store::open_read_only(StateRoot::open_existing(state).unwrap()).unwrap();
        let view = store.load_view(id).await.unwrap().unwrap();
        let events = store.load_session(id).await.unwrap();
        assert!(view.runs.is_empty());
        assert!(view.pending_provider_setup_edit.is_none());
        assert!(!view.provider_setup_uncertain);
        let rendered = arany_cli::presentation::render_session(
            &view,
            &events,
            arany_cli::presentation::Output::Jsonl,
        );
        assert!(!rendered.contains(ACCESS));
        store.close().await.unwrap();
        (view.defaults, view.title, view.last_sequence)
    })
}

fn assert_details(content: &str, id: Uuid, storage: &str, access: Access) {
    for text in [
        "Provider: ChatGPT",
        &format!("Account: {id}"),
        "Billing: ChatGPT plan; no API-key billing",
        access.description(),
    ] {
        assert_result_contains(content, text);
    }
    assert_result_contains(
        content,
        if storage == "keyring" {
            "Backend: OS password store"
        } else {
            "Backend: private file (not encrypted)"
        },
    );
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
