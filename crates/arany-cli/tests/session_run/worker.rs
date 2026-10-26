use super::*;
use arany::test_support::scenario::ScriptedProvider;
use arany_cli::presentation::{Output, render_session};
use arany_cli::protocol::EventLine;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn worker_id_resume_survives_exit_show_fork_and_compaction_without_reexecution() {
    let observed = Box::pin(arany::test_support::worker_teams::resume_by_id(
        std::path::Path::new(env!("CARGO_BIN_EXE_arany")),
    ))
    .await;
    let state = observed.directory.path().join("state");
    let workspace = observed.directory.path().join("workspace");
    let engine = engine_support::open(
        StateRoot::open_existing(&state).unwrap(),
        arany::EngineConfig::default(),
    )
    .unwrap();
    let fork = engine
        .fork_session(observed.session, workspace.clone(), None)
        .await
        .unwrap();
    let provider = Arc::new(ScriptedProvider::new(Vec::new()).with_compactions(vec![Ok(
        arany::CompactionResponse {
            summary: "Historical workers are bounded data; no execution authority".into(),
            response_id: None,
            usage: arany::Usage::default(),
            wire_provenance: None,
        },
    )]));
    engine
        .compact_session(
            &arany::AdmittedProviders::single(provider.clone()),
            observed.session,
            workspace.clone(),
        )
        .await
        .unwrap();
    assert!(
        provider.requests.lock().unwrap().is_empty(),
        "compaction re-executed a historical worker"
    );
    let input = format!("{:?}", provider.compact_requests.lock().unwrap()[0].items);
    for worker in observed
        .view
        .runs
        .iter()
        .flat_map(arany::RunView::worker_history)
    {
        assert!(
            input.contains(&worker.assignment_id.to_string()),
            "compaction lost source identity"
        );
    }
    engine.close().await.unwrap();
    for session in [observed.session, fork] {
        let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
        let events = store.load_session(session).await.unwrap();
        let view = store.load_view(session).await.unwrap().unwrap();
        store.close().await.unwrap();
        assert_eq!(
            view.runs, observed.view.runs,
            "fork/compaction changed worker history"
        );
        for output in [Output::Text, Output::Jsonl] {
            let actual = Command::new(env!("CARGO_BIN_EXE_arany"))
                .env_clear()
                .current_dir(&workspace)
                .args(["show", "--state-dir"])
                .arg(&state)
                .args([
                    "--output",
                    if output == Output::Text {
                        "text"
                    } else {
                        "jsonl"
                    },
                ])
                .arg(session.to_string())
                .bounded_output_for(Duration::from_secs(30), 4 * 1024 * 1024)
                .unwrap();
            assert!(actual.status.success());
            assert!(actual.stderr.is_empty());
            assert_eq!(
                actual.stdout,
                render_session(&view, &events, output).as_bytes()
            );
            if output == Output::Jsonl {
                let lines: Vec<EventLine> = std::str::from_utf8(&actual.stdout)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert_eq!(
                    lines,
                    events.iter().map(EventLine::from).collect::<Vec<_>>()
                );
            } else {
                let text = std::str::from_utf8(&actual.stdout).unwrap();
                let source = observed.view.runs[0].agents[1].id;
                let resumed = observed.view.runs[1].agents[1].id;
                assert!(text.contains(&source.to_string()));
                assert!(text.contains(&resumed.to_string()));
                for worker in view.runs.iter().flat_map(arany::RunView::worker_history) {
                    assert!(text.contains(&arany_cli::presentation::escape_terminal(
                        &worker.description()
                    )));
                }
            }
        }
    }
    assert_eq!(
        std::fs::read(workspace.join("notes/0")).unwrap(),
        b"edited 0"
    );
}
