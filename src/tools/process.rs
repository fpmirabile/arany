use super::ToolError;
use super::catalog::string;
use super::guard::{GuardContext, capture, payload};
use super::types::{CommandResult, ToolCall, valid_name, valid_relative};
use serde_json::{Value, json};
use std::time::Duration;

pub(super) fn valid(call: &ToolCall) -> bool {
    matches!(call, ToolCall::Command { program, args, cwd }
        if valid_name(program)
            && valid_relative(cwd, true)
            && args.len() <= 64
            && args.iter().all(|arg| arg.len() <= 4096 && !arg.contains('\0')))
}

pub(super) fn schema() -> Option<Vec<(&'static str, Value)>> {
    Some(vec![
        ("program", string()),
        ("args", json!({"type":"array", "items": {"type":"string"}})),
        ("cwd", string()),
    ])
}

pub(super) async fn run(site: &GuardContext<'_>) -> Result<String, ToolError> {
    let ToolCall::Command { program, args, cwd } = &site.intent.call else {
        return Err(ToolError::Configuration);
    };
    let definition = site
        .config
        .commands
        .iter()
        .find(|value| &value.name == program)
        .ok_or(ToolError::Configuration)?;
    #[cfg(target_os = "linux")]
    let before = super::fs::command_snapshot(site.workspace)?;
    let mut command = payload(
        definition,
        args,
        cwd,
        site.root,
        &site.config.runtime_grants,
        site.intent.limits.cache_bytes > 0,
    )?;
    let (status, out, err) = capture(
        command.spawn().map_err(|_| ToolError::Operation)?,
        (site.intent.limits.result_bytes as usize / 2).saturating_sub(128),
        Duration::from_millis(u64::from(site.intent.limits.runtime_ms)),
    )
    .await?;
    #[cfg(target_os = "linux")]
    let changeset = {
        site.stop_descendants().await?;
        super::fs::command_changeset(site.workspace, &before)?
    };
    #[cfg(not(target_os = "linux"))]
    let changeset = super::types::CommandChangeset::default();
    let output = CommandResult {
        exit_code: status.code(),
        stdout: String::from_utf8(out).map_err(|_| ToolError::Operation)?,
        stderr: String::from_utf8(err).map_err(|_| ToolError::Operation)?,
        changeset,
    };
    if !output.valid() {
        return Err(ToolError::Limit);
    }
    serde_json::to_string(&output).map_err(|_| ToolError::Operation)
}
