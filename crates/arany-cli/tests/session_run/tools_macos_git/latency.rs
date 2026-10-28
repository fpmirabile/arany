use super::*;
use arany::test_support::latency::Sample;

const PREFIX: &str = "Git write latency ";

pub(crate) fn forward(stdout: &[u8]) {
    for line in String::from_utf8_lossy(stdout).lines() {
        if line.starts_with(PREFIX) {
            println!("{line}");
        }
    }
}

pub(super) fn report(samples: &[Sample], events: &[arany::EventEnvelope]) {
    let samples: Vec<_> = samples
        .iter()
        .filter(|sample| {
            events
                .iter()
                .any(|event| event.session_id == sample.session_id)
        })
        .collect();
    assert_eq!(samples.len(), 1, "one audited Git write per latency row");
    let sample = samples[0];
    assert_eq!(sample.requested_tools, 1);
    assert_eq!(sample.disposition, Some(ToolDisposition::Succeeded));
    assert!(sample.tool_receipt_to_commit_us.is_some());
    let intervals = sample.audit(events).unwrap();
    let observation = events
        .iter()
        .find_map(|event| match &event.event {
            arany::Event::ToolFinished { observation, .. }
                if Some(observation.intent.id) == sample.intent_id =>
            {
                Some(observation)
            }
            _ => None,
        })
        .unwrap();
    let operation = match &observation.intent.call {
        ToolCall::GitBranch { name: Some(_), .. } => "git_branch",
        ToolCall::GitCommit { staged: false, .. } => "git_commit_paths",
        ToolCall::GitCommit { staged: true, .. } => "git_commit_staged",
        ToolCall::GitSwitch { .. } => "git_switch",
        _ => panic!("unexpected Git latency operation"),
    };
    let result: serde_json::Value = serde_json::from_str(&observation.output).unwrap();
    assert!(result.get("publication").is_some());
    assert!(matches!(
        observation.attestation,
        Some(arany::Attestation::Guard(_))
    ));
    println!(
        "{PREFIX}{}",
        json!({
            "platform": "macos", "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            "operation": operation, "sample": sample, "journal_interval_ms": intervals,
            "publication": result["publication"], "switched": result.get("switched"),
        })
    );
}
