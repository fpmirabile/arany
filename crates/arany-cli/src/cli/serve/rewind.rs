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

pub(super) async fn preview(
    engine: &Engine,
    state: &Path,
    threads: &Threads,
    input: ThreadRewindPreviewParams,
) -> Result<(ThreadRewindPreviewResult, Pending), Fault> {
    let loaded = threads
        .map
        .get(&input.thread_id)
        .ok_or_else(|| Fault::state(Reason::ThreadNotLoaded))?;
    let view = threads::load_view(state, input.thread_id).await?;
    let target = view
        .rewinds
        .checkpoints
        .iter()
        .find(|target| match target {
            CheckpointTarget::Run { run_id, .. } => *run_id == input.turn_id,
            CheckpointTarget::Rewind { rewind_id, .. } => {
                rewind_id.to_string() == input.turn_id.to_string()
            }
        })
        .cloned()
        .ok_or_else(Fault::params)?;
    let plan =
        rewind::plan(&view, &target).map_err(|_| Fault::engine(EngineError::RewindTooLarge))?;
    let mut blockers = Vec::new();
    if !plan.blockers.is_empty() {
        blockers.push(RewindBlocker::UncertainPublication);
    }
    let code = if matches!(loaded.grant, ToolGrant::None) {
        blockers.push(RewindBlocker::NoWriteGrant);
        None
    } else {
        match engine
            .rewind_preview(RewindRequest {
                session_id: input.thread_id,
                workspace: loaded.workspace.clone(),
                target: target.clone(),
                tools: loaded.grant.clone(),
                kept: Vec::new(),
                overwrite: Vec::new(),
            })
            .await
        {
            Ok(preview) => Some(preview),
            Err(EngineError::GuardUnavailable) => {
                blockers.push(RewindBlocker::GuardUnavailable);
                None
            }
            Err(EngineError::RewindBlocked) => {
                blockers.push(RewindBlocker::NoWriteGrant);
                None
            }
            Err(error) => return Err(Fault::engine(error)),
        }
    };
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
    let checkpoint = match &target {
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
    };
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
            non_rewindable: plan
                .non_rewindable
                .iter()
                .map(|(sequence, effect)| RewindNonRewindable {
                    sequence: *sequence,
                    effect: match effect {
                        NonRewindable::Host => RewindEffect::Host,
                        NonRewindable::UnguardedCommand => RewindEffect::UnguardedCommand,
                        NonRewindable::Mcp => RewindEffect::Mcp,
                        NonRewindable::Cache => RewindEffect::Cache,
                        NonRewindable::Directory => RewindEffect::Directory,
                    },
                })
                .collect(),
            preview_token: token,
        },
        pending,
    ))
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
    engine: &Engine,
    state: &Path,
    threads: &mut Threads,
    pending: &mut Option<Pending>,
    input: ThreadRewindParams,
    cancellation: RunCancellation,
) -> Result<ThreadRewindResult, Fault> {
    let loaded = threads
        .map
        .get(&input.thread_id)
        .ok_or_else(|| Fault::state(Reason::ThreadNotLoaded))?
        .clone();
    if threads.approval_channel != super::server::ApprovalChannel::Human {
        return Err(Fault::state(Reason::HumanConfirmationRequired));
    }
    if input.kept.len() + input.overwrite.len() > 64
        || input.kept.iter().any(|p| input.overwrite.contains(p))
    {
        return Err(Fault::params());
    }
    if !input.overwrite.is_empty() && !input.confirm_overwrite {
        return Err(Fault::state(Reason::OverwriteConfirmationRequired));
    }
    let saved = pending
        .as_ref()
        .filter(|p| p.thread == input.thread_id && p.token == input.preview_token)
        .ok_or_else(|| Fault::state(Reason::PreviewChanged))?;
    let view = threads::load_view(state, input.thread_id).await?;
    if saved.head != view.last_sequence || saved.prefix != view.rewinds.prefix_digest() {
        pending.take();
        return Err(Fault::state(Reason::PreviewChanged));
    }
    if input.restore != RewindRestore::Code {
        if !matches!(saved.target, CheckpointTarget::Run { .. }) {
            return Err(Fault::params());
        }
        threads.check_capacity(None)?;
    }
    if input.restore == RewindRestore::Conversation
        && (!input.kept.is_empty() || !input.overwrite.is_empty() || input.confirm_overwrite)
    {
        return Err(Fault::params());
    }
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
        let CheckpointTarget::Run { run_id, .. } = saved.target else {
            return Err(Fault::params());
        };
        ConversationRewindOutcome::Restored(Box::new(
            engine
                .rewind_conversation_at_head(
                    input.thread_id,
                    loaded.workspace.clone(),
                    run_id,
                    (saved.head, saved.prefix),
                )
                .await
                .map_err(Fault::engine)?,
        ))
    } else {
        let original = saved
            .code
            .ok_or_else(|| Fault::state(Reason::RewindBlocked))?;
        let preview = if input.kept.is_empty() && input.overwrite.is_empty() {
            original
        } else {
            let selected = engine
                .rewind_preview(RewindRequest {
                    session_id: input.thread_id,
                    workspace: loaded.workspace.clone(),
                    target: saved.target,
                    tools: loaded.grant.clone(),
                    kept: input.kept,
                    overwrite: input.overwrite,
                })
                .await
                .map_err(Fault::engine)?;
            if !selected.observations_match(&original) {
                return Err(Fault::state(Reason::PreviewChanged));
            }
            selected
        };
        if !preview.can_restore() {
            return Err(Fault::state(Reason::RewindBlocked));
        }
        let count = preview.plan().files.len();
        let rewind_id = preview.rewind_id();
        let publication = if input.restore == RewindRestore::Both {
            engine.rewind_both(preview, cancellation).await
        } else {
            engine
                .rewind(preview, cancellation)
                .await
                .map(|code| arany::RewindBothResult {
                    code,
                    conversation: ConversationRewindOutcome::Unchanged,
                })
        };
        let (finished, conversation) = match publication {
            Ok(published) => (published.code, published.conversation),
            Err(error) => {
                let replayed = threads::load_view(state, input.thread_id).await;
                if let Ok(view) = replayed
                    && let Some(operation) = view
                        .rewinds
                        .operations
                        .iter()
                        .find(|op| op.started.rewind_id == rewind_id)
                {
                    if let Some(finished) = &operation.finished {
                        result.rewind = wire_receipt(finished);
                        result
                            .notices
                            .push(display::receipt(finished, count, &label));
                    } else {
                        result.rewind = RewindReceipt {
                            id: Some(rewind_id.to_string()),
                            outcome: RewindResultOutcome::Uncertain,
                            conflicts: Vec::new(),
                            conflict_count: 0,
                        };
                        result
                            .notices
                            .push("Rewind outcome unknown: inspect files before continuing".into());
                    }
                    if input.restore == RewindRestore::Both {
                        result
                            .notices
                            .push("Conversation unchanged; restore conversation separately".into());
                    }
                    return Ok(result);
                }
                return Err(Fault::engine(error));
            }
        };
        result
            .notices
            .push(display::receipt(&finished, count, &label));
        result.rewind = wire_receipt(&finished);
        conversation
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
            if conversation.images_omitted { result.notices.push("Images were not copied; attach them again".into()); }
            result.conversation = Some(RewindConversation { thread_id: conversation.session_id, prompt: conversation.prompt, images_omitted: conversation.images_omitted });
        }
        ConversationRewindOutcome::Failed(_) => result.notices.push("Code restored; conversation unchanged; use thread/rewind/preview and Restore conversation".into()),
        ConversationRewindOutcome::Unchanged if input.restore == RewindRestore::Both && result.rewind.outcome == RewindResultOutcome::Restored => result.notices.push("Code restored; conversation unchanged".into()),
        _ => {}
    }
    Ok(result)
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
