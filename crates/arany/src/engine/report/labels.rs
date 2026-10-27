use crate::tools::catalog::Family;
use crate::tools::{DisplayFacts, ToolCall};

pub(super) fn format(call: &ToolCall) -> String {
    let facts = call.display_facts();
    match facts.family {
        Family::File => file(call, &facts),
        Family::Process | Family::Skill => format!("{} {}", facts.verb, facts.subject),
        Family::Mcp => {
            let verb = if matches!(call, ToolCall::McpList { .. }) {
                "MCP catalog"
            } else {
                facts.verb
            };
            format!("{verb} {}", facts.subject)
        }
        Family::Git => git(call, &facts),
        Family::Preparation => "Agent definition preparation".into(),
    }
}

fn file(call: &ToolCall, facts: &DisplayFacts<'_>) -> String {
    match call {
        ToolCall::Move { .. } => format!("move {} to {}", facts.paths[0], facts.paths[1]),
        ToolCall::Glob { .. } => format!("glob {} in {}", facts.subject, facts.paths[0]),
        ToolCall::ApplyChangeset { .. } => "Command changeset publication".into(),
        _ => format!("{} {}", call.label(), facts.paths[0]),
    }
}

fn git(call: &ToolCall, facts: &DisplayFacts<'_>) -> String {
    match call {
        ToolCall::GitPush { remote, .. } => format!("host Git push {} to {remote}", facts.subject),
        ToolCall::GhPrCreate { .. } => format!("host PR creation in {}", facts.subject),
        ToolCall::GhPrList { .. } => format!("host PR list in {}", facts.subject),
        ToolCall::GhPrView { number, .. } => format!("host PR view #{number} in {}", facts.subject),
        ToolCall::GitCommit { .. } => "Git commit".into(),
        ToolCall::GitBranch { name: None, .. } => "Git branch list".into(),
        _ => format!("{} {}", facts.verb, facts.subject),
    }
}
