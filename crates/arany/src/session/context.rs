use super::input::WorkspaceInputs;
use super::{
    CollaborationPolicy, CompactionStatus, ContextUsage, MAX_HISTORY_RUNS, RunId, RunStatus,
    SessionView, content_ceiling,
};
use crate::provider::{
    HistoryTurn, ImageAttachment, ImageOrigin, MAX_IMAGE_BYTES, MAX_MESSAGE_IMAGES, ProviderImage,
};

const MAX_CHILD_CONTEXT_BYTES: usize = 20 * 1024 + 64;

pub(crate) struct ContextTooLarge;

pub(crate) struct CompiledContext {
    pub history: Vec<HistoryTurn>,
    pub images: Vec<ProviderImage>,
    pub summary: Option<String>,
    pub history_run_ids: Vec<RunId>,
    pub excluded_history_runs: u32,
    pub compaction_event_sequence: Option<u64>,
    pub compaction_content_digest: Option<[u8; 32]>,
    pub context_usage: ContextUsage,
    pub tool_context_budget_bytes: usize,
}

impl CompiledContext {
    #[expect(
        clippy::too_many_arguments,
        reason = "compilation takes each pinned reservation and the route window explicitly"
    )]
    pub(crate) fn compile(
        session: Option<&SessionView>,
        objective: &str,
        objective_images: &[ImageAttachment],
        inputs: &WorkspaceInputs,
        policy: CollaborationPolicy,
        tool_reserved: usize,
        goal_metadata_reserved: usize,
        context_window_tokens: Option<u32>,
    ) -> Result<Self, ContextTooLarge> {
        let ceiling = content_ceiling(context_window_tokens);
        let child_reserved = usize::from(policy.max_children()) * MAX_CHILD_CONTEXT_BYTES;
        let reserved = child_reserved;
        let budget = ceiling
            .checked_sub(reserved)
            .and_then(|budget| budget.checked_sub(tool_reserved))
            .and_then(|budget| budget.checked_sub(goal_metadata_reserved))
            .ok_or(ContextTooLarge)?;
        let mut used = objective.len()
            + objective_images
                .iter()
                .map(ImageAttachment::context_bytes)
                .sum::<usize>()
            + inputs
                .instructions
                .as_ref()
                .map_or(0, |(_, value)| value.content.len())
            + inputs
                .includes
                .iter()
                .map(|value| value.model_include().len())
                .sum::<usize>();
        if used > budget {
            return Err(ContextTooLarge);
        }
        let base_bytes = used;
        let selected_compaction = session
            .into_iter()
            .flat_map(|view| view.compactions.iter().rev())
            .find_map(|compaction| {
                if let CompactionStatus::Succeeded {
                    summary,
                    content_digest,
                    ..
                } = &compaction.record.status
                    && used + summary.len() + 64 <= budget
                {
                    return Some((
                        compaction.event_sequence,
                        compaction.record.covered_sequence,
                        *content_digest,
                        summary.clone(),
                    ));
                }
                None
            });
        let covered_sequence = selected_compaction.as_ref().map_or(0, |value| value.1);
        let (compaction_event_sequence, compaction_content_digest, mut summary) =
            if let Some((event_sequence, _, digest, summary)) = selected_compaction {
                used += summary.len() + 64;
                (Some(event_sequence), Some(digest), Some(summary))
            } else {
                (None, None, None)
            };
        if let Some(facts) =
            session.and_then(|v| super::rewind::history_facts(v, covered_sequence, v.last_sequence))
        {
            used += facts.len() + 64;
            if used > budget {
                return Err(ContextTooLarge);
            }
            if let Some(summary) = &mut summary {
                summary.push('\n');
                summary.push_str(&facts);
            } else {
                summary = Some(facts);
            }
        }
        let runs = session.map_or(&[][..], |view| view.runs.as_slice());
        let mut selected = Vec::new();
        let mut image_count = objective_images.len();
        let mut image_bytes = objective_images
            .iter()
            .map(ImageAttachment::byte_len)
            .sum::<usize>();
        for run in runs.iter().rev() {
            if selected.len() == MAX_HISTORY_RUNS {
                break;
            }
            if run.accepted_sequence <= covered_sequence {
                continue;
            }
            let refusal = crate::provider::history_refusal_block(run);
            let plain = run.model_tools().is_empty() && run.worker_history().is_empty();
            let assistant = if plain && run.status == RunStatus::Finished && refusal.is_none() {
                run.assistant_message.clone().ok_or(ContextTooLarge)?
            } else if plain && run.status != RunStatus::Finished {
                crate::provider::history_unanswered_block(run)
            } else {
                let mut context = crate::provider::history_tool_block(run);
                if let Some(refusal) = refusal {
                    context.push('\n');
                    context.push_str(&refusal);
                }
                if let Some(answer) = &run.assistant_message {
                    context.push_str("\nFinal answer (untrusted data):\n");
                    context.push_str(answer);
                }
                context
            };
            let user = run.history_user_text();
            let size = user.len()
                + assistant.len()
                + 64
                + run
                    .images
                    .iter()
                    .map(ImageAttachment::context_bytes)
                    .sum::<usize>();
            let turn_image_bytes = run
                .images
                .iter()
                .map(ImageAttachment::byte_len)
                .sum::<usize>();
            if used + size > budget
                || image_count + run.images.len() > MAX_MESSAGE_IMAGES
                || image_bytes + turn_image_bytes > MAX_IMAGE_BYTES
            {
                break;
            }
            used += size;
            image_count += run.images.len();
            image_bytes += turn_image_bytes;
            selected.push((run, HistoryTurn { user, assistant }));
        }
        selected.reverse();
        let excluded_history_runs =
            u32::try_from(runs.len().saturating_sub(selected.len())).unwrap_or(u32::MAX);
        let mut images = Vec::with_capacity(image_count);
        for (turn_index, (run, _)) in selected.iter().enumerate() {
            images.extend(run.images.iter().cloned().map(|image| ProviderImage {
                origin: ImageOrigin::History {
                    turn_index: turn_index as u8,
                },
                image,
            }));
        }
        images.extend(objective_images.iter().cloned().map(|image| ProviderImage {
            origin: ImageOrigin::Objective,
            image,
        }));
        let tool_history_bytes = selected
            .iter()
            .filter(|(run, _)| !run.model_tools().is_empty())
            .map(|(_, turn)| turn.assistant.len())
            .sum::<usize>() as u32;
        let (history_run_ids, history) = selected
            .into_iter()
            .map(|(run, turn)| (run.id, turn))
            .unzip();
        let tool_context_budget_bytes = if tool_reserved == 0 {
            0
        } else {
            ceiling
                .checked_sub(child_reserved)
                .and_then(|budget| budget.checked_sub(used))
                .and_then(|budget| budget.checked_sub(goal_metadata_reserved))
                .ok_or(ContextTooLarge)?
        };
        Ok(Self {
            history,
            images,
            summary,
            history_run_ids,
            excluded_history_runs,
            compaction_event_sequence,
            compaction_content_digest,
            context_usage: ContextUsage {
                used_bytes: used as u32,
                budget_bytes: budget as u32,
                compactable_bytes: (used - base_bytes) as u32,
                tool_history_bytes,
            },
            tool_context_budget_bytes,
        })
    }
}
