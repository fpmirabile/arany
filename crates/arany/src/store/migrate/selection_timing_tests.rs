use super::*;

#[test]
fn selection_timing_migration_preserves_output_and_marks_old_timing_unknown() {
    for (kind, from, fixture) in [
        ("ToolFinished", 30, SELECTION_TOOLFINISHED),
        ("HookFinished", 19, SELECTION_HOOKFINISHED),
    ] {
        let step = REGISTRY.event_step(kind, from).unwrap();
        let mut source: Value = serde_json::from_str(fixture).unwrap();
        let observation = if kind == "HookFinished" {
            &mut source["observation"]
        } else {
            &mut source
        };
        let mut result: Value =
            serde_json::from_str(observation["output"].as_str().unwrap()).unwrap();
        result["selection"] = serde_json::json!({
            "ignored_entries":2,"pruned_directories":1,"oversized_inputs":0,"discarded_outputs":0
        });
        observation["output"] = result.to_string().into();
        let canonical = (step.canonical)(&source.to_string()).unwrap();
        assert_eq!((step.canonical)(&canonical), Some(canonical.clone()));
        let migrated = REGISTRY
            .upgrade_payload(kind, from, &canonical)
            .unwrap()
            .unwrap();
        let migrated: Value = serde_json::from_str(&migrated).unwrap();
        assert_decodes_after_migration(kind, crate::test_support::payload_version(kind), &migrated);
        let observation = if kind == "HookFinished" {
            &migrated["observation"]
        } else {
            &migrated
        };
        let command: crate::tools::types::CommandResult =
            serde_json::from_str(observation["output"].as_str().unwrap()).unwrap();
        let selection = command.selection.unwrap();
        assert_eq!(selection.elapsed_ms, None);
        assert_eq!(selection.ignored_entries, 2);
        assert_eq!(command.stdout, result["stdout"].as_str().unwrap());
        assert_eq!(command.stderr, result["stderr"].as_str().unwrap());
        assert_eq!(
            command.exit_code,
            serde_json::from_value::<Option<i32>>(result["exit_code"].clone()).unwrap()
        );
        assert!(serde_json::from_value::<crate::tools::types::CommandResult>(result).is_err());
        for timing in [Value::Null, serde_json::json!(6_000)] {
            let mut forged = source.clone();
            let observation = if kind == "HookFinished" {
                &mut forged["observation"]
            } else {
                &mut forged
            };
            let mut output: Value =
                serde_json::from_str(observation["output"].as_str().unwrap()).unwrap();
            output["selection"]["elapsed_ms"] = timing;
            observation["output"] = output.to_string().into();
            assert!((step.canonical)(&forged.to_string()).is_none());
            assert!((step.transform)(forged).is_none());
        }
    }
}

#[test]
fn selection_timing_migration_refits_a_full_result_without_losing_totals() {
    let mut source: Value = serde_json::from_str(SELECTION_TOOLFINISHED).unwrap();
    let budget = source["intent"]["limits"]["result_bytes"].as_u64().unwrap() as usize;
    let mut result: Value = serde_json::from_str(source["output"].as_str().unwrap()).unwrap();
    result["selection"] = serde_json::json!({
        "ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0
    });
    result["stdout"] = "x".repeat(budget).into();
    result["stdout_total"] = serde_json::json!({"bytes":budget,"lines":1});
    let overhead = result.to_string().len() - budget;
    let length = budget - overhead;
    result["stdout"] = "x".repeat(length).into();
    result["stdout_total"]["bytes"] = length.into();
    assert_eq!(result.to_string().len(), budget);
    source["output"] = result.to_string().into();
    let step = REGISTRY.event_step("ToolFinished", 30).unwrap();
    let migrated = (step.transform)(source).unwrap();
    assert_decodes_after_migration(
        "ToolFinished",
        crate::test_support::payload_version("ToolFinished"),
        &migrated,
    );
    let output = migrated["output"].as_str().unwrap();
    assert!(output.len() <= budget);
    let command: crate::tools::types::CommandResult = serde_json::from_str(output).unwrap();
    assert_eq!(command.stdout_total.bytes, length as u64);
    assert_eq!(command.stdout_total.lines, 1);
    assert!(command.stdout.len() < length);
    assert_eq!(command.selection.unwrap().elapsed_ms, None);
}
