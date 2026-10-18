use super::ToolError;
use super::catalog::string;
use super::guard::{GuardContext, capture, payload, spawn_before};
use super::types::{CommandResult, ToolCall, valid_name, valid_relative};
use serde_json::{Value, json};

pub(super) fn valid(call: &ToolCall) -> bool {
    match call {
        ToolCall::Command { program, args, cwd } => valid_command(program, args, cwd),
        ToolCall::CommandUnguarded {
            program,
            args,
            cwd,
            justification,
        } => {
            valid_command(program, args, cwd)
                && (1..=512).contains(&justification.len())
                && !justification
                    .chars()
                    .any(|character| character.is_control() && character != '\t')
        }
        _ => false,
    }
}

pub(super) fn valid_command(program: &str, args: &[String], cwd: &str) -> bool {
    valid_name(program)
        && valid_relative(cwd, true)
        && args.len() <= 64
        && args
            .iter()
            .all(|arg| arg.len() <= 4096 && !arg.contains('\0'))
}

pub(super) fn schema(name: &str) -> Option<Vec<(&'static str, Value)>> {
    let mut fields = vec![
        ("program", string()),
        ("args", json!({"type":"array", "items": {"type":"string"}})),
        ("cwd", string()),
    ];
    if name == "command_unguarded" {
        fields.push(("justification", string()));
    }
    Some(fields)
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
    command.stdin(if site.hook_stdin.is_some() {
        std::process::Stdio::piped()
    } else {
        std::process::Stdio::null()
    });
    let deadline = tokio::time::Instant::now() + site.remaining_runtime();
    let mut child = spawn_before(&mut command, deadline)?;
    let mut writer = None;
    if let (Some(input), Some(mut stdin)) = (site.hook_stdin, child.stdin.take()) {
        let input = input.to_owned();
        // Aborted below once capture ends, so a hook that never reads cannot outlive the deadline.
        writer = Some(tokio::spawn(async move {
            let _ = tokio::io::AsyncWriteExt::write_all(&mut stdin, input.as_bytes()).await;
        }));
    }
    let captured = capture(
        child,
        (site.intent.limits.result_bytes as usize / 2).saturating_sub(128),
        deadline.saturating_duration_since(tokio::time::Instant::now()),
    )
    .await;
    if let Some(writer) = writer {
        writer.abort();
    }
    let (status, out, err) = captured?;
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
