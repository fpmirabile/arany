//! Outcome decoding shared by every adapter: Finish, Delegate and native Tool requests.

use super::{Delegate, Finish, ProviderError, ProviderOutcome, ToolUseId};
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
    Finish { summary: String, result: String },
    Delegate { children: Vec<String> },
}

impl From<StructuredOutcome> for ProviderOutcome {
    fn from(outcome: StructuredOutcome) -> Self {
        match outcome {
            StructuredOutcome::Finish { summary, result } => {
                Self::Finish(Finish { summary, result })
            }
            StructuredOutcome::Delegate { children } => Self::Delegate(Delegate { children }),
        }
    }
}

/// Finish or Delegate from the schema-constrained `{"outcome": {...}}` text; anything else is rejected.
pub(crate) fn structured_outcome(text: &str) -> Result<ProviderOutcome, ProviderError> {
    serde_json::from_str::<OutcomeEnvelope>(text)
        .map(|envelope| envelope.outcome.into())
        .map_err(|_| ProviderError::InvalidOutcome)
}

/// Finish or Delegate from the arguments of the `finish` or `delegate` function.
pub(crate) fn function_outcome(
    operation: &str,
    mut fields: Map<String, Value>,
) -> Result<ProviderOutcome, ProviderError> {
    if fields.insert("type".into(), json!(operation)).is_some() {
        return Err(ProviderError::InvalidOutcome);
    }
    structured_outcome(&json!({"outcome": fields}).to_string())
}

/// One typed Tool request from a native call: `operation` is injected, a sibling `plan_contract` lifted.
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
    if !call.valid() || matches!(call, ToolCall::ApplyChangeset { .. }) {
        return Err(ProviderError::InvalidOutcome);
    }
    Ok(ToolRequest {
        id,
        call,
        plan_contract,
    })
}

/// Tool requests one step may carry until multi-call steps land.
const STEP_TOOL_CALLS: usize = 1;
const _: () = assert!(STEP_TOOL_CALLS <= crate::session::MAX_STEP_TOOL_CALLS as usize);

/// `Tools` outcome of one step; the ids must be unique.
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
