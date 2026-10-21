use super::ToolError;
use super::types::{MAX_SEARCH_OFFSET, MAX_SEARCH_ROWS, ToolCall};
use regex::{Regex, RegexBuilder};
use serde_json::{Value, json};
use std::time::Instant;

const MAX_PATTERN_BYTES: usize = 512;
const MAX_GLOB_EXPRESSION_BYTES: usize = 8 * 1024;
const MAX_GLOB_ALTERNATIVES: usize = 16;
const PREVIEW_CHARS: usize = 512;

const INVALID_REGEX: &str =
    "regex syntax is invalid or unsupported (Rust regex syntax; no look-around or backreferences)";
const LARGE_REGEX: &str = "regex exceeds the 128 KiB compiled size limit";
const INVALID_GLOB: &str = "include glob is invalid or unsupported";
const LARGE_GLOB: &str =
    "include glob exceeds its 8 KiB translation, 16-alternative or compiled size limit";

pub(super) fn valid(call: &ToolCall) -> bool {
    matches!(call, ToolCall::Search { query, include, offset, limit, .. }
        if !query.is_empty()
            && query.len() <= MAX_PATTERN_BYTES
            && include.as_ref().is_none_or(|glob| !glob.is_empty() && glob.len() <= MAX_PATTERN_BYTES)
            && *offset <= MAX_SEARCH_OFFSET
            && (1..=MAX_SEARCH_ROWS).contains(limit))
}

// One page of matching lines: offset rows are counted and skipped, never retained, and one
// lookahead match proves that another page exists.
pub(super) struct Search<'a> {
    base: &'a str,
    query: Regex,
    include: Option<Regex>,
    offset: u32,
    limit: usize,
    skipped: u32,
    rows: Vec<Value>,
    more: bool,
    deadline: Instant,
}

impl<'a> Search<'a> {
    pub(super) fn new(call: &'a ToolCall, deadline: Instant) -> Result<Self, ToolError> {
        let ToolCall::Search {
            path,
            query,
            regex,
            include,
            case_sensitive,
            offset,
            limit,
        } = call
        else {
            return Err(ToolError::Operation);
        };
        if !valid(call) {
            return Err(ToolError::Operation);
        }
        let pattern = if *regex {
            query.clone()
        } else {
            regex::escape(query)
        };
        let query = compile(&pattern, !case_sensitive).map_err(|error| {
            ToolError::Pattern(match error {
                regex::Error::CompiledTooBig(_) => LARGE_REGEX,
                _ => INVALID_REGEX,
            })
        })?;
        Ok(Self {
            base: if path == "." { "" } else { path },
            query,
            include: include.as_deref().map(glob).transpose()?,
            offset: *offset,
            limit: *limit as usize,
            skipped: 0,
            rows: Vec::new(),
            more: false,
            deadline,
        })
    }

    // Callers apply this before reading a file, so an excluded file is never read.
    pub(super) fn includes(&self, file: &str) -> bool {
        let Some(include) = &self.include else {
            return true;
        };
        let relative = if file == self.base {
            file.rsplit('/').next().unwrap_or(file)
        } else if self.base.is_empty() {
            file
        } else {
            match file
                .strip_prefix(self.base)
                .and_then(|rest| rest.strip_prefix('/'))
            {
                Some(rest) => rest,
                None => return false,
            }
        };
        include.is_match(relative)
    }

    // Ok(true) once the page and its lookahead are complete; later files need no scan.
    pub(super) fn scan(&mut self, file: &str, content: &[u8]) -> Result<bool, ToolError> {
        let Ok(text) = std::str::from_utf8(content) else {
            return Ok(false);
        };
        for (index, line) in text.lines().enumerate() {
            if Instant::now() >= self.deadline {
                return Err(ToolError::Timeout(String::new()));
            }
            // One boolean match per line keeps matching linear in scanned bytes.
            if !self.query.is_match(line) {
                continue;
            }
            if self.skipped < self.offset {
                self.skipped += 1;
            } else if self.rows.len() == self.limit {
                self.more = true;
                return Ok(true);
            } else {
                self.rows.push(json!({"path":file,"line":index+1,"text":line.chars().take(PREVIEW_CHARS).collect::<String>()}));
            }
        }
        Ok(false)
    }

    // Drops only trailing rows to fit, and continues at the first dropped row.
    pub(super) fn finish(mut self, result_bytes: usize) -> Result<String, ToolError> {
        let had_rows = !self.rows.is_empty();
        loop {
            let next = self
                .more
                .then(|| u64::from(self.offset) + self.rows.len() as u64);
            let output = json!({"entries":self.rows,"limit":self.limit,"may_be_truncated":self.more,"next_offset":next}).to_string();
            if output.len() <= result_bytes {
                if had_rows && self.rows.is_empty() {
                    return Err(ToolError::Limit);
                }
                return Ok(output);
            }
            self.rows.pop().ok_or(ToolError::Limit)?;
            self.more = true;
        }
    }
}

fn compile(pattern: &str, ignore_case: bool) -> Result<Regex, regex::Error> {
    RegexBuilder::new(pattern)
        .case_insensitive(ignore_case)
        .size_limit(128 * 1024)
        .dfa_size_limit(32 * 1024)
        .nest_limit(32)
        .build()
}

// The closed discovery glob grammar: `*`, `?`, whole-component `**` and single-level `{a,b}`.
pub(super) fn glob(pattern: &str) -> Result<Regex, ToolError> {
    let invalid = || ToolError::Pattern(INVALID_GLOB);
    if pattern.is_empty() || pattern.len() > MAX_PATTERN_BYTES || pattern.starts_with(['~', '!']) {
        return Err(invalid());
    }
    let mut expression = String::from(r"\A");
    let mut alternatives = 0usize;
    let components: Vec<_> = pattern.split('/').collect();
    for (index, component) in components.iter().enumerate() {
        let last = index + 1 == components.len();
        if component.is_empty() || matches!(*component, "." | "..") {
            return Err(invalid());
        }
        if *component == "**" {
            expression.push_str(if last { ".*" } else { "(?:[^/]+/)*" });
            continue;
        }
        if component.contains("**") {
            return Err(invalid());
        }
        let mut group = false;
        let mut empty = false;
        for character in component.chars() {
            match character {
                '*' => expression.push_str("[^/]*"),
                '?' => expression.push_str("[^/]"),
                '{' if !group => {
                    group = true;
                    alternatives += 1;
                    expression.push_str("(?:");
                }
                ',' | '}' if group => {
                    if empty {
                        return Err(invalid());
                    }
                    if character == ',' {
                        alternatives += 1;
                        expression.push('|');
                    } else {
                        group = false;
                        expression.push(')');
                    }
                }
                '{' | '}' | '[' | ']' | '\\' => return Err(invalid()),
                character if character.is_control() => return Err(invalid()),
                character => {
                    expression.push_str(&regex::escape(character.encode_utf8(&mut [0; 4])))
                }
            }
            empty = matches!(character, '{' | ',') && group;
        }
        if group {
            return Err(invalid());
        }
        if !last {
            expression.push('/');
        }
    }
    expression.push_str(r"\z");
    if alternatives > MAX_GLOB_ALTERNATIVES || expression.len() > MAX_GLOB_EXPRESSION_BYTES {
        return Err(ToolError::Pattern(LARGE_GLOB));
    }
    compile(&expression, false).map_err(|error| match error {
        regex::Error::CompiledTooBig(_) => ToolError::Pattern(LARGE_GLOB),
        _ => invalid(),
    })
}
