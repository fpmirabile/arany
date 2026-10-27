use super::*;
use opentelemetry_http::ResponseBodyTooLarge;
use std::{
    future::Future,
    io::{ErrorKind, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::Instant,
};

const RESPONSE_CAP: usize = 4 * 1024 * 1024;
const PEER_WAIT: Duration = Duration::from_secs(10);
const REQUEST_CAP: usize = 4096;

fn collector() -> TcpListener {
    let listener = TcpListener::bind("127.0.0.1:0").expect("local Collector");
    listener.set_nonblocking(true).expect("bounded accept");
    listener
}

fn accept_request(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + PEER_WAIT;
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "Collector request missing");
                thread::yield_now();
            }
            Err(error) => panic!("Collector accept failed: {error}"),
        }
    };
    stream.set_nonblocking(false).expect("blocking socket");
    stream
        .set_read_timeout(Some(PEER_WAIT))
        .expect("read bound");
    stream
        .set_write_timeout(Some(PEER_WAIT))
        .expect("write bound");
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        assert_eq!(stream.read(&mut byte).expect("request header"), 1);
        assert!(request.len() < REQUEST_CAP, "request cap");
        request.push(byte[0]);
    }
    assert!(request.starts_with(b"POST /v1/traces HTTP/1.1\r\n"));
    stream
}

fn request(listener: &TcpListener) -> Request<Bytes> {
    Request::post(format!(
        "http://{}/v1/traces",
        listener.local_addr().expect("Collector address")
    ))
    .body(Bytes::new())
    .expect("empty request")
}

async fn supervised<T>(future: impl Future<Output = T>) -> T {
    let deadline = Instant::now() + PEER_WAIT;
    tokio::pin!(future);
    loop {
        assert!(Instant::now() < deadline, "Collector supervision expired");
        tokio::select! {
            biased;
            result = &mut future => return result,
            _ = tokio::task::yield_now() => {}
        }
    }
}

async fn gate(receiver: &Receiver<()>) {
    loop {
        match receiver.try_recv() {
            Ok(()) => return,
            Err(TryRecvError::Empty) => tokio::task::yield_now().await,
            Err(TryRecvError::Disconnected) => panic!("Collector gate disconnected"),
        }
    }
}

#[test]
#[cfg_attr(arany_guard, ignore = "requires local test IPC")]
fn streaming_response_cap_is_independent_of_the_deadline() {
    let client = BoundedHttpClient::new().expect("bounded HTTP client");
    for length in [RESPONSE_CAP - 1, RESPONSE_CAP, RESPONSE_CAP + 1] {
        let listener = collector();
        let request = request(&listener);
        let peer = thread::spawn(move || {
            let mut stream = accept_request(&listener);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                .expect("response without Content-Length");
            stream
                .write_all(&vec![b'x'; length])
                .expect("streamed body");
            let _ = stream.shutdown(Shutdown::Write);
        });
        client.runtime.block_on(async {
            tokio::time::pause();
            let started = tokio::time::Instant::now();
            let result = supervised(send_bounded(&client.client, request)).await;
            assert_eq!(tokio::time::Instant::now(), started, "no timeout advanced");
            if length > RESPONSE_CAP {
                assert!(
                    result
                        .expect_err("oversize refusal")
                        .is::<ResponseBodyTooLarge>(),
                    "typed response-size refusal required"
                );
            } else {
                let response = result.expect("bounded response accepted");
                assert_eq!(response.status(), 200);
                assert_eq!(response.body().as_ref(), vec![b'x'; length]);
            }
            tokio::time::resume();
        });
        peer.join().expect("Collector thread");
    }
}

#[test]
#[cfg_attr(arany_guard, ignore = "requires local test IPC")]
fn incomplete_response_deadline_releases_the_read_before_the_next_export() {
    let client = BoundedHttpClient::new().expect("bounded HTTP client");
    let listener = collector();
    let first_request = request(&listener);
    let second_request = request(&listener);
    let (partial_sent, partial_seen) = mpsc::sync_channel(1);
    let (disposed, disposal_seen) = mpsc::sync_channel(1);
    let peer = thread::spawn(move || {
        let mut first = accept_request(&listener);
        first
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\npart\r\n")
            .expect("incomplete bounded response");
        partial_sent.send(()).expect("partial response gate");
        let mut byte = [0];
        match first.read(&mut byte) {
            Ok(0) => {}
            Err(error) if error.kind() == ErrorKind::ConnectionReset => {}
            other => panic!("client did not dispose the incomplete response: {other:?}"),
        }
        disposed.send(()).expect("positive disposal gate");
        let mut second = accept_request(&listener);
        second
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .expect("subsequent export response");
    });
    client.runtime.block_on(async {
        tokio::time::pause();
        let response = send_bounded(&client.client, first_request);
        tokio::pin!(response);
        supervised(async {
            tokio::select! {
                result = &mut response => panic!("incomplete response completed early: {result:?}"),
                _ = gate(&partial_seen) => {}
            }
        })
        .await;
        tokio::time::advance(EXPORT_TIMEOUT).await;
        assert!(
            supervised(response).await.is_err(),
            "incomplete body refused"
        );
        supervised(gate(&disposal_seen)).await;
        let next = supervised(send_bounded(&client.client, second_request))
            .await
            .expect("next export succeeds");
        assert_eq!(next.status(), 200);
        assert!(next.body().is_empty());
        tokio::time::resume();
    });
    peer.join().expect("Collector thread");
}
