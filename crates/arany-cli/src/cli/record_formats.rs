use crate::accounts::chatgpt::account::CHATGPT_ACCOUNTS;
use crate::accounts::chatgpt::registration::CHATGPT_REGISTRATION;
use crate::accounts::credentials::SAVED_ACCOUNT;
use crate::cli::attached::{controls::UI_PREFERENCES, models::MODEL_PREFERENCES};
use crate::cli::provider::{roster::PROVIDER_ROSTER, routing::PROVIDER_ROUTING};
use arany::{ArchivedRecords, RECORD_FORMATS, RecordFormat, StateRoot};
use std::path::Path;

pub(crate) fn formats() -> Vec<&'static RecordFormat> {
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

// Runs before any record owner reads, so an incompatible record is archived instead of refused.
pub(crate) fn archive_incompatible(state_dir: &Path) -> Result<Vec<String>, String> {
    let mut roots = vec![state_dir.to_path_buf()];
    if let Ok(account) = StateRoot::account_path()
        && account != state_dir
    {
        roots.push(account);
    }
    let formats = formats();
    let mut notices = Vec::new();
    for path in roots {
        let Some(root) =
            StateRoot::open_optional(&path).map_err(|_| "state directory unavailable or unsafe")?
        else {
            continue;
        };
        if let Some(archived) = root
            .archive_incompatible_records(&formats)
            .map_err(|_| "saved records from another version could not be archived; retry")?
        {
            notices.push(archived_records_notice(&archived));
        }
    }
    Ok(notices)
}

fn archived_records_notice(archived: &ArchivedRecords) -> String {
    crate::presentation::escape_terminal(&format!(
        "Saved records from an incompatible version ({}) archived intact to {}; defaults apply",
        archived.files().join(", "),
        archived.path().display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::chatgpt::account::TOKEN_RECORD_SCHEMA;
    use crate::accounts::credentials::SAVED_ACCOUNT_SCHEMA;
    use arany::RecordKind;
    use serde_json::Value;

    // permissions.json follows the Claude Code settings shape and AGENTS.md is plain Markdown; neither carries a version.
    const UNVERSIONED: [RecordKind; 2] = [RecordKind::Permissions, RecordKind::UserInstructions];

    // One record per versioned kind; `current` stamps its owner's current version.
    const CURRENT: [(RecordKind, &str); 14] = [
        (
            RecordKind::ToolsConfig,
            r#"{"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}"#,
        ),
        (RecordKind::WorkspacePermissions, r#"{"workspaces":[]}"#),
        (RecordKind::UnguardedCommands, r#"{"commands":[]}"#),
        (RecordKind::CommandEnvironment, COMMAND_ENVIRONMENT),
        (RecordKind::CommandTools, r#"{"tools":[]}"#),
        (RecordKind::ProviderProfiles, PROFILES),
        (
            RecordKind::UiPreferences,
            r#"{"preferences":{"goal_budget":{"max_provider_calls":64}}}"#,
        ),
        (RecordKind::ModelPreferences, r#"{"sources":[]}"#),
        (RecordKind::SavedAccount, ACCOUNT),
        (RecordKind::ChatgptAccounts, CHATGPT_ACCOUNT),
        (
            RecordKind::ChatgptRegistration,
            r#"{"host_id":"6f1c2a3e-4b5d-4e6f-8a7b-9c0d1e2f3a4b","issued_client_id":null}"#,
        ),
        (RecordKind::McpOauth, MCP_OAUTH),
        (RecordKind::ProviderRoster, ROSTER),
        (RecordKind::ProviderRouting, ROUTING),
    ];
    const COMMAND_ENVIRONMENT: &str = r#"{"source":"process","shell":null,"captured_at":0,"path":["/usr/bin","/bin"],"names":[{"name":"LANG","value":"C"},{"name":"USER","value":"owner"}],"excluded":[],"sha256":"f9aa85256800d2c10b3d044c2e5005c385f5ef42a543e0182c0a1e2366c4b6b4"}"#;
    const MCP_OAUTH: &str = r#"{"servers":{"remote":{"url":"https://mcp.example.com/mcp","client_id":"client","token_endpoint":"https://auth.example.com/token","revocation_endpoint":null,"access_token":"token","refresh_token":null,"expires_at":null,"renewal":"ready","claim":null}}}"#;
    const PROFILES: &str = r#"{"profiles":[{"name":"local","protocol":"openai-responses","endpoint":"http://127.0.0.1:9321/v1/responses","model":"model-1","credential_env":"ARANY_PROVIDER_LOCAL_KEY","outcome_encoding":"json_schema","privacy":"user_authorized","max_output_tokens":4096,"efforts":[]}]}"#;
    const ACCOUNT: &str = r#"{"storage":"private_file","account":{"schema":3,"id":"01a15ecf-a92a-7707-8af6-ce3d36320b67","provider":{"kind":"openai","custom_name":null},"model":"gpt-5.4","effort":"low","api_key":"synthetic-test-key","anthropic_workspace_id":null}}"#;
    const CHATGPT_ACCOUNT: &str = r#"{"selected":"01a15ecf-a92a-7707-8af6-ce3d36320b67","accounts":[{"id":"01a15ecf-a92a-7707-8af6-ce3d36320b67","client_id":"oaiapp_fake_fixture","subject":"fake-fixture-subject","chatgpt_account_id":null,"keyring_slot":null,"host_id":"6f1c2a3e-4b5d-4e6f-8a7b-9c0d1e2f3a4b","storage":"private_file","renewal_pending":false,"signout_pending":false,"disconnected":false,"plan_permission_missing":false,"token":{"schema":2,"slot":null,"credentials":{"client_id":"oaiapp_fake_fixture","subject":"fake-fixture-subject","host_id":"6f1c2a3e-4b5d-4e6f-8a7b-9c0d1e2f3a4b","id_token":"fake-fixture-id-token","access_token":"fake-fixture-access-token","refresh_token":"fake-fixture-refresh-token","access_expires_at_unix":1800000000},"consent":{"warning_version":4,"warning_digest":[105,213,101,28,44,236,194,209,3,187,128,126,6,193,202,212,235,202,191,172,193,80,5,139,128,220,33,211,20,67,139,31],"storage":"private_file","client_id":"oaiapp_fake_fixture","subject":"fake-fixture-subject","host_id":"6f1c2a3e-4b5d-4e6f-8a7b-9c0d1e2f3a4b","accepted_at_sec":1700000000}}},{"id":"01a15ecf-a92a-7707-8af6-ce3d36320b68","client_id":"oaiapp_fake_vault","subject":"fake-keyring-subject","chatgpt_account_id":null,"keyring_slot":"chatgpt-Ne20hIaDcO1pbYxw0HwZIWr8MYnsSAeMgUzaAXLL3rM","host_id":"6f1c2a3e-4b5d-4e6f-8a7b-9c0d1e2f3a4b","storage":"keyring","renewal_pending":false,"signout_pending":false,"disconnected":false,"plan_permission_missing":false,"token":null}]}"#;
    const ROSTER: &str = r#"{"profiles":[{"name":"main","routes":[{"provider":{"kind":"openai","custom_name":null},"model":"gpt-5.4","effort":null,"account_id":null}]}],"presets":[{"name":"coding","providers":{"primary":"main","child":null,"review":null,"compaction":null},"child_profiles":["main"]}]}"#;
    const ROUTING: &str = r#"{"providers":{"primary":null,"child":[{"provider":{"kind":"openai","custom_name":null},"model":"gpt-5.4","effort":"low","account_id":null}],"review":null,"compaction":null}}"#;

    // A nested schema change must bump its enclosing record, or old records would be refused, not archived.
    type NestedSchema = (RecordKind, &'static str, u8, u64);
    const NESTED_SCHEMAS: [NestedSchema; 2] = [
        (
            RecordKind::SavedAccount,
            "/account/schema",
            SAVED_ACCOUNT_SCHEMA,
            2,
        ),
        (
            RecordKind::ChatgptAccounts,
            "/accounts/0/token/schema",
            TOKEN_RECORD_SCHEMA,
            4,
        ),
    ];

    fn current(kind: RecordKind) -> Vec<u8> {
        let format = formats()
            .into_iter()
            .find(|format| format.kind == kind)
            .unwrap_or_else(|| panic!("{kind:?} has no format"));
        let fixture = CURRENT
            .iter()
            .find(|(fixture, _)| *fixture == kind)
            .map(|(_, record)| *record)
            .unwrap_or_else(|| panic!("{kind:?} has no current fixture"));
        let mut record: Value = serde_json::from_str(fixture).unwrap();
        record[format.field] = format.current.into();
        serde_json::to_vec(&record).unwrap()
    }

    #[test]
    fn every_versioned_record_declares_one_format_that_accepts_its_current_fixture() {
        let formats = formats();
        for &kind in RecordKind::ALL {
            let declared = formats.iter().filter(|format| format.kind == kind).count();
            assert_eq!(
                declared,
                usize::from(!UNVERSIONED.contains(&kind)),
                "{kind:?}"
            );
        }
        for format in formats {
            let record = current(format.kind);
            assert!((format.accepts)(&record), "{:?}", format.kind);
            assert!(!format.mismatched(&record), "{:?}", format.kind);
            let mut other: Value = serde_json::from_slice(&record).unwrap();
            other[format.field] = (format.current + 1).into();
            assert!(
                format.mismatched(&serde_json::to_vec(&other).unwrap()),
                "{:?}",
                format.kind
            );
        }
    }

    #[test]
    fn nested_schemas_are_pinned_to_their_enclosing_record_version() {
        let formats = formats();
        for (kind, path, schema, enclosing) in NESTED_SCHEMAS {
            let format = formats.iter().find(|format| format.kind == kind).unwrap();
            assert_eq!(
                format.current, enclosing,
                "{kind:?} changed; update the pinned nested schema"
            );
            let fixture: Value = serde_json::from_slice(&current(kind)).unwrap();
            assert_eq!(
                fixture.pointer(path).and_then(Value::as_u64),
                Some(u64::from(schema)),
                "{kind:?} {path} changed without bumping its enclosing record version"
            );
        }
    }

    #[test]
    fn startup_archives_records_from_another_version_and_keeps_current_ones() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let state = StateRoot::admit(&state_dir).unwrap();
        let older = r#"{"version":0,"preferences":{}}"#;
        let roster = current(RecordKind::ProviderRoster);
        for (name, bytes) in [
            ("ui-preferences.json", older.as_bytes()),
            ("provider-roster.json", roster.as_slice()),
        ] {
            let path = state_dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let notices = archive_incompatible(&state_dir).unwrap();
        let [notice] = notices.as_slice() else {
            panic!("{notices:?}");
        };
        assert!(notice.contains("(ui-preferences.json)"), "{notice}");
        assert_eq!(
            state.record(RecordKind::UiPreferences).read().unwrap(),
            None
        );
        assert_eq!(
            state.record(RecordKind::ProviderRoster).read().unwrap(),
            Some(roster)
        );
        let archived = std::fs::read_dir(state_dir.join("archive"))
            .unwrap()
            .map(|entry| entry.unwrap().path().join("ui-preferences.json"))
            .collect::<Vec<_>>();
        assert_eq!(std::fs::read(&archived[0]).unwrap(), older.as_bytes());
        assert!(archive_incompatible(&state_dir).unwrap().is_empty());
        assert!(
            archive_incompatible(&temp.path().join("missing"))
                .unwrap()
                .is_empty()
        );
        let unsafe_dir = temp.path().join("file");
        std::fs::write(&unsafe_dir, b"").unwrap();
        assert!(archive_incompatible(&unsafe_dir).is_err());
    }
}
