use super::super::escape_terminal;
use super::changeset_version_lines;
use arany::{Family, Site, ToolCall};

pub(super) fn call_lines(call: &ToolCall) -> Vec<String> {
    match call.family() {
        Family::File => file(call),
        Family::Process => process(call),
        Family::Skill | Family::Web => fallback(call),
        Family::Mcp => mcp(call),
        Family::Git => match call.site() {
            Site::Host => host(call),
            Site::Git => git(call),
            Site::InProcess | Site::Guard | Site::Unguarded => {
                unreachable!("Git family execution site")
            }
        },
        Family::Preparation => fallback(call),
    }
}

fn host(call: &ToolCall) -> Vec<String> {
    let mut lines = Vec::new();
    match call {
        ToolCall::GitPush { remote, branch } => {
            lines.push(format!("Destination: {}", escape_terminal(remote)));
            lines.push(format!("Branch: {}", escape_terminal(branch)));
        }
        ToolCall::GhPrCreate {
            repository,
            base,
            head,
            title,
            body,
            draft,
        } => {
            lines.push(
                "PR uses the remote head branch at creation, not a fixed local commit.".into(),
            );
            for (label, value) in [
                ("Repository", repository),
                ("Base", base),
                ("Head", head),
                ("Title", title),
            ] {
                lines.push(format!("{label}: {}", escape_terminal(value)));
            }
            lines.push(format!("Draft: {draft}"));
            lines.push("Body:".into());
            lines.extend(
                body.lines()
                    .map(|line| format!("  {}", escape_terminal(line))),
            );
        }
        ToolCall::GhPrList {
            repository,
            state,
            base,
            head,
            limit,
        } => {
            lines.push("Private GitHub PR data may be sent to the current Provider.".into());
            lines.push(format!("Repository: {}", escape_terminal(repository)));
            lines.push(format!("State: {}", state.as_str()));
            lines.push(format!(
                "Base: {}",
                escape_terminal(base.as_deref().unwrap_or("any"))
            ));
            lines.push(format!(
                "Head: {}",
                escape_terminal(head.as_deref().unwrap_or("any"))
            ));
            lines.push(format!("Limit: {limit}"));
        }
        ToolCall::GhPrView { repository, number } => {
            lines.push("Private GitHub PR data may be sent to the current Provider.".into());
            lines.push(format!("Repository: {}", escape_terminal(repository)));
            lines.push(format!("Pull request: #{number}"));
        }
        _ => unreachable!("host Git family operation"),
    }
    lines
}

fn file(call: &ToolCall) -> Vec<String> {
    let mut lines = Vec::new();
    match call {
        ToolCall::Write {
            path,
            expected_digest,
            content,
        } => {
            lines.push(format!(
                "{} {}",
                if expected_digest.is_some() {
                    "Replace"
                } else {
                    "Create"
                },
                escape_terminal(path)
            ));
            lines.push("Content:".into());
            lines.extend(
                content
                    .split('\n')
                    .map(|line| format!("  {}", escape_terminal(line))),
            );
        }
        ToolCall::Edit { path, old, new, .. } => {
            lines.push(format!(
                "Edit {} · only if the file is unchanged",
                escape_terminal(path)
            ));
            push_replacement(&mut lines, old, new);
        }
        ToolCall::MultiEdit {
            file_path, edits, ..
        } => {
            lines.push(format!(
                "MultiEdit {} · only if the file is unchanged",
                escape_terminal(file_path)
            ));
            for (index, edit) in edits.iter().enumerate() {
                lines.push(format!(
                    "Edit {} · {}",
                    index + 1,
                    if edit.replace_all {
                        "all matches"
                    } else {
                        "one match"
                    }
                ));
                push_replacement(&mut lines, &edit.old_string, &edit.new_string);
            }
        }
        ToolCall::Delete { path, .. } => lines.push(format!(
            "Delete {} · only if the file is unchanged",
            escape_terminal(path)
        )),
        ToolCall::Move {
            source,
            destination,
            ..
        } => {
            lines.push(format!(
                "Move {} → {}",
                escape_terminal(source),
                escape_terminal(destination)
            ));
            lines.push("Only if the source is unchanged and the destination is absent".into());
        }
        ToolCall::Mkdir { path } => {
            lines.push(format!("Create folder {}", escape_terminal(path)));
        }
        ToolCall::ApplyChangeset {
            source: arany::worker::ChangesetSource::CommandReceipt { changeset, .. },
        } => push_changeset(&mut lines, changeset),
        _ => return fallback(call),
    }
    lines
}

fn push_replacement(lines: &mut Vec<String>, old: &str, new: &str) {
    lines.push("Remove:".into());
    lines.extend(
        old.split('\n')
            .map(|line| format!("- {}", escape_terminal(line))),
    );
    lines.push("Insert:".into());
    lines.extend(
        new.split('\n')
            .map(|line| format!("+ {}", escape_terminal(line))),
    );
}

fn push_changeset(lines: &mut Vec<String>, changeset: &arany::CommandChangeset) {
    lines.push(format!(
        "Publish {} command file changes",
        changeset.files.len()
    ));
    for file in &changeset.files {
        let operation = match (&file.before, &file.after) {
            (None, Some(_)) => "Create",
            (Some(_), None) => "Delete",
            _ => "Replace",
        };
        lines.push(format!("{operation} {}", escape_terminal(&file.path)));
        if let Some(before) = &file.before {
            changeset_version_lines(lines, "Before", "-", before);
        }
        if let Some(after) = &file.after {
            changeset_version_lines(lines, "After", "+", after);
        }
    }
}

fn process(call: &ToolCall) -> Vec<String> {
    let ToolCall::Command {
        program,
        args,
        cwd,
        resource_class,
        timeout_ms,
        additional_reads,
    } = call
    else {
        return fallback(call);
    };
    let mut lines = vec![format!(
        "Run {} in {}",
        escape_terminal(program),
        if cwd.is_empty() {
            "project root".into()
        } else {
            escape_terminal(cwd)
        }
    )];
    if let Some(class) = resource_class {
        let label = match class {
            arany::ToolResourceClass::Default => "Default",
            arany::ToolResourceClass::Build => "Build",
        };
        lines.push(format!("Resources: {label}"));
    }
    if let Some(timeout) = timeout_ms {
        lines.push(format!(
            "Timeout: {}",
            super::format_duration(u64::from(*timeout))
        ));
    }
    lines.push("Offline isolated copy · changes are validated before publication".into());
    lines.extend(args.iter().enumerate().map(|(index, argument)| {
        format!(
            "Argument {}: {}",
            index + 1,
            escape_terminal(&serde_json::to_string(argument).expect("argument string"))
        )
    }));
    if let Some(request) = additional_reads {
        lines.push(
            "Also reads and runs, never writes, these host folders and everything in them:".into(),
        );
        lines.extend(
            request
                .paths
                .iter()
                .map(|path| format!("  {}", escape_terminal(path))),
        );
        lines.push(format!(
            "Reason: {}",
            escape_terminal(&request.justification)
        ));
    }
    lines
}

fn mcp(call: &ToolCall) -> Vec<String> {
    let ToolCall::McpCall {
        server,
        tool,
        arguments,
        ..
    } = call
    else {
        return fallback(call);
    };
    vec![
        format!(
            "Call {} / {}",
            escape_terminal(server),
            escape_terminal(tool)
        ),
        format!("Arguments: {}", escape_terminal(arguments)),
    ]
}

fn git(call: &ToolCall) -> Vec<String> {
    let mut lines = Vec::new();
    match call {
        ToolCall::GitCommit {
            message,
            paths,
            staged,
        } => {
            lines.push(if *staged {
                "Commit the staged changes to the current branch".into()
            } else {
                "Commit the working-tree contents of these paths to the current branch:".into()
            });
            lines.extend(
                paths
                    .iter()
                    .map(|path| format!("  {}", escape_terminal(path))),
            );
            lines.push("Message:".into());
            lines.extend(
                message
                    .split('\n')
                    .map(|line| format!("  {}", escape_terminal(line))),
            );
            lines.push("Repository hooks and configuration do not run".into());
        }
        ToolCall::GitBranch {
            name: Some(name),
            start_point,
            ..
        } => lines.push(format!(
            "Create branch {} at {} without switching",
            escape_terminal(name),
            escape_terminal(start_point.as_deref().unwrap_or("HEAD"))
        )),
        ToolCall::GitSwitch { branch } => {
            lines.push(format!("Switch to branch {}", escape_terminal(branch)));
            lines.push(
                "Changed Workspace files are replaced; local changes they would overwrite refuse"
                    .into(),
            );
        }
        _ => return fallback(call),
    }
    lines
}

fn fallback(call: &ToolCall) -> Vec<String> {
    vec![escape_terminal(
        &serde_json::to_string(call).expect("typed action"),
    )]
}
