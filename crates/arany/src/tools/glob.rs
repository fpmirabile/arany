use super::ToolError;
use super::config::Config;
use super::fs::{identity, ignored_directory, open_file};
use super::types::{MAX_SNAPSHOT_FILES, ToolCall, forbidden, valid_relative};
use cap_std::fs::Dir;
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

const MAX_GLOB_ENTRIES: u32 = 100;
const MAX_PATTERN_BYTES: usize = 512;
const MAX_ALTERNATIVES: usize = 16;
const MAX_TRANSLATION_BYTES: usize = 8 * 1024;
const MAX_SCANNED_ENTRIES: usize = MAX_SNAPSHOT_FILES * 4;
const MAX_CANDIDATES: usize = MAX_SNAPSHOT_FILES;
const MAX_DEPTH: usize = 16;

pub(super) fn default_path() -> String {
    ".".into()
}

pub(super) const fn default_limit() -> u32 {
    MAX_GLOB_ENTRIES
}

pub(super) fn path_or_default<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?
        .filter(|path| !path.is_empty())
        .unwrap_or_else(default_path))
}

pub(super) fn limit_or_default<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<u32, D::Error> {
    Ok(Option::<u32>::deserialize(deserializer)?.unwrap_or(MAX_GLOB_ENTRIES))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PatternError {
    Length,
    Control,
    Anchored,
    Negation,
    Escape,
    Class,
    EmptyComponent,
    DotComponent,
    Recursive,
    Brace,
    Alternatives,
    Size,
}

// Discovery only: permission path rules keep their own semantics. Search's include filter should reuse this.
pub(super) fn matcher(pattern: &str) -> Result<Regex, PatternError> {
    RegexBuilder::new(&translate(pattern)?)
        .size_limit(128 * 1024)
        .dfa_size_limit(32 * 1024)
        .nest_limit(32)
        .build()
        .map_err(|_| PatternError::Size)
}

fn translate(pattern: &str) -> Result<String, PatternError> {
    if pattern.is_empty() || pattern.len() > MAX_PATTERN_BYTES {
        return Err(PatternError::Length);
    }
    if pattern.chars().any(char::is_control) {
        return Err(PatternError::Control);
    }
    if pattern.starts_with(['/', '~']) {
        return Err(PatternError::Anchored);
    }
    if pattern.starts_with('!') {
        return Err(PatternError::Negation);
    }
    if pattern.contains('\\') {
        return Err(PatternError::Escape);
    }
    if pattern.contains(['[', ']']) {
        return Err(PatternError::Class);
    }
    let components: Vec<_> = pattern.split('/').collect();
    let mut regex = String::from(r"\A");
    let mut alternatives = 0;
    for (index, component) in components.iter().enumerate() {
        let last = index + 1 == components.len();
        match *component {
            "" => return Err(PatternError::EmptyComponent),
            "." | ".." => return Err(PatternError::DotComponent),
            "**" if last => regex.push_str(".+"),
            "**" => regex.push_str("(?:[^/]+/)*"),
            _ if component.contains("**") => return Err(PatternError::Recursive),
            _ => {
                translate_component(component, &mut regex, &mut alternatives)?;
                if !last {
                    regex.push('/');
                }
            }
        }
    }
    regex.push_str(r"\z");
    if regex.len() > MAX_TRANSLATION_BYTES {
        return Err(PatternError::Size);
    }
    Ok(regex)
}

fn translate_component(
    component: &str,
    regex: &mut String,
    alternatives: &mut usize,
) -> Result<(), PatternError> {
    let mut rest = component;
    while let Some(start) = rest.find(['{', '}']) {
        translate_wildcards(&rest[..start], regex);
        let group = rest[start..].strip_prefix('{').ok_or(PatternError::Brace)?;
        let end = group.find(['{', '}']).ok_or(PatternError::Brace)?;
        if !group[end..].starts_with('}') {
            return Err(PatternError::Brace);
        }
        let options: Vec<_> = group[..end].split(',').collect();
        if options.len() < 2 || options.iter().any(|option| option.is_empty()) {
            return Err(PatternError::Brace);
        }
        *alternatives += options.len();
        if *alternatives > MAX_ALTERNATIVES {
            return Err(PatternError::Alternatives);
        }
        regex.push_str("(?:");
        for (index, option) in options.iter().enumerate() {
            if index > 0 {
                regex.push('|');
            }
            translate_wildcards(option, regex);
        }
        regex.push(')');
        rest = &group[end + 1..];
    }
    translate_wildcards(rest, regex);
    Ok(())
}

fn translate_wildcards(text: &str, regex: &mut String) {
    let mut literal = String::new();
    for character in text.chars() {
        let wildcard = match character {
            '*' => "[^/]*",
            '?' => "[^/]",
            _ => {
                literal.push(character);
                continue;
            }
        };
        regex.push_str(&regex::escape(&literal));
        literal.clear();
        regex.push_str(wildcard);
    }
    regex.push_str(&regex::escape(&literal));
}

pub(super) fn valid(call: &ToolCall) -> bool {
    matches!(call, ToolCall::Glob { pattern, path, limit }
        if valid_relative(path, true)
            && (1..=MAX_GLOB_ENTRIES).contains(limit)
            && matcher(pattern).is_ok())
}

pub(super) fn schema() -> Vec<(&'static str, Value)> {
    vec![
        (
            "pattern",
            json!({"type":"string", "description":"Glob matched against each regular file's path relative to path, case-sensitively. * and ? stay within one path component; a whole ** component crosses zero or more directories, so use **/ for deeper files; {a,b} lists at most 16 alternatives in total. Bracket classes, backslash escapes, a leading !, absolute or ~ prefixes and . or .. components are refused."}),
        ),
        (
            "path",
            json!({"type":["string", "null"], "description":"Workspace-relative directory to search; null searches the whole Workspace."}),
        ),
        (
            "limit",
            json!({"type":["integer", "null"], "description":format!("Maximum entries, 1 through {MAX_GLOB_ENTRIES}; null returns up to {MAX_GLOB_ENTRIES}. When may_be_truncated is true, narrow pattern or path.")}),
        ),
    ]
}

pub(super) fn inspect(
    root: &Dir,
    call: &ToolCall,
    config: &Config,
    result_bytes: usize,
) -> Result<String, ToolError> {
    let ToolCall::Glob {
        pattern,
        path,
        limit,
    } = call
    else {
        return Err(ToolError::Operation);
    };
    let matcher = matcher(pattern).map_err(|_| ToolError::Operation)?;
    let path = if path == "." { "" } else { path.as_str() };
    let mut walk = Walk {
        root,
        device: identity(root)?.0,
        matcher: &matcher,
        base: path,
        max_depth: (!pattern.split('/').any(|part| part == "**"))
            .then(|| pattern.split('/').count()),
        scanned: 0,
        matches: BTreeMap::new(),
    };
    for selected in &config.workspace_paths {
        if selected == "." {
            walk.start(path, true, true)?;
        } else if path.is_empty() || Path::new(selected).starts_with(path) {
            walk.start(selected, false, false)?;
        } else if Path::new(path).starts_with(selected) {
            walk.start(path, true, false)?;
        }
    }
    render(walk.matches, *limit, result_bytes)
}

pub(super) fn inspect_files<'a>(
    files: impl Iterator<Item = (&'a str, usize)>,
    call: &ToolCall,
    result_bytes: usize,
) -> Result<String, ToolError> {
    let ToolCall::Glob {
        pattern,
        path,
        limit,
    } = call
    else {
        return Err(ToolError::Operation);
    };
    let matcher = matcher(pattern).map_err(|_| ToolError::Operation)?;
    let mut matches = BTreeMap::new();
    for (name, bytes) in files {
        let relative = if path == "." {
            Some(name)
        } else {
            name.strip_prefix(path.as_str())
                .and_then(|rest| rest.strip_prefix('/'))
        };
        if relative.is_some_and(|relative| matcher.is_match(relative)) {
            matches.insert(name.to_owned(), bytes as u64);
            if matches.len() > MAX_CANDIDATES {
                return Err(ToolError::Limit);
            }
        }
    }
    render(matches, *limit, result_bytes)
}

struct Walk<'a> {
    root: &'a Dir,
    device: u64,
    matcher: &'a Regex,
    base: &'a str,
    max_depth: Option<usize>,
    scanned: usize,
    matches: BTreeMap<String, u64>,
}

impl Walk<'_> {
    fn start(&mut self, path: &str, requested: bool, whole_project: bool) -> Result<(), ToolError> {
        if path.is_empty() {
            let directory = self.root.try_clone().map_err(|_| ToolError::Path)?;
            return self.visit(&directory, "", whole_project);
        }
        let file = open_file(self.root, path)?;
        let metadata = cap_std::fs::Metadata::from_file(&file).map_err(|_| ToolError::Path)?;
        if metadata.is_dir() {
            return self.visit(&Dir::from_std_file(file), path, whole_project);
        }
        if requested && metadata.is_file() {
            return Err(ToolError::Operation);
        }
        // A selected root that names a file is itself a candidate.
        if !metadata.is_file() || !admitted(&metadata, self.device) {
            return Err(ToolError::Path);
        }
        self.candidate(path, metadata.len())
    }

    fn visit(&mut self, directory: &Dir, path: &str, whole_project: bool) -> Result<(), ToolError> {
        let mut children = Vec::new();
        for entry in directory.entries().map_err(|_| ToolError::Path)? {
            self.scanned += 1;
            if self.scanned > MAX_SCANNED_ENTRIES {
                return Err(ToolError::Limit);
            }
            let Ok(name) = entry
                .map_err(|_| ToolError::Path)?
                .file_name()
                .into_string()
            else {
                continue;
            };
            if forbidden(&name) || whole_project && ignored_directory(&name) {
                continue;
            }
            let relative = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}/{name}")
            };
            if relative.split('/').count() > MAX_DEPTH {
                return Err(ToolError::Limit);
            }
            if !valid_relative(&relative, false) {
                if relative.len() > 512 {
                    return Err(ToolError::Limit);
                }
                continue;
            }
            let metadata = directory
                .symlink_metadata(&name)
                .map_err(|_| ToolError::Path)?;
            if metadata.is_symlink() || !admitted(&metadata, self.device) {
                continue;
            }
            if metadata.is_dir() {
                if self
                    .max_depth
                    .is_none_or(|depth| self.depth(&relative) < depth)
                {
                    children.push(name);
                }
            } else if metadata.is_file() {
                self.candidate(&relative, metadata.len())?;
            }
        }
        children.sort_unstable();
        for name in children {
            let relative = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}/{name}")
            };
            let child = Dir::from_std_file(open_file(self.root, &relative)?);
            self.visit(&child, &relative, whole_project)?;
        }
        Ok(())
    }

    fn depth(&self, relative: &str) -> usize {
        self.relative(relative)
            .map_or(0, |rest| rest.split('/').count())
    }

    fn relative<'p>(&self, path: &'p str) -> Option<&'p str> {
        if self.base.is_empty() {
            Some(path)
        } else {
            path.strip_prefix(self.base)?.strip_prefix('/')
        }
    }

    fn candidate(&mut self, path: &str, bytes: u64) -> Result<(), ToolError> {
        if self
            .relative(path)
            .is_some_and(|relative| self.matcher.is_match(relative))
        {
            self.matches.insert(path.to_owned(), bytes);
            if self.matches.len() > MAX_CANDIDATES {
                return Err(ToolError::Limit);
            }
        }
        Ok(())
    }
}

// Hard links and other mounts are outside the admitted regular-file boundary; skip, never follow.
fn admitted(metadata: &cap_std::fs::Metadata, device: u64) -> bool {
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt;
        metadata.dev() == device && (!metadata.is_file() || metadata.nlink() == 1)
    }
    #[cfg(not(unix))]
    {
        let _ = (metadata, device);
        false
    }
}

// Keeps a Limit, not an empty success, when matches exist but none fits.
fn render(
    matches: BTreeMap<String, u64>,
    limit: u32,
    result_bytes: usize,
) -> Result<String, ToolError> {
    let mut truncated = matches.len() > limit as usize;
    let mut entries: Vec<_> = matches
        .into_iter()
        .take(limit as usize)
        .map(|(path, bytes)| json!({"path":path, "bytes":bytes}))
        .collect();
    let had_entries = !entries.is_empty();
    loop {
        let output =
            json!({"entries":entries, "limit":limit, "may_be_truncated":truncated}).to_string();
        if output.len() <= result_bytes {
            if had_entries && entries.is_empty() {
                return Err(ToolError::Limit);
            }
            return Ok(output);
        }
        entries.pop().ok_or(ToolError::Limit)?;
        truncated = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_closed_grammar_matches_whole_relative_paths() {
        for (pattern, yes, no) in [
            (
                "*.rs",
                &["main.rs", ".rs", "lib.rs"][..],
                &["src/main.rs", "main.rsx"][..],
            ),
            (
                "**/*.rs",
                &["main.rs", "src/main.rs", "a/b/c.rs"],
                &["src/main.r", "rs"],
            ),
            (
                "src/**",
                &["src/a", "src/a/b.rs"],
                &["src", "srcx/a", "a/src/b"],
            ),
            ("**", &["a", "a/b/c"], &[""]),
            ("a/**/b", &["a/b", "a/x/b", "a/x/y/b"], &["a/xb", "b"]),
            (
                "*.{toml,md}",
                &["Cargo.toml", "README.md"],
                &["a.rs", "x/README.md", "a.tomlmd"],
            ),
            (
                "v?.txt",
                &["v1.txt", "vé.txt"],
                &["v.txt", "v12.txt", "v/.txt"],
            ),
            (
                "{src,tests}/*.r?",
                &["src/a.rs", "tests/b.rt"],
                &["docs/a.rs", "src/a.r"],
            ),
            (".*", &[".gitignore", ".github"], &["a.txt"]),
            ("a+b(c)$.^|", &["a+b(c)$.^|"], &["aab(c)$.^|"]),
            ("Cargo.toml", &["Cargo.toml"], &["cargo.toml", "CargoXtoml"]),
        ] {
            let matcher = matcher(pattern).unwrap_or_else(|error| panic!("{pattern}: {error:?}"));
            for path in yes {
                assert!(matcher.is_match(path), "{pattern} should match {path}");
            }
            for path in no {
                assert!(!matcher.is_match(path), "{pattern} should not match {path}");
            }
        }
    }

    #[test]
    fn patterns_outside_the_grammar_are_refused_with_a_named_cause() {
        let alternatives = format!("{{{}}}", vec!["a"; MAX_ALTERNATIVES + 1].join(","));
        let split = format!("{{{}}}/{{{}}}", ["a"; 8].join(","), ["b"; 9].join(","));
        for (pattern, cause) in [
            ("", PatternError::Length),
            (&"a".repeat(MAX_PATTERN_BYTES + 1), PatternError::Length),
            ("a\nb", PatternError::Control),
            ("/etc/*", PatternError::Anchored),
            ("~/x", PatternError::Anchored),
            ("!*.rs", PatternError::Negation),
            ("a\\*", PatternError::Escape),
            ("[ab].rs", PatternError::Class),
            ("[!a]", PatternError::Class),
            ("a]", PatternError::Class),
            ("a//b", PatternError::EmptyComponent),
            ("src/", PatternError::EmptyComponent),
            ("./a", PatternError::DotComponent),
            ("a/../b", PatternError::DotComponent),
            ("a**", PatternError::Recursive),
            ("***", PatternError::Recursive),
            ("{a,**}", PatternError::Recursive),
            ("{a,b", PatternError::Brace),
            ("a,b}", PatternError::Brace),
            ("{a}", PatternError::Brace),
            ("{a,}", PatternError::Brace),
            ("{a,{b,c}}", PatternError::Brace),
            ("{a/b,c}", PatternError::Brace),
            (&alternatives, PatternError::Alternatives),
            (&split, PatternError::Alternatives),
            (&"*?".repeat(MAX_PATTERN_BYTES / 2), PatternError::Size),
        ] {
            assert_eq!(matcher(pattern).map(|_| ()), Err(cause), "{pattern:?}");
        }
        let at_limit = format!("{{{}}}", vec!["a"; MAX_ALTERNATIVES].join(","));
        assert!(matcher(&at_limit).is_ok());
    }

    #[test]
    fn null_or_omitted_path_and_limit_normalize_before_hashing() {
        let normalized = ToolCall::Glob {
            pattern: "*.rs".into(),
            path: ".".into(),
            limit: MAX_GLOB_ENTRIES,
        };
        for input in [
            json!({"operation":"glob","pattern":"*.rs"}),
            json!({"operation":"glob","pattern":"*.rs","path":null,"limit":null}),
            json!({"operation":"glob","pattern":"*.rs","path":"","limit":100}),
        ] {
            assert_eq!(
                serde_json::from_value::<ToolCall>(input.clone()).unwrap(),
                normalized,
                "{input}"
            );
        }
        assert_eq!(
            serde_json::to_value(&normalized).unwrap(),
            json!({"operation":"glob","pattern":"*.rs","path":".","limit":100})
        );
        for (limit, valid_limit) in [(0, false), (1, true), (100, true), (101, false)] {
            let call = ToolCall::Glob {
                pattern: "*.rs".into(),
                path: "src".into(),
                limit,
            };
            assert_eq!(call.valid(), valid_limit, "limit {limit}");
        }
        for path in ["/src", "../x", "src/.env", "a//b"] {
            let call = ToolCall::Glob {
                pattern: "*.rs".into(),
                path: path.into(),
                limit: 1,
            };
            assert!(!call.valid(), "{path}");
        }
    }
}
