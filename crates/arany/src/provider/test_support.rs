use super::{
    AgentPhase, ChildResult, CompactionItem, CompactionRequest, HistoryTurn, PromptRole,
    ProviderRequest, RunInput,
};
use crate::session::{
    AgentRunId, CollaborationPolicy, GoalBudget, GoalPlanContract, RunId, SessionId,
};
use crate::tools::ToolContext;
use serde_json::{Value, json};

pub(crate) fn catalog() -> Value {
    json!({
        "write": true,
        "host_commands": true,
        "commands": [{"name": "bash"}],
        "skills": [{"name": "example"}],
        "mcp_servers": [{"name": "example", "tools": ["echo"]}],
        "git": {"write": true, "operations": ["git_status", "git_diff", "git_log", "git_commit", "git_branch", "git_switch"]},
        "tool_calls_remaining": 4,
    })
}

pub(crate) fn tool_context(catalog: Value) -> ToolContext {
    ToolContext::new(catalog)
}

fn request(model: &str, phase: AgentPhase, input: RunInput) -> ProviderRequest {
    ProviderRequest {
        session_id: crate::session::SessionId::new(),
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        phase,
        collaboration: CollaborationPolicy::Single,
        model: model.into(),
        max_output_tokens: 4096,
        input,
    }
}

pub(crate) fn primary_request(model: &str) -> ProviderRequest {
    request(
        model,
        AgentPhase::RootPlan,
        RunInput {
            goal_budget: Some(GoalBudget::default()),
            tools: Some(tool_context(catalog())),
            ..RunInput::new(PromptRole::Primary, "objective canary".into())
        },
    )
}

pub(crate) fn read_only_request(model: &str) -> ProviderRequest {
    let mut request = request(
        model,
        AgentPhase::RootSynthesis,
        RunInput {
            workspace_guidance: vec![super::WorkspaceGuidance {
                path: "AGENTS.md".into(),
                text: "workspace guidance canary".into(),
            }],
            includes: vec!["include canary".into()],
            history: vec![HistoryTurn {
                user: "previous user canary".into(),
                assistant: "previous assistant canary".into(),
            }],
            context_summary: Some("derived summary canary".into()),
            child_results: vec![ChildResult {
                objective: "child objective canary".into(),
                summary: "child summary canary".into(),
                result: "child result canary".into(),
            }],
            ..RunInput::new(PromptRole::ReadOnly, "objective canary".into())
        },
    );
    request.collaboration = CollaborationPolicy::Auto {
        max_active_children: 3,
    };
    request
}

pub(crate) fn researcher_request(model: &str) -> ProviderRequest {
    request(
        model,
        AgentPhase::ChildWork,
        RunInput {
            tools: Some(tool_context(catalog())),
            ..RunInput::new(PromptRole::Researcher, "research canary".into())
        },
    )
}

pub(crate) fn goal_plan_contract() -> GoalPlanContract {
    GoalPlanContract {
        mission: "Verify the requested Workspace update".into(),
        paths: vec!["src".into()],
        commands: vec!["bash".into()],
        resource_class: crate::tools::ToolResourceClass::Default,
        command_timeout_ms: None,
        skills: vec![crate::session::GoalSkillBinding {
            name: "example".into(),
            resource: Some("references/checklist.md".into()),
        }],
        mcp_tools: vec![crate::session::GoalMcpTool {
            server: "example".into(),
            tool: "echo".into(),
        }],
        success_criterion: crate::session::GoalSuccessCriterion::WorkspaceFileContent {
            path: "src/main.rs".into(),
            content: "verified\n".into(),
        },
        budget: GoalBudget::default(),
    }
}

fn intent(call: crate::tools::ToolCall) -> crate::tools::EffectIntent {
    crate::tools::EffectIntent {
        host: None,
        host_command: call.is_unguarded().then(crate::tools::types::test_host_pin),
        target: crate::session::worker::EffectTarget::Workspace,
        id: uuid::Uuid::now_v7(),
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call,
        limits: crate::tools::ToolLimits::default(),
        use_count: 1,
    }
}

pub(crate) fn observation(
    call: crate::tools::ToolCall,
    output: &str,
) -> crate::tools::ToolObservation {
    let intent = intent(call);
    let attestation = crate::tools::Attestation::for_site(&intent);
    crate::tools::ToolObservation {
        spill: None,
        enforcement_cause: None,
        limit_cause: None,
        attestation: Some(attestation),
        intent,
        disposition: crate::tools::ToolDisposition::Succeeded,
        output: output.into(),
        approval_cause: None,
        guard_stage: None,
        cache_update: crate::tools::CacheUpdate::NotApplicable,
    }
}

pub(crate) fn observed_step(
    response: super::ProviderResponse,
    output: &str,
) -> crate::tools::ToolStep {
    let super::ProviderOutcome::Tools(requests) = response.outcome else {
        panic!("a Tool response");
    };
    crate::tools::ToolStep {
        narrowed_observations: Default::default(),
        refusal_index: None,
        refusal: None,
        plan: None,
        turn: response.turn,
        observations: requests
            .iter()
            .map(|request| observation(request.call.clone(), output))
            .collect(),
        requests,
        facts: Some(super::GoalContextFacts::maximum()),
        steering_notes: Vec::new(),
        calls_remaining: None,
        cleared: false,
        hook_feedback: Default::default(),
    }
}

pub(crate) fn tool_surface_requests(model: &str) -> Vec<(&'static str, ProviderRequest)> {
    let mut read_only = primary_request(model);
    read_only.input.tools = Some(tool_context(json!({
        "write": false, "commands": [], "skills": [], "mcp_servers": [], "git": "unavailable",
    })));
    vec![
        ("primary", primary_request(model)),
        ("primary_read_only", read_only),
        ("researcher", researcher_request(model)),
    ]
}

pub(crate) fn assignment_shapes() -> Vec<(Value, bool)> {
    vec![
        (json!({"objective": "Inspect", "profile": null}), true),
        (json!({"objective": "Inspect", "profile": "inspect"}), true),
        (json!("Inspect"), false),
        (json!({"objective": "Inspect"}), false),
        (json!({"profile": null}), false),
        (
            json!({"objective": "Inspect", "profile": null, "model": "arbitrary"}),
            false,
        ),
        (
            json!({"objective": "Inspect", "profile": null, "provider": "arbitrary"}),
            false,
        ),
        (json!({"objective": "Inspect", "profile": 1}), false),
        (
            json!({"objective": "Inspect", "profile": "../escape"}),
            false,
        ),
    ]
}

pub(crate) fn four_read_step(adapter: &'static str) -> crate::tools::ToolStep {
    let requests: Vec<_> = (0..4)
        .map(|index| crate::ToolRequest {
            id: super::ToolUseId::new(format!("call_{index}")).unwrap(),
            call: crate::ToolCall::Read {
                path: "notes/facts.txt".into(),
                offset: None,
                limit: None,
            },
        })
        .collect();
    let items: Vec<_> = requests.iter().map(|request| {
        let arguments = json!({"path":"notes/facts.txt","offset":null,"limit":null});
        if adapter == "anthropic" {
            json!({"type":"tool_use","id":request.id.as_str(),"name":"arany_read","input":arguments})
        } else {
            json!({"type":"function_call","id":format!("item_{}", request.id.as_str()),"call_id":request.id.as_str(),"name":"arany_read","arguments":arguments.to_string(),"status":"completed"})
        }
    }).collect();
    observed_step(
        super::ProviderResponse {
            turn: super::ProviderTurn::new(
                adapter,
                &serde_json::to_string(&items).unwrap(),
                "synthetic-secret",
                4096,
            )
            .unwrap(),
            outcome: super::ProviderOutcome::Tools(requests),
            response_id: None,
            usage: super::Usage::default(),
            wire_provenance: None,
        },
        "bounded facts",
    )
}

const SCHEMA_BOUND_KEYWORDS: [&str; 6] = [
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "minimum",
    "maximum",
];

pub(crate) fn assert_no_schema_bounds(value: &Value, at: &str) {
    match value {
        Value::Object(map) => {
            for keyword in SCHEMA_BOUND_KEYWORDS {
                assert!(!map.contains_key(keyword), "{at} carries {keyword}");
            }
            for (key, child) in map {
                assert_no_schema_bounds(child, &format!("{at}.{key}"));
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                assert_no_schema_bounds(child, &format!("{at}[{index}]"));
            }
        }
        _ => {}
    }
}

pub(crate) fn compaction_bound_request(
    model: &str,
    body_of: fn(&CompactionRequest) -> Value,
) -> (CompactionRequest, Value) {
    let request = |user: String| CompactionRequest {
        session_id: SessionId::new(),
        covered_run_id: RunId::new(),
        model: model.into(),
        previous_summary: None,
        items: vec![CompactionItem::Completed(HistoryTurn {
            user,
            assistant: String::new(),
        })],
        max_output_tokens: crate::session::MAX_COMPACTION_OUTPUT_TOKENS,
    };
    let base = crate::provider::compaction_input_bytes(&request(String::new())).expect("input");
    let slack = crate::session::MAX_COMPACTION_INPUT_BYTES - base;
    // Each quote serializes to two input bytes and four request bytes: the worst expansion.
    let at_bound = request(format!(
        "{}{}",
        "\"".repeat(slack / 2),
        "a".repeat(slack % 2)
    ));
    assert_eq!(
        crate::provider::compaction_input_bytes(&at_bound).expect("input"),
        crate::session::MAX_COMPACTION_INPUT_BYTES
    );
    let body = body_of(&at_bound);
    (at_bound, body)
}
