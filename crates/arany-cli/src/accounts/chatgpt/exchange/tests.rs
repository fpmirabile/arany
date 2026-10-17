use super::*;
use aws_lc_rs::{
    rand::SystemRandom,
    rsa::KeySize,
    signature::{KeyPair as _, RSA_PKCS1_SHA256, RsaKeyPair},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const NOW: u64 = 1_800_000_000;

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

fn claims(now: u64, nonce: &str) -> Value {
    json!({
        "iss": "https://auth.openai.com",
        "aud": "oaiapp_test",
        "iat": now,
        "exp": now + 3600,
        "nonce": nonce,
        "sub": "synthetic-subject"
    })
}

fn signed_token_with_header(pair: &RsaKeyPair, claims: &Value, header: &Value) -> String {
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

fn signed_token(pair: &RsaKeyPair, claims: &Value) -> String {
    signed_token_with_header(
        pair,
        claims,
        &json!({"alg": "RS256", "kid": "test-key", "typ": "JWT"}),
    )
}

fn jwks(pair: &RsaKeyPair) -> Vec<u8> {
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

#[test]
fn signed_identity_requires_exact_issuer_client_nonce_and_time() {
    let pair = RsaKeyPair::generate(KeySize::Rsa2048).expect("test RSA key");
    let keys = jwks(&pair);
    let good = signed_token(&pair, &claims(NOW, "expected-nonce"));
    assert_eq!(
        identity::verify(&good, &keys, "oaiapp_test", "expected-nonce", NOW),
        Ok("synthetic-subject".into())
    );
    let mut singleton = claims(NOW, "expected-nonce");
    singleton["aud"] = json!(["oaiapp_test"]);
    assert_eq!(
        identity::verify(
            &signed_token(&pair, &singleton),
            &keys,
            "oaiapp_test",
            "expected-nonce",
            NOW
        ),
        Ok("synthetic-subject".into()),
        "a signed singleton audience is the same issued client"
    );
    for audience in [json!("oaiapp_test"), json!(["oaiapp_test"])] {
        let mut bound_party = claims(NOW, "expected-nonce");
        bound_party["aud"] = audience;
        bound_party["azp"] = json!("oaiapp_test");
        assert_eq!(
            identity::verify(
                &signed_token(&pair, &bound_party),
                &keys,
                "oaiapp_test",
                "expected-nonce",
                NOW
            ),
            Ok("synthetic-subject".into()),
            "present authorized party is the issued client"
        );
        bound_party["azp"] = json!("another-client");
        assert_eq!(
            identity::verify(
                &signed_token(&pair, &bound_party),
                &keys,
                "oaiapp_test",
                "expected-nonce",
                NOW
            ),
            Err(AuthorizationError::IdentityRejected(
                identity::IdentityFailure::AuthorizedParty
            )),
            "neither audience representation admits a different authorized party"
        );
    }
    for (field, replacement, failure) in [
        (
            "aud",
            json!(["different-client"]),
            identity::IdentityFailure::Audience,
        ),
        ("aud", json!([]), identity::IdentityFailure::ClaimsFormat),
        (
            "aud",
            json!(["oaiapp_test", "another-client"]),
            identity::IdentityFailure::ClaimsFormat,
        ),
        (
            "aud",
            json!(["another-client", "oaiapp_test"]),
            identity::IdentityFailure::ClaimsFormat,
        ),
        (
            "aud",
            json!(["oaiapp_test", "oaiapp_test"]),
            identity::IdentityFailure::ClaimsFormat,
        ),
        (
            "aud",
            json!([null]),
            identity::IdentityFailure::ClaimsFormat,
        ),
        ("aud", json!([42]), identity::IdentityFailure::ClaimsFormat),
        (
            "aud",
            json!([["oaiapp_test"]]),
            identity::IdentityFailure::ClaimsFormat,
        ),
        ("aud", json!(null), identity::IdentityFailure::ClaimsFormat),
        ("aud", json!(42), identity::IdentityFailure::ClaimsFormat),
        ("aud", json!(true), identity::IdentityFailure::ClaimsFormat),
        (
            "aud",
            json!({"client": "oaiapp_test"}),
            identity::IdentityFailure::ClaimsFormat,
        ),
        (
            "azp",
            json!("another-client"),
            identity::IdentityFailure::AuthorizedParty,
        ),
        ("azp", json!(""), identity::IdentityFailure::AuthorizedParty),
        ("azp", json!(null), identity::IdentityFailure::ClaimsFormat),
        (
            "azp",
            json!(["oaiapp_test"]),
            identity::IdentityFailure::ClaimsFormat,
        ),
        ("azp", json!(true), identity::IdentityFailure::ClaimsFormat),
    ] {
        let mut rejected = claims(NOW, "expected-nonce");
        rejected[field] = replacement;
        assert_eq!(
            identity::verify(
                &signed_token(&pair, &rejected),
                &keys,
                "oaiapp_test",
                "expected-nonce",
                NOW
            ),
            Err(AuthorizationError::IdentityRejected(failure)),
            "rejected {field} fixture"
        );
    }
    let mut altered = claims(NOW, "expected-nonce");
    for (field, replacement, failure) in [
        (
            "iss",
            json!("https://another.example"),
            identity::IdentityFailure::Issuer,
        ),
        (
            "aud",
            json!("different-client"),
            identity::IdentityFailure::Audience,
        ),
        (
            "nonce",
            json!("wrong-nonce"),
            identity::IdentityFailure::Nonce,
        ),
        ("sub", json!(""), identity::IdentityFailure::Subject),
        ("exp", json!(NOW - 60), identity::IdentityFailure::Lifetime),
        ("iat", json!(NOW + 60), identity::IdentityFailure::Lifetime),
        ("nbf", json!(NOW + 60), identity::IdentityFailure::Lifetime),
    ] {
        let original = altered[field].clone();
        altered[field] = replacement;
        let token = signed_token(&pair, &altered);
        assert_eq!(
            identity::verify(&token, &keys, "oaiapp_test", "expected-nonce", NOW),
            Err(AuthorizationError::IdentityRejected(failure)),
            "{field}"
        );
        if field == "nonce" {
            let error = identity::verify(&token, &keys, "oaiapp_test", "expected-nonce", NOW)
                .expect_err("mismatched nonce");
            assert!(error.to_string().contains("nonce mismatch"), "{error}");
            assert!(!error.to_string().contains("wrong-nonce"));
        }
        altered[field] = original;
    }
    let (signed, signature) = good.rsplit_once('.').expect("JWT signature");
    let mut signature = URL_SAFE_NO_PAD.decode(signature).expect("signature bytes");
    signature[0] ^= 1;
    let tampered = format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature));
    assert_eq!(
        identity::verify(&tampered, &keys, "oaiapp_test", "expected-nonce", NOW),
        Err(AuthorizationError::IdentityRejected(
            identity::IdentityFailure::Signature
        ))
    );
    for (header, failure) in [
        (
            json!({"alg": "none", "kid": "test-key"}),
            identity::IdentityFailure::Algorithm,
        ),
        (
            json!({"alg": "RS256", "kid": "different-key"}),
            identity::IdentityFailure::KeyId,
        ),
        (
            json!({"alg": "RS256", "kid": "test-key", "crit": ["unknown"]}),
            identity::IdentityFailure::HeaderFormat,
        ),
        (
            json!({"alg": "RS256", "kid": "test-key", "b64": false}),
            identity::IdentityFailure::HeaderFormat,
        ),
    ] {
        let token = signed_token_with_header(&pair, &claims(NOW, "expected-nonce"), &header);
        assert_eq!(
            identity::verify(&token, &keys, "oaiapp_test", "expected-nonce", NOW),
            Err(AuthorizationError::IdentityRejected(failure)),
            "{header}"
        );
    }
    let duplicate = serde_json::to_vec(&json!({"keys": [
        serde_json::from_slice::<Value>(&keys).expect("JWKS")["keys"][0],
        serde_json::from_slice::<Value>(&keys).expect("JWKS")["keys"][0]
    ]}))
    .expect("duplicate JWKS");
    assert_eq!(
        identity::verify(&good, &duplicate, "oaiapp_test", "expected-nonce", NOW),
        Err(AuthorizationError::IdentityRejected(
            identity::IdentityFailure::KeyId
        ))
    );
    let wrong_key = RsaKeyPair::generate(KeySize::Rsa2048).expect("other test RSA key");
    assert_eq!(
        identity::verify(
            &good,
            &jwks(&wrong_key),
            "oaiapp_test",
            "expected-nonce",
            NOW
        ),
        Err(AuthorizationError::IdentityRejected(
            identity::IdentityFailure::Signature
        ))
    );
    let too_many = serde_json::to_vec(&json!({
        "keys": vec![serde_json::from_slice::<Value>(&keys).expect("JWKS")["keys"][0].clone(); 17]
    }))
    .expect("oversized JWKS");
    assert_eq!(
        identity::verify(&good, &too_many, "oaiapp_test", "expected-nonce", NOW),
        Err(AuthorizationError::IdentityRejected(
            identity::IdentityFailure::KeySet
        ))
    );
}

struct Reply {
    status: &'static str,
    content_type: &'static str,
    encoding: Option<&'static str>,
    body: Vec<u8>,
    chunked: bool,
}

async fn issuer(replies: Vec<Reply>) -> (Endpoints, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test issuer bind");
    let origin = format!(
        "http://{}",
        listener.local_addr().expect("test issuer addr")
    );
    let endpoints = Endpoints {
        token: Url::parse(&format!("{origin}/api/accounts/oauth/token")).expect("test token URL"),
        jwks: Url::parse(&format!("{origin}/.well-known/jwks.json")).expect("test JWKS URL"),
        https_only: false,
    };
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for reply in replies {
            let (mut stream, _) = listener.accept().await.expect("accept test request");
            let mut bytes = Vec::new();
            let mut header_end = None;
            while header_end.is_none() {
                let mut buffer = [0u8; 1024];
                let size = stream.read(&mut buffer).await.expect("read test request");
                assert!(size > 0 && bytes.len() + size <= 32 * 1024);
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
                assert!(size > 0 && bytes.len() + size <= 32 * 1024);
                bytes.extend_from_slice(&buffer[..size]);
            }
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

fn exchange() -> CodeExchange {
    CodeExchange {
        code: "synthetic-code".into(),
        client_id: "oaiapp_test".into(),
        redirect_uri: "http://127.0.0.1:1455/auth/callback".into(),
        verifier: "synthetic-verifier".into(),
        nonce: "synthetic-nonce".into(),
        host_id: uuid::Uuid::parse_str("123e4567-e89b-42d3-a456-426614174000").unwrap(),
        expected_subject: None,
    }
}

fn token_reply(id_token: &str, scope: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "access_token": "synthetic-access",
        "refresh_token": "synthetic-refresh",
        "id_token": id_token,
        "token_type": "Bearer",
        "expires_in": 3600,
        "scope": scope
    }))
    .expect("test token reply")
}

#[tokio::test]
async fn redemption_sends_exact_grant_and_returns_only_verified_credentials() {
    let pair = RsaKeyPair::generate(KeySize::Rsa2048).expect("test RSA key");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("test clock")
        .as_secs();
    let mut identity = claims(now, "synthetic-nonce");
    identity["aud"] = serde_json::json!(["oaiapp_test"]);
    identity["azp"] = serde_json::json!("oaiapp_test");
    let id_token = signed_token(&pair, &identity);
    let replies = vec![
        Reply {
            status: "200 OK",
            content_type: "application/json",
            encoding: None,
            body: token_reply(
                &id_token,
                "openid offline_access resource.invoke chatgpt.tokens.use.direct",
            ),
            chunked: false,
        },
        Reply {
            status: "200 OK",
            content_type: "application/jwk-set+json",
            encoding: None,
            body: jwks(&pair),
            chunked: false,
        },
    ];
    let (endpoints, task) = issuer(replies).await;
    let attempt = exchange();
    let host_id = attempt.host_id;
    let VerifiedSignIn::PlanEnabled(verified) =
        redeem_at(attempt, endpoints).await.expect("verified grant")
    else {
        panic!("complete permission must enable the plan");
    };
    assert_eq!(verified.client_id, "oaiapp_test");
    assert_eq!(verified.host_id, host_id);
    assert_eq!(verified.subject, "synthetic-subject");
    assert_eq!(verified.id_token, id_token);
    assert_eq!(verified.access_token, "synthetic-access");
    assert_eq!(verified.refresh_token, "synthetic-refresh");
    assert!(verified.access_expires_at_unix > now);
    let requests = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("issuer deadline")
        .expect("issuer task");
    assert_eq!(requests.len(), 2);
    let token_request = String::from_utf8(requests[0].clone()).expect("form request");
    assert!(token_request.starts_with("POST /api/accounts/oauth/token HTTP/1.1\r\n"));
    assert!(token_request.contains("content-type: application/x-www-form-urlencoded\r\n"));
    let form = token_request.split("\r\n\r\n").nth(1).expect("form body");
    let fields = url::form_urlencoded::parse(form.as_bytes())
        .into_owned()
        .collect::<Vec<_>>();
    assert_eq!(
        fields,
        [
            ("grant_type".into(), "authorization_code".into()),
            ("client_id".into(), "oaiapp_test".into()),
            ("code".into(), "synthetic-code".into()),
            ("code_verifier".into(), "synthetic-verifier".into()),
            (
                "redirect_uri".into(),
                "http://127.0.0.1:1455/auth/callback".into()
            ),
            ("resource".into(), "https://api.openai.com/v1".into())
        ]
    );
    let jwks_request = String::from_utf8(requests[1].clone()).expect("JWKS request");
    assert!(jwks_request.starts_with("GET /.well-known/jwks.json HTTP/1.1\r\n"));
    assert!(!jwks_request.contains("synthetic-access"));
    assert!(!jwks_request.contains("synthetic-refresh"));
    assert!(!jwks_request.contains(&id_token));
}

#[tokio::test]
async fn returning_redemption_rejects_a_signed_but_different_account() {
    let pair = RsaKeyPair::generate(KeySize::Rsa2048).expect("test RSA key");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("test clock")
        .as_secs();
    let id_token = signed_token(&pair, &claims(now, "synthetic-nonce"));
    for scope in [
        "openid offline_access resource.invoke chatgpt.tokens.use.direct",
        "openid offline_access resource.invoke",
    ] {
        for (expected_subject, admitted) in [
            ("synthetic-subject", true),
            ("different-saved-subject", false),
        ] {
            let replies = vec![
                Reply {
                    status: "200 OK",
                    content_type: "application/json",
                    encoding: None,
                    body: token_reply(&id_token, scope),
                    chunked: false,
                },
                Reply {
                    status: "200 OK",
                    content_type: "application/jwk-set+json",
                    encoding: None,
                    body: jwks(&pair),
                    chunked: false,
                },
            ];
            let (endpoints, task) = issuer(replies).await;
            let mut selected = exchange();
            selected.expected_subject = Some(expected_subject.into());
            let result = redeem_at(selected, endpoints).await;
            if admitted {
                let subject = match result.expect("same signed account") {
                    VerifiedSignIn::PlanEnabled(verified) => {
                        assert!(scope.contains("chatgpt.tokens.use.direct"));
                        verified.subject
                    }
                    VerifiedSignIn::PlanDisabled(identity) => {
                        assert!(!scope.contains("chatgpt.tokens.use.direct"));
                        identity.subject
                    }
                };
                assert_eq!(subject, expected_subject);
            } else {
                assert!(matches!(
                    result,
                    Err(AuthorizationError::IdentityRejected(
                        identity::IdentityFailure::SelectedSubject
                    ))
                ));
            }
            let requests = tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("issuer deadline")
                .expect("issuer task");
            assert_eq!(requests.len(), 2);
            let token_request = String::from_utf8(requests[0].clone()).expect("form request");
            assert!(!token_request.contains(expected_subject));
            assert!(!token_request.contains(&id_token));
        }
    }
}

#[tokio::test]
async fn redemption_keeps_disabled_identity_and_rejects_failed_responses() {
    let pair = RsaKeyPair::generate(KeySize::Rsa2048).expect("test RSA key");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("test clock")
        .as_secs();
    let id_token = signed_token(&pair, &claims(now, "synthetic-nonce"));
    for (scope, retain_refresh) in [
        ("openid offline_access resource.invoke", true),
        ("openid", false),
        ("openid chatgpt.tokens.use.direct offline_access", true),
        ("openid chatgpt.tokens.use.direct resource.invoke", false),
    ] {
        let mut reply: Value = serde_json::from_slice(&token_reply(&id_token, scope)).unwrap();
        if !retain_refresh {
            reply.as_object_mut().unwrap().remove("refresh_token");
        }
        let (endpoints, task) = issuer(vec![
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: None,
                body: serde_json::to_vec(&reply).unwrap(),
                chunked: false,
            },
            Reply {
                status: "200 OK",
                content_type: "application/jwk-set+json",
                encoding: None,
                body: jwks(&pair),
                chunked: false,
            },
        ])
        .await;
        let VerifiedSignIn::PlanDisabled(identity) = redeem_at(exchange(), endpoints)
            .await
            .expect("verified identity must survive absent plan permission")
        else {
            panic!("missing permission must not return usable credentials");
        };
        assert_eq!(identity.client_id, "oaiapp_test");
        assert_eq!(identity.host_id, exchange().host_id);
        assert_eq!(identity.subject, "synthetic-subject");
        let requests = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("issuer deadline")
            .expect("issuer task");
        assert_eq!(
            requests.len(),
            2,
            "identity must be verified before retention"
        );
    }
    let other_pair = RsaKeyPair::generate(KeySize::Rsa2048).expect("different signing key");
    for (name, identity, signer, expected) in [
        (
            "missing-permission nonce mismatch",
            claims(now, "different-nonce"),
            &pair,
            identity::IdentityFailure::Nonce,
        ),
        (
            "missing-permission expired identity",
            claims(now - 7200, "synthetic-nonce"),
            &pair,
            identity::IdentityFailure::Lifetime,
        ),
        (
            "missing-permission signature mismatch",
            claims(now, "synthetic-nonce"),
            &other_pair,
            identity::IdentityFailure::Signature,
        ),
    ] {
        let (endpoints, task) = issuer(vec![
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: None,
                body: token_reply(
                    &signed_token(signer, &identity),
                    "openid offline_access resource.invoke",
                ),
                chunked: false,
            },
            Reply {
                status: "200 OK",
                content_type: "application/jwk-set+json",
                encoding: None,
                body: jwks(&pair),
                chunked: false,
            },
        ])
        .await;
        assert!(
            matches!(redeem_at(exchange(), endpoints).await,
            Err(AuthorizationError::IdentityRejected(failure)) if failure == expected),
            "{name}"
        );
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("issuer deadline")
            .expect("issuer task");
    }
    let mut wrong_type: Value = serde_json::from_slice(&token_reply(
        &id_token,
        "openid offline_access resource.invoke chatgpt.tokens.use.direct",
    ))
    .expect("test token reply");
    wrong_type["token_type"] = json!("Basic");
    let mut missing_refresh = wrong_type.clone();
    missing_refresh["token_type"] = json!("Bearer");
    missing_refresh
        .as_object_mut()
        .unwrap()
        .remove("refresh_token");
    let scenarios = [
        (
            "spent authorization code",
            Reply {
                status: "400 Bad Request",
                content_type: "application/json",
                encoding: None,
                body: br#"{"error":"invalid_grant"}"#.to_vec(),
                chunked: false,
            },
            AuthorizationError::GrantUnusable,
        ),
        (
            "unstructured spent-code text",
            Reply {
                status: "400 Bad Request",
                content_type: "application/json",
                encoding: None,
                body: br#"{"detail":"invalid_grant"}"#.to_vec(),
                chunked: false,
            },
            AuthorizationError::ExchangeFailed,
        ),
        (
            "server failure mentioning a spent code",
            Reply {
                status: "503 Unavailable",
                content_type: "application/json",
                encoding: None,
                body: br#"{"error":"invalid_grant"}"#.to_vec(),
                chunked: false,
            },
            AuthorizationError::Unavailable,
        ),
        (
            "encoded spent-code response",
            Reply {
                status: "400 Bad Request",
                content_type: "application/json",
                encoding: Some("gzip"),
                body: br#"{"error":"invalid_grant"}"#.to_vec(),
                chunked: false,
            },
            AuthorizationError::ExchangeFailed,
        ),
        (
            "wrong token type",
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: None,
                body: serde_json::to_vec(&wrong_type).expect("wrong type reply"),
                chunked: false,
            },
            AuthorizationError::ExchangeFailed,
        ),
        (
            "enabled permission without refresh token",
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: None,
                body: serde_json::to_vec(&missing_refresh).unwrap(),
                chunked: false,
            },
            AuthorizationError::ExchangeFailed,
        ),
        (
            "duplicate scopes without plan permission",
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: None,
                body: token_reply(&id_token, "openid openid"),
                chunked: false,
            },
            AuthorizationError::ExchangeFailed,
        ),
        (
            "wrong content type",
            Reply {
                status: "200 OK",
                content_type: "text/plain",
                encoding: None,
                body: token_reply(
                    &id_token,
                    "openid offline_access resource.invoke chatgpt.tokens.use.direct",
                ),
                chunked: false,
            },
            AuthorizationError::ExchangeFailed,
        ),
        (
            "redirect",
            Reply {
                status: "302 Found",
                content_type: "application/json",
                encoding: None,
                body: Vec::new(),
                chunked: false,
            },
            AuthorizationError::ExchangeFailed,
        ),
        (
            "encoded",
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: Some("gzip"),
                body: token_reply(
                    &id_token,
                    "openid offline_access resource.invoke chatgpt.tokens.use.direct",
                ),
                chunked: false,
            },
            AuthorizationError::ExchangeFailed,
        ),
        (
            "declared overflow",
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: None,
                body: vec![b' '; MAX_TOKEN_RESPONSE_BYTES + 1],
                chunked: false,
            },
            AuthorizationError::ExchangeFailed,
        ),
        (
            "chunked overflow",
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: None,
                body: vec![b' '; MAX_TOKEN_RESPONSE_BYTES + 1],
                chunked: true,
            },
            AuthorizationError::ExchangeFailed,
        ),
        (
            "malformed token JSON",
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: None,
                body: b"{".to_vec(),
                chunked: false,
            },
            AuthorizationError::ExchangeFailed,
        ),
    ];
    for (name, token_reply, expected) in scenarios {
        let (endpoints, task) = issuer(vec![token_reply]).await;
        let result = redeem_at(exchange(), endpoints).await;
        assert!(matches!(result, Err(error) if error == expected), "{name}");
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("issuer deadline")
            .expect("issuer task");
    }
}

#[tokio::test]
async fn redemption_rejects_untrusted_jwks_transport_before_identity_use() {
    let pair = RsaKeyPair::generate(KeySize::Rsa2048).expect("test RSA key");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("test clock")
        .as_secs();
    let id_token = signed_token(&pair, &claims(now, "synthetic-nonce"));
    let scope = "openid offline_access resource.invoke chatgpt.tokens.use.direct";
    let scenarios = [
        (
            "status",
            Reply {
                status: "503 Unavailable",
                content_type: "application/json",
                encoding: None,
                body: Vec::new(),
                chunked: false,
            },
        ),
        (
            "encoding",
            Reply {
                status: "200 OK",
                content_type: "application/jwk-set+json",
                encoding: Some("gzip"),
                body: jwks(&pair),
                chunked: false,
            },
        ),
        (
            "declared overflow",
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: None,
                body: vec![b' '; MAX_JWKS_BYTES + 1],
                chunked: false,
            },
        ),
        (
            "chunked overflow",
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: None,
                body: vec![b' '; MAX_JWKS_BYTES + 1],
                chunked: true,
            },
        ),
        (
            "malformed key set",
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: None,
                body: b"{".to_vec(),
                chunked: false,
            },
        ),
    ];
    for (name, key_reply) in scenarios {
        let expected = if name == "malformed key set" {
            identity::IdentityFailure::KeySet
        } else {
            identity::IdentityFailure::KeyTransport
        };
        let replies = vec![
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: None,
                body: token_reply(&id_token, scope),
                chunked: false,
            },
            key_reply,
        ];
        let (endpoints, task) = issuer(replies).await;
        let result = redeem_at(exchange(), endpoints).await;
        assert!(
            matches!(result, Err(AuthorizationError::IdentityRejected(failure)) if failure == expected),
            "{name}"
        );
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("issuer deadline")
            .expect("issuer task");
    }
}

fn saved_credentials() -> VerifiedCredentials {
    VerifiedCredentials {
        client_id: "oaiapp_test".into(),
        host_id: uuid::Uuid::now_v7(),
        subject: "synthetic-subject".into(),
        id_token: "synthetic-signed-id-hint".into(),
        access_token: "synthetic-old-access".into(),
        refresh_token: "synthetic-old-refresh".into(),
        access_expires_at_unix: 1,
    }
}

fn refresh_reply(access_token: &str, refresh_token: &str, scope: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "access_token": access_token,
        "refresh_token": refresh_token,
        "token_type": "Bearer",
        "expires_in": 3600,
        "scope": scope
    }))
    .expect("test refresh reply")
}

#[tokio::test]
async fn refresh_rotates_only_the_selected_registration_with_exact_form() {
    let original = saved_credentials();
    let scope = "openid offline_access resource.invoke chatgpt.tokens.use.direct";
    let (endpoints, task) = issuer(vec![Reply {
        status: "200 OK",
        content_type: "application/json",
        encoding: None,
        body: refresh_reply("synthetic-new-access", "synthetic-new-refresh", scope),
        chunked: false,
    }])
    .await;
    let replacement = refresh_at(&original, endpoints)
        .await
        .expect("rotated credentials");
    assert_eq!(replacement.client_id, original.client_id);
    assert_eq!(replacement.host_id, original.host_id);
    assert_eq!(replacement.subject, original.subject);
    assert_eq!(replacement.id_token, original.id_token);
    assert_eq!(replacement.access_token, "synthetic-new-access");
    assert_eq!(replacement.refresh_token, "synthetic-new-refresh");
    assert_ne!(
        replacement.access_expires_at_unix,
        original.access_expires_at_unix
    );
    assert_eq!(original.access_token, "synthetic-old-access");
    assert_eq!(original.refresh_token, "synthetic-old-refresh");
    let requests = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("issuer deadline")
        .expect("issuer task");
    assert_eq!(requests.len(), 1);
    let request = String::from_utf8(requests[0].clone()).expect("form request");
    assert!(request.starts_with("POST /api/accounts/oauth/token HTTP/1.1\r\n"));
    let form = request.split("\r\n\r\n").nth(1).expect("form body");
    let fields = url::form_urlencoded::parse(form.as_bytes())
        .into_owned()
        .collect::<Vec<_>>();
    assert_eq!(
        fields,
        [
            ("grant_type".into(), "refresh_token".into()),
            ("client_id".into(), "oaiapp_test".into()),
            ("refresh_token".into(), "synthetic-old-refresh".into()),
            ("resource".into(), "https://api.openai.com/v1".into()),
        ]
    );
    assert!(!request.contains("synthetic-old-access"));
    assert!(!request.contains("synthetic-signed-id-hint"));
}

#[test]
fn absolute_access_expiry_is_checked_from_request_start() {
    assert_eq!(access_expiry_at(1_000, 3_600), Ok(4_600));
    for lifetime in [0, 7_201] {
        assert_eq!(
            access_expiry_at(1_000, lifetime),
            Err(AuthorizationError::ExchangeFailed)
        );
    }
    assert_eq!(
        access_expiry_at(u64::MAX - 3_599, 3_600),
        Err(AuthorizationError::ExchangeFailed)
    );
}

#[tokio::test]
async fn refresh_rejects_invalid_rotation_without_changing_prior_credentials() {
    let original = saved_credentials();
    let scope = "openid offline_access resource.invoke chatgpt.tokens.use.direct";
    let mut wrong_type: Value = serde_json::from_slice(&refresh_reply(
        "synthetic-new-access",
        "synthetic-new-refresh",
        scope,
    ))
    .expect("test reply");
    wrong_type["token_type"] = json!("Basic");
    for (name, reply, expected) in [
        (
            "permission missing",
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: None,
                body: refresh_reply(
                    "synthetic-new-access",
                    "synthetic-new-refresh",
                    "openid offline_access resource.invoke",
                ),
                chunked: false,
            },
            AuthorizationError::PermissionMissing,
        ),
        (
            "wrong token type",
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: None,
                body: serde_json::to_vec(&wrong_type).expect("wrong type"),
                chunked: false,
            },
            AuthorizationError::ExchangeFailed,
        ),
        (
            "old refresh token reused",
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: None,
                body: refresh_reply("synthetic-new-access", "synthetic-old-refresh", scope),
                chunked: false,
            },
            AuthorizationError::ExchangeFailed,
        ),
        (
            "unusable refresh token",
            Reply {
                status: "400 Bad Request",
                content_type: "application/json",
                encoding: None,
                body: br#"{"error":"invalid_grant"}"#.to_vec(),
                chunked: false,
            },
            AuthorizationError::RefreshTokenUnusable,
        ),
        (
            "invalid client",
            Reply {
                status: "401 Unauthorized",
                content_type: "application/json",
                encoding: None,
                body: br#"{"error":{"code":"invalid_client"}}"#.to_vec(),
                chunked: false,
            },
            AuthorizationError::InvalidClient,
        ),
        (
            "temporary server failure cannot invalidate tokens",
            Reply {
                status: "503 Service Unavailable",
                content_type: "application/json",
                encoding: None,
                body: br#"{"error":"invalid_grant"}"#.to_vec(),
                chunked: false,
            },
            AuthorizationError::Unavailable,
        ),
        (
            "rate limit cannot invalidate tokens",
            Reply {
                status: "429 Too Many Requests",
                content_type: "application/json",
                encoding: None,
                body: br#"{"error":"invalid_grant"}"#.to_vec(),
                chunked: false,
            },
            AuthorizationError::Unavailable,
        ),
        (
            "unrecognized error preserves tokens",
            Reply {
                status: "400 Bad Request",
                content_type: "application/json",
                encoding: None,
                body: br#"{"error":"unknown"}"#.to_vec(),
                chunked: false,
            },
            AuthorizationError::ExchangeFailed,
        ),
        (
            "malformed error preserves tokens",
            Reply {
                status: "400 Bad Request",
                content_type: "application/json",
                encoding: None,
                body: br#"{"detail":"invalid_grant"}"#.to_vec(),
                chunked: false,
            },
            AuthorizationError::ExchangeFailed,
        ),
        (
            "redirect",
            Reply {
                status: "302 Found",
                content_type: "application/json",
                encoding: None,
                body: Vec::new(),
                chunked: false,
            },
            AuthorizationError::ExchangeFailed,
        ),
        (
            "encoded",
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: Some("gzip"),
                body: refresh_reply("synthetic-new-access", "synthetic-new-refresh", scope),
                chunked: false,
            },
            AuthorizationError::ExchangeFailed,
        ),
        (
            "declared overflow",
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: None,
                body: vec![b' '; MAX_TOKEN_RESPONSE_BYTES + 1],
                chunked: false,
            },
            AuthorizationError::ExchangeFailed,
        ),
        (
            "chunked overflow",
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: None,
                body: vec![b' '; MAX_TOKEN_RESPONSE_BYTES + 1],
                chunked: true,
            },
            AuthorizationError::ExchangeFailed,
        ),
    ] {
        let (endpoints, task) = issuer(vec![reply]).await;
        let result = refresh_at(&original, endpoints).await;
        assert!(matches!(result, Err(error) if error == expected), "{name}");
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("issuer deadline")
            .expect("issuer task");
        assert_eq!(original.access_token, "synthetic-old-access", "{name}");
        assert_eq!(original.refresh_token, "synthetic-old-refresh", "{name}");
    }

    for code in [
        "invalid_refresh_token",
        "token_expired",
        "refresh_token_expired",
        "refresh_token_invalidated",
        "refresh_token_reused",
    ] {
        let (endpoints, task) = issuer(vec![Reply {
            status: "400 Bad Request",
            content_type: "application/json",
            encoding: None,
            body: serde_json::to_vec(&json!({"error": code})).expect("error reply"),
            chunked: false,
        }])
        .await;
        assert!(matches!(
            refresh_at(&original, endpoints).await,
            Err(AuthorizationError::RefreshTokenUnusable)
        ));
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("issuer deadline")
            .expect("issuer task");
    }

    let mut invalid = saved_credentials();
    invalid.client_id = "dynamic_agent_client".into();
    assert!(matches!(
        refresh_at(&invalid, Endpoints::production()).await,
        Err(AuthorizationError::InvalidIdentity)
    ));
}

#[tokio::test]
async fn refresh_form_accepts_the_longest_admitted_escaped_token() {
    let mut original = saved_credentials();
    original.refresh_token = "%".repeat(8 * 1024);
    let (endpoints, task) = issuer(vec![Reply {
        status: "200 OK",
        content_type: "application/json",
        encoding: None,
        body: refresh_reply(
            "synthetic-new-access",
            "synthetic-new-refresh",
            "openid offline_access resource.invoke chatgpt.tokens.use.direct",
        ),
        chunked: false,
    }])
    .await;
    let replacement = refresh_at(&original, endpoints)
        .await
        .expect("long admitted refresh token");
    assert_eq!(replacement.refresh_token, "synthetic-new-refresh");
    let requests = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("issuer deadline")
        .expect("issuer task");
    let request = String::from_utf8(requests[0].clone()).expect("form request");
    let form = request.split("\r\n\r\n").nth(1).expect("form body");
    assert!(form.len() <= MAX_REFRESH_FORM_BYTES);
    let fields = url::form_urlencoded::parse(form.as_bytes())
        .into_owned()
        .collect::<Vec<_>>();
    assert_eq!(fields[2].0, "refresh_token");
    assert_eq!(fields[2].1.len(), 8 * 1024);
}
