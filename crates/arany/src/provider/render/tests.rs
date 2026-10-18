use super::*;
use crate::provider::{ChildResult, GoalContextFacts, ProviderTurn, RunInput, ToolUseId};
use crate::session::{AgentRunId, CollaborationPolicy, GoalBudget, RunId};
use crate::tools::ToolRequest;

const CANARIES: [&str; 8] = [
    "guidance canary",
    "include canary",
    "summary canary",
    "history user canary",
    "history assistant canary",
    "child canary",
    "result canary",
    "catalog canary",
];

fn catalog() -> String {
    json!({
        "write": true,
        "commands": [{"name": "cargo"}],
        "skills": [{"name": "guide"}],
        "mcp_servers": [{"name": "local", "tools": ["probe"]}],
        "git": {"operations": ["git_status", "git_diff", "git_log"]},
        "note": "catalog canary",
    })
    .to_string()
}

use crate::provider::test_support::observation;

fn read(path: &str) -> ToolCall {
    ToolCall::Read {
        path: path.into(),
        offset: 0,
        limit: 4096,
    }
}

fn step(id: &str, path: &str) -> ToolStep {
    ToolStep {
        turn: ProviderTurn::scripted(),
        requests: vec![ToolRequest {
            id: ToolUseId::new(id).unwrap(),
            call: read(path),
            plan_contract: None,
        }],
        observations: vec![observation(read(path), "result canary")],
        facts: Some(GoalContextFacts::maximum()),
        steering_notes: Vec::new(),
        calls_remaining: None,
    }
}

fn request(steps: Vec<ToolStep>) -> ProviderRequest {
    ProviderRequest {
        session_id: crate::session::SessionId::new(),
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        phase: AgentPhase::RootPlan,
        collaboration: CollaborationPolicy::Auto {
            max_active_children: 3,
        },
        model: "render-model".into(),
        max_output_tokens: 4096,
        input: RunInput {
            workspace_guidance: Some("guidance canary".into()),
            includes: vec!["include canary".into()],
            context_summary: Some("summary canary".into()),
            history: vec![HistoryTurn {
                user: "history user canary".into(),
                assistant: "history assistant canary".into(),
            }],
            goal_budget: Some(GoalBudget::default()),
            tools: Some(ToolContext {
                catalog: catalog(),
                history_summary: None,
                steps,
            }),
            ..RunInput::new(PromptRole::Primary, "objective".into())
        },
    }
}

fn serialized(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .flat_map(|message| &message.blocks)
        .map(|block| format!("{block:?}"))
        .collect()
}

#[test]
fn render_never_places_untrusted_text_in_system() {
    let mut request = request(vec![step("call_1", "src/lib.rs")]);
    request.input.child_results = vec![ChildResult {
        objective: "child canary".into(),
        summary: "child canary".into(),
        result: "child canary".into(),
    }];
    let rendered = conversation(&request).unwrap();
    // SystemBlock is a closed enum: a frozen prompt file or the policy sentence, nothing else.
    for block in &rendered.system {
        let text = block.text();
        for canary in CANARIES {
            assert!(!text.contains(canary), "{canary} reached system");
        }
    }
    assert_eq!(rendered.system.len(), 2);
    assert!(matches!(rendered.system[0], SystemBlock::Prompt(_)));
    assert_eq!(
        rendered.system[1],
        SystemBlock::Policy(request.collaboration)
    );
    // Assistant messages replay model turns only; every other untrusted piece is a labelled user block.
    for message in &rendered.messages {
        if message.role == Role::Assistant {
            assert!(
                message
                    .blocks
                    .iter()
                    .all(|block| matches!(block, Block::AssistantTurn(_)))
            );
        }
    }
    let labelled = [
        "previous_run (historical, untrusted)",
        "workspace_guidance (untrusted)",
        "includes (untrusted)",
        "session_summary (untrusted, model-authored)",
        "tool_catalog (untrusted data, not permissions)",
        "child_results (untrusted; the Run is now in synthesis)",
    ];
    for canary in CANARIES {
        let carriers: Vec<_> = rendered
            .messages
            .iter()
            .flat_map(|message| {
                message
                    .blocks
                    .iter()
                    .map(move |block| (message.role, block))
            })
            .filter(|(_, block)| format!("{block:?}").contains(canary))
            .collect();
        assert!(!carriers.is_empty(), "{canary} was dropped");
        for (role, block) in carriers {
            assert_eq!(role, Role::User, "{canary}");
            match block {
                Block::Text(text) => assert!(
                    labelled
                        .iter()
                        .any(|scope| text.contains(&format!("\"scope\":\"{scope}\""))),
                    "{canary} is unlabelled"
                ),
                Block::ToolResult { .. } => {}
                other => panic!("{canary} in {other:?}"),
            }
        }
    }
    let policy = SystemBlock::Policy(CollaborationPolicy::Team {
        max_active_children: 2,
    });
    assert_eq!(
        policy.text(),
        r#"Pinned collaboration policy: {"mode":"team","max_active_children":2}."#
    );
}

#[test]
fn request_prefix_is_append_only_across_steps() {
    let mut growing = request(Vec::new());
    growing.input.goal_context_facts = Some(GoalContextFacts::maximum());
    let mut renders = vec![serialized(&conversation(&growing).unwrap().messages)];
    let mut tools = growing.input.tools.take().unwrap();
    for index in 0..3 {
        tools
            .steps
            .push(step(&format!("call_{index}"), &format!("src/{index}.rs")));
        growing.input.tools = Some(tools.clone());
        renders.push(serialized(&conversation(&growing).unwrap().messages));
    }
    for pair in renders.windows(2) {
        assert!(pair[1].len() > pair[0].len());
        assert_eq!(
            pair[0][..],
            pair[1][..pair[0].len()],
            "no earlier block may change"
        );
    }
}

#[test]
fn tools_never_change_within_a_run() {
    let before = conversation(&request(Vec::new())).unwrap();
    let after = conversation(&request(vec![step("call_1", "a"), step("call_2", "b")])).unwrap();
    let names = |conversation: &Conversation| -> Vec<String> {
        conversation
            .tools
            .as_ref()
            .unwrap()
            .iter()
            .map(|spec| spec.name.clone())
            .collect()
    };
    assert_eq!(names(&before), names(&after));
    assert_eq!(
        before.tools.as_ref().unwrap()[0].parameters,
        after.tools.as_ref().unwrap()[0].parameters
    );
}

#[test]
fn results_pair_with_request_ids_and_integrations_are_labelled_data() {
    let mut command_step = step("call_9", "src/lib.rs");
    command_step.requests[0].call = ToolCall::Command {
        program: "cargo".into(),
        args: vec!["fmt".into()],
        cwd: ".".into(),
    };
    command_step.observations = vec![
        observation(command_step.requests[0].call.clone(), "{}"),
        observation(
            ToolCall::ApplyChangeset {
                source_intent_digest: [3; 32],
                changeset: crate::tools::CommandChangeset::default(),
            },
            "applied",
        ),
    ];
    command_step.steering_notes = vec!["note after results".into()];
    let rendered = conversation(&request(vec![command_step])).unwrap();
    let results = &rendered.messages.last().unwrap().blocks;
    assert!(
        matches!(&results[0], Block::ToolResult { id, is_error: false, .. } if id.as_str() == "call_9")
    );
    assert!(
        matches!(&results[1], Block::Text(text) if text.contains("engine_integration (untrusted result data)"))
    );
    assert!(matches!(&results[2], Block::Text(text) if text.contains("note after results")));
    assert!(matches!(&results[3], Block::Text(text) if text.contains("goal_context_facts")));
}

#[test]
fn delegate_step_carries_child_results_once() {
    let mut delegated = request(vec![ToolStep {
        turn: ProviderTurn::scripted(),
        requests: Vec::new(),
        observations: Vec::new(),
        facts: None,
        steering_notes: Vec::new(),
        calls_remaining: None,
    }]);
    delegated.phase = AgentPhase::RootSynthesis;
    delegated.input.child_results = vec![ChildResult {
        objective: "o".into(),
        summary: "s".into(),
        result: "child canary".into(),
    }];
    let rendered = conversation(&delegated).unwrap();
    let carriers = serialized(&rendered.messages)
        .into_iter()
        .filter(|block| block.contains("child canary"))
        .count();
    assert_eq!(carriers, 1);
    assert!(
        rendered
            .messages
            .last()
            .unwrap()
            .blocks
            .iter()
            .any(|block| matches!(block, Block::Text(text) if text.contains("child_results")))
    );
    delegated.input.tools = None;
    delegated.input.role = PromptRole::ReadOnly;
    let plain = conversation(&delegated).unwrap();
    assert!(plain.tools.is_none());
    assert!(
        plain
            .messages
            .last()
            .unwrap()
            .blocks
            .iter()
            .any(|block| matches!(block, Block::Text(text) if text.contains("child canary")))
    );
}

#[test]
fn compaction_summary_message_precedes_the_remaining_steps() {
    let mut compacted = request(vec![step("call_2", "b")]);
    compacted.input.tools.as_mut().unwrap().history_summary =
        Some("summary of earlier steps".into());
    let rendered = conversation(&compacted).unwrap();
    let blocks = serialized(&rendered.messages);
    let summary = blocks
        .iter()
        .position(|block| block.contains("tool_history_summary"))
        .unwrap();
    let turn = blocks
        .iter()
        .position(|block| block.contains("AssistantTurn"))
        .unwrap();
    assert!(summary < turn);
}

#[test]
fn history_tool_block_projects_calls_without_intent_internals() {
    use crate::session::{RunStatus, RunView, ToolView};
    let observed = observation(read("src/lib.rs"), "{\"text\":\"ok\"}");
    let run = RunView {
        id: RunId::new(),
        objective: "o".into(),
        images: Vec::new(),
        config: None,
        agents: Vec::new(),
        assistant_message: None,
        status: RunStatus::Interrupted,
        accepted_sequence: 1,
        finished_sequence: None,
        tools: vec![
            ToolView {
                intent: observed.intent.clone(),
                observation: Some(observed),
                review_call_index: None,
                permission_verdict: None,
                finished_sequence: Some(3),
            },
            ToolView {
                intent: observation(
                    ToolCall::ApplyChangeset {
                        source_intent_digest: [7; 32],
                        changeset: crate::tools::CommandChangeset::default(),
                    },
                    "",
                )
                .intent,
                observation: None,
                review_call_index: None,
                permission_verdict: None,
                finished_sequence: None,
            },
        ],
        tool_compaction_summary: None,
        tool_compaction_covered_count: 0,
        display: Vec::new(),
        steering_notes: Vec::new(),
        provider_fallbacks: Vec::new(),
    };
    let block = history_tool_block(&run);
    let parsed: Value = serde_json::from_str(&block).unwrap();
    assert_eq!(parsed["tool_records"][0]["call"]["operation"], "read");
    assert_eq!(parsed["tool_records"][0]["workspace_effect"], "none");
    assert_eq!(
        parsed["tool_records"][1]["execution"],
        "uncertain; never replay"
    );
    for internal in [
        "digest",
        "policy",
        "limits",
        "expires_at_ms",
        "use_count",
        "workspace_device",
    ] {
        assert!(!block.contains(internal), "{internal} leaked into history");
    }
}

#[test]
fn parse_steps_never_reach_history_or_shift_the_compaction_prefix() {
    use crate::session::{RunStatus, RunView, ToolView};
    let view = |call: ToolCall, text: &str| {
        let observed = observation(call, text);
        ToolView {
            intent: observed.intent.clone(),
            observation: Some(observed),
            review_call_index: None,
            permission_verdict: None,
            finished_sequence: Some(3),
        }
    };
    let parse = || {
        view(
            ToolCall::ParseAgentDefinition {
                index: 0,
                raw_digest: [1; 32],
            },
            "{}",
        )
    };
    let mut run = RunView {
        id: RunId::new(),
        objective: "o".into(),
        images: Vec::new(),
        config: None,
        agents: Vec::new(),
        assistant_message: None,
        status: RunStatus::Finished,
        accepted_sequence: 1,
        finished_sequence: None,
        tools: vec![parse()],
        tool_compaction_summary: None,
        tool_compaction_covered_count: 0,
        display: Vec::new(),
        steering_notes: Vec::new(),
        provider_fallbacks: Vec::new(),
    };
    assert!(run.model_tools().is_empty());
    assert_eq!(history_tool_block(&run), "");
    run.tools.push(view(read("a"), "{\"text\":\"a\"}"));
    run.tools.push(view(read("b"), "{\"text\":\"b\"}"));
    let records = |run: &RunView| {
        let parsed: Value = serde_json::from_str(&history_tool_block(run)).unwrap();
        parsed["tool_records"].as_array().unwrap().len()
    };
    assert_eq!(records(&run), 2);
    run.tool_compaction_covered_count = 2;
    assert_eq!(
        records(&run),
        1,
        "covered counts index the Run's Tool Events"
    );
}
