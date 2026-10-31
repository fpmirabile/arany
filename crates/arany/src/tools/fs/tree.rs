use super::*;
use cap_std::fs::MetadataExt;
use std::cmp::Ordering;
use std::time::Instant;

#[cfg(test)]
mod tests;

// Names one walk holds at once across its pending directory levels; the walk holds no inventory.
pub(in crate::tools) const HELD_NAME_BYTES: usize = COMMAND_PATH_METADATA_BYTES;

pub(in crate::tools) struct Node {
    pub(in crate::tools) path: String,
    pub(in crate::tools) metadata: cap_std::fs::Metadata,
}

// Depth-first, no-follow walk of one device in component order, so two walks merge without an inventory.
// Only the root and the current directory stay open: a deep tree must not exhaust a sandbox's
// descriptor limit, so returning to a parent reopens it.
pub(in crate::tools) struct Walk {
    root: Dir,
    levels: Vec<Level>,
    descend: Option<String>,
    device: u64,
    held: usize,
    deadline: Instant,
}

struct Level {
    path: String,
    dir: Option<Dir>,
    names: std::vec::IntoIter<String>,
    bytes: usize,
}

pub(in crate::tools) trait Sorted {
    type Item;
    fn path(item: &Self::Item) -> &str;
    fn next(&mut self) -> Result<Option<Self::Item>, ToolError>;
    // Leaves the directory just returned by `next` unvisited.
    fn skip(&mut self);
}

pub(in crate::tools) enum Pair<L, R> {
    Left(L),
    Right(R),
    Both(L, R),
}

pub(in crate::tools) fn order(left: &str, right: &str) -> Ordering {
    left.split('/').cmp(right.split('/'))
}

pub(in crate::tools) fn joined(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.into()
    } else {
        format!("{parent}/{name}")
    }
}

pub(in crate::tools) fn check_time(deadline: Instant) -> Result<(), ToolError> {
    if Instant::now() >= deadline {
        return Err(ToolError::Timeout(String::new()));
    }
    Ok(())
}

impl Walk {
    pub(in crate::tools) fn new(root: &Dir, deadline: Instant) -> Result<Self, ToolError> {
        let mut walk = Self {
            root: root.try_clone().map_err(|_| ToolError::Path)?,
            levels: Vec::new(),
            descend: None,
            device: identity(root)?.0,
            held: 0,
            deadline,
        };
        walk.push(String::new())?;
        Ok(walk)
    }

    // Opens a directory component by component without following links.
    fn open(&self, path: &str) -> Result<Dir, ToolError> {
        let mut dir = self.root.try_clone().map_err(|_| ToolError::Path)?;
        for name in path.split('/').filter(|name| !name.is_empty()) {
            dir = dir.open_dir_nofollow(name).map_err(|_| ToolError::Path)?;
        }
        if dir.dir_metadata().map_err(|_| ToolError::Path)?.dev() != self.device {
            return Err(ToolError::Path);
        }
        Ok(dir)
    }

    fn push(&mut self, path: String) -> Result<(), ToolError> {
        let dir = self.open(&path)?;
        let mut names = Vec::new();
        let mut bytes = 0usize;
        for entry in dir.entries().map_err(|_| ToolError::Path)? {
            check_time(self.deadline)?;
            let name = entry
                .map_err(|_| ToolError::Path)?
                .file_name()
                .into_string()
                .map_err(|_| ToolError::Path)?;
            bytes += name.len();
            if self.held + bytes > HELD_NAME_BYTES {
                return Err(ToolError::Limit);
            }
            names.push(name);
        }
        names.sort_unstable();
        self.held += bytes;
        if let Some(parent) = self.levels.last_mut() {
            parent.dir = None;
        }
        self.levels.push(Level {
            path,
            dir: Some(dir),
            names: names.into_iter(),
            bytes,
        });
        Ok(())
    }
}

impl Sorted for Walk {
    type Item = Node;

    fn path(item: &Node) -> &str {
        &item.path
    }

    fn next(&mut self) -> Result<Option<Node>, ToolError> {
        if let Some(path) = self.descend.take() {
            self.push(path)?;
        }
        while let Some(level) = self.levels.last() {
            check_time(self.deadline)?;
            let reopened = match level.dir {
                Some(_) => None,
                None => Some(self.open(&level.path)?),
            };
            let level = self.levels.last_mut().ok_or(ToolError::Operation)?;
            if reopened.is_some() {
                level.dir = reopened;
            }
            let Some(name) = level.names.next() else {
                self.held -= level.bytes;
                self.levels.pop();
                continue;
            };
            let metadata = level
                .dir
                .as_ref()
                .ok_or(ToolError::Operation)?
                .symlink_metadata(&name)
                .map_err(|_| ToolError::Path)?;
            let path = joined(&level.path, &name);
            if metadata.is_dir() {
                self.descend = Some(path.clone());
            }
            return Ok(Some(Node { path, metadata }));
        }
        Ok(None)
    }

    fn skip(&mut self) {
        self.descend = None;
    }
}

// Visits both sides in component order; `visit` returning false leaves that directory's subtree unvisited.
pub(in crate::tools) fn diff<L: Sorted, R: Sorted>(
    left: &mut L,
    right: &mut R,
    mut visit: impl FnMut(Pair<L::Item, R::Item>) -> Result<bool, ToolError>,
) -> Result<(), ToolError> {
    let (mut l, mut r) = (left.next()?, right.next()?);
    loop {
        let side = match (&l, &r) {
            (None, None) => return Ok(()),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some(a), Some(b)) => order(L::path(a), R::path(b)),
        };
        match side {
            Ordering::Less => {
                if !visit(Pair::Left(l.take().ok_or(ToolError::Operation)?))? {
                    left.skip();
                }
                l = left.next()?;
            }
            Ordering::Greater => {
                if !visit(Pair::Right(r.take().ok_or(ToolError::Operation)?))? {
                    right.skip();
                }
                r = right.next()?;
            }
            Ordering::Equal => {
                let pair = l.take().zip(r.take()).ok_or(ToolError::Operation)?;
                if !visit(Pair::Both(pair.0, pair.1))? {
                    left.skip();
                    right.skip();
                }
                (l, r) = (left.next()?, right.next()?);
            }
        }
    }
}
