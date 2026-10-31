use crate::store::{StateRoot, StoreError};
use serde::{Deserialize, Serialize};

pub(super) const PRIMARY_AUTHORITY: &str = include_str!("prompts/authority/primary.md");
pub(super) const TEAM_AUTHORITY: &str = include_str!("prompts/authority/team.md");
pub(super) const RESEARCH_AUTHORITY: &str = include_str!("prompts/authority/researcher.md");
pub(super) const EDITOR_AUTHORITY: &str = include_str!("prompts/authority/editor.md");
pub(super) const READ_ONLY_INSTRUCTIONS: &str = include_str!("prompts/read_only.md");
pub(super) const PLAN_INSTRUCTIONS: &str = include_str!("prompts/plan.md");
const COMPACTION_AUTHORITY: &str = include_str!("prompts/authority/compaction.md");
pub(crate) const TOOL_REVIEW_INSTRUCTIONS: &str = include_str!("prompts/tool_review.md");

pub const MAX_PROMPT_OVERRIDE_BYTES: usize = 32 * 1024;
// Kept small so worst-case JSON escaping (6 bytes per control byte) stays inside the compaction envelope.
pub const MAX_COMPACTION_PROMPT_OVERRIDE_BYTES: usize = 1024;
pub(crate) const PROMPT_OVERRIDE_DIRECTORY: &str = "prompts";

/// A working-style prompt a user may replace from `<StateRoot>/prompts/`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptFile {
    Primary,
    Team,
    Researcher,
    Editor,
    Compaction,
}

impl PromptFile {
    pub const ALL: [Self; 5] = [
        Self::Primary,
        Self::Team,
        Self::Researcher,
        Self::Editor,
        Self::Compaction,
    ];

    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Primary => "primary.md",
            Self::Team => "team.md",
            Self::Researcher => "researcher.md",
            Self::Editor => "editor.md",
            Self::Compaction => "compaction.md",
        }
    }

    pub const fn limit(self) -> usize {
        match self {
            Self::Compaction => MAX_COMPACTION_PROMPT_OVERRIDE_BYTES,
            Self::Primary | Self::Team | Self::Researcher | Self::Editor => {
                MAX_PROMPT_OVERRIDE_BYTES
            }
        }
    }

    const fn default_text(self) -> &'static str {
        match self {
            Self::Primary => include_str!("prompts/primary.md"),
            Self::Team => include_str!("prompts/team.md"),
            Self::Researcher => include_str!("prompts/researcher.md"),
            Self::Editor => include_str!("prompts/editor.md"),
            Self::Compaction => include_str!("prompts/compaction.md"),
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptSource {
    Default,
    Override,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptPin {
    pub file: PromptFile,
    pub source: PromptSource,
    pub digest: [u8; 32],
}

impl PromptPin {
    pub(crate) fn pins_valid(pins: &[Self]) -> bool {
        pins.len() == PromptFile::ALL.len()
            && pins
                .iter()
                .zip(PromptFile::ALL)
                .all(|(pin, file)| pin.file == file)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PromptOverrideError {
    #[error("prompt override {path} exceeds {limit} bytes")]
    TooLarge { path: String, limit: usize },
    #[error("prompt override {path} is not UTF-8")]
    NotUtf8 { path: String },
    #[error("prompt override {path} is empty; delete it to use the default")]
    Empty { path: String },
    #[error(
        "prompt override {path} must be a regular file owned by you with mode 0600, in a directory with mode 0700, and not a link"
    )]
    NotPrivate { path: String },
    #[error("prompt override {path} is unavailable")]
    Unavailable {
        path: String,
        #[source]
        source: StoreError,
    },
}

/// The working-style prompts one Run uses: compiled defaults unless the user's StateRoot overrides them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EffectivePrompts {
    overrides: [Option<std::sync::Arc<str>>; PromptFile::ALL.len()],
}

impl EffectivePrompts {
    pub(crate) fn load(state: &StateRoot) -> Result<Self, PromptOverrideError> {
        Self::load_files(state, &PromptFile::ALL)
    }

    pub(crate) fn load_files(
        state: &StateRoot,
        files: &[PromptFile],
    ) -> Result<Self, PromptOverrideError> {
        let mut prompts = Self::default();
        for &file in files {
            let path = state
                .path()
                .join(PROMPT_OVERRIDE_DIRECTORY)
                .join(file.file_name())
                .display()
                .to_string();
            let bytes = match state.read_user_file(
                PROMPT_OVERRIDE_DIRECTORY,
                file.file_name(),
                file.limit(),
            ) {
                Ok(None) => continue,
                Ok(Some(bytes)) => bytes,
                Err(StoreError::StateNotPrivate) => {
                    return Err(PromptOverrideError::NotPrivate { path });
                }
                Err(source) => return Err(PromptOverrideError::Unavailable { path, source }),
            };
            if bytes.len() > file.limit() {
                return Err(PromptOverrideError::TooLarge {
                    path,
                    limit: file.limit(),
                });
            }
            let text = String::from_utf8(bytes)
                .map_err(|_| PromptOverrideError::NotUtf8 { path: path.clone() })?;
            if text.trim().is_empty() {
                return Err(PromptOverrideError::Empty { path });
            }
            prompts.overrides[file.index()] = Some(text.into());
        }
        Ok(prompts)
    }

    pub fn text(&self, file: PromptFile) -> &str {
        self.overrides[file.index()]
            .as_deref()
            .unwrap_or(file.default_text())
    }

    pub fn pin(&self, file: PromptFile) -> PromptPin {
        PromptPin {
            file,
            source: if self.overrides[file.index()].is_some() {
                PromptSource::Override
            } else {
                PromptSource::Default
            },
            digest: crate::validate::digest(self.text(file).as_bytes()),
        }
    }

    pub fn pins(&self) -> Vec<PromptPin> {
        PromptFile::ALL.map(|file| self.pin(file)).to_vec()
    }

    pub(crate) fn compaction_instructions(&self) -> String {
        format!(
            "{}\n\n{COMPACTION_AUTHORITY}",
            self.text(PromptFile::Compaction)
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_override(mut self, file: PromptFile, text: &str) -> Self {
        self.overrides[file.index()] = Some(text.into());
        self
    }
}

#[cfg(test)]
mod tests;
