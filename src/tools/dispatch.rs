use super::catalog::{Family, Site};
use super::guard::SiteContext;
use super::types::ToolCall;
use super::{ToolError, fs, git, mcp, process, skills};

pub(super) async fn run(site: &SiteContext<'_>) -> Result<String, ToolError> {
    let call = &site.intent.call;
    let facts = call.facts();
    debug_assert_eq!(facts.site, Site::Guard);
    match facts.family {
        Family::File => fs::native(site.workspace, call, site.config),
        Family::Process => process::run(site).await,
        Family::Skill => skills::run(call, site.skill.as_deref().ok_or(ToolError::Configuration)?),
        Family::Mcp => {
            mcp::execute(
                call,
                site.config,
                site.root,
                site.intent.limits.cache_bytes > 0,
            )
            .await
        }
        Family::Git => git::run(site).await,
    }
}

/// Whether a delivered output reports a failed operation rather than a result.
pub(super) fn failed(call: &ToolCall, output: &str) -> bool {
    let value = || serde_json::from_str::<serde_json::Value>(output);
    match call {
        ToolCall::Command { .. } => value().map_or(true, |value| value["exit_code"] != 0),
        ToolCall::McpCall { .. } => value().map_or(true, |value| value["is_error"] == true),
        _ if call.is_git_read() => value().map_or(true, |value| value.get("failed").is_some()),
        _ => false,
    }
}
