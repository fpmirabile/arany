use super::ToolError;
use super::catalog::{nullable, string};
use super::config::Skill;
use super::fs;
use super::types::{MAX_SKILLS, MAX_SNAPSHOT_BYTES, ToolCall};
use super::types::{MAX_TOOL_RESULT_BYTES, hex_digest};
use crate::validate::{valid_name, valid_relative};
use cap_std::fs::Dir;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

pub(super) fn valid(call: &ToolCall) -> bool {
    matches!(call, ToolCall::Skill { name, resource }
        if valid_name(name) && resource.as_ref().is_none_or(|path| valid_relative(path, false)))
}

pub(super) fn schema() -> Option<Vec<(&'static str, Value)>> {
    Some(vec![("name", string()), ("resource", nullable())])
}

pub(super) fn read(
    root: &Dir,
    skill: &Skill,
    call: &ToolCall,
    result_bytes: usize,
) -> Result<String, ToolError> {
    let ToolCall::Skill { resource, .. } = call else {
        return Err(ToolError::Configuration);
    };
    let resource = resource.as_deref().unwrap_or("SKILL.md");
    let expected = skill.files.get(resource).ok_or(ToolError::Path)?;
    if fs::identity(root)? != fs::identity(&fs::open_directory(Path::new(&skill.directory))?)? {
        return Err(ToolError::ChangedInput);
    }
    let file = fs::open_file(root, resource)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if file.metadata().map_err(|_| ToolError::Path)?.mode() & 0o6000 != 0 {
            return Err(ToolError::FilesystemDenied);
        }
    }
    let bytes = fs::read_regular(file, MAX_TOOL_RESULT_BYTES)?;
    if hex_digest(&bytes) != *expected {
        return Err(ToolError::ChangedInput);
    }
    std::str::from_utf8(&bytes).map_err(|_| ToolError::Operation)?;
    let mut window = super::output::Window::new(bytes.len());
    window.push(&bytes, MAX_TOOL_RESULT_BYTES as u64)?;
    window.fitted(result_bytes.saturating_sub(2))
}

pub struct ProjectSkills {
    pub names: Vec<String>,
    pub installation_missing: bool,
}

pub fn project_skills(workspace: &Path) -> Result<ProjectSkills, ToolError> {
    let root = fs::open_directory(workspace)?;
    let Some(directory) = project_directory(&root)? else {
        return Ok(ProjectSkills {
            names: Vec::new(),
            installation_missing: optional_metadata(&root, "skills-lock.json")?
                .is_some_and(|metadata| metadata.is_file()),
        });
    };
    let mut names = Vec::new();
    for (index, entry) in directory
        .entries()
        .map_err(|_| ToolError::Path)?
        .enumerate()
    {
        if index == MAX_SKILLS * 4 {
            return Err(ToolError::Limit);
        }
        let entry = entry.map_err(|_| ToolError::Path)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| ToolError::Path)?;
        if !valid_name(&name) {
            continue;
        }
        let metadata = directory
            .symlink_metadata(&name)
            .map_err(|_| ToolError::Path)?;
        if metadata.is_symlink() {
            return Err(ToolError::Path);
        }
        if !metadata.is_dir() {
            continue;
        }
        let skill = Dir::from_std_file(fs::open_file(&directory, &name)?);
        if optional_metadata(&skill, "SKILL.md")?.is_some() {
            let file = fs::open_file(&skill, "SKILL.md")?;
            let metadata = file.metadata().map_err(|_| ToolError::Path)?;
            if !metadata.is_file() {
                return Err(ToolError::Path);
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.nlink() != 1 {
                    return Err(ToolError::Path);
                }
            }
            if metadata.len() > MAX_TOOL_RESULT_BYTES as u64 {
                return Err(ToolError::Limit);
            }
            names.push(name);
            if names.len() > MAX_SKILLS {
                return Err(ToolError::Limit);
            }
        }
    }
    names.sort();
    Ok(ProjectSkills {
        names,
        installation_missing: false,
    })
}

fn optional_metadata(root: &Dir, name: &str) -> Result<Option<cap_std::fs::Metadata>, ToolError> {
    match root.symlink_metadata(name) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(ToolError::Path),
    }
}

fn project_directory(root: &Dir) -> Result<Option<Dir>, ToolError> {
    if optional_metadata(root, ".agents")?.is_none() {
        return Ok(None);
    }
    let agents = Dir::from_std_file(fs::open_file(root, ".agents")?);
    if optional_metadata(&agents, "skills")?.is_none() {
        return Ok(None);
    }
    let skills = Dir::from_std_file(fs::open_file(&agents, "skills")?);
    if !skills.dir_metadata().map_err(|_| ToolError::Path)?.is_dir() {
        return Err(ToolError::Path);
    }
    Ok(Some(skills))
}

pub(super) fn discover(workspace: &Path, admitted: &mut Vec<Skill>) -> Result<(), ToolError> {
    let candidates = project_skills(workspace)?;
    let root = fs::open_directory(workspace)?;
    let Some(directory) = project_directory(&root)? else {
        if !candidates.names.is_empty() {
            return Err(ToolError::ChangedInput);
        }
        return Ok(());
    };
    let mut budget = DiscoveryBudget::default();
    for name in candidates.names {
        if admitted.iter().any(|skill| skill.name == name) {
            continue;
        }
        if admitted.len() == MAX_SKILLS {
            return Err(ToolError::Limit);
        }
        let dir = Dir::from_std_file(fs::open_file(&directory, &name)?);
        let mut files = BTreeMap::new();
        pin_files(&dir, "", &mut files, &mut budget)?;
        if !files.contains_key("SKILL.md") {
            return Err(ToolError::ChangedInput);
        }
        let bytes = fs::read_regular(fs::open_file(&dir, "SKILL.md")?, MAX_TOOL_RESULT_BYTES)?;
        if files["SKILL.md"] != hex_digest(&bytes) {
            return Err(ToolError::ChangedInput);
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| ToolError::Configuration)?;
        let preview = frontmatter(text)
            .unwrap_or_default()
            .chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .take(192)
            .collect::<String>();
        admitted.push(Skill {
            disable_model_invocation: disables_model_invocation(&bytes),
            name: name.clone(),
            description: if preview.trim().is_empty() {
                format!("Project Skill {name}")
            } else {
                preview
            },
            directory: workspace
                .join(".agents/skills")
                .join(&name)
                .to_str()
                .ok_or(ToolError::Path)?
                .to_owned(),
            files,
        });
    }
    Ok(())
}

#[derive(Default)]
struct DiscoveryBudget {
    entries: usize,
    bytes: usize,
}

fn pin_files(
    root: &Dir,
    prefix: &str,
    files: &mut BTreeMap<String, String>,
    budget: &mut DiscoveryBudget,
) -> Result<(), ToolError> {
    for entry in root.entries().map_err(|_| ToolError::Path)? {
        budget.entries += 1;
        if budget.entries > 8192 {
            return Err(ToolError::Limit);
        }
        let entry = entry.map_err(|_| ToolError::Path)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| ToolError::Path)?;
        let path = format!("{prefix}{name}");
        if !valid_relative(&path, false) {
            continue;
        }
        if path.split('/').count() > 16 {
            return Err(ToolError::Limit);
        }
        let file = fs::open_file(root, &name)?;
        if file.metadata().map_err(|_| ToolError::Path)?.is_dir() {
            pin_files(
                &Dir::from_std_file(file),
                &format!("{path}/"),
                files,
                budget,
            )?;
        } else {
            if files.len() == 32 {
                return Err(ToolError::Limit);
            }
            let bytes = fs::read_regular(file, MAX_TOOL_RESULT_BYTES)?;
            budget.bytes += bytes.len();
            if budget.bytes > MAX_SNAPSHOT_BYTES {
                return Err(ToolError::Limit);
            }
            files.insert(path, hex_digest(&bytes));
        }
    }
    Ok(())
}

// Discovery reads this literal opt-out only, without evaluating YAML.
pub(super) fn disables_model_invocation(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return true;
    };
    frontmatter(text).is_some_and(|header| {
        let indentation = header
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
            .map(|line| line.len() - line.trim_start().len())
            .min()
            .unwrap_or_default();
        header
            .lines()
            .filter(|line| line.len() - line.trim_start().len() == indentation)
            .any(|line| {
                let Some((key, value)) = line.split_once(':') else {
                    return false;
                };
                key.trim().trim_matches(['\'', '"']) == "disable-model-invocation"
                    && value
                        .split('#')
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .eq_ignore_ascii_case("true")
            })
    })
}

fn frontmatter(text: &str) -> Option<&str> {
    let text = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    let boundary = text
        .split_inclusive('\n')
        .scan(0, |offset, line| {
            let start = *offset;
            *offset += line.len();
            Some((start, line.trim_end_matches(['\r', '\n'])))
        })
        .find_map(|(offset, line)| (line == "---").then_some(offset))?;
    Some(&text[..boundary])
}

fn check_yaml_tokens(yaml: &str) -> Result<(), ToolError> {
    let mut quote = None;
    let mut escaped = false;
    let mut block_indent = None;
    for line in yaml.lines() {
        let indent = line.bytes().take_while(|byte| *byte == b' ').count();
        if block_indent.is_some_and(|base| line.trim().is_empty() || indent > base) {
            continue;
        }
        block_indent = None;
        let mut previous = b' ';
        let mut significant = None;
        for byte in line.bytes() {
            if let Some(delimiter) = quote {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' && delimiter == b'"' {
                    escaped = true;
                } else if byte == delimiter {
                    quote = None;
                }
            } else if byte == b'#' && previous.is_ascii_whitespace() {
                break;
            } else if matches!(byte, b'\'' | b'"')
                && (previous.is_ascii_whitespace() || matches!(previous, b'[' | b'{' | b':' | b','))
                && significant
                    .is_none_or(|last| matches!(last, b':' | b'-' | b'[' | b'{' | b',' | b'?'))
            {
                quote = Some(byte);
            } else if matches!(byte, b'&' | b'*' | b'!')
                && (previous.is_ascii_whitespace()
                    || matches!(previous, b'[' | b'{' | b':' | b',' | b'?'))
            {
                return Err(ToolError::Configuration);
            } else if matches!(byte, b'|' | b'>') && previous.is_ascii_whitespace() {
                block_indent = Some(indent);
            }
            previous = byte;
            if !byte.is_ascii_whitespace() {
                significant = Some(byte);
            }
        }
        if quote.is_some() {
            return Err(ToolError::Configuration);
        }
    }
    Ok(())
}

pub(super) fn definition_document(bytes: &[u8]) -> Result<(Value, &str), ToolError> {
    if bytes.len() > MAX_TOOL_RESULT_BYTES {
        return Err(ToolError::Limit);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ToolError::Configuration)?;
    let yaml = frontmatter(text).ok_or(ToolError::Configuration)?;
    check_yaml_tokens(yaml)?;
    let mut documents = yaml_serde::Deserializer::from_str(yaml);
    let value = crate::validate::deserialize_value_bounded(
        documents.next().ok_or(ToolError::Configuration)?,
        MAX_TOOL_RESULT_BYTES,
        4096,
        32,
        true,
    )
    .map_err(|_| ToolError::Configuration)?;
    if documents.next().is_some() || !value.is_object() {
        return Err(ToolError::Configuration);
    }
    let prefix = if text.starts_with("---\r\n") { 5 } else { 4 };
    let rest = &text[prefix + yaml.len()..];
    let body = rest
        .strip_prefix("---\r\n")
        .or_else(|| rest.strip_prefix("---\n"))
        .or_else(|| (rest == "---").then_some(""))
        .ok_or(ToolError::Configuration)?;
    Ok((value, body))
}

pub(crate) fn parse_json(bytes: &[u8], max_bytes: usize) -> Result<Value, ToolError> {
    parse_json_bounded(bytes, max_bytes, 4096, 32)
}

pub(crate) fn parse_json_bounded(
    bytes: &[u8],
    max_bytes: usize,
    max_nodes: usize,
    max_depth: usize,
) -> Result<Value, ToolError> {
    crate::validate::parse_json_bounded(bytes, max_bytes, max_nodes, max_depth).map_err(|error| {
        match error {
            crate::validate::JsonError::Limit => ToolError::Limit,
            crate::validate::JsonError::Operation => ToolError::Operation,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_opt_out_is_literal_data_without_yaml_evaluation() {
        for (declaration, disabled) in [
            ("disable-model-invocation: true", true),
            ("disable-model-invocation: true # explicit only", true),
            ("disable-model-invocation: false", false),
            ("disable-model-invocation: TRUE", true),
            ("description: |\n  disable-model-invocation: true", false),
            ("description: disable-model-invocation: true", false),
            ("allowed-tools: Bash", false),
        ] {
            let source =
                format!("---\nname: useful\ndescription: Guidance\n{declaration}\n---\nBody");
            assert_eq!(disables_model_invocation(source.as_bytes()), disabled);
        }
    }
}
