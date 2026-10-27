use super::*;

#[tokio::test]
async fn requests_before_initialize_and_second_initialize_are_refused() {
    let _deadline = deadline();
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
    let client = async {
        for (message, expected) in [
            (request(0, "unknown", json!({})), Some("notInitialized")),
            (initialize(), None),
            (initialize(), Some("alreadyInitialized")),
            (request(4, "unknown", json!({})), Some("")),
        ] {
            input.send(message).await.unwrap();
            let value = next(&mut output).await;
            if let Some(reason) = expected {
                if reason.is_empty() {
                    assert_eq!(value["error"]["code"], -32601);
                } else {
                    assert_eq!(value["error"]["data"]["reason"], reason);
                }
            } else {
                assert!(value.get("result").is_some());
            }
        }
        input.send(ClientMessage::Eof).await.unwrap();
    };
    let (result, ()) = tokio::join!(
        server::run_with_threads(
            engine(&state),
            state,
            inbound,
            outbound,
            threads::Threads::default()
        ),
        client
    );
    result.unwrap();
}

#[tokio::test]
async fn oversized_and_malformed_lines_are_rejected_without_stopping_the_server() {
    let _deadline = deadline();
    let (mut client, transport) = tokio::io::duplex(4096);
    let (input, mut inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
    let reader = read_messages(transport, input, outbound);
    let peer = async {
        client
            .write_all(&vec![b'x'; MAX_RPC_LINE_BYTES + 33])
            .await
            .unwrap();
        client.write_all(b"\n[]\n{\"id\":1,\"method\":\"initialize\",\"params\":{\"clientInfo\":{\"name\":\"test\",\"version\":\"1\"}}}\n").await.unwrap();
        client.shutdown().await.unwrap();
        assert_eq!(
            next(&mut output).await["error"]["data"]["reason"],
            "messageTooLarge"
        );
        assert_eq!(next(&mut output).await["error"]["code"], -32700);
        assert!(matches!(
            inbound.recv().await,
            Some(ClientMessage::Message(Message::Request(_)))
        ));
        assert!(matches!(inbound.recv().await, Some(ClientMessage::Eof)));
    };
    let (result, ()) = tokio::join!(reader, peer);
    result.unwrap();
}

#[tokio::test]
async fn reader_failure_notifies_the_server_of_eof() {
    let _deadline = deadline();
    let (input, mut inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, _output) = mpsc::channel(OUTBOUND_CAPACITY);
    assert_eq!(
        read_messages(FailingReader, input, outbound).await,
        Err("protocol input failed".into())
    );
    assert!(matches!(inbound.try_recv(), Ok(ClientMessage::Eof)));
}
#[tokio::test]
async fn bounded_reader_backpressures_after_inbound_capacity_pending_requests() {
    let _deadline = deadline();
    use std::{future::Future, task::Poll};
    let (input, mut inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, _output) = mpsc::channel(OUTBOUND_CAPACITY);
    let bytes = b"{\"id\":1,\"method\":\"x\"}\n".repeat(INBOUND_CAPACITY + 1);
    let mut reader = std::pin::pin!(read_messages(bytes.as_slice(), input, outbound));
    let ready = std::future::poll_fn(|cx| Poll::Ready(reader.as_mut().poll(cx).is_ready())).await;
    assert!(!ready);
    assert_eq!(inbound.len(), INBOUND_CAPACITY);
    inbound.recv().await.unwrap();
    let ready = std::future::poll_fn(|cx| Poll::Ready(reader.as_mut().poll(cx).is_ready())).await;
    assert!(!ready, "EOF also waits behind admitted requests");
    assert_eq!(inbound.len(), INBOUND_CAPACITY);
    for _ in 0..INBOUND_CAPACITY {
        inbound.recv().await.unwrap();
    }
    reader.await.unwrap();
    assert!(matches!(inbound.recv().await, Some(ClientMessage::Eof)));
}

#[tokio::test]
async fn bounded_writer_preserves_messages_and_backpressures_at_outbound_capacity() {
    let _deadline = deadline();
    use std::{future::Future, task::Poll};
    let (outbound, writer) = mpsc::channel(OUTBOUND_CAPACITY);
    let message = server::response(
        RequestId::Integer(1),
        json!({"text":"line\nnext\u{001b}[31m"}),
    );
    for _ in 0..OUTBOUND_CAPACITY {
        server::send(&outbound, message.clone()).await.unwrap();
    }
    let extra = outbound.clone();
    let extra_message = message.clone();
    let mut send = Box::pin(async move { server::send(&extra, extra_message).await });
    assert!(!std::future::poll_fn(|cx| Poll::Ready(send.as_mut().poll(cx).is_ready())).await);
    let (transport, mut client) = tokio::io::duplex(128);
    let writing = write_messages(transport, writer);
    let peer = async {
        send.await.unwrap();
        drop(outbound);
        let expected = encode_line(&message).unwrap().repeat(OUTBOUND_CAPACITY + 1);
        let mut actual = vec![0; expected.len()];
        tokio::io::AsyncReadExt::read_exact(&mut client, &mut actual)
            .await
            .unwrap();
        assert_eq!(actual, expected);
    };
    let (result, ()) = tokio::join!(writing, peer);
    result.unwrap();
}

#[test]
fn engine_errors_keep_closed_variant_reasons_and_compiled_messages() {
    for (error, reason) in [
        (
            arany::EngineError::GoalAdmissionUncertain,
            "goalAdmissionUncertain",
        ),
        (
            arany::EngineError::CancellationDrainTimeout,
            "cancellationDrainTimeout",
        ),
        (arany::EngineError::WorkspaceMismatch, "workspaceMismatch"),
    ] {
        let expected = error.to_string();
        let message =
            serde_json::to_value(server::Fault::engine(error).message(Some(RequestId::Integer(9))))
                .unwrap();
        assert_eq!(message["error"]["code"], -32000);
        assert_eq!(message["error"]["message"], expected);
        assert_eq!(message["error"]["data"]["reason"], reason);
    }
}

#[test]
fn events_resync_reports_the_gap_and_last_committed_sequence() {
    let thread = arany::SessionId::new();
    let mut state = items::TurnState {
        last_sequence: 41,
        ..Default::default()
    };
    let notes = items::project(&arany::Tapped::Resync { missed: 300 }, thread, &mut state);
    assert_eq!(notes.len(), 1);
    let message = Message::Notification(notes.into_iter().next().unwrap());
    validate(&message);
    assert_eq!(
        serde_json::to_value(message).unwrap(),
        json!({"jsonrpc":"2.0","method":"events/resync","params":{"threadId":thread,"missed":300,"lastSequence":41}})
    );
    assert_eq!(state.last_sequence, 41);
}
