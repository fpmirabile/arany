use super::input::WorkspaceInputs;
use super::{
    CollaborationPolicy, CompactionStatus, ContextUsage, MAX_CONTEXT_CONTENT_BYTES,
    MAX_HISTORY_RUNS, RunId, RunStatus, SessionView,
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
}

impl CompiledContext {
    pub(crate) fn compile(
        session: Option<&SessionView>,
        objective: &str,
        objective_images: &[ImageAttachment],
        inputs: &WorkspaceInputs,
        policy: CollaborationPolicy,
        tool_reserved: usize,
    ) -> Result<Self, ContextTooLarge> {
        let reserved = usize::from(policy.max_children()) * MAX_CHILD_CONTEXT_BYTES;
        let budget = MAX_CONTEXT_CONTENT_BYTES
            .checked_sub(reserved)
            .and_then(|budget| budget.checked_sub(tool_reserved))
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
                .map(|value| value.content.len())
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
        let (compaction_event_sequence, compaction_content_digest, summary) =
            if let Some((event_sequence, _, digest, summary)) = selected_compaction {
                used += summary.len() + 64;
                (Some(event_sequence), Some(digest), Some(summary))
            } else {
                (None, None, None)
            };
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
            if run.status != RunStatus::Finished && run.tools.is_empty() {
                continue;
            }
            if run
                .finished_sequence
                .is_some_and(|sequence| sequence <= covered_sequence)
            {
                continue;
            }
            let assistant = if run.tools.is_empty() {
                run.assistant_message.clone().ok_or(ContextTooLarge)?
            } else {
                let mut context = run.tool_context();
                if let Some(answer) = &run.assistant_message {
                    context.push_str("\nFinal answer (untrusted data):\n");
                    context.push_str(answer);
                }
                context
            };
            let size = run.objective.len()
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
            selected.push((
                run,
                HistoryTurn {
                    user: run.objective.clone(),
                    assistant,
                },
            ));
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
            .filter(|(run, _)| !run.tools.is_empty())
            .map(|(_, turn)| turn.assistant.len())
            .sum::<usize>() as u32;
        let (history_run_ids, history) = selected
            .into_iter()
            .map(|(run, turn)| (run.id, turn))
            .unzip();
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
        })
    }
}
