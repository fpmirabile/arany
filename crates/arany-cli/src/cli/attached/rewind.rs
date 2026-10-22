use super::{Admission, load_view, new_conversation};
use crate::presentation::{escape_terminal, rewind as display};
use crate::terminal::{AttachedTerminal, Composer, RewindPanelError};
use arany::rewind::{self, CheckpointTarget, RewindOutcome};
use arany::{
    ConversationRewind, ConversationRewindOutcome, Engine, RewindFile, RewindIssue, RewindPreview,
    RewindRequest, RunCancellation, SessionId, SessionView, StateRoot, Store, ToolGrant,
};

enum Failure {
    Refused(String),
    Terminal(RewindPanelError),
}
impl From<String> for Failure {
    fn from(value: String) -> Self {
        Self::Refused(value)
    }
}
impl From<RewindPanelError> for Failure {
    fn from(value: RewindPanelError) -> Self {
        Self::Terminal(value)
    }
}
impl From<crate::terminal::TerminalError> for Failure {
    fn from(value: crate::terminal::TerminalError) -> Self {
        Self::Terminal(value.into())
    }
}

fn grant(admission: &Admission) -> ToolGrant {
    if admission.tools {
        ToolGrant::Configured
    } else {
        admission
            .workspace_permissions
            .as_ref()
            .filter(|p| p.is_trusted())
            .map_or(ToolGrant::None, |p| ToolGrant::Trusted(p.clone()))
    }
}

pub(super) async fn open(
    engine: &Engine,
    terminal: &mut AttachedTerminal,
    admission: &Admission,
    session_id: &mut SessionId,
    view: &mut SessionView,
    composer: &mut Composer,
) -> Result<String, String> {
    engine
        .revoke_host_approvals()
        .map_err(|error| error.to_string())?;
    if view.rewinds.checkpoints.is_empty() {
        return Ok("No checkpoints; send a message first".into());
    }
    if !composer.is_empty() {
        return Ok("Submit or clear this draft before rewinding".into());
    }
    let result = journey(engine, terminal, admission, view).await;
    match result {
        Ok((notice, conversation)) => {
            if let Some(conversation) = conversation {
                let mut next = match conversation.session_id {
                    Some(id) => match load_view(&admission.state_dir, id).await {
                        Ok(view) => view,
                        Err(error) => {
                            return Ok(format!(
                                "{notice}; conversation fork saved but could not be opened: {error}; use /resume"
                            ));
                        }
                    },
                    None => new_conversation(conversation.defaults),
                };
                let mut notice = notice;
                if let Some(prompt) = conversation.prompt {
                    if !matches!(
                        composer.restore_prompt(&prompt),
                        Ok(crate::terminal::ComposerEdit::Changed
                            | crate::terminal::ComposerEdit::Unchanged)
                    ) {
                        notice.push_str(
                            "; prompt could not fit safely in the draft and was not copied",
                        );
                    }
                } else {
                    notice.push_str("; prompt exceeds 8.0 KB and was not copied");
                }
                if conversation.images_omitted {
                    notice.push_str("; images were not copied; attach them again");
                }
                next.defaults = view.defaults.clone();
                *session_id = next.id;
                *view = next;
                terminal
                    .show_rewind_draft(composer)
                    .map_err(|e| e.to_string())?;
                Ok(notice)
            } else {
                if let Ok(reloaded) = load_view(&admission.state_dir, *session_id).await {
                    *view = reloaded;
                }
                Ok(notice)
            }
        }
        Err(Failure::Refused(error)) => {
            let head = view.last_sequence;
            match load_view(&admission.state_dir, *session_id).await {
                Ok(reloaded) => {
                    let notice = reloaded
                        .rewinds
                        .operations
                        .last()
                        .filter(|op| op.started_sequence > head)
                        .map(|op| {
                            if let Some(finished) = &op.finished {
                                display::receipt(
                                    finished,
                                    op.files.len(),
                                    &display::checkpoint_label(&reloaded, &op.started.target),
                                )
                            } else {
                                "Rewind outcome unknown: inspect files before continuing".into()
                            }
                        });
                    *view = reloaded;
                    Ok(notice.unwrap_or_else(|| format!("Rewind refused: {error}")))
                }
                Err(_) => Ok(format!(
                    "Rewind could not be confirmed: {error}; inspect files and use /resume"
                )),
            }
        }
        Err(Failure::Terminal(error)) => Err(error.to_string()),
    }
}

async fn journey(
    engine: &Engine,
    terminal: &mut AttachedTerminal,
    admission: &Admission,
    view: &SessionView,
) -> Result<(String, Option<ConversationRewind>), Failure> {
    let store = Store::open_read_only(
        StateRoot::open_existing(&admission.state_dir).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let events = store.load_session(view.id).await;
    store.close().await.map_err(|e| e.to_string())?;
    let events = events.map_err(|e| e.to_string())?;
    let clock = rusqlite::Connection::open_in_memory().map_err(|e| e.to_string())?;
    let mut timestamp = clock
        .prepare("SELECT strftime('%Y-%m-%d %H:%M:%S', ?1 / 1000, 'unixepoch')")
        .map_err(|e| e.to_string())?;
    let targets = &view.rewinds.checkpoints;
    let rows: Vec<_> = targets
        .iter()
        .map(|target| {
            let label = display::checkpoint_label(view, target);
            let time = match events.iter().find(|e| e.sequence == target.sequence()) {
                Some(event) => {
                    let time = timestamp
                        .query_row([event.created_at_ms], |row| row.get::<_, Option<String>>(0))
                        .map_err(|e| e.to_string())?
                        .unwrap_or_else(|| "Unknown".into());
                    format!("{time} UTC")
                }
                None => "inherited".into(),
            };
            let count = rewind::plan(view, target).map(|p| p.files.len());
            let count = match count {
                Ok(0) => "no code changes since".into(),
                Ok(count) => format!("{count} {} changed since", display::file_noun(count)),
                Err(_) => "code plan exceeds bounds".into(),
            };
            let prompt = match target {
                CheckpointTarget::Run { run_id, .. } => view
                    .runs
                    .iter()
                    .find(|run| run.id == *run_id)
                    .map(|run| {
                        let excerpt: String = run.objective.chars().take(80).collect();
                        let goal = view
                            .goals
                            .iter()
                            .find_map(|goal| {
                                goal.run_ids
                                    .iter()
                                    .position(|id| id == run_id)
                                    .filter(|n| *n > 0)
                                    .map(|n| format!("Goal run {} · ", n + 1))
                            })
                            .unwrap_or_default();
                        format!(" · {goal}{}", escape_terminal(&excerpt))
                    })
                    .unwrap_or_default(),
                _ => String::new(),
            };
            Ok(format!("{label}{prompt} · {time} · {count}"))
        })
        .collect::<Result<_, String>>()?;
    drop(timestamp);
    drop(clock);
    let Some(index) = terminal
        .rewind_choice("Choose a checkpoint", &[], &rows, rows.len() - 1)
        .await?
    else {
        return Ok(("Rewind cancelled".into(), None));
    };
    let target = targets[index].clone();
    let label = display::checkpoint_label(view, &target);
    let plan =
        rewind::plan(view, &target).map_err(|_| "too large; restore with your VCS".to_owned())?;
    let conversation_run = match target {
        CheckpointTarget::Run { run_id, .. } => Some(run_id),
        _ => None,
    };
    let mut code_available = !plan.files.is_empty()
        && plan.blockers.is_empty()
        && !matches!(grant(admission), ToolGrant::None);
    let mut first_preview = None;
    let mut admission_reason = None;
    if code_available {
        match engine
            .rewind_preview(RewindRequest {
                session_id: view.id,
                workspace: admission.workspace.clone(),
                target: target.clone(),
                tools: grant(admission),
                kept: Vec::new(),
                overwrite: Vec::new(),
            })
            .await
        {
            Ok(preview) => {
                if !preview.files().is_empty()
                    && preview
                        .files()
                        .iter()
                        .all(|f| f.issues.contains(&RewindIssue::OutsideGrant))
                {
                    code_available = false;
                    admission_reason =
                        Some("current write grant excludes every listed file".to_owned());
                }
                first_preview = Some(preview);
            }
            Err(error) => {
                code_available = false;
                admission_reason = Some(error.to_string());
            }
        }
    }
    let mut actions = Vec::new();
    if !plan.files.is_empty() && conversation_run.is_some() {
        actions.push("Restore code and conversation".into());
    }
    if conversation_run.is_some() {
        actions.push("Restore conversation".into());
    }
    if !plan.files.is_empty() {
        actions.push("Restore code".into());
    }
    actions.push("Never mind".into());
    let mut explanation = vec![format!("Before {label}")];
    explanation.extend(actions.iter().cloned());
    explanation.extend(display::non_rewindable_lines(&plan));
    if !plan.blockers.is_empty() {
        explanation.push(format!("Code actions disabled: unknown or open publication at Events {:?}; inspect files. Restore conversation remains available.", plan.blockers));
    }
    if matches!(grant(admission), ToolGrant::None) {
        explanation.push("Code actions disabled: this folder is read only; restore conversation remains available.".into());
    }
    if let Some(reason) = admission_reason {
        explanation.push(format!(
            "Code actions disabled: {reason}; conversation restore remains available"
        ));
    }
    let Some(action) = terminal
        .rewind_choice("Restore choices", &explanation, &actions, actions.len() - 1)
        .await?
    else {
        return Ok(("Rewind cancelled".into(), None));
    };
    let action = actions[action].as_str();
    if action == "Never mind" {
        return Ok(("Rewind cancelled".into(), None));
    }
    if action == "Restore conversation" {
        let conversation = engine
            .rewind_conversation_at_head(
                view.id,
                admission.workspace.clone(),
                conversation_run.expect("conversation checkpoint"),
                (view.last_sequence, view.rewinds.prefix_digest()),
                i64::try_from(arany::now_ms())
                    .unwrap_or(i64::MAX)
                    .saturating_add(60_000),
            )
            .await
            .map_err(|e| e.to_string())?;
        return Ok((
            "Conversation restored; selected prompt is a draft".into(),
            Some(conversation),
        ));
    }
    if !code_available {
        return Ok(("Code restore blocked: inspect the unknown outcome or grant folder write access; conversation unchanged".into(), None));
    }
    let mut kept = Vec::new();
    let mut overwrite = Vec::new();
    let mut selection_basis = None::<RewindPreview>;
    loop {
        let preview = match first_preview.take() {
            Some(preview) => preview,
            None => engine
                .rewind_preview(RewindRequest {
                    session_id: view.id,
                    workspace: admission.workspace.clone(),
                    target: target.clone(),
                    tools: grant(admission),
                    kept: kept.clone(),
                    overwrite: overwrite.clone(),
                })
                .await
                .map_err(|e| e.to_string())?,
        };
        if selection_basis
            .take()
            .is_some_and(|basis| !preview.observations_match(&basis))
        {
            return Err("rewind preview changed or expired; preview again"
                .to_owned()
                .into());
        }
        let lines = display::preview_lines(&preview);
        if preview.checkpoint_storage_unavailable() {
            let choices = [
                "Retry".into(),
                "Reset checkpoint storage".into(),
                "Cancel".into(),
            ];
            match terminal
                .rewind_choice("Checkpoint storage unavailable", &lines, &choices, 0)
                .await?
            {
                Some(0) => continue,
                Some(1) => {
                    reset_storage(terminal, admission).await?;
                    continue;
                }
                _ => return Ok(("Rewind cancelled".into(), None)),
            }
        }
        let options = [
            "Cancel".into(),
            "Compare".into(),
            if preview.can_restore() {
                "Restore".into()
            } else {
                "Resolve listed files".into()
            },
        ];
        match terminal
            .rewind_choice("Rewind preview", &lines, &options, 0)
            .await?
        {
            None | Some(0) => return Ok(("Rewind cancelled".into(), None)),
            Some(1) => {
                compare(terminal, &preview).await?;
                first_preview = Some(preview);
            }
            Some(_) if !preview.can_restore() => {
                let conflicts: Vec<_> = preview
                    .files()
                    .iter()
                    .filter(|file| !file.kept && !file.issues.is_empty())
                    .collect();
                if conflicts.is_empty() {
                    return Ok(("Code restore blocked: unknown outcome or empty effective plan; conversation unchanged".into(), None));
                }
                let choices = [
                    "Cancel".into(),
                    display::KEEP.into(),
                    display::OVERWRITE.into(),
                ];
                let Some(choice) = terminal
                    .rewind_choice("Resolve listed files", &lines, &choices, 0)
                    .await?
                else {
                    return Ok(("Rewind cancelled".into(), None));
                };
                match choice {
                    1 => {
                        kept.extend(conflicts.iter().map(|f| f.path.clone()));
                        kept.sort();
                        kept.dedup();
                    }
                    2 if conflicts.iter().any(|f| overwritable(f)) => {
                        let warning = ["Overwrite changed files. Their current content must be saved before any write. This applies only to the listed drifted paths; unavailable, unsafe and outside-grant files stay kept; review and confirm the new preview.".into()];
                        if terminal
                            .rewind_choice(
                                "Confirm overwrite",
                                &warning,
                                &["Cancel".into(), "Overwrite after saving".into()],
                                0,
                            )
                            .await?
                            != Some(1)
                        {
                            return Ok(("Rewind cancelled".into(), None));
                        }
                        let (drifted, rest) = split_overwrite(&conflicts);
                        overwrite = drifted;
                        kept.extend(rest);
                        kept.sort();
                        kept.dedup();
                    }
                    2 => {
                        return Ok((
                            "Unavailable, unsafe and outside-grant files can only be kept".into(),
                            None,
                        ));
                    }
                    _ => return Ok(("Rewind cancelled".into(), None)),
                }
                selection_basis = Some(preview);
            }
            Some(_) => {
                let count = preview.plan().files.len();
                let result = publish(
                    engine,
                    terminal,
                    preview,
                    action == "Restore code and conversation",
                )
                .await?;
                let (finished, conversation) = (result.code, result.conversation);
                let mut notice = display::receipt(&finished, count, &label);
                return match conversation {
                    ConversationRewindOutcome::Restored(conversation) => {
                        Ok((notice, Some(*conversation)))
                    }
                    ConversationRewindOutcome::Failed(_) => {
                        notice.push_str("; Code restored; conversation unchanged; use /rewind → Restore conversation");
                        Ok((notice, None))
                    }
                    ConversationRewindOutcome::Unchanged => {
                        if action == "Restore code and conversation"
                            && finished.outcome == RewindOutcome::Restored
                        {
                            notice.push_str("; Code restored; conversation unchanged; use /rewind → Restore conversation");
                        }
                        Ok((notice, None))
                    }
                };
            }
        }
    }
}

async fn publish(
    engine: &Engine,
    terminal: &mut AttachedTerminal,
    preview: RewindPreview,
    both: bool,
) -> Result<arany::RewindBothResult, Failure> {
    let cancellation = RunCancellation::new();
    let operation = async {
        if both {
            engine.rewind_both(preview, cancellation.clone()).await
        } else {
            engine
                .rewind(preview, cancellation.clone())
                .await
                .map(|code| arany::RewindBothResult {
                    code,
                    conversation: ConversationRewindOutcome::Unchanged,
                })
        }
    };
    tokio::pin!(operation);
    loop {
        tokio::select! {
            result = &mut operation => return result.map_err(|e|Failure::Refused(e.to_string())),
            input = terminal.next_input() => {
                let failure = match input {
                    Ok(crate::terminal::TerminalInput::Shutdown(signal)) => Some(RewindPanelError::Shutdown(signal.name())),
                    Ok(crate::terminal::TerminalInput::Interrupt | crate::terminal::TerminalInput::Escape | crate::terminal::TerminalInput::EndOfInput) => None,
                    Err(error) => Some(error.into()),
                    _ => continue,
                };
                cancellation.cancel();
                let result = (&mut operation).await;
                return match failure {
                    Some(error) => Err(Failure::Terminal(error)),
                    None => result.map_err(|e|Failure::Refused(e.to_string())),
                };
            }
        }
    }
}

async fn reset_storage(
    terminal: &mut AttachedTerminal,
    admission: &Admission,
) -> Result<(), Failure> {
    let store =
        Store::open(StateRoot::open_existing(&admission.state_dir).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let reset = store.prepare_checkpoint_reset().await;
    store.close().await.map_err(|e| e.to_string())?;
    let reset = reset.map_err(|e| e.to_string())?;
    let lines = ["Reset deletes only private checkpoint storage and its SQLite sidecars. Earlier code versions will be unavailable. Conversations, current files, accounts and folder trust remain. Unsafe, busy or changed storage is never deleted.".into()];
    if terminal
        .rewind_choice(
            "Reset checkpoint storage?",
            &lines,
            &["Cancel".into(), "Reset checkpoint storage".into()],
            0,
        )
        .await?
        == Some(1)
    {
        tokio::task::spawn_blocking(move || reset.apply())
            .await
            .map_err(|_| "Checkpoint reset failed".to_owned())?
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn compare(terminal: &mut AttachedTerminal, preview: &RewindPreview) -> Result<(), Failure> {
    let mut choices: Vec<_> = preview
        .files()
        .iter()
        .map(|f| escape_terminal(&f.path))
        .collect();
    choices.push("Back".into());
    loop {
        let Some(index) = terminal
            .rewind_choice("Compare a file", &[], &choices, 0)
            .await?
        else {
            return Ok(());
        };
        let Some(file) = preview.files().get(index) else {
            return Ok(());
        };
        let pages = preview
            .comparisons()
            .iter()
            .find(|c| c.path == file.path)
            .map(display::compare_pages)
            .unwrap_or_else(|| {
                vec![vec![
                    "Comparison unavailable: path or saved content was not admitted.".into(),
                ]]
            });
        for (page, lines) in pages.iter().enumerate() {
            if terminal
                .rewind_choice(
                    &format!("Compare · page {}/{}", page + 1, pages.len()),
                    lines,
                    &["Back".into(), "Next".into()],
                    0,
                )
                .await?
                != Some(1)
            {
                break;
            }
        }
    }
}

fn overwritable(file: &RewindFile) -> bool {
    file.issues
        .iter()
        .all(|i| matches!(i, RewindIssue::Drifted | RewindIssue::HistoryDrift))
}

fn split_overwrite(conflicts: &[&RewindFile]) -> (Vec<String>, Vec<String>) {
    let (drifted, rest): (Vec<_>, Vec<_>) = conflicts.iter().partition(|f| overwritable(f));
    let paths = |files: Vec<&&RewindFile>| {
        let mut paths: Vec<_> = files.iter().map(|f| f.path.clone()).collect();
        paths.sort();
        paths
    };
    (paths(drifted), paths(rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, issues: &[RewindIssue]) -> RewindFile {
        RewindFile {
            path: path.into(),
            observed: None,
            issues: issues.to_vec(),
            kept: false,
        }
    }

    #[test]
    fn mixed_conflicts_overwrite_only_drifted_paths_and_keep_the_rest() {
        let a = file("a", &[RewindIssue::Drifted]);
        let b = file("b", &[RewindIssue::Unavailable]);
        let c = file("c", &[RewindIssue::HistoryDrift, RewindIssue::Unsafe]);
        let (overwrite, kept) = split_overwrite(&[&b, &a, &c]);
        assert_eq!(overwrite, ["a"]);
        assert_eq!(kept, ["b", "c"]);
    }
}
