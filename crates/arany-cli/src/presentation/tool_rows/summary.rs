use arany::{Family, Site, ToolCall};

pub(super) fn operation(call: &ToolCall) -> String {
    match call.family() {
        Family::File => file(call),
        Family::Process => process(call),
        Family::Skill => skill(call),
        Family::Mcp => mcp(call),
        Family::Git => match call.site() {
            Site::Guard => git(call),
            Site::Host => host(call),
            Site::InProcess | Site::Unguarded => unreachable!("Git family execution site"),
        },
        Family::Preparation => preparation(call),
    }
}

fn file(call: &ToolCall) -> String {
    match call {
        ToolCall::List { path } => format!("List({path})"),
        ToolCall::Read { path, .. } => format!("Read({path})"),
        ToolCall::Search {
            path,
            query,
            regex,
            include,
            case_sensitive,
            offset,
            limit,
        } => {
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
        ToolCall::Glob { pattern, path, .. } => format!("Glob({pattern}, {path})"),
        ToolCall::Delete { path, .. } => format!("Delete({path})"),
        ToolCall::Move {
            source,
            destination,
            ..
        } => format!("Move({source} → {destination})"),
        ToolCall::Mkdir { path } => format!("Create folder({path})"),
        ToolCall::Write { path, .. } => format!("Write({path})"),
        ToolCall::Edit { path, .. } => format!("Edit({path})"),
        ToolCall::MultiEdit {
            file_path, edits, ..
        } => format!("MultiEdit({file_path}, {} edits)", edits.len()),
        ToolCall::ApplyChangeset {
            source: arany::worker::ChangesetSource::Rewind { .. },
        } => "Rewind publication()".into(),
        ToolCall::ApplyChangeset {
            source: arany::worker::ChangesetSource::WorkerHandover { .. },
        } => "Worker changeset publication()".into(),
        ToolCall::ApplyChangeset {
            source: arany::worker::ChangesetSource::CommandReceipt { changeset, .. },
        } => format!("Publish({} command file changes)", changeset.files.len()),
        _ => unreachable!("file family operation"),
    }
}

fn process(call: &ToolCall) -> String {
    match call {
        ToolCall::Command { program, args, .. } => format!("Command({program} {})", args.join(" ")),
        ToolCall::CommandUnguarded { program, args, .. } => {
            format!("Unguarded command({program} {})", args.join(" "))
        }
        _ => unreachable!("process family operation"),
    }
}

fn skill(call: &ToolCall) -> String {
    match call {
        ToolCall::Skill { name, .. } => format!("Skill({name})"),
        _ => unreachable!("Skill family operation"),
    }
}

fn mcp(call: &ToolCall) -> String {
    match call {
        ToolCall::McpList { server } => format!("MCP list({server})"),
        ToolCall::McpCall { server, tool, .. } => format!("MCP({server} / {tool})"),
        _ => unreachable!("MCP family operation"),
    }
}

fn git(call: &ToolCall) -> String {
    match call {
        ToolCall::GitStatus { path } => format!("Git status({path})"),
        ToolCall::GitDiff { path, .. } => format!("Git diff({path})"),
        ToolCall::GitLog { path, .. } => format!("Git log({path})"),
        ToolCall::GitCommit {
            paths,
            staged: true,
            ..
        } if paths.is_empty() => "Git commit(staged)".into(),
        ToolCall::GitCommit { paths, .. } => format!("Git commit({})", paths.join(", ")),
        ToolCall::GitBranch { name: None, .. } => "Git branches()".into(),
        ToolCall::GitBranch {
            name: Some(name),
            start_point,
            ..
        } => match start_point {
            Some(start) => format!("Git branch({name} from {start})"),
            None => format!("Git branch({name})"),
        },
        ToolCall::GitSwitch { branch } => format!("Git switch({branch})"),
        _ => unreachable!("guarded Git family operation"),
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

fn preparation(call: &ToolCall) -> String {
    match call {
        ToolCall::PrepareWorker { .. } => "Worker data preparation()".into(),
        ToolCall::ParseAgentDefinition { .. } => "Parse agent definition()".into(),
        _ => unreachable!("preparation family operation"),
    }
}
