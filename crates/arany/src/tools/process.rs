use super::ToolError;
use super::catalog::string;
use super::guard::{GuardContext, capture, capture_command, payload, spawn_before};
use super::types::{CommandResult, ToolCall, valid_name, valid_relative};
use serde_json::{Value, json};

const WORKER_CAPTURE_RESULT_BYTES: usize = 512;

#[derive(Clone, Copy)]
pub(super) enum Truncation {
    Allow,
    Refuse,
}

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
    let before = if site.intent.target == crate::session::worker::EffectTarget::Workspace {
        Some(super::fs::command_snapshot(site.workspace)?)
    } else {
        None
    };
    #[cfg(target_os = "macos")]
    let args = args
        .iter()
        .map(|arg| {
            if let Ok(relative) = std::path::Path::new(arg).strip_prefix("/workspace") {
                site.workspace.join(relative).to_string_lossy().into_owned()
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>();
    #[cfg(target_os = "macos")]
    let args = &args;
    let mut command = payload(
        definition,
        args,
        cwd,
        site.root,
        &site.config.runtime_grants,
        site.intent.limits.cache_bytes > 0,
    )?;
    command.current_dir(site.workspace.join(cwd));
    command.env("ARANY_TOOL_WORKSPACE", site.workspace);
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
    // Hook output is control data: a cut deny verdict must still fail closed.
    let captured = if site.hook_stdin.is_some() {
        match capture(
            child,
            (site.intent.limits.result_bytes as usize / 2).saturating_sub(128),
            deadline.saturating_duration_since(tokio::time::Instant::now()),
        )
        .await
        {
            Ok((status, stdout, stderr)) => {
                let mut out = super::output::Window::new(site.intent.limits.result_bytes as usize);
                let mut err = super::output::Window::new(site.intent.limits.result_bytes as usize);
                out.push(&stdout, super::output::DRAIN_BYTES)?;
                err.push(&stderr, super::output::DRAIN_BYTES)?;
                Ok((status, out, err))
            }
            Err(error) => Err(error),
        }
    } else {
        capture_command(
            child,
            site.intent.limits.result_bytes as usize,
            deadline.saturating_duration_since(tokio::time::Instant::now()),
        )
        .await
    };
    if let Some(writer) = writer {
        writer.abort();
    }
    let (status, out, err) = captured?;
    #[cfg(target_os = "linux")]
    let changeset = {
        site.stop_descendants().await?;
        match before {
            Some(before) => super::fs::command_changeset(site.workspace, &before)?,
            None => super::types::CommandChangeset::default(),
        }
    };
    #[cfg(not(target_os = "linux"))]
    let changeset = super::types::CommandChangeset::default();
    let budget = site.intent.limits.result_bytes as usize;
    let truncation = if site.hook_stdin.is_some() {
        Truncation::Refuse
    } else {
        Truncation::Allow
    };
    if matches!(
        site.intent.target,
        crate::session::worker::EffectTarget::WorkerView { .. }
    ) {
        return super::output::fit_streams(
            &out,
            &err,
            budget.saturating_sub(WORKER_CAPTURE_RESULT_BYTES),
            |stdout, stderr| {
                serde_json::to_string(&crate::session::worker::WorkerCommandResult {
                    exit_code: status.code(),
                    stdout: stdout.into(),
                    stderr: stderr.into(),
                    stdout_total: out.total(),
                    stderr_total: err.total(),
                    private_capture: crate::session::worker::PrivateCapture::None {},
                })
                .map_err(|_| ToolError::Operation)
            },
        );
    }
    #[cfg(target_os = "macos")]
    {
        if matches!(truncation, Truncation::Refuse) {
            fit_command(
                status.code(),
                &out,
                &err,
                changeset.clone(),
                budget,
                truncation,
            )?;
        }
        super::output::CommandWindows {
            stdout: out,
            stderr: err,
        }
        .send()?;
        fit_command(
            status.code(),
            &super::output::Window::default(),
            &super::output::Window::default(),
            changeset,
            budget,
            truncation,
        )
    }
    #[cfg(not(target_os = "macos"))]
    fit_command(status.code(), &out, &err, changeset, budget, truncation)
}

pub(super) fn fit_command(
    exit_code: Option<i32>,
    stdout: &super::output::Window,
    stderr: &super::output::Window,
    changeset: super::types::CommandChangeset,
    budget: usize,
    truncation: Truncation,
) -> Result<String, ToolError> {
    if !changeset.valid() {
        return Err(ToolError::Limit);
    }
    let render = |out: &str, err: &str| {
        serde_json::to_string(&CommandResult {
            exit_code,
            stdout: out.into(),
            stderr: err.into(),
            stdout_total: stdout.total(),
            stderr_total: stderr.total(),
            changeset: changeset.clone(),
        })
        .map_err(|_| ToolError::Operation)
    };
    match truncation {
        Truncation::Allow => super::output::fit_streams(stdout, stderr, budget, render),
        Truncation::Refuse => {
            if stdout.total().bytes > budget as u64 || stderr.total().bytes > budget as u64 {
                return Err(ToolError::Limit);
            }
            let output = render(&stdout.preview(budget), &stderr.preview(budget))?;
            if output.len() > budget {
                return Err(ToolError::Limit);
            }
            Ok(output)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::{CommandChangeset, FileChange, FileVersion};
    use super::*;

    #[test]
    fn command_output_fits_changeset_first_and_never_cuts_it() {
        let changeset = CommandChangeset {
            files: vec![FileChange {
                path: "src/made".into(),
                before: None,
                after: Some(FileVersion::from_bytes(&vec![b'x'; 1024], false)),
            }],
        };
        let mut out = super::super::output::Window::new(4096);
        out.push(&vec![b'y'; 8192], super::super::output::DRAIN_BYTES)
            .unwrap();
        let err = super::super::output::Window::default();
        let text = fit_command(
            Some(0),
            &out,
            &err,
            changeset.clone(),
            4096,
            Truncation::Allow,
        )
        .unwrap();
        let result: CommandResult = serde_json::from_str(&text).unwrap();
        assert_eq!(result.changeset, changeset);
        assert!(matches!(
            fit_command(
                Some(0),
                &out,
                &err,
                Default::default(),
                4096,
                Truncation::Refuse
            ),
            Err(ToolError::Limit)
        ));
        assert_eq!(result.stdout_total.bytes, 8192);
        assert!(result.stdout.contains(" bytes, 1 lines total]"));
        assert!(result.valid());
        assert!(matches!(
            fit_command(Some(0), &out, &err, changeset, 512, Truncation::Allow),
            Err(ToolError::Limit)
        ));
    }
}
