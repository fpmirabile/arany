use super::config::Config;
use crate::tools::skills::parse_json;
use crate::tools::tests::config_value;
use crate::tools::types::*;
use serde_json::json;

#[test]
fn tool_admission_corpus_is_closed_and_restrict_only() {
    let value = config_value();
    let config: Config = serde_json::from_value(value.clone()).unwrap();
    config.validate().unwrap();
    assert!(config.receipt().valid());
    assert_eq!(config.child_unguarded, UnguardedGrant::Denied);
    let mut widened = value.clone();
    widened["child_unguarded"] = json!("human_only");
    assert!(
        serde_json::from_value::<Config>(widened)
            .unwrap()
            .validate()
            .is_err()
    );
    for invalid in [json!(true), json!(null), json!("automatic")] {
        let mut row = value.clone();
        row["child_unguarded"] = invalid;
        assert!(serde_json::from_value::<Config>(row).is_err());
    }
    for roots in [
        json!([]),
        json!(["src", "src/child"]),
        json!(["src", "src"]),
        json!([".", "src"]),
        json!(["../outside"]),
        json!(["src/.env"]),
        json!(["/absolute"]),
    ] {
        let mut row = value.clone();
        row["workspace_paths"] = roots;
        assert!(
            serde_json::from_value::<Config>(row)
                .unwrap()
                .validate()
                .is_err()
        );
    }
    let mut root_value = value.clone();
    root_value["workspace_paths"] = json!(["."]);
    let root_config: Config = serde_json::from_value(root_value.clone()).unwrap();
    root_config.validate().unwrap();
    assert!(root_config.allows(&ToolCall::List { path: ".".into() }));
    assert!(!root_config.allows(&ToolCall::Mkdir { path: ".".into() }));
    for (path, expected) in [
        ("README.md", true),
        ("nested/source.rs", true),
        (".git/config", false),
        (".env.local", false),
        (".ENV", false),
        (".Env.LOCAL", false),
        ("../outside", false),
    ] {
        assert_eq!(
            root_config.allows(&ToolCall::Read {
                path: path.into(),
                offset: None,
                limit: Some(1)
            }),
            expected,
            "root grant: {path}"
        );
    }
    root_value["version"] = json!(2);
    assert!(
        serde_json::from_value::<Config>(root_value)
            .unwrap()
            .validate()
            .is_err()
    );
    let duplicate = br#"{"version":1,"version":1,"workspace_paths":["src"],"write":false,"commands":[],"skills":[],"mcp":[]}"#;
    assert!(parse_json(duplicate, 64 * 1024).is_err());
    let mut unknown = value.clone();
    unknown["network"] = json!(true);
    assert!(serde_json::from_value::<Config>(unknown).is_err());
    for (path, expected) in [
        ("src/main.rs", true),
        ("src2/main.rs", false),
        ("src/../omitted", false),
        ("src/.git/config", false),
        ("src/.env.local", false),
        ("src/.Env", false),
        ("src/line\nbreak", false),
    ] {
        assert_eq!(
            config.allows(&ToolCall::Read {
                path: path.into(),
                offset: None,
                limit: Some(1)
            }),
            expected,
            "path case"
        );
    }
    assert!(!config.allows(&ToolCall::Write {
        path: "src/new".into(),
        expected_digest: None,
        content: "new".into()
    }));
    assert!(!config.allows(&ToolCall::Command {
        program: "unconfigured".into(),
        args: Vec::new(),
        cwd: "".into(),
        resource_class: None,
        timeout_ms: None,
    }));
    assert!(!config.allows(&ToolCall::Skill {
        name: "unconfigured".into(),
        resource: None
    }));
    assert!(!config.allows(&ToolCall::McpList {
        server: "unconfigured".into()
    }));
    let mut duplicate_names = value;
    duplicate_names["mcp"] = json!([{"name":"example","program":{"name":"python","executable":"/usr/bin/python","sha256":"0".repeat(64),"interpreter":true,"inputs":[]},"args":[],"tools":["echo","echo"]}]);
    assert!(
        serde_json::from_value::<Config>(duplicate_names)
            .unwrap()
            .validate()
            .is_err()
    );
}

#[test]
fn host_runtime_roots_can_only_come_from_remembered_folder_trust() {
    use crate::store::StateRoot;
    use crate::tools::toolchains::{RuntimeKind, discover};
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let node = home.join(".nvm/versions/node/v22.3.0");
    std::fs::create_dir_all(node.join("bin")).unwrap();
    std::fs::write(node.join("bin/node"), b"synthetic executable").unwrap();
    std::fs::set_permissions(
        node.join("bin/node"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let grant = discover(&home)
        .unwrap()
        .into_iter()
        .find(|candidate| candidate.grant.kind == RuntimeKind::Node)
        .unwrap()
        .grant;
    let mut value = config_value();
    value["runtime_grants"] = json!([grant]);
    let config: Config = serde_json::from_value(value.clone()).unwrap();
    config.validate().unwrap();

    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    let path = state.path().join("tools.json");
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(Config::load(&state).is_err());
}

#[test]
fn network_grant_is_closed_and_pinned() {
    let base = config_value();
    let mut value = base.clone();
    value["network"] = json!({"allow": ["example.com", "*.pypi.org", "127.0.0.1"]});
    let bytes = serde_json::to_vec(&value).unwrap();
    let parsed = Config::parse(&bytes);
    assert!(parsed.is_ok());
    let config: Config = serde_json::from_value(value.clone()).unwrap();
    assert_ne!(
        config.digest(),
        serde_json::from_value::<Config>(base.clone())
            .unwrap()
            .digest()
    );
    for invalid in [
        json!({"allow": ["*.com"]}),
        json!({"allow": ["10.0.0.1"]}),
        json!({"allow": ["example.com:443"]}),
        json!({"allow": ["example.com", "example.com"]}),
        json!({"allow": [], "deny": ["example.com"]}),
        json!({"allow": (0..=32).map(|index| format!("h{index}.example.com")).collect::<Vec<_>>()}),
    ] {
        let mut row = base.clone();
        row["network"] = invalid;
        assert!(
            Config::parse(&serde_json::to_vec(&row).unwrap()).is_err(),
            "{row}"
        );
    }
    let mut unknown = base.clone();
    unknown["network"] = json!({"allow": [], "ports": [80]});
    assert!(Config::parse(&serde_json::to_vec(&unknown).unwrap()).is_err());
    let mut absent = base;
    absent["network"] = json!(null);
    assert!(
        Config::parse(&serde_json::to_vec(&absent).unwrap())
            .unwrap()
            .network
            .is_none()
    );
}
