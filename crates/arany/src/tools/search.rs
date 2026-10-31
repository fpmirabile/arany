use super::ToolError;
use super::types::{MAX_SEARCH_OFFSET, MAX_SEARCH_ROWS, ToolCall};
use regex::{Regex, RegexBuilder};
use serde_json::{Value, json};
use std::time::Instant;

pub(super) const MAX_PATTERN_BYTES: usize = 512;
const PREVIEW_CHARS: usize = 512;

// Every refusal fits the 128-byte minimum result budget after its fixed prefix and suffix.
const INVALID_REGEX: &str =
    "invalid or unsupported regex (Rust syntax, no look-around or backreferences)";
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
    skipped_large: u64,
    skipped_binary: u64,
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
            skipped_large: 0,
            skipped_binary: 0,
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

    pub(super) fn has_rows(&self) -> bool {
        !self.rows.is_empty()
    }

    // A scan bound reached after the first row ends the page early; the caller resumes at next_offset.
    pub(super) fn truncate(&mut self) {
        self.more = true;
    }

    pub(super) fn skip_large(&mut self) {
        self.skipped_large += 1;
    }

    // Ok(true) once the page and its lookahead are complete; later files need no scan.
    pub(super) fn scan(&mut self, file: &str, content: &[u8]) -> Result<bool, ToolError> {
        let Ok(text) = std::str::from_utf8(content) else {
            self.skipped_binary += 1;
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
            let mut page = json!({"entries":self.rows,"limit":self.limit,"may_be_truncated":self.more,"next_offset":next});
            for (field, count) in [
                ("skipped_large", self.skipped_large),
                ("skipped_binary", self.skipped_binary),
            ] {
                if count > 0 {
                    page[field] = json!(count);
                }
            }
            let output = page.to_string();
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

// Include filters share the discovery grammar and bounds of the glob Tool.
fn glob(pattern: &str) -> Result<Regex, ToolError> {
    super::glob::matcher(pattern).map_err(|error| {
        ToolError::Pattern(match error {
            super::glob::PatternError::Alternatives | super::glob::PatternError::Size => LARGE_GLOB,
            _ => INVALID_GLOB,
        })
    })
}
