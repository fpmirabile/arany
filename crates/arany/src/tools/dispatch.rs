use super::catalog::Family;
use super::guard::GuardContext;
use super::types::{EffectIntent, EnforcementCause, ToolCall, ToolDisposition};
use super::{ToolError, fs, git, mcp, process, skills};

pub(super) async fn run(site: &GuardContext<'_>) -> Result<String, ToolError> {
    let call = &site.intent.call;
    let facts = call.facts();
    match facts.family {
        Family::File if site.handover.is_some() => fs::apply_worker_changeset(
            site.workspace,
            site.handover.as_ref().ok_or(ToolError::Configuration)?,
        ),
        Family::File => fs::native(site.workspace, call, site.config),
        Family::Process => process::run(site).await,
        Family::Skill => skills::run(call, site.skill.as_deref().ok_or(ToolError::Configuration)?),
        Family::Mcp => {
            mcp::execute(
                call,
                site.config,
                site.root,
                site.intent.limits.cache_bytes > 0,
                site.intent.limits.result_bytes as usize,
            )
            .await
        }
        Family::Git => git::run(site).await,
        Family::Preparation if matches!(call, ToolCall::PrepareWorker { .. }) => {
            Ok("Private candidate captured".into())
        }
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
        ToolCall::McpCall { .. } => value().map_or(true, |value| value["is_error"] == true),
        _ if call.is_git_read() => value().map_or(true, |value| value.get("failed").is_some()),
        _ => false,
    }
}

pub(super) fn outcome(
    intent: &EffectIntent,
    result: Result<String, ToolError>,
) -> (ToolDisposition, String, Option<EnforcementCause>) {
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
        Err(ToolError::OutputCeiling) => (
            ToolDisposition::Limit,
            "output_ceiling: output passed 1 GiB per stream; stopped".into(),
        ),
        Err(ToolError::Conflict) => (
            ToolDisposition::Conflict,
            "File changed or edit did not match exactly once; read it again".into(),
        ),
        Err(ToolError::Path | ToolError::FilesystemDenied) => (
            ToolDisposition::Denied,
            "File path is unavailable or outside the admitted regular-file boundary".into(),
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
    (disposition, output, cause)
}
