use super::*;

#[test]
fn event_stream_frames_join_data_lines_and_skip_everything_else() {
    let body = b": keep-alive\r\nid: 7\r\nevent: message\r\ndata: {\"a\":\r\ndata:1}\r\n\r\nretry: 5\n\ndata:{\"b\":2}\n\n";
    assert_eq!(
        event_data(body),
        vec![b"{\"a\":\n1}".to_vec(), b"{\"b\":2}".to_vec()]
    );
    assert!(event_data(b": only a comment\n\n").is_empty());
}

#[test]
fn unauthorized_result_names_the_login_action_without_a_token() {
    let server = Server {
        name: "remote".into(),
        program: None,
        args: Vec::new(),
        tools: vec!["echo".into()],
        url: Some("https://mcp.example.com/mcp".into()),
        auth: Some(McpAuth::Oauth),
    };
    let result: Value = serde_json::from_str(&unauthorized(&server)).unwrap();
    assert_eq!(result["error"], "unauthorized");
    assert_eq!(
        result["action"],
        "run `arany mcp login remote` to authorize this server"
    );
}

#[test]
fn partial_events_wait_for_the_rest_of_the_chunk_stream() {
    let mut buffer = b"data: {\"a\":1}\r".to_vec();
    assert!(drain_events(&mut buffer).is_empty());
    buffer.extend_from_slice(b"\n\r\ndata: {\"b\"");
    assert_eq!(drain_events(&mut buffer), vec![b"{\"a\":1}".to_vec()]);
    assert_eq!(buffer, b"data: {\"b\"");
    buffer.extend_from_slice(b":2}\n\n");
    assert_eq!(drain_events(&mut buffer), vec![b"{\"b\":2}".to_vec()]);
    assert!(buffer.is_empty());
}

#[test]
fn reflected_credentials_are_found_in_decoded_strings_and_keys() {
    let token = "tok\"en\\x";
    for frame in [
        json!({"result": {"tools": [{"description": format!("see {token}")}]}}),
        json!({"result": {"content": [{"text": token}]}}),
        json!({"result": {token: 1}}),
    ] {
        let wire = serde_json::to_string(&frame).unwrap();
        assert!(!wire.contains(token), "escaped on the wire");
        assert!(mentions(&frame, token));
    }
    assert!(!mentions(&json!({"result": {"text": "clean"}}), token));
}
