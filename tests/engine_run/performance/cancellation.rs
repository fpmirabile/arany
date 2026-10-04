use super::super::*;
use std::time::Instant;

const WARMUPS: usize = 10;
const SAMPLES: usize = 100;
const CHILDREN: usize = 3;

#[test]
#[ignore = "run only with cargo test --release --test engine_run team_cancellation_latency -- --ignored --nocapture"]
fn team_cancellation_latency_on_named_host() {
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
        let (started, mut started_rx) = mpsc::unbounded_channel();
        let requests = Arc::new(Mutex::new(Vec::with_capacity((WARMUPS + SAMPLES) * 4)));
        let provider = GatedProvider {
            started,
            requests: Arc::clone(&requests),
            fail_child: None,
            capacity: CHILDREN as u8,
        };
        let mut engine = Engine::open(StateRoot::admit(&state).expect("private state"), provider)
            .expect("benchmark Engine");
        let observer = Store::open_read_only(StateRoot::open_existing(&state).expect("state"))
            .expect("read-only observer");
        let mut samples = Vec::with_capacity(SAMPLES);
        let mut last_view = None;
        for index in 0..WARMUPS + SAMPLES {
            let cancellation = RunCancellation::new();
            let run_cancellation = cancellation.clone();
            let mut run_request = request(&workspace, "Synthetic cancelled team question");
            run_request.policy = CollaborationPolicy::Team {
                max_active_children: CHILDREN as u8,
            };
            let mut run_task = tokio::spawn(async move {
                let outcome = engine.run_with_cancel(run_request, run_cancellation).await;
                (engine, outcome)
            });
            let gates = tokio::time::timeout(Duration::from_secs(5), async {
                let mut gates = Vec::with_capacity(CHILDREN);
                for _ in 0..CHILDREN {
                    gates.push(started_rx.recv().await.expect("active child call"));
                }
                gates
            })
            .await;
            let gates = match gates {
                Ok(gates) => gates,
                Err(_) => {
                    run_task.abort();
                    let _ = run_task.await;
                    panic!("team calls did not start within the sample deadline");
                }
            };
            let started = Instant::now();
            assert!(cancellation.cancel(), "first cancellation request");
            let joined = tokio::time::timeout(Duration::from_secs(5), &mut run_task).await;
            let (returned_engine, outcome) = match joined {
                Ok(result) => result.expect("team Run task"),
                Err(_) => {
                    run_task.abort();
                    let _ = run_task.await;
                    panic!("team cancellation exceeded the sample deadline");
                }
            };
            let elapsed = started.elapsed();
            engine = returned_engine;
            if index >= WARMUPS {
                samples.push(elapsed);
            }
            let outcome = outcome.expect("durable cancelled team Run");
            assert_eq!(outcome.run.status, RunStatus::Cancelled);
            assert_eq!(outcome.run.agents.len(), CHILDREN + 1);
            assert!(
                outcome
                    .run
                    .agents
                    .iter()
                    .all(|agent| agent.status == AgentStatus::Cancelled)
            );
            assert!(outcome.run.assistant_message.is_none());
            let mut objectives = gates
                .iter()
                .map(|(objective, _)| objective.as_str())
                .collect::<Vec<_>>();
            objectives.sort_unstable();
            assert_eq!(objectives, ["A", "B", "C"]);
            for (_, release) in gates {
                assert!(release.send(()).is_err(), "cancelled call still live");
            }
            assert_eq!(
                requests.lock().expect("scripted requests").len(),
                (index + 1) * (CHILDREN + 1)
            );
            let events = observer
                .load_session(outcome.session_id)
                .await
                .expect("committed Events");
            assert!(events.iter().any(|event| matches!(
                event.event,
                Event::RunFinished {
                    disposition: arany::RunDisposition::Cancelled,
                    ..
                }
            )));
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event.event, Event::MessageCommitted { .. }))
            );
            let view = SessionView::replay(outcome.session_id, &events)
                .expect("strict replay")
                .expect("cancelled Session");
            assert_eq!(view.runs.len(), 1);
            assert_eq!(view.runs[0], outcome.run);
            last_view = Some(view);
        }
        assert!(matches!(
            started_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        observer.close().await.expect("close observer");
        engine.close().await.expect("close Engine");
        let last_view = last_view.expect("last cancelled Session");
        let reopened = Store::open_read_only(StateRoot::open_existing(&state).expect("state"))
            .expect("reopened Store");
        let events = reopened
            .load_session(last_view.id)
            .await
            .expect("reopened Events");
        let replayed = SessionView::replay(last_view.id, &events)
            .expect("reopened strict replay")
            .expect("reopened Session");
        assert_eq!(replayed, last_view);
        reopened.close().await.expect("close reopened Store");
        samples.sort_unstable();
        let p50 = samples[SAMPLES / 2 - 1];
        let p95 = samples[SAMPLES * 95 / 100 - 1];
        let max = samples[SAMPLES - 1];
        println!(
            "team3_cancel: n={SAMPLES} p50={}us p95={}us max={}us profile=release",
            p50.as_micros(),
            p95.as_micros(),
            max.as_micros()
        );
        assert!(
            p95 <= Duration::from_millis(100),
            "three-child cancellation p95 exceeds the 100 ms release gate"
        );
    });
}
