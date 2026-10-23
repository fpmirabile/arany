use super::ToolError;
use crate::store::{RecordFormat, RecordKind, StateRoot};
use crate::validate::MAX_PATH_BYTES;
use serde::{Deserialize, Serialize};
use std::path::Path;

const MAX_ENTRIES: usize = 128;

#[derive(Clone)]
pub(crate) struct ExactCommandRules;

#[derive(Clone, Copy, Eq, PartialEq)]
struct ExactCommand<'a> {
    program: &'a str,
    args: &'a [String],
    cwd: &'a str,
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
            .record(RecordKind::UnguardedCommands)
            .present()
            .map(|present| present.then_some(Self))
            .map_err(|_| ToolError::Configuration)
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
        Ok(Self)
    }
}

fn valid_workspace(path: &Path, inode: u64) -> bool {
    path.is_absolute()
        && path.as_os_str().len() <= MAX_PATH_BYTES
        && path.to_str().is_some_and(|path| !path.contains('\0'))
        && inode != 0
}
