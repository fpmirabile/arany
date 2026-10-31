use super::renewal::MAX_REFRESH_FORM_BYTES;
use super::{Endpoints, MAX_TOKEN_RESPONSE_BYTES};
use crate::accounts::chatgpt::CLIENT_ID;
use aws_lc_rs::{
    rand::SystemRandom,
    signature::{KeyPair as _, RSA_PKCS1_SHA256, RsaKeyPair},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::Url;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

pub(super) const LONG_TOKEN_LIFETIME_SECONDS: u64 = 10 * 24 * 60 * 60;

pub(super) fn random_value() -> String {
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).expect("test random value");
    URL_SAFE_NO_PAD.encode(random)
}

fn key_parts(pair: &RsaKeyPair) -> (Vec<u8>, Vec<u8>) {
    fn value<'a>(cursor: &mut &'a [u8], tag: u8) -> &'a [u8] {
        assert_eq!(cursor[0], tag);
        let length_byte = cursor[1];
        let (offset, length) = if length_byte & 0x80 == 0 {
            (2, usize::from(length_byte))
        } else {
            let length_bytes = usize::from(length_byte & 0x7f);
            assert!((1..=2).contains(&length_bytes));
            let mut length = 0usize;
            for byte in &cursor[2..2 + length_bytes] {
                length = (length << 8) | usize::from(*byte);
            }
            (2 + length_bytes, length)
        };
        assert!(cursor.len() >= offset + length);
        let result = &cursor[offset..offset + length];
        *cursor = &cursor[offset + length..];
        result
    }

    let mut der = pair.public_key().as_ref();
    let mut sequence = value(&mut der, 0x30);
    assert!(der.is_empty());
    let modulus = value(&mut sequence, 0x02)
        .strip_prefix(&[0])
        .unwrap_or_else(|| unreachable!("generated RSA modulus needs a sign byte"))
        .to_vec();
    let exponent = value(&mut sequence, 0x02).to_vec();
    assert!(sequence.is_empty());
    (modulus, exponent)
}

pub(super) fn claims(now: u64, nonce: &str) -> Value {
    json!({
        "iss": "https://auth.openai.com",
        "aud": CLIENT_ID,
        "iat": now,
        "exp": now + 3600,
        "nonce": nonce,
        "sub": "synthetic-subject",
        "https://api.openai.com/auth": {"chatgpt_account_id": "account-one"}
    })
}

pub(super) fn signed_token_with_header(
    pair: &RsaKeyPair,
    claims: &Value,
    header: &Value,
) -> String {
    let header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(header).expect("header"));
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("claims"));
    let signed = format!("{header}.{payload}");
    let mut signature = vec![0u8; pair.public_modulus_len()];
    pair.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        signed.as_bytes(),
        &mut signature,
    )
    .expect("sign test identity");
    format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature))
}

pub(super) fn signed_token(pair: &RsaKeyPair, claims: &Value) -> String {
    signed_token_with_header(
        pair,
        claims,
        &json!({"alg": "RS256", "kid": "test-key", "typ": "JWT"}),
    )
}

pub(super) fn jwks(pair: &RsaKeyPair) -> Vec<u8> {
    let (modulus, exponent) = key_parts(pair);
    serde_json::to_vec(&json!({"keys": [{
        "kty": "RSA",
        "kid": "test-key",
        "use": "sig",
        "alg": "RS256",
        "n": URL_SAFE_NO_PAD.encode(modulus),
        "e": URL_SAFE_NO_PAD.encode(exponent)
    }]}))
    .expect("test JWKS")
}

pub(super) struct Reply {
    pub(super) status: &'static str,
    pub(super) content_type: &'static str,
    pub(super) encoding: Option<&'static str>,
    pub(super) body: Vec<u8>,
    pub(super) chunked: bool,
}

pub(super) async fn read_issuer_request(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut header_end = None;
    while header_end.is_none() {
        let mut buffer = [0u8; 1024];
        let size = stream.read(&mut buffer).await.expect("read test request");
        assert!(size > 0 && bytes.len() + size <= MAX_TOKEN_RESPONSE_BYTES);
        bytes.extend_from_slice(&buffer[..size]);
        header_end = bytes.windows(4).position(|window| window == b"\r\n\r\n");
    }
    let body_start = header_end.expect("headers") + 4;
    let headers = String::from_utf8(bytes[..body_start].to_vec()).expect("ASCII headers");
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length: ")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    assert!(content_length <= MAX_REFRESH_FORM_BYTES);
    while bytes.len() < body_start + content_length {
        let mut buffer = [0u8; 1024];
        let size = stream.read(&mut buffer).await.expect("read test body");
        assert!(size > 0 && bytes.len() + size <= MAX_TOKEN_RESPONSE_BYTES);
        bytes.extend_from_slice(&buffer[..size]);
    }
    bytes
}

pub(super) async fn issuer(
    replies: Vec<Reply>,
) -> (Endpoints, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test issuer bind");
    let origin = format!(
        "http://{}",
        listener.local_addr().expect("test issuer addr")
    );
    let endpoints = Endpoints {
        token: Url::parse(&format!("{origin}/oauth/token")).expect("test token URL"),
        jwks: Url::parse(&format!("{origin}/.well-known/jwks.json")).expect("test JWKS URL"),
        https_only: false,
    };
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for reply in replies {
            let (mut stream, _) = listener.accept().await.expect("accept test request");
            let bytes = read_issuer_request(&mut stream).await;
            requests.push(bytes);
            let encoding = reply
                .encoding
                .map(|value| format!("Content-Encoding: {value}\r\n"))
                .unwrap_or_default();
            let framing = if reply.chunked {
                "Transfer-Encoding: chunked\r\n".to_owned()
            } else {
                format!("Content-Length: {}\r\n", reply.body.len())
            };
            let location = if reply.status.starts_with("302") {
                "Location: /redirected\r\n"
            } else {
                ""
            };
            let head = format!(
                "HTTP/1.1 {}\r\nContent-Type: {}\r\n{}{}{}Connection: close\r\n\r\n",
                reply.status, reply.content_type, encoding, framing, location
            );
            stream
                .write_all(head.as_bytes())
                .await
                .expect("reply headers");
            if reply.chunked {
                stream
                    .write_all(format!("{:X}\r\n", reply.body.len()).as_bytes())
                    .await
                    .expect("chunk header");
            }
            // Header-level rejection may close the socket before the fixture finishes writing.
            let _ = stream.write_all(&reply.body).await;
            if reply.chunked {
                let _ = stream.write_all(b"\r\n0\r\n\r\n").await;
            }
        }
        requests
    });
    (endpoints, task)
}
