use crate::diagnostics::{BoundedRecord, record_local};
use std::fmt::{self, Write};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SubscriptionFailureStage {
    RequestTransport,
    Deadline,
    HttpStatus,
    Destination,
    ContentEncoding,
    ContentLength,
    ReadTransport,
    EndBeforeCompleted,
    StreamLimit,
    EventFraming,
    EventJson,
    EventType,
    CredentialReflection,
    RemoteFailure,
    IncompleteResponse,
    CompletedResponse,
    ResponseEnvelope,
    ResponseModel,
    ResponseStatus,
    ResponseIdentifier,
    ResponseMessage,
    ResponseContent,
    ResponsePhase,
    ResponseMissingFinalMessage,
    ResponseDuplicateFinalMessage,
    ResponseAmbiguousFinalMessage,
    ResponseLateCommentary,
    OutcomeContract,
    OutcomeEncoding,
    OutcomeFields,
    OutcomeReadBounds,
    OutcomeToolArguments,
    OutcomeToolPath,
    OutcomeToolDigest,
    OutcomeToolEmptyEdit,
    OutcomeToolTextSize,
    OutcomeToolArgumentSize,
    OutcomeToolProgram,
    UsageContract,
    LocalOutputLimit,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct StreamCounts {
    pub(crate) bytes: usize,
    pub(crate) events: usize,
    pub(crate) http_status: Option<u16>,
    pub(crate) response_shape: Option<ResponseShape>,
    pub(crate) tool_arguments: Option<ToolArgumentShape>,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ResponseShape {
    pub(crate) messages: usize,
    pub(crate) commentary: usize,
    pub(crate) finals: usize,
    pub(crate) unphased: usize,
    pub(crate) done_messages: usize,
    pub(crate) done_finals: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct ToolArgumentShape {
    operation: &'static str,
    serialized_bytes: usize,
    path_bytes: Option<usize>,
    path_valid: Option<bool>,
    digest_chars: Option<usize>,
    digest_valid: Option<bool>,
    read_offset: Option<u32>,
    read_limit: Option<u32>,
    old_bytes: Option<usize>,
    new_bytes: Option<usize>,
    text_bytes: Option<usize>,
    program_chars: Option<usize>,
    program_valid: Option<bool>,
    args_count: Option<usize>,
    largest_arg_bytes: Option<usize>,
    nul_args_count: Option<usize>,
}

impl fmt::Debug for ToolArgumentShape {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolArgumentShape")
            .field("operation", &self.operation)
            .field("serialized_bytes", &self.serialized_bytes)
            .field("path_bytes", &self.path_bytes)
            .field("path_valid", &self.path_valid)
            .field("digest_chars", &self.digest_chars)
            .field("digest_valid", &self.digest_valid)
            .field("read_offset", &self.read_offset)
            .field("read_limit", &self.read_limit)
            .field("old_bytes", &self.old_bytes)
            .field("new_bytes", &self.new_bytes)
            .field("text_bytes", &self.text_bytes)
            .field("program_chars", &self.program_chars)
            .field("program_valid", &self.program_valid)
            .field("args_count", &self.args_count)
            .field("largest_arg_bytes", &self.largest_arg_bytes)
            .field("nul_args_count", &self.nul_args_count)
            .finish()
    }
}

impl ToolArgumentShape {
    pub(crate) fn from_serialized(operation: &'static str, serialized_bytes: usize) -> Self {
        Self {
            operation,
            serialized_bytes,
            path_bytes: None,
            path_valid: None,
            digest_chars: None,
            digest_valid: None,
            read_offset: None,
            read_limit: None,
            old_bytes: None,
            new_bytes: None,
            text_bytes: None,
            program_chars: None,
            program_valid: None,
            args_count: None,
            largest_arg_bytes: None,
            nul_args_count: None,
        }
    }

    pub(crate) fn from_call(call: &crate::tools::ToolCall) -> Self {
        use crate::tools::ToolCall;
        use crate::validate::{valid_digest, valid_name, valid_relative};
        let mut shape = Self {
            operation: call.label(),
            serialized_bytes: serde_json::to_vec(call).map_or(0, |bytes| bytes.len()),
            path_bytes: None,
            path_valid: None,
            digest_chars: None,
            digest_valid: None,
            read_offset: None,
            read_limit: None,
            old_bytes: None,
            new_bytes: None,
            text_bytes: None,
            program_chars: None,
            program_valid: None,
            args_count: None,
            largest_arg_bytes: None,
            nul_args_count: None,
        };
        let path = match call {
            ToolCall::List { path }
            | ToolCall::Search { path, .. }
            | ToolCall::Glob { path, .. } => Some((path, true)),
            ToolCall::Read { path, .. }
            | ToolCall::Mkdir { path }
            | ToolCall::Write { path, .. }
            | ToolCall::Edit { path, .. }
            | ToolCall::Delete { path, .. }
            | ToolCall::MultiEdit {
                file_path: path, ..
            } => Some((path, false)),
            ToolCall::Move { source, .. } => Some((source, false)),
            ToolCall::Command { cwd, .. } => Some((cwd, true)),
            _ => None,
        };
        if let Some((path, root)) = path {
            shape.path_bytes = Some(path.len());
            shape.path_valid = Some(valid_relative(path, root));
        }
        let digest = match call {
            ToolCall::Delete {
                expected_digest, ..
            }
            | ToolCall::Move {
                expected_digest, ..
            }
            | ToolCall::Edit {
                expected_digest, ..
            }
            | ToolCall::MultiEdit {
                expected_digest, ..
            }
            | ToolCall::Write {
                expected_digest, ..
            } => expected_digest.as_ref(),
            ToolCall::McpCall { schema_digest, .. } => Some(schema_digest),
            _ => None,
        };
        if let Some(digest) = digest {
            shape.digest_chars = Some(digest.chars().count());
            shape.digest_valid = Some(valid_digest(digest));
        }
        match call {
            ToolCall::Read { offset, limit, .. } => {
                shape.read_offset = *offset;
                shape.read_limit = *limit;
            }
            ToolCall::Edit { old, new, .. } => {
                shape.old_bytes = Some(old.len());
                shape.new_bytes = Some(new.len());
            }
            ToolCall::Write { content, .. } => shape.text_bytes = Some(content.len()),
            ToolCall::Search { query, .. } => shape.text_bytes = Some(query.len()),
            ToolCall::Glob { pattern, .. } => shape.text_bytes = Some(pattern.len()),
            ToolCall::McpCall { arguments, .. } => shape.text_bytes = Some(arguments.len()),
            ToolCall::Command { program, args, .. } => {
                shape.program_chars = Some(program.chars().count());
                shape.program_valid = Some(valid_name(program));
                shape.args_count = Some(args.len());
                shape.largest_arg_bytes = args.iter().map(String::len).max();
                shape.nul_args_count = Some(args.iter().filter(|arg| arg.contains('\0')).count());
            }
            _ => {}
        }
        shape
    }
}

pub(crate) fn stream_limit(bytes: usize, events: usize) {
    record_local(stream_limit_record(bytes, events));
}

fn stream_limit_record(bytes: usize, events: usize) -> BoundedRecord {
    let mut record = BoundedRecord::default();
    let _ = writeln!(
        record,
        "stream reason=StreamProtocol stage=StreamLimit bytes={bytes} events={events}"
    );
    record
}

pub(crate) fn subscription_failure(stage: SubscriptionFailureStage, counts: StreamCounts) {
    if crate::diagnostics::enabled() {
        record_local(failure_record("subscription", stage, counts));
    }
}

pub(crate) fn messages_failure(stage: SubscriptionFailureStage, counts: StreamCounts) {
    if crate::diagnostics::enabled() {
        record_local(failure_record("messages", stage, counts));
    }
}

pub(crate) fn failure_record(
    site: &str,
    stage: SubscriptionFailureStage,
    counts: StreamCounts,
) -> BoundedRecord {
    let mut record = BoundedRecord::default();
    let _ = writeln!(
        record,
        "{site} stage={stage:?} bytes={} events={} http_status={:?}",
        counts.bytes, counts.events, counts.http_status
    );
    if let Some(shape) = counts.response_shape {
        let _ = writeln!(
            record,
            "response_shape messages={} commentary={} finals={} unphased={} done_messages={} done_finals={}",
            shape.messages,
            shape.commentary,
            shape.finals,
            shape.unphased,
            shape.done_messages,
            shape.done_finals,
        );
    }
    if let Some(shape) = counts.tool_arguments {
        let _ = writeln!(record, "tool_arguments {shape:?}");
    }
    record
}

pub(crate) fn tool_context_full(
    request: &crate::provider::ProviderRequest,
    window_tokens: Option<u32>,
    tool_allowance_bytes: usize,
    pending_turn_bytes: usize,
) {
    if crate::diagnostics::enabled() {
        record_local(context_record(
            request,
            window_tokens,
            tool_allowance_bytes,
            pending_turn_bytes,
        ));
    }
}

fn context_record(
    request: &crate::provider::ProviderRequest,
    window_tokens: Option<u32>,
    tool_allowance_bytes: usize,
    pending_turn_bytes: usize,
) -> BoundedRecord {
    let input = &request.input;
    let mut components = vec![
        ("objective", input.objective.len()),
        (
            "instructions",
            input
                .workspace_guidance
                .iter()
                .map(|value| crate::provider::instructions_text(&value.path, &value.text).len())
                .sum(),
        ),
        ("includes", input.includes.iter().map(String::len).sum()),
        (
            "history",
            input
                .history
                .iter()
                .map(|turn| turn.user.len() + turn.assistant.len() + 64)
                .sum(),
        ),
        (
            "summary",
            input.context_summary.as_ref().map_or(0, String::len),
        ),
        (
            "tools",
            input
                .tools
                .as_ref()
                .map_or(0, crate::tools::ToolContext::rendered_len),
        ),
        ("pending_turn", pending_turn_bytes),
        (
            "images",
            input
                .images
                .iter()
                .map(|value| value.image.context_bytes())
                .sum(),
        ),
        (
            "steering",
            input.steering_notes.iter().map(String::len).sum(),
        ),
        (
            "goal",
            serde_json::to_string(&(
                &input.goal_budget,
                &input.goal_plan_contract,
                &input.goal_context_facts,
            ))
            .map_or(0, |value| value.len()),
        ),
    ];
    let estimated_bytes = components.iter().map(|(_, bytes)| bytes).sum::<usize>();
    components.sort_by_key(|(_, bytes)| std::cmp::Reverse(*bytes));
    let mut record = BoundedRecord::default();
    // Only a bounded model identifier is permitted; all conversation and Tool values stay private.
    let model = if request.model.len() <= super::MAX_MODEL_ID_BYTES
        && request
            .model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:/".contains(&byte))
    {
        request.model.as_str()
    } else {
        "unreported"
    };
    let _ = writeln!(
        record,
        "context cause=ToolContextFull model={model} window_tokens={window_tokens:?} ceiling_bytes={} tool_allowance_bytes={tool_allowance_bytes} estimated_content_bytes={estimated_bytes} estimated_content_tokens={} max_output_tokens={}",
        crate::session::content_ceiling(window_tokens),
        estimated_bytes.div_ceil(3),
        request.max_output_tokens,
    );
    for (label, bytes) in components {
        let _ = writeln!(record, "component={label} bytes={bytes}");
    }
    if let Some(context) = &input.tools {
        let largest_turn = context
            .steps
            .iter()
            .map(|step| step.turn.len())
            .max()
            .unwrap_or(0);
        let largest_result = context
            .steps
            .iter()
            .flat_map(|step| {
                (0..step.observations.len())
                    .map(move |index| context.model_result(step, index).to_string().len())
            })
            .max()
            .unwrap_or(0);
        let _ = writeln!(
            record,
            "catalog_bytes={} largest_turn_bytes={largest_turn} largest_result_bytes={largest_result}",
            context.catalog().len()
        );
    }
    record
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_diagnostic_reports_sizes_without_conversation_values() {
        let mut input = crate::provider::RunInput::new(
            crate::provider::PromptRole::Primary,
            "private-objective-canary".into(),
        );
        input.includes.push("private-include-canary".repeat(100));
        input.tools = Some(crate::tools::ToolContext::new(serde_json::json!({
            "private-catalog-canary": "private-value-canary"
        })));
        input.history.push(crate::provider::HistoryTurn {
            user: "private-history-canary".into(),
            assistant: "private-answer-canary".into(),
        });
        let request = crate::provider::ProviderRequest {
            session_id: Default::default(),
            run_id: Default::default(),
            agent_run_id: Default::default(),
            phase: crate::provider::AgentPhase::RootPlan,
            collaboration: crate::session::CollaborationPolicy::Single,
            model: "gpt-6-luna".into(),
            max_output_tokens: 16_384,
            input,
        };
        for window in [None, Some(272_000)] {
            let record = context_record(&request, window, 16_896, 1234).to_string();
            assert!(record.contains("model=gpt-6-luna"));
            assert!(record.contains(&format!("window_tokens={window:?}")));
            assert!(record.contains("tool_allowance_bytes=16896"));
            assert!(record.contains("component=pending_turn bytes=1234"));
            assert!(record.contains("estimated_content_tokens="));
            assert!(
                record.find("component=includes").unwrap()
                    < record.find("component=history").unwrap()
            );
            assert!(!record.contains("canary") && !record.contains("private-"));
        }
    }

    #[test]
    fn stream_limit_record_names_the_reason_stage_and_counts() {
        assert_eq!(
            stream_limit_record(1024, 12).to_string(),
            "stream reason=StreamProtocol stage=StreamLimit bytes=1024 events=12\n"
        );
    }

    #[test]
    fn failure_record_names_site_stage_and_observed_status() {
        assert_eq!(
            failure_record(
                "messages",
                SubscriptionFailureStage::OutcomeToolArguments,
                StreamCounts {
                    bytes: 2048,
                    events: 9,
                    http_status: Some(200),
                    ..Default::default()
                },
            )
            .to_string(),
            "messages stage=OutcomeToolArguments bytes=2048 events=9 http_status=Some(200)\n"
        );
    }
}
