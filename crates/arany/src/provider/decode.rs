use super::{ChildAssignment, Delegate, Finish, ProviderError, ProviderOutcome, ToolUseId};
use crate::tools::types::MAX_TOOL_ARGUMENT_BYTES;
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
    Finish {
        summary: String,
        result: String,
    },
    Delegate {
        children: Vec<ChildAssignment>,
        agent: Option<String>,
    },
}

impl From<StructuredOutcome> for ProviderOutcome {
    fn from(outcome: StructuredOutcome) -> Self {
        match outcome {
            StructuredOutcome::Finish { summary, result } => {
                Self::Finish(Finish { summary, result })
            }
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

pub(crate) fn tool_request(
    id: ToolUseId,
    operation: &str,
    input: Value,
) -> Result<ToolRequest, ProviderError> {
    let invalid = |_| ProviderError::InvalidOutcome;
    let Value::Object(mut fields) = input else {
        return Err(ProviderError::InvalidOutcome);
    };
    if serde_json::to_vec(&fields).map_err(invalid)?.len() > MAX_TOOL_ARGUMENT_BYTES {
        return Err(ProviderError::InvalidOutcome);
    }
    let plan_contract = match fields.remove("plan_contract") {
        None | Some(Value::Null) => None,
        Some(contract) => Some(Box::new(serde_json::from_value(contract).map_err(invalid)?)),
    };
    if fields
        .insert("operation".into(), json!(operation))
        .is_some()
    {
        return Err(ProviderError::InvalidOutcome);
    }
    let call: ToolCall = serde_json::from_value(Value::Object(fields)).map_err(invalid)?;
    if !call.valid()
        || matches!(
            call,
            ToolCall::ApplyChangeset { .. } | ToolCall::ParseAgentDefinition { .. }
        )
    {
        return Err(ProviderError::InvalidOutcome);
    }
    Ok(ToolRequest {
        id,
        call,
        plan_contract,
    })
}

const STEP_TOOL_CALLS: usize = 1;
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
