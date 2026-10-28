use super::*;

#[test]
fn command_tool_migrations_preserve_old_bytes_and_refuse_new_provenance_in_old_rows() {
    for (kind, from, input) in [
        ("RunStarted", 25, include_str!("runstarted-v24.json")),
        ("ToolProposed", 4, include_str!("toolproposed-v1.json")),
        ("ToolStarted", 26, include_str!("toolstarted-v23.json")),
        ("ToolFinished", 33, SELECTION_TOOLFINISHED),
        (
            "HookStarted",
            15,
            include_str!("hookstarted-without-expiry.json"),
        ),
        ("HookFinished", 22, SELECTION_HOOKFINISHED),
    ] {
        let step = REGISTRY.event_step(kind, from).unwrap();
        assert_eq!((step.canonical)(input).as_deref(), Some(input));
        let value: Value = serde_json::from_str(input).unwrap();
        assert_eq!((step.transform)(value.clone()), Some(value.clone()));
        let mut forged = value;
        forged
            .as_object_mut()
            .unwrap()
            .insert("rewrite".into(), serde_json::json!([]));
        assert!((step.canonical)(&forged.to_string()).is_none());
        assert!((step.transform)(forged).is_none());
    }
}
