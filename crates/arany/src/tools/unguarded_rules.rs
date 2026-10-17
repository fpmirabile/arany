use super::types::{digest, valid_name, valid_relative};
use super::{ToolError, now_ms};
use crate::session::WorkspaceIdentity;
use crate::store::{RecordKind, StateRoot};
use serde::{Deserialize, Serialize};
use std::path::Path;

const MAX_ENTRIES: usize = 128;

#[derive(Clone)]
pub(crate) struct ExactCommandRules {
    entries: Vec<ExactCommandRule>,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "pinned by the receipt format integration")
    )]
    pub(super) digest: [u8; 32],
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct ExactCommand<'a> {
    pub(crate) program: &'a str,
    pub(crate) args: &'a [String],
    pub(crate) cwd: &'a str,
}

impl ExactCommand<'_> {
    fn valid(self) -> bool {
        valid_name(self.program)
            && valid_relative(self.cwd, true)
            && self.args.len() <= 64
            && self
                .args
                .iter()
                .all(|arg| arg.len() <= 4096 && !arg.contains('\0'))
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

impl ExactCommandRules {
    pub(crate) fn load(state: &StateRoot) -> Result<Option<Self>, ToolError> {
        state
            .record(RecordKind::UnguardedCommands)
            .read()
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
                    || !entry.command().valid()
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

    pub(crate) fn matches(
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

    pub(crate) fn record(
        state: &StateRoot,
        path: &Path,
        identity: WorkspaceIdentity,
        command: ExactCommand<'_>,
    ) -> Result<(), ToolError> {
        if !valid_workspace(path, identity.inode) || !command.valid() {
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
                    .is_some_and(|rules| rules.matches(command, identity, path))
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
            .map_err(|_| ToolError::Configuration)?
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

    #[test]
    fn exact_command_rules_match_only_identical_argv_cwd_and_workspace_identity() {
        let rules = ExactCommandRules::parse(&record(vec![entry()])).unwrap();
        let args = vec!["-c".into(), "cargo test".into()];
        let command = ExactCommand {
            program: "sh",
            args: &args,
            cwd: "",
        };
        let identity = WorkspaceIdentity {
            device: 1,
            inode: 2,
        };
        let path = Path::new("/project");
        assert!(rules.matches(command, identity, path));
        for other_path in ["/another", "/project/.", "//project"] {
            assert!(
                !rules.matches(command, identity, Path::new(other_path)),
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
            assert!(!rules.matches(command, other, path));
        }
        let changed_args = vec!["-c".into(), "cargo tesT".into()];
        let extra_args = vec!["-c".into(), "cargo test".into(), "".into()];
        for other in [
            ExactCommand {
                program: "bash",
                ..command
            },
            ExactCommand {
                args: &changed_args,
                ..command
            },
            ExactCommand {
                args: &extra_args,
                ..command
            },
            ExactCommand {
                cwd: ".",
                ..command
            },
            ExactCommand {
                cwd: "src",
                ..command
            },
        ] {
            assert!(!rules.matches(other, identity, path));
        }
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
        let args = vec!["-c".into(), "cargo test".into()];
        let command = ExactCommand {
            program: "sh",
            args: &args,
            cwd: "",
        };
        ExactCommandRules::record(&state, path, identity, command).unwrap();
        let file = state.path().join("unguarded-commands.json");
        let old = std::fs::File::open(&file).unwrap();
        let bytes = std::fs::read(&file).unwrap();
        let admitted = ExactCommandRules::load(&state).unwrap().unwrap();
        ExactCommandRules::record(&state, path, identity, command).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
        let second = ExactCommand {
            cwd: "src",
            ..command
        };
        ExactCommandRules::record(&state, path, identity, second).unwrap();
        let new = std::fs::metadata(&file).unwrap();
        assert_ne!(old.metadata().unwrap().ino(), new.ino());
        assert_eq!(new.permissions().mode() & 0o777, 0o600);
        assert_eq!(new.nlink(), 1);
        assert!(!state.path().join("unguarded-commands.pending").exists());
        assert!(!admitted.matches(second, identity, path));
        assert!(
            ExactCommandRules::load(&state)
                .unwrap()
                .unwrap()
                .matches(second, identity, path)
        );
        assert_eq!(admitted.digest, digest(&bytes));
        let before = std::fs::read(&file).unwrap();
        state
            .with_account_replacement_lock(path, || {
                assert!(matches!(
                    ExactCommandRules::record(&state, path, identity, command),
                    Err(ToolError::Configuration)
                ));
            })
            .unwrap();
        let outside = temp.path().join("outside");
        std::fs::write(&outside, b"unchanged").unwrap();
        let pending = state.path().join("unguarded-commands.pending");
        std::os::unix::fs::symlink(&outside, &pending).unwrap();
        assert!(matches!(
            ExactCommandRules::record(
                &state,
                path,
                identity,
                ExactCommand {
                    cwd: "other",
                    ..command
                }
            ),
            Err(ToolError::Configuration)
        ));
        assert_eq!(std::fs::read(&file).unwrap(), before);
        assert_eq!(std::fs::read(&outside).unwrap(), b"unchanged");
        std::fs::remove_file(pending).unwrap();
        let oversized_args = vec!["x".repeat(4096); 16];
        assert!(matches!(
            ExactCommandRules::record(
                &state,
                path,
                identity,
                ExactCommand {
                    args: &oversized_args,
                    ..command
                }
            ),
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
            ExactCommandRules::record(&state, path, identity, command),
            Err(ToolError::Limit)
        ));
        assert_eq!(std::fs::read(&file).unwrap(), full);
        ExactCommandRules::record(
            &state,
            path,
            identity,
            ExactCommand {
                cwd: "dir0",
                ..command
            },
        )
        .unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), full);
    }
}
