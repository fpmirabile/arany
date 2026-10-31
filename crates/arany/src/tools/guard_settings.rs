use super::ToolError;
use super::toolchains::{GrantRoot, RuntimeGrant, RuntimeKind, RuntimeManager};
use crate::store::{RecordFormat, RecordKind, StateRoot, StoreError};
use crate::tools::fs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const MAX_SAVED_READ_GRANTS: usize = 32;
// The Linux Guard reads at most this many first-level entries of an approved folder per command.
pub(crate) const MAX_READ_FOLDER_ENTRIES: usize = 4096;
// Directories a human allowed once stay granted for the rest of that Run.
pub const MAX_RUN_READ_GRANTS: usize = 16;

/// Whether commands run in the Guard, fall back to approved host commands, or always run on the host.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardMode {
    #[default]
    Auto,
    Required,
    Off,
}

impl GuardMode {
    pub const ALL: [Self; 3] = [Self::Auto, Self::Required, Self::Off];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Required => "required",
            Self::Off => "off",
        }
    }
}

/// The user's Guard setting and the directories every guarded command may read and execute.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardSettings {
    mode: GuardMode,
    reads: Vec<GrantRoot>,
}

pub(crate) const GUARD_SETTINGS: RecordFormat = RecordFormat {
    kind: RecordKind::GuardSettings,
    accepts: |bytes| GuardSettings::parse(bytes).is_ok(),
};

impl GuardSettings {
    fn parse(bytes: &[u8]) -> Result<Self, ToolError> {
        let value = super::skills::parse_json(bytes, RecordKind::GuardSettings.limit())
            .map_err(|_| ToolError::Configuration)?;
        let settings: Self = serde_json::from_value(value).map_err(|_| ToolError::Configuration)?;
        if settings.reads.len() > MAX_SAVED_READ_GRANTS
            || settings
                .reads
                .iter()
                .enumerate()
                .any(|(index, read)| !read.valid_shape() || settings.reads[..index].contains(read))
        {
            return Err(ToolError::Configuration);
        }
        Ok(settings)
    }

    /// Reads the account root's record; a missing account root or record keeps the defaults.
    pub fn load() -> Result<Self, ToolError> {
        let account = StateRoot::account_path().map_err(|_| ToolError::Configuration)?;
        match StateRoot::open_existing(&account) {
            Ok(root) => Self::read(&root),
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(Self::default())
            }
            Err(_) => Err(ToolError::Configuration),
        }
    }

    fn read(root: &StateRoot) -> Result<Self, ToolError> {
        root.record(RecordKind::GuardSettings)
            .read()
            .map_err(|_| ToolError::Configuration)?
            .map_or_else(|| Ok(Self::default()), |bytes| Self::parse(&bytes))
    }

    pub fn mode(&self) -> GuardMode {
        self.mode
    }

    pub fn reads(&self) -> impl Iterator<Item = &Path> {
        self.reads.iter().map(|read| read.path.as_path())
    }

    /// Applies one change to the current record under the account lock and saves it atomically.
    pub fn update(
        change: impl FnOnce(&mut Self) -> Result<(), ToolError>,
    ) -> Result<Self, ToolError> {
        let account = StateRoot::account_path().map_err(|_| ToolError::Configuration)?;
        let root = StateRoot::admit(&account).map_err(|_| ToolError::Configuration)?;
        let save = || {
            let mut settings = Self::read(&root)?;
            change(&mut settings)?;
            let bytes = serde_json::to_vec(&settings).map_err(|_| ToolError::Configuration)?;
            Self::parse(&bytes)?;
            root.record(RecordKind::GuardSettings)
                .replace(&bytes)
                .map_err(|_| ToolError::Configuration)?;
            Ok(settings)
        };
        root.with_record_upgrade_lock(|| Ok(save()))
            .map_err(|_| ToolError::Configuration)?
    }

    pub fn set_mode(&mut self, mode: GuardMode) {
        self.mode = mode;
    }

    pub fn remove_read(&mut self, path: &Path) {
        self.reads.retain(|read| read.path != path);
    }

    pub(crate) fn add_read(&mut self, read: &RuntimeGrant) -> Result<(), ToolError> {
        let root = GrantRoot {
            path: read.path.clone(),
            device: read.device,
            inode: read.inode,
        };
        if !self.reads.contains(&root) {
            if self.reads.len() == MAX_SAVED_READ_GRANTS {
                return Err(ToolError::Limit);
            }
            self.reads.push(root);
        }
        Ok(())
    }

    // Saved directories that still verify; a replaced or missing one grants nothing.
    pub(crate) fn grants(&self, protected: &[&Path]) -> Vec<RuntimeGrant> {
        self.reads
            .iter()
            .filter_map(|read| admit_read(&read.path, protected).ok())
            .filter(|grant| (grant.device, grant.inode) == self.identity(&grant.path))
            .collect()
    }

    fn identity(&self, path: &Path) -> (u64, u64) {
        self.reads
            .iter()
            .find(|read| read.path == path)
            .map_or((0, 0), |read| (read.device, read.inode))
    }
}

// Everything beneath an admitted directory is exposed; only its own path is checked.
pub(crate) fn admit_read(path: &Path, protected: &[&Path]) -> Result<RuntimeGrant, ToolError> {
    if !path.is_absolute() || std::fs::canonicalize(path).ok().as_deref() != Some(path) {
        return Err(ToolError::Path);
    }
    let directory = fs::open_directory(path)?;
    let (device, inode) = fs::identity(&directory)?;
    let grant = RuntimeGrant {
        kind: RuntimeKind::ExtraRead,
        manager: RuntimeManager::UserDirectory,
        version: None,
        path: PathBuf::from(path),
        device,
        inode,
        root: None,
        masks: Vec::new(),
    };
    grant.validate_shape()?;
    if path.components().any(|part| {
        part.as_os_str()
            .to_str()
            .is_none_or(crate::validate::forbidden)
    }) {
        return Err(ToolError::Path);
    }
    grant.reject_overlap(protected)?;
    if super::guard::reserved_target(path) {
        return Err(ToolError::Path);
    }
    Ok(grant)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_record_bounds_and_refuses_protected_or_overlapping_paths() {
        assert_eq!(
            GuardSettings::parse(b"{\"mode\":\"auto\",\"reads\":[]}").unwrap(),
            GuardSettings::default()
        );
        for refused in [
            &b"{\"mode\":\"sometimes\",\"reads\":[]}"[..],
            b"{\"mode\":\"off\"}",
            b"{\"mode\":\"off\",\"reads\":[],\"extra\":1}",
            b"{\"mode\":\"off\",\"reads\":[{\"path\":\"relative\",\"device\":1,\"inode\":2}]}",
        ] {
            assert!(GuardSettings::parse(refused).is_err());
        }
        let many = (0..=MAX_SAVED_READ_GRANTS)
            .map(|index| format!("{{\"path\":\"/r/{index}\",\"device\":1,\"inode\":2}}"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(
            GuardSettings::parse(format!("{{\"mode\":\"auto\",\"reads\":[{many}]}}").as_bytes())
                .is_err()
        );

        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let workspace = root.join("project");
        let tools = root.join("tools");
        let secret = root.join(".ssh");
        for path in [&workspace, &tools, &secret] {
            std::fs::create_dir(path).unwrap();
        }
        let grant = admit_read(&tools, &[&workspace]).unwrap();
        assert_eq!(grant.kind, RuntimeKind::ExtraRead);
        for refused in [&workspace, &root, &secret, &workspace.join("missing")] {
            assert!(
                admit_read(refused, &[&workspace]).is_err(),
                "{}",
                refused.display()
            );
        }
        assert!(admit_read(Path::new("relative"), &[]).is_err());

        let mut settings = GuardSettings::default();
        settings.add_read(&grant).unwrap();
        settings.add_read(&grant).unwrap();
        assert_eq!(settings.reads().collect::<Vec<_>>(), [tools.as_path()]);
        assert_eq!(settings.grants(&[&workspace]), [grant]);
        std::fs::remove_dir(&tools).unwrap();
        assert!(settings.grants(&[&workspace]).is_empty());
    }
}
