use super::{ToolError, ToolRuntime, fs, git};
use crate::session::worker::EditorChange;
use crate::session::{AgentRunId, SessionId};
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[cfg(test)]
mod tests;

pub(super) const EDITORS: &str = "editors";
// Populating an editor workspace, or diffing it at integration, walks the project once within this time.
pub(crate) const EDITOR_WORKSPACE_TIME: Duration = Duration::from_secs(120);

impl ToolRuntime {
    fn session_editors(&self, session: SessionId) -> Result<PathBuf, ToolError> {
        let state = self
            .cache_state
            .as_ref()
            .ok_or(ToolError::ProtectionUnavailable)?;
        Ok(state.path().join(EDITORS).join(session.to_string()))
    }

    fn editor_directory(&self, session: SessionId, id: AgentRunId) -> Result<PathBuf, ToolError> {
        Ok(self.session_editors(session)?.join(id.to_string()))
    }

    // A resumed editor adopts its source's kept workspace; any other editor gets a fresh clone.
    pub(crate) fn create_editor_workspace(
        &self,
        session: SessionId,
        id: AgentRunId,
        resumes: Option<AgentRunId>,
    ) -> Result<(), ToolError> {
        let directory = self.editor_directory(session, id)?;
        let parent = directory.parent().ok_or(ToolError::Path)?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|_| ToolError::Operation)?;
        if let Some(source) = resumes {
            let kept = self.editor_directory(session, source)?;
            if fs::open_directory(&kept).is_ok_and(|kept| fs::editor::ready(&kept)) {
                std::fs::rename(&kept, &directory).map_err(|_| ToolError::Operation)?;
                let tree = self.tree_in(&directory)?;
                if let Some(record) = fs::open_directory(&directory)
                    .ok()
                    .and_then(|directory| fs::editor::worktree(&directory))
                {
                    git::relocate_worktree(self, &tree, &record)?;
                }
                return self.attach_editor(id, &tree);
            }
        }
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .map_err(|_| ToolError::Operation)?;
        let created = fs::open_directory(&self.workspace).and_then(|workspace| {
            if fs::workspace_identity(&workspace)? != self.workspace_identity {
                return Err(ToolError::ChangedInput);
            }
            let editor = fs::open_directory(&directory)?;
            match git::create_worktree(self, &directory, &format!("{}{id}", git::WORKTREE_PREFIX))?
            {
                Some((record, tracked, untracked)) => fs::editor::complete_worktree(
                    &editor, &workspace, &record, &tracked, &untracked,
                ),
                None => fs::editor::create(
                    &editor,
                    &workspace,
                    &self.config,
                    Instant::now() + EDITOR_WORKSPACE_TIME,
                ),
            }
        });
        if let Err(error) = created {
            self.discard_editor_directory(&directory);
            return Err(error);
        }
        self.attach_editor(id, &self.tree_in(&directory)?)
    }

    // A worktree editor's tree carries its Git name; a copy's is the fixed tree directory.
    fn tree_in(&self, directory: &std::path::Path) -> Result<PathBuf, ToolError> {
        let record = fs::editor::worktree(&fs::open_directory(directory)?);
        Ok(directory.join(record.map_or(fs::editor::TREE.to_owned(), |record| record.name)))
    }

    // Arany's own worktree leaves Git's records before its directory is deleted.
    fn discard_editor_directory(&self, directory: &std::path::Path) {
        let record = fs::open_directory(directory)
            .ok()
            .and_then(|directory| fs::editor::worktree(&directory));
        let tree = record
            .as_ref()
            .map(|record| directory.join(&record.name))
            .or_else(|| {
                std::fs::read_dir(directory)
                    .ok()?
                    .flatten()
                    .find_map(|entry| {
                        let name = entry.file_name().into_string().ok()?;
                        name.starts_with(git::WORKTREE_PREFIX).then(|| entry.path())
                    })
            });
        if let Some(tree) = tree {
            let name = tree
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_owned();
            let record = record.unwrap_or(git::Worktree {
                name,
                base: String::new(),
            });
            git::remove_worktree(self, &tree, &record);
        }
        let _ = std::fs::remove_dir_all(directory);
    }

    #[cfg(test)]
    fn editor_tree_path(&self, id: AgentRunId) -> Option<PathBuf> {
        self.editor_root(id).map(|root| root.path)
    }

    pub(crate) fn editor_changes(
        &self,
        id: AgentRunId,
        files: u32,
        bytes: u64,
    ) -> Result<Vec<EditorChange>, ToolError> {
        let root = self.editor_root(id).ok_or(ToolError::Configuration)?;
        let directory = fs::open_directory(root.path.parent().ok_or(ToolError::Path)?)?;
        let workspace = fs::open_directory(&self.workspace)?;
        if fs::workspace_identity(&workspace)? != self.workspace_identity {
            return Err(ToolError::ChangedInput);
        }
        let limits = fs::editor::ChangeLimits {
            files: files as usize,
            bytes,
        };
        if let Some(record) = fs::editor::worktree(&directory) {
            let (tracked, untracked) = git::worktree_changes(self, &root.path, &record)?;
            return fs::editor::worktree_changes(
                &directory, &workspace, &limits, tracked, &untracked,
            );
        }
        fs::editor::changes(
            &directory,
            &workspace,
            Instant::now() + EDITOR_WORKSPACE_TIME,
            &limits,
        )
    }

    // Integrated or unchanged workspaces are deleted; others stay for a resume until the sweep.
    pub(crate) fn release_editor_workspace(
        &self,
        session: SessionId,
        id: AgentRunId,
        discard: bool,
    ) {
        self.detach_editor(id);
        if discard && let Ok(directory) = self.editor_directory(session, id) {
            self.discard_editor_directory(&directory);
        }
    }

    // Deletes this Session's editor workspaces that are incomplete or no longer resumable.
    pub(crate) fn sweep_editor_workspaces(
        &self,
        session: SessionId,
        resumable: impl Fn(AgentRunId) -> bool,
    ) {
        let Ok(root) = self.session_editors(session) else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(&root) else {
            return;
        };
        for entry in entries.flatten() {
            let keep = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<AgentRunId>().ok())
                .is_some_and(|id| {
                    resumable(id)
                        && fs::open_directory(&entry.path())
                            .is_ok_and(|directory| fs::editor::ready(&directory))
                });
            if !keep {
                self.discard_editor_directory(&entry.path());
            }
        }
    }
}
