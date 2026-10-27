use super::catalog::{Family, Site};
use super::types::ToolCall;
use crate::session::worker::ChangesetSource;
use std::borrow::Cow;

#[derive(Debug)]
pub struct DisplayFacts<'a> {
    pub family: Family,
    pub site: Site,
    pub verb: &'static str,
    pub subject: Cow<'a, str>,
    pub paths: Vec<&'a str>,
}

impl ToolCall {
    pub fn display_facts(&self) -> DisplayFacts<'_> {
        let (verb, subject, paths): (_, Cow<'_, str>, _) = match self {
            Self::List { path } => ("List", path.as_str().into(), vec![path.as_str()]),
            Self::Read { path, .. } => ("Read", path.as_str().into(), vec![path.as_str()]),
            Self::Search { query, path, .. } => {
                ("Search", query.as_str().into(), vec![path.as_str()])
            }
            Self::Glob { pattern, path, .. } => {
                ("Glob", pattern.as_str().into(), vec![path.as_str()])
            }
            Self::Mkdir { path } => ("Create folder", path.as_str().into(), vec![path.as_str()]),
            Self::Write { path, .. } => ("Write", path.as_str().into(), vec![path.as_str()]),
            Self::Edit { path, .. } => ("Edit", path.as_str().into(), vec![path.as_str()]),
            Self::MultiEdit { file_path, .. } => (
                "MultiEdit",
                file_path.as_str().into(),
                vec![file_path.as_str()],
            ),
            Self::Delete { path, .. } => ("Delete", path.as_str().into(), vec![path.as_str()]),
            Self::Move {
                source,
                destination,
                ..
            } => (
                "Move",
                source.as_str().into(),
                vec![source.as_str(), destination.as_str()],
            ),
            Self::ApplyChangeset {
                source: ChangesetSource::CommandReceipt { changeset, .. },
            } => (
                "Publish",
                changeset
                    .files
                    .first()
                    .map_or("", |file| file.path.as_str())
                    .into(),
                changeset
                    .files
                    .iter()
                    .map(|file| file.path.as_str())
                    .collect(),
            ),
            Self::ApplyChangeset {
                source: ChangesetSource::Rewind { .. },
            } => ("Rewind publication", "".into(), vec![]),
            Self::Command { program, cwd, .. } => {
                ("Command", program.as_str().into(), vec![cwd.as_str()])
            }
            Self::CommandUnguarded { program, cwd, .. } => (
                "Unguarded command",
                program.as_str().into(),
                vec![cwd.as_str()],
            ),
            Self::Skill { name, .. } => ("Skill", name.as_str().into(), vec![]),
            Self::McpList { server } => ("MCP list", server.as_str().into(), vec![]),
            Self::McpCall { server, tool, .. } => {
                ("MCP", format!("{server}.{tool}").into(), vec![])
            }
            Self::GitStatus { path } => ("Git status", path.as_str().into(), vec![path.as_str()]),
            Self::GitDiff { path, .. } => ("Git diff", path.as_str().into(), vec![path.as_str()]),
            Self::GitLog { path, .. } => ("Git log", path.as_str().into(), vec![path.as_str()]),
            Self::GitCommit { paths, .. } => (
                "Git commit",
                paths.join(", ").into(),
                paths.iter().map(String::as_str).collect(),
            ),
            Self::GitBranch { name: None, .. } => ("Git branches", "".into(), vec![]),
            Self::GitBranch {
                name: Some(name), ..
            } => ("Git branch", name.as_str().into(), vec![]),
            Self::GitSwitch { branch } => ("Git switch", branch.as_str().into(), vec![]),
            Self::GitPush { branch, .. } => ("Git push", branch.as_str().into(), vec![]),
            Self::GhPrCreate { repository, .. } => {
                ("PR create", repository.as_str().into(), vec![])
            }
            Self::GhPrList { repository, .. } => ("PR list", repository.as_str().into(), vec![]),
            Self::GhPrView { repository, .. } => ("PR view", repository.as_str().into(), vec![]),
            Self::ParseAgentDefinition { .. } => ("Parse agent definition", "".into(), vec![]),
        };
        DisplayFacts {
            family: self.family(),
            site: self.site(),
            verb,
            subject,
            paths,
        }
    }
}
