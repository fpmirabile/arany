use super::types::digest;
use super::{ToolCall, ToolError, now_ms};
use crate::session::WorkspaceIdentity;
use crate::store::{RecordFormat, RecordKind, StateRoot, StoreError};
use serde::{Deserialize, Serialize};
use std::path::Path;

const MAX_ENTRIES: usize = 128;

#[derive(Clone)]
pub(crate) struct ExactCommandRules {
    entries: Vec<ExactCommandRule>,
    pub(super) digest: [u8; 32],
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct ExactCommand<'a> {
    program: &'a str,
    args: &'a [String],
    cwd: &'a str,
}

impl<'a> ExactCommand<'a> {
    fn from_call(call: &'a ToolCall) -> Option<Self> {
        let ToolCall::CommandUnguarded {
            program, args, cwd, ..
        } = call
        else {
            return None;
        };
        super::process::valid(call).then_some(Self { program, args, cwd })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleRecord {
    version: u8,
    commands: Vec<ExactCommandRule>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExactCommandRule {
    workspace: Workspace,
    program: String,
    args: Vec<String>,
    cwd: String,
    created_unix_ms: u64,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Workspace {
    path: String,
    device: u64,
    inode: u64,
}

impl ExactCommandRule {
    fn command(&self) -> ExactCommand<'_> {
        ExactCommand {
            program: &self.program,
            args: &self.args,
            cwd: &self.cwd,
        }
    }
}

pub(crate) const UNGUARDED_COMMANDS: RecordFormat = RecordFormat {
    kind: RecordKind::UnguardedCommands,
    field: "version",
    floor: 1,
    current: 1,
    steps: &[],
    accepts: |bytes| ExactCommandRules::parse(bytes).is_ok(),
};

impl ExactCommandRules {
    pub(crate) fn load(state: &StateRoot) -> Result<Option<Self>, ToolError> {
        state
            .read_current(&UNGUARDED_COMMANDS)
            .map_err(|_| ToolError::Configuration)?
            .map(|bytes| Self::parse(&bytes))
            .transpose()
    }

    fn parse(bytes: &[u8]) -> Result<Self, ToolError> {
        if bytes.len() > RecordKind::UnguardedCommands.limit() {
            return Err(ToolError::Configuration);
        }
        let record: RuleRecord =
            serde_json::from_slice(bytes).map_err(|_| ToolError::Configuration)?;
        if record.version != 1
            || record.commands.len() > MAX_ENTRIES
            || record.commands.iter().enumerate().any(|(index, entry)| {
                !valid_workspace(Path::new(&entry.workspace.path), entry.workspace.inode)
                    || !super::process::valid_command(&entry.program, &entry.args, &entry.cwd)
                    || record.commands[..index].iter().any(|prior| {
                        prior.workspace == entry.workspace && prior.command() == entry.command()
                    })
            })
        {
            return Err(ToolError::Configuration);
        }
        Ok(Self {
            entries: record.commands,
            digest: digest(bytes),
        })
    }

    pub(super) fn matches(
        &self,
        call: &ToolCall,
        identity: WorkspaceIdentity,
        path: &Path,
    ) -> bool {
        let Some(command) = ExactCommand::from_call(call) else {
            return false;
        };
        self.matches_command(command, identity, path)
    }

    fn matches_command(
        &self,
        command: ExactCommand<'_>,
        identity: WorkspaceIdentity,
        path: &Path,
    ) -> bool {
        self.entries.iter().any(|entry| {
            std::ffi::OsStr::new(&entry.workspace.path) == path.as_os_str()
                && entry.workspace.device == identity.device
                && entry.workspace.inode == identity.inode
                && entry.command() == command
        })
    }

    pub(super) fn record(
        state: &StateRoot,
        path: &Path,
        identity: WorkspaceIdentity,
        call: &ToolCall,
    ) -> Result<(), ToolError> {
        let command = ExactCommand::from_call(call).ok_or(ToolError::Configuration)?;
        if !valid_workspace(path, identity.inode) {
            return Err(ToolError::Configuration);
        }
        state
            .with_account_replacement_lock(path, || {
                if super::workspace_identity(path)? != identity {
                    return Err(ToolError::ChangedInput);
                }
                let rules = Self::load(state)?;
                if rules
                    .as_ref()
                    .is_some_and(|rules| rules.matches_command(command, identity, path))
                {
                    return Ok(());
                }
                let mut entries = rules.map_or_else(Vec::new, |rules| rules.entries);
                if entries.len() == MAX_ENTRIES {
                    return Err(ToolError::Limit);
                }
                entries.push(ExactCommandRule {
                    workspace: Workspace {
                        path: path.to_str().ok_or(ToolError::Configuration)?.to_owned(),
                        device: identity.device,
                        inode: identity.inode,
                    },
                    program: command.program.to_owned(),
                    args: command.args.to_vec(),
                    cwd: command.cwd.to_owned(),
                    created_unix_ms: now_ms(),
                });
                let bytes = serde_json::to_vec(&RuleRecord {
                    version: 1,
                    commands: entries,
                })
                .map_err(|_| ToolError::Configuration)?;
                if bytes.len() > RecordKind::UnguardedCommands.limit() {
                    return Err(ToolError::Limit);
                }
                state
                    .record(RecordKind::UnguardedCommands)
                    .replace(&bytes)
                    .map_err(|_| ToolError::Configuration)
            })
            .map_err(|error| match error {
                StoreError::AccountBusy => ToolError::AccountBusy,
                _ => ToolError::Configuration,
            })?
    }
}

fn valid_workspace(path: &Path, inode: u64) -> bool {
    path.is_absolute()
        && path.as_os_str().len() <= 4096
        && path.to_str().is_some_and(|path| !path.contains('\0'))
        && inode != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn entry() -> Value {
        json!({
            "workspace": {"path":"/project", "device":1, "inode":2},
            "program":"sh", "args":["-c", "cargo test"], "cwd":"",
            "created_unix_ms": 1
        })
    }

    fn record(commands: Vec<Value>) -> Vec<u8> {
        serde_json::to_vec(&json!({"version":1, "commands":commands})).unwrap()
    }

    fn command(cwd: &str) -> ToolCall {
        ToolCall::CommandUnguarded {
            program: "sh".into(),
            args: vec!["-c".into(), "cargo test".into()],
            cwd: cwd.into(),
            justification: "Allow this exact command?".into(),
        }
    }

    #[test]
    fn exact_command_rules_match_only_identical_argv_cwd_and_workspace_identity() {
        let rules = ExactCommandRules::parse(&record(vec![entry()])).unwrap();
        let call = command("");
        let identity = WorkspaceIdentity {
            device: 1,
            inode: 2,
        };
        let path = Path::new("/project");
        assert!(rules.matches(&call, identity, path));
        for other_path in ["/another", "/project/.", "//project"] {
            assert!(
                !rules.matches(&call, identity, Path::new(other_path)),
                "{other_path}"
            );
        }
        for other in [
            WorkspaceIdentity {
                device: 2,
                ..identity
            },
            WorkspaceIdentity {
                inode: 3,
                ..identity
            },
        ] {
            assert!(!rules.matches(&call, other, path));
        }
        for changed_field in ["program", "arg-byte", "extra-arg", "justification"] {
            let mut other = call.clone();
            let ToolCall::CommandUnguarded {
                program,
                args,
                justification,
                ..
            } = &mut other
            else {
                unreachable!();
            };
            match changed_field {
                "program" => *program = "bash".into(),
                "arg-byte" => args[1] = "cargo tesT".into(),
                "extra-arg" => args.push(String::new()),
                "justification" => justification.clear(),
                _ => unreachable!(),
            }
            assert!(!rules.matches(&other, identity, path), "{changed_field}");
        }
        for other in [
            command("."),
            command("src"),
            ToolCall::Command {
                program: "sh".into(),
                args: vec!["-c".into(), "cargo test".into()],
                cwd: "".into(),
            },
        ] {
            assert!(!rules.matches(&other, identity, path));
        }
        let mut other_reason = call;
        let ToolCall::CommandUnguarded { justification, .. } = &mut other_reason else {
            unreachable!();
        };
        *justification = "A different question for the same command?".into();
        assert!(rules.matches(&other_reason, identity, path));
    }

    #[test]
    fn exact_command_rules_reject_invalid_schema_duplicates_and_command_bounds() {
        let mut cases = vec![
            b"{".to_vec(),
            br#"{"version":2,"commands":[]}"#.to_vec(),
            br#"{"version":1,"version":1,"commands":[]}"#.to_vec(),
            br#"{"version":1,"commands":[],"extra":true}"#.to_vec(),
            vec![b' '; RecordKind::UnguardedCommands.limit() + 1],
        ];
        let mut duplicate = entry();
        duplicate["created_unix_ms"] = json!(2);
        cases.push(record(vec![entry(), duplicate]));
        cases.push(record(
            (0..129)
                .map(|index| {
                    let mut entry = entry();
                    entry["cwd"] = json!(format!("dir{index}"));
                    entry
                })
                .collect(),
        ));
        for (field, value) in [
            ("unknown", json!(true)),
            ("program", json!("sh*")),
            ("args", json!(["bad\0arg"])),
            ("args", json!(["x".repeat(4097)])),
            ("args", json!(vec!["a"; 65])),
            ("cwd", json!("../outside")),
            ("cwd", json!("x".repeat(513))),
        ] {
            let mut entry = entry();
            entry[field] = value;
            cases.push(record(vec![entry]));
        }
        for (field, value) in [
            ("unknown", json!(true)),
            ("path", json!("relative")),
            ("path", json!("/x\0bad")),
            ("path", json!(format!("/{}", "x".repeat(4096)))),
            ("inode", json!(0)),
        ] {
            let mut entry = entry();
            entry["workspace"][field] = value;
            cases.push(record(vec![entry]));
        }
        for (index, bytes) in cases.iter().enumerate() {
            assert!(
                matches!(
                    ExactCommandRules::parse(bytes),
                    Err(ToolError::Configuration)
                ),
                "case {index}"
            );
        }
        let bytes = record(vec![entry()]);
        assert_eq!(
            ExactCommandRules::parse(&bytes).unwrap().digest,
            digest(&bytes)
        );
    }

    #[test]
    fn recording_a_rule_is_atomic_and_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let state = StateRoot::admit(&temp.path().join("state")).unwrap();
        assert!(ExactCommandRules::load(&state).unwrap().is_none());
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let path = workspace.as_path();
        let identity = super::super::workspace_identity(path).unwrap();
        let call = command("");
        ExactCommandRules::record(&state, path, identity, &call).unwrap();
        let file = state.path().join("unguarded-commands.json");
        let old = std::fs::File::open(&file).unwrap();
        let bytes = std::fs::read(&file).unwrap();
        ExactCommandRules::record(&state, path, identity, &call).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
        ExactCommandRules::record(&state, path, identity, &command("src")).unwrap();
        let new = std::fs::metadata(&file).unwrap();
        assert_ne!(old.metadata().unwrap().ino(), new.ino());
        assert_eq!(new.permissions().mode() & 0o777, 0o600);
        assert_eq!(new.nlink(), 1);
        assert!(!state.path().join("unguarded-commands.pending").exists());
        let before = std::fs::read(&file).unwrap();
        state
            .with_account_replacement_lock(path, || {
                let error = ExactCommandRules::record(&state, path, identity, &call).unwrap_err();
                assert!(matches!(error, ToolError::AccountBusy));
                assert!(error.to_string().contains("retry"));
            })
            .unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), before);
        let outside = temp.path().join("outside");
        std::fs::write(&outside, b"unchanged").unwrap();
        let pending = state.path().join("unguarded-commands.pending");
        std::os::unix::fs::symlink(&outside, &pending).unwrap();
        assert!(matches!(
            ExactCommandRules::record(&state, path, identity, &command("other")),
            Err(ToolError::Configuration)
        ));
        assert_eq!(std::fs::read(&file).unwrap(), before);
        assert_eq!(std::fs::read(&outside).unwrap(), b"unchanged");
        std::fs::remove_file(pending).unwrap();
        let mut oversized = call.clone();
        let ToolCall::CommandUnguarded { args, .. } = &mut oversized else {
            unreachable!();
        };
        *args = vec!["x".repeat(4096); 16];
        assert!(matches!(
            ExactCommandRules::record(&state, path, identity, &oversized),
            Err(ToolError::Limit)
        ));
        assert_eq!(std::fs::read(&file).unwrap(), before);
        let entries = (0..128)
            .map(|index| {
                let mut entry = entry();
                entry["workspace"] =
                    json!({"path":path,"device":identity.device,"inode":identity.inode});
                entry["cwd"] = json!(format!("dir{index}"));
                entry
            })
            .collect();
        let full = record(entries);
        state
            .record(RecordKind::UnguardedCommands)
            .replace(&full)
            .unwrap();
        assert!(matches!(
            ExactCommandRules::record(&state, path, identity, &call),
            Err(ToolError::Limit)
        ));
        assert_eq!(std::fs::read(&file).unwrap(), full);
        ExactCommandRules::record(&state, path, identity, &command("dir0")).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), full);
    }
}
