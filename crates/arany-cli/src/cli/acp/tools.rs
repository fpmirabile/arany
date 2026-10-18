use arany::{RunId, SessionId, ToolCall};
use arany_cli::protocol::{methods::*, rpc::encode_line};
use serde_json::{Value, json};

pub(super) const UNKNOWN: &str = "Outcome unknown: Arany could not confirm whether this effect happened. It was not retried; inspect the Workspace before continuing.";

pub(super) fn content(text: &str, session: SessionId) -> Value {
    let text = bounded(text, 64 * 1024, session);
    fenced(&text)
}

pub(super) fn approval_content(lines: &[String]) -> Value {
    let text = lines
        .iter()
        .map(|line| crate::presentation::escape_terminal(line))
        .collect::<Vec<_>>()
        .join("\n");
    fenced(&text)
}

fn fenced(text: &str) -> Value {
    let longest = text.split(|ch| ch != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(3.max(longest + 1));
    json!([{"type":"content", "content":{"type":"text", "text":format!("{fence}\n{text}\n{fence}")}}])
}

fn bounded(text: &str, limit: usize, session: SessionId) -> String {
    let mut safe = String::new();
    for line in text.split_inclusive('\n') {
        let (line, newline) = line.strip_suffix('\n').map_or((line, false), |s| (s, true));
        // Escape before editor markdown rendering, retaining only structural newlines.
        let mut end = line.len().min(limit);
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        let escaped = crate::presentation::escape_terminal(&line[..end]);
        let remaining = limit.saturating_sub(safe.len());
        let mut take = escaped.len().min(remaining);
        while !escaped.is_char_boundary(take) {
            take -= 1;
        }
        safe.push_str(&escaped[..take]);
        if end < line.len() || take < escaped.len() || (newline && safe.len() == limit) {
            safe.push_str(&format!("\n[truncated; see arany show {session}]"));
            break;
        }
        if newline {
            safe.push('\n');
        }
    }
    safe
}

struct ToolItem<'a> {
    id: &'a str,
    kind: &'static str,
    label: &'static str,
    detail: &'a str,
    paths: Vec<&'a str>,
    status: &'a ItemStatus,
    disposition: &'a Option<arany::ToolDisposition>,
    output: &'a Option<String>,
    raw: Option<Value>,
}

pub(super) fn project(item: &Item, cwd: &str, session: SessionId, started: bool) -> Option<Value> {
    let tool = match item {
        Item::CommandExecution {
            id,
            command,
            args,
            cwd: command_cwd,
            status,
            disposition,
            output,
            ..
        } => ToolItem {
            id,
            kind: "execute",
            label: "Run command",
            detail: command,
            paths: vec![command_cwd],
            status,
            disposition,
            output,
            raw: Some(
                json!({"operation":"command", "program":command,"args":args,"cwd":command_cwd}),
            ),
        },
        Item::FileChange {
            id,
            changes,
            status,
            disposition,
            output,
        } => ToolItem {
            id,
            kind: if !changes.is_empty() && changes.iter().all(|c| c.kind == FileChangeKind::Delete)
            {
                "delete"
            } else {
                "edit"
            },
            label: "Change files",
            detail: changes.first().map_or("", |c| c.path.as_str()),
            paths: changes.iter().map(|c| c.path.as_str()).collect(),
            status,
            disposition,
            output,
            raw: None,
        },
        Item::McpToolCall {
            id,
            server,
            status,
            disposition,
            output,
            ..
        } => ToolItem {
            id,
            kind: "other",
            label: "MCP",
            detail: server,
            paths: vec![],
            status,
            disposition,
            output,
            raw: None,
        },
        Item::ToolCall {
            id,
            call,
            status,
            disposition,
            output,
        } => {
            let typed: ToolCall = serde_json::from_value(call.clone()).ok()?;
            let (kind, label) = match typed {
                ToolCall::CommandUnguarded { .. } => ("execute", "Unguarded command"),
                ToolCall::Read { .. } => ("read", "Read"),
                ToolCall::GitStatus { .. } | ToolCall::GitDiff { .. } | ToolCall::GitLog { .. } => {
                    ("read", "Git read")
                }
                ToolCall::Skill { .. } => ("read", "Read Skill"),
                ToolCall::List { .. } => ("search", "List"),
                ToolCall::Search { .. } => ("search", "Search"),
                _ => ("other", "Tool"),
            };
            let path = call["path"].as_str();
            ToolItem {
                id,
                kind,
                label,
                detail: path
                    .or_else(|| call["name"].as_str())
                    .or_else(|| call["program"].as_str())
                    .unwrap_or(""),
                paths: path.into_iter().collect(),
                status,
                disposition,
                output,
                raw: Some(call.clone()),
            }
        }
        _ => return None,
    };
    let uncertain = *tool.status == ItemStatus::Uncertain;
    let mut update = json!({
        "sessionUpdate": if started {"tool_call"} else {"tool_call_update"},
        "toolCallId": tool.id,
        "title": bounded(&format!("{} {}", tool.label, tool.detail), 256, session).replace('\n', " "),
        "kind": tool.kind,
        "status": if started {"pending"} else if *tool.status == ItemStatus::Completed {"completed"} else {"failed"},
        "locations": tool.paths.into_iter().map(|path| json!({"path":display_path(cwd, path)})).collect::<Vec<_>>()
    });
    if started {
        if let Some(raw) = tool.raw {
            update["rawInput"] = raw;
        }
    } else {
        let disposition = tool
            .disposition
            .map(|d| serde_json::to_value(d).expect("disposition"))
            .unwrap_or(Value::Null);
        let text = format!(
            "{}{}{}",
            if uncertain {
                UNKNOWN
            } else {
                disposition.as_str().unwrap_or("failed")
            },
            if tool.output.is_some() { "\n" } else { "" },
            tool.output.as_deref().unwrap_or("")
        );
        update["content"] = content(&text, session);
        if uncertain {
            update["_meta"]["arany.status"] = json!("uncertain");
        }
    }
    Some(update)
}

fn display_path(cwd: &str, path: &str) -> String {
    if path.is_empty() || path == "." {
        cwd.into()
    } else {
        format!("{}/{path}", cwd.trim_end_matches('/'))
    }
}

pub(super) fn notification(session: SessionId, mut update: Value, run: RunId) -> Value {
    let wrap = |update| json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":update}});
    let message = wrap(update.clone());
    if encode_line(&message).is_ok() {
        return message;
    }
    for field in ["rawOutput", "rawInput", "locations"] {
        update.as_object_mut().expect("update").remove(field);
    }
    update["content"] = content(
        &format!("[truncated; see arany show {session}]\nRun {run}"),
        session,
    );
    wrap(update)
}
