use super::{
    Admission, Feedback, controls::ActiveSubmission, emit_outcome, load_confirmed_outcome, run,
};
use crate::cli::chatgpt;
use arany::{
    AttachedTerminal, CompactionFailure, CompactionRecord, CompactionStatus, Composer,
    ComposerEdit, ImageAttachment, RunCancellation, RunOutcome, RunProgress, RunRequest,
    RunSteering, SessionView, ShutdownSignal, SteeringRefused, TerminalError, TerminalInput,
    TerminalNotice,
};
use std::time::Duration;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

pub(super) enum RunSubmission {
    /// The bool records a preview already printed; the optional reason says why the autonomous Goal stopped.
    Completed(Box<RunOutcome>, bool, Option<String>),
    InterruptedBeforeRun,
    Shutdown(String),
}

pub(super) enum CompactionOutcome {
    Notice(String),
    Unavailable(String),
    Interrupted(String),
    Shutdown(ShutdownSignal),
}

pub(super) fn objective_includes(
    admission: &Admission,
    objective: &str,
) -> Result<Vec<std::path::PathBuf>, String> {
    let mut paths = admission.include_paths.clone();
    for path in arany::mention_paths(objective).map_err(|error| error.to_string())? {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    Ok(paths)
}

pub(super) fn validate_objective_inputs(
    admission: &Admission,
    objective: &str,
) -> Result<(), String> {
    let paths = objective_includes(admission, objective)?;
    let root = arany::StateRoot::admit(&admission.state_dir)
        .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
    arany::validate_workspace_inputs(&root, &admission.workspace, &paths)
        .map_err(|error| error.to_string())
}

/// How an idle submission starts or continues its Goal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GoalStart {
    /// A plain message: one Run in a new Goal.
    Plain,
    /// `/goal OBJECTIVE`: a new Goal that the Engine keeps running until it stops.
    Autonomous,
    /// Continues the interrupted Goal of the Session with its pinned objective.
    Resume,
}

pub(super) fn goal_request(
    view: &SessionView,
    start: GoalStart,
) -> Result<arany::GoalRequest, String> {
    if start == GoalStart::Resume {
        return view
            .interrupted_goal_id()
            .map(|id| arany::GoalRequest::resume(id, true))
            .ok_or_else(|| "No interrupted Goal is available to resume".into());
    }
    if let Some(goal) = view.active_goal() {
        return Err(format!(
            "Goal {} is still active and cannot accept another Run",
            goal.id
        ));
    }
    Ok(match start {
        GoalStart::Autonomous => arany::GoalRequest::autonomous(
            arany::GoalBudget::default(),
            arany::GoalPhase::Implementation,
        ),
        _ => arany::GoalRequest::default(),
    })
}

/// Names what a draft typed during an admitted Run waits for.
fn draft_retained_notice(autonomous: bool) -> &'static str {
    if autonomous {
        "Draft retained; press Enter after this Goal to submit"
    } else {
        "Draft retained; press Enter after this Run to submit"
    }
}

/// How the attached Run answers a stop request: `/goal pause` waits for a step boundary; Ctrl+C cancels now,
/// and in raw-key presentations Escape does too (canonical linear input cannot send Escape).
#[derive(Clone, Copy, Eq, PartialEq)]
enum ActiveStop {
    Running,
    Pausing,
    Cancelling,
}

const STEERING_SENT: &str = "Steering note sent to the agent";

fn steering_refusal(refused: SteeringRefused, autonomous: bool) -> String {
    match refused {
        SteeringRefused::RunFull => format!(
            "No room for more steering notes this Run. {}",
            draft_retained_notice(autonomous)
        ),
        SteeringRefused::TooLarge => "Steering note exceeds 8 KiB; draft retained".into(),
        SteeringRefused::Empty => "Steering note is empty; draft retained".into(),
        SteeringRefused::Unavailable => "Steering note unavailable; draft retained".into(),
    }
}

/// Consumes a note the Engine recorded since the last check. The composer clears only while it
/// still holds the offered text; an edited draft stays as the user's retained text.
///
/// Clears the composer only when `delivered_notes()` advanced and the text is unchanged; any edit
/// withdraws an offer the Engine has not taken. `/goal pause` is intercepted by
/// `controls::handle_active_submission` as `ActiveSubmission::Pause`; Ctrl+C or an Escape with no
/// open menu moves the loop to `Cancelling`. `exec` never calls `set_steering`.
fn settle_recorded_note(
    delivered: u64,
    seen: &mut u64,
    offered: &mut Option<String>,
    composer: &mut Composer,
) -> bool {
    if delivered == *seen {
        return false;
    }
    *seen = delivered;
    let Some(text) = offered.take() else {
        return false;
    };
    if composer.text() == text && composer.images().is_empty() {
        composer.take();
    }
    true
}

/// Maps Escape during a Run: a pending clipboard read takes it first, otherwise it cancels only
/// when nothing is focused and no cancellation is already under way.
fn active_escape(
    input: TerminalInput,
    menu_open: bool,
    clipboard_loading: bool,
    stop: ActiveStop,
) -> TerminalInput {
    match input {
        TerminalInput::Escape
            if !clipboard_loading && !menu_open && stop != ActiveStop::Cancelling =>
        {
            TerminalInput::Interrupt
        }
        input => input,
    }
}

/// The one notice a Run's end shows about retained text. A note that did not return to the
/// composer is named as dropped, so no notice claims it was kept.
fn run_end_notice(sent: bool, note_returned: bool, restored: bool) -> Option<&'static str> {
    if !restored {
        return Some(if note_returned {
            "Retained draft, steering note and newer text exceed the draft limit; newer text kept"
        } else {
            "Retained draft and newer text exceed the draft limit; newer text kept"
        });
    }
    if sent {
        Some(STEERING_SENT)
    } else if note_returned {
        Some("Steering note withdrawn; draft retained")
    } else {
        None
    }
}

/// The undelivered note goes ahead of a held draft it was offered before; the result returns to the composer.
fn retained_linear_draft(note: Option<String>, held: Option<HeldDraft>) -> Option<HeldDraft> {
    match (note, held) {
        (Some(note), Some((text, images))) => Some((format!("{note}\n{text}"), images)),
        (Some(note), None) => Some((note, Vec::new())),
        (None, draft) => draft,
    }
}

/// A linear draft kept aside during a Run, where the composer is not drawn.
type HeldDraft = (String, Vec<ImageAttachment>);

/// Moves a refused linear draft out of the composer so later lines cannot be appended to it.
fn hold_draft(composer: &mut Composer) -> Option<HeldDraft> {
    if composer.is_empty() {
        return None;
    }
    Some((composer.take(), composer.take_images()))
}

/// Returns a held draft at Run end, ahead of newer text. If both do not fit, the newer draft stays and this returns false.
fn release_held_draft(composer: &mut Composer, (text, images): HeldDraft) -> bool {
    let typed = composer.take();
    let typed_images = composer.take_images();
    let joined = if typed.is_empty() {
        text
    } else {
        format!("{text}\n{typed}")
    };
    let fits = matches!(
        composer.insert_paste(&joined),
        Ok(ComposerEdit::Changed | ComposerEdit::Unchanged)
    ) && images
        .into_iter()
        .chain(typed_images.iter().cloned())
        .all(|image| composer.attach_image(image).is_ok());
    if !fits {
        composer.clear();
        composer
            .insert_paste(&typed)
            .expect("newer draft came from this composer");
        for image in typed_images {
            composer
                .attach_image(image)
                .expect("newer images came from this composer");
        }
    }
    fits
}

/// Checks an idle run before any Session or Goal state is created. Returns the notice to show.
pub(super) fn preflight_idle_run(
    admission: &Admission,
    view: &SessionView,
    start: GoalStart,
    draft: &str,
    has_images: bool,
    linear: bool,
) -> Result<(), String> {
    let Some(profile) = view.defaults.provider.as_deref() else {
        return Err("Use /setup to choose an account".into());
    };
    let Some(model) = view.defaults.model.as_deref() else {
        return Err(
            match view.defaults.account_id.filter(|_| profile == "chatgpt") {
                Some(id) => match chatgpt::selected_reauthorization_target(id) {
                    Ok(target) if target.plan_permission_missing => {
                        chatgpt::AuthorizationError::PermissionMissing.to_string()
                    }
                    Err(error) => error.to_string(),
                    _ => "Choose a model with /model before submitting".into(),
                },
                None => "Choose a model with /model before submitting".into(),
            },
        );
    };
    goal_request(view, start).map_err(|error| format!("Error: {error}"))?;
    if start == GoalStart::Resume && has_images {
        return Err("Error: /goal resume takes no image attachments; clear them first".into());
    }
    if has_images && profile.starts_with("custom:") {
        return Err("Error: Image input is not admitted for custom Providers; clear these attachments first".into());
    }
    run::validate_local_selection(run::Selection {
        workspace: &admission.workspace,
        profile,
        model,
        effort: view.defaults.effort,
        account_id: view.defaults.account_id,
    })
    .map_err(|error| format!("Error: {error}"))?;
    validate_objective_inputs(admission, draft).map_err(|error| {
        format!(
            "Error: {error}; {}",
            if linear {
                "check the file inputs and enter a new line"
            } else {
                "check the file inputs; draft unchanged"
            }
        )
    })
}

/// Creates the `RunSteering` handle once per submission and passes it through `run::run_selected`.
/// Race selected-Provider admission against the sticky cancellation waiter before constructing the
/// Engine. Mutate the caller-owned `SessionView` for progress; copying the resolved history per Run
/// scales with the whole transcript. Capture the last committed sequence once before submission so
/// active `/status`, `/provider` and `/model` report only facts after that boundary.
pub(super) async fn submit_objective(
    terminal: &mut AttachedTerminal,
    admission: &Admission,
    goal: GoalStart,
    view: &mut SessionView,
    composer: &mut Composer,
    objective: String,
) -> Result<RunSubmission, String> {
    let profile = view
        .defaults
        .provider
        .clone()
        .ok_or("select /provider and /model before submitting")?;
    let model = view
        .defaults
        .model
        .clone()
        .ok_or("select /model before submitting")?;
    let starting_sequence = view.last_sequence;
    let autonomous = match goal {
        GoalStart::Resume => view
            .interrupted_goal_id()
            .and_then(|id| view.goal(id))
            .is_some_and(|resumed| resumed.autonomous),
        GoalStart::Plain | GoalStart::Autonomous => goal == GoalStart::Autonomous,
    };
    let cancellation = RunCancellation::new();
    let mut progress = if terminal.is_linear() {
        RunProgress::new()
    } else {
        RunProgress::with_previews()
    };
    let mut previews = progress.previews();
    let mut waiting_progress = progress.clone();
    let (approvals, mut inbox) =
        arany::ToolApprovals::new(composer.approval_mode().unwrap_or_default());
    let mut pending: Option<arany::ToolApproval> = None;
    let mut approval_page = 0usize;
    let mut approval_allow = false;
    let mut approval_answer = Composer::default();
    let include_paths = objective_includes(admission, &objective)?;
    let execution_settings = super::controls::restore_settings(admission).await?;
    let mut goal = goal_request(view, goal)?;
    if goal.id.is_none() {
        goal.budget = Some(execution_settings.goal_budget);
    }
    goal.resource_settings = execution_settings.resources;
    let request = RunRequest {
        session_id: Some(view.id),
        title: None,
        objective,
        images: composer.take_images(),
        workspace: admission.workspace.clone(),
        include_paths,
        policy: view.defaults.policy,
        goal,
    };
    let steering = RunSteering::new();
    draw_active_snapshot(terminal, view, composer, false, ActiveStop::Running, None)
        .map_err(|error| error.to_string())?;
    let future = run::run_selected(
        &admission.state_dir,
        &admission.telemetry,
        run::Selection {
            workspace: &admission.workspace,
            profile: &profile,
            model: &model,
            effort: view.defaults.effort,
            account_id: view.defaults.account_id,
        },
        request,
        cancellation.clone(),
        progress.clone(),
        run::ToolAccess {
            configured: admission.tools,
            permissions: admission.workspace_permissions.as_ref(),
            approvals: &approvals,
        },
        steering.clone(),
    );
    tokio::pin!(future);
    let mut stop = ActiveStop::Running;
    let mut user_recorded = false;
    let mut offered: Option<String> = None;
    let mut held: Option<HeldDraft> = None;
    let mut seen_delivered = 0u64;
    let shutdown_reason = loop {
        let approval_more = match &pending {
            Some(approval) => terminal
                .draw_tool_approval(&approval.intent, approval_page, approval_allow)
                .map_err(|error| error.to_string())?,
            None => false,
        };
        tokio::select! {
            approval = inbox.next(), if pending.is_none() => {
                pending = approval;
                approval_page = 0;
                approval_allow = false;
                approval_answer.clear();
            }
            result = &mut future => {
                terminal.set_waiting_tool(None);
                terminal.set_preview(None);
                composer.set_file_candidates(None, Vec::new());
                if terminal.close_agents_at_run_end().is_err() {
                    return Ok(RunSubmission::Shutdown(
                        "terminal input cleanup failed; Run may have committed; check Session history".into(),
                    ));
                }
                // An undelivered note ends the Run as an ordinary retained draft.
                let had_offer = offered.is_some();
                let sent = settle_recorded_note(steering.delivered_notes(), &mut seen_delivered, &mut offered, composer);
                let had_held = held.is_some();
                // The undelivered note was offered before any held draft, so it joins ahead of that draft.
                // Linear offers left the composer empty; the note always returns here, never dropped.
                let note = steering.withdraw_note().filter(|_| terminal.is_linear());
                let withdrawn = note.is_some();
                let pending = retained_linear_draft(note, held.take());
                // A held draft returns before the undelivered note, so a newer draft wins over the note.
                let restored = pending.is_none_or(|draft| release_held_draft(composer, draft));
                if (result.is_err() || had_offer || sent || had_held)
                    && terminal.restore_draft_input(composer.text().len()).is_err()
                {
                    return Ok(RunSubmission::Shutdown("terminal input cleanup failed".into()));
                }
                if let Some(notice) = run_end_notice(sent, withdrawn, restored)
                    && draw_active_snapshot(terminal, view, composer, user_recorded, stop, Some(TerminalNotice::History(notice)))
                        .is_err()
                {
                    return Ok(RunSubmission::Shutdown("terminal rendering failed".into()));
                }
                return match result {
                    Ok(outcome) => Ok(RunSubmission::Completed(Box::new(outcome), user_recorded, None)),
                    Err(run::RunAttemptError::Stopped { outcome, reason }) => {
                        Ok(RunSubmission::Completed(outcome, user_recorded, Some(reason)))
                    }
                    Err(run::RunAttemptError::InterruptedBeforeRun) => Ok(RunSubmission::InterruptedBeforeRun),
                    Err(run::RunAttemptError::Failed(error)) => Err(error),
                };
            },
            preview = previews.changed() => {
                terminal.set_preview(preview);
                if draw_active_snapshot(terminal, view, composer, user_recorded, stop, None).is_err() {
                    break "terminal rendering failed".to_owned();
                }
            }
            update = progress.changed() => {
                if update.sequence <= view.last_sequence {
                    continue;
                }
                if !user_recorded {
                    if terminal.record_user_run(&update.run).is_err() {
                        break "terminal rendering failed".to_owned();
                    }
                    user_recorded = true;
                }
                if let Some(goal) = update.goal {
                    upsert_goal(&mut view.goals, goal);
                }
                if let Some(last) = view.runs.last_mut()
                    && last.id == update.run.id
                {
                    *last = update.run;
                } else {
                    view.runs.push(update.run);
                }
                view.last_sequence = update.sequence;
                let sent = settle_recorded_note(steering.delivered_notes(), &mut seen_delivered, &mut offered, composer);
                if sent && terminal.restore_draft_input(composer.text().len()).is_err() {
                    break "terminal input failed".to_owned();
                }
                let notice = sent.then_some(TerminalNotice::History(STEERING_SENT));
                if draw_active_snapshot(terminal, view, composer, user_recorded, stop, notice).is_err() {
                    break "terminal rendering failed".to_owned();
                }
            }
            waiting = waiting_progress.waiting_changed() => {
                terminal.set_waiting_tool(waiting);
                if draw_active_snapshot(terminal, view, composer, user_recorded, stop, None).is_err() {
                    break "terminal rendering failed".to_owned();
                }
            }
            input = terminal.next_input() => {
                let input = match input {
                    Ok(input) => input,
                    Err(_) => break "terminal input failed".to_owned(),
                };
                if pending.is_some() && !matches!(input, TerminalInput::Interrupt | TerminalInput::Shutdown(_)) {
                    match input {
                        TerminalInput::CycleApprovalMode => {
                            let _ = super::permissions::cycle(composer);
                            approvals.set_mode(composer.approval_mode().unwrap_or_default());
                        }
                        TerminalInput::Escape | TerminalInput::EndOfInput => {
                            pending.take().expect("pending action").decide(false);
                            terminal.restore_draft_input(composer.text().len()).map_err(|error| error.to_string())?;
                        }
                        TerminalInput::Tab if !approval_more => approval_allow = !approval_allow,
                        TerminalInput::Up if !approval_more => approval_allow = false,
                        TerminalInput::Down if !approval_more => approval_allow = true,
                        TerminalInput::Down | TerminalInput::PageDown if approval_more => approval_page += 1,
                        TerminalInput::Up | TerminalInput::PageUp => { approval_page = approval_page.saturating_sub(1); approval_allow = false; }
                        TerminalInput::Resize => { approval_page = 0; approval_allow = false; }
                        TerminalInput::Suspend => {
                            terminal.suspend_and_resume(composer.text().len()).map_err(|error| error.to_string())?;
                            approval_page = 0;
                            approval_allow = false;
                        }
                        TerminalInput::Character(character) if terminal.is_linear() => { approval_answer.apply(TerminalInput::Character(character)); }
                        TerminalInput::Backspace if terminal.is_linear() => { approval_answer.apply(TerminalInput::Backspace); }
                        TerminalInput::Submit if approval_more => approval_page += 1,
                        TerminalInput::Submit => {
                            let answer = approval_answer.text().trim().to_ascii_lowercase();
                            if terminal.is_linear() {
                                approval_allow = matches!(answer.as_str(), "allow" | "yes");
                                if !matches!(answer.as_str(), "" | "deny" | "no" | "allow" | "yes") { approval_answer.clear(); continue; }
                            }
                            pending.take().expect("pending action").decide(approval_allow);
                            terminal.restore_draft_input(composer.text().len()).map_err(|error| error.to_string())?;
                        }
                        _ => {}
                    }
                    continue;
                }
                if terminal.help_open() {
                    match input {
                        TerminalInput::Interrupt => terminal.close_help(),
                        TerminalInput::Suspend | TerminalInput::Shutdown(_) => {}
                        _ => {
                            terminal.help_input(input);
                            if draw_active_snapshot(terminal, view, composer, user_recorded, stop, None).is_err() {
                                break "terminal rendering failed".to_owned();
                            }
                            continue;
                        }
                    }
                }
                if terminal.agents_open()
                    && !matches!(input, TerminalInput::Interrupt | TerminalInput::Suspend | TerminalInput::Shutdown(_))
                {
                    if terminal.agent_input(view, input).is_err() {
                        break "terminal rendering failed".to_owned();
                    }
                    if !terminal.agents_open()
                        && draw_active_snapshot(terminal, view, composer, user_recorded, stop, None).is_err()
                    {
                        break "terminal rendering failed".to_owned();
                    }
                    continue;
                }
                if terminal.agents_open()
                    && matches!(input, TerminalInput::Interrupt | TerminalInput::Shutdown(_))
                    && terminal.close_agents().is_err()
                {
                    break "terminal input cleanup failed".to_owned();
                }
                match terminal.handle_transcript_input(input, view) {
                    Ok(true) => {
                        if draw_active_snapshot(terminal, view, composer, user_recorded, stop, None).is_err() {
                            break "terminal rendering failed".to_owned();
                        }
                        continue;
                    }
                    Ok(false) => {}
                    Err(_) => break "terminal rendering failed".to_owned(),
                }
                if terminal.handle_history_input(input, view) {
                    if draw_active_snapshot(terminal, view, composer, user_recorded, stop, None).is_err() {
                        break "terminal rendering failed".to_owned();
                    }
                    continue;
                }
                let input = active_escape(input, composer.has_open_menu(), terminal.clipboard_loading(), stop);
                let mut notice = match input {
                    TerminalInput::Escape if terminal.clipboard_loading() => {
                        terminal.cancel_clipboard_paste();
                        Some(Feedback::Transient("Clipboard read cancelled; draft retained".into()))
                    }
                    TerminalInput::CycleApprovalMode => {
                        let notice = super::permissions::cycle(composer);
                        approvals.set_mode(composer.approval_mode().unwrap_or_default());
                        Some(Feedback::History(notice))
                    }
                    TerminalInput::ClipboardPaste => Some(match terminal.request_clipboard_paste() {
                        Ok(()) => Feedback::Transient("Reading clipboard... draft retained".into()),
                        Err(error) => Feedback::History(format!("Error: {error}; draft unchanged")),
                    }),
                    TerminalInput::Paste => match terminal.paste_into(composer) {
                        Err(error) => Some(Feedback::History(format!("Error: {error}; draft unchanged"))),
                        Ok(ComposerEdit::Changed) => {
                            if draw_active_snapshot(terminal, view, composer, user_recorded, stop, None).is_err() {
                                break "terminal rendering failed".to_owned();
                            }
                            None
                        }
                        Ok(_) => None,
                    },
                    input @ (TerminalInput::Character(_)
                    | TerminalInput::Newline
                    | TerminalInput::Backspace
                    | TerminalInput::Delete
                    | TerminalInput::Left
                    | TerminalInput::Right
                    | TerminalInput::WordLeft
                    | TerminalInput::WordRight
                    | TerminalInput::BackspaceWord
                    | TerminalInput::Up
                    | TerminalInput::Down
                    | TerminalInput::Home
                    | TerminalInput::End
                    | TerminalInput::Tab
                    | TerminalInput::Escape) => match composer.apply(input) {
                        ComposerEdit::AtCapacity => Some(Feedback::History("Error: message is too long; shorten it; draft unchanged".into())),
                        ComposerEdit::Changed if !terminal.is_linear() => {
                            if draw_active_snapshot(terminal, view, composer, user_recorded, stop, None).is_err() {
                                break "terminal rendering failed".to_owned();
                            }
                            None
                        }
                        ComposerEdit::Unchanged => None,
                        ComposerEdit::Changed => None,
                    },
                    TerminalInput::LineRejected => {
                        Some(Feedback::History("Error: invalid or overlong terminal line; draft unchanged".into()))
                    }
                    TerminalInput::LineContinued if held.is_some() => {
                        composer.take();
                        if terminal.restore_draft_input(0).is_err() {
                            break "terminal input failed".to_owned();
                        }
                        Some(Feedback::Transient("A draft is already retained; line not kept".into()))
                    }
                    TerminalInput::LineContinued => {
                        Some(Feedback::History(format!("Draft: {} characters; Enter offers it as a steering note", composer.character_count())))
                    }
                    TerminalInput::QuickActions => {
                        Some(Feedback::History("Quick actions are unavailable during a Run; /agents opens details".into()))
                    }
                    TerminalInput::Submit if terminal.clipboard_loading() && !terminal.is_linear() => {
                        if terminal.restore_draft_input(composer.text().len()).is_err() {
                            break "terminal input failed".to_owned();
                        }
                        Some(Feedback::Transient("Clipboard loading; press Enter after paste to submit".into()))
                    }
                    TerminalInput::Submit if !terminal.is_linear()
                        && composer.apply(TerminalInput::Submit) == ComposerEdit::Changed => {
                        if draw_active_snapshot(terminal, view, composer, user_recorded, stop, None).is_err() {
                            break "terminal rendering failed".to_owned();
                        }
                        None
                    }
                    TerminalInput::Submit => {
                        match super::controls::handle_active_submission(composer, view, starting_sequence) {
                            ActiveSubmission::SteeringNote(text) => {
                                let feedback = if terminal.is_linear() && held.is_some() {
                                    composer.take();
                                    composer.take_images();
                                    Feedback::Transient("A draft is already retained; line not kept".into())
                                } else if !composer.images().is_empty() {
                                    Feedback::Transient("Steering notes are text only; draft retained".into())
                                } else if offered.is_some() && !terminal.is_linear() {
                                    // The composer still holds the waiting note; editing it withdraws the offer.
                                    Feedback::Transient("Steering note waits for the agent's next step; editing withdraws it".into())
                                } else {
                                    match steering.offer_note(text) {
                                        Ok(()) => {
                                            offered = Some(composer.text().to_owned());
                                            if terminal.is_linear() {
                                                composer.take();
                                                Feedback::Transient("Steering note waits for the agent's next step".into())
                                            } else {
                                                Feedback::Transient("Steering note waits for the agent's next step; editing withdraws it".into())
                                            }
                                        }
                                        Err(refused) => Feedback::Transient(steering_refusal(refused, autonomous)),
                                    }
                                };
                                // A linear draft kept by a refusal is invisible, so it waits aside until the Run ends.
                                if terminal.is_linear()
                                    && let Some(draft) = hold_draft(composer)
                                {
                                    held = Some(draft);
                                }
                                if terminal.restore_draft_input(composer.text().len()).is_err() {
                                    break "terminal input failed".to_owned();
                                }
                                Some(feedback)
                            }
                            ActiveSubmission::Pause => {
                                let notice = match stop {
                                    ActiveStop::Running => {
                                        steering.pause();
                                        stop = ActiveStop::Pausing;
                                        "Pausing at the next step; Ctrl+C cancels now"
                                    }
                                    ActiveStop::Pausing => "Already pausing at the next step",
                                    ActiveStop::Cancelling => "Run is already cancelling",
                                };
                                // Classic filters the history notice from the status row, so a redraw without it shows the pausing state there; linear prints notices as lines.
                                if draw_active_snapshot(terminal, view, composer, user_recorded, stop, Some(TerminalNotice::History(notice))).is_err()
                                    || (!terminal.is_linear()
                                        && draw_active_snapshot(terminal, view, composer, user_recorded, stop, None).is_err())
                                {
                                    break "terminal rendering failed".to_owned();
                                }
                                None
                            }
                            ActiveSubmission::Rejected(notice) => {
                                if terminal.is_linear() {
                                    composer.take();
                                }
                                Some(Feedback::History(format!("Error: {notice}")))
                            }
                            ActiveSubmission::Notice(notice) => Some(Feedback::History(notice)),
                            ActiveSubmission::ReadClipboard => Some(match terminal.request_clipboard_paste() {
                                Ok(()) => Feedback::Transient("Reading clipboard... draft retained".into()),
                                Err(error) => Feedback::History(format!("Error: {error}; draft unchanged")),
                            }),
                            ActiveSubmission::OpenAgents => {
                                if terminal.open_agents(view, true).is_err() {
                                    break "terminal rendering failed".to_owned();
                                }
                                None
                            }
                            ActiveSubmission::OpenHelp => {
                                if terminal.open_help().is_err() {
                                    break "terminal rendering failed".to_owned();
                                }
                                None
                            }
                            ActiveSubmission::OpenTranscript => {
                                if terminal.toggle_transcript(view).is_err()
                                    || draw_active_snapshot(terminal, view, composer, user_recorded, stop, None).is_err()
                                {
                                    break "terminal rendering failed".to_owned();
                                }
                                None
                            }
                        }
                    }
                    TerminalInput::Interrupt => {
                        if stop == ActiveStop::Cancelling {
                            break "forced shutdown after second Ctrl+C".to_owned();
                        }
                        stop = ActiveStop::Cancelling;
                        cancellation.cancel();
                        None
                    }
                    TerminalInput::Suspend => {
                        if terminal.suspend_and_resume(composer.text().len()).is_err() {
                            break "terminal resume failed".to_owned();
                        }
                        if draw_active_snapshot(terminal, view, composer, user_recorded, stop, None).is_err() {
                            break "terminal rendering failed".to_owned();
                        }
                        None
                    }
                    TerminalInput::Shutdown(signal) => {
                        break format!("terminated by {}", signal.name());
                    }
                    TerminalInput::Resize => {
                        if draw_active_snapshot(terminal, view, composer, user_recorded, stop, None).is_err() {
                            break "terminal rendering failed".to_owned();
                        }
                        None
                    }
                    TerminalInput::EndOfInput if !composer.is_empty() || held.is_some() => {
                        Some(Feedback::Transient(draft_retained_notice(autonomous).into()))
                    }
                    _ => None,
                };
                if !terminal.is_linear() && offered.as_deref().is_some_and(|text| composer.text() != text || !composer.images().is_empty()) {
                    offered = None;
                    notice = Some(if steering.withdraw_note().is_some() {
                        Feedback::Transient("Steering note withdrawn; draft retained".into())
                    } else {
                        // The Engine already took the note, so it is sent; the edited draft stays.
                        Feedback::History(STEERING_SENT.into())
                    });
                }
                if (notice.is_some() || matches!(input, TerminalInput::Interrupt))
                    && draw_active_snapshot(terminal, view, composer, user_recorded, stop, notice.as_ref().map(Feedback::notice)).is_err()
                {
                    break "terminal rendering failed".to_owned();
                }
            }
        }
    };
    cancellation.cancel();
    let linear = terminal.is_linear();
    let mode = (!linear).then_some(terminal.mode());
    let preview_printed = if linear {
        user_recorded
    } else {
        view.runs
            .last()
            .is_some_and(|run| terminal.user_run_recorded(run.id))
    };
    let restored = terminal.restore();
    let mut reason = shutdown_reason;
    match tokio::time::timeout(SHUTDOWN_GRACE, future.as_mut()).await {
        Ok(Ok(outcome)) if restored.is_ok() => {
            let emitted = match load_confirmed_outcome(&admission.state_dir, &outcome).await {
                Ok(confirmed) => emit_outcome(&confirmed, &outcome, preview_printed, !linear, mode),
                Err(error) => Err(error),
            };
            if emitted.is_err() {
                reason.push_str("; committed receipt unavailable; check Session history");
            }
        }
        Ok(Ok(_)) => reason.push_str("; Run committed; check Session history"),
        Ok(Err(run::RunAttemptError::InterruptedBeforeRun)) => {
            reason
                .push_str("; no Run started for this submission; account work may still complete");
        }
        Ok(Err(_)) => reason.push_str("; Run outcome unavailable; check Session history"),
        Err(_) => reason.push_str("; Run may be interrupted; check Session history"),
    }
    if restored.is_err() {
        reason.push_str("; terminal restoration failed");
    }
    Ok(RunSubmission::Shutdown(reason))
}

fn draw_active_snapshot(
    terminal: &mut AttachedTerminal,
    view: &SessionView,
    composer: &Composer,
    current_run_started: bool,
    stop: ActiveStop,
    notice: Option<TerminalNotice<'_>>,
) -> Result<(), TerminalError> {
    if !current_run_started {
        let status = match stop {
            ActiveStop::Running => None,
            ActiveStop::Pausing => Some("Pausing at the next step... Ctrl+C cancels now"),
            ActiveStop::Cancelling => Some("Stopping request..."),
        };
        terminal.draw_preparing_notice(
            view,
            composer,
            notice.or(status.map(TerminalNotice::Transient)),
        )
    } else if stop != ActiveStop::Running || terminal.is_linear() {
        let notice = notice.unwrap_or(TerminalNotice::Transient(match stop {
            ActiveStop::Cancelling => "Cancelling Run...",
            ActiveStop::Pausing => "Pausing at the next step... Ctrl+C cancels now",
            ActiveStop::Running => "Run in progress... Ctrl+C cancels",
        }));
        if terminal.is_linear() {
            terminal.draw_busy_notice(view, composer, Some(notice))
        } else {
            terminal.draw_notice(view, composer, Some(notice))
        }
    } else {
        terminal.draw_notice(view, composer, notice)
    }
}

pub(super) async fn compact_current(
    terminal: &mut AttachedTerminal,
    admission: &Admission,
    view: &SessionView,
    composer: &mut Composer,
    pinned: Option<(&arany::RunConfig, arany::RunId)>,
) -> Result<CompactionOutcome, String> {
    let (profile, model, effort, account_id) = if let Some((config, _)) = pinned {
        (
            config.provider.as_str(),
            config.model.as_str(),
            config.effort,
            config.saved_api_account_id.or_else(|| {
                config
                    .chatgpt_provenance
                    .as_ref()
                    .map(|provenance| provenance.account_id)
            }),
        )
    } else {
        let Some(profile) = view.defaults.provider.as_deref() else {
            return Ok(CompactionOutcome::Unavailable(
                "select /provider and /model before compacting".into(),
            ));
        };
        let Some(model) = view.defaults.model.as_deref() else {
            return Ok(CompactionOutcome::Unavailable(
                "select /model before compacting".into(),
            ));
        };
        (
            profile,
            model,
            view.defaults.effort,
            view.defaults.account_id,
        )
    };
    let selection = run::Selection {
        workspace: &admission.workspace,
        profile,
        model,
        effort,
        account_id,
    };
    let progress_text = if pinned.is_some() {
        "Automatically compacting Session... Ctrl+C interrupts"
    } else {
        "Compacting Session... Ctrl+C interrupts"
    };
    terminal
        .draw_busy_notice(
            view,
            composer,
            Some(TerminalNotice::Transient(progress_text)),
        )
        .map_err(|error| error.to_string())?;
    let future = run::compact_selected(
        &admission.state_dir,
        &admission.telemetry,
        selection,
        view.id,
        &admission.workspace,
        pinned.map(|(_, run_id)| run_id),
    );
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return Ok(match result {
                Ok(record) => CompactionOutcome::Notice(record.as_ref().map_or_else(
                    || "Automatic compaction skipped; Run boundary changed or already compacted".into(),
                    compaction_notice,
                )),
                Err(error) => CompactionOutcome::Unavailable(error),
            }),
            input = terminal.next_input() => {
                let input = input.map_err(|error| error.to_string())?;
                if terminal.handle_history_input(input, view) {
                    terminal.draw_busy(view, composer, None)
                        .map_err(|error| error.to_string())?;
                    continue;
                }
                let notice = match input {
                    TerminalInput::ClipboardPaste => Some(match terminal.request_clipboard_paste() {
                        Ok(()) => Feedback::Transient("Reading clipboard... draft retained".into()),
                        Err(error) => Feedback::History(format!("Error: {error}; draft unchanged")),
                    }),
                    TerminalInput::Paste => match terminal.paste_into(composer) {
                        Err(error) => Some(Feedback::History(format!("Error: {error}; draft unchanged"))),
                        Ok(ComposerEdit::Changed) => {
                            terminal.draw_busy(view, composer, None)
                                .map_err(|error| error.to_string())?;
                            None
                        }
                        Ok(_) => None,
                    },
                    TerminalInput::Interrupt => {
                        return Ok(CompactionOutcome::Interrupted(
                            "Compaction interrupted; check Session history".into(),
                        ));
                    }
                    TerminalInput::Resize => {
                        terminal.draw_busy(view, composer, None)
                            .map_err(|error| error.to_string())?;
                        None
                    }
                    TerminalInput::Suspend => {
                        terminal.suspend_and_resume(composer.text().len())
                            .map_err(|error| error.to_string())?;
                        terminal.draw_busy(view, composer, None)
                            .map_err(|error| error.to_string())?;
                        None
                    }
                    TerminalInput::Shutdown(signal) => {
                        return Ok(CompactionOutcome::Shutdown(signal));
                    }
                    TerminalInput::Submit | TerminalInput::EndOfInput => {
                        terminal.restore_draft_input(composer.text().len())
                            .map_err(|error| error.to_string())?;
                        Some(Feedback::Transient("Draft retained; press Enter after compaction to submit".into()))
                    }
                    TerminalInput::LineRejected => Some(Feedback::History("Error: invalid or overlong terminal line; draft unchanged".into())),
                    TerminalInput::LineContinued => Some(Feedback::History(format!(
                        "Draft: {} characters; Enter retains until compaction ends",
                        composer.character_count(),
                    ))),
                    TerminalInput::QuickActions => Some(Feedback::History("Quick actions are unavailable during compaction; draft retained".into())),
                    input => match composer.apply(input) {
                        ComposerEdit::AtCapacity => Some(Feedback::History("Error: message is too long; shorten it; draft unchanged".into())),
                        ComposerEdit::Changed if !terminal.is_linear() => Some(Feedback::Transient(progress_text.into())),
                        ComposerEdit::Changed | ComposerEdit::Unchanged => None,
                    },
                };
                if let Some(notice) = notice {
                    terminal.draw_busy_notice(view, composer, Some(notice.notice()))
                        .map_err(|error| error.to_string())?;
                }
            }
        }
    }
}

fn compaction_notice(record: &CompactionRecord) -> String {
    let usage = match (record.input_tokens, record.output_tokens) {
        (Some(input), Some(output)) => format!("{input} input / {output} output tokens"),
        _ => "usage unavailable".into(),
    };
    let bound = if record.output_token_bound == arany::OutputTokenBound::LocalAcceptanceOnly {
        "; remote usage not capped"
    } else {
        ""
    };
    match &record.status {
        CompactionStatus::Succeeded { .. } => format!("Compaction saved; {usage}{bound}"),
        CompactionStatus::Failed { reason } => {
            let reason = match reason {
                CompactionFailure::ProviderUnavailable => "Provider unavailable",
                CompactionFailure::ProviderRejected => "Provider rejected",
                CompactionFailure::InvalidOutcome => "invalid Provider outcome",
                CompactionFailure::TimedOut => "timed out",
                CompactionFailure::Cancelled => "cancelled",
            };
            format!("Error: Compaction failed: {reason}; {usage}{bound}")
        }
    }
}

/// Goals are appended in start order, so `goals.last()` is the newest Goal; a known Goal is replaced in place.
fn upsert_goal(goals: &mut Vec<arany::GoalView>, goal: arany::GoalView) {
    match goals.iter_mut().find(|known| known.id == goal.id) {
        Some(known) => *known = goal,
        None => goals.push(goal),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_update_appends_new_goal_and_replaces_known_goal() {
        let mut first = session_with_goal_view();
        let mut second = first.clone();
        second.id = arany::GoalId::new();
        let mut goals = Vec::new();

        upsert_goal(&mut goals, first.clone());
        upsert_goal(&mut goals, second.clone());
        assert_eq!(
            goals.iter().map(|goal| goal.id).collect::<Vec<_>>(),
            [first.id, second.id]
        );

        first.objective = "Replaced in place".into();
        upsert_goal(&mut goals, first.clone());
        assert_eq!(goals.len(), 2);
        assert_eq!(goals[0], first);
        assert_eq!(goals.last().map(|goal| goal.id), Some(second.id));
    }

    fn session_with_goal_view() -> arany::GoalView {
        let session_id = arany::SessionId::new();
        let events = [
            arany::Event::SessionStarted {
                title: "Goals".into(),
                workspace_identity: (1, 2),
            },
            arany::Event::GoalStarted {
                goal_id: arany::GoalId::new(),
                objective: "Ship the parser".into(),
                workspace_identity: (1, 2),
                budget: arany::GoalBudget::default(),
                phase: arany::GoalPhase::Implementation,
                autonomous: false,
                resource_settings: arany::ToolResourceSettings::default(),
            },
        ]
        .into_iter()
        .zip(1..)
        .map(|(event, sequence)| arany::EventEnvelope {
            sequence,
            session_id,
            run_id: None,
            agent_run_id: None,
            event,
            created_at_ms: 0,
        })
        .collect::<Vec<_>>();
        SessionView::replay(session_id, &events)
            .expect("valid Goal history")
            .expect("Session")
            .goals
            .remove(0)
    }

    #[test]
    fn compaction_notice_reports_committed_status_without_exposing_summary() {
        let summary = "\u{1b}[31muntrusted summary";
        let summary_bytes = summary.len() as u32;
        let mut record = CompactionRecord {
            covered_run_id: arany::RunId::new(),
            covered_sequence: 1,
            source_digest: [0; 32],
            provider: "openai".into(),
            model: "gpt-5.4".into(),
            prompt_version: 1,
            schema_version: 1,
            compiler_version: 3,
            source_bytes: 4,
            source_tokens_estimate: 1,
            input_tokens: Some(12),
            output_tokens: Some(3),
            response_id: None,
            wire_provenance: None,
            chatgpt_provenance: None,
            output_token_bound: arany::OutputTokenBound::ProviderEnforced,
            status: CompactionStatus::Succeeded {
                summary: summary.into(),
                content_digest: [0; 32],
                summary_bytes,
                summary_tokens_estimate: summary_bytes.div_ceil(4),
            },
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        };
        assert_eq!(
            compaction_notice(&record),
            "Compaction saved; 12 input / 3 output tokens"
        );
        record.input_tokens = None;
        record.output_tokens = None;
        record.status = CompactionStatus::Failed {
            reason: CompactionFailure::ProviderRejected,
        };
        assert_eq!(
            compaction_notice(&record),
            "Error: Compaction failed: Provider rejected; usage unavailable"
        );
        record.output_token_bound = arany::OutputTokenBound::LocalAcceptanceOnly;
        assert_eq!(
            compaction_notice(&record),
            "Error: Compaction failed: Provider rejected; usage unavailable; remote usage not capped"
        );
        for (reason, label) in [
            (
                CompactionFailure::ProviderUnavailable,
                "Provider unavailable",
            ),
            (CompactionFailure::ProviderRejected, "Provider rejected"),
            (
                CompactionFailure::InvalidOutcome,
                "invalid Provider outcome",
            ),
            (CompactionFailure::TimedOut, "timed out"),
        ] {
            record.status = CompactionStatus::Failed { reason };
            assert_eq!(
                compaction_notice(&record),
                format!(
                    "Error: Compaction failed: {label}; usage unavailable; remote usage not capped"
                )
            );
        }
    }
}

#[cfg(test)]
mod goal_request_tests {
    use super::*;
    use crate::cli::attached::{Admission, EntryMode};
    use arany::SessionDefaults;

    fn session_with_goal(autonomous: bool) -> SessionView {
        let session_id = arany::SessionId::new();
        let event = arany::Event::GoalStarted {
            goal_id: arany::GoalId::new(),
            objective: "Ship the parser".into(),
            workspace_identity: (1, 2),
            budget: arany::GoalBudget::default(),
            phase: arany::GoalPhase::Implementation,
            autonomous,
            resource_settings: arany::ToolResourceSettings::default(),
        };
        let started = arany::Event::SessionStarted {
            title: "Goals".into(),
            workspace_identity: (1, 2),
        };
        let events = [started, event]
            .into_iter()
            .zip(1..)
            .map(|(event, sequence)| arany::EventEnvelope {
                sequence,
                session_id,
                run_id: None,
                agent_run_id: None,
                event,
                created_at_ms: 0,
            })
            .collect::<Vec<_>>();
        SessionView::replay(session_id, &events)
            .expect("valid Goal history")
            .expect("Session")
    }

    #[test]
    fn goal_starts_map_to_plain_autonomous_and_resume_requests() {
        let mut view = session_with_goal(false);
        view.goals.clear();
        assert_eq!(
            goal_request(&view, GoalStart::Plain),
            Ok(arany::GoalRequest::default())
        );
        let autonomous = goal_request(&view, GoalStart::Autonomous).expect("autonomous start");
        assert!(autonomous.autonomous);
        assert_eq!(autonomous.id, None);
        assert_eq!(
            goal_request(&view, GoalStart::Resume),
            Err("No interrupted Goal is available to resume".into())
        );
    }

    #[test]
    fn paused_goal_admits_new_goals_and_resumes_only_through_goal_resume() {
        let mut view = session_with_goal(true);
        view.goals[0].status = arany::GoalStatus::Interrupted;
        let id = view.goals[0].id;
        assert_eq!(
            goal_request(&view, GoalStart::Resume),
            Ok(arany::GoalRequest::resume(id, true))
        );
        assert_eq!(
            goal_request(&view, GoalStart::Plain),
            Ok(arany::GoalRequest::default())
        );
        let autonomous = goal_request(&view, GoalStart::Autonomous).expect("autonomous start");
        assert!(autonomous.autonomous);
    }

    #[test]
    fn idle_resume_refusal_precedes_draft_images_and_session_state() {
        let temp = tempfile::tempdir().expect("temporary root");
        let mut view = session_with_goal(true);
        view.goals[0].status = arany::GoalStatus::Interrupted;
        view.defaults.provider = Some("openai".into());
        view.defaults.model = Some("gpt-5.4".into());
        let admission = Admission {
            telemetry: arany::Telemetry::disabled(),
            state_dir: temp.path().join("state"),
            explicit_state_dir: false,
            workspace: temp.path().join("workspace"),
            include_paths: Vec::new(),
            tools: false,
            workspace_permissions: None,
            defaults: SessionDefaults::default(),
            screen_reader: false,
            classic: false,
            no_color: false,
            setup_requested: false,
            entry: EntryMode::New,
            prompt: None,
            resume_goal: None,
        };
        view.goals.clear();
        let message = preflight_idle_run(&admission, &view, GoalStart::Resume, "", false, false)
            .expect_err("no interrupted Goal to resume");
        assert_eq!(message, "Error: No interrupted Goal is available to resume");
        assert!(
            !admission.state_dir.exists(),
            "refused submissions create no State"
        );
    }

    #[test]
    fn steering_refusals_name_the_retained_draft_without_echoing_text() {
        assert_eq!(
            steering_refusal(SteeringRefused::TooLarge, false),
            "Steering note exceeds 8 KiB; draft retained"
        );
        assert_eq!(
            steering_refusal(SteeringRefused::Unavailable, true),
            "Steering note unavailable; draft retained"
        );
        assert_eq!(
            steering_refusal(SteeringRefused::RunFull, true),
            "No room for more steering notes this Run. Draft retained; press Enter after this Goal to submit"
        );
    }

    #[test]
    fn recorded_note_clears_only_an_unchanged_composer() {
        let mut seen = 0;
        let mut composer = Composer::default();
        for character in "first note".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        let mut offered = Some(composer.text().to_owned());
        assert!(!settle_recorded_note(
            0,
            &mut seen,
            &mut offered,
            &mut composer
        ));
        assert!(settle_recorded_note(
            1,
            &mut seen,
            &mut offered,
            &mut composer
        ));
        assert!(
            composer.is_empty(),
            "an unchanged offer clears the composer"
        );
        assert!(offered.is_none());
        assert!(!settle_recorded_note(
            1,
            &mut seen,
            &mut offered,
            &mut composer
        ));

        // An edit after the Engine took the note keeps the user's text as a retained draft.
        for character in "second".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        let mut offered = Some("second".to_owned());
        composer.apply(TerminalInput::Character('!'));
        assert!(settle_recorded_note(
            2,
            &mut seen,
            &mut offered,
            &mut composer
        ));
        assert_eq!(composer.text(), "second!");
    }

    #[test]
    fn held_linear_draft_waits_aside_and_returns_before_later_text() {
        let mut composer = Composer::default();
        for character in "kept".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        let held = hold_draft(&mut composer).expect("refused draft is held");
        assert!(
            composer.is_empty(),
            "the invisible draft leaves the composer"
        );
        assert!(hold_draft(&mut composer).is_none());

        for character in "later".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        assert!(release_held_draft(&mut composer, held));
        assert_eq!(composer.text(), "kept\nlater");
    }

    #[test]
    fn overflowing_release_keeps_the_newer_draft() {
        let mut composer = Composer::default();
        for character in std::iter::repeat_n('a', 5000) {
            composer.apply(TerminalInput::Character(character));
        }
        let held = hold_draft(&mut composer).expect("refused draft is held");
        for character in std::iter::repeat_n('b', 4000) {
            composer.apply(TerminalInput::Character(character));
        }
        assert!(!release_held_draft(&mut composer, held));
        assert_eq!(composer.text(), "b".repeat(4000));
    }

    #[test]
    fn retained_draft_names_the_autonomous_goal_boundary() {
        assert_eq!(
            draft_retained_notice(true),
            "Draft retained; press Enter after this Goal to submit"
        );
        assert_eq!(
            draft_retained_notice(false),
            "Draft retained; press Enter after this Run to submit"
        );
    }
}

#[cfg(test)]
mod escape_tests {
    use super::*;

    #[test]
    fn run_end_notice_never_claims_a_dropped_note_was_kept() {
        assert_eq!(
            run_end_notice(false, true, false),
            Some(
                "Retained draft, steering note and newer text exceed the draft limit; newer text kept"
            )
        );
        assert_eq!(
            run_end_notice(false, false, false),
            Some("Retained draft and newer text exceed the draft limit; newer text kept")
        );
        assert_eq!(
            run_end_notice(false, true, true),
            Some("Steering note withdrawn; draft retained")
        );
        assert_eq!(run_end_notice(true, false, true), Some(STEERING_SENT));
        assert_eq!(run_end_notice(false, false, true), None);
    }

    #[test]
    fn undelivered_note_stays_ahead_of_a_held_draft() {
        assert_eq!(
            retained_linear_draft(Some("note".into()), Some(("held".into(), Vec::new()))),
            Some(("note\nheld".into(), Vec::new()))
        );
        assert_eq!(
            retained_linear_draft(Some("note".into()), None),
            Some(("note".into(), Vec::new()))
        );
        assert_eq!(
            retained_linear_draft(None, Some(("held".into(), Vec::new()))),
            Some(("held".into(), Vec::new()))
        );
        assert_eq!(retained_linear_draft(None, None), None);
    }

    #[test]
    fn escape_cancels_only_when_nothing_else_claims_it() {
        let idle = |input, menu, clipboard, stop| active_escape(input, menu, clipboard, stop);
        assert_eq!(
            idle(TerminalInput::Escape, false, false, ActiveStop::Running),
            TerminalInput::Interrupt
        );
        assert_eq!(
            idle(TerminalInput::Escape, true, false, ActiveStop::Running),
            TerminalInput::Escape,
            "an open menu closes first"
        );
        assert_eq!(
            idle(TerminalInput::Escape, false, true, ActiveStop::Running),
            TerminalInput::Escape,
            "a pending clipboard read is cancelled, not the Run"
        );
        assert_eq!(
            idle(TerminalInput::Escape, false, false, ActiveStop::Cancelling),
            TerminalInput::Escape,
            "a second Escape never forces shutdown"
        );
        assert_eq!(
            idle(TerminalInput::Interrupt, false, false, ActiveStop::Running),
            TerminalInput::Interrupt
        );
    }
}
