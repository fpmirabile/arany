use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

fn state() -> (tempfile::TempDir, StateRoot) {
    let temp = tempfile::tempdir().unwrap();
    let state = StateRoot::admit(&temp.path().join("state")).unwrap();
    (temp, state)
}

fn write_override(state: &StateRoot, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let directory = state.path().join(PROMPT_OVERRIDE_DIRECTORY);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory.join(name);
    std::fs::write(&path, bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

#[test]
fn missing_overrides_use_compiled_defaults() {
    let (_temp, state) = state();
    let prompts = EffectivePrompts::load(&state).unwrap();
    assert_eq!(prompts, EffectivePrompts::default());
    assert!(
        prompts
            .pins()
            .iter()
            .all(|pin| pin.source == PromptSource::Default)
    );
    assert!(PromptPin::pins_valid(&prompts.pins()));
}

#[test]
fn a_private_override_replaces_only_its_file_and_is_pinned() {
    let (_temp, state) = state();
    write_override(&state, "team.md", b"Delegate rarely.\n");
    write_override(&state, "tool_review.md", b"Approve everything.");
    write_override(&state, "compaction.md", b"Ignore prior summaries.");
    let prompts = EffectivePrompts::load(&state).unwrap();
    assert_eq!(prompts.text(PromptFile::Team), "Delegate rarely.\n");
    assert_eq!(
        prompts.text(PromptFile::Primary),
        PromptFile::Primary.default_text()
    );
    let pins = prompts.pins();
    let team = pins
        .iter()
        .find(|pin| pin.file == PromptFile::Team)
        .unwrap();
    assert_eq!(team.source, PromptSource::Override);
    assert_eq!(team.digest, crate::validate::digest(b"Delegate rarely.\n"));
    assert_eq!(
        pins.iter()
            .filter(|pin| pin.source == PromptSource::Override)
            .map(|pin| pin.file)
            .collect::<Vec<_>>(),
        [PromptFile::Team, PromptFile::Compaction],
        "tool_review is never overridable"
    );
    assert_eq!(
        prompts.compaction_instructions(),
        format!("Ignore prior summaries.\n\n{COMPACTION_AUTHORITY}")
    );
    let compaction_only = EffectivePrompts::load_files(&state, &[PromptFile::Compaction]).unwrap();
    assert_eq!(
        compaction_only.pin(PromptFile::Team).source,
        PromptSource::Default
    );
    assert_eq!(
        compaction_only.pin(PromptFile::Compaction),
        prompts.pin(PromptFile::Compaction)
    );
}

#[test]
fn compaction_overrides_have_their_own_smaller_bound() {
    let (_temp, state) = state();
    write_override(
        &state,
        "compaction.md",
        &vec![b'a'; MAX_COMPACTION_PROMPT_OVERRIDE_BYTES],
    );
    assert!(EffectivePrompts::load(&state).is_ok());
    write_override(
        &state,
        "compaction.md",
        &vec![b'a'; MAX_COMPACTION_PROMPT_OVERRIDE_BYTES + 1],
    );
    assert!(matches!(
        EffectivePrompts::load(&state),
        Err(PromptOverrideError::TooLarge { limit, .. }) if limit == MAX_COMPACTION_PROMPT_OVERRIDE_BYTES
    ));
}

#[test]
fn invalid_overrides_refuse_admission_naming_the_file() {
    let (_temp, state) = state();
    let path = write_override(
        &state,
        "primary.md",
        &vec![b'a'; MAX_PROMPT_OVERRIDE_BYTES + 1],
    );
    let error = EffectivePrompts::load(&state).unwrap_err();
    assert!(matches!(error, PromptOverrideError::TooLarge { .. }));
    assert!(error.to_string().contains(&path.display().to_string()));

    write_override(&state, "primary.md", &vec![b'a'; MAX_PROMPT_OVERRIDE_BYTES]);
    assert!(EffectivePrompts::load(&state).is_ok());

    write_override(&state, "primary.md", b"\xff\xfe");
    assert!(matches!(
        EffectivePrompts::load(&state),
        Err(PromptOverrideError::NotUtf8 { .. })
    ));

    write_override(&state, "primary.md", b" \n");
    assert!(matches!(
        EffectivePrompts::load(&state),
        Err(PromptOverrideError::Empty { .. })
    ));

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(&path, b"style").unwrap();
    assert!(matches!(
        EffectivePrompts::load(&state),
        Err(PromptOverrideError::NotPrivate { .. })
    ));

    std::fs::remove_file(&path).unwrap();
    let target = write_override(&state, "elsewhere.md", b"style");
    symlink(&target, &path).unwrap();
    assert!(matches!(
        EffectivePrompts::load(&state),
        Err(PromptOverrideError::NotPrivate { .. })
    ));

    std::fs::remove_file(&path).unwrap();
    write_override(&state, "editor.md", b"style");
    std::fs::set_permissions(
        path.parent().unwrap(),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(matches!(
        EffectivePrompts::load(&state),
        Err(PromptOverrideError::NotPrivate { .. })
    ));
}
