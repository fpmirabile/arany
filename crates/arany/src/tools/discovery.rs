mod suggestions;
use super::config::Config;
use super::fs::{self, Keep};
use super::glob::{MAX_DEPTH, MAX_SCANNED_ENTRIES};
use super::selection::{CONTROL_BYTES, Selector};
use super::types::{MAX_FILE_BYTES, MAX_SNAPSHOT_BYTES, MAX_SNAPSHOT_FILES};
use super::{ToolCall, ToolError};
use crate::validate::valid_relative;
use cap_std::fs::{Dir, MetadataExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};
pub use suggestions::FileSuggestionSource;

pub(super) const TYPED_WAIT: Duration = Duration::from_millis(250);
pub const SUGGESTION_LIFETIME: Duration = Duration::from_secs(30);
pub const SUGGESTION_QUERY_TIME: Duration = Duration::from_millis(50);
const INDEX_NAMES: usize = 2048;
const INDEX_BYTES: usize = 48 * 1024;
const DISPLAY_NAMES: usize = 64;
const MAX_LIST_ENTRIES: usize = MAX_SNAPSHOT_FILES * 4;
// Search stops scanning at this share of its time budget, leaving the rest for control checks.
const SEARCH_SCAN_PERCENT: u32 = 60;

#[derive(Clone, Copy)]
pub(super) enum Output {
    Tool,
    Suggestions,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SuggestionIndex {
    names: Vec<String>,
}

impl SuggestionIndex {
    pub(super) fn decode(output: &str) -> Result<Self, ToolError> {
        if output.len() > INDEX_BYTES {
            return Err(ToolError::Limit);
        }
        let index: Self = serde_json::from_str(output).map_err(|_| ToolError::Operation)?;
        if index.names.len() > INDEX_NAMES
            || index.names.iter().any(|path| {
                let path = path.strip_suffix('/').unwrap_or(path);
                !valid_relative(path, false) || path.split('/').count() > MAX_DEPTH
            })
        {
            return Err(ToolError::Limit);
        }
        Ok(index)
    }

    pub fn refine(&self, folder: &str, query: &str) -> Result<Vec<String>, ToolError> {
        super::types::validate_mention_path(folder, true)?;
        super::types::validate_mention_path(query, true)?;
        let deadline = Instant::now() + SUGGESTION_QUERY_TIME;
        let query = query.to_lowercase();
        let mut matches = Vec::new();
        for path in &self.names {
            if Instant::now() >= deadline {
                return Err(ToolError::Timeout(String::new()));
            }
            let relative = if folder.is_empty() || folder == "." {
                path.as_str()
            } else if let Some(relative) =
                path.strip_prefix(folder).and_then(|p| p.strip_prefix('/'))
            {
                relative
            } else {
                continue;
            };
            if query.is_empty() && relative.trim_end_matches('/').contains('/') {
                continue;
            }
            if let Some(rank) = fs::path_match(path.trim_end_matches('/'), &query) {
                matches.push((rank, path));
            }
        }
        matches.sort_by(|(a, left), (b, right)| {
            a.cmp(b)
                .then(left.len().cmp(&right.len()))
                .then(left.cmp(right))
        });
        if Instant::now() >= deadline {
            return Err(ToolError::Timeout(String::new()));
        }
        Ok(matches
            .into_iter()
            .take(DISPLAY_NAMES)
            .map(|(_, p)| p.clone())
            .collect())
    }
}

pub(super) fn enforcement_contract() -> Vec<u8> {
    let mut bytes = b";typed-discovery-v2-guard-owned-handle-relative-no-follow-controls-restrictive-read-before-payload-nested-repositories-pruned-partial-discovery-pages-no-fallback".to_vec();
    for limit in [
        super::selection::DISCOVERY_CALLS,
        MAX_DEPTH as u64,
        MAX_SCANNED_ENTRIES as u64,
        MAX_LIST_ENTRIES as u64,
        u64::from(SEARCH_SCAN_PERCENT),
    ] {
        bytes.extend_from_slice(&limit.to_le_bytes());
    }
    for duration in [super::selection::DISCOVERY_TIME, TYPED_WAIT] {
        bytes.extend_from_slice(&duration.as_nanos().to_le_bytes());
    }
    bytes
}

pub(super) fn possible(call: &ToolCall) -> bool {
    matches!(
        call,
        ToolCall::List { .. } | ToolCall::Glob { .. } | ToolCall::Search { .. }
    )
}

pub(super) fn exact_search(root: &Dir, call: &ToolCall) -> Result<bool, ToolError> {
    match call {
        ToolCall::Search { path, .. } if path != "." => {
            let file = fs::open_file(root, path)?;
            let metadata = file.metadata().map_err(|_| ToolError::Path)?;
            Ok(metadata.is_file())
        }
        _ => Ok(false),
    }
}

pub(super) struct Request<'a> {
    pub(super) root: &'a Dir,
    pub(super) call: &'a ToolCall,
    pub(super) config: &'a Config,
    pub(super) hidden: Keep<'a>,
    pub(super) result_bytes: usize,
    pub(super) deadline: Instant,
    pub(super) output: Output,
    pub(super) cancellation: Option<&'a super::Cancellation>,
}

struct Walk<'a> {
    request: Request<'a>,
    selector: Selector,
    controls: BTreeMap<String, Option<(std::fs::Metadata, Vec<u8>)>>,
    visited: BTreeSet<String>,
    entries: BTreeMap<String, Node>,
    scanned: usize,
    content_bytes: usize,
    device: u64,
    query: Query<'a>,
    base: String,
    prefix: Vec<String>,
    scan_until: Instant,
    stopped: bool,
}

enum Query<'a> {
    List,
    Glob {
        matcher: regex::Regex,
        depth: Option<usize>,
        offset: u32,
        skipped: u32,
        limit: u32,
    },
    Search(super::search::Search<'a>),
}

struct Node {
    directory: bool,
    bytes: u64,
}

pub(super) fn inspect(request: Request<'_>) -> Result<String, ToolError> {
    Walk::new(request)?.inspect()
}

impl<'a> Walk<'a> {
    fn new(request: Request<'a>) -> Result<Self, ToolError> {
        let path = match request.call {
            ToolCall::List { path }
            | ToolCall::Search { path, .. }
            | ToolCall::Glob { path, .. } => path,
            _ => return Err(ToolError::Operation),
        };
        let path = if path == "." { "" } else { path.as_str() };
        let (query, pattern) = match request.call {
            ToolCall::List { .. } => (Query::List, None),
            ToolCall::Glob {
                pattern,
                offset,
                limit,
                ..
            } => (
                Query::Glob {
                    matcher: super::glob::matcher(pattern).map_err(|_| ToolError::Operation)?,
                    depth: (!pattern.split('/').any(|part| part == "**"))
                        .then(|| pattern.split('/').count()),
                    offset: *offset,
                    skipped: 0,
                    limit: *limit,
                },
                Some(pattern),
            ),
            ToolCall::Search { include, .. } => (
                Query::Search(super::search::Search::new(request.call, request.deadline)?),
                include.as_ref(),
            ),
            _ => return Err(ToolError::Operation),
        };
        let device = fs::identity(request.root)?.0;
        let now = Instant::now();
        let budget = request
            .deadline
            .saturating_duration_since(now)
            .min(super::selection::DISCOVERY_TIME);
        Ok(Self {
            selector: Selector::discovery(request.deadline),
            request,
            controls: BTreeMap::new(),
            visited: BTreeSet::new(),
            entries: BTreeMap::new(),
            scanned: 0,
            content_bytes: 0,
            device,
            query,
            base: path.into(),
            prefix: pattern.map_or_else(Vec::new, |pattern| super::glob::literal_prefix(pattern)),
            scan_until: now + budget * SEARCH_SCAN_PERCENT / 100,
            stopped: false,
        })
    }

    fn inspect(mut self) -> Result<String, ToolError> {
        let base = self.base.clone();
        let path = base.as_str();
        self.check_time()?;
        let root = self.request.root.try_clone().map_err(|_| ToolError::Path)?;
        self.control("", &root)?;
        let mut roots = Vec::new();
        for selected in &self.request.config.workspace_paths {
            if selected == "."
                || !path.is_empty() && std::path::Path::new(path).starts_with(selected)
            {
                roots.push(path.to_owned());
            } else if path.is_empty() || std::path::Path::new(selected).starts_with(path) {
                roots.push(selected.clone());
            }
        }
        // Directory order keeps Search rows in path byte order across scope roots.
        roots.sort_by_key(|root| format!("{root}/"));
        roots.dedup();
        for root in roots {
            if self.stopped {
                break;
            }
            self.start(&root)?;
        }
        self.verify_controls()?;
        let output = self.render()?;
        self.check_time()?;
        Ok(output)
    }

    fn check_time(&self) -> Result<(), ToolError> {
        if self
            .request
            .cancellation
            .is_some_and(super::Cancellation::is_cancelled)
        {
            return Err(ToolError::Operation);
        }
        self.selector.check_time()
    }

    fn admitted(&self, path: &str) -> bool {
        self.request.config.in_workspace_scope(path) && !(self.request.hidden)(path)
    }

    // Search returns the rows found so far when a scan bound stops it after at least one row.
    fn bound(&mut self) -> Result<(), ToolError> {
        match &mut self.query {
            Query::Search(search) if search.has_rows() => {
                search.truncate();
                self.stopped = true;
                Ok(())
            }
            _ => Err(ToolError::Limit),
        }
    }

    fn excluded(&mut self, path: &str, directory: bool) -> Result<bool, ToolError> {
        match self.selector.excluded(path, directory) {
            Err(ToolError::Limit) => self.bound().map(|()| true),
            result => result,
        }
    }

    fn open_path(&self, path: &str) -> Result<Dir, ToolError> {
        fs::open_file(self.request.root, if path.is_empty() { "." } else { path })
            .map(Dir::from_std_file)
    }

    fn control(&mut self, directory: &str, handle: &Dir) -> Result<(), ToolError> {
        let path = joined(directory, ".gitignore");
        if self.controls.contains_key(&path) {
            return Ok(());
        }
        if path.split('/').count() > MAX_DEPTH || path.len() > crate::validate::MAX_PATH_BYTES {
            return Err(ToolError::Limit);
        }
        match handle.symlink_metadata(".gitignore") {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.controls.insert(path, None);
                return Ok(());
            }
            Err(_) => return Err(ToolError::Path),
            Ok(metadata)
                if !self.admitted(&path) || !metadata.is_file() || metadata.nlink() != 1 =>
            {
                return Err(ToolError::FilesystemDenied);
            }
            Ok(_) => {}
        }
        let file = fs::open_child(handle, ".gitignore", self.device)?;
        let metadata = file.metadata().map_err(|_| ToolError::Path)?;
        let bytes = fs::read_regular(file, CONTROL_BYTES)?;
        self.selector.add(directory, &bytes)?;
        self.controls.insert(path, Some((metadata, bytes)));
        Ok(())
    }

    fn start(&mut self, path: &str) -> Result<(), ToolError> {
        if path != self.base
            && matches!(
                fs::open_file(self.request.root, path),
                Err(ToolError::NotFound)
            )
        {
            return Ok(());
        }
        for (index, _) in path.match_indices('/') {
            let ancestor = &path[..index];
            if (self.request.hidden)(ancestor) || self.excluded(ancestor, true)? {
                return Ok(());
            }
            let handle = self.open_path(ancestor)?;
            self.control(ancestor, &handle)?;
        }
        if path.is_empty() {
            let root = self.request.root.try_clone().map_err(|_| ToolError::Path)?;
            return self.directory(path, root);
        }
        let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
        match self.open_path(parent) {
            Ok(parent) => self.entry(path, &parent),
            Err(ToolError::NotFound) if path != self.base => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn entry(&mut self, path: &str, parent: &Dir) -> Result<(), ToolError> {
        self.check_time()?;
        if self.stopped || !valid_relative(path, false) || (self.request.hidden)(path) {
            return Ok(());
        }
        if matches!(&self.query, Query::Search(search) if search.has_rows())
            && Instant::now() >= self.scan_until
        {
            return self.bound();
        }
        if path.split('/').count() > MAX_DEPTH {
            return Err(ToolError::Limit);
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        let metadata = match parent.symlink_metadata(name) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && path != self.base => {
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ToolError::NotFound);
            }
            Err(_) => return Err(ToolError::Path),
        };
        if path.split('/').any(fs::ignored_directory) || self.excluded(path, metadata.is_dir())? {
            return Ok(());
        }
        if metadata.is_symlink()
            || !metadata.is_dir() && !metadata.is_file()
            || metadata.dev() != self.device
            || metadata.is_file() && metadata.nlink() != 1
        {
            return if path == self.base {
                Err(ToolError::Path)
            } else {
                Ok(())
            };
        }
        let node = Node {
            directory: metadata.is_dir(),
            bytes: metadata.len(),
        };
        if !metadata.is_dir()
            && matches!(self.request.call, ToolCall::List { path: requested } | ToolCall::Glob { path: requested, .. } if requested == path)
        {
            return Err(ToolError::Operation);
        }
        if self.admitted(path) && self.candidate(path, &node) {
            if matches!(self.query, Query::Search(_)) {
                self.scan(path, parent, &metadata)?;
            } else {
                self.retain(path, node)?;
            }
        }
        if metadata.is_dir() && self.descend(path) {
            let child = Dir::from_std_file(fs::open_child(parent, name, self.device)?);
            if !self.explicit(path) && fs::nested_repository(&child)? {
                return Ok(());
            }
            self.directory(path, child)?;
        }
        Ok(())
    }

    // Requested bases and scope roots are walked even when they hold their own `.git`.
    fn explicit(&self, path: &str) -> bool {
        path == self.base
            || self
                .request
                .config
                .workspace_paths
                .iter()
                .any(|root| root == path)
    }

    fn scan(
        &mut self,
        path: &str,
        parent: &Dir,
        metadata: &cap_std::fs::Metadata,
    ) -> Result<(), ToolError> {
        use std::os::unix::fs::MetadataExt as _;
        if !self.visited.insert(path.into()) {
            return Ok(());
        }
        let Query::Search(search) = &mut self.query else {
            return Err(ToolError::Operation);
        };
        if metadata.len() > MAX_FILE_BYTES as u64 {
            search.skip_large();
            return Ok(());
        }
        if self.content_bytes as u64 + metadata.len() > MAX_SNAPSHOT_BYTES as u64 {
            return self.bound();
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        let file = match fs::open_child(parent, name, self.device) {
            Err(ToolError::NotFound) => return Ok(()),
            file => file?,
        };
        if file.metadata().map_err(|_| ToolError::Path)?.ino() != metadata.ino() {
            return Err(ToolError::ChangedInput);
        }
        let Query::Search(search) = &mut self.query else {
            return Err(ToolError::Operation);
        };
        let content = match fs::read_regular(file, MAX_FILE_BYTES) {
            Err(ToolError::Limit) => {
                search.skip_large();
                return Ok(());
            }
            content => content?,
        };
        self.content_bytes += content.len();
        if search.scan(path, &content)? {
            self.stopped = true;
        }
        Ok(())
    }

    // Glob skips its offset matches unretained. The walk visits matches in path order, so one
    // lookahead match past the page, or a full Tool listing, ends it with the rows so far.
    fn retain(&mut self, path: &str, node: Node) -> Result<(), ToolError> {
        if let Query::Glob {
            offset, skipped, ..
        } = &mut self.query
            && *skipped < *offset
        {
            *skipped += 1;
            return Ok(());
        }
        if self.entries.len() == self.retained_limit() {
            if !matches!(
                (&self.query, self.request.output),
                (Query::List, Output::Tool)
            ) {
                return Err(ToolError::Limit);
            }
            self.stopped = true;
            return Ok(());
        }
        self.entries.insert(path.into(), node);
        if matches!(self.query, Query::Glob { limit, .. } if self.entries.len() > limit as usize) {
            self.stopped = true;
        }
        Ok(())
    }

    fn immediate_list(&self) -> bool {
        self.request.config.workspace_paths == ["."]
    }

    fn retained_limit(&self) -> usize {
        match (self.request.output, &self.query) {
            (Output::Suggestions, _) => INDEX_NAMES,
            (Output::Tool, Query::List) => MAX_LIST_ENTRIES,
            (Output::Tool, Query::Glob { limit, .. }) => *limit as usize + 1,
            (Output::Tool, Query::Search(_)) => MAX_SNAPSHOT_FILES,
        }
    }

    fn candidate(&self, path: &str, node: &Node) -> bool {
        let relative = if self.base.is_empty() {
            Some(path)
        } else {
            path.strip_prefix(&self.base)
                .and_then(|rest| rest.strip_prefix('/'))
        };
        match &self.query {
            Query::List => {
                path != self.base
                    || !self.immediate_list() && matches!(self.request.output, Output::Tool)
            }
            Query::Glob { matcher, .. } => {
                !node.directory && relative.is_some_and(|path| matcher.is_match(path))
            }
            Query::Search(search) => !node.directory && search.includes(path),
        }
    }

    fn descend(&self, path: &str) -> bool {
        match &self.query {
            Query::List => {
                matches!(self.request.output, Output::Suggestions)
                    || !self.immediate_list()
                    || path == self.base
            }
            Query::Glob { depth, .. } => depth.is_none_or(|depth| {
                let relative = if self.base.is_empty() {
                    path
                } else {
                    path.strip_prefix(&self.base)
                        .unwrap_or(path)
                        .trim_start_matches('/')
                };
                path == self.base || relative.split('/').count() < depth
            }),
            Query::Search(_) => true,
        }
    }

    // A literal pattern prefix prunes names that no match can lie under, so results are unchanged.
    fn within_prefix(&self, path: &str) -> bool {
        let relative = if self.base.is_empty() {
            Some(path)
        } else {
            path.strip_prefix(&self.base)
                .and_then(|rest| rest.strip_prefix('/'))
        };
        relative.is_none_or(|relative| {
            relative
                .split('/')
                .zip(&self.prefix)
                .all(|(part, literal)| part == literal)
        })
    }

    fn directory(&mut self, path: &str, handle: Dir) -> Result<(), ToolError> {
        if !self.visited.insert(path.into()) {
            return Ok(());
        }
        self.control(path, &handle)?;
        let mut names = Vec::new();
        for entry in handle.entries().map_err(|_| ToolError::Path)? {
            self.check_time()?;
            self.scanned += 1;
            if self.scanned > MAX_SCANNED_ENTRIES {
                return self.bound();
            }
            let entry = entry.map_err(|_| ToolError::Path)?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let relative = joined(path, &name);
            if relative.split('/').any(crate::validate::forbidden) {
                continue;
            }
            if relative.len() > crate::validate::MAX_PATH_BYTES {
                return Err(ToolError::Limit);
            }
            if valid_relative(&relative, false)
                && !(self.request.hidden)(&relative)
                && self.within_prefix(&relative)
            {
                let directory = entry.file_type().is_ok_and(|kind| kind.is_dir());
                names.push((if directory { format!("{name}/") } else { name }, relative));
            }
        }
        // Sorting directories as `name/` visits files in full-path byte order.
        names.sort();
        for (_, name) in names {
            if self.stopped {
                break;
            }
            self.entry(&name, &handle)?;
        }
        Ok(())
    }

    fn verify_controls(&self) -> Result<(), ToolError> {
        use std::os::unix::fs::MetadataExt;
        for (path, original) in &self.controls {
            self.check_time()?;
            let Some((metadata, bytes)) = original else {
                if !matches!(
                    fs::open_file(self.request.root, path),
                    Err(ToolError::NotFound)
                ) {
                    return Err(ToolError::ChangedInput);
                }
                continue;
            };
            let file = fs::open_file(self.request.root, path)?;
            let current = file.metadata().map_err(|_| ToolError::Path)?;
            if (
                metadata.dev(),
                metadata.ino(),
                metadata.mtime(),
                metadata.mtime_nsec(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            ) != (
                current.dev(),
                current.ino(),
                current.mtime(),
                current.mtime_nsec(),
                current.ctime(),
                current.ctime_nsec(),
            ) || fs::read_regular(file, CONTROL_BYTES)? != *bytes
            {
                return Err(ToolError::ChangedInput);
            }
        }
        Ok(())
    }

    fn render(&mut self) -> Result<String, ToolError> {
        match self.request.output {
            Output::Suggestions => self.names(),
            Output::Tool => match std::mem::replace(&mut self.query, Query::List) {
                Query::List => self.list(&self.base),
                Query::Glob { offset, limit, .. } => super::glob::render(
                    self.entries
                        .iter()
                        .map(|(path, node)| (path.clone(), node.bytes))
                        .collect(),
                    offset,
                    limit,
                    self.request.result_bytes,
                ),
                Query::Search(search) => search.finish(self.request.result_bytes),
            },
        }
    }

    fn names(&self) -> Result<String, ToolError> {
        let index = SuggestionIndex {
            names: self
                .entries
                .iter()
                .map(|(path, node)| format!("{path}{}", if node.directory { "/" } else { "" }))
                .collect(),
        };
        let output = serde_json::to_string(&index).map_err(|_| ToolError::Operation)?;
        if index.names.len() > INDEX_NAMES
            || output.len() > INDEX_BYTES.min(self.request.result_bytes)
        {
            return Err(ToolError::Limit);
        }
        Ok(output)
    }

    fn list(&self, base: &str) -> Result<String, ToolError> {
        let base = if base == "." { "" } else { base };
        let rows: Vec<_> = self
            .entries
            .iter()
            .filter(|(path, node)| {
                if self.immediate_list() {
                    path.rsplit_once('/').map_or("", |(parent, _)| parent) == base
                } else {
                    !node.directory
                        || !self
                            .entries
                            .range(format!("{path}/")..)
                            .next()
                            .is_some_and(|(child, _)| child.starts_with(&format!("{path}/")))
                }
            })
            .map(|(path, node)| {
                if self.immediate_list() {
                    json!(format!("{path}{}", if node.directory { "/" } else { "" }))
                } else if node.directory {
                    json!({"path":path,"kind":"directory"})
                } else {
                    json!({"path":path,"bytes":node.bytes})
                }
            })
            .collect();
        let truncated = self.stopped || rows.len() > DISPLAY_NAMES;
        fs::fit_rows(
            rows.into_iter().take(DISPLAY_NAMES).collect(),
            truncated,
            self.request.result_bytes,
        )
    }
}

fn joined(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.into()
    } else {
        format!("{parent}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vanished_descendants_are_omitted_after_enumeration() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fs::open_directory(fixture.path()).unwrap();
        let config = serde_json::from_value(json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]})).unwrap();
        let calls = [
            (json!({"operation":"list","path":"."}), Output::Tool),
            (json!({"operation":"glob","pattern":"**"}), Output::Tool),
            (
                json!({"operation":"search","path":".","query":"needle"}),
                Output::Tool,
            ),
            (json!({"operation":"list","path":"."}), Output::Suggestions),
        ];
        for (call, output) in calls {
            std::fs::write(fixture.path().join("first-visible.txt"), b"needle").unwrap();
            let removed = fixture.path().join("second-removed.txt");
            std::fs::write(&removed, b"needle").unwrap();
            let call = serde_json::from_value(call).unwrap();
            let mut walk = Walk::new(Request {
                root: &root,
                call: &call,
                config: &config,
                hidden: fs::NONE,
                result_bytes: super::super::types::MAX_TOOL_RESULT_BYTES,
                deadline: Instant::now() + super::super::selection::DISCOVERY_TIME,
                output,
                cancellation: None,
            })
            .unwrap();
            walk.selector.in_progress =
                Some(Box::new(move || std::fs::remove_file(removed).unwrap()));
            let result = walk.inspect().unwrap();
            assert!(result.contains("first-visible.txt"));
            assert!(!result.contains("second-removed.txt"));
        }
    }

    #[test]
    fn missing_requested_bases_fail_but_absent_scope_roots_are_omitted() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fs::open_directory(fixture.path()).unwrap();
        let config = serde_json::from_value(json!({"version":1,"workspace_paths":["missing/nested"],"write":false,"commands":[],"skills":[],"mcp":[]})).unwrap();
        for path in [".", "missing/nested"] {
            let call = ToolCall::List { path: path.into() };
            let result = inspect(Request {
                root: &root,
                call: &call,
                config: &config,
                hidden: fs::NONE,
                result_bytes: super::super::types::MAX_TOOL_RESULT_BYTES,
                deadline: Instant::now() + super::super::selection::DISCOVERY_TIME,
                output: Output::Tool,
                cancellation: None,
            });
            if path == "." {
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&result.unwrap()).unwrap()["entries"],
                    json!([])
                );
            } else {
                assert!(matches!(result, Err(ToolError::NotFound)));
            }
        }
    }

    #[test]
    fn listings_past_the_entry_bound_return_a_truncated_page() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(fixture.path().join("notes/deep")).unwrap();
        for index in 0..=MAX_LIST_ENTRIES {
            std::fs::write(fixture.path().join(format!("notes/deep/e{index:05}")), b"").unwrap();
        }
        let root = fs::open_directory(fixture.path()).unwrap();
        for (scope, path, first) in [
            ("notes", "notes", "notes/deep/e00000"),
            (".", "notes/deep", "notes/deep/e00000"),
        ] {
            let config = serde_json::from_value(json!({"version":1,"workspace_paths":[scope],"write":false,"commands":[],"skills":[],"mcp":[]})).unwrap();
            let call = ToolCall::List { path: path.into() };
            let page: serde_json::Value = serde_json::from_str(
                &inspect(tool_request(&root, &call, &config, Output::Tool)).unwrap(),
            )
            .unwrap();
            let entries = page["entries"].as_array().unwrap();
            assert_eq!(entries.len(), DISPLAY_NAMES, "{scope}");
            let first_row = entries[0]
                .get("path")
                .unwrap_or(&entries[0])
                .as_str()
                .unwrap();
            assert_eq!(first_row, first, "{scope}");
            assert_eq!(page["may_be_truncated"], true, "{scope}");
        }
    }

    #[test]
    fn listings_mark_truncation_only_when_a_row_is_omitted() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::create_dir(fixture.path().join("notes")).unwrap();
        let root = fs::open_directory(fixture.path()).unwrap();
        let call = ToolCall::List {
            path: "notes".into(),
        };
        let mut created = 0;
        for count in [DISPLAY_NAMES - 1, DISPLAY_NAMES, DISPLAY_NAMES + 1] {
            while created < count {
                std::fs::write(fixture.path().join(format!("notes/entry-{created}")), b"").unwrap();
                created += 1;
            }
            for scope in [".", "notes"] {
                let config = serde_json::from_value(json!({"version":1,"workspace_paths":[scope],"write":false,"commands":[],"skills":[],"mcp":[]})).unwrap();
                let output = inspect(Request {
                    root: &root,
                    call: &call,
                    config: &config,
                    hidden: fs::NONE,
                    result_bytes: super::super::types::MAX_TOOL_RESULT_BYTES,
                    deadline: Instant::now() + super::super::selection::DISCOVERY_TIME,
                    output: Output::Tool,
                    cancellation: None,
                })
                .unwrap();
                let result: serde_json::Value = serde_json::from_str(&output).unwrap();
                assert_eq!(
                    result["entries"].as_array().unwrap().len(),
                    count.min(DISPLAY_NAMES)
                );
                assert_eq!(result["may_be_truncated"], count > DISPLAY_NAMES);
            }
        }
    }

    #[test]
    fn suggestion_indices_are_complete_bounded_and_refinable_without_controls() {
        let index = SuggestionIndex {
            names: (0..INDEX_NAMES)
                .map(|i| format!("notes/item-{i:04}.txt"))
                .collect(),
        };
        let encoded = serde_json::to_string(&index).unwrap();
        assert!(encoded.len() <= INDEX_BYTES);
        let index = SuggestionIndex::decode(&encoded).unwrap();
        let found = index.refine("notes", "ITEM").unwrap();
        assert_eq!(found.len(), DISPLAY_NAMES);
        assert_eq!(found[0], "notes/item-0000.txt");
        assert!(index.refine("", "").unwrap().is_empty());
        for names in [
            vec![".ENV/secret".into()],
            vec!["normal".into(); INDEX_NAMES + 1],
            vec!["x".repeat(INDEX_BYTES)],
        ] {
            let output = serde_json::to_string(&SuggestionIndex { names }).unwrap();
            assert!(SuggestionIndex::decode(&output).is_err());
        }
    }

    #[test]
    fn local_suggestion_reads_reject_entry_byte_and_depth_overflow() {
        enum Bound {
            Entries,
            Bytes,
            Depth,
        }
        crate::test_support::isolate_account_root();
        for bound in [Bound::Entries, Bound::Bytes, Bound::Depth] {
            let fixture = tempfile::tempdir().unwrap();
            let workspace = fixture.path().join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            match bound {
                Bound::Entries => {
                    for index in 0..=INDEX_NAMES {
                        std::fs::write(workspace.join(format!("entry-{index}")), b"").unwrap();
                    }
                }
                Bound::Bytes => {
                    let prefix = "entry".repeat(20);
                    for index in 0..=INDEX_BYTES / prefix.len() {
                        std::fs::write(workspace.join(format!("{prefix}-{index}")), b"").unwrap();
                    }
                }
                Bound::Depth => {
                    let mut directory = workspace.clone();
                    for _ in 0..MAX_DEPTH {
                        directory.push("folder");
                    }
                    std::fs::create_dir_all(directory).unwrap();
                }
            }
            let source = FileSuggestionSource::new(
                crate::StateRoot::admit(&fixture.path().join("state")).unwrap(),
                &workspace,
                crate::ToolGrant::None,
            )
            .unwrap();
            let generation = source.generation().unwrap();
            assert!(matches!(
                source.read(String::new(), generation, super::super::Cancellation::new()),
                Err(ToolError::Limit)
            ));
        }
    }

    #[test]
    fn suggestions_share_nested_selection_protection_and_read_restrictions() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("notes/pruned")).unwrap();
        std::fs::write(
            temp.path().join(".gitignore"),
            b"*.cache\n!.env\n!.aws/\nnotes/pruned/\n",
        )
        .unwrap();
        for path in [
            "notes/visible.txt",
            "notes/ignored.cache",
            "notes/private.txt",
            "notes/.ENV",
            "notes/pruned/leaf",
        ] {
            std::fs::write(temp.path().join(path), b"fixture payload").unwrap();
        }
        let root = fs::open_directory(temp.path()).unwrap();
        let config = serde_json::from_value(json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]})).unwrap();
        let call = ToolCall::List {
            path: "notes".into(),
        };
        let output = inspect(Request {
            root: &root,
            call: &call,
            config: &config,
            hidden: &|p| p == "notes/private.txt",
            result_bytes: super::super::types::MAX_TOOL_RESULT_BYTES,
            deadline: Instant::now() + super::super::selection::DISCOVERY_TIME,
            output: Output::Suggestions,
            cancellation: None,
        })
        .unwrap();
        let index = SuggestionIndex::decode(&output).unwrap();
        assert_eq!(index.refine("notes", "").unwrap(), ["notes/visible.txt"]);
        assert!(!output.contains("payload") && !output.contains("ignored.cache"));
        std::fs::write(temp.path().join(".gitignore"), b"*").unwrap();
        assert_eq!(
            index.refine("notes", "visible").unwrap(),
            ["notes/visible.txt"]
        );
    }

    fn tool_request<'a>(
        root: &'a Dir,
        call: &'a ToolCall,
        config: &'a Config,
        output: Output,
    ) -> Request<'a> {
        Request {
            root,
            call,
            config,
            hidden: fs::NONE,
            result_bytes: super::super::types::MAX_TOOL_RESULT_BYTES,
            deadline: Instant::now() + super::super::selection::DISCOVERY_TIME,
            output,
            cancellation: None,
        }
    }

    fn whole_workspace() -> Config {
        serde_json::from_value(json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]})).unwrap()
    }

    fn search_call(path: &str, query: &str, include: Option<&str>) -> ToolCall {
        ToolCall::Search {
            path: path.into(),
            query: query.into(),
            regex: false,
            include: include.map(str::to_owned),
            case_sensitive: true,
            offset: 0,
            limit: super::super::types::MAX_SEARCH_ROWS,
        }
    }

    #[test]
    fn directories_replaced_by_symlinks_mid_walk_are_never_followed() {
        let config = whole_workspace();
        for replaced in ["a", "a/sub"] {
            let fixture = tempfile::tempdir().unwrap();
            let workspace = fixture.path().join("workspace");
            std::fs::create_dir_all(workspace.join("a/sub")).unwrap();
            std::fs::write(workspace.join("a/sub/inside.txt"), b"needle inside").unwrap();
            std::fs::create_dir(fixture.path().join("outside")).unwrap();
            std::fs::write(
                fixture.path().join("outside/secret.txt"),
                b"needle OUTSIDE_CANARY",
            )
            .unwrap();
            let root = fs::open_directory(&workspace).unwrap();
            for (call, output) in [
                (search_call(".", "needle", None), Output::Tool),
                (
                    serde_json::from_value(json!({"operation":"glob","pattern":"**"})).unwrap(),
                    Output::Tool,
                ),
                (ToolCall::List { path: ".".into() }, Output::Suggestions),
            ] {
                let target = workspace.join(replaced);
                if std::fs::symlink_metadata(&target).unwrap().is_symlink() {
                    std::fs::remove_file(&target).unwrap();
                    std::fs::rename(fixture.path().join("moved"), &target).unwrap();
                }
                let mut walk = Walk::new(tool_request(&root, &call, &config, output)).unwrap();
                let (outside, moved) =
                    (fixture.path().join("outside"), fixture.path().join("moved"));
                walk.selector.in_progress = Some(Box::new(move || {
                    std::fs::rename(&target, moved).unwrap();
                    std::os::unix::fs::symlink(outside, &target).unwrap();
                }));
                // A replaced ancestor refuses the walk; a replaced descendant is omitted.
                match walk.inspect() {
                    Ok(result) => assert!(
                        replaced == "a/sub"
                            && !result.contains("OUTSIDE_CANARY")
                            && !result.contains("secret.txt"),
                        "{replaced}: {result}"
                    ),
                    Err(error) => assert!(
                        replaced == "a" && matches!(error, ToolError::Path),
                        "{replaced}"
                    ),
                }
            }
        }
    }

    #[test]
    fn nested_repositories_are_pruned_unless_requested() {
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path();
        for (path, content) in [
            ("src/lib.rs", "needle"),
            ("worktrees/copy/src/lib.rs", "needle"),
            ("worktrees/copy/.git", "gitdir: elsewhere"),
            ("vendor/repo/.git/HEAD", "ref: refs/heads/main"),
            ("vendor/repo/file.rs", "needle"),
            (".git/HEAD", "ref: refs/heads/main"),
        ] {
            std::fs::create_dir_all(workspace.join(path).parent().unwrap()).unwrap();
            std::fs::write(workspace.join(path), content).unwrap();
        }
        let root = fs::open_directory(workspace).unwrap();
        let config = whole_workspace();
        let glob = serde_json::from_value(json!({"operation":"glob","pattern":"**"})).unwrap();
        let list = ToolCall::List { path: ".".into() };
        let search = search_call(".", "needle", None);
        for (call, output) in [
            (&search, Output::Tool),
            (&glob, Output::Tool),
            (&list, Output::Suggestions),
        ] {
            let result = inspect(tool_request(&root, call, &config, output)).unwrap();
            assert!(result.contains("src/lib.rs"), "{result}");
            assert!(
                !result.contains("copy/src") && !result.contains("repo/file.rs"),
                "{result}"
            );
        }
        let requested = search_call("worktrees/copy", "needle", None);
        let result = inspect(tool_request(&root, &requested, &config, Output::Tool)).unwrap();
        assert!(result.contains("worktrees/copy/src/lib.rs"), "{result}");
    }

    #[test]
    fn include_prefixes_prune_the_walk_without_changing_results() {
        assert_eq!(
            super::super::glob::literal_prefix("src/deep/*.rs"),
            ["src", "deep"]
        );
        assert_eq!(super::super::glob::literal_prefix("src/**/x.rs"), ["src"]);
        assert!(super::super::glob::literal_prefix("{src,lib}/x.rs").is_empty());
        assert!(super::super::glob::literal_prefix("*.rs").is_empty());
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path();
        for path in ["src/a.rs", "src/deep/b.rs", "srcx/c.rs", "other/d.rs"] {
            std::fs::create_dir_all(workspace.join(path).parent().unwrap()).unwrap();
            std::fs::write(workspace.join(path), "needle").unwrap();
        }
        // An unreadable control fails any walk that enters `other`.
        std::fs::write(workspace.join("other/.gitignore"), [0xff]).unwrap();
        let root = fs::open_directory(workspace).unwrap();
        let config = whole_workspace();
        let pruned = search_call(".", "needle", Some("src/**/*.rs"));
        let result: serde_json::Value = serde_json::from_str(
            &inspect(tool_request(&root, &pruned, &config, Output::Tool)).unwrap(),
        )
        .unwrap();
        let paths: Vec<_> = result["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, ["src/a.rs", "src/deep/b.rs"]);
        let glob =
            serde_json::from_value(json!({"operation":"glob","pattern":"src/*.rs"})).unwrap();
        let output = inspect(tool_request(&root, &glob, &config, Output::Tool)).unwrap();
        assert!(
            output.contains("src/a.rs") && !output.contains("deep"),
            "{output}"
        );
        let unpruned = search_call(".", "needle", Some("**/*.rs"));
        assert!(inspect(tool_request(&root, &unpruned, &config, Output::Tool)).is_err());
    }

    #[test]
    fn search_skips_unreadable_files_and_returns_partial_pages_at_scan_bounds() {
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path();
        std::fs::write(workspace.join("a.txt"), "needle one").unwrap();
        std::fs::write(workspace.join("b.bin"), b"needle \xff binary").unwrap();
        std::fs::File::create(workspace.join("c.big"))
            .unwrap()
            .set_len(MAX_FILE_BYTES as u64 + 1)
            .unwrap();
        std::fs::write(workspace.join("d.txt"), "needle two").unwrap();
        let root = fs::open_directory(workspace).unwrap();
        let config = whole_workspace();
        let call = search_call(".", "needle", None);
        let page: serde_json::Value = serde_json::from_str(
            &inspect(tool_request(&root, &call, &config, Output::Tool)).unwrap(),
        )
        .unwrap();
        assert_eq!(page["entries"].as_array().unwrap().len(), 2);
        assert_eq!(
            (
                page["skipped_large"].as_u64(),
                page["skipped_binary"].as_u64()
            ),
            (Some(1), Some(1))
        );
        assert_eq!(page["may_be_truncated"], false);

        let mut walk = Walk::new(tool_request(&root, &call, &config, Output::Tool)).unwrap();
        walk.scan_until = Instant::now();
        let page: serde_json::Value = serde_json::from_str(&walk.inspect().unwrap()).unwrap();
        assert_eq!(page["entries"][0]["path"], "a.txt");
        assert_eq!(page["entries"].as_array().unwrap().len(), 1);
        assert_eq!(page["may_be_truncated"], true);
        assert_eq!(page["next_offset"], 1);

        let glob = ToolCall::Glob {
            pattern: "*.txt".into(),
            path: ".".into(),
            offset: 0,
            limit: 100,
        };
        for call in [
            search_call(".", "needle", None),
            search_call(".", "absent", None),
            glob,
        ] {
            let mut walk = Walk::new(tool_request(&root, &call, &config, Output::Tool)).unwrap();
            walk.scanned = MAX_SCANNED_ENTRIES;
            assert!(matches!(walk.inspect(), Err(ToolError::Limit)), "{call:?}");
        }
    }
}
