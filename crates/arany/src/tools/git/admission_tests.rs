use crate::tools::config::Config;
use crate::tools::tests::config_value;
use crate::tools::types::{ToolCall, model_call};
use crate::validate::{MAX_REVISION_BYTES, valid_revision};
use serde_json::{Value, json};

#[test]
fn revision_grammar_admits_names_and_ancestry_only() {
    let long = "a".repeat(128);
    for value in [
        "HEAD",
        "main",
        "feature/x-1",
        "v1.2.3",
        "HEAD~2",
        "abc1234^",
        "origin/main~1^2",
        "main~",
        long.as_str(),
    ] {
        assert!(valid_revision(value), "{value}");
    }
    let too_long = "a".repeat(129);
    for value in [
        "",
        "-p",
        "--output=x",
        "HEAD:src",
        "@{-1}",
        "main..dev",
        "a b",
        "refs/.hidden",
        "x.lock",
        "a/../b",
        "HEAD^{tree}",
        "~1",
        "main~12345",
        "main~1~1~1~1~1",
        too_long.as_str(),
        "feature/",
        "a//b",
    ] {
        assert!(!valid_revision(value), "{value}");
    }
}

#[test]
fn git_read_calls_validate_paths_combinations_and_counts() {
    let valid = [
        ToolCall::GitStatus { path: ".".into() },
        ToolCall::GitStatus {
            path: String::new(),
        },
        ToolCall::GitDiff {
            path: "src".into(),
            base: None,
            head: None,
            staged: false,
        },
        ToolCall::GitDiff {
            path: "src".into(),
            base: None,
            head: None,
            staged: true,
        },
        ToolCall::GitDiff {
            path: ".".into(),
            base: Some("HEAD~1".into()),
            head: Some("HEAD".into()),
            staged: false,
        },
        ToolCall::GitDiff {
            path: ".".into(),
            base: Some("main".into()),
            head: None,
            staged: true,
        },
        ToolCall::GitLog {
            path: ".".into(),
            revision: None,
            max_count: 1,
        },
        ToolCall::GitLog {
            path: ".".into(),
            revision: Some("main".into()),
            max_count: 50,
        },
        ToolCall::GitLog {
            path: ".".into(),
            revision: Some("release..feature/topic".into()),
            max_count: 50,
        },
    ];
    for call in &valid {
        assert!(call.valid(), "{call:?}");
        assert!(call.is_goal_plan_prelude() && call.is_git() && !call.mutates());
        assert!(!call.is_process() && !call.is_child_research());
    }
    assert_eq!(valid[0].label(), "git_status");
    assert_eq!(valid[2].label(), "git_diff");
    assert_eq!(valid[6].label(), "git_log");
    let invalid = [
        ToolCall::GitStatus {
            path: ".git".into(),
        },
        ToolCall::GitStatus {
            path: ".env".into(),
        },
        ToolCall::GitStatus {
            path: "../x".into(),
        },
        ToolCall::GitDiff {
            path: ".".into(),
            base: None,
            head: Some("HEAD".into()),
            staged: false,
        },
        ToolCall::GitDiff {
            path: ".".into(),
            base: Some("a".into()),
            head: Some("b".into()),
            staged: true,
        },
        ToolCall::GitDiff {
            path: ".".into(),
            base: Some("-p".into()),
            head: None,
            staged: false,
        },
        ToolCall::GitLog {
            path: ".".into(),
            revision: None,
            max_count: 0,
        },
        ToolCall::GitLog {
            path: ".".into(),
            revision: None,
            max_count: 51,
        },
        ToolCall::GitLog {
            path: ".".into(),
            revision: Some("..HEAD".into()),
            max_count: 5,
        },
        ToolCall::GitLog {
            path: ".".into(),
            revision: Some("HEAD..".into()),
            max_count: 5,
        },
        ToolCall::GitLog {
            path: ".".into(),
            revision: Some("HEAD...main".into()),
            max_count: 5,
        },
        ToolCall::GitLog {
            path: ".".into(),
            revision: Some("main..topic..other".into()),
            max_count: 5,
        },
        ToolCall::GitLog {
            path: ".".into(),
            revision: Some(format!(
                "{}..{}",
                "a".repeat(MAX_REVISION_BYTES),
                "b".repeat(MAX_REVISION_BYTES) + "b"
            )),
            max_count: 5,
        },
    ];
    for call in &invalid {
        assert!(!call.valid(), "{call:?}");
    }
    let legacy = r#"{"operation":"git_branch","name":null,"start_point":null}"#;
    let call: ToolCall = serde_json::from_str(legacy).unwrap();
    assert!(call.valid());
    assert_eq!(serde_json::to_string(&call).unwrap(), legacy);
    let model = model_call(&call);
    for field in ["scope", "base", "max_count"] {
        assert_eq!(model.get(field), Some(&Value::Null));
    }
    for scope in [None, Some("local"), Some("remote"), Some("all")] {
        for count in [None, Some(1), Some(256)] {
            let call = ToolCall::GitBranch {
                name: None,
                start_point: None,
                scope: scope.map(str::to_owned),
                base: Some("HEAD~1".into()),
                max_count: count,
            };
            assert!(call.valid());
            assert!(call.is_goal_plan_prelude() && !call.mutates() && !call.is_child_research());
            assert_eq!(
                serde_json::from_value::<ToolCall>(serde_json::to_value(&call).unwrap()).unwrap(),
                call
            );
        }
    }
    for fields in [
        json!({"scope":"live"}),
        json!({"max_count":0}),
        json!({"max_count":257}),
        json!({"base":"main..topic"}),
        json!({"base":"--all"}),
        json!({"start_point":"main"}),
    ] {
        let mut value: Value = serde_json::from_str(legacy).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        assert!(!serde_json::from_value::<ToolCall>(value).unwrap().valid());
    }
    for fields in [
        json!({"scope":"local"}),
        json!({"base":"HEAD"}),
        json!({"max_count":1}),
    ] {
        let mut value = json!({"operation":"git_branch","name":"topic","start_point":null});
        value
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        assert!(!serde_json::from_value::<ToolCall>(value).unwrap().valid());
    }
    let missing_head = json!({"operation":"git_diff","path":".","base":null,"staged":false});
    assert!(serde_json::from_value::<ToolCall>(missing_head).is_err());
    let with_head =
        json!({"operation":"git_diff","path":".","base":null,"head":null,"staged":false});
    assert_eq!(
        serde_json::from_value::<ToolCall>(with_head).unwrap(),
        ToolCall::GitDiff {
            path: ".".into(),
            base: None,
            head: None,
            staged: false
        }
    );
}

#[test]
fn git_pin_is_optional_closed_and_rooted_per_platform() {
    let good = if cfg!(target_os = "macos") {
        "/Library/Developer/CommandLineTools/usr/bin/git"
    } else {
        "/usr/bin/git"
    };
    let other = if cfg!(target_os = "macos") {
        "/usr/bin/git"
    } else {
        "/Library/Developer/CommandLineTools/usr/bin/git"
    };
    let pin = json!({"name":"git","executable":good,"sha256":"0".repeat(64),"interpreter":false,"inputs":[]});
    let config_with = |workspace: Value, git: Option<Value>| {
        let mut value = config_value();
        value["workspace_paths"] = workspace;
        if let Some(git) = git {
            value["git"] = git;
        }
        serde_json::from_value::<Config>(value).unwrap()
    };
    let status = ToolCall::GitStatus { path: ".".into() };
    let config = config_with(json!(["."]), Some(pin.clone()));
    assert!(config.validate().is_ok());
    assert!(config.allows(&status));
    assert!(config.allows(&ToolCall::GitLog {
        path: ".".into(),
        revision: None,
        max_count: 5
    }));
    assert!(config.allows(&ToolCall::GitDiff {
        path: "src".into(),
        base: None,
        head: None,
        staged: false,
    }));
    let mutations = [
        json!({"name":"git","executable":other,"sha256":"0".repeat(64),"interpreter":false,"inputs":[]}),
        json!({"name":"sh","executable":if cfg!(target_os = "macos") { "/Library/Developer/CommandLineTools/usr/bin/git" } else { "/usr/bin/git" },"sha256":"0".repeat(64),"interpreter":false,"inputs":[]}),
        json!({"name":"git","executable":"/usr/bin/git","sha256":"0".repeat(64),"interpreter":true,"inputs":[]}),
        json!({"name":"git","executable":"/usr/bin/git","sha256":"0".repeat(64),"interpreter":false,"inputs":[{"path":"/usr/bin/true","destination":"t","sha256":"0".repeat(64)}]}),
        json!({"name":"git","executable":"/opt/git/bin/git","sha256":"0".repeat(64),"interpreter":false,"inputs":[]}),
        json!({"name":"git","executable":"/usr/../opt/git","sha256":"0".repeat(64),"interpreter":false,"inputs":[]}),
    ];
    for mutation in mutations {
        assert!(
            config_with(json!(["."]), Some(mutation))
                .validate()
                .is_err()
        );
    }
    let narrow = config_with(json!(["src", "Cargo.toml"]), Some(pin));
    assert!(narrow.validate().is_ok());
    assert!(!narrow.allows(&status));
    let absent = config_with(json!(["."]), None);
    assert!(absent.validate().is_ok());
    assert!(!absent.allows(&status));
    assert!(serde_json::to_value(&absent).unwrap().get("git").is_none());
}
