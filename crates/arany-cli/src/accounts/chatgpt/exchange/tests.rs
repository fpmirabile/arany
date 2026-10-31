use super::test_support::{
    LONG_TOKEN_LIFETIME_SECONDS, Reply, claims, issuer, jwks, random_value, signed_token,
    signed_token_with_header,
};
use super::*;
use aws_lc_rs::{rsa::KeySize, signature::RsaKeyPair};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};

const NOW: u64 = 1_800_000_000;

#[test]
fn signed_identity_requires_exact_issuer_client_nonce_and_time() {
    let pair = RsaKeyPair::generate(KeySize::Rsa2048).expect("test RSA key");
    let nonce = random_value();
    let keys = jwks(&pair);
    let good = signed_token(&pair, &claims(NOW, &nonce));
    assert_eq!(
        identity::verify(&good, &keys, super::super::CLIENT_ID, &nonce, NOW),
        Ok(identity::VerifiedIdentity {
            subject: "synthetic-subject".into(),
            account_id: "account-one".into(),
        })
    );
    for account in [
        json!(null),
        json!({}),
        json!({"chatgpt_account_id": ""}),
        json!({"chatgpt_account_id": "x".repeat(arany::ChatGptProvider::MAX_ACCOUNT_ID_BYTES + 1)}),
        json!({"chatgpt_account_id": "account-\u{e9}"}),
        json!({"chatgpt_account_id": "account\n"}),
        json!({"chatgpt_account_id": 42}),
    ] {
        let mut rejected = claims(NOW, &nonce);
        rejected["https://api.openai.com/auth"] = account;
        assert!(
            identity::verify(
                &signed_token(&pair, &rejected),
                &keys,
                super::super::CLIENT_ID,
                &nonce,
                NOW,
            )
            .is_err()
        );
    }
    let mut singleton = claims(NOW, &nonce);
    singleton["aud"] = json!([super::super::CLIENT_ID]);
    assert_eq!(
        identity::verify(
            &signed_token(&pair, &singleton),
            &keys,
            super::super::CLIENT_ID,
            &nonce,
            NOW
        ),
        Ok(identity::VerifiedIdentity {
            subject: "synthetic-subject".into(),
            account_id: "account-one".into(),
        }),
        "a signed singleton audience is the same issued client"
    );
    for audience in [
        json!(super::super::CLIENT_ID),
        json!([super::super::CLIENT_ID]),
    ] {
        let mut bound_party = claims(NOW, &nonce);
        bound_party["aud"] = audience;
        bound_party["azp"] = json!(super::super::CLIENT_ID);
        assert_eq!(
            identity::verify(
                &signed_token(&pair, &bound_party),
                &keys,
                super::super::CLIENT_ID,
                &nonce,
                NOW
            ),
            Ok(identity::VerifiedIdentity {
                subject: "synthetic-subject".into(),
                account_id: "account-one".into(),
            }),
            "present authorized party is the issued client"
        );
        bound_party["azp"] = json!("another-client");
        assert_eq!(
            identity::verify(
                &signed_token(&pair, &bound_party),
                &keys,
                super::super::CLIENT_ID,
                &nonce,
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
            json!([super::super::CLIENT_ID, "another-client"]),
            identity::IdentityFailure::ClaimsFormat,
        ),
        (
            "aud",
            json!(["another-client", super::super::CLIENT_ID]),
            identity::IdentityFailure::ClaimsFormat,
        ),
        (
            "aud",
            json!([super::super::CLIENT_ID, super::super::CLIENT_ID]),
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
            json!([[super::super::CLIENT_ID]]),
            identity::IdentityFailure::ClaimsFormat,
        ),
        ("aud", json!(null), identity::IdentityFailure::ClaimsFormat),
        ("aud", json!(42), identity::IdentityFailure::ClaimsFormat),
        ("aud", json!(true), identity::IdentityFailure::ClaimsFormat),
        (
            "aud",
            json!({"client": super::super::CLIENT_ID}),
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
            json!([super::super::CLIENT_ID]),
            identity::IdentityFailure::ClaimsFormat,
        ),
        ("azp", json!(true), identity::IdentityFailure::ClaimsFormat),
    ] {
        let mut rejected = claims(NOW, &nonce);
        rejected[field] = replacement;
        assert_eq!(
            identity::verify(
                &signed_token(&pair, &rejected),
                &keys,
                super::super::CLIENT_ID,
                &nonce,
                NOW
            ),
            Err(AuthorizationError::IdentityRejected(failure)),
            "rejected {field} fixture"
        );
    }
    let mismatched_nonce = random_value();
    assert_ne!(nonce, mismatched_nonce);
    let mut altered = claims(NOW, &nonce);
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
            json!(&mismatched_nonce),
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
            identity::verify(&token, &keys, super::super::CLIENT_ID, &nonce, NOW),
            Err(AuthorizationError::IdentityRejected(failure)),
            "{field}"
        );
        if field == "nonce" {
            let error = identity::verify(&token, &keys, super::super::CLIENT_ID, &nonce, NOW)
                .expect_err("mismatched nonce");
            assert!(error.to_string().contains("nonce mismatch"), "{error}");
            assert!(!error.to_string().contains(&mismatched_nonce));
        }
        altered[field] = original;
    }
    let (signed, signature) = good.rsplit_once('.').expect("JWT signature");
    let mut signature = URL_SAFE_NO_PAD.decode(signature).expect("signature bytes");
    signature[0] ^= 1;
    let tampered = format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature));
    assert_eq!(
        identity::verify(&tampered, &keys, super::super::CLIENT_ID, &nonce, NOW),
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
        let token = signed_token_with_header(&pair, &claims(NOW, &nonce), &header);
        assert_eq!(
            identity::verify(&token, &keys, super::super::CLIENT_ID, &nonce, NOW),
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
        identity::verify(&good, &duplicate, super::super::CLIENT_ID, &nonce, NOW),
        Err(AuthorizationError::IdentityRejected(
            identity::IdentityFailure::KeyId
        ))
    );
    let wrong_key = RsaKeyPair::generate(KeySize::Rsa2048).expect("other test RSA key");
    assert_eq!(
        identity::verify(
            &good,
            &jwks(&wrong_key),
            super::super::CLIENT_ID,
            &nonce,
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
        identity::verify(&good, &too_many, super::super::CLIENT_ID, &nonce, NOW),
        Err(AuthorizationError::IdentityRejected(
            identity::IdentityFailure::KeySet
        ))
    );
}

fn exchange(nonce: &str) -> CodeExchange {
    CodeExchange {
        code: "synthetic-code".into(),
        client_id: super::super::CLIENT_ID.into(),
        redirect_uri: "http://127.0.0.1:1455/auth/callback".into(),
        verifier: random_value(),
        nonce: nonce.into(),
        host_id: uuid::Uuid::parse_str("123e4567-e89b-42d3-a456-426614174000").unwrap(),
        expected_subject: None,
        expected_account_id: None,
    }
}

fn token_reply(id_token: &str, scope: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "access_token": "synthetic-access",
        "refresh_token": "synthetic-refresh",
        "id_token": id_token,
        "token_type": "Bearer",
        "expires_in": LONG_TOKEN_LIFETIME_SECONDS,
        "scope": scope
    }))
    .expect("test token reply")
}

#[tokio::test]
async fn redemption_sends_exact_grant_and_returns_only_verified_credentials() {
    let pair = RsaKeyPair::generate(KeySize::Rsa2048).expect("test RSA key");
    let nonce = random_value();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("test clock")
        .as_secs();
    let mut identity = claims(now, &nonce);
    identity["aud"] = serde_json::json!([super::super::CLIENT_ID]);
    identity["azp"] = serde_json::json!(super::super::CLIENT_ID);
    let id_token = signed_token(&pair, &identity);
    let replies = vec![
        Reply {
            status: "200 OK",
            content_type: "application/json",
            encoding: None,
            body: token_reply(&id_token, "openid profile email offline_access"),
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
    let attempt = exchange(&nonce);
    let host_id = attempt.host_id;
    let verifier = attempt.verifier.clone();
    let verified = redeem_at(attempt, endpoints).await.expect("verified grant");
    assert_eq!(verified.client_id, super::super::CLIENT_ID);
    assert_eq!(verified.host_id, host_id);
    assert_eq!(verified.subject, "synthetic-subject");
    assert_eq!(verified.id_token, id_token);
    assert_eq!(verified.access_token, "synthetic-access");
    assert_eq!(verified.refresh_token, "synthetic-refresh");
    assert!(verified.access_expires_at_unix >= now + LONG_TOKEN_LIFETIME_SECONDS);
    let requests = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("issuer deadline")
        .expect("issuer task");
    assert_eq!(requests.len(), 2);
    let token_request = String::from_utf8(requests[0].clone()).expect("form request");
    assert!(token_request.starts_with("POST /oauth/token HTTP/1.1\r\n"));
    assert!(token_request.contains("content-type: application/x-www-form-urlencoded\r\n"));
    let form = token_request.split("\r\n\r\n").nth(1).expect("form body");
    let fields = url::form_urlencoded::parse(form.as_bytes())
        .into_owned()
        .collect::<Vec<_>>();
    assert_eq!(
        fields,
        [
            ("grant_type".into(), "authorization_code".into()),
            ("client_id".into(), super::super::CLIENT_ID.into()),
            ("code".into(), "synthetic-code".into()),
            ("code_verifier".into(), verifier),
            (
                "redirect_uri".into(),
                "http://127.0.0.1:1455/auth/callback".into()
            ),
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
    let nonce = random_value();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("test clock")
        .as_secs();
    let id_token = signed_token(&pair, &claims(now, &nonce));
    for scope in ["openid profile email offline_access", "openid"] {
        for (expected_subject, expected_account, failure) in [
            ("synthetic-subject", "account-one", None),
            (
                "different-saved-subject",
                "account-one",
                Some(identity::IdentityFailure::SelectedSubject),
            ),
            (
                "synthetic-subject",
                "account-two",
                Some(identity::IdentityFailure::AccountClaim),
            ),
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
            let mut selected = exchange(&nonce);
            selected.expected_subject = Some(expected_subject.into());
            selected.expected_account_id = Some(expected_account.into());
            let result = redeem_at(selected, endpoints).await;
            if failure.is_none() {
                let subject = result.expect("same signed account").subject;
                assert_eq!(subject, expected_subject);
            } else {
                assert!(matches!(
                    result,
                    Err(AuthorizationError::IdentityRejected(cause)) if Some(cause) == failure
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
async fn redemption_rejects_invalid_identity_and_failed_responses() {
    let pair = RsaKeyPair::generate(KeySize::Rsa2048).expect("test RSA key");
    let nonce = random_value();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("test clock")
        .as_secs();
    let id_token = signed_token(&pair, &claims(now, &nonce));
    let other_pair = RsaKeyPair::generate(KeySize::Rsa2048).expect("different signing key");
    let mismatched_nonce = random_value();
    assert_ne!(nonce, mismatched_nonce);
    for (name, identity, signer, expected) in [
        (
            "nonce mismatch",
            claims(now, &mismatched_nonce),
            &pair,
            identity::IdentityFailure::Nonce,
        ),
        (
            "expired identity",
            claims(now - 7200, &nonce),
            &pair,
            identity::IdentityFailure::Lifetime,
        ),
        (
            "signature mismatch",
            claims(now, &nonce),
            &other_pair,
            identity::IdentityFailure::Signature,
        ),
    ] {
        let (endpoints, task) = issuer(vec![
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: None,
                body: token_reply(&signed_token(signer, &identity), "openid"),
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
            matches!(redeem_at(exchange(&nonce), endpoints).await,
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
        "openid profile email offline_access",
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
            AuthorizationError::ExchangeHttp { status: 400 },
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
            AuthorizationError::ExchangeHttp { status: 400 },
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
            AuthorizationError::ExchangeReplyInvalid,
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
            AuthorizationError::ExchangeReplyInvalid,
        ),
        (
            "oversized scope",
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: None,
                body: token_reply(&id_token, &"x".repeat(super::super::MAX_SCOPE_BYTES + 1)),
                chunked: false,
            },
            AuthorizationError::ExchangeReplyInvalid,
        ),
        (
            "wrong content type",
            Reply {
                status: "200 OK",
                content_type: "text/plain",
                encoding: None,
                body: token_reply(&id_token, "openid profile email offline_access"),
                chunked: false,
            },
            AuthorizationError::ExchangeReplyInvalid,
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
            AuthorizationError::ExchangeHttp { status: 302 },
        ),
        (
            "encoded",
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: Some("gzip"),
                body: token_reply(&id_token, "openid profile email offline_access"),
                chunked: false,
            },
            AuthorizationError::ExchangeReplyInvalid,
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
            AuthorizationError::ExchangeReplyInvalid,
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
            AuthorizationError::ExchangeReplyInvalid,
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
            AuthorizationError::ExchangeReplyInvalid,
        ),
    ];
    let missing_fields = ["expires_in", "id_token", "null expires_in"].map(|field| {
        let mut reply: Value = serde_json::from_slice(&token_reply(&id_token, "openid")).unwrap();
        if field == "null expires_in" {
            reply["expires_in"] = Value::Null;
        } else {
            reply.as_object_mut().unwrap().remove(field);
        }
        (
            field,
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: None,
                body: serde_json::to_vec(&reply).unwrap(),
                chunked: false,
            },
            if field == "id_token" {
                AuthorizationError::ExchangeReplyInvalid
            } else {
                AuthorizationError::ExchangeLifetimeMissing
            },
        )
    });
    let invalid_lifetimes = [0, u64::MAX].map(|seconds| {
        let mut reply: Value = serde_json::from_slice(&token_reply(&id_token, "openid")).unwrap();
        reply["expires_in"] = json!(seconds);
        (
            "invalid token lifetime",
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: None,
                body: serde_json::to_vec(&reply).unwrap(),
                chunked: false,
            },
            AuthorizationError::ExchangeLifetimeInvalid,
        )
    });
    for (name, token_reply, expected) in scenarios
        .into_iter()
        .chain(missing_fields)
        .chain(invalid_lifetimes)
    {
        let (endpoints, task) = issuer(vec![token_reply]).await;
        let result = redeem_at(exchange(&nonce), endpoints).await;
        if name == "expires_in" {
            assert!(
                matches!(&result, Err(error) if error.to_string().contains("expires_in")),
                "missing token lifetime must be distinguishable from an HTTP rejection"
            );
        }
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
    let nonce = random_value();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("test clock")
        .as_secs();
    let id_token = signed_token(&pair, &claims(now, &nonce));
    let scope = "openid profile email offline_access";
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
        let result = redeem_at(exchange(&nonce), endpoints).await;
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

#[test]
fn absolute_access_expiry_is_checked_from_request_start() {
    assert_eq!(access_expiry_at(1_000, 3_600), Ok(4_600));
    assert_eq!(
        access_expiry_at(1_000, LONG_TOKEN_LIFETIME_SECONDS),
        Ok(1_000 + LONG_TOKEN_LIFETIME_SECONDS)
    );
    assert_eq!(access_expiry_at(1_000, u64::MAX - 1_000), Ok(u64::MAX));
    assert_eq!(
        access_expiry_at(1_000, 0),
        Err(AuthorizationError::ExchangeFailed)
    );
    assert_eq!(
        access_expiry_at(u64::MAX - 3_599, 3_600),
        Err(AuthorizationError::ExchangeFailed)
    );
}
