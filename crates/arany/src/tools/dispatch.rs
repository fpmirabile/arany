use super::catalog::Family;
use super::guard::GuardContext;
use super::types::{
    EffectIntent, EnforcementCause, LimitCause, NetworkGrant, ToolCall, ToolDisposition,
};
use super::{ToolError, fs, git, mcp, process};

pub(super) async fn run(site: &GuardContext<'_>) -> Result<String, ToolError> {
    let call = &site.intent.call;
    let facts = call.facts();
    match facts.family {
        Family::File if super::discovery::possible(call) => discover(site),
        Family::File => Err(ToolError::Configuration),
        Family::Process => process::run(site).await,
        Family::Skill => Err(ToolError::Configuration),
        Family::Mcp => match site.config.remote_server(call) {
            Some(_) if site.secret.as_deref() == Some(mcp::RENEWAL_UNCONFIRMED) => {
                Err(ToolError::Uncertain)
            }
            Some(server) if site.intent.limits.network == NetworkGrant::Proxy => {
                mcp::execute_remote(
                    call,
                    server,
                    site.secret.as_deref(),
                    site.intent.limits.result_bytes as usize,
                    &site.captured,
                )
                .await
            }
            Some(_) => Err(ToolError::Configuration),
            None => {
                mcp::execute(
                    call,
                    site.config,
                    site.root,
                    site.intent.limits.cache_bytes > 0,
                    site.intent.limits.result_bytes as usize,
                    &site.captured,
                )
                .await
            }
        },
        Family::Git => git::run(site).await,
        Family::Web => super::web::run(site, site.secret.as_deref()).await,
        Family::Preparation => {
            super::definitions::parse(site.definition.as_deref().ok_or(ToolError::Configuration)?)
        }
    }
}

pub(super) fn failed(call: &ToolCall, output: &str) -> bool {
    let value = || serde_json::from_str::<serde_json::Value>(output);
    match call {
        ToolCall::Command { .. } | ToolCall::CommandUnguarded { .. } => {
            value().map_or(true, |value| value["exit_code"] != 0)
        }
        ToolCall::McpList { .. } | ToolCall::McpCall { .. } => value().map_or(true, |value| {
            value["is_error"] == true || value.get("error").is_some()
        }),
        ToolCall::WebFetch { .. } | ToolCall::WebSearch { .. } => {
            value().map_or(true, |value| value.get("error").is_some())
        }
        _ if call.is_git() => value().map_or(true, |value| value.get("failed").is_some()),
        _ => false,
    }
}

pub(super) fn outcome(
    intent: &EffectIntent,
    result: Result<String, ToolError>,
) -> (
    ToolDisposition,
    String,
    Option<EnforcementCause>,
    Option<LimitCause>,
) {
    let limit_cause = match &result {
        Ok(output) if output.len() > intent.limits.result_bytes as usize => {
            Some(LimitCause::Resource)
        }
        Err(ToolError::Limit | ToolError::DiskSpace(_)) => Some(LimitCause::Resource),
        Err(ToolError::OutputCeiling) => Some(LimitCause::OutputCeiling),
        Err(ToolError::Timeout(_)) => Some(LimitCause::Timeout),
        _ => None,
    };
    let cause = match &result {
        Err(ToolError::FilesystemDenied) => Some(EnforcementCause::FilesystemDenied),
        _ => None,
    };
    let (disposition, output) = match result {
        Ok(output) if output.len() <= intent.limits.result_bytes as usize => {
            let disposition = if failed(&intent.call, &output) {
                ToolDisposition::Failed
            } else {
                ToolDisposition::Succeeded
            };
            (disposition, output)
        }
        Ok(_) | Err(ToolError::Limit) => (
            ToolDisposition::Limit,
            "Tool result or resource limit reached".into(),
        ),
        Err(ToolError::Timeout(output)) => (
            ToolDisposition::Limit,
            if output.is_empty() {
                "Tool runtime expired".into()
            } else {
                output
            },
        ),
        Err(ToolError::DiskSpace(output)) => (ToolDisposition::Limit, output),
        Err(ToolError::OutputCeiling) => (
            ToolDisposition::Limit,
            "output_ceiling: output passed 1 GiB per stream; stopped".into(),
        ),
        Err(ToolError::Pattern(cause)) => (
            ToolDisposition::Failed,
            format!("Search pattern refused: {cause}; nothing was read"),
        ),
        Err(ToolError::SpillUnavailable) => {
            (ToolDisposition::Failed, "no spill for this Run".into())
        }
        Err(ToolError::Conflict) => (
            ToolDisposition::Conflict,
            "File changed or edit did not match exactly once; read it again".into(),
        ),
        Err(ToolError::NotFound)
            if matches!(
                intent.call,
                ToolCall::Read { .. }
                    | ToolCall::List { .. }
                    | ToolCall::Search { .. }
                    | ToolCall::Glob { .. }
                    | ToolCall::Write { .. }
                    | ToolCall::Edit { .. }
                    | ToolCall::Delete { .. }
                    | ToolCall::Move { .. }
                    | ToolCall::MultiEdit { .. }
                    | ToolCall::Mkdir { .. }
            ) =>
        {
            (
                ToolDisposition::Failed,
                if matches!(intent.call, ToolCall::Mkdir { .. }) {
                    "Parent directory not found; create it with Mkdir first"
                } else {
                    "File or directory not found; List the parent directory to check the path"
                }
                .into(),
            )
        }
        Err(ToolError::Path | ToolError::FilesystemDenied | ToolError::NotFound) => (
            ToolDisposition::Denied,
            "File path is unavailable or outside the admitted regular-file boundary".into(),
        ),
        Err(ToolError::Cancelled) => (
            ToolDisposition::Cancelled,
            "File operation cancelled; nothing in the Workspace changed".into(),
        ),
        Err(ToolError::Uncertain) if intent.call.is_mcp() => (
            ToolDisposition::Uncertain,
            "The outcome of this remote MCP effect is unknown: its token renewal or the server's reply was lost or invalid. Check the server's state and authorization before another attempt"
                .into(),
        ),
        Err(ToolError::Uncertain) => (
            ToolDisposition::Uncertain,
            "Filesystem change may have occurred; inspect current state before another attempt"
                .into(),
        ),
        Err(_) => (
            ToolDisposition::Failed,
            "Tool operation failed; no automatic retry".into(),
        ),
    };
    (disposition, output, cause, limit_cause)
}

fn discover(site: &GuardContext<'_>) -> Result<String, ToolError> {
    let root = fs::open_directory(site.workspace)?;
    super::discovery::inspect(super::discovery::Request {
        root: &root,
        call: &site.intent.call,
        config: site.config,
        hidden: &|path| site.hidden.covers(path),
        deadline: std::time::Instant::now() + site.remaining_runtime(),
        result_bytes: site.intent.limits.result_bytes as usize,
        output: super::discovery::Output::Tool,
        cancellation: None,
    })
}
