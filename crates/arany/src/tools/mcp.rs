use super::ToolError;
use super::catalog::string;
use super::config::{Config, Server};
use super::guard::{
    TEARDOWN_WAIT, drain_stderr, payload, read_capped, read_line_capped, reap_owned_child,
};
use super::skills::parse_json;
use super::types::{ToolCall, parse_mcp_arguments};
use crate::validate::{hex_digest, valid_digest, valid_name};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

pub(super) fn valid(call: &ToolCall) -> bool {
    match call {
        ToolCall::McpList { server } => valid_name(server),
        ToolCall::McpCall {
            server,
            tool,
            schema_digest,
            arguments,
        } => {
            valid_name(server)
                && valid_digest(schema_digest)
                && !tool.is_empty()
                && tool.len() <= 128
                && tool.chars().all(|ch| !ch.is_control())
                && parse_mcp_arguments(arguments).is_ok()
        }
        _ => false,
    }
}

pub(super) fn schema(operation: &str) -> Option<Vec<(&'static str, Value)>> {
    Some(match operation {
        "mcp_list" => vec![("server", string())],
        "mcp_call" => vec![
            ("server", string()),
            ("tool", string()),
            ("schema_digest", string()),
            ("arguments", string()),
        ],
        _ => return None,
    })
}

// Task secret meaning "this call's token renewal is unconfirmed"; a real token is printable ASCII, so it cannot collide.
pub(crate) const RENEWAL_UNCONFIRMED: &str = "\0renewal-unconfirmed";

pub(crate) mod oauth;
mod remote;
pub(crate) use remote::{MAX_MCP_RESPONSE_BYTES, execute as execute_remote};

pub(crate) const FRAME_BYTES: usize = 128 * 1024;
const VERSION: &str = "2025-11-25";
const MAX_CONTENT_ITEMS: usize = 64;
const MAX_RESOURCE_URI_BYTES: usize = 2048;

// One JSON-RPC message per call in each direction; the transports differ only in framing.
trait Wire {
    fn send(&mut self, message: Value) -> impl Future<Output = Result<(), ToolError>>;
    fn receive(&mut self) -> impl Future<Output = Result<Value, ToolError>>;
}

fn encode(message: &Value) -> Result<Vec<u8>, ToolError> {
    let bytes = serde_json::to_vec(message).map_err(|_| ToolError::Operation)?;
    if bytes.len() > FRAME_BYTES {
        return Err(ToolError::Limit);
    }
    Ok(bytes)
}

struct Pipes {
    child: Child,
    writer: ChildStdin,
    reader: BufReader<ChildStdout>,
}

impl Wire for Pipes {
    async fn send(&mut self, message: Value) -> Result<(), ToolError> {
        let bytes = encode(&message)?;
        self.writer
            .write_all(&bytes)
            .await
            .map_err(|_| ToolError::Operation)?;
        self.writer
            .write_all(b"\n")
            .await
            .map_err(|_| ToolError::Operation)
    }

    async fn receive(&mut self) -> Result<Value, ToolError> {
        let bytes = read_line_capped(&mut self.reader, FRAME_BYTES).await?;
        parse_json(&bytes, FRAME_BYTES)
    }
}

struct Session<W> {
    wire: W,
    next_id: u64,
    notifications: usize,
}

impl<W: Wire> Session<W> {
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, ToolError> {
        let id = self.next_id;
        self.next_id += 1;
        self.wire
            .send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        loop {
            let mut message = self.wire.receive().await?;
            if message["jsonrpc"] != "2.0" || !message.is_object() {
                return Err(ToolError::Operation);
            }
            if let Some(method) = message["method"].as_str() {
                self.notifications += 1;
                if self.notifications > 64 {
                    return Err(ToolError::Limit);
                }
                if let Some(server_id) = message.get("id") {
                    if !server_id.is_string() && !server_id.is_i64() {
                        return Err(ToolError::Operation);
                    }
                    let response = if method == "ping" {
                        json!({"jsonrpc":"2.0","id":server_id,"result":{}})
                    } else {
                        json!({"jsonrpc":"2.0","id":server_id,"error":{"code":-32601,"message":"Client capability is not available"}})
                    };
                    self.wire.send(response).await?;
                } else if method == "notifications/tools/list_changed" {
                    return Err(ToolError::ChangedInput);
                }
                continue;
            }
            if message["id"].as_u64() != Some(id)
                || message.get("result").is_some() == message.get("error").is_some()
            {
                return Err(ToolError::Operation);
            }
            if message.get("error").is_some() {
                return Err(ToolError::Operation);
            }
            return Ok(message["result"].take());
        }
    }

    async fn catalog(&mut self, allowed: &[String]) -> Result<BTreeMap<String, Value>, ToolError> {
        let mut tools = BTreeMap::new();
        let mut cursors = BTreeSet::new();
        let mut cursor = None;
        let mut count = 0;
        let mut bytes = 0;
        for _ in 0..4 {
            let params = cursor.map_or(json!({}), |cursor| json!({"cursor":cursor}));
            let page = self.request("tools/list", params).await?;
            let rows = page["tools"].as_array().ok_or(ToolError::Operation)?;
            count += rows.len();
            bytes += serde_json::to_vec(&page)
                .map_err(|_| ToolError::Operation)?
                .len();
            if count > 128 || bytes > 256 * 1024 {
                return Err(ToolError::Limit);
            }
            for tool in rows {
                let name = tool["name"]
                    .as_str()
                    .filter(|name| {
                        !name.is_empty() && name.len() <= 128 && !name.chars().any(char::is_control)
                    })
                    .ok_or(ToolError::Operation)?;
                if !allowed.iter().any(|allowed| allowed == name) {
                    continue;
                }
                if tools.contains_key(name) {
                    return Err(ToolError::Operation);
                }
                compile_schema(&tool["inputSchema"])?;
                if let Some(output) = tool.get("outputSchema") {
                    compile_schema(output)?;
                }
                let mut admitted = json!({"name":name,"inputSchema":tool["inputSchema"],"sha256":hex_digest(&serde_json::to_vec(tool).map_err(|_| ToolError::Operation)?)});
                if let Some(description) = tool["description"].as_str() {
                    if description.len() > 2048 {
                        return Err(ToolError::Limit);
                    }
                    admitted["description"] = json!(description);
                }
                if let Some(output) = tool.get("outputSchema") {
                    admitted["outputSchema"] = output.clone();
                }
                tools.insert(name.to_owned(), admitted);
            }
            let Some(next) = page.get("nextCursor") else {
                if allowed.iter().any(|name| !tools.contains_key(name)) {
                    return Err(ToolError::ChangedInput);
                }
                return Ok(tools);
            };
            let next = next
                .as_str()
                .filter(|value| !value.is_empty() && value.len() <= 256)
                .ok_or(ToolError::Operation)?;
            if !cursors.insert(next.to_owned()) {
                return Err(ToolError::Operation);
            }
            cursor = Some(next.to_owned());
        }
        Err(ToolError::Limit)
    }
}

async fn converse<W: Wire>(
    session: &mut Session<W>,
    call: &ToolCall,
    server: &Server,
    result_bytes: usize,
    captured: &std::sync::Mutex<Option<Vec<u8>>>,
) -> Result<String, ToolError> {
    let name = &server.name;
    tokio::time::timeout(Duration::from_millis(u64::from(super::types::MCP_TIMEOUT_MS)), async {
        let init = session
            .request(
                "initialize",
                json!({"protocolVersion":VERSION,"capabilities":{},"clientInfo":{"name":"arany","version":env!("CARGO_PKG_VERSION")}}),
            )
            .await?;
        if init["protocolVersion"] != VERSION || !init["capabilities"]["tools"].is_object() {
            return Err(ToolError::ProtectionUnavailable);
        }
        session.wire.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await?;
        let tools = session.catalog(&server.tools).await?;
        if matches!(call, ToolCall::McpList { .. }) {
            return fit_result(
                json!({"server":name,"protocol":VERSION,"tools":tools.values().collect::<Vec<_>>()}),
                result_bytes,
                captured,
            );
        }
        let ToolCall::McpCall {
            tool,
            schema_digest,
            arguments,
            ..
        } = call
        else {
            return Err(ToolError::Operation);
        };
        let definition = tools.get(tool).ok_or(ToolError::Configuration)?;
        if definition["sha256"].as_str() != Some(schema_digest) {
            return Err(ToolError::ChangedInput);
        }
        let arguments = parse_mcp_arguments(arguments)?;
        if !compile_schema(&definition["inputSchema"])?.is_valid(&arguments) {
            return Err(ToolError::Configuration);
        }
        let reply = session
            .request("tools/call", json!({"name":tool,"arguments":arguments}))
            .await?;
        let is_error = match reply.get("isError") {
            None => false,
            Some(value) => value.as_bool().ok_or(ToolError::Operation)?,
        };
        let normalized = normalized_content(&reply)?;
        let structured = structured_content(&reply, definition, is_error)?;
        fit_result(
            json!({"server":name,"tool":tool,"schema_digest":definition["sha256"],"is_error":is_error,"content":normalized,"structured_content":structured}),
            result_bytes,
            captured,
        )
    })
    .await
    .unwrap_or(Err(ToolError::Timeout(String::new())))
}

fn normalized_content(reply: &Value) -> Result<Vec<Value>, ToolError> {
    let content = reply["content"]
        .as_array()
        .filter(|items| items.len() <= MAX_CONTENT_ITEMS)
        .ok_or(ToolError::Operation)?;
    let mut normalized = Vec::new();
    for item in content {
        match item["type"].as_str() {
            Some("text") => {
                let text = item["text"].as_str().ok_or(ToolError::Operation)?;
                normalized.push(json!({"type":"text","text":text}));
            }
            Some("resource_link") => {
                let uri = item["uri"]
                    .as_str()
                    .filter(|uri| uri.len() <= MAX_RESOURCE_URI_BYTES)
                    .ok_or(ToolError::Operation)?;
                normalized.push(
                    json!({"type":"resource_link","uri":uri,"fetch":"not authorized or performed"}),
                );
            }
            _ => return Err(ToolError::Operation),
        }
    }
    Ok(normalized)
}

fn structured_content<'a>(
    reply: &'a Value,
    definition: &Value,
    is_error: bool,
) -> Result<Option<&'a Value>, ToolError> {
    let structured = reply.get("structuredContent");
    if structured.is_some_and(|value| !value.is_object()) {
        return Err(ToolError::Operation);
    }
    if !is_error
        && let Some(output) = definition.get("outputSchema")
        && !structured.is_some_and(|value| {
            compile_schema(output).is_ok_and(|validator| validator.is_valid(value))
        })
    {
        return Err(ToolError::Operation);
    }
    Ok(structured)
}

pub(super) async fn execute(
    call: &ToolCall,
    config: &Config,
    root: &Path,
    limits: &super::types::ToolLimits,
    captured: &std::sync::Mutex<Option<Vec<u8>>>,
) -> Result<String, ToolError> {
    let (cache_enabled, result_bytes) = (limits.cache_bytes > 0, limits.result_bytes as usize);
    let name = match call {
        ToolCall::McpList { server } | ToolCall::McpCall { server, .. } => server,
        _ => return Err(ToolError::Operation),
    };
    let server = config
        .mcp
        .iter()
        .find(|server| &server.name == name)
        .ok_or(ToolError::Configuration)?;
    let program = server.program.as_ref().ok_or(ToolError::Configuration)?;
    let mut command = payload(
        program,
        &server.args,
        "",
        root,
        super::guard::PayloadEnvironment {
            runtime_grants: &config.runtime_grants,
            cache_enabled,
            limits: &super::types::ToolLimits::default(),
        },
    )?;
    command.envs(&config.locale);
    command.stdin(Stdio::piped());
    let mut child = command.spawn().map_err(|_| ToolError::Operation)?;
    let writer = child.stdin.take().ok_or(ToolError::Operation)?;
    let reader = BufReader::new(child.stdout.take().ok_or(ToolError::Operation)?);
    let stderr = child.stderr.take().ok_or(ToolError::Operation)?;
    let mut stderr_task = tokio::spawn(read_capped(stderr, 16 * 1024));
    let mut session = Session {
        wire: Pipes {
            child,
            writer,
            reader,
        },
        next_id: 1,
        notifications: 0,
    };
    let result = converse(&mut session, call, server, result_bytes, captured).await;
    let mut wire = session.wire;
    let _ = wire.writer.shutdown().await;
    let _ = wire.child.start_kill();
    let teardown = tokio::time::Instant::now() + TEARDOWN_WAIT;
    let reaped = tokio::time::timeout_at(teardown, reap_owned_child(&mut wire.child)).await;
    let drained = drain_stderr(&mut stderr_task, teardown).await;
    if !matches!(reaped, Ok(Ok(_))) || drained.is_none() {
        return Err(ToolError::Operation);
    }
    result
}

fn fit_result(
    result: Value,
    budget: usize,
    captured: &std::sync::Mutex<Option<Vec<u8>>>,
) -> Result<String, ToolError> {
    let text = result.to_string();
    if text.len() <= budget {
        return Ok(text);
    }
    *captured.lock().map_err(|_| ToolError::Operation)? =
        Some(text.as_bytes()[..text.len().min(super::spill::MAX_SPILL_RESULT_BYTES)].to_vec());
    let mut window = super::output::Window::new(budget);
    window.push(text.as_bytes(), super::output::DRAIN_BYTES)?;
    let mut preview =
        json!({"server":result["server"], "content":[], "output_total":window.total()});
    for key in ["is_error", "tool", "schema_digest", "protocol"] {
        if let Some(value) = result.get(key) {
            preview[key] = value.clone();
        }
    }
    super::output::fit_streams(
        &window,
        &super::output::Window::default(),
        budget,
        |text, _| {
            let mut preview = preview.clone();
            preview["content"] = json!([{"type":"text", "text":text}]);
            Ok(preview.to_string())
        },
    )
}

fn compile_schema(value: &Value) -> Result<jsonschema::Validator, ToolError> {
    let bytes = serde_json::to_vec(value).map_err(|_| ToolError::Operation)?;
    if bytes.len() > 32 * 1024 || !value.is_object() {
        return Err(ToolError::Limit);
    }
    let draft = match value.get("$schema").and_then(Value::as_str) {
        None | Some("https://json-schema.org/draft/2020-12/schema") => {
            jsonschema::Draft::Draft202012
        }
        Some("http://json-schema.org/draft-07/schema#") => jsonschema::Draft::Draft7,
        _ => return Err(ToolError::Configuration),
    };
    let mut nodes = 0;
    admit_schema(value, 0, &mut nodes)?;
    jsonschema::options()
        .with_draft(draft)
        .offline()
        .with_pattern_options(
            jsonschema::PatternOptions::regex()
                .size_limit(256 * 1024)
                .dfa_size_limit(256 * 1024),
        )
        .build(value)
        .map_err(|_| ToolError::Configuration)
}

fn admit_schema(value: &Value, depth: usize, nodes: &mut usize) -> Result<(), ToolError> {
    *nodes += 1;
    if depth > 16 || *nodes > 256 {
        return Err(ToolError::Limit);
    }
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if matches!(
                    key.as_str(),
                    "$ref" | "$dynamicRef" | "$recursiveRef" | "$id"
                ) {
                    return Err(ToolError::Configuration);
                }
                if key == "pattern" && value.as_str().is_none_or(|pattern| pattern.len() > 256) {
                    return Err(ToolError::Limit);
                }
                admit_schema(value, depth + 1, nodes)?;
            }
        }
        Value::Array(values) => {
            if values.len() > 64 {
                return Err(ToolError::Limit);
            }
            for value in values {
                admit_schema(value, depth + 1, nodes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_output_preview_preserves_is_error_and_fits_the_rendered_budget() {
        for is_error in [false, true] {
            let original = json!({"server":"local","tool":"test","is_error":is_error,"content":[{"type":"text","text":format!("HEAD{}TAIL", "\n\"中\\".repeat(20000))}],"structured_content":{"value":"untrusted"}});
            let text = original.to_string();
            let captured = Default::default();
            let result = fit_result(original, 4096, &captured).unwrap();
            assert_eq!(captured.into_inner().unwrap().unwrap(), text.as_bytes());
            assert!(result.len() <= 4096);
            let value: Value = serde_json::from_str(&result).unwrap();
            assert_eq!(value["is_error"], is_error);
            assert_eq!(value["output_total"]["bytes"], text.len());
            assert_eq!(value["output_total"]["lines"], 1);
            let preview = value["content"][0]["text"].as_str().unwrap();
            assert!(preview.starts_with("{\"content\":") && preview.ends_with('}'));
            assert!(preview.contains(" lines omitted; "));
            assert!(value.get("structured_content").is_none());
        }
        assert_eq!(
            fit_result(
                json!({"is_error":false,"content":[]}),
                4096,
                &Default::default()
            )
            .unwrap(),
            "{\"content\":[],\"is_error\":false}"
        );
    }
}
