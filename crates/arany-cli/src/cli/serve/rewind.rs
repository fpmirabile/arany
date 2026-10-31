use super::{
    server::Fault,
    threads::{self, Threads},
};
use arany::rewind::{self, CheckpointTarget, NonRewindable};
use arany::{
    ConversationRewindOutcome, Engine, EngineError, RewindIssue, RewindPreview, RewindRequest,
    RunCancellation, SessionId, ToolGrant,
};
use arany_cli::{
    presentation::rewind as display,
    protocol::{methods::*, rpc::Reason},
};
use std::path::Path;

pub(super) struct Pending {
    thread: SessionId,
    token: String,
    target: CheckpointTarget,
    head: u64,
    prefix: [u8; 32],
    code: Option<RewindPreview>,
}

impl Pending {
    pub fn belongs_to(&self, id: SessionId) -> bool {
        self.thread == id
    }
}

#[derive(Clone, Copy)]
pub(super) struct Context<'a> {
    pub engine: &'a Engine,
    pub state: &'a Path,
}

pub(super) async fn preview(
    context: Context<'_>,
    threads: &Threads,
    input: ThreadRewindPreviewParams,
) -> Result<(ThreadRewindPreviewResult, Pending), Fault> {
    let Context { engine, state } = context;
    let loaded = threads
        .map
        .get(&input.thread_id)
        .ok_or_else(|| Fault::state(Reason::ThreadNotLoaded))?;
    let view = threads::load_view(state, input.thread_id).await?;
    let target = checkpoint_target(&view, input.turn_id).ok_or_else(Fault::params)?;
    let plan =
        rewind::plan(&view, &target).map_err(|_| Fault::engine(EngineError::RewindTooLarge))?;
    let mut blockers = Vec::new();
    if !plan.blockers.is_empty() {
        blockers.push(RewindBlocker::UncertainPublication);
    }
    let code = code_preview(engine, loaded, input.thread_id, &target, &mut blockers).await?;
    let (plan, code, files) = tokio::task::spawn_blocking(move || {
        let files = preview_files(&plan, code.as_ref());
        (plan, code, files)
    })
    .await
    .map_err(|_| Fault::engine(EngineError::CoordinatorFailed))?;
    if !files.is_empty()
        && files
            .iter()
            .all(|file| file.state == RewindFileState::OutsideGrant)
        && !blockers.contains(&RewindBlocker::NoWriteGrant)
    {
        blockers.push(RewindBlocker::NoWriteGrant);
    }
    let restore = restore_choices(&plan, &blockers, &target);
    let checkpoint = wire_checkpoint(&target);
    let token = uuid::Uuid::now_v7().to_string();
    let pending = Pending {
        thread: input.thread_id,
        token: token.clone(),
        target,
        head: view.last_sequence,
        prefix: view.rewinds.prefix_digest(),
        code,
    };
    Ok((
        ThreadRewindPreviewResult {
            checkpoint,
            restore,
            files,
            blockers,
            non_rewindable: non_rewindable(&plan),
            preview_token: token,
        },
        pending,
    ))
}

fn checkpoint_target(view: &arany::SessionView, turn_id: arany::RunId) -> Option<CheckpointTarget> {
    view.rewinds
        .checkpoints
        .iter()
        .find(|target| match target {
            CheckpointTarget::Run { run_id, .. } => *run_id == turn_id,
            CheckpointTarget::Rewind { rewind_id, .. } => {
                rewind_id.to_string() == turn_id.to_string()
            }
        })
        .cloned()
}

async fn code_preview(
    engine: &Engine,
    loaded: &threads::LoadedThread,
    thread_id: SessionId,
    target: &CheckpointTarget,
    blockers: &mut Vec<RewindBlocker>,
) -> Result<Option<RewindPreview>, Fault> {
    if matches!(loaded.grant, ToolGrant::None) {
        blockers.push(RewindBlocker::NoWriteGrant);
        return Ok(None);
    }
    match engine
        .rewind_preview(RewindRequest {
            session_id: thread_id,
            workspace: loaded.workspace.clone(),
            target: target.clone(),
            tools: loaded.grant.clone(),
            kept: Vec::new(),
            overwrite: Vec::new(),
        })
        .await
    {
        Ok(preview) => Ok(Some(preview)),
        Err(EngineError::GuardUnavailable) => {
            blockers.push(RewindBlocker::GuardUnavailable);
            Ok(None)
        }
        Err(EngineError::RewindBlocked) => {
            blockers.push(RewindBlocker::NoWriteGrant);
            Ok(None)
        }
        Err(error) => Err(Fault::engine(error)),
    }
}

fn restore_choices(
    plan: &rewind::RewindPlan,
    blockers: &[RewindBlocker],
    target: &CheckpointTarget,
) -> Vec<RewindRestore> {
    let conversation = matches!(target, CheckpointTarget::Run { .. });
    let mut restore = Vec::new();
    if !plan.files.is_empty() && blockers.is_empty() && conversation {
        restore.push(RewindRestore::Both);
    }
    if conversation {
        restore.push(RewindRestore::Conversation);
    }
    if !plan.files.is_empty() && blockers.is_empty() {
        restore.push(RewindRestore::Code);
    }
    restore
}

fn wire_checkpoint(target: &CheckpointTarget) -> RewindCheckpoint {
    match target {
        CheckpointTarget::Run { run_id, sequence } => RewindCheckpoint {
            kind: RewindCheckpointKind::Run,
            id: run_id.to_string(),
            sequence: *sequence,
        },
        CheckpointTarget::Rewind {
            rewind_id,
            sequence,
        } => RewindCheckpoint {
            kind: RewindCheckpointKind::Rewind,
            id: rewind_id.to_string(),
            sequence: *sequence,
        },
    }
}

fn non_rewindable(plan: &rewind::RewindPlan) -> Vec<RewindNonRewindable> {
    plan.non_rewindable
        .iter()
        .map(|(sequence, effect)| RewindNonRewindable {
            sequence: *sequence,
            effect: match effect {
                NonRewindable::Host => RewindEffect::Host,
                NonRewindable::Git => RewindEffect::Git,
                NonRewindable::UnguardedCommand => RewindEffect::UnguardedCommand,
                NonRewindable::Mcp => RewindEffect::Mcp,
                NonRewindable::Cache => RewindEffect::Cache,
                NonRewindable::Directory => RewindEffect::Directory,
            },
        })
        .collect()
}

fn preview_files(
    plan: &rewind::RewindPlan,
    code: Option<&RewindPreview>,
) -> Vec<RewindFilePreview> {
    let Some(code) = code else {
        return plan
            .files
            .iter()
            .map(|f| RewindFilePreview {
                path: f.path.clone(),
                state: RewindFileState::OutsideGrant,
                added: None,
                removed: None,
            })
            .collect();
    };
    code.files()
        .iter()
        .enumerate()
        .map(|(index, file)| {
            let state = match file.issues.first() {
                Some(RewindIssue::Drifted) => RewindFileState::Drifted,
                Some(RewindIssue::HistoryDrift) => RewindFileState::HistoryDrift,
                Some(RewindIssue::Unavailable) => RewindFileState::Unavailable,
                Some(RewindIssue::Unsafe) => RewindFileState::Unsafe,
                Some(RewindIssue::OutsideGrant) => RewindFileState::OutsideGrant,
                None => match display::file_state(code, index) {
                    "delete" => RewindFileState::Delete,
                    "recreate" => RewindFileState::Recreate,
                    _ => RewindFileState::Restore,
                },
            };
            let counts = code
                .comparisons()
                .iter()
                .find(|change| change.path == file.path)
                .and_then(display::line_counts);
            RewindFilePreview {
                path: file.path.clone(),
                state,
                added: counts.map(|c| c.0),
                removed: counts.map(|c| c.1),
            }
        })
        .collect()
}

pub(super) async fn restore(
    context: Context<'_>,
    threads: &mut Threads,
    pending: &mut Option<Pending>,
    input: ThreadRewindParams,
    cancellation: RunCancellation,
) -> Result<ThreadRewindResult, Fault> {
    let Context { engine, state } = context;
    let loaded = threads
        .map
        .get(&input.thread_id)
        .ok_or_else(|| Fault::state(Reason::ThreadNotLoaded))?
        .clone();
    check_restore_request(threads, &input)?;
    let saved = pending
        .as_ref()
        .filter(|p| p.thread == input.thread_id && p.token == input.preview_token)
        .ok_or_else(|| Fault::state(Reason::PreviewChanged))?;
    let view = threads::load_view(state, input.thread_id).await?;
    if saved.head != view.last_sequence || saved.prefix != view.rewinds.prefix_digest() {
        pending.take();
        return Err(Fault::state(Reason::PreviewChanged));
    }
    check_restore_target(threads, &input, &saved.target)?;
    let saved = pending.take().expect("checked preview token");
    let label = display::checkpoint_label(&view, &saved.target);
    let mut result = ThreadRewindResult {
        rewind: RewindReceipt {
            id: None,
            outcome: RewindResultOutcome::Restored,
            conflicts: Vec::new(),
            conflict_count: 0,
        },
        thread: None,
        conversation: None,
        notices: Vec::new(),
    };
    let conversation = if input.restore == RewindRestore::Conversation {
        restore_conversation(engine, &loaded, saved).await?
    } else {
        let request = CodeRestore {
            context,
            loaded: &loaded,
            restore: input.restore,
            label: &label,
        };
        match restore_code(request, saved, input.kept, input.overwrite, cancellation).await? {
            CodeOutcome::Published(receipt, notice, conversation) => {
                result.rewind = receipt;
                result.notices.push(notice);
                conversation
            }
            CodeOutcome::Recovered(receipt, notices) => {
                result.rewind = receipt;
                result.notices.extend(notices);
                return Ok(result);
            }
        }
    };
    match conversation {
        ConversationRewindOutcome::Restored(conversation) => {
            if let Some(id) = conversation.session_id {
                match (threads::load_view(state, id).await, threads::created_at_ms(state, id).await) {
                    (Ok(forked), Ok(created)) => {
                        result.thread = Some(threads::thread(&forked, created, &loaded.workspace));
                        threads.map.insert(id, loaded);
                    }
                    _ => result.notices.push(format!("Conversation fork {id} saved but could not be loaded; use thread/resume")),
                }
            }
            let images_omitted = !conversation.images.is_empty();
            if images_omitted { result.notices.push("Images were not copied; attach them again".into()); }
            result.conversation = Some(RewindConversation { thread_id: conversation.session_id, prompt: conversation.prompt, images_omitted });
        }
        ConversationRewindOutcome::Failed(_) => result.notices.push("Code restored; conversation unchanged; use thread/rewind/preview and Restore conversation".into()),
        ConversationRewindOutcome::Unchanged if input.restore == RewindRestore::Both && result.rewind.outcome == RewindResultOutcome::Restored => result.notices.push("Code restored; conversation unchanged".into()),
        _ => {}
    }
    Ok(result)
}

fn check_restore_request(threads: &Threads, input: &ThreadRewindParams) -> Result<(), Fault> {
    if threads.approval_channel != super::server::ApprovalChannel::Human {
        return Err(Fault::state(Reason::HumanConfirmationRequired));
    }
    if input.kept.len() + input.overwrite.len() > arany::rewind::MAX_REWIND_SELECTIONS
        || input.kept.iter().any(|p| input.overwrite.contains(p))
    {
        return Err(Fault::params());
    }
    if !input.overwrite.is_empty() && !input.confirm_overwrite {
        return Err(Fault::state(Reason::OverwriteConfirmationRequired));
    }
    Ok(())
}

fn check_restore_target(
    threads: &Threads,
    input: &ThreadRewindParams,
    target: &CheckpointTarget,
) -> Result<(), Fault> {
    if input.restore != RewindRestore::Code {
        if !matches!(target, CheckpointTarget::Run { .. }) {
            return Err(Fault::params());
        }
        threads.check_capacity(None)?;
    }
    if input.restore == RewindRestore::Conversation
        && (!input.kept.is_empty() || !input.overwrite.is_empty() || input.confirm_overwrite)
    {
        return Err(Fault::params());
    }
    Ok(())
}

async fn restore_conversation(
    engine: &Engine,
    loaded: &threads::LoadedThread,
    saved: Pending,
) -> Result<ConversationRewindOutcome, Fault> {
    let CheckpointTarget::Run { run_id, .. } = saved.target else {
        return Err(Fault::params());
    };
    Ok(ConversationRewindOutcome::Restored(Box::new(
        engine
            .rewind_conversation_at_head(
                saved.thread,
                loaded.workspace.clone(),
                run_id,
                (saved.head, saved.prefix),
            )
            .await
            .map_err(Fault::engine)?,
    )))
}

struct CodeRestore<'a> {
    context: Context<'a>,
    loaded: &'a threads::LoadedThread,
    restore: RewindRestore,
    label: &'a str,
}

enum CodeOutcome {
    Published(RewindReceipt, String, ConversationRewindOutcome),
    // A failed publication whose operation the journal shows; the receipt and notices settle it.
    Recovered(RewindReceipt, Vec<String>),
}

async fn restore_code(
    request: CodeRestore<'_>,
    saved: Pending,
    kept: Vec<String>,
    overwrite: Vec<String>,
    cancellation: RunCancellation,
) -> Result<CodeOutcome, Fault> {
    let CodeRestore {
        context,
        loaded,
        restore,
        label,
    } = request;
    let thread_id = saved.thread;
    let preview = selected_preview(context.engine, loaded, saved, kept, overwrite).await?;
    if !preview.can_restore() {
        return Err(Fault::state(Reason::RewindBlocked));
    }
    let count = preview.plan().files.len();
    let rewind_id = preview.rewind_id();
    let publication = if restore == RewindRestore::Both {
        context.engine.rewind_both(preview, cancellation).await
    } else {
        context
            .engine
            .rewind(preview, cancellation)
            .await
            .map(|code| arany::RewindBothResult {
                code,
                conversation: ConversationRewindOutcome::Unchanged,
            })
    };
    match publication {
        Ok(published) => Ok(CodeOutcome::Published(
            wire_receipt(&published.code),
            display::receipt(&published.code, count, label),
            published.conversation,
        )),
        Err(error) => {
            let replayed = threads::load_view(context.state, thread_id).await;
            let Ok(view) = replayed else {
                return Err(Fault::engine(error));
            };
            let Some(operation) = view
                .rewinds
                .operations
                .iter()
                .find(|op| op.started.rewind_id == rewind_id)
            else {
                return Err(Fault::engine(error));
            };
            Ok(recovered(operation, rewind_id, count, label, restore))
        }
    }
}

// Without a file selection the saved preview publishes as is; a selection must observe the same files.
async fn selected_preview(
    engine: &Engine,
    loaded: &threads::LoadedThread,
    saved: Pending,
    kept: Vec<String>,
    overwrite: Vec<String>,
) -> Result<RewindPreview, Fault> {
    let original = saved
        .code
        .ok_or_else(|| Fault::state(Reason::RewindBlocked))?;
    if kept.is_empty() && overwrite.is_empty() {
        return Ok(original);
    }
    let selected = engine
        .rewind_preview(RewindRequest {
            session_id: saved.thread,
            workspace: loaded.workspace.clone(),
            target: saved.target,
            tools: loaded.grant.clone(),
            kept,
            overwrite,
        })
        .await
        .map_err(Fault::engine)?;
    if !selected.observations_match(&original) {
        return Err(Fault::state(Reason::PreviewChanged));
    }
    Ok(selected)
}

fn recovered(
    operation: &rewind::RewindView,
    rewind_id: uuid::Uuid,
    count: usize,
    label: &str,
    restore: RewindRestore,
) -> CodeOutcome {
    let mut notices = Vec::new();
    let receipt = if let Some(finished) = &operation.finished {
        notices.push(display::receipt(finished, count, label));
        wire_receipt(finished)
    } else {
        notices.push("Rewind outcome unknown: inspect files before continuing".into());
        RewindReceipt {
            id: Some(rewind_id.to_string()),
            outcome: RewindResultOutcome::Uncertain,
            conflicts: Vec::new(),
            conflict_count: 0,
        }
    };
    if restore == RewindRestore::Both {
        notices.push("Conversation unchanged; restore conversation separately".into());
    }
    CodeOutcome::Recovered(receipt, notices)
}

fn wire_receipt(finished: &rewind::RewindFinished) -> RewindReceipt {
    RewindReceipt {
        id: Some(finished.rewind_id.to_string()),
        outcome: match finished.outcome {
            rewind::RewindOutcome::Restored => RewindResultOutcome::Restored,
            rewind::RewindOutcome::Conflict => RewindResultOutcome::Conflict,
            rewind::RewindOutcome::Denied => RewindResultOutcome::Denied,
            rewind::RewindOutcome::Failed => RewindResultOutcome::Failed,
            rewind::RewindOutcome::Cancelled => RewindResultOutcome::Cancelled,
            rewind::RewindOutcome::Uncertain => RewindResultOutcome::Uncertain,
        },
        conflicts: finished.conflicts.clone(),
        conflict_count: finished.conflict_count,
    }
}
