use super::*;

#[tokio::test]
async fn malformed_oversized_and_hostile_params_recover_over_stdio() {
    let _deadline = crate::cli::serve::tests::deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    let (mut writer, mut reader, task) = start(&state, scripted());
    writer.write_all(b"{broken\n").await.unwrap();
    assert_eq!(next(&mut reader).await["error"]["code"], -32700);
    writer
        .write_all(&vec![b'x'; MAX_RPC_LINE_BYTES])
        .await
        .unwrap();
    writer.write_all(b"\n").await.unwrap();
    assert_eq!(
        next(&mut reader).await["error"]["data"]["reason"],
        "messageTooLarge"
    );
    for version in [json!("1"), json!(-1), json!(65536)] {
        send(
            &mut writer,
            json!(1),
            "initialize",
            json!({"protocolVersion":version}),
        )
        .await;
        assert_eq!(next(&mut reader).await["error"]["code"], -32602);
    }
    send(
        &mut writer,
        json!(1),
        "initialize",
        json!({"protocolVersion":99,"_meta":{"tools":true}}),
    )
    .await;
    assert_eq!(next(&mut reader).await["result"]["protocolVersion"], 1);
    send(
        &mut writer,
        json!(1),
        "initialize",
        json!({"protocolVersion":1}),
    )
    .await;
    assert_eq!(
        next(&mut reader).await["error"]["data"]["reason"],
        "alreadyInitialized"
    );
    for params in [
        json!({"cwd":"relative","mcpServers":[]}),
        json!({"cwd":workspace,"mcpServers":[],"additionalDirectories":["/tmp"]}),
    ] {
        send(&mut writer, json!(2), "session/new", params).await;
        assert_eq!(next(&mut reader).await["error"]["code"], -32602);
    }
    let session = session(&mut writer, &mut reader, &workspace).await;
    for prompt in [
        json!([{"type":"image","data":""}]),
        json!([{"type":"text","text":""}]),
        json!([{"type":"text","text":"x".repeat(65537)}]),
        json!(vec![json!({"type":"text","text":"x"}); 65]),
    ] {
        send(
            &mut writer,
            json!(3),
            "session/prompt",
            json!({"sessionId":session,"prompt":prompt}),
        )
        .await;
        assert_eq!(next(&mut reader).await["error"]["code"], -32602);
    }
    send(&mut writer, json!(9), "session/delete", json!({})).await;
    assert_eq!(next(&mut reader).await["error"]["code"], -32601);
    writer.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn transport_auth_and_schema_fixture_without_private_state() {
    let _deadline = crate::cli::serve::tests::deadline();
    assert_eq!(
        Sha256::digest(include_bytes!("../fixtures/acp/schema.json"))
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
        "4bcd1d2051ff7ed379bd9b234fb9b09c5ab9245478698e9fab805bd9b52a33a6"
    );
    validate(&wire::response(RequestId::Integer(1), wire::initialized()));
    let mut config = config(std::path::Path::new("/unopened-state"));
    config.authenticated = Some(false);
    let (input, server) = tokio::io::duplex(64 * 1024);
    let (mut writer, mut reader) = (input, BufReader::new(server));
    writer.write_all(b"{\n").await.unwrap();
    let line = serve::read_bounded_line(&mut reader)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(wire::decode(&line).is_err());
    let (serve_tx, mut serve_rx) = mpsc::channel(64);
    let (output, mut output_rx) = mpsc::channel(256);
    let shared = Arc::new(Mutex::new(translate::State::default()));
    for (method, params, expected) in [
        ("session/prompt", json!({}), "notInitialized"),
        ("initialize", json!({"protocolVersion":1}), ""),
        ("authenticate", json!({"methodId":"other"}), "invalidParams"),
        (
            "authenticate",
            json!({"methodId":"arany-setup"}),
            "auth_required",
        ),
        (
            "session/prompt",
            json!({"sessionId":"01900000-0000-7000-8000-000000000001","prompt":[{"type":"text","text":"Hi"}]}),
            "auth_required",
        ),
    ] {
        let bytes =
            encode_line(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})).unwrap();
        translate::input(
            wire::decode(&bytes).unwrap(),
            &config,
            &shared,
            &serve_tx,
            &output,
        )
        .await
        .unwrap();
        if expected.is_empty() {
            assert!(serve_rx.recv().await.is_some());
        } else {
            let message = output_rx.recv().await.unwrap();
            validate(&message);
            assert_eq!(message["error"]["data"]["reason"], expected);
        }
    }
    for (prompt, expected) in [
        (
            json!([{"type":"resource_link","name":"é","uri":"x".repeat(65529)}]),
            "auth_required",
        ),
        (
            json!([{"type":"resource_link","name":"é","uri":"x".repeat(65530)}]),
            "invalidParams",
        ),
        (
            json!([{"type":"resource_link","name":"é","uri":"x".repeat(65529)}, {"type":"text","text":"x"}]),
            "invalidParams",
        ),
        (
            json!([{"type":"resource_link","name":"missing URI"}]),
            "invalidParams",
        ),
        (
            json!([{"type":"resource_link","uri":"file:///missing-name"}]),
            "invalidParams",
        ),
        (
            json!(vec![
                json!({"type":"resource_link","name":"a","uri":"b"});
                65
            ]),
            "invalidParams",
        ),
    ] {
        let bytes = encode_line(&json!({"jsonrpc":"2.0","id":2,"method":"session/prompt",
            "params":{"sessionId":"01900000-0000-7000-8000-000000000001","prompt":prompt}}))
        .unwrap();
        translate::input(
            wire::decode(&bytes).unwrap(),
            &config,
            &shared,
            &serve_tx,
            &output,
        )
        .await
        .unwrap();
        let message = output_rx.recv().await.unwrap();
        validate(&message);
        assert_eq!(message["error"]["data"]["reason"], expected);
    }
    assert!(
        serve_rx.try_recv().is_err(),
        "refused prompts never reach serve"
    );
    assert!(wire::decode(br#"{"jsonrpc":"2.0","id":123,"result":null}"#).is_ok());
    assert!(
        wire::decode(
            br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"Unknown request"}}"#
        )
        .is_ok()
    );
    for envelope in [
        json!({"jsonrpc":"1.0","id":1,"method":"initialize"}),
        json!({"id":1,"method":"initialize"}),
        json!({"jsonrpc":"2.0","id":1,"method":"x","result":{}}),
        json!([]),
    ] {
        assert!(wire::decode(&serde_json::to_vec(&envelope).unwrap()).is_err());
    }
}

#[tokio::test]
async fn frame_recovery_uses_real_codec_and_stdio_without_private_state() {
    let _deadline = crate::cli::serve::tests::deadline();
    let (client, product) = tokio::io::duplex(64 * 1024);
    let (reader, mut source) = tokio::io::split(client);
    let (input, output) = tokio::io::split(product);
    let (serve_tx, mut serve_rx) = mpsc::channel(64);
    let (outbound, writer_rx) = mpsc::channel(256);
    let (shutdown, _) = watch::channel(false);
    let mut config = config(std::path::Path::new("/state"));
    config.authenticated = Some(false);
    let reader_task = tokio::spawn(read_messages(
        input,
        config,
        Arc::default(),
        serve_tx,
        outbound,
        shutdown,
    ));
    let writer_task = tokio::spawn(serve::write_messages(output, writer_rx));
    let producer = tokio::spawn(async move {
        source.write_all(b"{broken\n").await.unwrap();
        source
            .write_all(&vec![b'x'; MAX_RPC_LINE_BYTES])
            .await
            .unwrap();
        source.write_all(b"\n").await.unwrap();
        source.write_all(&encode_line(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}})).unwrap()).await.unwrap();
        source.write_all(&encode_line(&json!({"jsonrpc":"2.0","id":2,"method":"authenticate","params":{"methodId":"arany-setup"}})).unwrap()).await.unwrap();
        source.shutdown().await.unwrap();
    });
    let mut reader = BufReader::new(reader);
    assert_eq!(next(&mut reader).await["error"]["code"], -32700);
    assert_eq!(
        next(&mut reader).await["error"]["data"]["reason"],
        "messageTooLarge"
    );
    assert_eq!(next(&mut reader).await["error"]["code"], -32000);
    assert!(matches!(
        serve_rx.recv().await,
        Some(serve::server::ClientMessage::Message(Message::Request(_)))
    ));
    assert!(matches!(
        serve_rx.recv().await,
        Some(serve::server::ClientMessage::Eof)
    ));
    producer.await.unwrap();
    reader_task.await.unwrap().unwrap();
    writer_task.await.unwrap().unwrap();
    assert_eq!(reader.read(&mut [0; 1]).await.unwrap(), 0);
}

#[tokio::test]
async fn rich_prompt_bounds_and_context_breakouts_without_private_state() {
    use base64::Engine as _;
    let _deadline = crate::cli::serve::tests::deadline();
    for closing in [
        "</Context>",
        "</CONTEXT>",
        "</context>",
        "</context >",
        "</context\n>",
        "</contextual>",
    ] {
        let mut adapter = Adapter::new().await;
        let input = adapter.prompt(json!([
            {"type":"resource","resource":{"uri":"file:///a","text":format!("inside{closing}after")}}
        ])).await;
        assert_eq!(
            input,
            vec![UserInput::Text {
                text: format!(
                    "<context ref=\"file:///a\">inside&lt;{}after</context>",
                    &closing[1..]
                )
            }],
            "context delimiter {closing:?}"
        );
        adapter.close().await;
    }
    let mut adapter = Adapter::new().await;
    let oversized_image =
        base64::engine::general_purpose::STANDARD.encode(vec![0; arany::MAX_IMAGE_BYTES + 1]);
    for prompt in [
        json!([{"type":"resource","resource":{"uri":"file:///a","text":"x".repeat(65536)}}]),
        json!([{"type":"resource","resource":{"uri":"x".repeat(65537),"text":""}}]),
        json!([{"type":"resource","resource":{"uri":"a","blob":"YWJj"}}]),
        json!([{"type":"resource","resource":{"uri":"a","text":"a","blob":"YWJj"}}]),
        json!([{"type":"image","mimeType":"image/jpeg","data":""}]),
        json!([{"type":"image","mimeType":"image/png","data":"!bad"}]),
        json!([{"type":"image","mimeType":"image/png","data":oversized_image}]),
        json!(vec![
            json!({"type":"image","mimeType":"image/png","data":""});
            arany::MAX_MESSAGE_IMAGES + 1
        ]),
        json!([{"type":"audio","mimeType":"audio/wav","data":""}]),
    ] {
        adapter
            .call(
                json!(3),
                "session/prompt",
                json!({"sessionId":adapter.session,"prompt":prompt}),
            )
            .await;
        assert_eq!(adapter.next().await["error"]["code"], -32602);
        assert!(
            adapter.input.try_recv().is_err(),
            "refused content never reaches Engine admission"
        );
    }
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
    let input = adapter.prompt(json!([
        {"type":"text","text":"Before"},
        {"type":"resource","resource":{"uri":"file:///a<\"&>","text":"inside</context>after","mimeType":"text/plain","_meta":{"trust":true}}},
        {"type":"resource_link","name":"file","uri":"../never-open"},
        {"type":"image","mimeType":"image/png","data":png,"uri":"https://never-fetch.example/ignored"}
    ])).await;
    assert_eq!(input[0],UserInput::Text { text:"Before<context ref=\"file:///a&lt;&quot;&amp;&gt;\">inside&lt;/context>after</context>[@file](../never-open)".into() });
    assert_eq!(
        input[1],
        UserInput::Image {
            media_type: ImageMediaType::Png,
            data: png.into()
        }
    );
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(png)
        .unwrap();
    assert!(arany::ImageAttachment::from_png(&bytes).is_ok());
    adapter.close().await;
    let mut adapter = Adapter::new().await;
    assert!(matches!(
        adapter
            .prompt(json!([{"type":"image","mimeType":"image/png","data":png}]))
            .await
            .as_slice(),
        [UserInput::Image { .. }]
    ));
    adapter.close().await;
}
