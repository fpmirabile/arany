use super::{
    Admission, Feedback, controls::ActiveSubmission, emit_outcome, load_confirmed_outcome, run,
};
use crate::accounts::chatgpt;
use crate::cli::provider::select::{Selection, validate_local_selection};
use crate::terminal::{
    AgentChatFollow, AgentChatInput, AttachedTerminal, Composer, ComposerEdit, ShutdownSignal,
    TerminalError, TerminalInput, TerminalNotice,
};
use arany::{
    CompactionFailure, CompactionRecord, CompactionStatus, ImageAttachment, RunCancellation,
    RunHints, RunOutcome, RunRequest, RunSteering, SessionView, SteeringRefused,
};
use std::time::Duration;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

pub(super) enum RunSubmission {
    Completed(Box<RunOutcome>, bool, Option<String>),
    InterruptedBeforeRun,
    Shutdown(String),
}

enum RunStep<'a> {
    Admitted(arany::RunHandle<'a>),
    Completed(Result<Box<RunOutcome>, run::RunAttemptError>),
}

pub(super) enum CompactionOutcome {
    Notice(String),
    Unavailable(String),
    Interrupted(String),
    Shutdown(ShutdownSignal),
}

fn linear_approval_choice(
    answer: &str,
    choices: &[crate::presentation::ToolApprovalChoice],
) -> Option<crate::presentation::ToolApprovalChoice> {
    use crate::presentation::ToolApprovalChoice::{No, Yes};
    match answer.trim().to_ascii_lowercase().as_str() {
        "" | "no" => Some(No),
        "yes" => Some(Yes),
        number => number
            .parse::<usize>()
            .ok()
            .and_then(|number| number.checked_sub(1))
            .and_then(|index| choices.get(index))
            .copied(),
    }
}

// A continuation submits no text, so its @-mentions come from the source Goal's objective.
pub(super) fn mention_source<'a>(
    view: &'a SessionView,
    start: GoalStart,
    text: &'a str,
) -> &'a str {
    if start == GoalStart::Continue {
        view.continuable_goal()
            .map_or(text, |source| source.objective.as_str())
    } else {
        text
    }
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
    if admission.roster.is_some() {
        return Ok(());
    }
    let paths = objective_includes(admission, objective)?;
    let root = arany::StateRoot::admit(&admission.state_dir)
        .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
    arany::validate_workspace_inputs(
        &root,
        &admission.workspace,
        std::env::current_dir().ok().as_deref(),
        &paths,
    )
    .map_err(|error| error.to_string())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GoalStart {
    Plain,
    Autonomous,
    Resume,
    Continue,
}

pub(super) fn goal_request(
    view: &SessionView,
    start: GoalStart,
    settings: super::controls::Preferences,
) -> Result<arany::GoalRequest, String> {
    if start == GoalStart::Resume {
        return view
            .interrupted_goal_id()
            .map(|id| arany::GoalRequest {
                resource_settings: settings.resources,
                ..arany::GoalRequest::resume(id, true)
            })
            .ok_or_else(|| "No interrupted Goal is available to resume".into());
    }
    if let Some(goal) = view.active_goal() {
        return Err(format!(
            "Goal {} is still active and cannot accept another Run",
            goal.id
        ));
    }
    if start == GoalStart::Continue {
        return view
            .continuable_goal()
            .map(|source| {
                arany::GoalRequest::continue_from(
                    source.id,
                    settings.goal_budget,
                    settings.resources,
                )
            })
            .ok_or_else(|| "No exhausted Goal is available to continue".into());
    }
    let mut request = match start {
        GoalStart::Autonomous => {
            arany::GoalRequest::autonomous(settings.goal_budget, arany::GoalPhase::Implementation)
        }
        _ => arany::GoalRequest::default(),
    };
    request.budget = Some(settings.goal_budget);
    request.resource_settings = settings.resources;
    Ok(request)
}

fn draft_retained_notice(autonomous: bool) -> &'static str {
    if autonomous {
        "Draft retained; press Enter after this Goal to submit"
    } else {
        "Draft retained; press Enter after this Run to submit"
    }
}

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
        SteeringRefused::TooLarge => "Steering note exceeds 8.0 KB; draft retained".into(),
        SteeringRefused::Empty => "Steering note is empty; draft retained".into(),
        SteeringRefused::Unavailable => "Steering note unavailable; draft retained".into(),
        SteeringRefused::ChildLimit => {
            "Only eight children accept steering notes at once; draft retained".into()
        }
    }
}

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

fn run_settled(view: &SessionView, starting_sequence: u64) -> bool {
    view.runs.last().is_some_and(|run| {
        run.accepted_sequence > starting_sequence
            && !matches!(
                run.status,
                arany::RunStatus::Pending | arany::RunStatus::Active
            )
    })
}

// Once the current Run's outcome is committed, /exit waits for the Run to return, as Ctrl+D does.
fn exits_after_settled_run(
    composer: &Composer,
    view: &SessionView,
    starting_sequence: u64,
) -> bool {
    matches!(
        composer.submission(),
        Ok(crate::terminal::Submission::Command {
            command: crate::terminal::InteractiveCommand::Exit,
            argument: None,
        })
    ) && run_settled(view, starting_sequence)
}

// An explicit /exit abandons what the settled Run would hand back (a held linear draft or an
// undelivered note), so the deferred Ctrl+D meets an empty composer and exits.
fn abandon_drafts_for_exit(
    composer: &mut Composer,
    held: &mut Option<HeldDraft>,
    steering: &RunSteering,
) {
    composer.take();
    composer.take_images();
    *held = None;
    let _ = steering.withdraw_note();
}

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

fn retained_linear_draft(note: Option<String>, held: Option<HeldDraft>) -> Option<HeldDraft> {
    match (note, held) {
        (Some(note), Some((text, images))) => Some((format!("{note}\n{text}"), images)),
        (Some(note), None) => Some((note, Vec::new())),
        (None, draft) => draft,
    }
}

type HeldDraft = (String, Vec<ImageAttachment>);

// Move a refused linear draft aside so later lines cannot be appended to it.
fn hold_draft(composer: &mut Composer) -> Option<HeldDraft> {
    if composer.is_empty() {
        return None;
    }
    Some((composer.take(), composer.take_images()))
}

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
    if let Some(name) = &admission.roster {
        let selected =
            crate::cli::provider::roster::load(&admission.state_dir, &admission.workspace, name)?;
        selected.matches_selection(Selection {
            workspace: &admission.workspace,
            profile,
            model,
            effort: view.defaults.effort,
            account_id: view.defaults.account_id,
        })?;
    }
    goal_request(view, start, Default::default()).map_err(|error| format!("Error: {error}"))?;
    if matches!(start, GoalStart::Resume | GoalStart::Continue) && has_images {
        return Err(
            "Error: /goal resume and /goal continue take no image attachments; clear them first"
                .into(),
        );
    }
    if has_images && profile.starts_with("custom:") {
        return Err("Error: Image input is not admitted for custom Providers; clear these attachments first".into());
    }
    validate_local_selection(Selection {
        workspace: &admission.workspace,
        profile,
        model,
        effort: view.defaults.effort,
        account_id: view.defaults.account_id,
    })
    .map_err(|error| format!("Error: {error}"))?;
    validate_objective_inputs(admission, mention_source(view, start, draft)).map_err(|error| {
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

#[expect(
    clippy::too_many_arguments,
    reason = "submission carries independent Run inputs"
)]
pub(super) async fn submit_objective(
    engine: &arany::Engine,
    terminal: &mut AttachedTerminal,
    admission: &Admission,
    goal: GoalStart,
    view: &mut SessionView,
    composer: &mut Composer,
    objective: String,
    skill: Option<String>,
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
    let roster = admission
        .roster
        .as_ref()
        .map(|name| {
            crate::cli::provider::roster::load(&admission.state_dir, &admission.workspace, name)
        })
        .transpose()?;
    let manifest = roster
        .as_ref()
        .map(|selected| selected.manifest())
        .transpose()?;
    let starting_sequence = view.last_sequence;
    let autonomous = match goal {
        GoalStart::Resume => view
            .interrupted_goal_id()
            .and_then(|id| view.goal(id))
            .is_some_and(|resumed| resumed.autonomous),
        GoalStart::Continue => view
            .continuable_goal()
            .is_some_and(|source| source.autonomous),
        GoalStart::Plain | GoalStart::Autonomous => goal == GoalStart::Autonomous,
    };
    let cancellation = RunCancellation::new();
    let hints = if terminal.is_linear() {
        RunHints::new()
    } else {
        RunHints::with_previews()
    };
    let mut previews = hints.previews();
    let mut waiting_hints = hints.clone();
    let (approvals, mut inbox) =
        arany::ToolApprovals::new(composer.approval_mode().unwrap_or_default());
    let mut pending: Option<arany::ToolApproval> = None;
    let mut approval_page = 0usize;
    let mut approval_choice = 0usize;
    let mut approval_answer = Composer::default();
    let mut approval_unarmed = false;
    let mut approval_review_notice = false;
    let include_paths = objective_includes(admission, mention_source(view, goal, &objective))?;
    let execution_settings = super::controls::settings_or_defaults(admission).await?;
    let goal = goal_request(view, goal, execution_settings)?;
    let request = RunRequest {
        skill,
        session_id: Some(view.id),
        title: None,
        objective,
        images: composer.take_images(),
        workspace: admission.workspace.clone(),
        launch_directory: std::env::current_dir().ok(),
        include_paths,
        policy: view.defaults.policy,
        goal,
    };
    let steering = RunSteering::new();
    draw_active_snapshot(terminal, view, composer, false, ActiveStop::Running, None)
        .map_err(|error| error.to_string())?;
    if let Some(manifest) = &manifest {
        terminal
            .draw_busy_notice(view, composer, Some(TerminalNotice::History(manifest)))
            .map_err(|error| error.to_string())?;
    }
    let future = run::submit_selected(
        engine,
        &admission.state_dir,
        Selection {
            workspace: &admission.workspace,
            profile: &profile,
            model: &model,
            effort: view.defaults.effort,
            account_id: view.defaults.account_id,
        },
        request,
        cancellation.clone(),
        hints.clone(),
        run::ToolAccess {
            configured: admission.tools,
            permissions: admission.workspace_permissions.as_ref(),
            approvals: &approvals,
        },
        steering.clone(),
        roster,
    );
    tokio::pin!(future);
    let mut handle: Option<arany::RunHandle<'_>> = None;
    let mut stop = ActiveStop::Running;
    let mut user_recorded = false;
    let mut offered: Option<String> = None;
    let mut held: Option<HeldDraft> = None;
    let mut seen_delivered = 0u64;
    let mut shown_retries = (None, 0usize);
    let mut exit_after_run = false;
    let shutdown_reason = loop {
        let approval_more = match &pending {
            Some(approval) => match terminal.draw_tool_approval(
                approval,
                view,
                approval_page,
                (!approval_unarmed).then_some(approval_choice),
                approval_review_notice,
            ) {
                Ok(more) => more,
                Err(_) => break "terminal rendering failed".to_owned(),
            },
            None => false,
        };
        // Only bytes the reader reads after this barrier reach the armed redraw.
        if std::mem::take(&mut approval_unarmed) {
            if terminal.discard_approval_typeahead().await.is_err() {
                break "terminal input admission failed".to_owned();
            }
            continue;
        }
        let (result, mut events) = handle
            .as_mut()
            .map(|handle| (&mut handle.result, &mut handle.events))
            .unzip();
        tokio::select! {
            approval = inbox.next(), if pending.is_none() => {
                pending = approval;
                if pending.is_some() { composer.disarm_continuation(); }
                approval_page = 0;
                approval_choice = pending.as_ref().filter(|approval| approval.unguarded.is_some()).map_or(0, |approval| crate::presentation::tool_approval_choices(approval).len() - 1);
                approval_unarmed = pending.is_some();
                approval_review_notice = false;
                approval_answer.clear();
            }
            step = async {
                match result {
                    Some(result) => RunStep::Completed(run::finish_run(result.await, !matches!(stop, ActiveStop::Running)).map(Box::new)),
                    None => match future.as_mut().await {
                        Ok(handle) => RunStep::Admitted(handle),
                        Err(error) => RunStep::Completed(Err(error)),
                    },
                }
            } => {
                let result = match step {
                    RunStep::Admitted(admitted) => {
                        handle = Some(admitted);
                        continue;
                    }
                    RunStep::Completed(result) => result,
                };
                let caught_up = !terminal.child_notes_pending()
                    || catch_up(&admission.state_dir, view).await.is_ok();
                terminal.settle_child_notes(view, &steering, true);
                let child_follow = if !caught_up { false } else { match terminal.follow_agent_chat(view).await {
                    Ok(follow) => follow != AgentChatFollow::Unchanged,
                    Err(_) => break "terminal input cleanup failed".into(),
                }};
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
                // The undelivered note predates the held draft, so restore it ahead of that draft.
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
                let notice = run_end_notice(sent, withdrawn, restored);
                if (child_follow || notice.is_some())
                    && draw_active_snapshot(terminal, view, composer, user_recorded, stop, notice.map(TerminalNotice::History)).is_err()
                {
                    return Ok(RunSubmission::Shutdown("terminal rendering failed".into()));
                }
                return match result {
                    Ok(outcome) => Ok(RunSubmission::Completed(outcome, user_recorded, None)),
                    Err(run::RunAttemptError::Stopped { outcome, reason }) => {
                        Ok(RunSubmission::Completed(outcome, user_recorded, Some(reason)))
                    }
                    Err(run::RunAttemptError::InterruptedBeforeRun) => Ok(RunSubmission::InterruptedBeforeRun),
                    Err(run::RunAttemptError::Failed(error)) => Err(error.to_string()),
                };
            },
            preview = previews.changed() => {
                terminal.set_preview(preview);
                // A child view shows neither the primary's preview nor its waiting hint.
                if pending.is_none() && !terminal.agent_chat_open() && draw_active_snapshot(terminal, view, composer, user_recorded, stop, None).is_err() {
                    break "terminal rendering failed".to_owned();
                }
            }
            event = async {
                match events.as_deref_mut() {
                    Some(events) => events.next().await,
                    None => std::future::pending().await,
                }
            } => {
                let ready = async || match events.as_deref_mut() {
                    Some(events) => tokio::time::timeout(Duration::ZERO, events.next()).await.ok(),
                    None => None,
                };
                if let Err(reason) = apply_burst(&admission.state_dir, view, event, ready).await {
                    break reason;
                }
                if !user_recorded && let Some(run) = view.runs.last().filter(|run| run.accepted_sequence > starting_sequence) {
                    if terminal.record_user_run(run).is_err() {
                        break "terminal rendering failed".to_owned();
                    }
                    user_recorded = true;
                }
                let sent = settle_recorded_note(steering.delivered_notes(), &mut seen_delivered, &mut offered, composer);
                if sent && terminal.restore_draft_input(composer.text().len()).is_err() {
                    break "terminal input failed".to_owned();
                }
                let retry = view
                    .runs
                    .last()
                    .filter(|run| run.accepted_sequence > starting_sequence)
                    .and_then(|run| {
                        let shown = if shown_retries.0 == Some(run.id) { shown_retries.1 } else { 0 };
                        shown_retries = (Some(run.id), run.provider_retries.len());
                        run.provider_retries.get(shown..)?.last()
                    })
                    .map(|(_, retry)| crate::presentation::escape_terminal(&retry.notice()));
                let notice = if sent {
                    Some(TerminalNotice::History(STEERING_SENT))
                } else {
                    retry.as_deref().map(TerminalNotice::Transient)
                };
                let child_changed = terminal.settle_child_notes(view, &steering, stop != ActiveStop::Running);
                if child_changed && terminal.restore_selected_draft_input(composer.text().len()).is_err() {
                    break "terminal input failed".to_owned();
                }
                let follow = match terminal.follow_agent_chat(view).await {
                    Ok(follow) => follow,
                    Err(_) => break "terminal rendering failed".to_owned(),
                };
                let notice = match follow {
                    AgentChatFollow::Closed(text) => Some(TerminalNotice::History(text)),
                    AgentChatFollow::Unchanged | AgentChatFollow::Redraw => notice,
                };
                if pending.is_none()
                    && (child_changed || follow != AgentChatFollow::Unchanged || notice.is_some())
                    && draw_active_snapshot(terminal, view, composer, user_recorded, stop, notice).is_err()
                {
                    break "terminal rendering failed".to_owned();
                }
            }
            waiting = waiting_hints.waiting_changed() => {
                terminal.set_waiting_tool(waiting);
                if pending.is_none() && !terminal.agent_chat_open() && draw_active_snapshot(terminal, view, composer, user_recorded, stop, None).is_err() {
                    break "terminal rendering failed".to_owned();
                }
            }
            input = async {
                if pending.is_some() { terminal.next_input().await } else { terminal.next_chat_input().await }
            }, if !exit_after_run => {
                let input = match input {
                    Ok(input) => input,
                    Err(_) => break "terminal input failed".to_owned(),
                };
                composer.observe_input(input);
                if terminal.agent_chat_open() || pending.is_some() || terminal.help_open() { composer.disarm_continuation(); }
                if pending.is_some() && !matches!(input, TerminalInput::Interrupt | TerminalInput::Shutdown(_)) {
                    let choices = crate::presentation::tool_approval_choices(pending.as_ref().expect("pending action"));
                    approval_choice = approval_choice.min(choices.len() - 1);
                    let mut selected = None;
                    match input {
                        TerminalInput::CycleApprovalMode => {
                            let _ = super::permissions::cycle(composer);
                            approvals.set_mode(composer.approval_mode().unwrap_or_default());
                        }
                        TerminalInput::Escape | TerminalInput::EndOfInput => {
                            pending.take().expect("pending action").decide(arany::ApprovalReply::Deny);
                            terminal.restore_draft_input(composer.text().len()).map_err(|error| error.to_string())?;
                        }
                        TerminalInput::Tab if !approval_more => approval_choice = (approval_choice + 1) % choices.len(),
                        TerminalInput::Up if !approval_more && approval_choice == 0 && approval_page > 0 => approval_page -= 1,
                        TerminalInput::Up if !approval_more => approval_choice = approval_choice.saturating_sub(1),
                        TerminalInput::Down if !approval_more => approval_choice = (approval_choice + 1).min(choices.len() - 1),
                        TerminalInput::Down | TerminalInput::PageDown if approval_more => { approval_page += 1; approval_review_notice = false; },
                        TerminalInput::Up | TerminalInput::PageUp => { approval_page = approval_page.saturating_sub(1); }
                        TerminalInput::Resize => { approval_page = 0; approval_choice = if pending.as_ref().is_some_and(|approval| approval.unguarded.is_some()) { choices.len() - 1 } else { 0 }; approval_review_notice = false; }
                        TerminalInput::Suspend => {
                            terminal.suspend_and_resume(composer.text().len()).map_err(|error| error.to_string())?;
                            approval_page = 0;
                            approval_choice = if pending.as_ref().is_some_and(|approval| approval.unguarded.is_some()) { choices.len() - 1 } else { 0 };
                        }
                        TerminalInput::Character(character) if terminal.is_linear() => { approval_answer.apply(TerminalInput::Character(character)); }
                        TerminalInput::Backspace if terminal.is_linear() => { approval_answer.apply(TerminalInput::Backspace); }
                        TerminalInput::Character(character) if character.is_ascii_digit() => {
                            if let Some(choice) = input.choice_index(choices.len()).map(|index| choices[index]) {
                                if approval_more && choice != crate::presentation::ToolApprovalChoice::No {
                                    approval_review_notice = true;
                                } else { selected = Some(choice); }
                            }
                        }
                        TerminalInput::Submit if approval_more => approval_page += 1,
                        TerminalInput::Submit => {
                            selected = if terminal.is_linear() {
                                let choice = linear_approval_choice(approval_answer.text(), &choices);
                                approval_answer.clear();
                                choice
                            } else { Some(choices[approval_choice]) };
                        }
                        _ => {}
                    }
                    if let Some(choice) = selected {
                        if choice == crate::presentation::ToolApprovalChoice::AutoEdits {
                            for _ in 0..2 {
                                if composer.approval_mode() == Some(arany::ApprovalMode::AutoEdits) { break; }
                                let _ = super::permissions::cycle(composer);
                            }
                            approvals.set_mode(arany::ApprovalMode::AutoEdits);
                        }
                        pending.take().expect("pending action").decide(choice.reply());
                        terminal.restore_draft_input(composer.text().len()).map_err(|error| error.to_string())?;
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
                    let notice = match terminal.agent_input(view, input).await {
                        Ok(notice) => notice.map(TerminalNotice::Transient),
                        Err(_) => break "terminal rendering failed".to_owned(),
                    };
                    if !terminal.agents_open()
                        && draw_active_snapshot(terminal, view, composer, user_recorded, stop, notice).is_err()
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
                // A child view takes Escape before Run cancellation; Ctrl+C passes through.
                let agent_notice = match terminal.agent_chat_input(view, input, composer).await {
                    Ok(AgentChatInput::ChildMessage { target, text }) => {
                        let agent = view.runs.iter().find(|run| run.id == target.run_id)
                            .and_then(|run| run.agents.iter().find(|agent| agent.id == target.agent_run_id));
                        let ordinal = agent.map_or(0, |agent| agent.ordinal);
                        let count = agent.map_or(0, |agent| agent.steering_notes.len());
                        let (notice, offer) = if stop == ActiveStop::Running {
                            match steering.offer_child_note(target, text.clone()) {
                                Ok(()) => (format!("Note waiting for child {ordinal}'s next step"), Some((text, count))),
                                Err(refused) => (steering_refusal(refused, autonomous), None),
                            }
                        } else { ("Child note unavailable while stopping; draft retained".into(), None) };
                        terminal.child_note_feedback(target, notice, offer);
                        Some(None)
                    }
                    Ok(AgentChatInput::ChildEdited(target)) => {
                        terminal.withdraw_child_offer(target, &steering);
                        Some(None)
                    }
                    Ok(AgentChatInput::Pass) => None,
                    Ok(AgentChatInput::Handled) => continue,
                    Ok(AgentChatInput::Returned) => Some(None),
                    Ok(AgentChatInput::Notice(text)) => Some(Some(TerminalNotice::Transient(text))),
                    Err(_) => break "terminal rendering failed".to_owned(),
                };
                if let Some(notice) = agent_notice {
                    if draw_active_snapshot(terminal, view, composer, user_recorded, stop, notice).is_err() {
                        break "terminal rendering failed".to_owned();
                    }
                    continue;
                }
                if terminal.ignores_page_key(input) {
                    continue;
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
                if terminal.handle_history_input(input, view, composer, true).await {
                    if draw_active_snapshot(terminal, view, composer, user_recorded, stop, None).is_err() {
                        break "terminal rendering failed".to_owned();
                    }
                    continue;
                }
                let input = composer.chat_input(input);
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
                    TerminalInput::Submit if !terminal.is_linear() && composer.continue_line() => {
                        if draw_active_snapshot(terminal, view, composer, user_recorded, stop, None).is_err() {
                            break "terminal rendering failed".to_owned();
                        }
                        None
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
                    TerminalInput::Submit if exits_after_settled_run(composer, view, starting_sequence) => {
                        abandon_drafts_for_exit(composer, &mut held, &steering);
                        offered = None;
                        if terminal.restore_draft_input(0).is_err() {
                            break "terminal input failed".to_owned();
                        }
                        terminal.defer_input(TerminalInput::EndOfInput);
                        exit_after_run = true;
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
                                        terminal.settle_child_notes(view, &steering, true);
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
                                if terminal.open_agents(view, true).await.is_err() {
                                    break "terminal rendering failed".to_owned();
                                }
                                None
                            }
                            ActiveSubmission::OpenHelp => {
                                if terminal.open_help(view).await.is_err() {
                                    break "terminal rendering failed".to_owned();
                                }
                                None
                            }
                            ActiveSubmission::OpenAgentChats(number) => match terminal.open_agent_chats(view, number).await {
                                Ok(AgentChatInput::Notice(text)) => Some(Feedback::Transient(text.into())),
                                Ok(_) => None,
                                Err(_) => break "terminal rendering failed".to_owned(),
                            },
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
                        terminal.settle_child_notes(view, &steering, true);
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
                    // The screen already shows the outcome, so exit once the Run returns.
                    TerminalInput::EndOfInput if run_settled(view, starting_sequence) => {
                        terminal.defer_input(TerminalInput::EndOfInput);
                        exit_after_run = true;
                        None
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
    terminal.settle_child_notes(view, &steering, true);
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
    let finish = async {
        let handle = match handle {
            Some(handle) => handle,
            None => future.await?,
        };
        run::finish_run(handle.await, true)
    };
    match tokio::time::timeout(SHUTDOWN_GRACE, finish).await {
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
            ActiveStop::Running => "Run is active; Ctrl+C cancels",
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
    engine: &arany::Engine,
    terminal: &mut AttachedTerminal,
    admission: &Admission,
    view: &SessionView,
    composer: &mut Composer,
    pinned: Option<(&arany::RunConfig, arany::RunId)>,
) -> Result<CompactionOutcome, String> {
    let pinned_config = pinned.map(|(config, _)| config).or_else(|| {
        view.runs
            .last()
            .and_then(|run| run.config.as_ref())
            .filter(|config| config.roster.is_some())
    });
    let (profile, model, effort, account_id) = if let Some(config) = pinned_config {
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
    let selection = Selection {
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
        engine,
        &admission.state_dir,
        selection,
        view.id,
        &admission.workspace,
        pinned.map(|(_, run_id)| run_id),
        view.runs
            .last()
            .and_then(|run| run.config.as_ref())
            .filter(|config| config.roster.is_some()),
    );
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return Ok(match result {
                Ok(Some(record)) => CompactionOutcome::Notice(
                    with_compaction_switches(&admission.state_dir, view, compaction_notice(&record)).await,
                ),
                Ok(None) => CompactionOutcome::Notice(
                    "Automatic compaction skipped; Run boundary changed or already compacted".into(),
                ),
                Err(error) => CompactionOutcome::Unavailable(error),
            }),
            input = terminal.next_chat_input() => {
                let input = input.map_err(|error| error.to_string())?;
                composer.observe_input(input);
                // The pager and a scrollback dump own their keys before the hidden composer.
                if terminal.handle_transcript_input(input, view).map_err(|error| error.to_string())?
                    || terminal.handle_history_input(input, view, composer, true).await
                {
                    terminal.draw_busy(view, composer, None)
                        .map_err(|error| error.to_string())?;
                    continue;
                }
                let input = composer.chat_input(input);
                let notice = match input {
                    TerminalInput::Submit if !terminal.is_linear() && composer.continue_line() => {
                        terminal.draw_busy(view, composer, None).map_err(|error| error.to_string())?;
                        None
                    },
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

fn compaction_switches(view: &SessionView) -> Vec<&arany::ProviderFallback> {
    view.runs.last().map_or_else(Vec::new, |run| {
        run.provider_fallbacks
            .iter()
            .filter(|(_, fallback)| fallback.role == arany::ProviderRole::Compaction)
            .map(|(_, fallback)| fallback)
            .collect()
    })
}

// `None` marks a view from another Run, so no earlier switch was shown for it.
fn unshown<T>(switches: Vec<T>, shown: Option<usize>) -> Vec<T> {
    switches.into_iter().skip(shown.unwrap_or(0)).collect()
}

async fn with_compaction_switches(
    state_dir: &std::path::Path,
    before: &SessionView,
    notice: String,
) -> String {
    let shown = compaction_switches(before).len();
    match super::load_view(state_dir, before.id).await {
        Ok(after) => {
            let same_run =
                before.runs.last().map(|run| run.id) == after.runs.last().map(|run| run.id);
            let shown = same_run.then_some(shown);
            append_switch_notices(notice, &unshown(compaction_switches(&after), shown))
        }
        Err(error) => {
            format!("{notice}\nError: compaction Provider switches could not be reloaded: {error}")
        }
    }
}

fn append_switch_notices(notice: String, switches: &[&arany::ProviderFallback]) -> String {
    switches.iter().fold(notice, |text, fallback| {
        format!(
            "{text}\n{}",
            crate::presentation::escape_terminal(&fallback.notice())
        )
    })
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

const EVENT_DRAIN_LIMIT: usize = 64;

// Applies one burst: `first`, then Events already ready, at most EVENT_DRAIN_LIMIT in commit
// order, so input is polled again before the next burst.
async fn apply_burst(
    state_dir: &std::path::Path,
    view: &mut SessionView,
    first: Option<arany::Tapped>,
    mut ready: impl AsyncFnMut() -> Option<Option<arany::Tapped>>,
) -> Result<(), String> {
    let mut event = first;
    for drained in 1..=EVENT_DRAIN_LIMIT {
        let Some(tapped) = event else {
            return Err("Run event stream closed".to_owned());
        };
        if apply_tapped(state_dir, view, tapped).await.is_err() {
            return Err("Run history unavailable; check Session history".to_owned());
        }
        if drained == EVENT_DRAIN_LIMIT {
            break;
        }
        match ready().await {
            Some(next) => event = next,
            None => break,
        }
    }
    Ok(())
}

async fn apply_tapped(
    state_dir: &std::path::Path,
    view: &mut SessionView,
    tapped: arany::Tapped,
) -> Result<(), String> {
    match tapped {
        arany::Tapped::Committed(event) => {
            if event.session_id != view.id || event.sequence <= view.last_sequence {
                return Ok(());
            }
            if event.sequence != view.last_sequence + 1 {
                return catch_up(state_dir, view).await;
            }
            match arany::reduce_into(view, &event) {
                Ok(()) => Ok(()),
                Err(_) => catch_up(state_dir, view).await,
            }
        }
        arany::Tapped::Resync { .. } => catch_up(state_dir, view).await,
    }
}

async fn catch_up(state_dir: &std::path::Path, view: &mut SessionView) -> Result<(), String> {
    let state = arany::StateRoot::open_existing(state_dir).map_err(|error| error.to_string())?;
    let store = arany::Store::open_read_only(state).map_err(|error| error.to_string())?;
    let loaded = store
        .load_open_view(view.id)
        .await
        .map_err(|error| error.to_string());
    store.close().await.map_err(|error| error.to_string())?;
    *view = loaded?.ok_or_else(|| "Session history unavailable".to_owned())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_shown_before_a_stale_view_do_not_skip_or_panic() {
        assert_eq!(unshown(vec![1, 2, 3], Some(1)), vec![2, 3]);
        assert_eq!(unshown(vec![1], Some(2)), Vec::<i32>::new());
        assert_eq!(unshown(vec![1, 2], None), vec![1, 2]);
    }

    #[test]
    fn compaction_notice_appends_only_this_compactions_switches() {
        let identity = |model: &str| {
            arany::ProviderIdentity::new("openai", model, arany::Provenance::native())
        };
        let switch = arany::ProviderFallback {
            role: arany::ProviderRole::Compaction,
            call_index: 0,
            failure: arany::ProviderCallDisposition::Unavailable,
            from: identity("gpt-5.4"),
            to: identity("gpt-5.4-mini"),
        };
        assert_eq!(
            append_switch_notices("Compaction saved".into(), &[]),
            "Compaction saved"
        );
        let text = append_switch_notices("Compaction saved".into(), &[&switch]);
        assert_eq!(text, format!("Compaction saved\n{}", switch.notice()));
        assert!(text.contains("compaction"));
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
mod burst_tests {
    use super::*;
    use arany::test_support::{
        engine::{open, run_to_end},
        scenario::{ScriptedProvider, request},
    };
    use arany::{
        Answer, ChildAssignment, CollaborationPolicy, Delegate, EngineConfig, ProviderOutcome,
        StateRoot, Store, Tapped,
    };
    use std::{collections::VecDeque, sync::Arc};

    // A team Run with two children, then single Runs, so the journal outgrows one burst.
    async fn committed_team() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        Vec<arany::EventEnvelope>,
    ) {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).unwrap();
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").unwrap();
        let state_dir = root.path().join("state");
        let engine = open(
            StateRoot::admit(&state_dir).unwrap(),
            EngineConfig::default(),
        )
        .unwrap();
        let answer = |text: &str| ProviderOutcome::Answer(Answer { text: text.into() });
        let child = |objective: &str| ChildAssignment {
            editor: None,
            objective: objective.into(),
            profile: None,
        };
        let mut responses = vec![
            ProviderOutcome::Delegate(Delegate {
                agent: None,
                children: vec![child("first task"), child("second task")],
            }),
            answer("first result"),
            answer("second result"),
            answer("team answer"),
        ];
        responses.extend((0..8).map(|_| answer("later answer")));
        let mut provider = ScriptedProvider::new(responses);
        // One call at a time, so each child takes its scripted answer in spawn order.
        provider.identity.limits.concurrency = 1;
        let provider = Arc::new(provider);
        let mut team = request(&workspace, "team task");
        team.policy = CollaborationPolicy::Team {
            max_active_children: 2,
        };
        let session = run_to_end(&engine, &provider, team)
            .await
            .unwrap()
            .session_id;
        for _ in 0..8 {
            let mut later = request(&workspace, "later task");
            later.session_id = Some(session);
            later.title = None;
            run_to_end(&engine, &provider, later).await.unwrap();
        }
        engine.close().await.unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(&state_dir).unwrap()).unwrap();
        let events = store.load_session(session).await.unwrap();
        store.close().await.unwrap();
        (root, state_dir, events)
    }

    fn tapped(envelope: &arany::EventEnvelope) -> Tapped {
        Tapped::Committed(Arc::new(envelope.clone()))
    }

    fn replay(events: &[arany::EventEnvelope]) -> SessionView {
        SessionView::replay(events[0].session_id, events)
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn committed_events_apply_in_bounded_in_order_bursts_and_gaps_reload_the_view() {
        let (_root, state_dir, events) = committed_team().await;
        let full = replay(&events);

        // Commit order, one Event at a time, reaches the strictly replayed view.
        let mut view = replay(&events[..1]);
        for envelope in &events[1..] {
            apply_tapped(&state_dir, &mut view, tapped(envelope))
                .await
                .unwrap();
            assert_eq!(view.last_sequence, envelope.sequence);
        }
        assert_eq!(view.runs, full.runs);

        // One burst applies at most EVENT_DRAIN_LIMIT ready Events and leaves the rest queued.
        let mut queue = events[1..].iter().map(tapped).collect::<VecDeque<_>>();
        assert!(queue.len() > EVENT_DRAIN_LIMIT);
        let mut view = replay(&events[..1]);
        let first = queue.pop_front();
        apply_burst(&state_dir, &mut view, first, async || {
            queue.pop_front().map(Some)
        })
        .await
        .unwrap();
        assert_eq!(view.last_sequence, events[EVENT_DRAIN_LIMIT].sequence);
        assert_eq!(queue.len(), events.len() - 1 - EVENT_DRAIN_LIMIT);
        // Nothing else ready ends the burst after its first Event.
        let first = queue.pop_front();
        apply_burst(&state_dir, &mut view, first, async || None)
            .await
            .unwrap();
        assert_eq!(view.last_sequence, events[EVENT_DRAIN_LIMIT + 1].sequence);

        // A sequence gap or a resync reloads the whole validated committed view.
        let gap = tapped(&events[EVENT_DRAIN_LIMIT + 3]);
        for missed in [Some(gap), Some(Tapped::Resync { missed: 3 })] {
            let mut view = replay(&events[..=EVENT_DRAIN_LIMIT]);
            apply_burst(&state_dir, &mut view, missed, async || None)
                .await
                .unwrap();
            assert_eq!(view.last_sequence, full.last_sequence);
            assert_eq!(view.runs, full.runs);
        }
        // A closed stream or unreadable history stops the Run view instead of inventing progress.
        let mut view = replay(&events[..1]);
        for (state, event, expected) in [
            (state_dir.as_path(), None, "Run event stream closed"),
            (
                std::path::Path::new("/nonexistent-arany-state"),
                Some(Tapped::Resync { missed: 1 }),
                "Run history unavailable; check Session history",
            ),
        ] {
            assert_eq!(
                apply_burst(state, &mut view, event, async || None).await,
                Err(expected.to_owned())
            );
        }
        assert_eq!(view.last_sequence, events[0].sequence);
    }
}

#[cfg(test)]
mod goal_request_tests {
    use super::*;
    use crate::cli::attached::{Admission, EntryMode};
    use arany::{SessionDefaults, WorkspaceIdentity};

    fn session_with_goal(autonomous: bool) -> SessionView {
        let session_id = arany::SessionId::new();
        let event = arany::Event::GoalStarted {
            goal_id: arany::GoalId::new(),
            objective: "Ship the parser".into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 2,
            },
            budget: arany::GoalBudget::default(),
            phase: arany::GoalPhase::Implementation,
            autonomous,
            resource_settings: arany::ToolResourceSettings::default(),
            continues: None,
        };
        let started = arany::Event::SessionStarted {
            title: "Goals".into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 2,
            },
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
            goal_request(&view, GoalStart::Plain, Default::default()),
            Ok(arany::GoalRequest::default())
        );
        let autonomous = goal_request(&view, GoalStart::Autonomous, Default::default())
            .expect("autonomous start");
        assert!(autonomous.autonomous);
        assert_eq!(autonomous.id, None);
        assert_eq!(
            goal_request(&view, GoalStart::Resume, Default::default()),
            Err("No interrupted Goal is available to resume".into())
        );
    }

    #[test]
    fn end_of_input_waits_for_only_the_current_run_to_settle() {
        let mut view = session_with_goal(false);
        let starting_sequence = view.last_sequence;
        assert!(!run_settled(&view, starting_sequence), "no current Run");
        let mut run = arany::RunView {
            skill: None,
            supervisor: Default::default(),
            workers: Default::default(),
            id: arany::RunId::new(),
            objective: "Fix".into(),
            images: Vec::new(),
            config: None,
            provider_fallbacks: Vec::new(),
            provider_retries: Vec::new(),
            agents: Vec::new(),
            assistant_message: None,
            status: arany::RunStatus::Finished,
            accepted_sequence: starting_sequence,
            finished_sequence: Some(starting_sequence),
            failure: None,
            tools: Vec::new(),
            tool_compaction_summary: None,
            tool_compaction_covered_count: 0,
            tool_results_cleared_count: 0,
            definitions: Default::default(),
            display: Vec::new(),
            steering_notes: Vec::new(),
            lifecycle_hooks: Vec::new(),
            lifecycle_hook_sequence: None,
            provider_sequences: Default::default(),
        };
        view.runs.push(run.clone());
        assert!(
            !run_settled(&view, starting_sequence),
            "an earlier Run cannot defer exit"
        );
        run.accepted_sequence = starting_sequence + 1;
        for (status, settled) in [
            (arany::RunStatus::Pending, false),
            (arany::RunStatus::Active, false),
            (arany::RunStatus::Finished, true),
            (arany::RunStatus::Failed, true),
            (arany::RunStatus::Cancelled, true),
        ] {
            run.status = status;
            *view.runs.last_mut().unwrap() = run.clone();
            assert_eq!(run_settled(&view, starting_sequence), settled, "{status:?}");
        }
    }

    #[test]
    fn exit_waits_for_only_the_current_run_to_settle() {
        let mut view = session_with_goal(false);
        let starting_sequence = view.last_sequence;
        let typed = |text: &str| {
            let mut composer = Composer::default();
            for character in text.chars() {
                composer.apply(TerminalInput::Character(character));
            }
            composer
        };
        let exit = typed("/exit");
        assert!(
            !exits_after_settled_run(&exit, &view, starting_sequence),
            "no current Run"
        );
        let mut run = arany::RunView {
            skill: None,
            supervisor: Default::default(),
            workers: Default::default(),
            id: arany::RunId::new(),
            objective: "Fix".into(),
            images: Vec::new(),
            config: None,
            provider_fallbacks: Vec::new(),
            provider_retries: Vec::new(),
            agents: Vec::new(),
            assistant_message: None,
            status: arany::RunStatus::Active,
            accepted_sequence: starting_sequence + 1,
            finished_sequence: None,
            failure: None,
            tools: Vec::new(),
            tool_compaction_summary: None,
            tool_compaction_covered_count: 0,
            tool_results_cleared_count: 0,
            definitions: Default::default(),
            display: Vec::new(),
            steering_notes: Vec::new(),
            lifecycle_hooks: Vec::new(),
            lifecycle_hook_sequence: None,
            provider_sequences: Default::default(),
        };
        view.runs.push(run.clone());
        assert!(
            !exits_after_settled_run(&exit, &view, starting_sequence),
            "an active Run keeps /exit refused"
        );
        run.status = arany::RunStatus::Finished;
        *view.runs.last_mut().unwrap() = run;
        assert!(exits_after_settled_run(&exit, &view, starting_sequence));
        for other in ["/exit now", "/status", "exit"] {
            assert!(
                !exits_after_settled_run(&typed(other), &view, starting_sequence),
                "{other}"
            );
        }
    }

    #[test]
    fn exit_abandons_a_held_linear_draft_and_waiting_note() {
        let mut composer = Composer::default();
        for character in "refused draft".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        let mut held = hold_draft(&mut composer);
        assert!(held.is_some(), "a refused linear draft is held");
        let steering = RunSteering::default();
        steering
            .offer_note("waiting note".into())
            .expect("note waits");
        for character in "/exit".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        abandon_drafts_for_exit(&mut composer, &mut held, &steering);
        assert!(composer.is_empty());
        // Run end hands back nothing, so the deferred Ctrl+D exits instead of retaining a draft.
        assert!(retained_linear_draft(steering.withdraw_note(), held.take()).is_none());
    }

    #[test]
    fn continuation_takes_its_file_mentions_from_the_source_objective() {
        let mut view = session_with_goal(true);
        view.goals[0].objective = "Fix @src/lib.rs".into();
        view.goals[0].status = arany::GoalStatus::Failed;
        view.goals[0].budget_closed = true;
        let run = arany::RunView {
            skill: None,
            supervisor: Default::default(),
            workers: Default::default(),
            id: arany::RunId::new(),
            objective: "Fix".into(),
            images: Vec::new(),
            config: None,
            provider_fallbacks: Vec::new(),
            provider_retries: Vec::new(),
            agents: Vec::new(),
            assistant_message: None,
            status: arany::RunStatus::Finished,
            accepted_sequence: 3,
            finished_sequence: Some(4),
            failure: None,
            tools: Vec::new(),
            tool_compaction_summary: None,
            tool_compaction_covered_count: 0,
            tool_results_cleared_count: 0,
            definitions: Default::default(),
            display: Vec::new(),
            steering_notes: Vec::new(),
            lifecycle_hooks: Vec::new(),
            lifecycle_hook_sequence: None,
            provider_sequences: Default::default(),
        };
        view.goals[0].run_ids.push(run.id);
        view.runs.push(run);
        assert_eq!(
            mention_source(&view, GoalStart::Continue, ""),
            "Fix @src/lib.rs"
        );
        assert_eq!(mention_source(&view, GoalStart::Plain, ""), "");
        assert_eq!(
            arany::mention_paths(mention_source(&view, GoalStart::Continue, "")).unwrap(),
            vec![std::path::PathBuf::from("src/lib.rs")]
        );
    }

    #[test]
    fn goal_continue_refuses_goals_that_were_not_closed_by_exhaustion() {
        let mut view = session_with_goal(true);
        let refusal = Err("No exhausted Goal is available to continue".to_owned());
        view.goals[0].status = arany::GoalStatus::Interrupted;
        assert_eq!(
            goal_request(&view, GoalStart::Continue, Default::default()),
            refusal
        );
        view.goals[0].status = arany::GoalStatus::Failed;
        view.goals[0].budget.max_runs = 0;
        assert_eq!(
            goal_request(&view, GoalStart::Continue, Default::default()),
            refusal
        );
    }

    #[test]
    fn paused_goal_admits_new_goals_and_resumes_only_through_goal_resume() {
        let mut view = session_with_goal(true);
        view.goals[0].status = arany::GoalStatus::Interrupted;
        let id = view.goals[0].id;
        assert_eq!(
            goal_request(&view, GoalStart::Resume, Default::default()),
            Ok(arany::GoalRequest::resume(id, true))
        );
        assert_eq!(
            goal_request(&view, GoalStart::Plain, Default::default()),
            Ok(arany::GoalRequest::default())
        );
        let autonomous = goal_request(&view, GoalStart::Autonomous, Default::default())
            .expect("autonomous start");
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
            roster: None,
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
            "Steering note exceeds 8.0 KB; draft retained"
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

#[cfg(test)]
mod approval_reply_tests {
    use super::linear_approval_choice;
    use crate::presentation::ToolApprovalChoice::{AutoEdits, ExactCommand, GitPrefix, No, Yes};

    #[test]
    fn numbered_replies_use_choice_identity_and_refuse_unavailable_choices() {
        let prefix = GitPrefix(arany::permissions::TypedGitPrefix::Branch);
        for choices in [
            vec![Yes, prefix, No],
            vec![Yes, No],
            vec![Yes, AutoEdits, No],
            vec![Yes, ExactCommand, No],
        ] {
            for (index, choice) in choices.iter().enumerate() {
                assert_eq!(
                    linear_approval_choice(&(index + 1).to_string(), &choices),
                    Some(*choice)
                );
            }
            for (answer, expected) in [
                ("", Some(No)),
                ("YES", Some(Yes)),
                ("no", Some(No)),
                ("allow", None),
                ("always", None),
                ("0", None),
                ("4", None),
            ] {
                assert_eq!(linear_approval_choice(answer, &choices), expected);
            }
        }
        assert_eq!(AutoEdits.reply(), arany::ApprovalReply::AllowOnce);
        assert_eq!(ExactCommand.reply(), arany::ApprovalReply::AllowForSession);
        assert_eq!(No.reply(), arany::ApprovalReply::Deny);
        assert_eq!(prefix.reply(), arany::ApprovalReply::AllowPrefixInFolder);
    }
}
