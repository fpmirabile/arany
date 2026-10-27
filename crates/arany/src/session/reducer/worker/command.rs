use super::*;

pub(super) fn admissible(
    run: &RunView,
    agent: &AgentView,
    intent: &crate::tools::EffectIntent,
) -> bool {
    let Some(worker) = &agent.assignment else {
        return false;
    };
    let originals = agent
        .tools
        .iter()
        .filter(|tool| !matches!(tool.intent.call, ToolCall::ApplyChangeset { .. }))
        .count();
    match &intent.call {
        ToolCall::ApplyChangeset {
            source: ChangesetSource::CommandReceipt { .. },
        } => super::super::tool::valid_integration_source(run, intent),
        _ => {
            super::super::agent::child_tool_requests(agent) == originals + 1
                && worker.pins.grant.allows(&intent.call)
        }
    }
}

pub(super) fn criterion_command(
    agent: &AgentView,
    tool: &ToolView,
    call: &ToolCall,
) -> Option<u64> {
    let ToolCall::ApplyChangeset {
        source:
            ChangesetSource::CommandReceipt {
                source_intent_digest,
                ..
            },
    } = &tool.intent.call
    else {
        return None;
    };
    agent.tools.iter().find_map(|source| {
        let observation = source.observation.as_ref()?;
        (source.intent.digest() == *source_intent_digest
            && source.intent.call == *call
            && observation.disposition == ToolDisposition::Succeeded
            && observation.attested_for(&source.intent)
            && succeeded(observation, false))
        .then_some(source.finished_sequence?)
    })
}

pub(super) fn succeeded(observation: &crate::tools::ToolObservation, empty: bool) -> bool {
    serde_json::from_str::<crate::tools::types::CommandResult>(&observation.output).is_ok_and(
        |result| {
            result.valid()
                && result.exit_code == Some(0)
                && (!empty || result.changeset.files.is_empty())
        },
    )
}
