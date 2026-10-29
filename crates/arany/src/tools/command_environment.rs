use super::ToolError;
use crate::session::LoginShellIdentity;
use crate::store::{RecordFormat, RecordKind, StateRoot, StoreError};
use crate::validate::{MAX_PATH_BYTES, digest, hex_digest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

const VERSION: u8 = 3;
const MAX_NAMES: usize = 32;
const MAX_PATH_ENTRIES: usize = 64;
const MAX_NAME_BYTES: usize = 128;
const MAX_VALUE_BYTES: usize = 1024;
pub(super) const PATH_LIMIT_BYTES: usize = 16 * 1024;
const CORE_NAMES: [&str; 11] = [
    "PATH", "SHELL", "TMPDIR", "TEMP", "TMP", "HOME", "LANG", "LC_ALL", "LC_CTYPE", "LOGNAME",
    "USER",
];
pub(super) const LOCALE_NAMES: [&str; 3] = ["LANG", "LC_ALL", "LC_CTYPE"];
const SECRET_PATTERNS: [&str; 3] = ["KEY", "SECRET", "TOKEN"];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Source {
    Process,
    LoginShell,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Variable {
    name: String,
    value: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandEnvironment {
    version: u8,
    source: Source,
    shell: Option<LoginShellIdentity>,
    captured_at: u64,
    path: Vec<String>,
    names: Vec<Variable>,
    excluded: Vec<String>,
    sha256: String,
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum CommandEnvironmentError {
    #[error("not a valid variable name")]
    Name,
    #[error("{0} matches *KEY*, *SECRET* or *TOKEN* and is never selectable")]
    Secret(String),
    #[error(
        "{0} is not set to text of at most {MAX_VALUE_BYTES} bytes in the Arany process environment"
    )]
    Unset(String),
    #[error("{0} is already selected")]
    Selected(String),
    #[error("{0} is not selected")]
    NotSelected(String),
    #[error("at most {MAX_NAMES} names can be selected")]
    Full,
    #[error("the command environment is too large: {0}")]
    TooLarge(&'static str),
    #[error("the command environment could not be saved")]
    Store,
    #[error("the command environment record is invalid")]
    Invalid,
    #[error("the command environment save is unconfirmed")]
    Unconfirmed,
}

pub(crate) const COMMAND_ENVIRONMENT: RecordFormat = RecordFormat {
    kind: RecordKind::CommandEnvironment,
    field: "version",
    current: VERSION as u64,
    accepts: |bytes| CommandEnvironment::parse(bytes).is_ok(),
};

fn locked<T>(
    state: &StateRoot,
    operation: impl FnOnce() -> Result<T, CommandEnvironmentError>,
) -> Result<T, CommandEnvironmentError> {
    state
        .with_record_upgrade_lock(|| Ok(operation()))
        .map_err(|_| CommandEnvironmentError::Store)?
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && !name.starts_with(|first: char| first.is_ascii_digit())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn secret(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    SECRET_PATTERNS.iter().any(|pattern| name.contains(pattern))
}

pub(super) fn valid_locale(locale: &BTreeMap<String, String>) -> bool {
    locale
        .iter()
        .all(|(name, value)| LOCALE_NAMES.contains(&name.as_str()) && valid_value(value))
}

fn valid_value(value: &str) -> bool {
    value.len() <= MAX_VALUE_BYTES && !value.contains('\0')
}

fn valid_entry(entry: &str) -> bool {
    Path::new(entry).is_absolute() && entry.len() <= MAX_PATH_BYTES && !entry.contains(['\0', ':'])
}

impl CommandEnvironment {
    pub(super) fn for_discovery(path: &Path) -> Result<Self, ToolError> {
        let found = match StateRoot::open_existing(path) {
            Ok(state) => state
                .record(RecordKind::CommandEnvironment)
                .read()
                .map_err(|_| ToolError::Configuration)?,
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(ToolError::Configuration),
        };
        // Discovery writes nothing, so an unadmitted State or a record awaiting archive previews as a fresh one.
        match found.filter(|bytes| !COMMAND_ENVIRONMENT.mismatched(bytes)) {
            Some(bytes) => Self::parse(&bytes),
            None => Self::from_process().map_err(|error| match error {
                CommandEnvironmentError::TooLarge(_) => ToolError::Limit,
                _ => ToolError::Configuration,
            }),
        }
    }

    pub fn load(state: &StateRoot) -> Result<Self, ToolError> {
        let found = state
            .record(COMMAND_ENVIRONMENT.kind)
            .read()
            .map_err(|_| ToolError::Configuration)?;
        match found {
            Some(bytes) => Self::parse(&bytes),
            None => locked(state, || Self::current_or_init(state)).map_err(|error| match error {
                CommandEnvironmentError::Unconfirmed => ToolError::CommandEnvironmentUnconfirmed,
                CommandEnvironmentError::TooLarge(_) => ToolError::Limit,
                _ => ToolError::Configuration,
            }),
        }
    }

    // The change applies to the record current under the account lock, so a stale editor never restores another editor's removal.
    pub fn edit(
        state: &StateRoot,
        change: impl FnOnce(&mut Self) -> Result<(), CommandEnvironmentError>,
    ) -> Result<Self, CommandEnvironmentError> {
        locked(state, || {
            let mut current = Self::current_or_init(state)?;
            change(&mut current)?;
            current.write(state)?;
            Ok(current)
        })
    }

    #[cfg(test)]
    pub(crate) fn save(&self, state: &StateRoot) -> Result<(), CommandEnvironmentError> {
        locked(state, || self.write(state))
    }

    fn current_or_init(state: &StateRoot) -> Result<Self, CommandEnvironmentError> {
        match state
            .record(COMMAND_ENVIRONMENT.kind)
            .read()
            .map_err(|_| CommandEnvironmentError::Store)?
        {
            Some(bytes) => Self::parse(&bytes).map_err(|_| CommandEnvironmentError::Invalid),
            None => {
                let derived = Self::from_process()?;
                derived.write(state)?;
                Ok(derived)
            }
        }
    }

    fn write(&self, state: &StateRoot) -> Result<(), CommandEnvironmentError> {
        let bytes = serde_json::to_vec(self).expect("typed record is serializable");
        if bytes.len() > RecordKind::CommandEnvironment.limit() {
            return Err(CommandEnvironmentError::TooLarge("record"));
        }
        state
            .record(RecordKind::CommandEnvironment)
            .replace(&bytes)
            .map_err(|error| match error {
                crate::store::StoreError::ReplaceUnconfirmed => {
                    CommandEnvironmentError::Unconfirmed
                }
                _ => CommandEnvironmentError::Store,
            })
    }

    pub const NAME_LIMIT: usize = MAX_NAME_BYTES;

    pub(crate) fn sha256(&self) -> &str {
        &self.sha256
    }

    pub fn source(&self) -> &'static str {
        match self.source {
            Source::Process => "process",
            Source::LoginShell => "login-shell",
        }
    }

    pub fn shell(&self) -> Option<&LoginShellIdentity> {
        self.shell.as_ref()
    }

    pub fn process_path_differs(&self) -> bool {
        std::env::var_os("PATH").unwrap_or_default() != std::ffi::OsString::from(self.path_string())
    }

    pub(crate) fn replace_captured(
        &self,
        state: &StateRoot,
        expected: &str,
    ) -> Result<(), CommandEnvironmentError> {
        locked(state, || {
            if Self::current_or_init(state)?.sha256 != expected {
                return Err(CommandEnvironmentError::Invalid);
            }
            self.write(state)
        })
    }

    pub fn path(&self) -> &[String] {
        &self.path
    }

    pub fn names(&self) -> impl Iterator<Item = (&str, &str)> {
        self.names
            .iter()
            .map(|variable| (variable.name.as_str(), variable.value.as_str()))
    }

    pub fn select(&mut self, name: &str) -> Result<(), CommandEnvironmentError> {
        self.select_from(name, |name| std::env::var(name).ok())
    }

    fn select_from(
        &mut self,
        name: &str,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<(), CommandEnvironmentError> {
        if !valid_name(name) || name == "PATH" {
            return Err(CommandEnvironmentError::Name);
        }
        if secret(name) {
            return Err(CommandEnvironmentError::Secret(name.to_owned()));
        }
        let Err(index) = self
            .names
            .binary_search_by(|entry| entry.name.as_str().cmp(name))
        else {
            return Err(CommandEnvironmentError::Selected(name.to_owned()));
        };
        if self.names.len() >= MAX_NAMES {
            return Err(CommandEnvironmentError::Full);
        }
        let value = lookup(name)
            .filter(|value| valid_value(value))
            .ok_or_else(|| CommandEnvironmentError::Unset(name.to_owned()))?;
        self.names.insert(
            index,
            Variable {
                name: name.to_owned(),
                value,
            },
        );
        self.seal();
        Ok(())
    }

    pub fn deselect(&mut self, name: &str) -> Result<(), CommandEnvironmentError> {
        let Some(index) = self.names.iter().position(|entry| entry.name == name) else {
            return Err(CommandEnvironmentError::NotSelected(name.to_owned()));
        };
        self.names.remove(index);
        self.seal();
        Ok(())
    }

    pub(crate) fn digest(&self) -> [u8; 32] {
        digest(&self.canonical())
    }

    pub(crate) fn path_string(&self) -> String {
        self.path.join(":")
    }

    pub(crate) fn locale(&self) -> BTreeMap<String, String> {
        self.names()
            .filter(|(name, _)| LOCALE_NAMES.contains(name))
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect()
    }

    // Capture time is informational; keeping it out of the identity keeps the pinned digest stable across state roots.
    fn canonical(&self) -> Vec<u8> {
        serde_json::to_vec(&(
            self.version,
            self.source,
            &self.shell,
            &self.path,
            &self.names,
            &self.excluded,
        ))
        .expect("typed record is serializable")
    }

    fn seal(&mut self) {
        self.sha256 = hex_digest(&self.canonical());
    }

    fn from_process() -> Result<Self, CommandEnvironmentError> {
        Self::derive(|name| std::env::var_os(name).and_then(|value| value.into_string().ok()))
    }

    fn derive(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, CommandEnvironmentError> {
        let mut path = Vec::new();
        let mut names = Vec::new();
        for name in CORE_NAMES {
            let Some(value) = lookup(name) else {
                continue;
            };
            if name == "PATH" {
                let entries: Vec<String> = std::env::split_paths(&value)
                    .filter_map(|entry| entry.into_os_string().into_string().ok())
                    .collect();
                if value.len() > PATH_LIMIT_BYTES
                    || entries.len() > MAX_PATH_ENTRIES
                    || entries.iter().any(|entry| entry.len() > MAX_PATH_BYTES)
                {
                    return Err(CommandEnvironmentError::TooLarge(name));
                }
                path = entries
                    .into_iter()
                    .filter(|entry| valid_entry(entry))
                    .collect();
            } else if valid_value(&value) {
                names.push(Variable {
                    name: name.to_owned(),
                    value,
                });
            } else {
                return Err(CommandEnvironmentError::TooLarge(name));
            }
        }
        names.sort_by(|left, right| left.name.cmp(&right.name));
        let mut record = Self {
            version: VERSION,
            source: Source::Process,
            shell: None,
            captured_at: super::now_ms(),
            path,
            names,
            excluded: Vec::new(),
            sha256: String::new(),
        };
        record.seal();
        Ok(record)
    }

    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn empty() -> Self {
        let mut record = Self {
            version: VERSION,
            source: Source::Process,
            shell: None,
            captured_at: 0,
            path: Vec::new(),
            names: Vec::new(),
            excluded: Vec::new(),
            sha256: String::new(),
        };
        record.seal();
        record
    }

    fn parse(bytes: &[u8]) -> Result<Self, ToolError> {
        if bytes.len() > RecordKind::CommandEnvironment.limit() {
            return Err(ToolError::Configuration);
        }
        let record: Self = serde_json::from_slice(bytes).map_err(|_| ToolError::Configuration)?;
        let sorted = |names: &mut dyn Iterator<Item = &str>| {
            let names: Vec<_> = names.collect();
            names.windows(2).all(|pair| pair[0] < pair[1])
        };
        if record.version != VERSION
            || !match (&record.source, &record.shell) {
                (Source::Process, None) => true,
                (Source::LoginShell, Some(shell)) => shell.valid(),
                _ => false,
            }
            || record.path.len() > MAX_PATH_ENTRIES
            || !record.path.iter().all(|entry| valid_entry(entry))
            || record.path_string().len() > PATH_LIMIT_BYTES
            || record.names.len() > MAX_NAMES
            || !sorted(&mut record.names().map(|(name, _)| name))
            || record.names().any(|(name, value)| {
                !valid_name(name) || name == "PATH" || secret(name) || !valid_value(value)
            })
            || record.excluded.len() > MAX_NAMES
            || !sorted(&mut record.excluded.iter().map(String::as_str))
            || record
                .excluded
                .iter()
                .any(|name| !valid_name(name) || !secret(name))
            || record.sha256 != hex_digest(&record.canonical())
        {
            return Err(ToolError::Configuration);
        }
        Ok(record)
    }

    pub(crate) fn capture_output(
        &self,
        shell: LoginShellIdentity,
        bytes: &[u8],
    ) -> Result<Self, CommandEnvironmentError> {
        if !bytes.ends_with(&[0]) {
            return Err(CommandEnvironmentError::Invalid);
        }
        let mut values = BTreeMap::new();
        let mut excluded = Vec::new();
        for entry in bytes[..bytes.len() - 1].split(|byte| *byte == 0) {
            let split = entry
                .iter()
                .position(|byte| *byte == b'=')
                .ok_or(CommandEnvironmentError::Invalid)?;
            let name = std::str::from_utf8(&entry[..split])
                .map_err(|_| CommandEnvironmentError::Invalid)?;
            if name.starts_with("BASH_FUNC_") && name.ends_with("%%") {
                continue;
            }
            if !valid_name(name) {
                return Err(CommandEnvironmentError::Invalid);
            }
            if secret(name) {
                excluded.push(name.to_owned());
            } else if name == "PATH" || self.names().any(|(selected, _)| selected == name) {
                let value = std::str::from_utf8(&entry[split + 1..])
                    .map_err(|_| CommandEnvironmentError::Invalid)?;
                if values.insert(name.to_owned(), value.to_owned()).is_some() {
                    return Err(CommandEnvironmentError::Invalid);
                }
            }
        }
        excluded.sort();
        excluded.dedup();
        if excluded.len() > MAX_NAMES || !values.contains_key("PATH") {
            return Err(CommandEnvironmentError::Invalid);
        }
        let mut record = Self::derive(|name| values.get(name).cloned())?;
        for (name, value) in &values {
            if !CORE_NAMES.contains(&name.as_str()) {
                record.select_from(name, |_| Some(value.clone()))?;
            }
        }
        record.source = Source::LoginShell;
        record.shell = Some(shell);
        record.excluded = excluded;
        record.seal();
        if serde_json::to_vec(&record).expect("typed record").len()
            > RecordKind::CommandEnvironment.limit()
        {
            return Err(CommandEnvironmentError::TooLarge("record"));
        }
        Ok(record)
    }

    #[cfg(test)]
    pub(crate) fn with_name(mut self, name: &str, value: &str) -> Self {
        self.names.push(Variable {
            name: name.into(),
            value: value.into(),
        });
        self.names.sort_by(|left, right| left.name.cmp(&right.name));
        self.seal();
        self
    }

    #[cfg(test)]
    pub(crate) fn for_path(path: &str) -> Self {
        let mut record = Self::empty();
        record.path = std::env::split_paths(path)
            .map(|entry| entry.to_string_lossy().into_owned())
            .collect();
        record.seal();
        record
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell() -> LoginShellIdentity {
        LoginShellIdentity {
            path: "/bin/sh".into(),
            device: 1,
            inode: 2,
            sha256: "1".repeat(64),
        }
    }

    #[test]
    fn login_capture_filters_secrets_preserves_selection_and_rejects_malformed_output() {
        let record = CommandEnvironment::for_path("/bin")
            .with_name("LANG", "C")
            .with_name("BUILD_MODE", "debug");
        let captured = record.capture_output(shell(), b"PATH=/usr/bin:/bin\0BASH_FUNC_which%%=() { :; }\0LANG=en_US.UTF-8\0HOME=/private\0BUILD_MODE=release\0API_TOKEN=CAPTURE_SECRET_CANARY\0client_secret=CAPTURE_SECRET_CANARY\0UNSELECTED=hidden\0").unwrap();
        assert_eq!(captured.source(), "login-shell");
        assert_eq!(captured.shell(), Some(&shell()));
        assert_eq!(
            captured.names().collect::<Vec<_>>(),
            [("BUILD_MODE", "release"), ("LANG", "en_US.UTF-8")]
        );
        assert_eq!(captured.excluded, ["API_TOKEN", "client_secret"]);
        let bytes = serde_json::to_vec(&captured).unwrap();
        assert!(
            !bytes
                .windows(b"CAPTURE_SECRET_CANARY".len())
                .any(|part| part == b"CAPTURE_SECRET_CANARY")
        );
        assert_eq!(CommandEnvironment::parse(&bytes).unwrap(), captured);
        for bytes in [
            b"PATH=/bin".as_slice(),
            b"startup banner\0PATH=/bin\0",
            b"PATH=/bin\0PATH=/usr/bin\0",
            b"LANG=C\0",
            b"PATH=/bin\0LANG=\xff\0",
        ] {
            assert!(record.capture_output(shell(), bytes).is_err());
        }
        assert_ne!(record.digest(), captured.digest());
    }

    #[test]
    fn capture_replacement_refuses_editor_drift_and_preserves_prior_bytes() {
        let (_temp, state) = state();
        let before = CommandEnvironment::load(&state).unwrap();
        let mut metadata = before.clone();
        metadata.captured_at += 1;
        metadata.seal();
        assert_eq!(metadata.digest(), before.digest());
        assert!(CommandEnvironment::parse(&serde_json::to_vec(&metadata).unwrap()).is_ok());
        let captured = before
            .capture_output(shell(), b"PATH=/bin\0LANG=C\0")
            .unwrap();
        CommandEnvironment::edit(&state, |record| {
            record.path = vec!["/edited-programs".into()];
            record.seal();
            Ok(())
        })
        .unwrap();
        let prior = state.record(RecordKind::CommandEnvironment).read().unwrap();
        assert_eq!(
            captured.replace_captured(&state, before.sha256()),
            Err(CommandEnvironmentError::Invalid)
        );
        assert_eq!(
            state.record(RecordKind::CommandEnvironment).read().unwrap(),
            prior
        );
        let current = CommandEnvironment::load(&state).unwrap();
        captured.replace_captured(&state, current.sha256()).unwrap();
        assert_eq!(CommandEnvironment::load(&state).unwrap(), captured);
    }

    fn state() -> (tempfile::TempDir, StateRoot) {
        let temp = tempfile::tempdir().unwrap();
        let state = StateRoot::admit(&temp.path().join("state")).unwrap();
        (temp, state)
    }

    #[test]
    fn discovery_preserves_absent_state_and_existing_records_without_fallback_on_damage() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("missing/state");
        let preview = CommandEnvironment::for_discovery(&path).unwrap();
        assert_eq!(preview.source(), "process");
        assert!(!temp.path().join("missing").exists());

        let state = StateRoot::admit(&path).unwrap();
        let entries = || {
            let mut names = std::fs::read_dir(&path)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        let seeded = entries();
        let preview = CommandEnvironment::for_discovery(&path).unwrap();
        assert_eq!(preview.source(), "process");
        assert_eq!(entries(), seeded, "discovery must create no files or locks");

        let selected = CommandEnvironment::for_path("/opt/selected").with_name("KEEP_ME", "1");
        selected.save(&state).unwrap();
        let record = state.record(RecordKind::CommandEnvironment);
        let saved = record.read().unwrap();
        assert_eq!(CommandEnvironment::for_discovery(&path).unwrap(), selected);
        assert_eq!(record.read().unwrap(), saved);

        let mut previous = serde_json::to_value(&selected).unwrap();
        previous["version"] = (VERSION - 1).into();
        let previous_bytes = serde_json::to_vec(&previous).unwrap();
        record.replace(&previous_bytes).unwrap();
        assert_eq!(
            CommandEnvironment::for_discovery(&path).unwrap().source(),
            "process"
        );
        assert_eq!(record.read().unwrap().unwrap(), previous_bytes);

        record.replace(b"invalid saved environment").unwrap();
        assert!(matches!(
            CommandEnvironment::for_discovery(&path),
            Err(ToolError::Configuration)
        ));
        assert_eq!(
            record.read().unwrap().unwrap(),
            b"invalid saved environment"
        );

        selected.save(&state).unwrap();
        std::fs::remove_file(path.join("events.sqlite3")).unwrap();
        let seeded = entries();
        assert_eq!(
            CommandEnvironment::for_discovery(&path).unwrap().source(),
            "process"
        );
        assert_eq!(
            entries(),
            seeded,
            "an unadmitted State previews as a new one"
        );
        assert_eq!(record.read().unwrap(), saved);
    }

    #[test]
    fn first_load_persists_the_process_derived_core_record() {
        let (_temp, state) = state();
        let first = CommandEnvironment::load(&state).unwrap();
        assert_eq!(first.source(), "process");
        assert!(
            first
                .path()
                .iter()
                .all(|entry| Path::new(entry).is_absolute())
        );
        assert!(
            first
                .names()
                .all(|(name, _)| CORE_NAMES.contains(&name) && name != "PATH")
        );
        assert_eq!(CommandEnvironment::load(&state).unwrap(), first);
        assert!(
            state
                .record(RecordKind::CommandEnvironment)
                .present()
                .unwrap()
        );
    }

    #[test]
    fn secret_pattern_names_are_never_selectable_in_any_case() {
        let (_temp, state) = state();
        let mut record = CommandEnvironment::load(&state).unwrap();
        for name in ["API_KEY", "client_secret", "GitHub_Token", "MONKEYS"] {
            assert_eq!(
                record.select(name),
                Err(CommandEnvironmentError::Secret(name.into())),
                "{name}"
            );
        }
        for name in ["", "A-B", "1A", "PATH", "A B"] {
            assert_eq!(
                record.select(name),
                Err(CommandEnvironmentError::Name),
                "{name}"
            );
        }
        assert!(record.names().all(|(name, _)| !secret(name)));
    }

    #[test]
    fn select_and_deselect_change_the_digest_and_round_trip() {
        let (_temp, state) = state();
        let mut record = CommandEnvironment::load(&state).unwrap();
        let name = "ARANY_TEST_UNSET_NAME";
        assert_eq!(
            record.select(name),
            Err(CommandEnvironmentError::Unset(name.into()))
        );
        record.names.push(Variable {
            name: "ZED".into(),
            value: "1".into(),
        });
        record.seal();
        let before = record.digest();
        assert_eq!(
            record.select("ZED"),
            Err(CommandEnvironmentError::Selected("ZED".into()))
        );
        record.deselect("ZED").unwrap();
        assert_ne!(record.digest(), before);
        record.save(&state).unwrap();
        assert_eq!(CommandEnvironment::load(&state).unwrap(), record);
        assert_eq!(
            record.deselect("ZED"),
            Err(CommandEnvironmentError::NotSelected("ZED".into()))
        );
    }

    #[test]
    fn tampered_or_malformed_records_are_rejected() {
        let record = CommandEnvironment::for_path("/usr/bin:/bin");
        let good = serde_json::to_value(&record).unwrap();
        assert!(CommandEnvironment::parse(good.to_string().as_bytes()).is_ok());
        let mutations: [(&str, serde_json::Value); 5] = [
            ("version", 4.into()),
            ("path", serde_json::json!(["relative"])),
            ("path", serde_json::json!(["/a:b"])),
            (
                "names",
                serde_json::json!([{"name": "API_TOKEN", "value": "x"}]),
            ),
            ("sha256", "0".repeat(64).into()),
        ];
        for (field, value) in mutations {
            let mut bad = good.clone();
            bad[field] = value;
            assert!(
                CommandEnvironment::parse(bad.to_string().as_bytes()).is_err(),
                "{field}"
            );
        }
        let mut extra = good;
        extra["unknown"] = true.into();
        assert!(CommandEnvironment::parse(extra.to_string().as_bytes()).is_err());
    }

    fn lookup<'a>(path: &'a str, home: &'a str) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| match name {
            "PATH" => Some(path.to_owned()),
            "HOME" => Some(home.to_owned()),
            _ => None,
        }
    }

    #[test]
    fn oversized_process_input_is_refused_not_truncated() {
        let entry = |length: usize| format!("/{}", "a".repeat(length - 1));
        let at_limit = [4096, 4096, 4096, 4093].map(entry).join(":");
        assert_eq!(at_limit.len(), PATH_LIMIT_BYTES);
        let record = CommandEnvironment::derive(lookup(&at_limit, "/h")).unwrap();
        assert_eq!(record.path_string(), at_limit);
        let over = [4096, 4096, 4096, 4094].map(entry).join(":");
        assert_eq!(
            CommandEnvironment::derive(lookup(&over, "/h")),
            Err(CommandEnvironmentError::TooLarge("PATH"))
        );
        for overlong in [
            format!("/{}:/bin", "a".repeat(PATH_LIMIT_BYTES)),
            format!("/{}:/bin", "a".repeat(MAX_PATH_BYTES)),
        ] {
            assert_eq!(
                CommandEnvironment::derive(lookup(&overlong, "/h")),
                Err(CommandEnvironmentError::TooLarge("PATH"))
            );
        }
        let many = vec!["/a"; MAX_PATH_ENTRIES + 1].join(":");
        assert_eq!(
            CommandEnvironment::derive(lookup(&many, "/h")),
            Err(CommandEnvironmentError::TooLarge("PATH"))
        );
        let value = "v".repeat(MAX_VALUE_BYTES + 1);
        assert_eq!(
            CommandEnvironment::derive(lookup("/bin", &value)),
            Err(CommandEnvironmentError::TooLarge("HOME"))
        );
        let exact = "v".repeat(MAX_VALUE_BYTES);
        assert!(CommandEnvironment::derive(lookup("/bin", &exact)).is_ok());
    }

    #[test]
    fn the_largest_valid_record_fits_the_record_limit() {
        let mut record = CommandEnvironment::empty();
        record.path = vec![format!("/{}", "a".repeat(254)); MAX_PATH_ENTRIES];
        for index in 0..MAX_NAMES {
            record.names.push(Variable {
                name: format!("N{index:0>127}"),
                value: "v".repeat(MAX_VALUE_BYTES),
            });
        }
        record.seal();
        let bytes = serde_json::to_vec(&record).unwrap();
        assert!(bytes.len() <= RecordKind::CommandEnvironment.limit());
        assert!(CommandEnvironment::parse(&bytes).is_ok());
    }

    #[test]
    fn a_save_whose_directory_sync_fails_is_unconfirmed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let state = StateRoot::admit(&path).unwrap();
        crate::fault::FaultPlan::fail_record_sync(path, "command-environment.json");
        assert_eq!(
            CommandEnvironment::for_path("/bin").save(&state),
            Err(CommandEnvironmentError::Unconfirmed)
        );
        let fresh = StateRoot::admit(&temp.path().join("first")).unwrap();
        crate::fault::FaultPlan::fail_record_sync(
            temp.path().join("first"),
            "command-environment.json",
        );
        assert!(matches!(
            CommandEnvironment::load(&fresh),
            Err(ToolError::CommandEnvironmentUnconfirmed)
        ));
    }

    #[test]
    fn initialization_rechecks_the_record_under_the_lock() {
        let (_temp, state) = state();
        let selected = CommandEnvironment::for_path("/opt/selected").with_name("KEEP_ME", "1");
        selected.save(&state).unwrap();
        let initialized = locked(&state, || CommandEnvironment::current_or_init(&state)).unwrap();
        assert_eq!(initialized, selected);
        assert_eq!(CommandEnvironment::load(&state).unwrap(), selected);
    }

    #[test]
    fn a_stale_editor_cannot_restore_a_removed_name() {
        let (_temp, state) = state();
        CommandEnvironment::for_path("/bin")
            .with_name("OLD_NAME", "1")
            .with_name("KEPT_NAME", "2")
            .save(&state)
            .unwrap();
        CommandEnvironment::edit(&state, |record| record.deselect("OLD_NAME")).unwrap();
        let edited = CommandEnvironment::edit(&state, |record| {
            record.select_from("NEW_NAME", |_| Some("3".into()))
        })
        .unwrap();
        let names: Vec<_> = edited.names().collect();
        assert_eq!(names, [("KEPT_NAME", "2"), ("NEW_NAME", "3")]);
        assert_eq!(CommandEnvironment::load(&state).unwrap(), edited);
        assert_eq!(
            CommandEnvironment::edit(&state, |record| record.deselect("OLD_NAME")),
            Err(CommandEnvironmentError::NotSelected("OLD_NAME".into()))
        );
    }

    #[test]
    fn locale_carries_only_locale_names() {
        let mut record = CommandEnvironment::for_path("/usr/bin");
        for (name, value) in [("HOME", "/h"), ("LANG", "C.UTF-8"), ("LC_ALL", "C")] {
            record.names.push(Variable {
                name: name.into(),
                value: value.into(),
            });
        }
        record
            .names
            .sort_by(|left, right| left.name.cmp(&right.name));
        let locale = record.locale();
        assert_eq!(
            locale.keys().map(String::as_str).collect::<Vec<_>>(),
            ["LANG", "LC_ALL"]
        );
    }
}
