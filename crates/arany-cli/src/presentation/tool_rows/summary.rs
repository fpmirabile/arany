use arany::{Family, Site, ToolCall};

pub(super) fn operation(call: &ToolCall) -> String {
    match call.family() {
        Family::File => file(call),
        Family::Process => process(call),
        Family::Skill => simple(call),
        Family::Mcp => mcp(call),
        Family::Git => match call.site() {
            Site::Guard => git(call),
            Site::Host => host(call),
            Site::InProcess | Site::Unguarded => unreachable!("Git family execution site"),
        },
        Family::Preparation => simple(call),
    }
}

fn file(call: &ToolCall) -> String {
    let facts = call.display_facts();
    match call {
        ToolCall::Search {
            regex,
            include,
            case_sensitive,
            offset,
            limit,
            ..
        } => {
            let query = &facts.subject;
            let path = facts.paths[0];
            let mut options = String::new();
            if *regex {
                options.push_str(", regex");
            }
            if let Some(include) = include {
                options.push_str(&format!(", include {include}"));
            }
            if !case_sensitive {
                options.push_str(", ignore case");
            }
            if *offset > 0 {
                options.push_str(&format!(", offset {offset}"));
            }
            if *limit != arany::MAX_SEARCH_ROWS {
                options.push_str(&format!(", limit {limit}"));
            }
            format!("Search({query}, {path}{options})")
        }
        ToolCall::Glob { .. } => format!("Glob({}, {})", facts.subject, facts.paths[0]),
        ToolCall::Move { .. } => format!("Move({} → {})", facts.paths[0], facts.paths[1]),
        ToolCall::MultiEdit { edits, .. } => {
            format!("MultiEdit({}, {} edits)", facts.subject, edits.len())
        }
        ToolCall::ApplyChangeset {
            source: arany::worker::ChangesetSource::Rewind { .. },
        } => "Rewind publication()".into(),
        ToolCall::ApplyChangeset {
            source: arany::worker::ChangesetSource::CommandReceipt { .. },
        } => format!("Publish({} command file changes)", facts.paths.len()),
        _ => format!("{}({})", facts.verb, facts.subject),
    }
}

fn process(call: &ToolCall) -> String {
    match call {
        ToolCall::Command { program, args, .. } => {
            format!("Command({program} {})", command_line(args))
        }
        ToolCall::CommandUnguarded { program, args, .. } => {
            format!("Unguarded command({program} {})", command_line(args))
        }
        _ => unreachable!("process family operation"),
    }
}

// Shows a multi-line script argument as its first non-blank line plus "…".
fn command_line(args: &[String]) -> String {
    let mut shown = Vec::with_capacity(args.len());
    for (index, arg) in args.iter().enumerate() {
        if !arg.contains('\n') {
            shown.push(arg.clone());
            continue;
        }
        let mut lines = arg.lines().skip_while(|line| line.trim().is_empty());
        let first = lines.next().unwrap_or_default();
        let more = lines.any(|line| !line.trim().is_empty())
            || args[index + 1..].iter().any(|rest| !rest.trim().is_empty());
        shown.push(if more {
            format!("{first} …")
        } else {
            first.into()
        });
        break;
    }
    shown.join(" ")
}

fn simple(call: &ToolCall) -> String {
    let facts = call.display_facts();
    format!("{}({})", facts.verb, facts.subject)
}

fn mcp(call: &ToolCall) -> String {
    match call {
        ToolCall::McpList { server } => format!("MCP list({server})"),
        ToolCall::McpCall { server, tool, .. } => format!("MCP({server} / {tool})"),
        _ => unreachable!("MCP family operation"),
    }
}

fn git(call: &ToolCall) -> String {
    let facts = call.display_facts();
    match call {
        ToolCall::GitCommit {
            paths,
            staged: true,
            ..
        } if paths.is_empty() => "Git commit(staged)".into(),
        ToolCall::GitCommit { .. } => format!("Git commit({})", facts.subject),
        ToolCall::GitBranch {
            name: Some(name),
            start_point,
            ..
        } => match start_point {
            Some(start) => format!("Git branch({name} from {start})"),
            None => format!("Git branch({name})"),
        },
        _ => format!("{}({})", facts.verb, facts.subject),
    }
}

fn host(call: &ToolCall) -> String {
    match call {
        ToolCall::GitPush { remote, branch } => format!("host Git push({branch} → {remote})"),
        ToolCall::GhPrCreate { repository, .. } => format!("host PR create({repository})"),
        ToolCall::GhPrList { repository, .. } => format!("host PR list({repository})"),
        ToolCall::GhPrView { repository, number } => {
            format!("host PR view #{number}({repository})")
        }
        _ => unreachable!("host Git family operation"),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ReadGroup {
    Pattern,
    File,
    Directory,
    Git,
    Skill,
    Mcp,
}

pub(super) fn read_group(call: &ToolCall) -> Option<ReadGroup> {
    match call.family() {
        Family::File => {
            if matches!(call, ToolCall::Search { .. } | ToolCall::Glob { .. }) {
                Some(ReadGroup::Pattern)
            } else if matches!(call, ToolCall::Read { .. }) {
                Some(ReadGroup::File)
            } else if matches!(call, ToolCall::List { .. }) {
                Some(ReadGroup::Directory)
            } else {
                None
            }
        }
        Family::Git => matches!(
            call,
            ToolCall::GitStatus { .. }
                | ToolCall::GitDiff { .. }
                | ToolCall::GitLog { .. }
                | ToolCall::GitBranch { name: None, .. }
        )
        .then_some(ReadGroup::Git),
        Family::Skill => Some(ReadGroup::Skill),
        Family::Mcp => matches!(call, ToolCall::McpList { .. }).then_some(ReadGroup::Mcp),
        Family::Process | Family::Preparation => None,
    }
}

pub(super) enum ResultKind {
    Read,
    Applied,
    Command,
    GitCommit,
    GitSwitch,
    GitWrite,
    Host,
}

pub(super) fn result_kind(call: &ToolCall) -> ResultKind {
    match call.family() {
        Family::File if read_group(call).is_none() => ResultKind::Applied,
        Family::Process => ResultKind::Command,
        Family::Git if call.site() == Site::Host => ResultKind::Host,
        Family::Git if matches!(call, ToolCall::GitCommit { .. }) => ResultKind::GitCommit,
        Family::Git if matches!(call, ToolCall::GitSwitch { .. }) => ResultKind::GitSwitch,
        Family::Git if read_group(call).is_none() => ResultKind::GitWrite,
        Family::File | Family::Git | Family::Skill | Family::Mcp | Family::Preparation => {
            ResultKind::Read
        }
    }
}

pub(super) fn uncertain_git_write(call: &ToolCall) -> bool {
    matches!(
        call,
        ToolCall::GitCommit { .. } | ToolCall::GitBranch { .. } | ToolCall::GitSwitch { .. }
    )
}
