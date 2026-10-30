use super::{Output, Request, SuggestionIndex};
use crate::session::WorkspaceIdentity;
use crate::store::StateRoot;
use crate::tools::{Cancellation, ToolCall, ToolError, ToolGrant, config::Config, fs};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone)]
pub struct FileSuggestionSource {
    state: Arc<StateRoot>,
    workspace: PathBuf,
    identity: WorkspaceIdentity,
    grant: ToolGrant,
}

impl FileSuggestionSource {
    pub fn new(state: StateRoot, workspace: &Path, grant: ToolGrant) -> Result<Self, ToolError> {
        let identity = fs::workspace_identity(&fs::open_directory(workspace)?)?;
        Ok(Self {
            state: Arc::new(state),
            workspace: workspace.into(),
            identity,
            grant,
        })
    }

    fn config(&self) -> Result<Config, ToolError> {
        let paths = match &self.grant {
            ToolGrant::Configured => Config::load(&self.state)?.workspace_paths,
            ToolGrant::Trusted(access) => {
                access.verify(&self.workspace)?;
                if !access.is_trusted()
                    || !crate::tools::WorkspacePermissions::open(&self.workspace)
                        .map_err(|_| ToolError::Configuration)?
                        .is_trusted()
                {
                    return Err(ToolError::ChangedInput);
                }
                vec![".".into()]
            }
            ToolGrant::None => vec![".".into()],
        };
        Ok(Config {
            workspace_paths: paths,
            write: false,
            child_unguarded: crate::tools::UnguardedGrant::Denied,
            child_web: false,
            commands: Vec::new(),
            skills: Vec::new(),
            mcp: Vec::new(),
            runtime_grants: Vec::new(),
            git: None,
            git_identity: None,
            locale: Default::default(),
            environment_sha256: None,
            network: None,
            web_search: None,
        })
    }

    fn pin(&self) -> Result<(Config, Option<crate::tools::PermissionSnapshot>), ToolError> {
        if fs::workspace_identity(&fs::open_directory(&self.workspace)?)? != self.identity {
            return Err(ToolError::ChangedInput);
        }
        Ok((self.config()?, crate::tools::load_permissions(&self.state)?))
    }

    fn digest(config: &Config, permissions: Option<&crate::tools::PermissionSnapshot>) -> [u8; 32] {
        let mut bytes = config.digest().to_vec();
        if let Some(snapshot) = permissions {
            bytes.extend_from_slice(&snapshot.digest);
        }
        crate::validate::digest(&bytes)
    }

    pub fn generation(&self) -> Result<[u8; 32], ToolError> {
        let (config, permissions) = self.pin()?;
        Ok(Self::digest(&config, permissions.as_ref()))
    }

    pub fn read(
        self,
        folder: String,
        generation: [u8; 32],
        cancellation: Cancellation,
    ) -> Result<SuggestionIndex, ToolError> {
        let deadline = std::time::Instant::now() + super::super::selection::DISCOVERY_TIME;
        if cancellation.is_cancelled() {
            return Err(ToolError::Operation);
        }
        crate::tools::types::validate_mention_path(&folder, true)?;
        let (config, permissions) = self.pin()?;
        if Self::digest(&config, permissions.as_ref()) != generation {
            return Err(ToolError::ChangedInput);
        }
        let root = fs::open_directory(&self.workspace)?;
        if fs::workspace_identity(&root)? != self.identity {
            return Err(ToolError::ChangedInput);
        }
        crate::tools::admission_paths(&self.workspace, &self.state, &config, &root)?;
        let call = ToolCall::List {
            path: if folder.is_empty() {
                ".".into()
            } else {
                folder
            },
        };
        let hidden = if let Some(mut snapshot) = permissions {
            let permissions = std::sync::Arc::make_mut(&mut snapshot.config);
            permissions.scope_to(&self.workspace, self.identity);
            if permissions.decision(&call) != crate::tools::permissions::Decision::Allow {
                return Err(ToolError::FilesystemDenied);
            }
            permissions.hidden_paths(&call)
        } else {
            crate::tools::permissions::HiddenPaths::default()
        };
        let output = super::inspect(Request {
            root: &root,
            call: &call,
            config: &config,
            hidden: &|path| hidden.covers(path),
            result_bytes: super::INDEX_BYTES,
            deadline,
            output: Output::Suggestions,
            cancellation: Some(&cancellation),
        })?;
        if cancellation.is_cancelled() {
            return Err(ToolError::Operation);
        }
        if self.generation()? != generation {
            return Err(ToolError::ChangedInput);
        }
        let index = SuggestionIndex::decode(&output)?;
        if std::time::Instant::now() >= deadline {
            return Err(ToolError::Timeout(String::new()));
        }
        Ok(index)
    }
}
