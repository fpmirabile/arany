use super::input::WorkspaceInputs;
use super::{
    CollaborationPolicy, CompactionStatus, ContextUsage, MAX_HISTORY_RUNS, RunId, RunStatus,
    SessionView, content_ceiling,
};
use crate::provider::{
    HistoryTurn, ImageAttachment, ImageOrigin, MAX_IMAGE_BYTES, MAX_MESSAGE_IMAGES, ProviderImage,
};

const MAX_CHILD_CONTEXT_BYTES: usize = 20 * 1024 + 64;
const SUMMARY_CONTEXT_OVERHEAD_BYTES: usize = 64;
pub(crate) const HISTORY_TURN_OVERHEAD_BYTES: usize = 64;

fn summary_context_cost(content_bytes: usize, existing_summary: bool) -> Option<usize> {
    content_bytes.checked_add(if existing_summary {
        super::COMPACTION_SUMMARY_SEPARATOR_BYTES
    } else {
        SUMMARY_CONTEXT_OVERHEAD_BYTES
    })
}

fn fits_history_budget(used: usize, addition: Option<usize>, budget: usize) -> bool {
    addition
        .and_then(|addition| used.checked_add(addition))
        .is_some_and(|total| total <= budget)
}

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
        let budget = remaining(
            ceiling,
            [
                child_reserved,
                tool_reserved.min(crate::tools::types::MIN_TOOL_CONTEXT_BYTES),
                goal_metadata_reserved,
            ],
        )?;
        let mut used = input_bytes(objective, objective_images, inputs);
        if used > budget {
            return Err(ContextTooLarge);
        }
        let base_bytes = used;
        // Optional history must leave room for a continuation; explicit inputs still use the admission budget.
        let history_budget = ceiling
            .saturating_sub(child_reserved)
            .saturating_sub(goal_metadata_reserved)
            .saturating_sub(tool_reserved)
            .max(base_bytes)
            .min(budget);
        let CompactedHistory {
            covered_sequence,
            event_sequence: compaction_event_sequence,
            content_digest: compaction_content_digest,
            mut summary,
            files: compacted_files,
        } = compacted_history(session, &mut used, budget, history_budget)?;
        let runs = session.map_or(&[][..], |view| view.runs.as_slice());
        let mut window = HistoryWindow::new(used, history_budget, objective_images);
        let selected = window.select(runs, covered_sequence)?;
        used = window.used;
        let excluded_history_runs =
            u32::try_from(runs.len().saturating_sub(selected.len())).unwrap_or(u32::MAX);
        let files = compacted_files
            .into_iter()
            .chain(recent_run_files(session, covered_sequence));
        append_recent_files(&mut summary, &mut used, files, history_budget)?;
        let images = context_images(&selected, objective_images, window.image_count);
        let tool_history_bytes = HistoryWindow::tool_bytes(&selected);
        let (history_run_ids, history) = selected
            .into_iter()
            .map(|(run, turn)| (run.id, turn))
            .unzip();
        let tool_context_budget_bytes = if tool_reserved == 0 {
            0
        } else {
            remaining(ceiling, [child_reserved, used, goal_metadata_reserved])?
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

fn remaining(ceiling: usize, reservations: [usize; 3]) -> Result<usize, ContextTooLarge> {
    reservations
        .into_iter()
        .try_fold(ceiling, usize::checked_sub)
        .ok_or(ContextTooLarge)
}

fn input_bytes(
    objective: &str,
    objective_images: &[ImageAttachment],
    inputs: &WorkspaceInputs,
) -> usize {
    objective.len()
        + objective_images
            .iter()
            .map(ImageAttachment::context_bytes)
            .sum::<usize>()
        + inputs
            .user_instructions
            .iter()
            .chain(inputs.instructions.iter())
            .map(|value| value.model_instructions().len())
            .sum::<usize>()
        + inputs
            .includes
            .iter()
            .map(|value| value.model_include().len())
            .sum::<usize>()
}

// The event sequence, covered sequence, digest, summary and recent files of a compaction.
type SelectedCompaction = (u64, u64, [u8; 32], String, Option<String>);

struct CompactedHistory {
    covered_sequence: u64,
    event_sequence: Option<u64>,
    content_digest: Option<[u8; 32]>,
    summary: Option<String>,
    files: Option<String>,
}

fn compacted_history(
    session: Option<&SessionView>,
    used: &mut usize,
    budget: usize,
    history_budget: usize,
) -> Result<CompactedHistory, ContextTooLarge> {
    let selected_compaction = latest_compaction(session, *used, history_budget);
    let covered_sequence = selected_compaction.as_ref().map_or(0, |value| value.1);
    let (event_sequence, content_digest, mut summary, files) =
        if let Some((event_sequence, _, digest, summary, files)) = selected_compaction {
            *used += summary.len() + SUMMARY_CONTEXT_OVERHEAD_BYTES;
            (Some(event_sequence), Some(digest), Some(summary), files)
        } else {
            (None, None, None, None)
        };
    if let Some(facts) =
        session.and_then(|v| super::rewind::history_facts(v, covered_sequence, v.last_sequence))
    {
        *used += facts.len() + usize::from(summary.is_some()) + SUMMARY_CONTEXT_OVERHEAD_BYTES;
        if *used > budget {
            return Err(ContextTooLarge);
        }
        if let Some(summary) = &mut summary {
            summary.push('\n');
            summary.push_str(&facts);
        } else {
            summary = Some(facts);
        }
    }
    Ok(CompactedHistory {
        covered_sequence,
        event_sequence,
        content_digest,
        summary,
        files,
    })
}

fn latest_compaction(
    session: Option<&SessionView>,
    used: usize,
    history_budget: usize,
) -> Option<SelectedCompaction> {
    session
        .into_iter()
        .flat_map(|view| view.compactions.iter().rev())
        .find_map(|compaction| {
            if compaction.record.compiler_version == super::COMPACTION_COMPILER_VERSION
                && let CompactionStatus::Succeeded {
                    summary,
                    content_digest,
                    ..
                } = &compaction.record.status
                && fits_history_budget(
                    used,
                    summary_context_cost(summary.len(), false),
                    history_budget,
                )
            {
                return Some((
                    compaction.event_sequence,
                    compaction.record.covered_sequence,
                    *content_digest,
                    summary.clone(),
                    super::render_recent_files(&compaction.record.recent_files),
                ));
            }
            None
        })
}

struct HistoryWindow {
    used: usize,
    limit: usize,
    image_count: usize,
    image_bytes: usize,
}

impl HistoryWindow {
    fn new(used: usize, limit: usize, objective_images: &[ImageAttachment]) -> Self {
        Self {
            used,
            limit,
            image_count: objective_images.len(),
            image_bytes: objective_images
                .iter()
                .map(ImageAttachment::byte_len)
                .sum::<usize>(),
        }
    }

    fn tool_bytes(selected: &[(&super::RunView, HistoryTurn)]) -> u32 {
        selected
            .iter()
            .filter(|(run, _)| !run.model_tools().is_empty())
            .map(|(_, turn)| turn.assistant.len())
            .sum::<usize>() as u32
    }

    fn select<'a>(
        &mut self,
        runs: &'a [super::RunView],
        covered_sequence: u64,
    ) -> Result<Vec<(&'a super::RunView, HistoryTurn)>, ContextTooLarge> {
        let mut selected = Vec::new();
        for run in runs.iter().rev() {
            if selected.len() == MAX_HISTORY_RUNS {
                break;
            }
            if run.accepted_sequence <= covered_sequence {
                continue;
            }
            let assistant = history_assistant(run)?;
            let user = run.history_user_text();
            let size = user.len()
                + assistant.len()
                + HISTORY_TURN_OVERHEAD_BYTES
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
            if self.used + size > self.limit
                || self.image_count + run.images.len() > MAX_MESSAGE_IMAGES
                || self.image_bytes + turn_image_bytes > MAX_IMAGE_BYTES
            {
                break;
            }
            self.used += size;
            self.image_count += run.images.len();
            self.image_bytes += turn_image_bytes;
            selected.push((run, HistoryTurn { user, assistant }));
        }
        selected.reverse();
        Ok(selected)
    }
}

fn history_assistant(run: &super::RunView) -> Result<String, ContextTooLarge> {
    let refusal = crate::provider::history_refusal_block(run);
    let plain = run.model_tools().is_empty() && run.worker_history().is_empty();
    Ok(
        if plain && run.status == RunStatus::Finished && refusal.is_none() {
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
        },
    )
}

fn append_recent_files(
    summary: &mut Option<String>,
    used: &mut usize,
    files: impl Iterator<Item = String>,
    history_budget: usize,
) -> Result<(), ContextTooLarge> {
    for files in files {
        let cost = summary_context_cost(files.len(), summary.is_some());
        if !fits_history_budget(*used, cost, history_budget) {
            continue;
        }
        *used += cost.ok_or(ContextTooLarge)?;
        *summary = Some(super::join_recent_files(summary.take(), files));
    }
    Ok(())
}

fn recent_run_files(session: Option<&SessionView>, covered_sequence: u64) -> Option<String> {
    session
        .into_iter()
        .flat_map(|view| view.runs.iter().rev())
        .find(|run| {
            run.accepted_sequence > covered_sequence && !run.tool_compaction_recent_files.is_empty()
        })
        .and_then(|run| super::render_recent_files(&run.tool_compaction_recent_files))
}

fn context_images(
    selected: &[(&super::RunView, HistoryTurn)],
    objective_images: &[ImageAttachment],
    image_count: usize,
) -> Vec<ProviderImage> {
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
    images
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_admission_counts_the_recent_file_join_exactly() {
        let summary_bytes = 17;
        let recent_files_bytes = 31;
        let cost = summary_context_cost(summary_bytes, false).unwrap()
            + summary_context_cost(recent_files_bytes, true).unwrap();
        let joined = super::super::join_recent_files(
            Some("s".repeat(summary_bytes)),
            "f".repeat(recent_files_bytes),
        );
        assert_eq!(cost, joined.len() + SUMMARY_CONTEXT_OVERHEAD_BYTES);
        assert!(fits_history_budget(7, Some(cost), 7 + cost));
        assert!(!fits_history_budget(7, Some(cost), 6 + cost));
        assert_eq!(
            summary_context_cost(recent_files_bytes, false),
            Some(recent_files_bytes + SUMMARY_CONTEXT_OVERHEAD_BYTES)
        );
    }
}
