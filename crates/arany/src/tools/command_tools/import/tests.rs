use super::*;
use std::os::unix::fs::PermissionsExt;

#[test]
fn import_accepts_literal_arguments_and_refuses_shell_computation() {
    assert_eq!(
        literal_words("filter 'a b' --hook"),
        Some(vec!["filter".into(), "a b".into(), "--hook".into()])
    );
    assert_eq!(literal_words(r#"filter "a\q""#).unwrap()[1], r"a\q");
    for text in [
        "filter | cat",
        "$(filter)",
        "filter && other",
        "filter > log",
        "filter $HOME",
        "filter *",
        "filter # comment",
        "'unclosed",
        "",
    ] {
        assert!(literal_words(text).is_none());
    }
}

#[test]
fn user_settings_are_proposals_only_and_project_settings_are_never_imported() {
    let temp = tempfile::tempdir().unwrap();
    let (state, workspace, executable, _) = super::super::tests::fixture(temp.path());
    let home = temp.path().join("home");
    let directory = home.join(".claude");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::create_dir_all(workspace.join(".claude")).unwrap();
    let settings = serde_json::json!({"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[
        {"type":"command","command":format!("'{executable}' --hook")},
        {"type":"command","command":"filter | cat"}
    ]}]}});
    let path = directory.join("settings.json");
    std::fs::write(&path, serde_json::to_vec(&settings).unwrap()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(workspace.join(".claude/settings.json"), b"invalid").unwrap();
    let proposals = import_from(&state, &workspace, &home).unwrap();
    assert_eq!(proposals.refused, 1);
    assert_eq!(proposals.candidates.len(), 1);
    assert!(!proposals.candidates[0].enabled);
    assert_eq!(proposals.candidates[0].args, ["--hook"]);
    assert!(CommandTools::load(&state).unwrap().entries().is_empty());
    std::fs::write(&path, b"invalid").unwrap();
    assert!(import_from(&state, &workspace, &home).is_err());
    assert!(import_from(&state, &workspace, &workspace).is_err());
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(workspace.join(".claude/settings.json"), &path).unwrap();
    assert!(import_from(&state, &workspace, &home).is_err());
}
