use super::*;
use crate::session::{AgentRunId, CompactionRecord, PAYLOAD_VERSIONS, RunId};
use crate::store::journal::{append, fork_session, open_connection};
use crate::store::replay::resolve_open_view;
use crate::store::state::FAIL_AT;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

type EventFixture<'a> = (&'a str, i64, &'a str, &'a str);
// One (kind, from, input, expected) row per production step. The glob steps keep payload bytes.
const EVENT_FIXTURES: [EventFixture<'static>; 10] = [
    ("ToolStarted", 13, GLOB_TOOLSTARTED, GLOB_TOOLSTARTED),
    ("ToolFinished", 19, GLOB_TOOLFINISHED, GLOB_TOOLFINISHED),
    ("HookStarted", 3, GLOB_HOOKSTARTED, GLOB_HOOKSTARTED),
    ("HookFinished", 8, GLOB_HOOKFINISHED, GLOB_HOOKFINISHED),
    (
        "GoalPlanContractRecorded",
        6,
        GLOB_GOALPLANCONTRACTRECORDED,
        GLOB_GOALPLANCONTRACTRECORDED,
    ),
    ("AgentSpawned", 11, GLOB_AGENTSPAWNED, GLOB_AGENTSPAWNED),
    ("RunStarted", 17, GLOB_RUNSTARTED, GLOB_RUNSTARTED),
    ("RunStarted", 18, ROOT_RUNSTARTED, NESTED_RUNSTARTED),
    ("RunStarted", 19, NESTED_RUNSTARTED, GLOBAL_RUNSTARTED),
    (
        "ProviderCallRecorded",
        13,
        GLOB_PROVIDERCALLRECORDED,
        GLOB_PROVIDERCALLRECORDED,
    ),
];
const GLOB_PROVIDERCALLRECORDED: &str = r#"{"delegation":null,"phase":"root_plan","disposition":"finished","requested_tools":0,"response_id":"response-1","input_tokens":100,"output_tokens":10,"cache_read_input_tokens":40,"cache_creation_input_tokens":20,"wire_provenance":null,"failure_reason":null,"refusal":null}"#;
const GLOB_TOOLSTARTED: &str = r#"{"intent":{"target":{"kind":"workspace"},"id":"01a15ecf-a92b-736a-91a6-560b8bc09501","run_id":"01a15ecf-a92a-7707-8af6-ce3d36320b67","agent_run_id":"01a15ecf-a92a-7707-8af6-ce3e0c00af83","policy_digest":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"enforcement_digest":[2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2],"workspace_device":1,"workspace_inode":2,"call":{"operation":"read","path":"src/lib.rs","offset":null,"limit":1024},"limits":{"result_bytes":65536,"runtime_ms":60000,"cache_transfer_ms":0,"memory_bytes":536870912,"max_processes":64,"scratch_bytes":67108864,"project_bytes":67108864,"cache_requested_bytes":67108864,"cache_bytes":67108864,"cache_candidate_bytes":67108864,"network":"none","resource_class":"default","resource_profile":"linux_kernel"},"expires_at_ms":1,"use_count":1},"active_ceiling_ms":1000}"#;
const GLOB_TOOLFINISHED: &str = r#"{"intent":{"target":{"kind":"workspace"},"id":"01a15ecf-a92b-736a-91a6-560b8bc09501","run_id":"01a15ecf-a92a-7707-8af6-ce3d36320b67","agent_run_id":"01a15ecf-a92a-7707-8af6-ce3e0c00af83","policy_digest":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"enforcement_digest":[2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2],"workspace_device":1,"workspace_inode":2,"call":{"operation":"read","path":"src/lib.rs","offset":null,"limit":1024},"limits":{"result_bytes":65536,"runtime_ms":60000,"cache_transfer_ms":0,"memory_bytes":536870912,"max_processes":64,"scratch_bytes":67108864,"project_bytes":67108864,"cache_requested_bytes":67108864,"cache_bytes":67108864,"cache_candidate_bytes":67108864,"network":"none","resource_class":"default","resource_profile":"linux_kernel"},"expires_at_ms":1,"use_count":1},"disposition":"succeeded","enforcement_cause":null,"limit_cause":null,"output":"{\"sha256\":\"2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881\"}","attestation":{"in_process":{"policy_digest":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"workspace_device":1,"workspace_inode":2}},"cache_update":"not_applicable","checkpoint":null}"#;
const GLOB_HOOKSTARTED: &str = r#"{"event":"PreToolUse","group":0,"handler":0,"subject_digest":[4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4],"intent":{"target":{"kind":"workspace"},"id":"01a15ecf-a92b-736a-91a6-560ce9fc3701","run_id":"01a15ecf-a92a-7707-8af6-ce3d36320b67","agent_run_id":"01a15ecf-a92a-7707-8af6-ce3e0c00af83","policy_digest":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"enforcement_digest":[2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2],"workspace_device":1,"workspace_inode":2,"call":{"operation":"command","program":"sh","args":["-c","exit 2"],"cwd":""},"limits":{"result_bytes":65536,"runtime_ms":60000,"cache_transfer_ms":0,"memory_bytes":536870912,"max_processes":64,"scratch_bytes":67108864,"project_bytes":67108864,"cache_requested_bytes":67108864,"cache_bytes":67108864,"cache_candidate_bytes":67108864,"network":"none","resource_class":"default","resource_profile":"linux_kernel"},"expires_at_ms":1,"use_count":1},"input_digest":[9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9],"active_ceiling_ms":null}"#;
const GLOB_HOOKFINISHED: &str = r#"{"boundary_active_ms":null,"observation":{"intent":{"target":{"kind":"workspace"},"id":"01a15ecf-a92b-736a-91a6-560ce9fc3701","run_id":"01a15ecf-a92a-7707-8af6-ce3d36320b67","agent_run_id":"01a15ecf-a92a-7707-8af6-ce3e0c00af83","policy_digest":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"enforcement_digest":[2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2],"workspace_device":1,"workspace_inode":2,"call":{"operation":"command","program":"sh","args":["-c","exit 2"],"cwd":""},"limits":{"result_bytes":65536,"runtime_ms":60000,"cache_transfer_ms":0,"memory_bytes":536870912,"max_processes":64,"scratch_bytes":67108864,"project_bytes":67108864,"cache_requested_bytes":67108864,"cache_bytes":67108864,"cache_candidate_bytes":67108864,"network":"none","resource_class":"default","resource_profile":"linux_kernel"},"expires_at_ms":1,"use_count":1},"disposition":"failed","enforcement_cause":null,"limit_cause":null,"output":"{\"changeset\":{\"files\":[]},\"exit_code\":2,\"stderr\":\"blocked\",\"stderr_total\":{\"bytes\":7,\"lines\":1},\"stdout\":\"\",\"stdout_total\":{\"bytes\":0,\"lines\":0}}","attestation":{"guard":{"contract_version":1,"intent_digest":[131,11,117,251,8,22,13,66,136,215,191,13,209,208,209,113,109,80,241,218,188,193,7,40,115,63,160,61,172,44,137,133],"enforcement_digest":[2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2],"limits":{"result_bytes":65536,"runtime_ms":60000,"cache_transfer_ms":0,"memory_bytes":536870912,"max_processes":64,"scratch_bytes":67108864,"project_bytes":67108864,"cache_requested_bytes":67108864,"cache_bytes":67108864,"cache_candidate_bytes":67108864,"network":"none","resource_class":"default","resource_profile":"linux_kernel"}}},"cache_update":"not_applicable"},"result":{"restricted":{"decision":"deny","reason":"blocked"}}}"#;
const GLOB_GOALPLANCONTRACTRECORDED: &str = r#"{"goal_id":"01a15ecf-a92a-7707-8af6-ce3c5a16fa5f","contract":{"mission":"Plan","paths":["src/lib.rs"],"commands":[],"resource_class":"default","command_timeout_ms":null,"skills":[],"mcp_tools":[],"success_criterion":{"kind":"workspace_file_content","path":"src/lib.rs","content":"ok"},"budget":{"max_provider_calls":64,"max_tokens":1000,"max_active_ms":1800000,"max_runs":16}}}"#;
const GLOB_AGENTSPAWNED: &str = r#"{"profile":null,"assignment":null,"definition":null,"role":"primary","ordinal":0,"objective":null,"grant":null}"#;
const GLOB_RUNSTARTED: &str = r#"{"config":{"supervisor":null,"worker_policy":null,"agent_definitions":null,"roster":null,"provider":"openai","model":"gpt-5.4","effort":null,"custom_profile_provenance":null,"saved_api_account_id":null,"chatgpt_provenance":null,"output_token_bound":"provider_enforced","context_window_tokens":null,"policy":{"mode":"single"},"output_token_cap":4096,"provider_concurrency":1,"workspace_device":1,"workspace_inode":1,"instruction_digest":null,"include_digests":[],"history_run_ids":[],"excluded_history_runs":0,"context_usage":null,"compaction_event_sequence":null,"compaction_content_digest":null,"tool_policy":null},"active_deadline_ms":1800000}"#;
// Version 18 pinned only the root file's digest; version 19 names it as the root's source.
const ROOT_RUNSTARTED: &str = r#"{"config":{"supervisor":null,"worker_policy":null,"agent_definitions":null,"roster":null,"provider":"openai","model":"gpt-5.4","effort":null,"custom_profile_provenance":null,"saved_api_account_id":null,"chatgpt_provenance":null,"output_token_bound":"provider_enforced","context_window_tokens":null,"policy":{"mode":"single"},"output_token_cap":4096,"provider_concurrency":1,"workspace_device":1,"workspace_inode":1,"instruction_digest":[7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7],"include_digests":[],"history_run_ids":[],"excluded_history_runs":0,"context_usage":null,"compaction_event_sequence":null,"compaction_content_digest":null,"tool_policy":null},"active_deadline_ms":1800000}"#;
// Version 20 adds the explicit null user-instruction digest.
const GLOBAL_RUNSTARTED: &str = r#"{"config":{"supervisor":null,"worker_policy":null,"agent_definitions":null,"roster":null,"provider":"openai","model":"gpt-5.4","effort":null,"custom_profile_provenance":null,"saved_api_account_id":null,"chatgpt_provenance":null,"output_token_bound":"provider_enforced","context_window_tokens":null,"policy":{"mode":"single"},"output_token_cap":4096,"provider_concurrency":1,"workspace_device":1,"workspace_inode":1,"user_instruction_digest":null,"instruction_sources":[{"directory":"","file":null,"digest":[7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7]}],"launch_directory":"","include_digests":[],"history_run_ids":[],"excluded_history_runs":0,"context_usage":null,"compaction_event_sequence":null,"compaction_content_digest":null,"tool_policy":null},"active_deadline_ms":1800000}"#;
const NESTED_RUNSTARTED: &str = r#"{"config":{"supervisor":null,"worker_policy":null,"agent_definitions":null,"roster":null,"provider":"openai","model":"gpt-5.4","effort":null,"custom_profile_provenance":null,"saved_api_account_id":null,"chatgpt_provenance":null,"output_token_bound":"provider_enforced","context_window_tokens":null,"policy":{"mode":"single"},"output_token_cap":4096,"provider_concurrency":1,"workspace_device":1,"workspace_inode":1,"instruction_sources":[{"directory":"","file":null,"digest":[7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7]}],"launch_directory":"","include_digests":[],"history_run_ids":[],"excluded_history_runs":0,"context_usage":null,"compaction_event_sequence":null,"compaction_content_digest":null,"tool_policy":null},"active_deadline_ms":1800000}"#;

// Synthetic test-only edge: a version-5 SessionStarted stored `name` where version 6 stores `title`.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StartedV5 {
    name: String,
    workspace_device: u64,
    workspace_inode: u64,
}
fn started_v5(value: Value) -> Option<Value> {
    let Value::Object(mut fields) = value else {
        return None;
    };
    let title = fields.remove("name")?;
    fields.insert("title".into(), title);
    (fields.len() == 3).then_some(Value::Object(fields))
}
fn started_v5_canonical(payload: &str) -> Option<String> {
    serde_json::to_string(&serde_json::from_str::<StartedV5>(payload).ok()?).ok()
}
const STARTED_STEP: [EventStep; 1] = [EventStep {
    kind: "SessionStarted",
    from: 5,
    transform: started_v5,
    canonical: started_v5_canonical,
}];
const REFUSING_STEP: [EventStep; 1] = [EventStep {
    kind: "SessionStarted",
    from: 5,
    transform: |_| None,
    canonical: started_v5_canonical,
}];
const STARTED_FIXTURE: EventFixture<'static> = (
    "SessionStarted",
    5,
    r#"{"name":"Session","workspace_device":1,"workspace_inode":2}"#,
    r#"{"title":"Session","workspace_device":1,"workspace_inode":2}"#,
);

fn started_floor() -> Vec<(&'static str, i64)> {
    EVENT_FLOOR
        .iter()
        .map(|&(kind, floor)| (kind, if kind == "SessionStarted" { 5 } else { floor }))
        .collect()
}

fn synthetic<'a>(floor: &'a [(&'a str, i64)], steps: &'a [EventStep]) -> Registry<'a> {
    Registry {
        event_floor: floor,
        event_steps: steps,
        ..REGISTRY
    }
}

fn fail_after(steps: Option<usize>) {
    FAIL_AT.with(|fail| fail.set(steps));
}

fn failed_compaction(run: RunId, covered_sequence: u64, digest: [u8; 32]) -> CompactionRecord {
    CompactionRecord {
        covered_run_id: run,
        covered_sequence,
        source_digest: digest,
        provider: "scripted".into(),
        model: "test".into(),
        prompt_version: 1,
        schema_version: 1,
        compiler_version: 3,
        source_bytes: 1,
        source_tokens_estimate: 1,
        input_tokens: None,
        output_tokens: None,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        response_id: None,
        wire_provenance: None,
        chatgpt_provenance: None,
        output_token_bound: crate::provider::OutputTokenBound::ProviderEnforced,
        status: crate::session::CompactionStatus::Failed {
            reason: crate::session::CompactionFailure::ProviderUnavailable,
        },
    }
}

struct History {
    root: StateRoot,
    sessions: [SessionId; 3],
}

fn forked_history(path: &Path) -> History {
    history(path, false, 0)
}

// A source with a finished Run (optionally Goal-compacted), a rewind and a compaction over it,
// `renames` idle renames, a child fork and a grandchild fork.
fn history(path: &Path, goal_compacted: bool, renames: usize) -> History {
    let root = StateRoot::admit(path).unwrap();
    let fixture = crate::session::worker_compacted_fixture(goal_compacted);
    let source = fixture.session;
    let mut connection = open_connection(&root, false).unwrap();
    let mut hot = None;
    for event in &fixture.events {
        let mut event = event.clone();
        // The fixture pins its digest over zero timestamps; the journal stamps real ones.
        if let Event::GoalContextCompacted { record, .. } = &mut event {
            let (events, _) = resolve_open_view(&connection, source, &mut 0).unwrap();
            record.source_digest =
                crate::session::prefix_digest(&events, record.covered_sequence).unwrap();
        }
        append(&mut connection, &mut hot, source, event).unwrap();
    }
    let view = |connection: &Connection| resolve_view(connection, source, 0, &mut 0).unwrap();
    let current = view(&connection).1.unwrap();
    let started =
        crate::session::rewind_started_fixture(&current, current.rewinds.checkpoints[0].clone());
    append(&mut connection, &mut hot, source, started).unwrap();
    let finished = crate::session::rewind_finished_fixture(
        &view(&connection).1.unwrap(),
        crate::session::rewind::RewindOutcome::Restored,
    );
    append(&mut connection, &mut hot, source, finished).unwrap();
    let (events, current) = view(&connection);
    let covered = current.unwrap().last_sequence;
    let record = failed_compaction(
        fixture.run,
        covered,
        crate::session::prefix_digest(&events, covered).unwrap(),
    );
    let compacted = Event::ContextCompacted {
        record,
        active_ms: None,
    };
    append(&mut connection, &mut hot, source, compacted).unwrap();
    let rename = Event::SessionRenamed { title: "x".into() };
    let transaction = connection.transaction().unwrap();
    for _ in 0..renames {
        transaction
            .execute(
                "INSERT INTO events (sequence, session_id, kind, event_version, payload, created_at_ms)
                 VALUES ((SELECT MAX(sequence) + 1 FROM events), ?1, ?2, ?3, ?4, 0)",
                rusqlite::params![
                    source.to_string(),
                    rename.kind(),
                    rename.version(),
                    rename.payload().unwrap()
                ],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    let child = fork_session(&mut connection, source, "Child".into()).unwrap();
    let grandchild = fork_session(&mut connection, child, "Grandchild".into()).unwrap();
    History {
        root,
        sessions: [source, child, grandchild],
    }
}

fn rows(connection: &Connection) -> Vec<RawRow> {
    let mut statement = connection.prepare(ROWS_AFTER_SQL).unwrap();
    let mut rows: Vec<RawRow> = Vec::new();
    loop {
        let after = rows.last().map_or(0, |row| row.sequence);
        let chunk = statement
            .query_map([after], |row| Ok(raw_row(row).unwrap()))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        if chunk.is_empty() {
            return rows;
        }
        rows.extend(chunk);
    }
}

fn stored_prefix(row: &RawRow) -> PrefixRow<'_> {
    PrefixRow {
        sequence: u64::try_from(row.sequence).unwrap(),
        session_id: &row.session_id,
        run_id: row.run_id.as_deref(),
        agent_run_id: row.agent_run_id.as_deref(),
        kind: &row.kind,
        payload: &row.payload,
        created_at_ms: row.created_at_ms,
    }
}

fn decode_row(row: &RawRow) -> Event {
    let run = row.run_id.as_deref().map(|id| id.parse::<RunId>().unwrap());
    Event::decode(&row.kind, row.version, &row.payload, run, None).unwrap()
}

// Rewrites SessionStarted to the synthetic version 5 and re-pins every prefix reference over the
// stored bytes. With canonical text this is what a version-5 build wrote; noncanonical text pins
// digests that version-5 replay rejected.
fn downgrade_started(connection: &Connection, canonical: bool) {
    repin(connection, |row| {
        if row.kind == "SessionStarted" {
            let current: Value = serde_json::from_str(&row.payload).unwrap();
            let (name, device, inode) = (
                &current["title"],
                &current["workspace_device"],
                &current["workspace_inode"],
            );
            row.payload = if canonical {
                format!(
                    r#"{{"name":{name},"workspace_device":{device},"workspace_inode":{inode}}}"#
                )
            } else {
                format!(
                    r#"{{"workspace_inode":{inode},"name":{name},"workspace_device":{device}}}"#
                )
            };
            row.version = 5;
        }
    });
}

// Rewrites each RunStarted row to its version-19 bytes and re-pins every prefix reference.
fn downgrade_run_started(connection: &Connection) {
    repin(connection, |row| {
        if row.kind == "RunStarted" {
            row.payload = row
                .payload
                .replacen(r#""user_instruction_digest":null,"#, "", 1);
            row.version = 19;
        }
    });
}

fn repin(connection: &Connection, rewrite: impl Fn(&mut RawRow)) {
    let mut lanes = HashMap::<String, PrefixHasher>::new();
    let mut points = HashMap::<(String, u64), [u8; 32]>::new();
    for mut row in rows(connection) {
        rewrite(&mut row);
        let lane = lanes.entry(row.session_id.clone()).or_default();
        if matches!(
            row.kind.as_str(),
            "SessionForked"
                | "ContextCompacted"
                | "GoalContextCompacted"
                | "WorkspaceRewindStarted"
        ) {
            let mut event = decode_row(&row);
            match &mut event {
                Event::SessionForked {
                    source_session_id,
                    source_sequence,
                    prefix_digest,
                    ..
                } => *prefix_digest = points[&(source_session_id.to_string(), *source_sequence)],
                Event::ContextCompacted { record, .. }
                | Event::GoalContextCompacted { record, .. } => {
                    record.source_digest =
                        points[&(row.session_id.clone(), record.covered_sequence)];
                }
                Event::WorkspaceRewindStarted { record } => record.prefix_digest = lane.digest(),
                _ => unreachable!(),
            }
            row.payload = event.payload().unwrap();
        }
        connection
            .execute(
                "UPDATE events SET event_version=?1, payload=?2 WHERE sequence=?3",
                rusqlite::params![row.version, row.payload, row.sequence],
            )
            .unwrap();
        lane.update_row(&stored_prefix(&row)).unwrap();
        points.insert(
            (row.session_id.clone(), u64::try_from(row.sequence).unwrap()),
            lane.digest(),
        );
    }
}

fn first_of(connection: &Connection, kind: &str) -> RawRow {
    rows(connection)
        .into_iter()
        .find(|row| row.kind == kind)
        .unwrap()
}

fn forge(connection: &Connection, kind: &str) {
    let row = first_of(connection, kind);
    let mut event = decode_row(&row);
    match &mut event {
        Event::SessionForked { prefix_digest, .. } => prefix_digest[0] ^= 1,
        Event::ContextCompacted { record, .. } | Event::GoalContextCompacted { record, .. } => {
            record.source_digest[0] ^= 1;
        }
        Event::WorkspaceRewindStarted { record } => record.prefix_digest[0] ^= 1,
        _ => unreachable!(),
    }
    connection
        .execute(
            "UPDATE events SET payload=?1 WHERE sequence=?2",
            rusqlite::params![event.payload().unwrap(), row.sequence],
        )
        .unwrap();
}

fn stored_rows(connection: &Connection) -> Vec<(i64, String, i64, String)> {
    rows(connection)
        .into_iter()
        .map(|row| (row.sequence, row.kind, row.version, row.payload))
        .collect()
}

fn fork_digests(connection: &Connection) -> Vec<String> {
    rows(connection)
        .into_iter()
        .filter(|row| row.kind == "SessionForked")
        .map(|row| {
            serde_json::from_str::<Value>(&row.payload).unwrap()["prefix_digest"].to_string()
        })
        .collect()
}

fn entries(path: &Path) -> Vec<String> {
    let mut names = match std::fs::read_dir(path) {
        Ok(entries) => entries
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

fn user_version(connection: &Connection) -> i64 {
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
}

fn facts(path: &Path) -> Vec<(String, i64, i64, i64, String)> {
    let connection = Connection::open(path.join(DATABASE_FILE)).unwrap();
    connection
        .prepare("SELECT id, from_schema, to_schema, rewritten_events, backup FROM migrations")
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn replays(root: &StateRoot, sessions: &[SessionId]) {
    let connection = open_connection(root, false).unwrap();
    for session in sessions {
        let (_, view) = resolve_view(&connection, *session, 0, &mut 0).unwrap();
        assert!(view.is_some());
    }
}

#[test]
fn a_migratable_history_upgrades_in_a_validated_copy_with_recomputed_prefix_digests() {
    // The Goal compaction adds one more re-pinned digest.
    for (goal_compacted, rewritten) in [(false, 5), (true, 6)] {
        upgrades_with_recomputed_digests(goal_compacted, rewritten);
    }
}

fn upgrades_with_recomputed_digests(goal_compacted: bool, rewritten: i64) {
    let temp = tempfile::tempdir().unwrap();
    let built = temp.path().join("built");
    let path = temp.path().join("state");
    let History { sessions, .. } = history(&built, goal_compacted, 0);
    let writer = Connection::open(built.join(DATABASE_FILE)).unwrap();
    writer
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    downgrade_started(&writer, true);
    let original = stored_rows(&writer);
    let old_digests = fork_digests(&writer);
    let root = StateRoot::admit(&path).unwrap();
    // A crash leaves the downgrade only in the uncheckpointed WAL.
    for name in [DATABASE_FILE, "events.sqlite3-wal"] {
        std::fs::copy(built.join(name), path.join(name)).unwrap();
        std::fs::set_permissions(path.join(name), std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    drop(writer);
    let floor = started_floor();

    assert!(
        root.upgrade_or_archive_history(&synthetic(&floor, &STARTED_STEP))
            .unwrap()
            .is_none(),
        "goal compacted: {goal_compacted}"
    );

    assert!(!path.join("archive").exists());
    for sidecar in &HISTORY_FILES[..3] {
        assert!(!path.join(sidecar).exists(), "{sidecar}");
    }
    let migrations = path.join(DIR);
    let names = entries(&migrations);
    assert_eq!(names.len(), 1, "only the backup remains: {names:?}");
    let backup = migrations.join(&names[0]);
    for (entry, mode) in [(&migrations, 0o700), (&backup, 0o600)] {
        assert_eq!(
            std::fs::metadata(entry).unwrap().permissions().mode() & 0o777,
            mode
        );
    }
    let backup = Connection::open_with_flags(&backup, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(
        stored_rows(&backup),
        original,
        "the backup holds every committed row"
    );
    drop(backup);
    let [fact] = facts(&path).try_into().unwrap();
    assert!(
        names[0].ends_with(&format!("-{}.sqlite3", fact.0)),
        "{names:?}"
    );
    assert_eq!(
        (fact.1, fact.2, fact.3, fact.4),
        (SCHEMA_VERSION, SCHEMA_VERSION, rewritten, names[0].clone()),
        "SessionStarted, every compaction, the rewind and two forks are rewritten"
    );
    replays(&root, &sessions);
    let upgraded = Connection::open(path.join(DATABASE_FILE)).unwrap();
    let new_digests = fork_digests(&upgraded);
    assert_eq!(new_digests.len(), 2);
    assert!(
        new_digests
            .iter()
            .zip(&old_digests)
            .all(|(new, old)| new != old),
        "every fork digest covers the rewritten prefix"
    );
    let started = first_of(&upgraded, "SessionStarted");
    assert_eq!(started.version, 6);
    assert!(
        started.payload.starts_with(r#"{"title":"#),
        "{}",
        started.payload
    );
}

#[test]
fn run_started_v19_migrates_without_reading_user_instructions() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let History { root, sessions } = history(&path, true, 0);
    std::fs::write(path.join("AGENTS.md"), "canary").unwrap();
    std::fs::set_permissions(
        path.join("AGENTS.md"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let connection = Connection::open(path.join(DATABASE_FILE)).unwrap();
    let current = stored_rows(&connection);
    let digests = fork_digests(&connection);
    downgrade_run_started(&connection);
    let downgraded = stored_rows(&connection);
    assert_ne!(downgraded, current);
    assert_ne!(fork_digests(&connection), digests);
    drop(connection);

    assert!(
        root.upgrade_or_archive_history(&REGISTRY)
            .unwrap()
            .is_none()
    );

    assert!(!path.join("archive").exists());
    replays(&root, &sessions);
    let upgraded = Connection::open(path.join(DATABASE_FILE)).unwrap();
    assert_eq!(stored_rows(&upgraded), current);
    assert_eq!(fork_digests(&upgraded), digests);
    let started = first_of(&upgraded, "RunStarted");
    assert_eq!(started.version, 20);
    assert!(
        started
            .payload
            .contains(r#""user_instruction_digest":null"#)
    );
    assert!(!started.payload.contains("canary"));
}

#[test]
fn every_interrupted_migration_step_ends_original_or_upgraded_and_recovers() {
    // One fixed clock makes a retry reuse the same timestamp; the migration ID keeps names apart.
    CLOCK.with(|clock| clock.set(Some(SystemTime::UNIX_EPOCH)));
    let mut interrupted = 0;
    let mut cleaning_checked = false;
    loop {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let History { root, sessions } = forked_history(&path);
        Connection::open(path.join(DATABASE_FILE))
            .unwrap()
            .execute_batch("DROP TABLE migrations; PRAGMA user_version=4;")
            .unwrap();
        let database = path.join(DATABASE_FILE);
        let original = std::fs::read(&database).unwrap();
        let original_rows = stored_rows(&Connection::open(&database).unwrap());
        seed_older_backup(&path);

        fail_after(Some(interrupted));
        let first = root.archive_incompatible_history();
        fail_after(None);
        match first {
            Ok(None) => break,
            Err(StoreError::Io(_)) => {}
            other => panic!("step {interrupted}: {other:?}"),
        }
        let current = std::fs::read(&database).unwrap();
        if current != original {
            assert_eq!(facts(&path).len(), 1, "step {interrupted}");
        }
        if let Some((_, marker)) = read_marker(&root).unwrap() {
            // Every changed pinned artifact fails closed before any cleanup.
            let staged = path.join(DIR).join(STAGED);
            let changed = match marker.phase {
                Phase::Preparing => vec![database.clone()],
                // After an upgrade the retained backup stays pinned; a rollback keeps nothing.
                Phase::Cleaning { upgraded, .. } => {
                    if upgraded {
                        cleaning_checked = true;
                        vec![path.join(DIR).join(&marker.backup)]
                    } else {
                        Vec::new()
                    }
                }
                Phase::Ready { .. } => {
                    let mut changed = vec![database.clone(), path.join(DIR).join(&marker.backup)];
                    changed.extend(staged.exists().then_some(staged));
                    changed
                }
            };
            for file in changed {
                let before = entries(&path.join(DIR));
                let saved = std::fs::read(&file).unwrap();
                std::fs::write(&file, b"changed").unwrap();
                assert!(
                    matches!(
                        root.archive_incompatible_history(),
                        Err(StoreError::MigrationConflict)
                    ),
                    "step {interrupted}: {file:?}"
                );
                assert_eq!(entries(&path.join(DIR)), before, "step {interrupted}");
                assert!(!path.join("archive").exists(), "step {interrupted}");
                std::fs::write(&file, saved).unwrap();
            }
            assert_eq!(
                std::fs::read(&database).unwrap(),
                current,
                "step {interrupted}"
            );
            if current != original {
                // A writable open recovers the pending upgrade under its own lease.
                let store = crate::Store::open(root.try_clone().unwrap()).unwrap();
                tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap()
                    .block_on(store.close())
                    .unwrap();
                assert!(!pending(&root).unwrap(), "step {interrupted}");
            }
        }

        // Recovery itself is interrupted at every step and restarts until it completes.
        let mut recovery = 0;
        loop {
            fail_after(Some(recovery));
            let resumed = root.archive_incompatible_history();
            fail_after(None);
            match resumed {
                Ok(None) => break,
                Err(StoreError::Io(_)) => assert!(
                    std::fs::read(&database).unwrap() == original || facts(&path).len() == 1,
                    "step {interrupted}.{recovery}"
                ),
                other => panic!("step {interrupted}.{recovery}: {other:?}"),
            }
            recovery += 1;
            assert!(
                recovery < 64,
                "step {interrupted}: recovery never completes"
            );
        }
        let [fact] = facts(&path).try_into().unwrap();
        assert_eq!((fact.1, fact.2), (4, SCHEMA_VERSION), "step {interrupted}");
        replays(&root, &sessions);
        let [backup] = entries(&path.join(DIR)).try_into().unwrap();
        assert_eq!(backup, fact.4, "step {interrupted}: only the latest backup");
        assert!(
            backup.starts_with("history-19700101T000000.000Z-"),
            "{backup}"
        );
        let backup = Connection::open(path.join(DIR).join(backup)).unwrap();
        assert_eq!(user_version(&backup), 4, "step {interrupted}");
        assert_eq!(stored_rows(&backup), original_rows, "step {interrupted}");
        interrupted += 1;
    }
    CLOCK.with(|clock| clock.set(None));
    assert!(interrupted >= 16, "every durability step was interrupted");
    assert!(
        cleaning_checked,
        "a backup changed after Cleaning was published fails closed"
    );
}

#[test]
fn a_later_migration_keeps_only_its_own_backup() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let History { root, sessions } = forked_history(&path);
    let database = Connection::open(path.join(DATABASE_FILE)).unwrap();
    database
        .execute_batch("DROP TABLE migrations; PRAGMA user_version=4;")
        .unwrap();
    drop(database);
    assert!(root.archive_incompatible_history().unwrap().is_none());
    let [first] = entries(&path.join(DIR)).try_into().unwrap();
    downgrade_started(&Connection::open(path.join(DATABASE_FILE)).unwrap(), true);
    let floor = started_floor();

    assert!(
        root.upgrade_or_archive_history(&synthetic(&floor, &STARTED_STEP))
            .unwrap()
            .is_none()
    );

    let [backup] = entries(&path.join(DIR)).try_into().unwrap();
    assert_ne!(backup, first, "the older backup is replaced");
    replays(&root, &sessions);
}

fn seed_older_backup(path: &Path) {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(path.join(DIR))
        .unwrap();
    let older = path.join(DIR).join(format!(
        "history-19691231T000000.000Z-{}.sqlite3",
        uuid::Uuid::now_v7()
    ));
    std::fs::write(&older, b"older backup").unwrap();
    std::fs::set_permissions(&older, std::fs::Permissions::from_mode(0o600)).unwrap();
}

type Gate = (
    std::path::PathBuf,
    std::sync::mpsc::SyncSender<()>,
    std::sync::mpsc::Receiver<()>,
);
static LEASE_GATE: std::sync::Mutex<Option<Gate>> = std::sync::Mutex::new(None);

// Parks a gated Store open before it takes its history lease.
pub(super) fn before_lease(root: &StateRoot) {
    let gate = {
        let mut gate = LEASE_GATE.lock().unwrap();
        if gate.as_ref().is_some_and(|gate| gate.0 == root.path()) {
            gate.take()
        } else {
            None
        }
    };
    if let Some((_, entered, release)) = gate {
        entered.send(()).unwrap();
        release.recv().unwrap();
    }
}

#[test]
fn a_writable_open_racing_an_interrupted_migration_recovers_before_admission() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let History { root, sessions } = forked_history(&path);
    let database = path.join(DATABASE_FILE);
    Connection::open(&database)
        .unwrap()
        .execute_batch("DROP TABLE migrations; PRAGMA user_version=4;")
        .unwrap();
    let original = std::fs::read(&database).unwrap();
    let (entered_sender, entered) = std::sync::mpsc::sync_channel(0);
    let (release, release_receiver) = std::sync::mpsc::sync_channel(0);
    *LEASE_GATE.lock().unwrap() =
        Some((root.path().to_path_buf(), entered_sender, release_receiver));
    let opener = {
        let root = root.try_clone().unwrap();
        std::thread::spawn(move || crate::Store::open(root))
    };
    entered.recv().unwrap();

    // While the opener waits for its lease, another startup swaps in the upgrade and crashes.
    fail_after(Some(12));
    assert!(matches!(
        root.archive_incompatible_history(),
        Err(StoreError::Io(_))
    ));
    fail_after(None);
    assert!(std::fs::read(&database).unwrap() != original && pending(&root).unwrap());
    release.send(()).unwrap();
    let store = opener.join().unwrap().unwrap();

    assert!(
        !pending(&root).unwrap(),
        "recovery precedes writable admission"
    );
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(store.close())
        .unwrap();
    let [_fact] = facts(&path).try_into().unwrap();
    replays(&root, &sessions);
}

// A trigger that restores the honest fork row once row 1 is rewritten, after the first batch was read.
fn trigger_repairs_a_later_forgery(connection: &Connection) {
    downgrade_started(connection, true);
    let honest = first_of(connection, "SessionForked");
    assert!(honest.sequence > 256, "the forged row is in a later batch");
    forge(connection, "SessionForked");
    connection
        .execute_batch(&format!(
            "CREATE TRIGGER repair AFTER UPDATE ON events WHEN new.sequence = 1 BEGIN
             UPDATE events SET payload = '{}' WHERE sequence = {}; END;",
            honest.payload.replace('\'', "''"),
            honest.sequence
        ))
        .unwrap();
}

#[test]
fn unsupported_refused_or_forged_history_is_archived_intact() {
    let floor = started_floor();
    let (refusing, upgrading) = (
        synthetic(&floor, &REFUSING_STEP),
        synthetic(&floor, &STARTED_STEP),
    );
    type Case<'a> = (&'a str, &'a Registry<'a>, bool, usize, fn(&Connection));
    let cases: [Case<'_>; 8] = [
        (
            "schema below the floor",
            &REGISTRY,
            false,
            0,
            |connection| {
                connection
                    .execute_batch("DROP TABLE migrations; PRAGMA user_version=3;")
                    .unwrap();
            },
        ),
        ("refused step", &refusing, false, 0, |connection| {
            downgrade_started(connection, true);
        }),
        ("noncanonical pins", &upgrading, false, 0, |connection| {
            downgrade_started(connection, false);
        }),
        ("forged fork", &upgrading, false, 0, |connection| {
            downgrade_started(connection, true);
            forge(connection, "SessionForked");
        }),
        ("forged rewind", &upgrading, false, 0, |connection| {
            downgrade_started(connection, true);
            forge(connection, "WorkspaceRewindStarted");
        }),
        ("forged compaction", &upgrading, false, 0, |connection| {
            downgrade_started(connection, true);
            forge(connection, "ContextCompacted");
        }),
        (
            "forged Goal compaction",
            &upgrading,
            true,
            0,
            |connection| {
                downgrade_started(connection, true);
                forge(connection, "GoalContextCompacted");
            },
        ),
        (
            "trigger beyond the first batch",
            &upgrading,
            false,
            300,
            trigger_repairs_a_later_forgery,
        ),
    ];
    for (name, registry, goal_compacted, renames, prepare) in cases {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let History { root, .. } = history(&path, goal_compacted, renames);
        prepare(&Connection::open(path.join(DATABASE_FILE)).unwrap());
        let original = std::fs::read(path.join(DATABASE_FILE)).unwrap();

        let archived = root.upgrade_or_archive_history(registry).unwrap().unwrap();

        assert_eq!(
            std::fs::read(archived.path().join(DATABASE_FILE)).unwrap(),
            original,
            "{name}"
        );
        assert!(entries(&path.join(DIR)).is_empty(), "{name}");
        archived.acknowledge().unwrap();
        let fresh = open_connection(&root, false).unwrap();
        assert_eq!(stored_rows(&fresh), Vec::new(), "{name}");
    }
}

#[test]
fn replay_validation_bounds_aggregate_work_but_not_each_lineage() {
    let floor = started_floor();
    let registry = synthetic(&floor, &STARTED_STEP);
    // Forks reload their ancestors: about 10,400 events in aggregate, under 10,000 per lineage.
    for (limit, migrated) in [(MAX_VALIDATION_EVENTS, true), (5_000, false)] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let History { root, sessions } = history(&path, false, 3_400);
        downgrade_started(&Connection::open(path.join(DATABASE_FILE)).unwrap(), true);
        let original = std::fs::read(path.join(DATABASE_FILE)).unwrap();
        VALIDATION_LIMIT.with(|bound| bound.set(limit));
        let result = root.upgrade_or_archive_history(&registry).unwrap();
        VALIDATION_LIMIT.with(|bound| bound.set(MAX_VALIDATION_EVENTS));
        if migrated {
            assert!(result.is_none());
            replays(&root, &sessions);
        } else {
            let archived = result.unwrap();
            assert_eq!(
                std::fs::read(archived.path().join(DATABASE_FILE)).unwrap(),
                original
            );
        }
    }
}

#[test]
fn unsupported_history_in_an_uncheckpointed_wal_is_archived_byte_for_byte() {
    let temp = tempfile::tempdir().unwrap();
    let built = temp.path().join("built");
    let path = temp.path().join("state");
    forked_history(&built);
    let writer = Connection::open(built.join(DATABASE_FILE)).unwrap();
    writer
        .execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
             UPDATE events SET event_version = 4 WHERE kind = 'SessionStarted';",
        )
        .unwrap();
    let root = StateRoot::admit(&path).unwrap();
    let mut copied = Vec::new();
    for name in [DATABASE_FILE, "events.sqlite3-wal"] {
        std::fs::copy(built.join(name), path.join(name)).unwrap();
        std::fs::set_permissions(path.join(name), std::fs::Permissions::from_mode(0o600)).unwrap();
        copied.push((name, std::fs::read(path.join(name)).unwrap()));
    }
    drop(writer);

    let archived = root.archive_incompatible_history().unwrap().unwrap();

    for (name, bytes) in copied {
        assert_eq!(
            std::fs::read(archived.path().join(name)).unwrap(),
            bytes,
            "{name} is archived unchanged"
        );
    }
    assert!(!path.join(DIR).exists());
}

// The guard: every version above its declared floor needs a registered step and fixture per edge.
fn uncovered(
    registry: &Registry<'_>,
    events: &[(&str, i64)],
    schema: i64,
    event_fixtures: &[EventFixture<'_>],
) -> Vec<String> {
    let mut problems = Vec::new();
    for &(kind, current) in events {
        let Some(floor) = registry.event_floor(kind) else {
            problems.push(format!("{kind} declares no migration floor"));
            continue;
        };
        let mut chained = None;
        for from in floor..current {
            let step = registry.event_step(kind, from);
            let fixture = event_fixtures
                .iter()
                .find(|fixture| fixture.0 == kind && fixture.1 == from);
            let (Some(step), Some(&(_, _, input, expected))) = (step, fixture) else {
                problems.push(format!(
                    "{kind} {from} -> {} has no step and fixture",
                    from + 1
                ));
                continue;
            };
            if (step.canonical)(input).as_deref() != Some(input) {
                problems.push(format!("{kind} {from} fixture is not canonical"));
            }
            let input: Value = serde_json::from_str(input).unwrap();
            let expected: Value = serde_json::from_str(expected).unwrap();
            if (step.transform)(input.clone()).as_ref() != Some(&expected) {
                problems.push(format!("{kind} {from} -> {} fixture mismatch", from + 1));
            }
            chained = Some(chained.unwrap_or(Some(input)).and_then(step.transform));
        }
        if let Some(value) = chained {
            let decodes = value.is_some_and(|value| {
                // A payload that pins its own actor (a Tool intent) decodes only under that scope.
                let pinned = intent_scope(&value);
                let scopes = [
                    (None, None),
                    (Some(RunId::new()), None),
                    (Some(RunId::new()), Some(AgentRunId::new())),
                    (pinned.map(|(run, _)| run), pinned.map(|(_, agent)| agent)),
                ];
                scopes.into_iter().any(|(run, agent)| {
                    Event::decode(kind, current, &value.to_string(), run, agent).is_ok()
                })
            });
            if !decodes {
                problems.push(format!("{kind} {floor} -> {current} does not decode"));
            }
        }
    }
    for from in registry.schema_floor..schema {
        let (Some(step), Some(target)) = (
            registry.schema_step(from),
            registry.expected_schema(from + 1),
        ) else {
            problems.push(format!(
                "schema {from} -> {} has no step and fixture",
                from + 1
            ));
            continue;
        };
        if schema_of(&format!("{}{}", step.source, step.sql)) != schema_of(target) {
            problems.push(format!("schema {from} -> {} fixture mismatch", from + 1));
        }
    }
    problems
}

fn intent_scope(value: &Value) -> Option<(RunId, AgentRunId)> {
    let intent = ["intent", "observation"]
        .into_iter()
        .find_map(|key| value.get(key))
        .map(|field| field.get("intent").unwrap_or(field))?;
    Some((
        serde_json::from_value(intent.get("run_id")?.clone()).ok()?,
        serde_json::from_value(intent.get("agent_run_id")?.clone()).ok()?,
    ))
}

fn schema_of(sql: &str) -> Vec<[String; 4]> {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch(sql).unwrap();
    schema_objects(&connection).unwrap()
}

#[test]
fn every_stored_version_above_its_floor_has_a_step_and_fixture() {
    assert_eq!(
        uncovered(
            &REGISTRY,
            &PAYLOAD_VERSIONS,
            SCHEMA_VERSION,
            &EVENT_FIXTURES
        ),
        Vec::<String>::new()
    );
    // History older than the glob Tool cannot name it, so such a row is forged and refuses.
    let forged = GLOB_TOOLSTARTED.replace(
        r#"{"operation":"read","path":"src/lib.rs","offset":null,"limit":1024}"#,
        r#"{"operation":"glob","pattern":"*.rs","path":".","limit":100}"#,
    );
    assert_ne!(forged, GLOB_TOOLSTARTED);
    let step = REGISTRY.event_step("ToolStarted", 13).unwrap();
    assert!((step.canonical)(&forged).is_none());
    assert!((step.transform)(serde_json::from_str(&forged).unwrap()).is_none());
    // A version-18 row that already pins nested sources is forged; one without instructions pins none.
    let step = REGISTRY.event_step("RunStarted", 18).unwrap();
    let forged = ROOT_RUNSTARTED.replace(
        r#""include_digests""#,
        r#""launch_directory":"","include_digests""#,
    );
    assert!((step.canonical)(&forged).is_none());
    assert!((step.transform)(serde_json::from_str(&forged).unwrap()).is_none());
    let step = REGISTRY.event_step("RunStarted", 19).unwrap();
    let forged = NESTED_RUNSTARTED.replace(
        r#""instruction_sources""#,
        r#""user_instruction_digest":null,"instruction_sources""#,
    );
    assert!((step.canonical)(&forged).is_none());
    assert!((step.transform)(serde_json::from_str(&forged).unwrap()).is_none());
    let step = REGISTRY.event_step("RunStarted", 18).unwrap();
    let upgraded = (step.transform)(serde_json::from_str(GLOB_RUNSTARTED).unwrap()).unwrap();
    assert_eq!(
        upgraded["config"]["instruction_sources"],
        serde_json::json!([])
    );
    let floor = started_floor();
    assert_eq!(
        uncovered(
            &synthetic(&floor, &STARTED_STEP),
            &[("SessionStarted", 6)],
            SCHEMA_VERSION,
            &[STARTED_FIXTURE]
        ),
        Vec::<String>::new(),
        "a registered step with its fixture covers its edge"
    );

    let mut bumped = PAYLOAD_VERSIONS.to_vec();
    bumped.iter_mut().for_each(|(kind, version)| {
        if *kind == "SessionRenamed" {
            *version += 1;
        }
    });
    bumped.push(("SyntheticKind", 1));
    assert_eq!(
        uncovered(&REGISTRY, &bumped, SCHEMA_VERSION + 1, &EVENT_FIXTURES),
        [
            "SessionRenamed 6 -> 7 has no step and fixture",
            "SyntheticKind declares no migration floor",
            "schema 5 -> 6 has no step and fixture",
        ]
    );
}
