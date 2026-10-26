use super::super::test_support::*;
use super::super::*;
use crate::provider::OutputTokenBound;
use crate::session::events::CollaborationPolicy;

#[test]
fn a_forks_boundary_compaction_is_refused_while_its_own_run_is_open() {
    use crate::session::events::{CompactionFailure, CompactionStatus};
    let (fork, source, source_run, run, agent) = (
        SessionId::new(),
        SessionId::new(),
        RunId::new(),
        RunId::new(),
        AgentRunId::new(),
    );
    let mut events = vec![Event::SessionForked {
        title: "Fork".into(),
        source_session_id: source,
        source_run_id: source_run,
        source_sequence: 1,
        prefix_digest: [0; 32],
    }];
    let goal = GoalId::new();
    events.push(goal_started(goal, GoalBudget::default()));
    events.extend(start_run(
        goal,
        run,
        agent,
        config(CollaborationPolicy::Single, false),
    ));
    let open = journal(fork, events.clone());
    assert!(SessionView::replay_open(fork, &open).unwrap().is_some());
    events.push(Event::ContextCompacted {
        record: crate::session::events::CompactionRecord {
            rate_limits: None,
            covered_run_id: source_run,
            covered_sequence: 1,
            source_digest: [0; 32],
            recent_files: Vec::new(),
            provider: "openai".into(),
            model: "gpt-5.4".into(),
            prompt_version: 1,
            schema_version: 1,
            compiler_version: crate::session::COMPACTION_COMPILER_VERSION,
            source_bytes: 1,
            source_tokens_estimate: 1,
            input_tokens: None,
            output_tokens: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            response_id: None,
            wire_provenance: None,
            chatgpt_provenance: None,
            output_token_bound: OutputTokenBound::ProviderEnforced,
            status: CompactionStatus::Failed {
                reason: CompactionFailure::ProviderUnavailable,
            },
        },
        active_ms: None,
    });
    assert!(SessionView::replay_open(fork, &journal(fork, events)).is_err());
}
