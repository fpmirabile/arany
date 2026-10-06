use super::ToolError;
use super::config::Skill;
use super::fs;
use super::types::{MAX_SKILLS, MAX_SNAPSHOT_BYTES, valid_name, valid_relative};
use super::types::{MAX_TOOL_RESULT_BYTES, hex_digest};
use cap_std::fs::Dir;
use serde::de::{DeserializeSeed, Error, MapAccess, SeqAccess, Visitor};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

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

struct Budget {
    nodes: usize,
    bytes: usize,
    max_nodes: usize,
    max_bytes: usize,
    max_depth: usize,
}
struct Bounded<'a> {
    budget: &'a mut Budget,
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for Bounded<'_> {
    type Value = Value;
    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        if self.depth > self.budget.max_depth || self.budget.nodes >= self.budget.max_nodes {
            return Err(D::Error::custom("metadata limit"));
        }
        self.budget.nodes += 1;
        deserializer.deserialize_any(self)
    }
}

impl Bounded<'_> {
    fn scalar<E: Error>(&mut self, value: &str) -> Result<(), E> {
        self.budget.bytes = self
            .budget
            .bytes
            .checked_add(value.len())
            .ok_or_else(|| E::custom("metadata limit"))?;
        if self.budget.bytes > self.budget.max_bytes {
            return Err(E::custom("metadata limit"));
        }
        Ok(())
    }
}

impl<'de> Visitor<'de> for Bounded<'_> {
    type Value = Value;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bounded portable Skill metadata")
    }
    fn visit_str<E: Error>(mut self, value: &str) -> Result<Value, E> {
        self.scalar(value)?;
        Ok(json!(value))
    }
    fn visit_string<E: Error>(mut self, value: String) -> Result<Value, E> {
        self.scalar(&value)?;
        Ok(json!(value))
    }
    fn visit_bool<E: Error>(self, value: bool) -> Result<Value, E> {
        Ok(json!(value))
    }
    fn visit_i64<E: Error>(self, value: i64) -> Result<Value, E> {
        Ok(json!(value))
    }
    fn visit_u64<E: Error>(self, value: u64) -> Result<Value, E> {
        Ok(json!(value))
    }
    fn visit_f64<E: Error>(self, value: f64) -> Result<Value, E> {
        if value.is_finite() {
            Ok(json!(value))
        } else {
            Err(E::custom("nonfinite metadata"))
        }
    }
    fn visit_unit<E: Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_none<E: Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(Bounded {
            budget: self.budget,
            depth: self.depth + 1,
        })? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut values = serde_json::Map::new();
        let mut seen = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(A::Error::custom("duplicate metadata key"));
            }
            self.budget.bytes = self
                .budget
                .bytes
                .checked_add(key.len())
                .ok_or_else(|| A::Error::custom("metadata limit"))?;
            if self.budget.bytes > self.budget.max_bytes {
                return Err(A::Error::custom("metadata limit"));
            }
            let value = map.next_value_seed(Bounded {
                budget: self.budget,
                depth: self.depth + 1,
            })?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

pub(super) fn metadata(expected_name: &str, bytes: &[u8]) -> Result<Value, ToolError> {
    if bytes.len() > MAX_TOOL_RESULT_BYTES {
        return Err(ToolError::Limit);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ToolError::Configuration)?;
    let yaml = frontmatter(text).ok_or(ToolError::Configuration)?;
    let mut documents = yaml_serde::Deserializer::from_str(yaml);
    let mut budget = Budget {
        nodes: 0,
        bytes: 0,
        max_nodes: 256,
        max_bytes: 16 * 1024,
        max_depth: 8,
    };
    let metadata = Bounded {
        budget: &mut budget,
        depth: 0,
    }
    .deserialize(documents.next().ok_or(ToolError::Configuration)?)
    .map_err(|_| ToolError::Configuration)?;
    if documents.next().is_some()
        || !metadata.is_object()
        || metadata["name"].as_str() != Some(expected_name)
    {
        return Err(ToolError::Configuration);
    }
    let description = metadata["description"]
        .as_str()
        .filter(|value| !value.trim().is_empty() && value.len() <= 1024)
        .ok_or(ToolError::Configuration)?;
    Ok(
        json!({"name":expected_name,"description":description,"sha256":hex_digest(bytes),"behavioral_fields":"guidance only; no permissions, interpolation, installation or fork activation"}),
    )
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

pub(super) fn parse_json(bytes: &[u8], max_bytes: usize) -> Result<Value, ToolError> {
    if bytes.len() > max_bytes {
        return Err(ToolError::Limit);
    }
    let mut budget = Budget {
        nodes: 0,
        bytes: 0,
        max_nodes: 4096,
        max_bytes,
        max_depth: 32,
    };
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let value = Bounded {
        budget: &mut budget,
        depth: 0,
    }
    .deserialize(&mut decoder)
    .map_err(|_| ToolError::Operation)?;
    decoder.end().map_err(|_| ToolError::Operation)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn portable_metadata_accepts_real_scalars_and_rejects_ambiguity() {
        for source in [
            "---\nname: useful\ndescription: 'Useful: a skill'\n---\nBody",
            "---\nname: useful\ndescription: >\n  Read selected files\n  carefully.\nallowed-tools: Bash\n---\nBody",
            "---\r\nname: useful\r\ndescription: >-\r\n  Read selected files\r\n---\r\nBody",
            "---\nname: useful\ndescription: !!str Read selected files\n---\nBody",
        ] {
            assert_eq!(
                metadata("useful", source.as_bytes()).expect("valid metadata")["name"],
                "useful"
            );
        }
        for source in [
            "---\nname: useful\nname: shadow\ndescription: ok\n---\nbody",
            "---\nname: other\ndescription: ok\n---\nbody",
            "---\nname: useful\ndescription: !execute whoami\n---\nbody",
            "body",
            "---\nname: useful\ndescription: false\n---\nbody",
        ] {
            assert!(
                metadata("useful", source.as_bytes()).is_err(),
                "hostile metadata"
            );
        }
    }
}
