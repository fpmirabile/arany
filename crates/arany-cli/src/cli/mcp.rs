use super::{CommandOutput, state_dir};
use crate::presentation::escape_terminal;
use arany::{McpAuthError, StateRoot, mcp_login, mcp_logout};
use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Args)]
pub(crate) struct McpArgs {
    #[command(subcommand)]
    action: McpAction,
}

#[derive(Subcommand)]
enum McpAction {
    #[command(about = "Authorize a remote MCP server configured with OAuth")]
    Login {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        server: String,
    },
    #[command(about = "Revoke and remove a remote MCP server's saved authorization")]
    Logout {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        server: String,
    },
}

fn describe(error: McpAuthError) -> String {
    error.to_string()
}

fn workspace() -> Result<PathBuf, String> {
    std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map_err(|_| "Workspace unavailable".to_owned())
}

fn admit(state: Option<PathBuf>) -> Result<StateRoot, String> {
    let path = state_dir(state).map_err(str::to_owned)?;
    StateRoot::open_existing(&path).map_err(|_| "state directory unavailable".to_owned())
}

pub(crate) async fn run(args: McpArgs) -> Result<CommandOutput, String> {
    match args.action {
        McpAction::Login { state_dir, server } => {
            let state = admit(state_dir)?;
            let workspace = workspace()?;
            mcp_login(&state, &workspace, &server, |url| {
                eprintln!(
                    "The token is saved unencrypted in your private account directory; other applications running as your user can read it.\nOpen this URL in your browser to authorize {}:\n{}",
                    escape_terminal(&server),
                    escape_terminal(url)
                );
            })
            .await
            .map_err(describe)?;
            Ok(CommandOutput {
                stdout: format!("Authorized {}\n", escape_terminal(&server)),
                stderr: String::new(),
                success: true,
            })
        }
        McpAction::Logout { state_dir, server } => {
            let state = admit(state_dir)?;
            let workspace = workspace()?;
            let revoked = mcp_logout(&state, &workspace, &server)
                .await
                .map_err(describe)?;
            Ok(CommandOutput {
                stdout: format!("Removed the authorization for {}\n", escape_terminal(&server)),
                stderr: if revoked {
                    String::new()
                } else {
                    "warning: the authorization server did not confirm revocation\n".to_owned()
                },
                success: true,
            })
        }
    }
}
