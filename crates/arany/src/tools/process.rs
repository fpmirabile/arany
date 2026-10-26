use super::ToolError;
use super::catalog::string;
use super::guard::{GuardContext, capture, capture_command, payload, spawn_before};
use super::types::{CommandResult, ToolCall};
use crate::validate::{MAX_PATH_BYTES, valid_name, valid_relative};
use serde_json::{Value, json};

pub(super) const MAX_ARGUMENT_BYTES: usize = 4096;
pub(super) const MAX_COMMAND_ARGS: usize = 64;

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
            prefix_rule,
        } => {
            prefix_rule.as_ref().is_none_or(|prefix| {
                (super::permissions::MIN_HOST_PREFIX_TOKENS..=args.len() + 1)
                    .contains(&prefix.len())
                    && std::iter::once(program)
                        .chain(args)
                        .take(prefix.len())
                        .eq(prefix.iter())
            }) && (valid_name(program)
                || (std::path::Path::new(program).is_absolute()
                    && program.len() <= MAX_PATH_BYTES
                    && !program.chars().any(char::is_control)))
                && valid_command("host", args, cwd)
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
        && args.len() <= MAX_COMMAND_ARGS
        && args
            .iter()
            .all(|arg| arg.len() <= MAX_ARGUMENT_BYTES && !arg.contains('\0'))
}

pub(super) fn schema(name: &str) -> Option<Vec<(&'static str, Value)>> {
    let mut fields = vec![
        ("program", string()),
        ("args", json!({"type":"array", "items": {"type":"string"}})),
        ("cwd", string()),
    ];
    if name == "command_unguarded" {
        fields.push(("justification", string()));
        fields.push(("prefix_rule", json!({"type":["array","null"], "items":{"type":"string"}, "description":format!("Optional literal argv prefix including the executable; at least {} tokens. Suggest remembrance only for a narrow eligible command, never scripts, interpreters, wrappers, Git/gh or rm.", super::permissions::MIN_HOST_PREFIX_TOKENS)})));
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
        site.intent.limits.resource_class,
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
                Ok((Some(status), out, err))
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
    let timed_out = status.is_none();
    let exit_code = status.and_then(|status| status.code());
    #[cfg(target_os = "linux")]
    let (changeset, selection) = {
        site.stop_descendants().await?;
        let mut capture_deadline = None;
        if site.command_before.is_some() {
            capture_deadline = Some(
                tokio::task::spawn_blocking(|| {
                    use std::io::{Read, Write};
                    let mut stdout = std::io::stdout().lock();
                    stdout
                        .write_all(b"CAPTURE_READY\n")
                        .and_then(|()| stdout.flush())
                        .map_err(|_| ToolError::Operation)?;
                    let mut acknowledgement = [0; 8];
                    std::io::stdin()
                        .lock()
                        .read_exact(&mut acknowledgement)
                        .map_err(|_| ToolError::Operation)?;
                    if acknowledgement != *b"CAPTURE\n" {
                        return Err(ToolError::Configuration);
                    }
                    let mut frame = [0; 8];
                    std::io::stdin()
                        .lock()
                        .read_exact(&mut frame)
                        .map_err(|_| ToolError::Operation)?;
                    super::guard::selection_phase::decode_deadline(frame)
                })
                .await
                .map_err(|_| ToolError::Operation)??,
            );
        }
        if timed_out {
            (
                super::types::CommandChangeset::default(),
                site.command_before
                    .as_ref()
                    .map(|(_, selection)| selection.summary)
                    .unwrap_or_default(),
            )
        } else {
            match &site.command_before {
                Some((before, selection)) => super::fs::selected_command_changeset(
                    site.workspace,
                    before,
                    selection,
                    capture_deadline.ok_or(ToolError::Operation)?,
                )?,
                None => (
                    super::types::CommandChangeset::default(),
                    Default::default(),
                ),
            }
        }
    };
    #[cfg(not(target_os = "linux"))]
    let (changeset, selection) = (
        super::types::CommandChangeset::default(),
        Default::default(),
    );
    command_output(site, exit_code, timed_out, out, err, changeset, selection)
}

fn command_output(
    site: &GuardContext<'_>,
    exit_code: Option<i32>,
    timed_out: bool,
    out: super::output::Window,
    err: super::output::Window,
    changeset: super::types::CommandChangeset,
    selection: super::types::CommandSelectionSummary,
) -> Result<String, ToolError> {
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
        let output = super::output::fit_streams(
            &out,
            &err,
            budget.saturating_sub(WORKER_CAPTURE_RESULT_BYTES),
            |stdout, stderr| {
                serde_json::to_string(&crate::session::worker::WorkerCommandResult {
                    exit_code,
                    stdout: stdout.into(),
                    stderr: stderr.into(),
                    stdout_total: out.total(),
                    stderr_total: err.total(),
                    private_capture: crate::session::worker::PrivateCapture::None {},
                })
                .map_err(|_| ToolError::Operation)
            },
        )?;
        return if timed_out {
            Err(ToolError::Timeout(output))
        } else {
            Ok(output)
        };
    }
    if site.hook_stdin.is_none() {
        *site.captured.lock().map_err(|_| ToolError::Operation)? =
            Some(super::spill::streams(&out, &err));
    }
    #[cfg(target_os = "macos")]
    {
        let output = fit_selected_command(
            exit_code, &out, &err, changeset, budget, truncation, selection,
        )?;
        super::output::CommandWindows {
            stdout: out,
            stderr: err,
        }
        .send()?;
        if timed_out {
            Err(ToolError::Timeout(output))
        } else {
            Ok(output)
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let output = fit_selected_command(
            exit_code, &out, &err, changeset, budget, truncation, selection,
        )?;
        if timed_out {
            Err(ToolError::Timeout(output))
        } else {
            Ok(output)
        }
    }
}

pub(super) fn fit_command(
    exit_code: Option<i32>,
    stdout: &super::output::Window,
    stderr: &super::output::Window,
    changeset: super::types::CommandChangeset,
    budget: usize,
    truncation: Truncation,
) -> Result<String, ToolError> {
    fit_selected_command(
        exit_code,
        stdout,
        stderr,
        changeset,
        budget,
        truncation,
        Default::default(),
    )
}

pub(super) fn fit_selected_command(
    exit_code: Option<i32>,
    stdout: &super::output::Window,
    stderr: &super::output::Window,
    changeset: super::types::CommandChangeset,
    budget: usize,
    truncation: Truncation,
    selection: super::types::CommandSelectionSummary,
) -> Result<String, ToolError> {
    if !changeset.valid() || !selection.valid() {
        return Err(ToolError::Limit);
    }
    let render = |out: &str, err: &str| {
        serde_json::to_string(&CommandResult {
            selection: Some(selection),
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
            if !stdout.is_utf8() || !stderr.is_utf8() {
                return Err(ToolError::Operation);
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
        assert_eq!(result.selection, Some(Default::default()));
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
