use super::ToolError;
use super::types::ToolCall;
use crate::validate::valid_relative;
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

const MAX_GLOB_ENTRIES: u32 = 100;
const MAX_PATTERN_BYTES: usize = 512;
const MAX_ALTERNATIVES: usize = 16;
const MAX_TRANSLATION_BYTES: usize = 8 * 1024;
// Discovery walks are bounded by their deadline; this caps the work a fast filesystem can add.
pub(super) const MAX_SCANNED_ENTRIES: usize = 200_000;
// No walk can match more files than it inspects entries.
const MAX_GLOB_OFFSET: u32 = MAX_SCANNED_ENTRIES as u32;
pub(super) const MAX_DEPTH: usize = 16;

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

// Shared by glob and Search include filters; permission path rules keep their own semantics.
pub(super) fn matcher(pattern: &str) -> Result<Regex, PatternError> {
    RegexBuilder::new(&translate(pattern)?)
        .size_limit(128 * 1024)
        .dfa_size_limit(32 * 1024)
        .nest_limit(32)
        .build()
        .map_err(|_| PatternError::Size)
}

// Wildcard-free leading directory components; every match lies beneath them.
pub(super) fn literal_prefix(pattern: &str) -> Vec<String> {
    let mut components: Vec<_> = pattern.split('/').collect();
    components.pop();
    components
        .into_iter()
        .take_while(|component| !component.contains(['*', '?', '{', '}']))
        .map(str::to_owned)
        .collect()
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
    matches!(call, ToolCall::Glob { pattern, path, offset, limit }
        if valid_relative(path, true)
            && *offset <= MAX_GLOB_OFFSET
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
            "offset",
            json!({"type":["integer", "null"], "description":"Number of matching files to skip; null starts at 0. Continue using next_offset."}),
        ),
        (
            "limit",
            json!({"type":["integer", "null"], "description":format!("Maximum entries, 1 through {MAX_GLOB_ENTRIES}; null returns up to {MAX_GLOB_ENTRIES}. When may_be_truncated is true, continue at next_offset or narrow pattern or path.")}),
        ),
    ]
}

// Keeps a Limit, not an empty success, when matches exist but none fits. `matches` follow the
// skipped offset and hold one lookahead entry when another page exists.
pub(super) fn render(
    matches: BTreeMap<String, u64>,
    offset: u32,
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
        let next = truncated.then(|| u64::from(offset) + entries.len() as u64);
        let output = json!({"entries":entries, "limit":limit, "may_be_truncated":truncated, "next_offset":next})
            .to_string();
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
    fn null_or_omitted_path_offset_and_limit_normalize_before_hashing() {
        let normalized = ToolCall::Glob {
            pattern: "*.rs".into(),
            path: ".".into(),
            offset: 0,
            limit: MAX_GLOB_ENTRIES,
        };
        for input in [
            json!({"operation":"glob","pattern":"*.rs"}),
            json!({"operation":"glob","pattern":"*.rs","path":null,"offset":null,"limit":null}),
            json!({"operation":"glob","pattern":"*.rs","path":"","offset":0,"limit":100}),
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
        let paged = ToolCall::Glob {
            pattern: "*.rs".into(),
            path: ".".into(),
            offset: 7,
            limit: 1,
        };
        assert_eq!(
            serde_json::to_value(&paged).unwrap(),
            json!({"operation":"glob","pattern":"*.rs","path":".","offset":7,"limit":1})
        );
        for (offset, limit, valid_limit) in [
            (0, 0, false),
            (0, 1, true),
            (0, 100, true),
            (0, 101, false),
            (MAX_GLOB_OFFSET, 1, true),
            (MAX_GLOB_OFFSET + 1, 1, false),
        ] {
            let call = ToolCall::Glob {
                pattern: "*.rs".into(),
                path: "src".into(),
                offset,
                limit,
            };
            assert_eq!(call.valid(), valid_limit, "offset {offset} limit {limit}");
        }
        for path in ["/src", "../x", "src/.env", "a//b"] {
            let call = ToolCall::Glob {
                pattern: "*.rs".into(),
                path: path.into(),
                offset: 0,
                limit: 1,
            };
            assert!(!call.valid(), "{path}");
        }
    }
}
