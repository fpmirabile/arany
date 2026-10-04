use super::*;
use std::time::Instant;

#[path = "performance/cancellation.rs"]
mod cancellation;

const WARMUPS: usize = 10;
const SAMPLES: usize = 100;

#[test]
#[ignore = "run only with cargo test --release --test engine_run team_two_core_latency -- --ignored --nocapture"]
fn team_two_core_latency_on_named_host() {
    let test_binary = std::env::current_exe().expect("test binary path");
    assert!(
        test_binary
            .components()
            .any(|part| part.as_os_str() == "release"),
        "release profile required for latency measurement"
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("benchmark runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("private benchmark root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("benchmark Workspace");
        std::fs::create_dir(workspace.join("notes")).expect("include directory");
        std::fs::write(workspace.join("notes/facts.txt"), "Synthetic fact")
            .expect("synthetic include");
        let state = temp.path().join("state");
        let mut responses = Vec::with_capacity((WARMUPS + SAMPLES) * 4);
        for _ in 0..WARMUPS + SAMPLES {
            responses.extend([
                ProviderOutcome::Delegate(Delegate {
                    children: vec!["A".into(), "B".into()],
                }),
                ProviderOutcome::Finish(Finish {
                    summary: "child".into(),
                    result: "done".into(),
                }),
                ProviderOutcome::Finish(Finish {
                    summary: "child".into(),
                    result: "done".into(),
                }),
                ProviderOutcome::Finish(Finish {
                    summary: "synthesized".into(),
                    result: "team answer".into(),
                }),
            ]);
        }
        let provider = ScriptedProvider::new(responses);
        let requests = Arc::clone(&provider.requests);
        let mut engine = Engine::open(StateRoot::admit(&state).expect("private state"), provider)
            .expect("benchmark Engine");
        let observer = Store::open_read_only(StateRoot::open_existing(&state).expect("state"))
            .expect("read-only observer");
        let mut expected_sequence = None;
        let mut last_view = None;
        let mut samples = Vec::with_capacity(SAMPLES);
        for index in 0..WARMUPS + SAMPLES {
            let mut run_request = request(&workspace, "Synthetic team question");
            run_request.policy = CollaborationPolicy::Team {
                max_active_children: 2,
            };
            let started = Instant::now();
            let outcome = tokio::time::timeout(Duration::from_secs(5), engine.run(run_request))
                .await
                .expect("bounded team Run")
                .expect("successful team Run");
            let elapsed = started.elapsed();
            if index >= WARMUPS {
                samples.push(elapsed);
            }
            assert_eq!(outcome.run.status, RunStatus::Finished);
            assert_eq!(outcome.run.agents.len(), 3);
            assert_eq!(
                outcome.run.assistant_message.as_deref(),
                Some("team answer")
            );
            let events = observer
                .load_session(outcome.session_id)
                .await
                .expect("committed Events");
            let view = SessionView::replay(outcome.session_id, &events)
                .expect("strict replay")
                .expect("team Session");
            assert_eq!(view.runs.len(), 1);
            assert_eq!(view.runs[0], outcome.run);
            let sequence = events
                .iter()
                .map(|event| std::mem::discriminant(&event.event))
                .collect::<Vec<_>>();
            if let Some(expected) = &expected_sequence {
                assert_eq!(sequence, *expected, "semantic Event sequence {index}");
            } else {
                expected_sequence = Some(sequence);
            }
            let calls = requests.lock().expect("scripted requests");
            let calls = &calls[index * 4..(index + 1) * 4];
            assert_eq!(
                calls.iter().map(|call| call.phase).collect::<Vec<_>>(),
                [
                    AgentPhase::RootPlan,
                    AgentPhase::ChildWork,
                    AgentPhase::ChildWork,
                    AgentPhase::RootSynthesis,
                ]
            );
            assert_eq!(
                calls[3]
                    .child_results
                    .iter()
                    .map(|child| child.objective.as_str())
                    .collect::<Vec<_>>(),
                ["A", "B"]
            );
            last_view = Some(view);
        }
        observer.close().await.expect("close observer");
        engine.close().await.expect("close Engine");
        let last_view = last_view.expect("final team Session");
        let reopened = Store::open_read_only(StateRoot::open_existing(&state).expect("state"))
            .expect("reopened Store");
        let events = reopened
            .load_session(last_view.id)
            .await
            .expect("reopened Events");
        let replayed = SessionView::replay(last_view.id, &events)
            .expect("reopened strict replay")
            .expect("reopened team Session");
        assert_eq!(replayed, last_view);
        reopened.close().await.expect("close reopened Store");
        samples.sort_unstable();
        let p50 = samples[SAMPLES / 2 - 1];
        let p95 = samples[SAMPLES * 95 / 100 - 1];
        let max = samples[SAMPLES - 1];
        println!(
            "team2_core: n={SAMPLES} p50={}us p95={}us max={}us profile=release",
            p50.as_micros(),
            p95.as_micros(),
            max.as_micros()
        );
        assert!(
            p95 <= Duration::from_millis(100),
            "two-child Engine Run p95 exceeds the 100 ms core budget"
        );
    });
}
