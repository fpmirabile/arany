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
            } => Some(expected_digest),
            ToolCall::Write {
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

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
