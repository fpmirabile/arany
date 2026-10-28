use arany::Provider;
use arany::test_support::{engine::run_to_end, routing::*, scenario::ScriptedProvider};

#[tokio::test]
async fn reopened_streaming_fallback_report_labels_both_attempt_identities() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let mut primary = ScriptedProvider::new(vec![])
        .with_profile("openai", "model-a")
        .with_saved_api_account(uuid::Uuid::now_v7());
    primary.responses.get_mut().unwrap().push_back(Err(
        arany::ProviderError::HttpServerUnavailable(Some(std::time::Duration::MAX)),
    ));
    primary
        .rate_limits
        .get_mut()
        .unwrap()
        .push_back(Some(api_sample(1000)));
    let primary = route(primary);
    let mut replacement = ScriptedProvider::new(vec![finish("done")])
        .with_profile("openai", "model-b")
        .with_saved_api_account(uuid::Uuid::now_v7());
    replacement
        .rate_limits
        .get_mut()
        .unwrap()
        .push_back(Some(api_sample(1100)));
    let replacement = route(replacement);
    let mut bindings = routes(&primary);
    bindings.roles.primary.push(replacement.clone());
    let engine = open(&state);
    let result = arany::test_support::engine::run_routed(
        &engine,
        bindings,
        input(&workspace),
        arany::RunControls::default(),
    )
    .await
    .unwrap();
    engine.close().await.unwrap();
    let (_, view) = reopen(&state, result.session_id).await;
    assert_eq!(
        view.runs[0].provider_fallbacks[0].1.from,
        *primary.identity()
    );
    assert_eq!(
        view.runs[0].provider_fallbacks[0].1.to,
        *replacement.identity()
    );
    assert_eq!(view.usage.api_limits[0].identity, *primary.identity());
    assert_eq!(view.usage.api_limits[1].identity, *replacement.identity());
    let expected = [
        "\nAPI rate limits · Run 1 · route 1 · historical\nProvider: openai\nModel: model-a\nRole: primary\nReported header dialect: OpenAI\nProvider-reported counters; not a plan balance or this Session's share\nRequests\nLimit: 100\nRemaining: 0\nLast reported: 1970-01-01T00:16:40Z · headers\nObservation age unknown\nReset: 1970-01-01T00:17:40Z\n".to_owned(),
        "\nAPI rate limits · Run 1 · route 2 · historical\nProvider: openai\nModel: model-b\nRole: primary\nReported header dialect: OpenAI\nProvider-reported counters; not a plan balance or this Session's share\nRequests\nLimit: 100\nRemaining: 0\nLast reported: 1970-01-01T00:18:20Z · headers\nObservation age unknown\nReset: 1970-01-01T00:19:20Z\n".to_owned(),
    ];
    assert_eq!(super::sections(&view, None), expected);
    assert_eq!(primary.requests.lock().unwrap().len(), 1);
    assert_eq!(replacement.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn reopened_api_report_keeps_exact_rows_omissions_age_and_run_scope() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let mut provider = ScriptedProvider::new(vec![finish("done"), finish("next")])
        .with_profile("openai", "model-a")
        .with_compactions(vec![Ok(arany::CompactionResponse {
            summary: "summary".into(),
            response_id: None,
            usage: arany::Usage::default(),
            wire_provenance: None,
        })]);
    provider
        .rate_limits
        .get_mut()
        .unwrap()
        .extend([Some(api_sample(1000)), None]);
    provider
        .compact_rate_limits
        .get_mut()
        .unwrap()
        .push_back(Some(api_sample(1000)));
    provider.usage_override = Some(arany::Usage::default());
    let provider = route(provider);
    let engine = open(&state);
    let first = run_to_end(&engine, &provider, input(&workspace))
        .await
        .unwrap();
    engine
        .compact_session(&routes(&provider), first.session_id, workspace.clone())
        .await
        .unwrap();
    let mut next = input(&workspace);
    next.session_id = Some(first.session_id);
    next.title = None;
    run_to_end(&engine, &provider, next).await.unwrap();
    engine.close().await.unwrap();
    let (_, view) = reopen(&state, first.session_id).await;
    let expected = "\nAPI rate limits · Run 1 · route 1 · historical\nProvider: openai\nModel: model-a\nRole: primary\nReported header dialect: OpenAI\nProvider-reported counters; not a plan balance or this Session's share\nRequests\nLimit: 100\nRemaining: 0\nLast reported: 1970-01-01T00:16:40Z · headers\nObservation age: 0 s\nReset: 1970-01-01T00:17:40Z\nResets in 1 min\n";
    let compaction = "\nAPI rate limits · Run 1 · route 2 · historical\nProvider: openai\nModel: model-a\nRole: compaction\nReported header dialect: OpenAI\nProvider-reported counters; not a plan balance or this Session's share\nRequests\nLimit: 100\nRemaining: 0\nLast reported: 1970-01-01T00:16:40Z · headers\nObservation age: 0 s\nReset: 1970-01-01T00:17:40Z\nResets in 1 min\n";
    let pages = super::super::usage_pages_at(&view, false, None, Some(1000));
    assert_eq!(pages.len(), 1);
    let counters = "Input: unknown (not reported)\nOutput: unknown (not reported)\nCache read (included in input): unknown (not reported)\nCache write (included in input): unknown (not reported)\n";
    assert_eq!(
        pages[0],
        format!(
            "Usage: Provider-reported tokens\nCommitted through Event {}\nCache reads/writes are included in input\nOutput includes reported reasoning tokens\n\nCurrent/latest Run\nProvider: openai\nModel: model-a\n{counters}API rate limits: unknown (not reported)\n\nSession totals\nProvider: openai\nModel: model-a\n{counters}API rate limits: see route observations below\n{expected}{compaction}",
            view.last_sequence
        )
    );
    assert!(
        !super::super::usage_pages_at(&view, true, None, Some(1000))
            .concat()
            .contains("API rate limits")
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert_eq!(provider.compact_requests.lock().unwrap().len(), 1);
    let mut changed = view.clone();
    let group = &mut changed.usage.api_limits[0];
    group.latest_omitted = Some(Vec::new());
    group.latest_unavailable = Some(arany::LimitFailure {
        reason: arany::LimitUnavailable::Invalid,
        source: arany::LimitSource::Headers,
        observed_at: 1100,
    });
    let pages = super::super::usage_pages_at(&changed, false, None, Some(1060)).concat();
    assert!(pages.contains("Remaining: 0\n"));
    assert!(pages.contains("Stale; reset time passed, current usage unknown"));
    assert!(pages.contains("Latest call did not report limits\n"));
    changed.usage.api_limits[0].latest_omitted = Some(vec![arany::ApiLimitDimension::Requests]);
    let partial = super::super::usage_pages_at(&changed, false, None, Some(1060)).concat();
    assert!(partial.contains("Latest call did not report limits for: Requests\n"));
    assert!(pages.contains("invalid optional metadata"));
    let text = super::super::usage_pages_at(&changed, false, None, Some(999)).concat();
    assert!(text.contains("Observation age unknown"));
    assert!(!text.contains("Resets in"));
}
