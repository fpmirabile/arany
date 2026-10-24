use crate::accounts::chatgpt::account::{CHATGPT_ACCOUNTS, TOKEN_RECORD_SCHEMA};
use crate::accounts::chatgpt::registration::CHATGPT_REGISTRATION;
use crate::accounts::credentials::{SAVED_ACCOUNT, SAVED_ACCOUNT_SCHEMA};
use crate::cli::attached::{controls::UI_PREFERENCES, models::MODEL_PREFERENCES};
use crate::cli::provider::{roster::PROVIDER_ROSTER, routing::PROVIDER_ROUTING};
use arany::{RECORD_FORMATS, RecordFormat, RecordKind};
use serde_json::Value;

// permissions.json follows the Claude Code settings shape and AGENTS.md is plain Markdown; neither carries a version.
const UNVERSIONED: [RecordKind; 2] = [RecordKind::Permissions, RecordKind::UserInstructions];

type Fixture<'a> = (RecordKind, u64, &'a str, &'a str);
// One identity row per record at its current version, plus one (input, expected) row per step.
const FIXTURES: [Fixture<'static>; 15] = [
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
    (RecordKind::ProviderProfiles, 1, PROFILES, PROFILES_V2),
    (RecordKind::ProviderProfiles, 2, PROFILES_V2, PROFILES_V2),
    (RecordKind::UiPreferences, 3, UI_V3, UI_V3_UPGRADED),
    (RecordKind::UiPreferences, 4, UI_V4, UI_V5),
    (RecordKind::UiPreferences, 5, UI_V5, UI_V5),
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
        CHATGPT_ACCOUNT_V1,
        CHATGPT_ACCOUNT,
    ),
    (
        RecordKind::ChatgptAccounts,
        2,
        CHATGPT_ACCOUNT,
        CHATGPT_ACCOUNT,
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
const PROFILES_V2: &str = r#"{"version":2,"profiles":[{"name":"local","protocol":"openai-responses","endpoint":"http://127.0.0.1:9321/v1/responses","model":"model-1","credential_env":"ARANY_PROVIDER_LOCAL_KEY","outcome_encoding":"json_schema","privacy":"user_authorized","max_output_tokens":4096,"capability_evidence_version":2,"efforts":[]}]}"#;
const UI_V3: &str = r#"{"version":3,"preferences":{"completion":"combined","view":"classic","goal_budget":{"max_provider_calls":64,"max_tokens":null,"max_active_ms":1800000,"max_runs":16},"resources":{"default_command_timeout_ms":120000,"max_command_timeout_ms":600000,"build_memory_bytes":4294967296,"build_processes":256,"build_project_bytes":1073741824,"build_cache_bytes":8589934592}}}"#;
const UI_V3_UPGRADED: &str = r#"{"version":4,"preferences":{"completion":"combined","view":"classic","goal_budget":{"max_provider_calls":64,"max_tokens":null,"max_active_ms":1800000,"max_runs":16},"resources":{"default_command_timeout_ms":120000,"max_command_timeout_ms":600000,"build_memory_bytes":4294967296,"build_processes":256,"build_project_bytes":1073741824,"build_cache_bytes":8589934592},"mouse":"wheel_and_clicks","copy_on_select":true}}"#;
const UI_V4: &str = r#"{"version":4,"preferences":{"completion":"tabs","view":"fullscreen","goal_budget":{"max_provider_calls":64,"max_tokens":null,"max_active_ms":1800000,"max_runs":16},"resources":{"default_command_timeout_ms":120000,"max_command_timeout_ms":600000,"build_memory_bytes":4294967296,"build_processes":256,"build_project_bytes":1073741824,"build_cache_bytes":8589934592},"mouse":"off","copy_on_select":false}}"#;
const UI_V5: &str = r#"{"version":5,"preferences":{"completion":"tabs","view":"fullscreen","goal_budget":{"max_provider_calls":64,"max_tokens":null,"max_active_ms":1800000,"max_runs":16},"resources":{"default_command_timeout_ms":120000,"max_command_timeout_ms":600000,"build_memory_bytes":4294967296,"build_processes":256,"build_project_bytes":1073741824,"build_cache_bytes":8589934592},"mouse":"off","copy_on_select":false,"newline":"shift_enter"}}"#;
const ACCOUNT: &str = r#"{"schema":1,"storage":"private_file","account":{"schema":2,"id":"01a15ecf-a92a-7707-8af6-ce3d36320b67","provider":"openai","model":"gpt-5.4","effort":"low","api_key":"synthetic-test-key","anthropic_workspace_id":null}}"#;
const CHATGPT_ACCOUNT_V1: &str = r#"{"schema":1,"selected":"01a15ecf-a92a-7707-8af6-ce3d36320b67","accounts":[{"id":"01a15ecf-a92a-7707-8af6-ce3d36320b67","client_id":"oaiapp_fake_fixture","subject":"fake-fixture-subject","host_id":"6f1c2a3e-4b5d-4e6f-8a7b-9c0d1e2f3a4b","storage":"private_file","renewal_pending":false,"signout_pending":false,"disconnected":false,"plan_permission_missing":false,"token":{"schema":1,"credentials":{"client_id":"oaiapp_fake_fixture","subject":"fake-fixture-subject","host_id":"6f1c2a3e-4b5d-4e6f-8a7b-9c0d1e2f3a4b","id_token":"fake-fixture-id-token","access_token":"fake-fixture-access-token","refresh_token":"fake-fixture-refresh-token","access_expires_at_unix":1800000000},"consent":{"warning_version":4,"warning_digest":[105,213,101,28,44,236,194,209,3,187,128,126,6,193,202,212,235,202,191,172,193,80,5,139,128,220,33,211,20,67,139,31],"storage":"private_file","client_id":"oaiapp_fake_fixture","subject":"fake-fixture-subject","host_id":"6f1c2a3e-4b5d-4e6f-8a7b-9c0d1e2f3a4b","accepted_at_sec":1700000000}}}],"model_checks":[{"account_id":"01a15ecf-a92a-7707-8af6-ce3d36320b67","fingerprint":[7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7],"checked_at_sec":1700000000,"expires_at_sec":1700086400,"context_window_tokens":272000}]}"#;
const CHATGPT_ACCOUNT: &str = r#"{"schema":2,"selected":"01a15ecf-a92a-7707-8af6-ce3d36320b67","accounts":[{"id":"01a15ecf-a92a-7707-8af6-ce3d36320b67","client_id":"oaiapp_fake_fixture","subject":"fake-fixture-subject","host_id":"6f1c2a3e-4b5d-4e6f-8a7b-9c0d1e2f3a4b","storage":"private_file","renewal_pending":false,"signout_pending":false,"disconnected":false,"plan_permission_missing":false,"token":{"schema":1,"credentials":{"client_id":"oaiapp_fake_fixture","subject":"fake-fixture-subject","host_id":"6f1c2a3e-4b5d-4e6f-8a7b-9c0d1e2f3a4b","id_token":"fake-fixture-id-token","access_token":"fake-fixture-access-token","refresh_token":"fake-fixture-refresh-token","access_expires_at_unix":1800000000},"consent":{"warning_version":4,"warning_digest":[105,213,101,28,44,236,194,209,3,187,128,126,6,193,202,212,235,202,191,172,193,80,5,139,128,220,33,211,20,67,139,31],"storage":"private_file","client_id":"oaiapp_fake_fixture","subject":"fake-fixture-subject","host_id":"6f1c2a3e-4b5d-4e6f-8a7b-9c0d1e2f3a4b","accepted_at_sec":1700000000}}}],"model_checks":[]}"#;
const ROSTER: &str = r#"{"version":1,"profiles":[{"name":"main","routes":[{"provider":"openai","model":"gpt-5.4","effort":null,"account_id":null}]}],"presets":[{"name":"coding","providers":{"primary":"main","child":null,"review":null,"compaction":null},"child_profiles":["main"]}]}"#;
const ROUTING: &str = r#"{"version":1,"providers":{"primary":null,"child":[{"provider":"openai","model":"gpt-5.4","effort":"low","account_id":null}],"review":null,"compaction":null}}"#;

type NestedSchema = (RecordKind, &'static str, u8, &'static [(u64, u8)]);
const NESTED_SCHEMAS: [NestedSchema; 2] = [
    (
        RecordKind::SavedAccount,
        "/account/schema",
        SAVED_ACCOUNT_SCHEMA,
        &[(1, 2)],
    ),
    (
        RecordKind::ChatgptAccounts,
        "/accounts/0/token/schema",
        TOKEN_RECORD_SCHEMA,
        &[(1, 1), (2, 1)],
    ),
];

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

fn uncovered(
    formats: &[&RecordFormat],
    fixtures: &[Fixture<'_>],
    nested_schemas: &[NestedSchema],
) -> Vec<String> {
    let mut problems = Vec::new();
    for &kind in RecordKind::ALL {
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
    for (kind, path, current, versions) in nested_schemas {
        let Some(format) = formats.iter().find(|format| format.kind == *kind) else {
            continue;
        };
        if !versions
            .iter()
            .any(|(version, schema)| *version == format.current && schema == current)
        {
            problems.push(format!(
                "{kind:?} {path} {current} requires an enclosing version bump with a step and fixture"
            ));
        }
        for version in format.floor..=format.current {
            let schema = versions
                .iter()
                .find(|(enclosing, _)| *enclosing == version)
                .map(|(_, schema)| u64::from(*schema));
            let fixture_schema = fixtures
                .iter()
                .find(|(fixture, from, ..)| fixture == kind && *from == version)
                .and_then(|(_, _, input, _)| serde_json::from_str::<Value>(input).ok())
                .and_then(|value| value.pointer(path).and_then(Value::as_u64));
            if schema.is_none() || fixture_schema != schema {
                problems.push(format!(
                    "{kind:?} {version} has no populated {path} fixture with its pinned schema"
                ));
            }
        }
    }
    problems
}

#[test]
fn every_record_version_from_its_floor_has_a_step_and_fixture() {
    assert_eq!(
        uncovered(&formats(), &FIXTURES, &NESTED_SCHEMAS),
        Vec::<String>::new()
    );

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
        uncovered(&changed, &FIXTURES, &NESTED_SCHEMAS),
        [
            "ProviderRouting declares 0 record formats",
            "UiPreferences 6 has no accepted identity fixture",
            "UiPreferences 5 -> 6 has no step and fixture",
            "UiPreferences 3 -> 6 does not decode",
        ]
    );
    // A decoder that changes shape without a version bump refuses its own identity fixture.
    let reshaped = RecordFormat {
        accepts: |bytes| !bytes.windows(5).any(|window| window == b"mouse"),
        ..UI_PREFERENCES
    };
    assert_eq!(
        uncovered(&[&reshaped], &FIXTURES, &NESTED_SCHEMAS)
            .into_iter()
            .filter(|problem| problem.starts_with("UiPreferences"))
            .collect::<Vec<_>>(),
        [
            "UiPreferences 5 has no accepted identity fixture",
            "UiPreferences 3 -> 5 does not decode",
        ]
    );
}

#[test]
fn nested_schema_bumps_require_an_enclosing_migration_even_with_an_updated_fixture() {
    for (index, (kind, path, current, _)) in NESTED_SCHEMAS.iter().enumerate() {
        let mut bumped = NESTED_SCHEMAS;
        bumped[index].2 += 1;
        let problem = format!(
            "{kind:?} {path} {} requires an enclosing version bump with a step and fixture",
            current + 1
        );
        assert_eq!(
            uncovered(&formats(), &FIXTURES, &bumped),
            std::slice::from_ref(&problem)
        );

        let enclosing = formats()
            .into_iter()
            .find(|format| format.kind == *kind)
            .unwrap()
            .current;
        let mut updated: Value = serde_json::from_str(
            FIXTURES
                .iter()
                .find(|(fixture, version, ..)| fixture == kind && *version == enclosing)
                .unwrap()
                .2,
        )
        .unwrap();
        *updated.pointer_mut(path).unwrap() = Value::from(current + 1);
        let updated = serde_json::to_string(&updated).unwrap();
        let fixtures: Vec<Fixture<'_>> = FIXTURES
            .iter()
            .map(|&(fixture, version, input, expected)| {
                if fixture == *kind && version == enclosing {
                    (fixture, version, updated.as_str(), updated.as_str())
                } else {
                    (fixture, version, input, expected)
                }
            })
            .collect();
        let reshaped = RecordFormat {
            accepts: |_| true,
            ..*formats()
                .into_iter()
                .find(|format| format.kind == *kind)
                .unwrap()
        };
        let formats: Vec<_> = formats()
            .into_iter()
            .map(|format| {
                if format.kind == *kind {
                    &reshaped
                } else {
                    format
                }
            })
            .collect();
        assert_eq!(
            uncovered(&formats, &fixtures, &bumped),
            [
                problem,
                format!(
                    "{kind:?} {enclosing} has no populated {path} fixture with its pinned schema"
                ),
            ]
        );
    }
}

#[test]
fn nested_schema_fixtures_must_be_populated() {
    for (kind, path, _, _) in NESTED_SCHEMAS {
        let enclosing = formats()
            .into_iter()
            .find(|format| format.kind == kind)
            .unwrap()
            .current;
        let empty = match kind {
            RecordKind::SavedAccount => r#"{"schema":1,"storage":"keyring","account":null}"#,
            RecordKind::ChatgptAccounts => {
                r#"{"schema":2,"selected":null,"accounts":[],"model_checks":[]}"#
            }
            _ => unreachable!(),
        };
        let fixtures: Vec<Fixture<'_>> = FIXTURES
            .iter()
            .map(|&(fixture, version, input, expected)| {
                if fixture == kind && version == enclosing {
                    (fixture, version, empty, empty)
                } else {
                    (fixture, version, input, expected)
                }
            })
            .collect();
        assert_eq!(
            uncovered(&formats(), &fixtures, &NESTED_SCHEMAS),
            [format!(
                "{kind:?} {enclosing} has no populated {path} fixture with its pinned schema"
            )]
        );
    }
}
