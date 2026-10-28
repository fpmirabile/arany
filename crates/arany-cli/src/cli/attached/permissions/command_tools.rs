use super::*;
use arany::{
    CommandTool, CommandToolProbe, CommandTools, Engine, RewriteStage, RunCancellation, SessionId,
    command_tool_candidates,
};

const PAGE_SIZE: usize = 32;
const NEXT: u8 = 250;
const PREVIOUS: u8 = 251;

fn executable_name(path: &str) -> String {
    let name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path);
    crate::presentation::escape_terminal(name)
}

struct Editor<'a> {
    state: &'a StateRoot,
    admission: &'a super::super::Admission,
    session: &'a mut SessionId,
    view: &'a mut arany::SessionView,
}

pub(in crate::cli::attached) async fn manage_command_tools(
    terminal: &mut AttachedTerminal,
    engine: &Engine,
    admission: &super::super::Admission,
    session: &mut SessionId,
    view: &mut arany::SessionView,
) -> Result<String, SetupError> {
    let mut notice =
        "Select, probe and confirm a Rewrite hook. Changes apply from the next Run.".to_owned();
    loop {
        let state = match StateRoot::open_existing(&admission.state_dir) {
            Ok(state) => state,
            Err(_) => return Ok("Command tools unchanged: state is unavailable or unsafe".into()),
        };
        let record = match CommandTools::load(&state) {
            Ok(record) => record,
            Err(error) => return Ok(format!("Command tools unavailable: {error}")),
        };
        let mut editor = Editor {
            state: &state,
            admission,
            session: &mut *session,
            view: &mut *view,
        };
        match choose_tools(terminal, &editor, &record, &notice).await? {
            Some(ADD) => {
                let Some(tool) = select(terminal, &editor).await? else {
                    continue;
                };
                notice = saved(CommandTools::edit(&state, |record| record.select(tool)))?;
            }
            Some(value) if value >= ENTRY_BASE => {
                let index = usize::from(value - ENTRY_BASE);
                let Some(tool) = record.entries().get(index) else {
                    continue;
                };
                notice = edit_entry(terminal, engine, &mut editor, index, tool).await?;
            }
            _ => return Ok(notice),
        }
    }
}

async fn choose_tools(
    terminal: &mut AttachedTerminal,
    editor: &Editor<'_>,
    record: &CommandTools,
    notice: &str,
) -> Result<Option<u8>, SetupError> {
    let mut rows = vec![(ADD, "Select executable and role".to_owned())];
    rows.extend(record.entries().iter().enumerate().map(|(index, tool)| {
        let pinned = tool.pin_status(editor.state, &editor.admission.workspace);
        let pin = if pinned { "pinned" } else { "needs re-pin" };
        let status = if !pinned {
            "unavailable"
        } else if tool.enabled {
            "enabled"
        } else {
            "disabled"
        };
        (
            ENTRY_BASE + u8::try_from(index).expect("command tools are capped"),
            format!(
                "{} · Rewrite hook · {pin} · last probe {} · {status}",
                executable_name(&tool.selected_path),
                tool.last_probe.map_or("not probed", RewriteStage::label)
            ),
        )
    }));
    rows.push((BACK, "Back".into()));
    let labels = rows
        .iter()
        .map(|(key, label)| (*key, label.as_str()))
        .collect::<Vec<_>>();
    super::super::setup::choose_focused(terminal, 2, "Command tools", notice, &labels, None).await
}

async fn edit_entry(
    terminal: &mut AttachedTerminal,
    engine: &Engine,
    editor: &mut Editor<'_>,
    index: usize,
    tool: &CommandTool,
) -> Result<String, SetupError> {
    let action = super::pick(
        terminal,
        "Command tools",
        "Choose an action",
        &[
            (1, "Probe and preview".into()),
            (2, "Re-pin (disables until probed)".into()),
            (3, "Remove".into()),
        ],
    )
    .await?;
    let result = match action {
        Some(1) => return probe(terminal, engine, editor, index, tool).await,
        Some(2) => CommandTool::pin(
            editor.state,
            &editor.admission.workspace,
            &tool.selected_path,
            tool.args.clone(),
        )
        .and_then(|replacement| {
            CommandTools::edit(editor.state, |record| {
                record.replace(index, tool, replacement)
            })
        }),
        Some(3) => CommandTools::edit(editor.state, |record| {
            if record.entries().get(index) != Some(tool) {
                return Err(arany::ToolError::ChangedInput);
            }
            record.remove(index)
        }),
        _ => return Ok("Command tools unchanged".into()),
    };
    saved(result)
}

fn saved(result: Result<CommandTools, arany::ToolError>) -> Result<String, SetupError> {
    match result {
        Ok(_) => Ok("Saved for the next Run. Probe a selected tool before enabling it.".into()),
        Err(arany::ToolError::Uncertain) => Err(uncertain_save()),
        Err(error) => Ok(format!("Command tools unchanged: {error}")),
    }
}

fn uncertain_save() -> SetupError {
    SetupError::Uncertain("Command tools save is uncertain; reopen /permissions to verify".into())
}

fn save_failure(error: arany::ToolError) -> Result<String, SetupError> {
    match error {
        arany::ToolError::Uncertain => Err(uncertain_save()),
        error => Ok(format!(
            "Probe save failed: {error}; reopen /permissions to verify"
        )),
    }
}

async fn select(
    terminal: &mut AttachedTerminal,
    editor: &Editor<'_>,
) -> Result<Option<CommandTool>, SetupError> {
    let candidates = match command_tool_candidates(editor.state, &editor.admission.workspace) {
        Ok(candidates) => candidates,
        Err(error) => {
            super::super::setup::review_warning(
                terminal,
                "Command tools",
                &format!("Cannot list granted executables: {error}"),
                "Return to Command tools",
            )
            .await?;
            return Ok(None);
        }
    };
    let Some(index) = candidate(terminal, &candidates).await? else {
        return Ok(None);
    };
    if super::pick(
        terminal,
        "Command tool role",
        "Choose a role",
        &[(1, "Rewrite hook".into())],
    )
    .await?
    .is_none()
    {
        return Ok(None);
    }
    let mut args = Vec::new();
    while args.len() < CommandTool::ARGUMENT_COUNT_LIMIT {
        match super::pick(
            terminal,
            "Fixed arguments",
            "Arguments are passed literally, in order",
            &[
                (1, "Add an argument".into()),
                (2, "Finish selection".into()),
            ],
        )
        .await?
        {
            Some(1) => match super::super::setup::read_setup_value(
                terminal,
                "Fixed argument",
                "Argument",
                "",
                CommandTool::ARGUMENT_LIMIT,
            )
            .await?
            {
                Some(argument) => args.push(argument),
                None => return Ok(None),
            },
            Some(2) => break,
            _ => return Ok(None),
        }
    }
    match CommandTool::pin(
        editor.state,
        &editor.admission.workspace,
        &candidates[index],
        args,
    ) {
        Ok(tool) => Ok(Some(tool)),
        Err(error) => {
            super::super::setup::review_warning(
                terminal,
                "Command tools",
                &format!("Cannot select executable: {error}"),
                "Return to Command tools",
            )
            .await?;
            Ok(None)
        }
    }
}

async fn candidate(
    terminal: &mut AttachedTerminal,
    paths: &[String],
) -> Result<Option<usize>, SetupError> {
    let mut offset = 0;
    loop {
        let mut rows = paths
            .iter()
            .enumerate()
            .skip(offset)
            .take(PAGE_SIZE)
            .map(|(index, path)| {
                (
                    ENTRY_BASE + u8::try_from(index - offset).expect("pages are capped"),
                    format!(
                        "{} · {}",
                        executable_name(path),
                        crate::presentation::escape_terminal(path)
                    ),
                )
            })
            .collect::<Vec<_>>();
        if offset + PAGE_SIZE < paths.len() {
            rows.push((NEXT, "Next page".into()));
        }
        if offset > 0 {
            rows.push((PREVIOUS, "Previous page".into()));
        }
        rows.push((BACK, "Back".into()));
        let labels = rows
            .iter()
            .map(|(key, label)| (*key, label.as_str()))
            .collect::<Vec<_>>();
        match super::super::setup::choose_focused(
            terminal,
            3,
            "Granted executables",
            "Select an executable (metadata only)",
            &labels,
            None,
        )
        .await?
        {
            Some(NEXT) => offset += PAGE_SIZE,
            Some(PREVIOUS) => offset -= PAGE_SIZE,
            Some(value) if value >= ENTRY_BASE => {
                return Ok(Some(offset + usize::from(value - ENTRY_BASE)));
            }
            _ => return Ok(None),
        }
    }
}

async fn probe(
    terminal: &mut AttachedTerminal,
    engine: &Engine,
    editor: &mut Editor<'_>,
    index: usize,
    tool: &CommandTool,
) -> Result<String, SetupError> {
    let mut disabled = tool.clone();
    disabled.enabled = false;
    if let Err(error) = CommandTools::edit(editor.state, |record| {
        record.replace(index, tool, disabled.clone())
    }) {
        return save_failure(error);
    }
    super::super::materialize_session(editor.admission, editor.session, editor.view).await?;
    let result = match await_probe(terminal, engine, editor, &disabled).await? {
        Ok(probe) => probe,
        Err(error) => return Ok(format!("Probe did not enable the tool: {error}")),
    };
    if result.tool != disabled {
        return Ok(format!(
            "Earlier probe for {} is unknown; the selected tool was not run",
            crate::presentation::escape_terminal(&result.tool.executable)
        ));
    }
    let outcome = result.outcome.expect("a completed probe has an outcome");
    let (stage, preview) = if outcome.observation.disposition == arany::ToolDisposition::Uncertain {
        (
            RewriteStage::Uncertain,
            "unknown outcome; no retry was attempted".into(),
        )
    } else {
        preview(&outcome.result, outcome.observation.disposition)
    };
    let mut probed = disabled.clone();
    probed.last_probe = Some(stage);
    if let Err(error) = CommandTools::edit(editor.state, |record| {
        record.replace(index, &disabled, probed.clone())
    }) {
        return save_failure(error);
    }
    confirm_probe(terminal, editor, index, &probed, &preview).await
}

async fn confirm_probe(
    terminal: &mut AttachedTerminal,
    editor: &Editor<'_>,
    index: usize,
    probed: &CommandTool,
    preview: &str,
) -> Result<String, SetupError> {
    let stage = probed
        .last_probe
        .expect("a completed probe records its stage");
    let mut updated = probed.clone();
    if matches!(stage, RewriteStage::Rewritten | RewriteStage::Unchanged) {
        let warning = format!(
            "Selected: {}\nExecutable: {}\nRole: Rewrite hook\nFixed arguments: {}\nSample: {}\nPreview: {}",
            crate::presentation::escape_terminal(&probed.selected_path),
            crate::presentation::escape_terminal(&probed.executable),
            crate::presentation::escape_terminal(
                &serde_json::to_string(&probed.args).expect("typed arguments serialize")
            ),
            arany::PROBE_COMMAND,
            crate::presentation::escape_terminal(preview)
        );
        updated.enabled = super::super::setup::review_warning(
            terminal,
            "Command tool probe",
            &warning,
            "Accept enables this Rewrite hook for the next Run",
        )
        .await?;
    }
    let status = if updated.enabled {
        "Enabled for the next Run"
    } else {
        "Disabled"
    };
    match CommandTools::edit(editor.state, |record| {
        record.replace(index, probed, updated)
    }) {
        Ok(_) => Ok(format!(
            "Probe {}: {}. {status}",
            stage.label(),
            crate::presentation::escape_terminal(preview)
        )),
        Err(error) => save_failure(error),
    }
}

async fn await_probe(
    terminal: &mut AttachedTerminal,
    engine: &Engine,
    editor: &Editor<'_>,
    tool: &CommandTool,
) -> Result<Result<CommandToolProbe, arany::EngineError>, SetupError> {
    let cancellation = RunCancellation::new();
    let operation = engine.probe_command_tool(
        *editor.session,
        &editor.admission.workspace,
        tool,
        cancellation.clone(),
    );
    tokio::pin!(operation);
    loop {
        if let Err(error) = terminal.draw_setup(
            4,
            "Command tool probe",
            "Probing command tool; Esc cancels",
            0,
            None,
        ) {
            cancellation.cancel();
            let _ = operation.await;
            return Err(error.into());
        }
        tokio::select! {
            result = &mut operation => return Ok(result),
            input = terminal.next_input() => match input {
                Ok(
                    TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput,
                ) => {
                    cancellation.cancel();
                    return Ok(operation.await);
                }
                Ok(TerminalInput::Shutdown(signal)) => {
                    cancellation.cancel();
                    let _ = operation.await;
                    return Err(SetupError::Shutdown(signal));
                }
                Ok(TerminalInput::Suspend) => {
                    cancellation.cancel();
                    let result = operation.await;
                    terminal.suspend_and_resume(crate::terminal::MAX_DRAFT_BYTES)?;
                    return Ok(result);
                }
                Err(error) => {
                    cancellation.cancel();
                    let _ = operation.await;
                    return Err(error.into());
                }
                Ok(_) => {}
            },
        }
    }
}

fn preview(
    result: &arany::permissions::HookResult,
    disposition: arany::ToolDisposition,
) -> (RewriteStage, String) {
    let (stage, command) = result.command_rewrite(disposition);
    (stage, command.unwrap_or(stage.label()).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arany::permissions::{Decision, HookResult};

    #[test]
    fn probe_preview_only_enables_an_actual_successful_rewrite_or_no_op() {
        for (result, disposition, stage) in [
            (
                HookResult::Passed,
                arany::ToolDisposition::Succeeded,
                RewriteStage::Unchanged,
            ),
            (
                HookResult::Passed,
                arany::ToolDisposition::Limit,
                RewriteStage::Limit,
            ),
            (
                HookResult::Restricted {
                    decision: Decision::Allow,
                    reason: None,
                },
                arany::ToolDisposition::Succeeded,
                RewriteStage::NotCompleted,
            ),
        ] {
            assert_eq!(preview(&result, disposition).0, stage);
        }
    }
}
