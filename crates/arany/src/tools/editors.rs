use super::{ToolError, ToolRuntime, fs};
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
                return self.attach_editor(id, &directory.join(fs::editor::TREE));
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
            fs::editor::create(
                &fs::open_directory(&directory)?,
                &workspace,
                &self.config,
                Instant::now() + EDITOR_WORKSPACE_TIME,
            )
        });
        if let Err(error) = created {
            let _ = std::fs::remove_dir_all(&directory);
            return Err(error);
        }
        self.attach_editor(id, &directory.join(fs::editor::TREE))
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
        fs::editor::changes(
            &directory,
            &workspace,
            Instant::now() + EDITOR_WORKSPACE_TIME,
            &fs::editor::ChangeLimits {
                files: files as usize,
                bytes,
            },
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
            let _ = std::fs::remove_dir_all(directory);
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
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}
