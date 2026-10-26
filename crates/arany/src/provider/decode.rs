use super::{ChildAssignment, Delegate, ProviderError, ProviderOutcome, ToolUseId};
use crate::tools::types::{MAX_ORDINARY_TOOL_ARGUMENT_BYTES, MAX_TOOL_ARGUMENT_BYTES};
use crate::tools::{ToolCall, ToolRequest};
use serde::Deserialize;
use serde_json::{Map, Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutcomeEnvelope {
    outcome: StructuredOutcome,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum StructuredOutcome {
    Delegate {
        children: Vec<ChildAssignment>,
        agent: Option<String>,
    },
}

impl From<StructuredOutcome> for ProviderOutcome {
    fn from(outcome: StructuredOutcome) -> Self {
        match outcome {
            StructuredOutcome::Delegate { children, agent } => {
                Self::Delegate(Delegate { children, agent })
            }
        }
    }
}

pub(crate) fn structured_outcome(text: &str) -> Result<ProviderOutcome, ProviderError> {
    serde_json::from_str::<OutcomeEnvelope>(text)
        .map_err(|_| ProviderError::InvalidOutcome)
        .and_then(|envelope| {
            let outcome: ProviderOutcome = envelope.outcome.into();
            if let ProviderOutcome::Delegate(value) = &outcome
                && !super::valid_assignments(&value.children, value.agent.as_deref())
            {
                return Err(ProviderError::InvalidOutcome);
            }
            Ok(outcome)
        })
}

pub(crate) fn function_outcome(
    operation: &str,
    mut fields: Map<String, Value>,
) -> Result<ProviderOutcome, ProviderError> {
    if fields.insert("type".into(), json!(operation)).is_some() {
        return Err(ProviderError::InvalidOutcome);
    }
    structured_outcome(&json!({"outcome": fields}).to_string())
}

// Chosen by operation before buffering or parsing, so whitespace or escapes that parsing compacts never widen an ordinary call.
pub(crate) fn argument_limit(operation: &str) -> usize {
    match operation {
        "delegate" => crate::store::MAX_EVENT_BYTES,
        "write" | "edit" | "multiedit" => MAX_TOOL_ARGUMENT_BYTES,
        _ => MAX_ORDINARY_TOOL_ARGUMENT_BYTES,
    }
}

pub(crate) fn tool_function(
    id: ToolUseId,
    operation: &str,
    input: Value,
) -> Result<ProviderOutcome, ProviderError> {
    if operation == "delegate" {
        let fields = input
            .as_object()
            .cloned()
            .ok_or(ProviderError::InvalidOutcome)?;
        return function_outcome(operation, fields);
    }
    if operation == "plan" {
        if !serde_json::to_vec(&input).is_ok_and(|bytes| bytes.len() <= argument_limit(operation)) {
            return Err(ProviderError::InvalidOutcome);
        }
        let contract = serde_json::from_value(input).map_err(|_| ProviderError::InvalidOutcome)?;
        return Ok(ProviderOutcome::Plan(Box::new(super::PlanRequest {
            id,
            contract,
        })));
    }
    tool_request(id, operation, input).and_then(|request| tools_outcome(vec![request]))
}

pub(crate) fn tool_request(
    id: ToolUseId,
    operation: &str,
    input: Value,
) -> Result<ToolRequest, ProviderError> {
    let invalid = |_| ProviderError::InvalidOutcome;
    let Value::Object(mut fields) = input else {
        return Err(ProviderError::InvalidOutcome);
    };
    if serde_json::to_vec(&fields).map_err(invalid)?.len() > argument_limit(operation) {
        return Err(ProviderError::InvalidOutcome);
    }
    if fields
        .insert("operation".into(), json!(operation))
        .is_some()
    {
        return Err(ProviderError::InvalidOutcome);
    }
    let (path_keys, directory): (&[&str], bool) = match operation {
        "list" | "search" | "glob" => (&["path"], true),
        "command" | "command_unguarded" => (&["cwd"], true),
        "multiedit" => (&["file_path"], false),
        "move" => (&["source", "destination"], false),
        _ => (&["path"], false),
    };
    for &path_key in path_keys {
        if let Some(Value::String(path)) = fields.get_mut(path_key) {
            let mut normalized = path.as_str();
            while let Some(rest) = normalized.strip_prefix("./") {
                normalized = rest;
            }
            if directory && normalized != "/" {
                normalized = normalized.strip_suffix('/').unwrap_or(normalized);
            }
            *path = normalized.to_owned();
        }
    }
    let call: ToolCall = serde_json::from_value(Value::Object(fields)).map_err(invalid)?;
    if matches!(
        call,
        ToolCall::ApplyChangeset { .. }
            | ToolCall::ParseAgentDefinition { .. }
            | crate::tools::ToolCall::PrepareWorker { .. }
    ) {
        return Err(ProviderError::InvalidOutcome);
    }
    Ok(ToolRequest { id, call })
}

pub(super) const STEP_TOOL_CALLS: usize = 1;
const _: () = assert!(STEP_TOOL_CALLS <= crate::session::MAX_STEP_TOOL_CALLS as usize);

pub(crate) fn tools_outcome(requests: Vec<ToolRequest>) -> Result<ProviderOutcome, ProviderError> {
    if requests.is_empty()
        || requests.len() > STEP_TOOL_CALLS
        || requests
            .iter()
            .enumerate()
            .any(|(index, request)| requests[..index].iter().any(|other| other.id == request.id))
    {
        return Err(ProviderError::InvalidOutcome);
    }
    Ok(ProviderOutcome::Tools(requests))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Review {
    verdict: super::ReviewVerdict,
}

pub(crate) fn review_outcome(text: &str) -> Result<ProviderOutcome, ProviderError> {
    let review: Review = serde_json::from_str(text).map_err(|_| ProviderError::InvalidOutcome)?;
    Ok(ProviderOutcome::Review(review.verdict))
}

pub(crate) fn review_schema() -> Value {
    json!({"type":"object", "properties":{"verdict":{"type":"string","enum":["approve","ask"]}}, "required":["verdict"], "additionalProperties":false})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_tool_paths_normalize_only_peer_spelling_before_admission() {
        for (operation, mut fields, expected, valid) in [
            ("read", json!({"path":"././src/a"}), "src/a", true),
            ("read", json!({"path":"./src/a/"}), "src/a/", false),
            (
                "write",
                json!({"path":"./src/a/", "expected_digest":null, "content":"x"}),
                "src/a/",
                false,
            ),
            ("mkdir", json!({"path":"./src/"}), "src/", false),
            ("list", json!({"path":"././src/"}), "src", true),
            ("search", json!({"path":"./src/", "query":"x"}), "src", true),
            ("glob", json!({"path":"./src/", "pattern":"*"}), "src", true),
            (
                "command",
                json!({"cwd":"././src/", "program":"rg", "args":[]}),
                "src",
                true,
            ),
            ("list", json!({"path":"./"}), "", true),
            ("read", json!({"path":"./"}), "", false),
            ("list", json!({"path":"/"}), "/", false),
            ("list", json!({"path":".//"}), "/", false),
            ("list", json!({"path":"src//"}), "src/", false),
            ("read", json!({"path":"./../outside"}), "../outside", false),
            ("read", json!({"path":"./.env"}), ".env", false),
            (
                "list",
                json!({"path":"src/./nested/"}),
                "src/./nested",
                false,
            ),
        ] {
            if operation == "read" {
                fields["offset"] = Value::Null;
                fields["limit"] = Value::Null;
            }
            let request = super::tool_request(
                super::ToolUseId::new("call_test").unwrap(),
                operation,
                fields,
            )
            .expect("bounded typed proposal");
            assert_eq!(
                request.call.path_argument().unwrap().0,
                expected,
                "{operation}"
            );
            assert_eq!(request.call.valid(), valid, "{operation}: {expected}");
        }
        for (source, destination, expected_source, expected_destination, valid) in [
            ("././src/a", "././src/b", "src/a", "src/b", true),
            ("./src/a/", "./src/b", "src/a/", "src/b", false),
            ("./src/a", "./src/b/", "src/a", "src/b/", false),
            ("./../outside", "./src/b", "../outside", "src/b", false),
            ("./src/a", "./../outside", "src/a", "../outside", false),
        ] {
            let request = super::tool_request(
                super::ToolUseId::new("call_test").unwrap(),
                "move",
                json!({"source":source,"destination":destination,"expected_digest":"0".repeat(64)}),
            )
            .expect("bounded typed move proposal");
            assert_eq!(request.call.valid(), valid, "{source} -> {destination}");
            let ToolCall::Move {
                source,
                destination,
                ..
            } = request.call
            else {
                panic!("typed move proposal");
            };
            assert_eq!(
                (source.as_str(), destination.as_str()),
                (expected_source, expected_destination)
            );
        }
    }

    #[test]
    fn assignments_require_one_current_closed_object_form_and_bounded_payload() {
        for profile in [Value::Null, json!("inspect")] {
            let value = json!({"outcome": {"type": "delegate", "children": [{"objective": "Inspect", "profile": profile}]}});
            assert!(structured_outcome(&value.to_string()).is_ok());
        }
        for text in [
            r#"{"outcome":{"type":"delegate","children":["Inspect"]}}"#,
            r#"{"outcome":{"type":"delegate","children":[{"objective":"Inspect"}]}}"#,
            r#"{"outcome":{"type":"delegate","children":[{"profile":null}]}}"#,
            r#"{"outcome":{"type":"delegate","children":[{"objective":"a","objective":"b","profile":null}]}}"#,
            r#"{"outcome":{"type":"delegate","children":[{"objective":"a","profile":null,"profile":"inspect"}]}}"#,
        ] {
            assert!(structured_outcome(text).is_err());
        }
        for profile in [
            json!(1),
            json!({}),
            json!([]),
            json!(""),
            json!("../inspect"),
            json!("Inspect"),
            json!("é"),
            json!("x".repeat(65)),
        ] {
            assert!(structured_outcome(&json!({"outcome": {"type": "delegate", "children": [{"objective": "Inspect", "profile": profile}]}}).to_string()).is_err());
        }
        let mut children = vec![
            ChildAssignment {
                editor: None,
                objective: "x".repeat(2048),
                profile: None
            };
            8
        ];
        let size = |children: &[ChildAssignment]| {
            serde_json::to_vec(
                &json!({"outcome": {"type": "delegate", "children": children, "agent": null}}),
            )
            .unwrap()
            .len()
        };
        let extra = 24 * 1024 - size(&children);
        let controls = extra / 5;
        let escaped = extra % 5;
        children[0].objective =
            "\0".repeat(controls) + &"\n".repeat(escaped) + &"x".repeat(2048 - controls - escaped);
        assert_eq!(size(&children), 24 * 1024);
        assert!(super::super::valid_assignments(&children, None));
        children[1].objective.replace_range(..1, "\n");
        assert_eq!(size(&children), 24 * 1024 + 1);
        assert!(!super::super::valid_assignments(&children, None));
        for (count, objective, valid) in [
            (0, "Inspect".into(), false),
            (8, "Inspect".into(), true),
            (9, "Inspect".into(), false),
            (1, " ".into(), false),
            (1, "x".repeat(2048), true),
            (1, "x".repeat(2049), false),
            (8, "\0".repeat(2048), false),
        ] {
            let children = vec![json!({"objective": objective, "profile": null}); count];
            assert_eq!(
                structured_outcome(
                    &json!({"outcome": {"type": "delegate", "children": children}}).to_string()
                )
                .is_ok(),
                valid
            );
        }
    }
    #[test]
    fn foreground_editor_wire_is_closed_scoped_and_checks_nested_credential_reflection() {
        let assignment = crate::session::worker::EditorAssignment {
            commands: Vec::new(),
            definition: "editor".into(),
            definition_digest: [1; 32],
            mission: "Edit notes".into(),
            direction: "Bounded edit".into(),
            lease: vec!["notes".into()],
            criterion: crate::session::GoalSuccessCriterion::WorkspaceFileContent {
                path: "notes/a".into(),
                content: "done".into(),
            },
            mode: crate::session::worker::WorkerMode::Foreground,
            max_turns: 17,
            wave: 1,
            resumes: None,
            supersedes: None,
        };
        let child = ChildAssignment {
            objective: assignment.mission.clone(),
            profile: None,
            editor: Some(Box::new(assignment)),
        };
        let mut value = json!({"outcome":{"type":"delegate","agent":null,"children":[child]}});
        assert!(structured_outcome(&value.to_string()).is_ok());
        value["outcome"]["children"][0]["editor"]["host_path"] = json!("/outside");
        assert!(structured_outcome(&value.to_string()).is_err());
        value["outcome"]["children"][0]["editor"]
            .as_object_mut()
            .unwrap()
            .remove("host_path");
        value["outcome"]["children"][0]["editor"]["direction"] = json!("synthetic-secret");
        let response = super::super::ProviderResponse {
            outcome: structured_outcome(&value.to_string()).unwrap(),
            turn: super::super::ProviderTurn::scripted(),
            response_id: None,
            usage: Default::default(),
            wire_provenance: None,
        };
        assert!(response.reflects_secret("synthetic-secret"));
        let mut request = super::super::test_support::primary_request("test-model");
        request.collaboration = crate::session::CollaborationPolicy::Auto {
            max_active_children: 3,
        };
        request.input.goal_plan_contract = Some(super::super::test_support::goal_plan_contract());
        let mut catalog = super::super::test_support::catalog();
        catalog["agents"] = json!([{"name":"editor","editor":true,"supported":true,"digest":vec![1;32],"max_turns":17}]);
        request.input.tools = Some(super::super::test_support::tool_context(catalog));
        let schema = super::super::run_outcome_schema(&request);
        let validator = jsonschema::validator_for(&schema).unwrap();
        assert!(validator.is_valid(&value));
        value["outcome"]["children"][0]["editor"]["resumes"] =
            json!(crate::session::AgentRunId::new().to_string());
        assert!(validator.is_valid(&value));
        assert!(structured_outcome(&value.to_string()).is_ok());
        value["outcome"]["children"][0]["editor"]["resumes"] = json!("not-an-agent-id");
        assert!(structured_outcome(&value.to_string()).is_err());
        value["outcome"]["children"][0]["editor"]["resumes"] = serde_json::Value::Null;
        value["outcome"]["children"][0]["editor"]["mode"] = json!("background");
        assert!(validator.is_valid(&value));
        value["outcome"]["children"][0]["editor"]["wave"] = json!(8);
        assert!(validator.is_valid(&value));
        value["outcome"]["children"][0]["editor"]["wave"] = json!(9);
        assert!(!validator.is_valid(&value));
        request.input.role = crate::provider::PromptRole::Editor;
        request.phase = crate::provider::AgentPhase::ChildWork;
        request.collaboration = crate::session::CollaborationPolicy::Single;
        assert!(request.validate_scope().is_ok());
        assert_eq!(
            super::super::run_outcome_schema(&request)["properties"]["outcome"]["anyOf"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }
}
