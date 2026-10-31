use crate::accounts::chatgpt::account::CHATGPT_ACCOUNTS;
use crate::accounts::chatgpt::registration::CHATGPT_REGISTRATION;
use crate::accounts::credentials::SAVED_ACCOUNT;
use crate::cli::provider::{roster::PROVIDER_ROSTER, routing::PROVIDER_ROUTING};
use arany::{ArchivedRecords, RECORD_FORMATS, RecordFormat, StateRoot};
use std::path::Path;

pub(crate) fn formats() -> Vec<&'static RecordFormat> {
    let mut formats = RECORD_FORMATS.to_vec();
    formats.extend([
        &SAVED_ACCOUNT,
        &CHATGPT_ACCOUNTS,
        &CHATGPT_REGISTRATION,
        &PROVIDER_ROSTER,
        &PROVIDER_ROUTING,
    ]);
    formats
}

// Runs before any record owner reads, so a strict record its owner refuses is archived instead of refused later.
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
            .map_err(|_| "unreadable saved records could not be archived; retry")?
        {
            notices.push(archived_records_notice(&archived));
        }
    }
    Ok(notices)
}

fn archived_records_notice(archived: &ArchivedRecords) -> String {
    crate::presentation::escape_terminal(&format!(
        "Saved records this version cannot read ({}) archived intact to {}; set them up again",
        archived.files().join(", "),
        archived.path().display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arany::RecordKind;

    // Tolerant records read leniently and are never archived; AGENTS.md is plain Markdown.
    const UNARCHIVED: [RecordKind; 3] = [
        RecordKind::UiPreferences,
        RecordKind::ModelPreferences,
        RecordKind::UserInstructions,
    ];

    #[test]
    fn every_strict_record_declares_one_format() {
        let formats = formats();
        for &kind in RecordKind::ALL {
            let declared = formats.iter().filter(|format| format.kind == kind).count();
            assert_eq!(
                declared,
                usize::from(!UNARCHIVED.contains(&kind)),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn startup_archives_refused_strict_records_and_keeps_others() {
        use std::os::unix::fs::PermissionsExt;
        arany::test_support::isolate_account_root();
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let state = StateRoot::admit(&state_dir).unwrap();
        let roster = br#"{"profiles":[],"presets":[],"unknown":1}"#;
        let preferences = br#"{"unknown":1}"#;
        for (name, bytes) in [
            ("provider-roster.json", roster.as_slice()),
            ("ui-preferences.json", preferences.as_slice()),
        ] {
            let path = state_dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let notices = archive_incompatible(&state_dir).unwrap();
        let [notice] = notices.as_slice() else {
            panic!("{notices:?}");
        };
        assert!(notice.contains("(provider-roster.json)"), "{notice}");
        assert_eq!(
            state.record(RecordKind::ProviderRoster).read().unwrap(),
            None
        );
        assert_eq!(
            state.record(RecordKind::UiPreferences).read().unwrap(),
            Some(preferences.to_vec())
        );
        let archived = std::fs::read_dir(state_dir.join("archive"))
            .unwrap()
            .map(|entry| entry.unwrap().path().join("provider-roster.json"))
            .collect::<Vec<_>>();
        assert_eq!(std::fs::read(&archived[0]).unwrap(), roster);
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
