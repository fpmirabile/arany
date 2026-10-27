use arany::{DisplayFacts, Family, Site, ToolCall};
use std::borrow::Cow;

pub(super) struct Projection<'a> {
    pub kind: &'static str,
    pub label: &'static str,
    pub detail: Cow<'a, str>,
    pub paths: Vec<&'a str>,
}

pub(super) fn project(call: &ToolCall) -> Projection<'_> {
    let facts = call.display_facts();
    let (kind, label, detail, paths) = match facts.family {
        Family::File => return file(call, facts),
        Family::Git => return git(call, facts),
        Family::Process => (
            if facts.site == Site::Unguarded {
                "execute"
            } else {
                "other"
            },
            if facts.site == Site::Unguarded {
                facts.verb
            } else {
                "Tool"
            },
            facts.subject,
            vec![],
        ),
        Family::Skill => ("read", "Read Skill", facts.subject, vec![]),
        Family::Mcp | Family::Preparation => ("other", "Tool", "".into(), vec![]),
    };
    Projection {
        kind,
        label,
        detail,
        paths,
    }
}

fn file<'a>(call: &'a ToolCall, facts: DisplayFacts<'a>) -> Projection<'a> {
    let (kind, label) = match call {
        ToolCall::Read { .. } => ("read", facts.verb),
        ToolCall::List { .. } | ToolCall::Search { .. } | ToolCall::Glob { .. } => {
            ("search", facts.verb)
        }
        _ => ("other", "Tool"),
    };
    let paths = match call {
        ToolCall::MultiEdit { .. } | ToolCall::Move { .. } | ToolCall::ApplyChangeset { .. } => {
            vec![]
        }
        _ => facts.paths,
    };
    Projection {
        kind,
        label,
        detail: paths.first().copied().unwrap_or("").into(),
        paths,
    }
}

fn git<'a>(call: &'a ToolCall, facts: DisplayFacts<'a>) -> Projection<'a> {
    let (kind, label, detail, paths) = match call {
        ToolCall::GitStatus { .. } | ToolCall::GitDiff { .. } | ToolCall::GitLog { .. } => {
            ("read", "Git read", facts.subject, facts.paths)
        }
        ToolCall::GitBranch { name: None, .. } => ("read", "Git read", facts.subject, vec![]),
        ToolCall::GitCommit { message, .. } => (
            "edit",
            "Git write",
            message.lines().next().unwrap_or("").into(),
            vec![],
        ),
        ToolCall::GitBranch { .. } | ToolCall::GitSwitch { .. } => {
            ("edit", "Git write", facts.subject, vec![])
        }
        ToolCall::GhPrList { .. } => ("read", "List GitHub pull requests", facts.subject, vec![]),
        ToolCall::GhPrView { .. } => ("read", "Inspect GitHub pull request", facts.subject, vec![]),
        ToolCall::GhPrCreate { .. } => ("other", "Tool", "".into(), vec![]),
        _ => ("other", "Tool", facts.subject, vec![]),
    };
    Projection {
        kind,
        label,
        detail,
        paths,
    }
}
