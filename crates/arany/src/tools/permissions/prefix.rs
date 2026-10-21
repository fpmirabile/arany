use super::{ConfigError, Decision, MAX_CONFIG_BYTES};
use crate::store::{RecordKind, StateRoot, StoreError};
use crate::tools::{ToolCall, types::digest};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::path::Path;

const MAX_PATH_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TypedGitPrefix {
    Commit,
    Branch,
    Switch,
    Push,
    PrCreate,
}

impl TypedGitPrefix {
    pub const ALL: [Self; 5] = [
        Self::Commit,
        Self::Branch,
        Self::Switch,
        Self::Push,
        Self::PrCreate,
    ];

    pub fn tokens(self) -> &'static [&'static str] {
        match self {
            Self::Commit => &["git", "commit"],
            Self::Branch => &["git", "branch"],
            Self::Switch => &["git", "switch"],
            Self::Push => &["git", "push"],
            Self::PrCreate => &["gh", "pr", "create"],
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Commit => "git commit",
            Self::Branch => "git branch",
            Self::Switch => "git switch",
            Self::Push => "git push",
            Self::PrCreate => "gh pr create",
        }
    }

    pub fn available(self) -> bool {
        !matches!(self, Self::Push | Self::PrCreate)
    }

    fn from_tokens(tokens: &[String]) -> Option<Self> {
        Self::ALL.into_iter().find(|prefix| {
            prefix.tokens().len() == tokens.len()
                && prefix.tokens().iter().zip(tokens).all(|(a, b)| a == b)
        })
    }

    pub(crate) fn of_call(call: &ToolCall) -> Option<Self> {
        match call {
            ToolCall::GitCommit { .. } => Some(Self::Commit),
            ToolCall::GitBranch { name: Some(_), .. } => Some(Self::Branch),
            ToolCall::GitSwitch { .. } => Some(Self::Switch),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Folder {
    path: String,
    device: u64,
    inode: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TypedRule {
    prefix: TypedGitPrefix,
    decision: Decision,
    folder: Option<Folder>,
}

impl TypedRule {
    // Without a scoped Workspace a folder rule can only restrict.
    pub(super) fn applies(&self, scoped: bool) -> bool {
        self.folder.is_none() || scoped || self.decision != Decision::Allow
    }

    pub(super) fn prefix(&self) -> TypedGitPrefix {
        self.prefix
    }

    pub(super) fn decision(&self) -> Decision {
        self.decision
    }

    pub(super) fn in_folder(&self, path: &Path, device: u64, inode: u64) -> bool {
        self.folder.as_ref().is_none_or(|folder| {
            std::ffi::OsStr::new(&folder.path) == path.as_os_str()
                && folder.device == device
                && folder.inode == inode
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawTypedRule {
    prefix: Vec<String>,
    decision: Decision,
    #[serde(deserialize_with = "Option::deserialize")]
    workspace: Option<RawFolder>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFolder {
    path: String,
    device: u64,
    inode: u64,
}

pub(super) fn parse_rules(raw: Vec<RawTypedRule>) -> Result<Vec<TypedRule>, ConfigError> {
    let mut rules: Vec<TypedRule> = Vec::with_capacity(raw.len());
    for (index, entry) in raw.into_iter().enumerate() {
        let rule_error = |reason| ConfigError::Rule {
            list: "typedPrefixes",
            index,
            reason,
        };
        let prefix = TypedGitPrefix::from_tokens(&entry.prefix)
            .ok_or_else(|| rule_error("prefix must be exactly one supported Git or gh verb"))?;
        let folder = entry
            .workspace
            .map(|folder| {
                valid_path(&folder.path)
                    .then_some(Folder {
                        path: folder.path,
                        device: folder.device,
                        inode: folder.inode,
                    })
                    .filter(|folder| folder.inode != 0)
                    .ok_or_else(|| {
                        rule_error("workspace needs a canonical absolute path and inode")
                    })
            })
            .transpose()?;
        let rule = TypedRule {
            prefix,
            decision: entry.decision,
            folder,
        };
        if rules.contains(&rule) {
            return Err(rule_error("duplicate typed prefix rule"));
        }
        rules.push(rule);
    }
    Ok(rules)
}

fn valid_path(path: &str) -> bool {
    path.len() <= MAX_PATH_BYTES
        && path.starts_with('/')
        && !path.chars().any(char::is_control)
        && path[1..]
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("permission settings are invalid or over a limit; fix permissions.json")]
    Invalid,
    #[error("permission settings changed since they were shown; reopen /permissions")]
    Conflict,
    #[error("another Arany process is editing settings; retry")]
    Busy,
    #[error("the selected rule no longer exists; reopen /permissions")]
    Missing,
    #[error("the folder changed identity; restart in the current folder")]
    Folder,
    #[error("save not confirmed; reopen /permissions before another edit")]
    NotConfirmed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TypedPrefixEdit {
    Insert {
        prefix: TypedGitPrefix,
        decision: Decision,
        folder: bool,
    },
    Replace {
        index: usize,
        decision: Decision,
    },
    Remove {
        index: usize,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TypedPrefixEntry {
    pub prefix: TypedGitPrefix,
    pub decision: Decision,
    pub folder: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PermissionSettings {
    entries: Vec<TypedRule>,
    digest: Option<[u8; 32]>,
}

impl PermissionSettings {
    pub fn load(state: &StateRoot) -> Result<Self, SettingsError> {
        Self::from_bytes(read(state)?.as_deref())
    }

    fn from_bytes(bytes: Option<&[u8]>) -> Result<Self, SettingsError> {
        let entries = match bytes {
            None => Vec::new(),
            Some(bytes) => {
                super::PermissionConfig::parse(bytes)
                    .map_err(|_| SettingsError::Invalid)?
                    .typed
            }
        };
        Ok(Self {
            entries,
            digest: bytes.map(digest),
        })
    }

    pub fn exists(&self) -> bool {
        self.digest.is_some()
    }

    pub fn digest(&self) -> Option<[u8; 32]> {
        self.digest
    }

    pub fn typed_prefixes(&self) -> Vec<TypedPrefixEntry> {
        self.entries
            .iter()
            .map(|rule| TypedPrefixEntry {
                prefix: rule.prefix,
                decision: rule.decision,
                folder: rule.folder.as_ref().map(|folder| folder.path.clone()),
            })
            .collect()
    }

    pub fn save_edit(
        state: &StateRoot,
        workspace: &Path,
        expected_digest: Option<[u8; 32]>,
        edit: TypedPrefixEdit,
    ) -> Result<Self, SettingsError> {
        save(state, workspace, Some(expected_digest), edit)
    }

    pub(crate) fn remember(
        state: &StateRoot,
        workspace: &Path,
        prefix: TypedGitPrefix,
    ) -> Result<(), SettingsError> {
        save(
            state,
            workspace,
            None,
            TypedPrefixEdit::Insert {
                prefix,
                decision: Decision::Allow,
                folder: true,
            },
        )
        .map(drop)
    }
}

fn read(state: &StateRoot) -> Result<Option<Vec<u8>>, SettingsError> {
    state
        .record(RecordKind::Permissions)
        .read()
        .map_err(|_| SettingsError::Invalid)
}

fn save(
    state: &StateRoot,
    workspace: &Path,
    expected: Option<Option<[u8; 32]>>,
    edit: TypedPrefixEdit,
) -> Result<PermissionSettings, SettingsError> {
    let identity =
        crate::tools::workspace_identity(workspace).map_err(|_| SettingsError::Folder)?;
    state
        .with_account_replacement_lock(workspace, || {
            if crate::tools::workspace_identity(workspace).map_err(|_| SettingsError::Folder)?
                != identity
            {
                return Err(SettingsError::Folder);
            }
            let bytes = read(state)?;
            if expected.is_some_and(|expected| expected != bytes.as_deref().map(digest)) {
                return Err(SettingsError::Conflict);
            }
            let current = PermissionSettings::from_bytes(bytes.as_deref())?;
            let mut entries = current.entries;
            match edit {
                TypedPrefixEdit::Insert {
                    prefix,
                    decision,
                    folder,
                } => {
                    let folder = if folder {
                        Some(Folder {
                            path: workspace
                                .to_str()
                                .filter(|path| valid_path(path))
                                .ok_or(SettingsError::Folder)?
                                .to_owned(),
                            device: identity.device,
                            inode: identity.inode,
                        })
                    } else {
                        None
                    };
                    let rule = TypedRule {
                        prefix,
                        decision,
                        folder,
                    };
                    if !entries.contains(&rule) {
                        entries.push(rule);
                    }
                }
                TypedPrefixEdit::Replace {
                    index,
                    decision: new,
                } => {
                    let rule = entries.get_mut(index).ok_or(SettingsError::Missing)?;
                    rule.decision = new;
                    if entries
                        .iter()
                        .enumerate()
                        .any(|(other, rule)| other != index && *rule == entries[index])
                    {
                        return Err(SettingsError::Invalid);
                    }
                }
                TypedPrefixEdit::Remove { index } => {
                    if index >= entries.len() {
                        return Err(SettingsError::Missing);
                    }
                    entries.remove(index);
                }
            }
            let mut document = match &bytes {
                Some(bytes) => crate::tools::parse_json(bytes, MAX_CONFIG_BYTES)
                    .map_err(|_| SettingsError::Invalid)?,
                None => json!({"permissions": {"defaultMode": "default"}}),
            };
            let root = document.as_object_mut().ok_or(SettingsError::Invalid)?;
            let permissions = root
                .entry("permissions")
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .ok_or(SettingsError::Invalid)?;
            if entries.is_empty() {
                permissions.remove("typedPrefixes");
            } else {
                permissions.insert("typedPrefixes".into(), rows(&entries));
            }
            let updated = serde_json::to_vec(&document).map_err(|_| SettingsError::Invalid)?;
            if updated.len() > MAX_CONFIG_BYTES {
                return Err(SettingsError::Invalid);
            }
            let parsed = PermissionSettings::from_bytes(Some(&updated))?;
            if parsed.entries != entries {
                return Err(SettingsError::Invalid);
            }
            state
                .record(RecordKind::Permissions)
                .replace(&updated)
                .map_err(|_| SettingsError::NotConfirmed)?;
            Ok(parsed)
        })
        .map_err(|error| match error {
            StoreError::AccountBusy => SettingsError::Busy,
            _ => SettingsError::Invalid,
        })?
}

fn rows(entries: &[TypedRule]) -> Value {
    Value::Array(
        entries
            .iter()
            .map(|rule| {
                json!({
                    "prefix": rule.prefix.tokens(),
                    "decision": rule.decision,
                    "workspace": rule.folder.as_ref().map(|folder| json!({
                        "path": folder.path,
                        "device": folder.device,
                        "inode": folder.inode,
                    })),
                })
            })
            .collect(),
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, StateRoot, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let state = StateRoot::admit(&temp.path().join("state")).unwrap();
        let workspace = temp.path().join("work");
        std::fs::create_dir(&workspace).unwrap();
        (temp, state, workspace.canonicalize().unwrap())
    }

    fn commit() -> ToolCall {
        ToolCall::GitCommit {
            message: "m".into(),
            paths: vec!["a".into()],
            staged: false,
        }
    }

    fn insert(prefix: TypedGitPrefix, decision: Decision, folder: bool) -> TypedPrefixEdit {
        TypedPrefixEdit::Insert {
            prefix,
            decision,
            folder,
        }
    }

    #[test]
    fn typed_prefix_edits_are_atomic_identity_bound_and_conflict_checked() {
        let (temp, state, workspace) = setup();
        let loaded = PermissionSettings::load(&state).unwrap();
        assert!(!loaded.exists() && loaded.typed_prefixes().is_empty());

        let saved = PermissionSettings::save_edit(
            &state,
            &workspace,
            loaded.digest(),
            insert(TypedGitPrefix::Commit, Decision::Allow, true),
        )
        .unwrap();
        let entries = saved.typed_prefixes();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].folder.as_deref(), workspace.to_str());
        let file = temp.path().join("state/permissions.json");
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains(r#""defaultMode":"default""#), "{text}");
        let mut config = super::super::PermissionConfig::parse(text.as_bytes()).unwrap();
        assert_eq!(config.decision(&commit()), Decision::Ask);
        let identity = crate::tools::workspace_identity(&workspace).unwrap();
        config.scope_to(&workspace, identity);
        assert_eq!(config.decision(&commit()), Decision::Allow);

        let stale = saved.digest();
        let fresh = PermissionSettings::save_edit(
            &state,
            &workspace,
            stale,
            insert(TypedGitPrefix::Switch, Decision::Ask, false),
        )
        .unwrap();
        assert!(matches!(
            PermissionSettings::save_edit(
                &state,
                &workspace,
                stale,
                insert(TypedGitPrefix::Branch, Decision::Deny, false),
            ),
            Err(SettingsError::Conflict)
        ));
        assert!(matches!(
            PermissionSettings::save_edit(
                &state,
                &workspace,
                None,
                insert(TypedGitPrefix::Branch, Decision::Deny, false),
            ),
            Err(SettingsError::Conflict)
        ));
        assert_eq!(
            PermissionSettings::load(&state)
                .unwrap()
                .typed_prefixes()
                .len(),
            2
        );

        let replaced = PermissionSettings::save_edit(
            &state,
            &workspace,
            fresh.digest(),
            TypedPrefixEdit::Replace {
                index: 1,
                decision: Decision::Deny,
            },
        )
        .unwrap();
        assert_eq!(replaced.typed_prefixes()[1].decision, Decision::Deny);
        assert!(matches!(
            PermissionSettings::save_edit(
                &state,
                &workspace,
                replaced.digest(),
                TypedPrefixEdit::Remove { index: 9 },
            ),
            Err(SettingsError::Missing)
        ));
        let removed = PermissionSettings::save_edit(
            &state,
            &workspace,
            replaced.digest(),
            TypedPrefixEdit::Remove { index: 0 },
        )
        .unwrap();
        let reopened = PermissionSettings::load(&state).unwrap();
        assert_eq!(reopened.typed_prefixes(), removed.typed_prefixes());
        assert_eq!(reopened.typed_prefixes()[0].prefix, TypedGitPrefix::Switch);
        let emptied = PermissionSettings::save_edit(
            &state,
            &workspace,
            reopened.digest(),
            TypedPrefixEdit::Remove { index: 0 },
        )
        .unwrap();
        assert!(emptied.exists() && emptied.typed_prefixes().is_empty());
        assert!(
            !std::fs::read_to_string(&file)
                .unwrap()
                .contains("typedPrefixes")
        );
    }

    #[test]
    fn typed_prefix_edits_preserve_other_settings_and_refuse_unsafe_files() {
        let (temp, state, workspace) = setup();
        let file = temp.path().join("state/permissions.json");
        let original = br#"{"permissions":{"defaultMode":"dontAsk","deny":["Read(secret)"],"allow":["Read"]},"hooks":{}}"#;
        std::fs::write(&file, original).unwrap();
        std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
        let loaded = PermissionSettings::load(&state).unwrap();
        PermissionSettings::save_edit(
            &state,
            &workspace,
            loaded.digest(),
            insert(TypedGitPrefix::Commit, Decision::Allow, false),
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(value["permissions"]["defaultMode"], "dontAsk");
        assert_eq!(value["permissions"]["deny"], json!(["Read(secret)"]));
        assert_eq!(value["permissions"]["allow"], json!(["Read"]));
        assert_eq!(value["hooks"], json!({}));

        std::fs::write(&file, b"{not json").unwrap();
        assert!(PermissionSettings::load(&state).is_err());
        assert!(
            PermissionSettings::save_edit(
                &state,
                &workspace,
                None,
                insert(TypedGitPrefix::Commit, Decision::Allow, false),
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"{not json");

        std::fs::remove_file(&file).unwrap();
        let target = temp.path().join("elsewhere");
        std::fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, temp.path().join("state/permissions.pending")).unwrap();
        assert!(
            PermissionSettings::save_edit(
                &state,
                &workspace,
                None,
                insert(TypedGitPrefix::Commit, Decision::Allow, false),
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        assert!(!file.exists());
    }

    #[test]
    fn typed_prefix_edits_refuse_a_full_record_without_truncating() {
        let (temp, state, workspace) = setup();
        let rules = (0..128)
            .map(|index| format!("Read(a{index})"))
            .collect::<Vec<_>>();
        let file = temp.path().join("state/permissions.json");
        std::fs::write(
            &file,
            serde_json::to_vec(&json!({"permissions": {"deny": rules}})).unwrap(),
        )
        .unwrap();
        std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
        let before = std::fs::read(&file).unwrap();
        let loaded = PermissionSettings::load(&state).unwrap();
        assert!(
            PermissionSettings::save_edit(
                &state,
                &workspace,
                loaded.digest(),
                insert(TypedGitPrefix::Commit, Decision::Allow, false),
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&file).unwrap(), before);
    }
}
