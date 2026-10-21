use crate::accounts::chatgpt::account::CHATGPT_ACCOUNTS;
use crate::accounts::chatgpt::registration::CHATGPT_REGISTRATION;
use crate::accounts::credentials::SAVED_ACCOUNT;
use crate::cli::attached::{controls::UI_PREFERENCES, models::MODEL_PREFERENCES};
use crate::cli::provider::{roster::PROVIDER_ROSTER, routing::PROVIDER_ROUTING};
use arany::{RECORD_FORMATS, RecordFormat, RecordKind};
use serde_json::Value;

// permissions.json follows the Claude Code settings shape, which carries no version.
const UNVERSIONED: [RecordKind; 1] = [RecordKind::Permissions];

type Fixture<'a> = (RecordKind, u64, &'a str, &'a str);
// One identity row per record at its current version, plus one (input, expected) row per step.
const FIXTURES: [Fixture<'static>; 12] = [
    (
        RecordKind::ToolsConfig,
        1,
        r#"{"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}"#,
        r#"{"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}"#,
    ),
    (
        RecordKind::WorkspacePermissions,
        2,
        r#"{"version":2,"workspaces":[]}"#,
        r#"{"version":2,"workspaces":[]}"#,
    ),
    (
        RecordKind::UnguardedCommands,
        1,
        r#"{"version":1,"commands":[]}"#,
        r#"{"version":1,"commands":[]}"#,
    ),
    (RecordKind::ProviderProfiles, 1, PROFILES, PROFILES),
    (RecordKind::UiPreferences, 3, UI_V3, UI_V3_UPGRADED),
    (RecordKind::UiPreferences, 4, UI_V4, UI_V4),
    (
        RecordKind::ModelPreferences,
        1,
        r#"{"version":1,"sources":[]}"#,
        r#"{"version":1,"sources":[]}"#,
    ),
    (RecordKind::SavedAccount, 1, ACCOUNT, ACCOUNT),
    (
        RecordKind::ChatgptAccounts,
        1,
        r#"{"schema":1,"selected":null,"accounts":[],"model_checks":[]}"#,
        r#"{"schema":1,"selected":null,"accounts":[],"model_checks":[]}"#,
    ),
    (
        RecordKind::ChatgptRegistration,
        1,
        r#"{"schema":1,"host_id":"6f1c2a3e-4b5d-4e6f-8a7b-9c0d1e2f3a4b","issued_client_id":null}"#,
        r#"{"schema":1,"host_id":"6f1c2a3e-4b5d-4e6f-8a7b-9c0d1e2f3a4b","issued_client_id":null}"#,
    ),
    (RecordKind::ProviderRoster, 1, ROSTER, ROSTER),
    (RecordKind::ProviderRouting, 1, ROUTING, ROUTING),
];
const PROFILES: &str = r#"{"version":1,"profiles":[{"name":"local","protocol":"openai-responses","endpoint":"http://127.0.0.1:9321/v1/responses","model":"model-1","credential_env":"ARANY_PROVIDER_LOCAL_KEY","outcome_encoding":"json_schema","privacy":"user_authorized","max_output_tokens":4096,"capability_evidence_version":2,"efforts":[]}]}"#;
const UI_V3: &str = r#"{"version":3,"preferences":{"completion":"combined","view":"classic","goal_budget":{"max_provider_calls":64,"max_tokens":null,"max_active_ms":1800000,"max_runs":16},"resources":{"default_command_timeout_ms":120000,"max_command_timeout_ms":600000,"build_memory_bytes":4294967296,"build_processes":256,"build_project_bytes":1073741824,"build_cache_bytes":8589934592}}}"#;
const UI_V3_UPGRADED: &str = r#"{"version":4,"preferences":{"completion":"combined","view":"classic","goal_budget":{"max_provider_calls":64,"max_tokens":null,"max_active_ms":1800000,"max_runs":16},"resources":{"default_command_timeout_ms":120000,"max_command_timeout_ms":600000,"build_memory_bytes":4294967296,"build_processes":256,"build_project_bytes":1073741824,"build_cache_bytes":8589934592},"mouse":"wheel_and_clicks","copy_on_select":true}}"#;
const UI_V4: &str = r#"{"version":4,"preferences":{"completion":"tabs","view":"fullscreen","goal_budget":{"max_provider_calls":64,"max_tokens":null,"max_active_ms":1800000,"max_runs":16},"resources":{"default_command_timeout_ms":120000,"max_command_timeout_ms":600000,"build_memory_bytes":4294967296,"build_processes":256,"build_project_bytes":1073741824,"build_cache_bytes":8589934592},"mouse":"off","copy_on_select":false}}"#;
const ACCOUNT: &str = r#"{"schema":1,"storage":"private_file","account":{"schema":2,"id":"01a15ecf-a92a-7707-8af6-ce3d36320b67","provider":"openai","model":"gpt-5.4","effort":"low","api_key":"synthetic-test-key","anthropic_workspace_id":null}}"#;
const ROSTER: &str = r#"{"version":1,"profiles":[{"name":"main","routes":[{"provider":"openai","model":"gpt-5.4","effort":null,"account_id":null}]}],"presets":[{"name":"coding","providers":{"primary":"main","child":null,"review":null,"compaction":null},"child_profiles":["main"]}]}"#;
const ROUTING: &str = r#"{"version":1,"providers":{"primary":null,"child":[{"provider":"openai","model":"gpt-5.4","effort":"low","account_id":null}],"review":null,"compaction":null}}"#;

fn formats() -> Vec<&'static RecordFormat> {
    let mut formats = RECORD_FORMATS.to_vec();
    formats.extend([
        &UI_PREFERENCES,
        &MODEL_PREFERENCES,
        &SAVED_ACCOUNT,
        &CHATGPT_ACCOUNTS,
        &CHATGPT_REGISTRATION,
        &PROVIDER_ROSTER,
        &PROVIDER_ROUTING,
    ]);
    formats
}

fn uncovered(formats: &[&RecordFormat], fixtures: &[Fixture<'_>]) -> Vec<String> {
    let mut problems = Vec::new();
    for kind in RecordKind::ALL {
        let declared = formats.iter().filter(|format| format.kind == kind).count();
        if declared != usize::from(!UNVERSIONED.contains(&kind)) {
            problems.push(format!("{kind:?} declares {declared} record formats"));
        }
    }
    for format in formats {
        let kind = format.kind;
        let fixture = |version| {
            fixtures
                .iter()
                .find(|(fixture, from, ..)| *fixture == kind && *from == version)
        };
        if !fixture(format.current).is_some_and(|(_, _, input, expected)| {
            input == expected && (format.accepts)(input.as_bytes())
        }) {
            problems.push(format!(
                "{kind:?} {} has no accepted identity fixture",
                format.current
            ));
        }
        for from in format.floor..format.current {
            let (Some(step), Some((_, _, input, expected))) = (format.step(from), fixture(from))
            else {
                problems.push(format!(
                    "{kind:?} {from} -> {} has no step and fixture",
                    from + 1
                ));
                continue;
            };
            let input: Value = serde_json::from_str(input).unwrap();
            let expected: Value = serde_json::from_str(expected).unwrap();
            if (step.transform)(input) != Some(expected) {
                problems.push(format!("{kind:?} {from} -> {} fixture mismatch", from + 1));
            }
        }
        if let Some((_, floor, input, _)) = fixture(format.floor)
            && format.floor < format.current
            && format.upgrade(input.as_bytes()).is_none()
        {
            problems.push(format!(
                "{kind:?} {floor} -> {} does not decode",
                format.current
            ));
        }
    }
    problems
}

#[test]
fn every_record_version_from_its_floor_has_a_step_and_fixture() {
    assert_eq!(uncovered(&formats(), &FIXTURES), Vec::<String>::new());

    let bumped = RecordFormat {
        current: UI_PREFERENCES.current + 1,
        ..UI_PREFERENCES
    };
    let changed: Vec<&RecordFormat> = formats()
        .into_iter()
        .filter(|format| format.kind != RecordKind::ProviderRouting)
        .map(|format| {
            if format.kind == RecordKind::UiPreferences {
                &bumped
            } else {
                format
            }
        })
        .collect();
    assert_eq!(
        uncovered(&changed, &FIXTURES),
        [
            "ProviderRouting declares 0 record formats",
            "UiPreferences 5 has no accepted identity fixture",
            "UiPreferences 4 -> 5 has no step and fixture",
            "UiPreferences 3 -> 5 does not decode",
        ]
    );
    // A decoder that changes shape without a version bump refuses its own identity fixture.
    let reshaped = RecordFormat {
        accepts: |bytes| !bytes.windows(5).any(|window| window == b"mouse"),
        ..UI_PREFERENCES
    };
    assert_eq!(
        uncovered(&[&reshaped], &FIXTURES)
            .into_iter()
            .filter(|problem| problem.starts_with("UiPreferences"))
            .collect::<Vec<_>>(),
        [
            "UiPreferences 4 has no accepted identity fixture",
            "UiPreferences 3 -> 4 does not decode",
        ]
    );
}
