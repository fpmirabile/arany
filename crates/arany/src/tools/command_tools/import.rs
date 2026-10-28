use super::super::permissions::{CommandHook, HookEvent, PermissionConfig};
use super::*;

pub struct CommandToolImport {
    pub candidates: Vec<CommandTool>,
    pub refused: usize,
}

pub fn import_command_tools(
    state: &StateRoot,
    workspace: &Path,
) -> Result<CommandToolImport, ToolError> {
    let user = nix::unistd::User::from_uid(nix::unistd::geteuid())
        .map_err(|_| ToolError::Configuration)?
        .ok_or(ToolError::Configuration)?;
    import_from(state, workspace, &user.dir)
}

fn settings_bytes(home: &Path, workspace: &Path) -> Result<Vec<u8>, ToolError> {
    use cap_std::fs::MetadataExt as _;

    let home = home.canonicalize().map_err(|_| ToolError::Path)?;
    if home.starts_with(workspace) {
        return Err(ToolError::Path);
    }
    let directory = super::super::fs::open_directory(&home.join(".claude"))?;
    let metadata = directory.dir_metadata().map_err(|_| ToolError::Path)?;
    let uid = rustix::process::geteuid().as_raw();
    if metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
        return Err(ToolError::Path);
    }
    let file = super::super::fs::open_file(&directory, "settings.json")?;
    let metadata = file.metadata().map_err(|_| ToolError::Path)?;
    if metadata.uid() != uid || metadata.nlink() != 1 || metadata.mode() & 0o022 != 0 {
        return Err(ToolError::Path);
    }
    super::super::fs::read_regular(file, super::super::permissions::MAX_CONFIG_BYTES)
}

fn import_hooks(bytes: &[u8]) -> Result<PermissionConfig, ToolError> {
    let value = super::super::parse_json(bytes, super::super::permissions::MAX_CONFIG_BYTES)?;
    let hooks = value
        .as_object()
        .ok_or(ToolError::Configuration)?
        .get("hooks");
    let groups = match hooks {
        Some(hooks) => hooks
            .as_object()
            .ok_or(ToolError::Configuration)?
            .get("PreToolUse")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([])),
        None => serde_json::json!([]),
    };
    PermissionConfig::parse(
        &serde_json::to_vec(&serde_json::json!({"hooks":{"PreToolUse":groups}}))
            .expect("bounded settings serialize"),
    )
    .map_err(|_| ToolError::Configuration)
}

fn import_from(
    state: &StateRoot,
    workspace: &Path,
    home: &Path,
) -> Result<CommandToolImport, ToolError> {
    let workspace = workspace.canonicalize().map_err(|_| ToolError::Path)?;
    let config = import_hooks(&settings_bytes(home, &workspace)?)?;
    let environment = super::super::CommandEnvironment::load(state)?;
    let mut result = CommandToolImport {
        candidates: Vec::new(),
        refused: 0,
    };
    for group in config.hooks().groups(HookEvent::PreToolUse) {
        if !group
            .matches("Bash")
            .map_err(|_| ToolError::Configuration)?
        {
            continue;
        }
        for hook in group.commands() {
            match candidate(hook, &environment, state, &workspace) {
                Some(tool) if !result.candidates.contains(&tool) => {
                    if result.candidates.len() >= MAX_COMMAND_TOOLS {
                        return Err(ToolError::Limit);
                    }
                    result.candidates.push(tool);
                }
                Some(_) => {}
                None => result.refused += 1,
            }
        }
    }
    Ok(result)
}

fn candidate(
    hook: &CommandHook,
    environment: &super::super::CommandEnvironment,
    state: &StateRoot,
    workspace: &Path,
) -> Option<CommandTool> {
    let mut words = literal_words(hook.command())?;
    if let Some(args) = hook.args() {
        if words.len() != 1 {
            return None;
        }
        words.extend_from_slice(args);
    }
    let path = resolve(environment, words.first()?)?;
    CommandTool::pin(state, workspace, path.to_str()?, words[1..].to_vec()).ok()
}

fn resolve(
    environment: &super::super::CommandEnvironment,
    program: &str,
) -> Option<std::path::PathBuf> {
    let path = Path::new(program);
    if path.is_absolute() {
        return Some(path.to_owned());
    }
    if program.contains(['/', '=']) {
        return None;
    }
    environment
        .path()
        .iter()
        .map(|directory| Path::new(directory).join(program))
        .find(|path| {
            path.metadata()
                .is_ok_and(|metadata| metadata.is_file() && metadata.mode() & 0o111 != 0)
        })
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Quote {
    None,
    Single,
    Double,
}

fn literal_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = Quote::None;
    let mut escaped = false;
    let mut started = false;
    for ch in command.chars() {
        if ch.is_control() {
            return None;
        }
        if escaped {
            if quote == Quote::Double && !matches!(ch, '$' | '`' | '\\' | '"') {
                word.push('\\');
            }
            word.push(ch);
            escaped = false;
        } else if ch == '\\' && quote != Quote::Single {
            escaped = true;
            started = true;
        } else if ch == '\'' && quote != Quote::Double {
            quote = if quote == Quote::Single {
                Quote::None
            } else {
                Quote::Single
            };
            started = true;
        } else if ch == '"' && quote != Quote::Single {
            quote = if quote == Quote::Double {
                Quote::None
            } else {
                Quote::Double
            };
            started = true;
        } else if quote == Quote::None && ch.is_whitespace() {
            if started {
                words.push(std::mem::take(&mut word));
                started = false;
            }
        } else if quote != Quote::Single && matches!(ch, '$' | '`')
            || quote == Quote::None
                && matches!(
                    ch,
                    ';' | '&'
                        | '|'
                        | '<'
                        | '>'
                        | '('
                        | ')'
                        | '*'
                        | '?'
                        | '['
                        | '{'
                        | '~'
                        | '#'
                        | '!'
                )
        {
            return None;
        } else {
            word.push(ch);
            started = true;
        }
    }
    if quote != Quote::None || escaped {
        return None;
    }
    if started {
        words.push(word);
    }
    (!words.is_empty() && !words[0].is_empty()).then_some(words)
}

#[cfg(test)]
mod tests;
