use super::{ConfigError, Decision, MAX_CONFIG_BYTES};
use crate::store::{RecordKind, StateRoot, StoreError};
use crate::tools::ToolCall;
use crate::validate::{MAX_PATH_BYTES, digest};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::path::Path;

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
            ToolCall::GitPush { .. } => Some(Self::Push),
            ToolCall::GhPrCreate { .. } => Some(Self::PrCreate),
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

pub(super) fn valid_path(path: &str) -> bool {
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
    host: Vec<super::HostPrefixRule>,
    digest: Option<[u8; 32]>,
    folder: crate::session::WorkspaceIdentity,
}

impl PermissionSettings {
    pub fn load(state: &StateRoot, workspace: &Path) -> Result<Self, SettingsError> {
        let folder =
            crate::tools::workspace_identity(workspace).map_err(|_| SettingsError::Folder)?;
        Self::from_bytes(read(state)?.as_deref(), folder)
    }

    fn from_bytes(
        bytes: Option<&[u8]>,
        folder: crate::session::WorkspaceIdentity,
    ) -> Result<Self, SettingsError> {
        let (entries, host) = match bytes {
            None => (Vec::new(), Vec::new()),
            Some(bytes) => {
                let config =
                    super::PermissionConfig::parse(bytes).map_err(|_| SettingsError::Invalid)?;
                (config.typed, config.host)
            }
        };
        Ok(Self {
            entries,
            host,
            digest: bytes.map(digest),
            folder,
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

    pub fn host_prefixes(&self) -> &[super::HostPrefixRule] {
        &self.host
    }

    pub fn save_host_edit(
        &self,
        state: &StateRoot,
        workspace: &Path,
        edit: HostPrefixEdit,
    ) -> Result<Self, SettingsError> {
        if let HostPrefixEdit::Insert(rule) = &edit {
            rule.verify_creation(state, workspace)
                .map_err(|_| SettingsError::Invalid)?;
        }
        save(
            state,
            workspace,
            self.folder,
            Some(self.digest),
            false,
            Edit::Host(edit),
        )
    }

    pub(crate) fn remember_host(
        state: &StateRoot,
        intent: &crate::tools::EffectIntent,
        expected: Option<[u8; 32]>,
    ) -> Result<[u8; 32], SettingsError> {
        let crate::tools::ToolCall::CommandUnguarded {
            prefix_rule: Some(prefix),
            ..
        } = &intent.call
        else {
            return Err(SettingsError::Invalid);
        };
        let rule = super::HostPrefixRule::from_intent(intent, prefix.len())
            .map_err(|_| SettingsError::Invalid)?;
        if rule.prefix != *prefix || prefix.len() < super::MIN_HOST_PREFIX_TOKENS {
            return Err(SettingsError::Invalid);
        }
        let identity = crate::session::WorkspaceIdentity {
            device: intent.workspace_device,
            inode: intent.workspace_inode,
        };
        save(
            state,
            Path::new(rule.workspace()),
            identity,
            Some(expected),
            false,
            Edit::Host(HostPrefixEdit::Insert(Box::new(rule.clone()))),
        )?
        .digest
        .ok_or(SettingsError::Invalid)
    }

    // The folder identity captured by `load` binds the edit; a replaced directory refuses.
    pub fn save_edit(
        &self,
        state: &StateRoot,
        workspace: &Path,
        edit: TypedPrefixEdit,
    ) -> Result<Self, SettingsError> {
        save(
            state,
            workspace,
            self.folder,
            Some(self.digest),
            false,
            Edit::Typed(edit),
        )
    }

    // Only extends an existing file, so remembering never activates settings for other Tools.
    pub(crate) fn remember(
        state: &StateRoot,
        workspace: &Path,
        admitted: crate::session::WorkspaceIdentity,
        prefix: TypedGitPrefix,
        expected: Option<[u8; 32]>,
    ) -> Result<[u8; 32], SettingsError> {
        save(
            state,
            workspace,
            admitted,
            Some(expected),
            true,
            Edit::Typed(TypedPrefixEdit::Insert {
                prefix,
                decision: Decision::Allow,
                folder: true,
            }),
        )
        .and_then(|settings| settings.digest.ok_or(SettingsError::Invalid))
    }
}

#[derive(Clone, Debug)]
pub enum HostPrefixEdit {
    Insert(Box<super::HostPrefixRule>),
    Replace { index: usize, decision: Decision },
    Remove { index: usize },
}

enum Edit {
    Typed(TypedPrefixEdit),
    Host(HostPrefixEdit),
}

fn edit_host(
    entries: &mut Vec<super::HostPrefixRule>,
    edit: HostPrefixEdit,
    workspace: &Path,
) -> Result<(), SettingsError> {
    match edit {
        HostPrefixEdit::Insert(rule) => {
            if !rule.valid() || Path::new(rule.workspace()) != workspace {
                return Err(SettingsError::Invalid);
            }
            if !entries.contains(&rule) {
                entries.push(*rule);
                if serde_json::to_vec(entries)
                    .map_err(|_| SettingsError::Invalid)?
                    .len()
                    > super::MAX_HOST_PREFIX_BYTES
                {
                    return Err(SettingsError::Invalid);
                }
            }
        }
        HostPrefixEdit::Replace { index, decision } => {
            entries
                .get_mut(index)
                .ok_or(SettingsError::Missing)?
                .decision = decision
        }
        HostPrefixEdit::Remove { index } => {
            if index >= entries.len() {
                return Err(SettingsError::Missing);
            }
            entries.remove(index);
        }
    }
    Ok(())
}

fn read(state: &StateRoot) -> Result<Option<Vec<u8>>, SettingsError> {
    state
        .record(RecordKind::Permissions)
        .read()
        .map_err(|_| SettingsError::Invalid)
}

fn edit_typed(
    entries: &mut Vec<TypedRule>,
    edit: TypedPrefixEdit,
    workspace: &Path,
    identity: crate::session::WorkspaceIdentity,
) -> Result<(), SettingsError> {
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
    Ok(())
}

fn save(
    state: &StateRoot,
    workspace: &Path,
    identity: crate::session::WorkspaceIdentity,
    expected: Option<Option<[u8; 32]>>,
    existing_only: bool,
    edit: Edit,
) -> Result<PermissionSettings, SettingsError> {
    state
        .with_account_replacement_lock(workspace, || {
            if crate::tools::workspace_identity(workspace).map_err(|_| SettingsError::Folder)?
                != identity
            {
                return Err(SettingsError::Folder);
            }
            let bytes = read(state)?;
            if existing_only && bytes.is_none() {
                return Err(SettingsError::Conflict);
            }
            if expected.is_some_and(|expected| expected != bytes.as_deref().map(digest)) {
                return Err(SettingsError::Conflict);
            }
            let current = PermissionSettings::from_bytes(bytes.as_deref(), identity)?;
            let mut entries = current.entries;
            let mut host = current.host;
            match edit {
                Edit::Host(edit) => edit_host(&mut host, edit, workspace)?,
                Edit::Typed(edit) => edit_typed(&mut entries, edit, workspace, identity)?,
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
            if host.is_empty() {
                permissions.remove("hostPrefixes");
            } else {
                permissions.insert(
                    "hostPrefixes".into(),
                    serde_json::to_value(&host).map_err(|_| SettingsError::Invalid)?,
                );
            }
            let updated = serde_json::to_vec(&document).map_err(|_| SettingsError::Invalid)?;
            if updated.len() > MAX_CONFIG_BYTES {
                return Err(SettingsError::Invalid);
            }
            let parsed = PermissionSettings::from_bytes(Some(&updated), identity)?;
            if parsed.entries != entries || parsed.host != host {
                return Err(SettingsError::Invalid);
            }
            state
                .record(RecordKind::Permissions)
                .replace(&updated)
                .map_err(|error| match error {
                    StoreError::ReplaceUnconfirmed => SettingsError::NotConfirmed,
                    _ => SettingsError::Invalid,
                })?;
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

    fn save_with(
        state: &StateRoot,
        workspace: &Path,
        digest: Option<[u8; 32]>,
        edit: TypedPrefixEdit,
    ) -> Result<PermissionSettings, SettingsError> {
        PermissionSettings {
            entries: Vec::new(),
            host: Vec::new(),
            digest,
            folder: crate::tools::workspace_identity(workspace).unwrap(),
        }
        .save_edit(state, workspace, edit)
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
        let loaded = PermissionSettings::load(&state, &workspace).unwrap();
        assert!(!loaded.exists() && loaded.typed_prefixes().is_empty());

        let saved = save_with(
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
        let fresh = save_with(
            &state,
            &workspace,
            stale,
            insert(TypedGitPrefix::Switch, Decision::Ask, false),
        )
        .unwrap();
        assert!(matches!(
            save_with(
                &state,
                &workspace,
                stale,
                insert(TypedGitPrefix::Branch, Decision::Deny, false),
            ),
            Err(SettingsError::Conflict)
        ));
        assert!(matches!(
            save_with(
                &state,
                &workspace,
                None,
                insert(TypedGitPrefix::Branch, Decision::Deny, false),
            ),
            Err(SettingsError::Conflict)
        ));
        assert_eq!(
            PermissionSettings::load(&state, &workspace)
                .unwrap()
                .typed_prefixes()
                .len(),
            2
        );

        let replaced = save_with(
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
            save_with(
                &state,
                &workspace,
                replaced.digest(),
                TypedPrefixEdit::Remove { index: 9 },
            ),
            Err(SettingsError::Missing)
        ));
        let removed = save_with(
            &state,
            &workspace,
            replaced.digest(),
            TypedPrefixEdit::Remove { index: 0 },
        )
        .unwrap();
        let reopened = PermissionSettings::load(&state, &workspace).unwrap();
        assert_eq!(reopened.typed_prefixes(), removed.typed_prefixes());
        assert_eq!(reopened.typed_prefixes()[0].prefix, TypedGitPrefix::Switch);
        let emptied = save_with(
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
        let loaded = PermissionSettings::load(&state, &workspace).unwrap();
        save_with(
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
        assert!(PermissionSettings::load(&state, &workspace).is_err());
        assert!(
            save_with(
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
            save_with(
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
        let loaded = PermissionSettings::load(&state, &workspace).unwrap();
        assert!(
            save_with(
                &state,
                &workspace,
                loaded.digest(),
                insert(TypedGitPrefix::Commit, Decision::Allow, false),
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&file).unwrap(), before);
    }

    #[test]
    fn typed_prefix_edits_refuse_a_workspace_replaced_after_load() {
        let (_temp, state, workspace) = setup();
        let loaded = PermissionSettings::load(&state, &workspace).unwrap();
        let moved = workspace.with_file_name("moved");
        std::fs::rename(&workspace, &moved).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        assert!(matches!(
            loaded.save_edit(
                &state,
                &workspace,
                insert(TypedGitPrefix::Commit, Decision::Allow, true),
            ),
            Err(SettingsError::Folder)
        ));
        assert!(
            !PermissionSettings::load(&state, &workspace)
                .unwrap()
                .exists()
        );
        assert!(matches!(
            PermissionSettings::remember(
                &state,
                &workspace,
                crate::tools::workspace_identity(&moved).unwrap(),
                TypedGitPrefix::Commit,
                None,
            ),
            Err(SettingsError::Folder)
        ));
    }

    #[test]
    fn remembering_returns_its_written_digest_and_refuses_unexpected_settings() {
        let (_temp, state, workspace) = setup();
        let file = state.path().join("permissions.json");
        std::fs::write(&file, b"{}").unwrap();
        std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
        let identity = crate::tools::workspace_identity(&workspace).unwrap();
        let expected = PermissionSettings::load(&state, &workspace)
            .unwrap()
            .digest();
        let written = PermissionSettings::remember(
            &state,
            &workspace,
            identity,
            TypedGitPrefix::Commit,
            expected,
        )
        .unwrap();
        assert_eq!(
            Some(written),
            PermissionSettings::load(&state, &workspace)
                .unwrap()
                .digest()
        );
        let bytes = std::fs::read(&file).unwrap();
        assert!(matches!(
            PermissionSettings::remember(
                &state,
                &workspace,
                identity,
                TypedGitPrefix::Branch,
                expected
            ),
            Err(SettingsError::Conflict)
        ));
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
    }

    #[test]
    fn remembering_extends_only_an_existing_file() {
        let (temp, state, workspace) = setup();
        let identity = crate::tools::workspace_identity(&workspace).unwrap();
        assert!(
            PermissionSettings::remember(
                &state,
                &workspace,
                identity,
                TypedGitPrefix::Commit,
                None
            )
            .is_err()
        );
        assert!(!temp.path().join("state/permissions.json").exists());
    }
}
