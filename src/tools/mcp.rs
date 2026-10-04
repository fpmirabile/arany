use super::ToolError;
use super::config::Config;
use super::guard::{payload, read_capped, read_line_capped};
use super::skills::parse_json;
use super::types::{MAX_TOOL_RESULT_BYTES, ToolCall, hex_digest};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

const FRAME_BYTES: usize = 128 * 1024;
const VERSION: &str = "2025-11-25";

struct Connection {
    child: Child,
    writer: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: u64,
    notifications: usize,
}

impl Connection {
    async fn send(&mut self, message: Value) -> Result<(), ToolError> {
        let bytes = serde_json::to_vec(&message).map_err(|_| ToolError::Operation)?;
        if bytes.len() > FRAME_BYTES {
            return Err(ToolError::Limit);
        }
        self.writer
            .write_all(&bytes)
            .await
            .map_err(|_| ToolError::Operation)?;
        self.writer
            .write_all(b"\n")
            .await
            .map_err(|_| ToolError::Operation)
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, ToolError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        loop {
            let bytes = read_line_capped(&mut self.reader, FRAME_BYTES).await?;
            let mut message = parse_json(&bytes, FRAME_BYTES)?;
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
                    self.send(response).await?;
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
                schema(&tool["inputSchema"])?;
                if let Some(output) = tool.get("outputSchema") {
                    schema(output)?;
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

pub(super) async fn execute(call: &ToolCall, config: &Config) -> Result<String, ToolError> {
    let name = match call {
        ToolCall::McpList { server } | ToolCall::McpCall { server, .. } => server,
        _ => return Err(ToolError::Operation),
    };
    let server = config
        .mcp
        .iter()
        .find(|server| &server.name == name)
        .ok_or(ToolError::Configuration)?;
    let mut command = payload(&server.program, &server.args, "")?;
    command.stdin(Stdio::piped());
    let mut child = command.spawn().map_err(|_| ToolError::Operation)?;
    let writer = child.stdin.take().ok_or(ToolError::Operation)?;
    let reader = BufReader::new(child.stdout.take().ok_or(ToolError::Operation)?);
    let stderr = child.stderr.take().ok_or(ToolError::Operation)?;
    let mut stderr_task = tokio::spawn(read_capped(stderr, 16 * 1024));
    let mut connection = Connection {
        child,
        writer,
        reader,
        next_id: 1,
        notifications: 0,
    };
    let result = tokio::time::timeout(Duration::from_secs(40), async {
        let init = connection.request("initialize", json!({"protocolVersion":VERSION,"capabilities":{},"clientInfo":{"name":"arany","version":env!("CARGO_PKG_VERSION")}})).await?;
        if init["protocolVersion"] != VERSION || !init["capabilities"]["tools"].is_object() { return Err(ToolError::ProtectionUnavailable); }
        connection.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await?;
        let tools = connection.catalog(&server.tools).await?;
        if matches!(call, ToolCall::McpList { .. }) {
            let result = json!({"server":name,"protocol":VERSION,"tools":tools.values().collect::<Vec<_>>()}).to_string();
            if result.len() > MAX_TOOL_RESULT_BYTES { return Err(ToolError::Limit); }
            return Ok(result);
        }
        let ToolCall::McpCall { tool, schema_digest, arguments, .. } = call else { return Err(ToolError::Operation); };
        let definition = tools.get(tool).ok_or(ToolError::Configuration)?;
        if definition["sha256"].as_str() != Some(schema_digest) { return Err(ToolError::ChangedInput); }
        let arguments = parse_json(arguments.as_bytes(), 8 * 1024)?;
        if !arguments.is_object() || !schema(&definition["inputSchema"])?.is_valid(&arguments) { return Err(ToolError::Configuration); }
        let reply = connection.request("tools/call", json!({"name":tool,"arguments":arguments})).await?;
        let is_error = match reply.get("isError") { None => false, Some(value) => value.as_bool().ok_or(ToolError::Operation)? };
        let content = reply["content"].as_array().filter(|items| items.len() <= 64).ok_or(ToolError::Operation)?;
        let mut normalized = Vec::new();
        for item in content {
            match item["type"].as_str() {
                Some("text") => { let text = item["text"].as_str().ok_or(ToolError::Operation)?; normalized.push(json!({"type":"text","text":text})); }
                Some("resource_link") => {
                    let uri = item["uri"].as_str().filter(|uri| uri.len() <= 2048).ok_or(ToolError::Operation)?;
                    normalized.push(json!({"type":"resource_link","uri":uri,"fetch":"not authorized or performed"}));
                }
                _ => return Err(ToolError::Operation),
            }
        }
        let structured = reply.get("structuredContent");
        if structured.is_some_and(|value| !value.is_object()) { return Err(ToolError::Operation); }
        if !is_error && let Some(output) = definition.get("outputSchema")
            && !structured.is_some_and(|value| schema(output).is_ok_and(|validator| validator.is_valid(value))) { return Err(ToolError::Operation); }
        let result = json!({"server":name,"tool":tool,"schema_digest":definition["sha256"],"is_error":is_error,"content":normalized,"structured_content":structured}).to_string();
        if result.len() > MAX_TOOL_RESULT_BYTES { return Err(ToolError::Limit); }
        Ok(result)
    }).await.unwrap_or(Err(ToolError::Limit));
    let _ = connection.writer.shutdown().await;
    let _ = connection.child.start_kill();
    let reaped = tokio::time::timeout(Duration::from_secs(2), connection.child.wait()).await;
    let drained = tokio::time::timeout(Duration::from_secs(2), &mut stderr_task).await;
    if drained.is_err() {
        stderr_task.abort();
        let _ = stderr_task.await;
    }
    if !matches!(reaped, Ok(Ok(_))) || !matches!(drained, Ok(Ok(Ok(_)))) {
        return Err(ToolError::Operation);
    }
    result
}

fn schema(value: &Value) -> Result<jsonschema::Validator, ToolError> {
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
