use super::{RunId, RunView, SessionView};
use crate::session::worker::{ChangesetSource, EffectTarget};
use crate::tools::{EffectIntent, ToolCall, ToolDisposition};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointStatus {
    Captured,
    NotNeeded,
    Skipped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointReason {
    Quota,
    TooLarge,
    StoreUnavailable,
    DigestMismatch,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub status: CheckpointStatus,
    #[serde(deserialize_with = "Option::deserialize")]
    pub reason: Option<CheckpointReason>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub pruned_before_run: Option<RunId>,
}

impl Checkpoint {
    pub(crate) fn not_needed() -> Self {
        Self {
            status: CheckpointStatus::NotNeeded,
            reason: None,
            pruned_before_run: None,
        }
    }

    pub(crate) fn skipped(reason: CheckpointReason) -> Self {
        Self {
            status: CheckpointStatus::Skipped,
            reason: Some(reason),
            pruned_before_run: None,
        }
    }

    pub fn notice(&self) -> Option<&'static str> {
        (self.status == CheckpointStatus::Skipped).then_some(match self.reason {
            Some(CheckpointReason::Quota) => {
                "This change cannot be rewound (checkpoint storage full)"
            }
            Some(CheckpointReason::TooLarge) => {
                "This change cannot be rewound (pre-image too large)"
            }
            Some(CheckpointReason::DigestMismatch) => {
                "This change cannot be rewound (file changed before capture)"
            }
            _ => "This change cannot be rewound (checkpoint storage unavailable)",
        })
    }

    pub(crate) fn valid(&self) -> bool {
        (self.status == CheckpointStatus::Skipped) == self.reason.is_some()
    }
}

pub(crate) fn publication(intent: &EffectIntent) -> bool {
    intent.target == EffectTarget::Workspace
        && matches!(
            intent.call,
            ToolCall::Write { .. }
                | ToolCall::Edit { .. }
                | ToolCall::Delete { .. }
                | ToolCall::Move { .. }
                | ToolCall::MultiEdit { .. }
                | ToolCall::ApplyChangeset { .. }
        )
}

pub(crate) fn before_versions(_run: &RunView, call: &ToolCall) -> Vec<(String, Option<String>)> {
    match call {
        ToolCall::Write {
            path,
            expected_digest,
            ..
        } => vec![(path.clone(), expected_digest.clone())],
        ToolCall::Delete {
            path,
            expected_digest,
        } => vec![(path.clone(), expected_digest.clone())],
        ToolCall::Move {
            source,
            destination,
            expected_digest,
        } => vec![
            (source.clone(), expected_digest.clone()),
            (destination.clone(), None),
        ],
        ToolCall::Edit {
            path,
            expected_digest,
            ..
        }
        | ToolCall::MultiEdit {
            file_path: path,
            expected_digest,
            ..
        } => vec![(path.clone(), expected_digest.clone())],
        ToolCall::ApplyChangeset {
            source: ChangesetSource::CommandReceipt { changeset, .. },
        } => changeset
            .files
            .iter()
            .map(|f| (f.path.clone(), f.before.as_ref().map(|v| v.sha256.clone())))
            .collect(),
        _ => Vec::new(),
    }
}

pub(crate) fn capture_needed(view: &SessionView, run_id: RunId, path: &str, before: &str) -> bool {
    let Some(run) = view.runs.iter().find(|r| r.id == run_id) else {
        return true;
    };
    let previous = run
        .tools
        .iter()
        .chain(run.agents.iter().flat_map(|a| &a.tools))
        .filter(|t| t.finished_sequence.is_some())
        .collect::<Vec<_>>();
    let mut previous = previous;
    previous.sort_by_key(|t| t.finished_sequence);
    let previous = previous
        .into_iter()
        .rev()
        .filter_map(|tool| tool.observation.as_ref())
        .filter(|o| o.disposition == ToolDisposition::Succeeded && publication(&o.intent))
        .find_map(|o| match &o.intent.call {
            ToolCall::Write { path: p, .. }
            | ToolCall::Edit { path: p, .. }
            | ToolCall::MultiEdit { file_path: p, .. }
                if p == path =>
            {
                let output: serde_json::Value = serde_json::from_str(&o.output).ok()?;
                Some(
                    output
                        .get("sha256")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned),
                )
            }
            ToolCall::Delete { path: source, .. } if source == path => Some(None),
            ToolCall::Move {
                source,
                destination,
                expected_digest,
            } => {
                if source == path {
                    Some(None)
                } else if destination == path {
                    Some(expected_digest.clone())
                } else {
                    None
                }
            }
            ToolCall::ApplyChangeset {
                source: ChangesetSource::CommandReceipt { changeset, .. },
            } => changeset
                .files
                .iter()
                .find(|f| f.path == path)
                .map(|f| f.after.as_ref().map(|v| v.sha256.clone())),
            _ => None,
        });
    previous != Some(Some(before.to_owned()))
}

pub(crate) fn validate_finished(
    view: &SessionView,
    observation: &crate::tools::ToolObservation,
    checkpoint: Option<&Checkpoint>,
) -> Result<(), super::ReplayError> {
    use super::ReplayError;
    let reached_go = observation.attestation.is_some()
        || matches!(observation.disposition, ToolDisposition::Uncertain);
    if !publication(&observation.intent) || !reached_go {
        return if checkpoint.is_none() {
            Ok(())
        } else {
            Err(ReplayError::InvalidTransition)
        };
    }
    let checkpoint = checkpoint
        .filter(|c| c.valid())
        .ok_or(ReplayError::InvalidTransition)?;
    if checkpoint
        .pruned_before_run
        .is_some_and(|pruned| !view.runs.iter().any(|run| run.id == pruned))
    {
        return Err(ReplayError::InvalidTransition);
    }
    let run = view
        .runs
        .iter()
        .find(|r| r.id == observation.intent.run_id)
        .ok_or(ReplayError::InvalidTransition)?;
    if observation.disposition == ToolDisposition::Succeeded {
        super::rewind::publication_files(run, observation)?;
    }
    let published = matches!(
        observation.disposition,
        ToolDisposition::Succeeded | ToolDisposition::Uncertain
    );
    let before = before_versions(run, &observation.intent.call);
    let needed = before.iter().any(|(path, digest)| {
        digest
            .as_ref()
            .is_some_and(|digest| capture_needed(view, run.id, path, digest))
    });
    match checkpoint.status {
        CheckpointStatus::Captured if needed && published => Ok(()),
        CheckpointStatus::NotNeeded if !needed || !published => Ok(()),
        CheckpointStatus::Skipped if needed && published => Ok(()),
        _ => Err(ReplayError::InvalidTransition),
    }
}
