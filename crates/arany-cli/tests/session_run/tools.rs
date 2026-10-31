use crate::{hash, private_file};
use arany::{
    AgentPhase, Answer, CollaborationPolicy, CompactionRequest, CompactionResponse, Provider,
    ProviderError, ProviderOutcome, ProviderRequest, ProviderResponse, RunRequest, RunStatus,
    SessionView, StateRoot, Store, ToolCall, ToolDisposition,
};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::engine_support::run_with;
use arany::{EngineConfig, RunControls, ToolGrant};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[path = "tools/admission.rs"]
mod admission;
#[path = "tools/coding.rs"]
mod coding;
#[path = "tools/containment.rs"]
mod containment;
#[path = "tools/git.rs"]
mod git;
#[path = "tools/mcp.rs"]
mod mcp;
#[path = "tools/network.rs"]
mod network;
#[path = "tools/permissions.rs"]
mod permissions;
#[path = "tools/read_replay.rs"]
mod read_replay;

#[path = "tools/native_exec.rs"]
mod native_exec;

fn assert_seed_retired(id: uuid::Uuid) {
    assert!(!Path::new("/tmp").join(format!("arany-tools-{id}")).exists());
    let guard = StateRoot::account_path().unwrap().join("guard");
    for suffix in ["unit", "seed", "seed.next"] {
        assert!(!guard.join(format!("{id}.{suffix}")).exists());
    }
}

fn proposal_plan_contract(call: &ToolCall) -> Option<arany::GoalPlanContract> {
    match call {
        ToolCall::McpList { .. } => None,
        ToolCall::McpCall { server, tool, .. } => Some(arany::GoalPlanContract {
            mission: "Invoke the exact synthetic MCP tool".into(),
            paths: Vec::new(),
            commands: Vec::new(),
            resource_class: arany::ToolResourceClass::Default,
            command_timeout_ms: None,
            skills: Vec::new(),
            mcp_tools: vec![arany::GoalMcpTool {
                server: server.clone(),
                tool: tool.clone(),
            }],
            success_criterion: arany::GoalSuccessCriterion::ToolCallSucceeded {
                call: Box::new(call.clone()),
            },
            budget: arany::GoalBudget::default(),
        }),
        _ => Some(arany::GoalPlanContract {
            mission: "Exercise the proposed Tool and verify the unchanged sentinel".into(),
            paths: vec!["src".into()],
            commands: match call {
                ToolCall::Command { program, .. } => vec![program.clone()],
                _ => Vec::new(),
            },
            resource_class: arany::ToolResourceClass::Default,
            command_timeout_ms: None,
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            success_criterion: arany::GoalSuccessCriterion::WorkspaceFileContent {
                path: "src/current.txt".into(),
                content: "current".into(),
            },
            budget: arany::GoalBudget::default(),
        }),
    }
}

struct Proposal {
    call: ToolCall,
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
    inspect_first: bool,
    plan_on_inspection: bool,
    omit_plan: bool,
}
#[async_trait::async_trait]
impl Provider for Proposal {
    fn identity(&self) -> &arany::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                arany::ProviderIdentity::new("openai", "guard-model", arany::Provenance::native())
            });
        &IDENTITY
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let mut requests = self.requests.lock().unwrap();
        let planned = crate::planned(&request);
        let request_index = requests.len().saturating_sub(usize::from(planned));
        if !planned
            && !self.omit_plan
            && request_index == usize::from(self.inspect_first && !self.plan_on_inspection)
            && let Some(contract) = proposal_plan_contract(&self.call)
        {
            requests.push(request);
            return Ok(ProviderResponse {
                turn: arany::ProviderTurn::scripted(),
                outcome: arany::test_support::scenario::plan_outcome(contract),
                response_id: None,
                usage: arany::Usage::default(),
                wire_provenance: None,
            });
        }
        if self.inspect_first && request_index == 0 {
            requests.push(request);
            return Ok(ProviderResponse {
                turn: arany::ProviderTurn::scripted(),
                outcome: crate::tool_outcome(ToolCall::Read {
                    path: "src/current.txt".into(),
                    offset: None,
                    limit: Some(64),
                }),
                response_id: None,
                usage: arany::Usage {
                    input_tokens: None,
                    output_tokens: None,
                    ..Default::default()
                },
                wire_provenance: None,
            });
        }
        let step = request_index.saturating_sub(if self.inspect_first { 1 } else { 0 });
        let outcome = match (step, &self.call) {
            (0, call) => crate::tool_outcome(call.clone()),
            (1, ToolCall::McpList { .. } | ToolCall::McpCall { .. }) | (2, _) => {
                ProviderOutcome::Answer(Answer {
                    text: "Observed the Tool result without retrying.".into(),
                })
            }
            (1, _) => crate::tool_outcome(ToolCall::Read {
                path: "src/current.txt".into(),
                offset: None,
                limit: Some(64),
            }),
            _ => return Err(ProviderError::InvalidOutcome),
        };
        requests.push(request);
        Ok(ProviderResponse {
            turn: arany::ProviderTurn::scripted(),
            outcome,
            response_id: None,
            usage: arany::Usage {
                input_tokens: None,
                output_tokens: None,
                ..Default::default()
            },
            wire_provenance: None,
        })
    }
}

fn basic_config(state_path: &Path) {
    let config = json!({"workspace_paths":["src"],"write":true,
        "commands":[{"name":"bash","executable":"/usr/bin/bash","sha256":hash(&std::fs::read("/usr/bin/bash").unwrap()),"interpreter":true,"inputs":[]}],"skills":[],"mcp":[]});
    private_file(
        &state_path.join("tools.json"),
        &serde_json::to_vec(&config).unwrap(),
    );
}

fn proposal_request(workspace: &Path) -> RunRequest {
    RunRequest {
        skill: None,
        session_id: None,
        title: None,
        objective: "Guard corpus".into(),
        images: Vec::new(),
        workspace: workspace.into(),
        launch_directory: None,
        include_paths: Vec::new(),
        policy: CollaborationPolicy::Single,
        goal: arany::GoalRequest::default(),
    }
}

fn assert_proposal_requests(requests: &[ProviderRequest], count: usize) {
    let planned = requests.iter().any(|request| {
        request
            .input
            .tools
            .as_ref()
            .is_some_and(|tools| tools.steps.iter().any(|step| step.plan.is_some()))
    });
    assert_eq!(requests.len(), count + usize::from(planned));
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(request.phase, AgentPhase::RootPlan);
        assert_eq!(request.model, "guard-model");
        assert_eq!(request.input.objective, "Guard corpus");
        assert!(
            request.input.workspace_guidance.is_empty()
                && request.input.includes.is_empty()
                && request.input.images.is_empty()
                && request.input.child_results.is_empty()
                && request.input.history.is_empty()
                && request.input.context_summary.is_none()
        );
        assert_eq!(request.max_output_tokens, 4096);
        let observations = request.input.tools.as_ref().unwrap().observations().len();
        let plans = request
            .input
            .tools
            .as_ref()
            .unwrap()
            .steps
            .iter()
            .filter(|step| step.plan.is_some())
            .count();
        assert_eq!(observations, index - plans);
    }
}

#[path = "tools/inline_approval.rs"]
mod inline_approval;
