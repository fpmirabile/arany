mod suggestions;
use super::config::Config;
use super::fs::{self, Keep};
use super::glob::{MAX_DEPTH, MAX_SCANNED_ENTRIES};
use super::selection::{CONTROL_BYTES, Selector};
use super::types::MAX_SNAPSHOT_FILES;
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
    let mut bytes = b";typed-discovery-v1-guard-owned-no-follow-controls-restrictive-read-before-payload-no-partial-results-no-fallback".to_vec();
    for limit in [
        super::selection::DISCOVERY_CALLS,
        MAX_DEPTH as u64,
        MAX_SCANNED_ENTRIES as u64,
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
    pub(super) private_image: bool,
    pub(super) cancellation: Option<&'a super::Cancellation>,
}

struct Walk<'a> {
    request: Request<'a>,
    selector: Selector,
    controls: BTreeMap<String, Option<(std::fs::Metadata, Vec<u8>)>>,
    visited: BTreeSet<String>,
    entries: BTreeMap<String, Node>,
    scanned: usize,
    device: u64,
    query: Query<'a>,
    base: String,
}

enum Query<'a> {
    List,
    Glob {
        matcher: regex::Regex,
        depth: Option<usize>,
        limit: u32,
    },
    Search(super::search::Search<'a>),
}

struct Node {
    directory: bool,
    bytes: u64,
    inode: u64,
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
        let query = match request.call {
            ToolCall::List { .. } => Query::List,
            ToolCall::Glob { pattern, limit, .. } => Query::Glob {
                matcher: super::glob::matcher(pattern).map_err(|_| ToolError::Operation)?,
                depth: (!pattern.split('/').any(|part| part == "**"))
                    .then(|| pattern.split('/').count()),
                limit: *limit,
            },
            ToolCall::Search { .. } => {
                Query::Search(super::search::Search::new(request.call, request.deadline)?)
            }
            _ => return Err(ToolError::Operation),
        };
        let device = fs::identity(request.root)?.0;
        Ok(Self {
            selector: Selector::discovery(request.deadline),
            request,
            controls: BTreeMap::new(),
            visited: BTreeSet::new(),
            entries: BTreeMap::new(),
            scanned: 0,
            device,
            query,
            base: path.into(),
        })
    }

    fn inspect(mut self) -> Result<String, ToolError> {
        let base = self.base.clone();
        let path = base.as_str();
        self.check_time()?;
        self.control("")?;
        for selected in &self.request.config.workspace_paths {
            let root = if selected == "."
                || !path.is_empty() && std::path::Path::new(path).starts_with(selected)
            {
                path
            } else if path.is_empty() || std::path::Path::new(selected).starts_with(path) {
                selected
            } else {
                continue;
            };
            self.start(root)?;
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

    fn control(&mut self, directory: &str) -> Result<(), ToolError> {
        let path = joined(directory, ".gitignore");
        if self.controls.contains_key(&path) {
            return Ok(());
        }
        if path.split('/').count() > MAX_DEPTH || path.len() > crate::validate::MAX_PATH_BYTES {
            return Err(ToolError::Limit);
        }
        if self.request.private_image && !self.admitted(&path) {
            return Err(ToolError::FilesystemDenied);
        }
        let parent = fs::open_file(
            self.request.root,
            if directory.is_empty() { "." } else { directory },
        )?;
        let parent = Dir::from_std_file(parent);
        match parent.symlink_metadata(".gitignore") {
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
        let file = fs::open_file(self.request.root, &path)?;
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
            if (self.request.hidden)(ancestor) || self.selector.excluded(ancestor, true)? {
                return Ok(());
            }
            self.control(ancestor)?;
        }
        if path.is_empty() {
            self.directory(path)
        } else {
            self.entry(path)
        }
    }

    fn entry(&mut self, path: &str) -> Result<(), ToolError> {
        self.check_time()?;
        if !valid_relative(path, false) || (self.request.hidden)(path) {
            return Ok(());
        }
        if path.split('/').count() > MAX_DEPTH {
            return Err(ToolError::Limit);
        }
        let (parent, name) = path.rsplit_once('/').unwrap_or((".", path));
        let metadata = fs::open_file(self.request.root, parent).and_then(|file| {
            Dir::from_std_file(file)
                .symlink_metadata(name)
                .map_err(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        ToolError::NotFound
                    } else {
                        ToolError::Path
                    }
                })
        });
        let metadata = match metadata {
            Ok(metadata) => metadata,
            Err(ToolError::NotFound) if path != self.base => return Ok(()),
            Err(error) => return Err(error),
        };
        if path.split('/').any(fs::ignored_directory)
            || self.selector.excluded(path, metadata.is_dir())?
        {
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
            inode: metadata.ino(),
        };
        if !metadata.is_dir()
            && matches!(self.request.call, ToolCall::List { path: requested } | ToolCall::Glob { path: requested, .. } if requested == path)
        {
            return Err(ToolError::Operation);
        }
        if self.admitted(path) && self.candidate(path, &node) {
            self.entries.insert(path.into(), node);
            if self.entries.len() > self.retained_limit() {
                return Err(ToolError::Limit);
            }
        }
        if metadata.is_dir() && self.descend(path) {
            self.directory(path)?;
        }
        Ok(())
    }

    fn immediate_list(&self) -> bool {
        !self.request.private_image && self.request.config.workspace_paths == ["."]
    }

    fn retained_limit(&self) -> usize {
        match self.request.output {
            Output::Suggestions => INDEX_NAMES,
            Output::Tool => match self.query {
                Query::List => {
                    if self.request.private_image {
                        MAX_SNAPSHOT_FILES
                    } else {
                        MAX_SCANNED_ENTRIES
                    }
                }
                Query::Glob { .. } => super::glob::MAX_GLOB_CANDIDATES,
                Query::Search(_) => MAX_SNAPSHOT_FILES,
            },
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

    fn directory(&mut self, path: &str) -> Result<(), ToolError> {
        if !self.visited.insert(path.into()) {
            return Ok(());
        }
        self.control(path)?;
        let directory = Dir::from_std_file(fs::open_file(
            self.request.root,
            if path.is_empty() { "." } else { path },
        )?);
        let mut names = Vec::new();
        for entry in directory.entries().map_err(|_| ToolError::Path)? {
            self.check_time()?;
            self.scanned += 1;
            if self.scanned > MAX_SCANNED_ENTRIES {
                return Err(ToolError::Limit);
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
            if valid_relative(&relative, false) && !(self.request.hidden)(&relative) {
                names.push(relative);
            }
        }
        names.sort();
        for name in names {
            self.entry(&name)?;
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
                Query::Glob { limit, .. } => super::glob::render(
                    self.entries
                        .iter()
                        .map(|(path, node)| (path.clone(), node.bytes))
                        .collect(),
                    limit,
                    self.request.result_bytes,
                ),
                Query::Search(search) => self.search(search),
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
                if self.request.private_image {
                    true
                } else if self.immediate_list() {
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
        if self.request.config.workspace_paths != ["."] && rows.len() > MAX_SNAPSHOT_FILES {
            return Err(ToolError::Limit);
        }
        let truncated = rows.len() > DISPLAY_NAMES;
        fs::fit_rows(
            rows.into_iter().take(DISPLAY_NAMES).collect(),
            truncated,
            self.request.result_bytes,
        )
    }

    fn search(&self, mut search: super::search::Search<'_>) -> Result<String, ToolError> {
        use std::os::unix::fs::MetadataExt;
        let mut bytes = 0usize;
        for (path, node) in &self.entries {
            self.check_time()?;
            if node.directory || !search.includes(path) {
                continue;
            }
            let file = fs::open_file(self.request.root, path)?;
            let metadata = file.metadata().map_err(|_| ToolError::Path)?;
            if metadata.ino() != node.inode || metadata.len() != node.bytes {
                return Err(ToolError::ChangedInput);
            }
            let content = fs::read_regular(file, super::types::MAX_FILE_BYTES)?;
            bytes = bytes.checked_add(content.len()).ok_or(ToolError::Limit)?;
            if bytes > super::types::MAX_SNAPSHOT_BYTES {
                return Err(ToolError::Limit);
            }
            if search.scan(path, &content)? {
                break;
            }
        }
        search.finish(self.request.result_bytes)
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
                private_image: false,
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
                private_image: false,
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
                    private_image: false,
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
    fn private_discovery_refuses_unavailable_ancestor_controls() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("notes")).unwrap();
        std::fs::write(temp.path().join("notes/visible"), b"fixture").unwrap();
        let root = fs::open_directory(temp.path()).unwrap();
        let config = serde_json::from_value(json!({"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]})).unwrap();
        let call = ToolCall::List {
            path: "notes".into(),
        };
        let result = inspect(Request {
            root: &root,
            call: &call,
            config: &config,
            hidden: fs::NONE,
            result_bytes: super::super::types::MAX_TOOL_RESULT_BYTES,
            deadline: Instant::now() + super::super::selection::DISCOVERY_TIME,
            output: Output::Tool,
            private_image: true,
            cancellation: None,
        });
        assert!(matches!(result, Err(ToolError::FilesystemDenied)));
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
            private_image: false,
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
}
