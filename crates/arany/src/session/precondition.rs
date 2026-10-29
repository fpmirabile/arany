use super::{AgentRunId, RunId, RunView, SessionView};
use crate::session::worker::ChangesetSource;
use crate::tools::catalog::Site;
use crate::tools::{ToolCall, ToolDisposition, ToolObservation};

pub(crate) fn digest_known(
    view: &SessionView,
    run_id: RunId,
    actor: AgentRunId,
    call: &ToolCall,
) -> bool {
    call.pending_precondition()
        .is_none_or(|path| run_digest(view, run_id, actor, path).is_some())
}

// Fills only an omitted digest; Write keeps null, and so creates a missing file, when none is known.
pub(crate) fn fill_expected_digest(
    view: &SessionView,
    run_id: RunId,
    actor: AgentRunId,
    call: &mut ToolCall,
) {
    if let Some((path, digest)) = call.precondition_mut()
        && digest.is_none()
    {
        *digest = run_digest(view, run_id, actor, path);
    }
}

fn run_digest(view: &SessionView, run_id: RunId, actor: AgentRunId, path: &str) -> Option<String> {
    let run = view.runs.iter().rev().find(|run| run.id == run_id)?;
    current_digest(run, actor, path)
}

// The newest committed SHA-256 of `path` that `actor` observed in this Run, unless a later effect may have changed it.
fn current_digest(run: &RunView, actor: AgentRunId, path: &str) -> Option<String> {
    let mut digest = run.config.as_ref().and_then(|config| {
        config
            .include_sources
            .iter()
            .find(|source| source.path == path)
            .map(|source| crate::validate::hex(&source.digest))
    });
    let mut tools = run
        .tools
        .iter()
        .chain(run.agents.iter().flat_map(|agent| &agent.tools))
        .filter(|tool| tool.finished_sequence.is_some())
        .collect::<Vec<_>>();
    tools.sort_by_key(|tool| tool.finished_sequence);
    for observation in tools.iter().filter_map(|tool| tool.observation.as_ref()) {
        if let Some(next) = observed_digest(observation, actor, path) {
            digest = next;
        }
    }
    digest
}

// Committed observations already passed replay validation. None leaves the digest unchanged;
// Some(None) means the path's content is unknown or absent.
fn observed_digest(
    observation: &ToolObservation,
    actor: AgentRunId,
    path: &str,
) -> Option<Option<String>> {
    let call = &observation.intent.call;
    if call.mutates() && matches!(call.facts().site, Site::Host | Site::Unguarded)
        || matches!(
            call,
            ToolCall::McpCall { .. }
                | ToolCall::GitSwitch { .. }
                | ToolCall::ApplyChangeset {
                    source: ChangesetSource::Rewind { .. }
                }
        )
    {
        return (observation.disposition != ToolDisposition::Denied).then_some(None);
    }
    let after = match call {
        ToolCall::Read { path: read, .. } if read == path => {
            return (observation.intent.agent_run_id == actor
                && observation.disposition == ToolDisposition::Succeeded)
                .then(|| output_digest(&observation.output));
        }
        ToolCall::Write { path: written, .. }
        | ToolCall::Edit { path: written, .. }
        | ToolCall::MultiEdit {
            file_path: written, ..
        } if written == path => output_digest(&observation.output),
        ToolCall::Delete { path: deleted, .. } if deleted == path => None,
        ToolCall::Move { source, .. } if source == path => None,
        ToolCall::Move {
            destination,
            expected_digest,
            ..
        } if destination == path => expected_digest.clone(),
        ToolCall::ApplyChangeset {
            source: ChangesetSource::CommandReceipt { changeset, .. },
        } => changeset
            .files
            .iter()
            .find(|change| change.path == path)?
            .after
            .as_ref()
            .map(|version| version.sha256.clone()),
        _ => return None,
    };
    match observation.disposition {
        ToolDisposition::Succeeded if observation.intent.agent_run_id == actor => Some(after),
        ToolDisposition::Succeeded | ToolDisposition::Uncertain => Some(None),
        _ => None,
    }
}

fn output_digest(output: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(output).ok()?;
    let digest = value.get("sha256")?.as_str()?;
    crate::validate::valid_digest(digest).then(|| digest.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{RunStatus, ToolView};
    use crate::tools::{
        CacheUpdate, CommandChangeset, EffectIntent, FileChange, FileVersion, ToolLimits,
    };
    use crate::validate::hex_digest;

    const PATH: &str = "src/lib.rs";

    fn read() -> ToolCall {
        ToolCall::Read {
            path: PATH.into(),
            offset: None,
            limit: None,
        }
    }

    fn edit(expected_digest: Option<String>) -> ToolCall {
        ToolCall::Edit {
            path: PATH.into(),
            expected_digest,
            old: "old".into(),
            new: "new".into(),
        }
    }

    fn sha256(content: &[u8]) -> String {
        serde_json::json!({"sha256": hex_digest(content)}).to_string()
    }

    fn record(
        run: &mut RunView,
        actor: AgentRunId,
        call: ToolCall,
        disposition: ToolDisposition,
        output: String,
    ) {
        let sequence = run.tools.len() as u64 + 1;
        let intent = EffectIntent {
            host: None,
            rewrite: Vec::new(),
            host_command: None,
            target: crate::session::worker::EffectTarget::Workspace,
            id: uuid::Uuid::now_v7(),
            run_id: run.id,
            agent_run_id: actor,
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call,
            limits: ToolLimits::default(),
            use_count: 1,
        };
        run.tools.push(ToolView {
            checkpoint: None,
            active_time_reserved: false,
            narrowing_source: None,
            permission_verdict: None,
            started_sequence: Some(sequence),
            hooks: Vec::new(),
            hook_sequence: None,
            intent: intent.clone(),
            observation: Some(ToolObservation {
                spill: None,
                enforcement_cause: None,
                limit_cause: None,
                intent,
                disposition,
                output,
                attestation: None,
                approval_cause: None,
                guard_stage: None,
                cache_update: CacheUpdate::NotApplicable,
            }),
            review_call_index: None,
            finished_sequence: Some(sequence),
        });
    }

    fn run() -> RunView {
        crate::session::tests::unanswered_run(RunStatus::Active)
    }

    #[test]
    fn omitted_digest_follows_the_actors_own_latest_read_or_edit() {
        let (actor, other) = (AgentRunId::new(), AgentRunId::new());
        let mut run = run();
        assert_eq!(current_digest(&run, actor, PATH), None);
        record(
            &mut run,
            actor,
            read(),
            ToolDisposition::Succeeded,
            sha256(b"v1"),
        );
        assert_eq!(current_digest(&run, actor, PATH), Some(hex_digest(b"v1")));
        assert_eq!(
            current_digest(&run, other, PATH),
            None,
            "reads are per actor"
        );
        let edited = edit(Some(hex_digest(b"v1")));
        record(
            &mut run,
            actor,
            edited,
            ToolDisposition::Succeeded,
            sha256(b"v2"),
        );
        assert_eq!(current_digest(&run, actor, PATH), Some(hex_digest(b"v2")));
        record(
            &mut run,
            other,
            read(),
            ToolDisposition::Succeeded,
            sha256(b"v2"),
        );
        record(
            &mut run,
            actor,
            edit(None),
            ToolDisposition::Conflict,
            "stale".into(),
        );
        assert_eq!(
            current_digest(&run, actor, PATH),
            Some(hex_digest(b"v2")),
            "a failed edit publishes nothing"
        );
        let foreign = edit(Some(hex_digest(b"v2")));
        record(
            &mut run,
            other,
            foreign,
            ToolDisposition::Succeeded,
            sha256(b"v3"),
        );
        assert_eq!(
            current_digest(&run, actor, PATH),
            None,
            "another actor's change"
        );
        record(
            &mut run,
            actor,
            read(),
            ToolDisposition::Succeeded,
            sha256(b"v3"),
        );
        let uncertain = edit(Some(hex_digest(b"v3")));
        record(
            &mut run,
            actor,
            uncertain,
            ToolDisposition::Uncertain,
            String::new(),
        );
        assert_eq!(current_digest(&run, actor, PATH), None, "unknown outcome");
    }

    #[test]
    fn deletes_moves_and_workspace_wide_effects_clear_the_digest() {
        let actor = AgentRunId::new();
        let mut run = run();
        let observed = |run: &mut RunView| {
            record(
                run,
                actor,
                read(),
                ToolDisposition::Succeeded,
                sha256(b"v1"),
            );
            assert_eq!(current_digest(run, actor, PATH), Some(hex_digest(b"v1")));
        };
        observed(&mut run);
        let delete = ToolCall::Delete {
            path: PATH.into(),
            expected_digest: Some(hex_digest(b"v1")),
        };
        record(
            &mut run,
            actor,
            delete,
            ToolDisposition::Succeeded,
            String::new(),
        );
        assert_eq!(current_digest(&run, actor, PATH), None);
        observed(&mut run);
        let moved = ToolCall::Move {
            source: PATH.into(),
            destination: "src/other.rs".into(),
            expected_digest: Some(hex_digest(b"v1")),
        };
        record(
            &mut run,
            actor,
            moved,
            ToolDisposition::Succeeded,
            String::new(),
        );
        assert_eq!(current_digest(&run, actor, PATH), None);
        assert_eq!(
            current_digest(&run, actor, "src/other.rs"),
            Some(hex_digest(b"v1")),
            "a move keeps the source bytes"
        );
        observed(&mut run);
        let host = ToolCall::CommandUnguarded {
            program: "cargo".into(),
            args: vec!["fmt".into()],
            cwd: ".".into(),
            justification: "Format".into(),
            prefix_rule: None,
        };
        record(
            &mut run,
            actor,
            host,
            ToolDisposition::Failed,
            String::new(),
        );
        assert_eq!(current_digest(&run, actor, PATH), None, "host commands");
        observed(&mut run);
        let switch = ToolCall::GitSwitch {
            branch: "main".into(),
        };
        record(
            &mut run,
            actor,
            switch.clone(),
            ToolDisposition::Denied,
            String::new(),
        );
        assert_eq!(current_digest(&run, actor, PATH), Some(hex_digest(b"v1")));
        record(
            &mut run,
            actor,
            switch,
            ToolDisposition::Succeeded,
            String::new(),
        );
        assert_eq!(current_digest(&run, actor, PATH), None);
    }

    #[test]
    fn command_integration_supplies_its_known_result_digest() {
        let actor = AgentRunId::new();
        let mut run = run();
        let integration = |path: &str, after: Option<&[u8]>| ToolCall::ApplyChangeset {
            source: ChangesetSource::CommandReceipt {
                source_intent_digest: [3; 32],
                changeset: CommandChangeset {
                    files: vec![FileChange {
                        path: path.into(),
                        before: None,
                        after: after.map(|bytes| FileVersion::from_bytes(bytes, false)),
                    }],
                },
            },
        };
        record(
            &mut run,
            actor,
            read(),
            ToolDisposition::Succeeded,
            sha256(b"v1"),
        );
        let formatted = integration(PATH, Some(b"formatted"));
        record(
            &mut run,
            actor,
            formatted,
            ToolDisposition::Succeeded,
            String::new(),
        );
        assert_eq!(
            current_digest(&run, actor, PATH),
            Some(hex_digest(b"formatted"))
        );
        let unrelated = integration("src/other.rs", None);
        record(
            &mut run,
            actor,
            unrelated,
            ToolDisposition::Succeeded,
            String::new(),
        );
        assert_eq!(
            current_digest(&run, actor, PATH),
            Some(hex_digest(b"formatted"))
        );
        let removed = integration(PATH, None);
        record(
            &mut run,
            actor,
            removed,
            ToolDisposition::Succeeded,
            String::new(),
        );
        assert_eq!(current_digest(&run, actor, PATH), None);
    }

    #[test]
    fn filling_keeps_supplied_digests_and_create_mode_writes() {
        let actor = AgentRunId::new();
        let session_id = crate::session::SessionId::new();
        let mut view = SessionView::replay(
            session_id,
            &[crate::session::EventEnvelope {
                sequence: 1,
                session_id,
                run_id: None,
                agent_run_id: None,
                event: crate::session::Event::SessionStarted {
                    title: "Preconditions".into(),
                    workspace_identity: crate::session::WorkspaceIdentity {
                        device: 1,
                        inode: 2,
                    },
                },
                created_at_ms: 0,
            }],
        )
        .unwrap()
        .unwrap();
        let mut run = run();
        record(
            &mut run,
            actor,
            read(),
            ToolDisposition::Succeeded,
            sha256(b"v1"),
        );
        let run_id = run.id;
        view.runs.push(run);
        let mut omitted = edit(None);
        assert!(digest_known(&view, run_id, actor, &omitted));
        fill_expected_digest(&view, run_id, actor, &mut omitted);
        assert_eq!(omitted, edit(Some(hex_digest(b"v1"))));
        let pinned = edit(Some(hex_digest(b"pinned")));
        let mut filled = pinned.clone();
        fill_expected_digest(&view, run_id, actor, &mut filled);
        assert_eq!(filled, pinned);
        let unknown = ToolCall::Delete {
            path: "src/unread.rs".into(),
            expected_digest: None,
        };
        assert!(!digest_known(&view, run_id, actor, &unknown));
        let mut create = ToolCall::Write {
            path: "src/new.rs".into(),
            expected_digest: None,
            content: "new".into(),
        };
        assert!(digest_known(&view, run_id, actor, &create));
        fill_expected_digest(&view, run_id, actor, &mut create);
        assert!(matches!(
            create,
            ToolCall::Write {
                expected_digest: None,
                ..
            }
        ));
        let mut replace = ToolCall::Write {
            path: PATH.into(),
            expected_digest: None,
            content: "v2".into(),
        };
        fill_expected_digest(&view, run_id, actor, &mut replace);
        assert!(matches!(
            replace,
            ToolCall::Write { expected_digest: Some(digest), .. } if digest == hex_digest(b"v1")
        ));
    }
}
