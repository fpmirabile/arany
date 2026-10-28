use super::ToolError;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::time::{Duration, Instant};

pub(super) const CONTROL_BYTES: usize = 32 * 1024;
pub(super) const CONTROL_TOTAL_BYTES: usize = 256 * 1024;
pub(super) const CONTROL_FILES: usize = 256;
pub(super) const RULES: usize = 4096;
const LINE_BYTES: usize = 4096;
const EXPANDED_LINE_BYTES: usize = 64 * 1024;
const EXPANDED_BYTES: usize = 1024 * 1024;
pub(super) const DISCOVERY_CALLS: u64 = 1_048_576;
pub(super) const DISCOVERY_TIME: Duration = Duration::from_secs(2);
pub(super) const COMMAND_CALLS: u64 = 8_388_608;
#[cfg(test)]
pub(super) const TEST_RUNTIME: Duration =
    Duration::from_millis(super::types::DEFAULT_COMMAND_TIMEOUT_MS as u64);

enum Deadline {
    System(Instant),
    Runtime(tokio::time::Instant),
}

pub(super) struct Selector {
    controls: Vec<(String, Gitignore)>,
    bytes: usize,
    rules: usize,
    expanded: usize,
    calls: u64,
    max_calls: u64,
    deadline: Deadline,
    #[cfg(test)]
    pub(super) in_progress: Option<Box<dyn FnOnce() + Send>>,
}

impl Selector {
    pub(super) fn new(calls: u64, deadline: Instant) -> Self {
        Self::with_deadline(calls, Deadline::System(deadline))
    }

    fn with_deadline(calls: u64, deadline: Deadline) -> Self {
        Self {
            controls: Vec::new(),
            bytes: 0,
            rules: 0,
            expanded: 0,
            calls,
            max_calls: COMMAND_CALLS,
            deadline,
            #[cfg(test)]
            in_progress: None,
        }
    }

    pub(super) fn discovery(deadline: Instant) -> Self {
        Self {
            max_calls: DISCOVERY_CALLS,
            ..Self::new(0, deadline.min(Instant::now() + DISCOVERY_TIME))
        }
    }

    pub(super) fn discovery_until(deadline: tokio::time::Instant) -> Self {
        Self {
            max_calls: DISCOVERY_CALLS,
            ..Self::with_deadline(
                0,
                Deadline::Runtime(deadline.min(tokio::time::Instant::now() + DISCOVERY_TIME)),
            )
        }
    }

    pub(super) fn check_time(&self) -> Result<(), ToolError> {
        let expired = match self.deadline {
            Deadline::System(deadline) => Instant::now() >= deadline,
            Deadline::Runtime(deadline) => tokio::time::Instant::now() >= deadline,
        };
        if expired {
            return Err(ToolError::Timeout(String::new()));
        }
        Ok(())
    }

    pub(super) fn calls(&self) -> u64 {
        self.calls
    }

    pub(super) fn add(&mut self, directory: &str, bytes: &[u8]) -> Result<(), ToolError> {
        #[cfg(test)]
        if let Some(checkpoint) = self.in_progress.take() {
            checkpoint();
        }
        self.check_time()?;
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .ok_or(ToolError::Limit)?;
        if bytes.len() > CONTROL_BYTES
            || self.bytes > CONTROL_TOTAL_BYTES
            || self.controls.len() >= CONTROL_FILES
        {
            return Err(ToolError::Limit);
        }
        let text = std::str::from_utf8(bytes).map_err(|_| ToolError::Path)?;
        let mut builder = GitignoreBuilder::new(if directory.is_empty() { "." } else { directory });
        for (index, line) in text.lines().enumerate() {
            if line.len() > LINE_BYTES {
                return Err(ToolError::Limit);
            }
            let line = if index == 0 {
                line.trim_start_matches('\u{feff}')
            } else {
                line
            };
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            self.rules = self.rules.checked_add(1).ok_or(ToolError::Limit)?;
            let charge = line
                .len()
                .checked_add(64)
                .and_then(|n| n.checked_mul(32))
                .ok_or(ToolError::Limit)?;
            self.expanded = self.expanded.checked_add(charge).ok_or(ToolError::Limit)?;
            if self.rules > RULES
                || charge > EXPANDED_LINE_BYTES
                || self.expanded > EXPANDED_BYTES
                || line.bytes().filter(|b| *b == b'*').count() > 16
                || line.matches("**").count() > 4
                || line.bytes().filter(|b| *b == b'{').count() > 4
            {
                return Err(ToolError::Limit);
            }
            self.check_time()?;
            builder.add_line(None, line).map_err(|_| ToolError::Path)?;
            self.check_time()?;
        }
        self.check_time()?;
        let matcher = builder.build().map_err(|_| ToolError::Path)?;
        self.check_time()?;
        self.controls.push((directory.to_owned(), matcher));
        Ok(())
    }

    pub(super) fn excluded(&mut self, path: &str, directory: bool) -> Result<bool, ToolError> {
        #[cfg(test)]
        if let Some(checkpoint) = self.in_progress.take() {
            checkpoint();
        }
        self.check_time()?;
        for (index, _) in path.match_indices('/') {
            if self.match_one(&path[..index], true)? {
                return Ok(true);
            }
        }
        self.match_one(path, directory)
    }

    fn match_one(&mut self, path: &str, directory: bool) -> Result<bool, ToolError> {
        let mut excluded = false;
        let mut depth = 0;
        for (base, matcher) in &self.controls {
            if !base.is_empty()
                && !path
                    .strip_prefix(base)
                    .is_some_and(|rest| rest.starts_with('/'))
            {
                continue;
            }
            self.calls = self.calls.checked_add(1).ok_or(ToolError::Limit)?;
            if self.calls > self.max_calls {
                return Err(ToolError::Limit);
            }
            self.check_time()?;
            let matched = matcher.matched(path, directory);
            self.check_time()?;
            if !matched.is_none() && base.len() >= depth {
                excluded = matched.is_ignore();
                depth = base.len();
            }
        }
        Ok(excluded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_keeps_its_smaller_work_and_time_budgets() {
        let now = Instant::now();
        let mut selector = Selector::discovery(now + TEST_RUNTIME);
        assert!(matches!(
            selector.deadline,
            Deadline::System(deadline) if deadline <= Instant::now() + DISCOVERY_TIME
        ));
        selector.add("", b"ignored").unwrap();
        selector.calls = DISCOVERY_CALLS - 1;
        assert!(selector.excluded("ignored", false).unwrap());
        assert!(matches!(
            selector.excluded("visible", false),
            Err(ToolError::Limit)
        ));
        let mut expired = Selector::discovery(now);
        assert!(matches!(
            expired.add("", b"ignored"),
            Err(ToolError::Timeout(_))
        ));
    }

    #[test]
    fn selection_uses_the_action_deadline_without_a_setup_cap() {
        let now = Instant::now();
        let action = now + Duration::from_millis(25);
        let selector = Selector::new(0, action);
        assert!(matches!(selector.deadline, Deadline::System(deadline) if deadline == action));
        let mut discovery = Selector::discovery(now + DISCOVERY_TIME);
        discovery.add("", b"ignored").unwrap();
        let action = now + TEST_RUNTIME;
        let mut bounded = Selector::new(0, action);
        assert!(matches!(
            bounded.deadline,
            Deadline::System(deadline) if deadline == action
        ));
        bounded.add("", b"ignored").unwrap();
        let mut expired = Selector::new(0, now);
        assert!(matches!(
            expired.add("", b"ignored"),
            Err(ToolError::Timeout(_))
        ));
        assert!(matches!(
            expired.excluded("visible", false),
            Err(ToolError::Timeout(_))
        ));
    }

    #[test]
    fn pinned_emitter_stays_within_admission_charge() {
        let corpus = [
            (
                "**/**/**/**/*a*a*a*a*a*a*a*a".into(),
                "aaaaaaaa".into(),
                false,
            ),
            (
                "{a,b}{a,b}{a,b}{a,b}[!0-9][a-z]".into(),
                "aaaaaz".into(),
                false,
            ),
            (r"\#literal\ ".into(), "#literal ".into(), false),
            (r"\!literal".into(), "!literal".into(), false),
            ("/root/**".into(), "root/leaf".into(), false),
            ("!keep/".into(), "keep".into(), true),
            ("é".repeat(992), "é".repeat(992), false),
            ("x".repeat(1984), "x".repeat(1984), false),
        ];
        for (line, candidate, directory) in corpus {
            let mut admitted = Selector::new(0, Instant::now() + TEST_RUNTIME);
            admitted.add("", line.as_bytes()).unwrap();
            let matched = admitted.controls[0].1.matched(&candidate, directory);
            let actual = matched
                .inner()
                .expect("corpus must exercise a compiled rule")
                .actual();
            let emitted = globset::GlobBuilder::new(actual)
                .literal_separator(true)
                .backslash_escape(true)
                .allow_unclosed_class(true)
                .build()
                .unwrap();
            assert!(emitted.regex().len() <= 32 * (line.len() + 64));
        }
    }

    #[test]
    fn retained_matchers_and_all_admission_budgets_are_bounded() {
        let mut rules = Selector::new(0, Instant::now() + TEST_RUNTIME);
        rules.add("", "x\n".repeat(504).as_bytes()).unwrap();
        assert_eq!(rules.rules, 504);
        assert!(matches!(rules.add("nested", b"x"), Err(ToolError::Limit)));

        let mut selector = Selector::new(0, Instant::now() + TEST_RUNTIME);
        for index in 0..CONTROL_FILES {
            let directory = format!("branch{}/nested{}", index / 16, index % 16);
            selector.add(&directory, "x".repeat(64).as_bytes()).unwrap();
        }
        assert_eq!(selector.expanded, EXPANDED_BYTES);
        assert!(!selector.excluded("branch0/nested0/visible", false).unwrap());
        assert!(matches!(selector.add("extra", b""), Err(ToolError::Limit)));

        let mut bytes = Selector::new(0, Instant::now() + TEST_RUNTIME);
        let comments = format!("#{}\n", "c".repeat(4094)).repeat(8);
        for index in 0..8 {
            bytes.add(&index.to_string(), comments.as_bytes()).unwrap();
        }
        assert_eq!(bytes.bytes, CONTROL_TOTAL_BYTES);
        assert!(matches!(bytes.add("overflow", b"#"), Err(ToolError::Limit)));

        let mut work = Selector::new(COMMAND_CALLS - 1, Instant::now() + TEST_RUNTIME);
        work.add("", b"ignored").unwrap();
        assert!(work.excluded("ignored", false).unwrap());
        assert!(matches!(
            work.excluded("visible", false),
            Err(ToolError::Limit)
        ));
        let mut elapsed = Selector::new(0, Instant::now() + TEST_RUNTIME);
        elapsed.deadline = Deadline::System(Instant::now());
        assert!(matches!(
            elapsed.excluded("visible", false),
            Err(ToolError::Timeout(_))
        ));
    }
}
