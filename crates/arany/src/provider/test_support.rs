use super::{AgentPhase, ChildResult, HistoryTurn, PromptRole, ProviderRequest, RunInput};
use crate::session::{AgentRunId, CollaborationPolicy, GoalBudget, GoalPlanContract, RunId};
use crate::tools::ToolContext;
use serde_json::{Value, json};

pub(crate) fn catalog() -> Value {
    json!({
        "write": true,
        "commands": [{"name": "bash"}],
        "skills": [{"name": "example"}],
        "mcp_servers": [{"name": "example", "tools": ["echo"]}],
        "git": {"operations": ["git_status", "git_diff", "git_log"]},
        "tool_calls_remaining": 4,
    })
}

pub(crate) fn tool_context(catalog: Value) -> ToolContext {
    ToolContext {
        catalog: catalog.to_string(),
        history_summary: None,
        steps: Vec::new(),
    }
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

/// A Tool-enabled primary request with a pinned Goal budget.
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

/// A request without Tools that carries every untrusted input class.
pub(crate) fn read_only_request(model: &str) -> ProviderRequest {
    let mut request = request(
        model,
        AgentPhase::RootSynthesis,
        RunInput {
            workspace_guidance: Some("workspace guidance canary".into()),
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
        expires_at_ms: 1,
        use_count: 1,
    }
}

/// A successful observation of `call`, attested by the site its operation runs at.
pub(crate) fn observation(
    call: crate::tools::ToolCall,
    output: &str,
) -> crate::tools::ToolObservation {
    let intent = intent(call);
    let attestation = crate::tools::Attestation::for_site(&intent);
    crate::tools::ToolObservation {
        enforcement_cause: None,
        attestation: Some(attestation),
        intent,
        disposition: crate::tools::ToolDisposition::Succeeded,
        output: output.into(),
        cache_update: crate::tools::CacheUpdate::NotApplicable,
    }
}

/// The step a decoded Tool response opens once its single request has been observed.
pub(crate) fn observed_step(
    response: super::ProviderResponse,
    output: &str,
) -> crate::tools::ToolStep {
    let super::ProviderOutcome::Tools(requests) = response.outcome else {
        panic!("a Tool response");
    };
    crate::tools::ToolStep {
        turn: response.turn,
        observations: requests
            .iter()
            .map(|request| observation(request.call.clone(), output))
            .collect(),
        requests,
        facts: Some(super::GoalContextFacts::maximum()),
        steering_notes: Vec::new(),
        calls_remaining: None,
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
