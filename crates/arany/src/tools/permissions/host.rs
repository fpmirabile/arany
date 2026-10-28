use super::{ConfigError, Decision};
use crate::tools::types::{EffectIntent, HostCommandPin};
use crate::tools::{ToolCall, ToolError};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostPrefixRule {
    version: u32,
    pub prefix: Vec<String>,
    pub decision: Decision,
    pin: HostCommandPin,
    workspace_device: u64,
    workspace_inode: u64,
    pub cwd: String,
}

fn eligible(program: &str, tokens: &[String]) -> bool {
    !crate::tools::unguarded::prohibited(program, tokens)
        && !crate::tools::unguarded::wrapper(program)
        && !crate::tools::unguarded::interpreter(program)
        && !Path::new(program)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| ["git", "gh", "rm"].contains(&name))
        && tokens.iter().all(|token| {
            token.len() <= crate::tools::process::MAX_ARGUMENT_BYTES
                && !token.chars().any(|c| {
                    c.is_control()
                        || matches!(c, ';' | '&' | '|' | '(' | ')' | '`' | '$' | '<' | '>')
                })
                && !crate::tools::unguarded::inline_option(token)
        })
}

impl HostPrefixRule {
    pub fn from_intent(intent: &EffectIntent, count: usize) -> Result<Self, ToolError> {
        let ToolCall::CommandUnguarded {
            program, args, cwd, ..
        } = &intent.call
        else {
            return Err(ToolError::Configuration);
        };
        if !crate::tools::process::valid(&intent.call) || !(1..=args.len() + 1).contains(&count) {
            return Err(ToolError::Configuration);
        }
        let mut pin = intent
            .host_command
            .clone()
            .ok_or(ToolError::Configuration)?;
        pin.permissions_digest = None;
        let rule = Self {
            version: crate::tools::unguarded::HOST_POLICY_VERSION,
            prefix: std::iter::once(program.clone())
                .chain(args.iter().cloned())
                .take(count)
                .collect(),
            decision: Decision::Allow,
            pin,
            workspace_device: intent.workspace_device,
            workspace_inode: intent.workspace_inode,
            cwd: cwd.clone(),
        };
        if !rule.valid() || !rule.matches(intent) {
            return Err(ToolError::Configuration);
        }
        Ok(rule)
    }

    pub(crate) fn valid(&self) -> bool {
        self.version == crate::tools::unguarded::HOST_POLICY_VERSION
            && self.pin.valid()
            && super::prefix::valid_path(&self.pin.workspace)
            && super::prefix::valid_path(&self.pin.executable)
            && self
                .pin
                .state_roots
                .iter()
                .all(|root| super::prefix::valid_path(root))
            && self.pin.permissions_digest.is_none()
            && self.workspace_inode != 0
            && crate::validate::valid_relative(&self.cwd, true)
            && (1..=crate::tools::process::MAX_COMMAND_ARGS + 1).contains(&self.prefix.len())
            && eligible(&self.pin.executable, &self.prefix)
            && self.prefix.first().is_some_and(|program| {
                (crate::validate::valid_name(program) || super::prefix::valid_path(program))
                    && eligible(program, &self.prefix)
            })
    }

    pub(crate) fn matches(&self, intent: &EffectIntent) -> bool {
        self.scope_matches(intent) && self.dispatch_eligible(intent)
    }

    pub(crate) fn dispatch_eligible(&self, intent: &EffectIntent) -> bool {
        let ToolCall::CommandUnguarded { program, args, .. } = &intent.call else {
            return false;
        };
        if !intent.rewrite.is_empty() {
            return false;
        }
        let Some(pin) = &intent.host_command else {
            return false;
        };
        let tokens = std::iter::once(program)
            .chain(args)
            .cloned()
            .collect::<Vec<_>>();
        eligible(program, &tokens)
            && eligible(&pin.executable, &tokens)
            && !crate::tools::unguarded::state_target(&intent.call, pin)
    }

    pub(crate) fn scope_matches(&self, intent: &EffectIntent) -> bool {
        let ToolCall::CommandUnguarded {
            program, args, cwd, ..
        } = &intent.call
        else {
            return false;
        };
        let Some(mut pin) = intent.host_command.clone() else {
            return false;
        };
        pin.permissions_digest = None;
        let tokens = std::iter::once(program)
            .chain(args)
            .cloned()
            .collect::<Vec<_>>();
        self.valid()
            && self.pin == pin
            && self.workspace_device == intent.workspace_device
            && self.workspace_inode == intent.workspace_inode
            && self.cwd == *cwd
            && tokens.starts_with(&self.prefix)
    }

    pub(super) fn in_folder(
        &self,
        path: &Path,
        identity: crate::session::WorkspaceIdentity,
    ) -> bool {
        Path::new(&self.pin.workspace) == path
            && self.workspace_device == identity.device
            && self.workspace_inode == identity.inode
    }

    pub(crate) fn admitted_by(&self, config: &crate::tools::config::Config) -> bool {
        use std::os::unix::fs::MetadataExt;
        config
            .commands
            .iter()
            .chain(config.git.iter())
            .filter(|program| {
                program.interpreter
                    || config
                        .git
                        .as_ref()
                        .is_some_and(|git| git.executable == program.executable)
                    || ["git", "gh"].contains(&program.name.as_str())
            })
            .all(|program| {
                program.executable != self.pin.executable
                    && program.sha256 != self.pin.sha256
                    && !std::fs::metadata(&program.executable).is_ok_and(|metadata| {
                        metadata.dev() == self.pin.device && metadata.ino() == self.pin.inode
                    })
            })
    }

    pub(super) fn verify_creation(
        &self,
        state: &crate::store::StateRoot,
        workspace: &Path,
    ) -> Result<(), ToolError> {
        let identity = crate::tools::workspace_identity(workspace)?;
        if identity.device != self.workspace_device
            || identity.inode != self.workspace_inode
            || Path::new(&self.pin.workspace) != workspace
        {
            return Err(ToolError::ChangedInput);
        }
        self.pin
            .verify(workspace, &crate::tools::CommandEnvironment::load(state)?)?;
        let config = match crate::tools::config::Config::load(state) {
            Ok(config) => config,
            Err(ToolError::MissingConfiguration) => {
                crate::tools::WorkspacePermissions::open(workspace)
                    .map_err(|_| ToolError::Configuration)?
                    .config(workspace)?
            }
            Err(error) => return Err(error),
        };
        if !self.admitted_by(&config) {
            return Err(ToolError::Configuration);
        }
        Ok(())
    }

    pub fn workspace(&self) -> &str {
        &self.pin.workspace
    }
}

pub(super) fn parse_rules(rules: Vec<HostPrefixRule>) -> Result<Vec<HostPrefixRule>, ConfigError> {
    for (index, rule) in rules.iter().enumerate() {
        if !rule.valid() || rules[..index].contains(rule) {
            return Err(ConfigError::Rule {
                list: "hostPrefixes",
                index,
                reason: "invalid, excluded or duplicate host prefix",
            });
        }
    }
    Ok(rules)
}
