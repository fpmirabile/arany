use super::*;

#[cfg(debug_assertions)]
#[test]
fn fresh_setup_root_saves_and_edits_chat_route_without_provider_egress() {
    let (temp, workspace, state) = workspace_root();
    let mut pty = start_setup_pty(&temp, &workspace, &state);
    pty.send_and_wait(b"2\r", b"Input:\r\n");
    let saved_at = add_custom_route(&mut pty, b"1\r", b"3\r");
    pty.acknowledge_linear_pages((saved_at, b"empty Enter selects 1. first-route"));
    // Without a conformance check the save result is one acknowledgment (terminal.md, Custom routes).
    assert!(
        String::from_utf8_lossy(&pty.transcript[saved_at..])
            .contains("Saved; select it to use this route"),
        "save result names the next step"
    );
    let before_edit = std::fs::read(state.join("provider-profiles.json")).unwrap();
    let route_at = pty.send(b"1\r");
    wait_setup_choices(&mut pty, route_at);
    let inspect_at = pty.send(b"1\r");
    pty.acknowledge_linear_pages((inspect_at, b"empty Enter selects 1. first-route"));
    let details = reviewed_setup_text(&String::from_utf8_lossy(&pty.transcript[inspect_at..]));
    assert!(details.contains("Protocol: OpenAI Chat Completions"));
    edit_custom_chat_route_unchanged(&mut pty);
    assert_changed_protocol_requires_destination(&mut pty);
    pty.send(b"3\r");
    let (result, transcript) = finish_setup_pty_with_transcript(pty);
    assert!(result.status.success(), "fresh custom setup failed");
    let profile = std::fs::read_to_string(state.join("provider-profiles.json"))
        .expect("first custom route record");
    assert!(profile.contains("first-route"));
    assert!(profile.contains("model-synthetic"));
    let document: serde_json::Value = serde_json::from_str(&profile).unwrap();
    assert_eq!(
        document["profiles"][0]["protocol"],
        "openai-chat-completions"
    );
    assert!(document["profiles"][0].get("auth").is_none());
    assert_eq!(profile.as_bytes(), before_edit);
    assert!(!profile.contains(FIRST_ROUTE_KEY_CANARY));
    assert!(!String::from_utf8_lossy(&transcript).contains(FIRST_ROUTE_KEY_CANARY));
    assert_setup_journal(&state, &workspace);
}

#[cfg(debug_assertions)]
fn assert_setup_journal(state: &Path, workspace: &Path) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("journal runtime")
        .block_on(async {
            let sessions = arany::list_sessions(
                StateRoot::open_existing(state).expect("setup StateRoot"),
                workspace.to_owned(),
            )
            .await
            .expect("reopen setup Session list");
            assert_eq!(sessions.len(), 1);
            let store = Store::open(StateRoot::open_existing(state).expect("setup StateRoot"))
                .expect("reopen setup journal");
            let events = store
                .load_session(sessions[0].id)
                .await
                .expect("setup Events");
            assert!(events.iter().all(|envelope| {
                !envelope
                    .event
                    .payload()
                    .expect("setup Event payload")
                    .contains(FIRST_ROUTE_KEY_CANARY)
            }));
            assert!(matches!(
                events[0].event,
                arany::Event::SessionStarted { .. }
            ));
            let operation_id = match &events[1].event {
                arany::Event::ProviderSetupEditStarted {
                    operation_id,
                    target,
                    ..
                } => {
                    assert_eq!(target.identity, "first-route");
                    *operation_id
                }
                event => panic!("unexpected setup Event: {}", event.kind()),
            };
            assert!(matches!(
                &events[2].event,
                arany::Event::ProviderSetupEditFinished {
                    operation_id: finished,
                    disposition: arany::ProviderSetupDisposition::Applied,
                    resulting_digest: Some(_),
                } if *finished == operation_id
            ));
            let view = store
                .load_view(sessions[0].id)
                .await
                .expect("reopened setup Session view")
                .expect("setup Session");
            assert_eq!(view.pending_provider_setup_edit, None);
            assert!(!view.provider_setup_uncertain);
            store.close().await.expect("close setup store");
        });
}

#[cfg(debug_assertions)]
fn edit_custom_chat_route_unchanged(pty: &mut Pty) {
    let edit_start = pty.transcript.len();
    let mut cursor = edit_start;
    pty.send(b"1\r");
    wait_setup_choices(pty, cursor);
    cursor = pty.transcript.len();
    pty.send(b"3\r");
    wait_setup_choices(pty, cursor);
    assert!(
        String::from_utf8_lossy(&pty.transcript[cursor..])
            .contains("empty Enter selects 3. OpenAI Chat Completions")
    );
    cursor = pty.transcript.len();
    pty.send(b"\r");
    for _ in 0..4 {
        pty.acknowledge_linear_pages((cursor, b"Input:\r\n"));
        cursor = pty.transcript.len();
        pty.send(b"\r");
    }
    wait_setup_choices(pty, cursor);
    assert!(String::from_utf8_lossy(&pty.transcript[cursor..]).contains("Declared efforts"));
    cursor = pty.transcript.len();
    pty.send(b"\r");
    pty.acknowledge_linear_pages((cursor, b"empty Enter selects 1. Back\r\nInput:\r\n"));
    cursor = pty.transcript.len();
    pty.send(b"2\r");
    pty.acknowledge_linear_pages((cursor, b"empty Enter selects 1. first-route"));
    assert!(
        String::from_utf8_lossy(&pty.transcript[edit_start..])
            .contains("Arany adds /chat/completions")
    );
}

#[cfg(debug_assertions)]
fn assert_changed_protocol_requires_destination(pty: &mut Pty) {
    let mut cursor = pty.transcript.len();
    pty.send(b"1\r");
    wait_setup_choices(pty, cursor);
    cursor = pty.transcript.len();
    pty.send(b"3\r");
    wait_setup_choices(pty, cursor);
    cursor = pty.transcript.len();
    pty.send(b"1\r");
    pty.acknowledge_linear_pages((cursor, b"Input:\r\n"));
    cursor = pty.transcript.len();
    pty.send(b"\r");
    wait_setup_choices(pty, cursor);
    let destination_prompt = String::from_utf8_lossy(&pty.transcript[cursor..]);
    assert!(destination_prompt.contains("Exact canonical /responses URL"));
    assert!(!destination_prompt.contains("Current value:"));
    assert!(!destination_prompt.contains("empty Enter keeps it"));
    cursor = pty.transcript.len();
    pty.send(b"\r");
    wait_setup_choices(pty, cursor);
    assert!(
        String::from_utf8_lossy(&pty.transcript[cursor..]).contains("This field cannot be empty")
    );
    cursor = pty.transcript.len();
    pty.send(b"\x03");
    wait_setup_choices(pty, cursor);
}

#[cfg(debug_assertions)]
fn seed_long_custom_profiles(state: &Path) -> String {
    let long_name = "z".repeat(arany::MAX_CUSTOM_PROFILE_NAME_BYTES);
    let profile = |name: &str, model: &str, credential_env: &str| arany::CustomProfileDraft {
        name: name.into(),
        url: "https://api.example.com/v1/responses".into(),
        model: model.into(),
        credential_env: credential_env.into(),
        ..arany::test_support::local_custom_profile(0)
    };
    arany::test_support::save_custom_profiles(
        state,
        &[
            profile(
                "first-route",
                "first-model",
                "ARANY_PROVIDER_FIRST_ROUTE_KEY",
            ),
            profile(
                &long_name,
                "second-model",
                "ARANY_PROVIDER_SECOND_ROUTE_KEY",
            ),
        ],
    );
    long_name
}

#[cfg(debug_assertions)]
#[test]
fn linear_setup_selects_nonfirst_maximum_length_custom_route() {
    let (temp, workspace, state) = workspace_root();
    let long_name = seed_long_custom_profiles(&state);
    let mut pty = start_setup_pty(&temp, &workspace, &state);
    let routes_at = pty.send(b"2\r");
    let route_prompt =
        b"Declarations are saved privately; each Run admits the selected route; type a listed choice number or exact name (1-4); empty Enter selects 1\r\nInput:\r\n";
    pty.acknowledge_linear_pages((routes_at, route_prompt));
    pty.send(b"2\r");
    pty.wait_for(b"ARANY_PROVIDER_SECOND_ROUTE_KEY");
    pty.wait_for_after(routes_at, b"empty Enter selects 1. Inspect\r\nInput:\r\n");
    let inspect_at = pty.send(b"1\r");
    let (_, list_at) = pty.acknowledge_linear_pages((inspect_at, b"Page 1/"));
    pty.acknowledge_linear_pages((list_at, route_prompt));
    pty.wait_for(b"Model: second-model");
    assert!(
        String::from_utf8_lossy(&pty.transcript).contains(&long_name),
        "selected route title did not preserve its maximum-length name"
    );
    pty.send(b"Back\r");
    let result = finish_setup_pty(pty);
    assert!(
        result.status.success(),
        "linear custom route selection failed"
    );
}
