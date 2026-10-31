use super::super::test_support::{
    LONG_TOKEN_LIFETIME_SECONDS, Reply, claims, issuer, jwks, random_value, read_issuer_request,
    signed_token,
};
use super::*;
use crate::accounts::chatgpt::{CLIENT_ID, identity};
use aws_lc_rs::{rsa::KeySize, signature::RsaKeyPair};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{io::AsyncWriteExt, net::TcpListener};

#[tokio::test]
async fn renewal_binds_signed_identity_and_optional_nonce_to_the_selected_account() {
    let pair = RsaKeyPair::generate(KeySize::Rsa2048).expect("renewal signing key");
    let nonce = random_value();
    let original_claims = claims(unix_now().unwrap(), &nonce);
    let mut original = saved_credentials();
    original.id_token = signed_token(&pair, &original_claims);
    for (field, value, expected) in renewal_cases(&nonce) {
        let mut identity = original_claims.clone();
        if let Some(value) = value {
            identity[field] = value;
        } else {
            identity.as_object_mut().unwrap().remove(field);
        }
        let mut reply: Value = serde_json::from_slice(&refresh_reply(
            "synthetic-new-access",
            &original.refresh_token,
            "openid",
        ))
        .unwrap();
        reply["id_token"] = json!(signed_token(&pair, &identity));
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
        let result = refresh_at(&original, endpoints).await;
        match expected {
            Some(expected) => assert!(
                matches!(result,
                Err(AuthorizationError::IdentityRejected(actual)) if actual == expected),
                "{field}"
            ),
            None => {
                let renewed = result.expect("same signed account");
                assert_eq!(renewed.refresh_token, original.refresh_token);
                assert_eq!(renewed.subject, original.subject);
                assert_eq!(renewed.chatgpt_account_id, original.chatgpt_account_id);
            }
        }
        let requests = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("issuer deadline")
            .expect("issuer task");
        assert_eq!(requests.len(), 2);
        assert!(requests[1].starts_with(b"GET /.well-known/jwks.json HTTP/1.1\r\n"));
        for secret in [
            &original.access_token,
            &original.refresh_token,
            &original.id_token,
        ] {
            assert!(
                !requests[1]
                    .windows(secret.len())
                    .any(|part| part == secret.as_bytes())
            );
        }
    }
}

fn renewal_cases(
    nonce: &str,
) -> Vec<(
    &'static str,
    Option<Value>,
    Option<identity::IdentityFailure>,
)> {
    vec![
        ("nonce", None, None),
        ("nonce", Some(json!(nonce)), None),
        (
            "nonce",
            Some(json!("different-nonce")),
            Some(identity::IdentityFailure::Nonce),
        ),
        (
            "nonce",
            Some(Value::Null),
            Some(identity::IdentityFailure::ClaimsFormat),
        ),
        (
            "sub",
            Some(json!("another-subject")),
            Some(identity::IdentityFailure::SelectedSubject),
        ),
        (
            "https://api.openai.com/auth",
            Some(json!({"chatgpt_account_id": "another-account"})),
            Some(identity::IdentityFailure::AccountClaim),
        ),
    ]
}

#[tokio::test]
async fn chained_renewal_keeps_the_original_nonce_when_a_reply_omits_it() {
    let pair = RsaKeyPair::generate(KeySize::Rsa2048).unwrap();
    let original_claims = claims(unix_now().unwrap(), &random_value());
    let original_id = signed_token(&pair, &original_claims);
    let mut current = saved_credentials();
    current.id_token = original_id.clone();
    let mut without_nonce = original_claims.clone();
    without_nonce.as_object_mut().unwrap().remove("nonce");
    for (index, identity) in [without_nonce, original_claims].into_iter().enumerate() {
        let mut body: Value = serde_json::from_slice(&refresh_reply(
            &format!("synthetic-renewal-{index}"),
            &current.refresh_token,
            "openid",
        ))
        .unwrap();
        body["id_token"] = json!(signed_token(&pair, &identity));
        let (endpoints, task) = issuer(vec![
            Reply {
                status: "200 OK",
                content_type: "application/json",
                encoding: None,
                body: serde_json::to_vec(&body).unwrap(),
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
        current = refresh_at(&current, endpoints)
            .await
            .expect("chained renewal");
        let requests = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requests.len(), 2);
    }
    assert_eq!(current.id_token, original_id);
}

#[tokio::test]
async fn consumed_refresh_with_a_lost_reply_or_unavailable_keys_reports_unknown() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let endpoints = Endpoints {
        token: Url::parse(&format!("{origin}/oauth/token")).unwrap(),
        jwks: Url::parse(&format!("{origin}/.well-known/jwks.json")).unwrap(),
        https_only: false,
    };
    let peer = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_issuer_request(&mut socket).await;
        assert!(request.starts_with(b"POST /oauth/token HTTP/1.1\r\n"));
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 64\r\nConnection: close\r\n\r\n{").await.unwrap();
    });
    let result = refresh_at(&saved_credentials(), endpoints).await;
    assert!(
        matches!(result, Err(AuthorizationError::RefreshOutcomeUnknown)),
        "a consumed refresh with a lost reply must remain unknown"
    );
    tokio::time::timeout(Duration::from_secs(5), peer)
        .await
        .unwrap()
        .unwrap();

    let (mut replies, _) = renewal_replies("synthetic-new-refresh");
    replies.truncate(1);
    let (endpoints, peer) = issuer(replies).await;
    let result = refresh_at(&saved_credentials(), endpoints).await;
    assert!(
        matches!(result, Err(AuthorizationError::RefreshOutcomeUnknown)),
        "keys unavailable after renewal must remain unknown"
    );
    let requests = tokio::time::timeout(Duration::from_secs(5), peer)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 1);
}

fn saved_credentials() -> VerifiedCredentials {
    VerifiedCredentials {
        client_id: CLIENT_ID.into(),
        host_id: uuid::Uuid::now_v7(),
        subject: "synthetic-subject".into(),
        chatgpt_account_id: Some("account-one".into()),
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
        "expires_in": LONG_TOKEN_LIFETIME_SECONDS,
        "scope": scope
    }))
    .expect("test refresh reply")
}

fn renewal_replies(refresh_token: &str) -> (Vec<Reply>, String) {
    let pair = RsaKeyPair::generate(KeySize::Rsa2048).expect("renewal signing key");
    let mut identity = claims(unix_now().unwrap(), &random_value());
    identity.as_object_mut().unwrap().remove("nonce");
    let id_token = signed_token(&pair, &identity);
    let mut reply: Value = serde_json::from_slice(&refresh_reply(
        "synthetic-new-access",
        refresh_token,
        "openid profile email offline_access",
    ))
    .unwrap();
    reply["id_token"] = json!(id_token);
    (
        vec![
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
        ],
        id_token,
    )
}

#[tokio::test]
async fn refresh_rotates_only_the_selected_registration_with_exact_form() {
    for signed in [true, false] {
        let original = saved_credentials();
        let (mut replies, id_token) = renewal_replies("synthetic-new-refresh");
        if !signed {
            replies.truncate(1);
            let mut body: Value = serde_json::from_slice(&replies[0].body).unwrap();
            body.as_object_mut().unwrap().remove("id_token");
            replies[0].body = serde_json::to_vec(&body).unwrap();
        }
        let (endpoints, task) = issuer(replies).await;
        let replacement = refresh_at(&original, endpoints)
            .await
            .expect("rotated credentials");
        assert_eq!(replacement.client_id, original.client_id);
        assert_eq!(replacement.host_id, original.host_id);
        assert_eq!(replacement.subject, original.subject);
        assert_eq!(replacement.id_token, original.id_token);
        assert_ne!(replacement.id_token, id_token);
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
        assert_eq!(requests.len(), 1 + usize::from(signed));
        let request = String::from_utf8(requests[0].clone()).expect("form request");
        assert!(request.starts_with("POST /oauth/token HTTP/1.1\r\n"));
        let form = request.split("\r\n\r\n").nth(1).expect("form body");
        let fields = url::form_urlencoded::parse(form.as_bytes())
            .into_owned()
            .collect::<Vec<_>>();
        assert_eq!(
            fields,
            [
                ("grant_type".into(), "refresh_token".into()),
                ("client_id".into(), CLIENT_ID.into()),
                ("refresh_token".into(), "synthetic-old-refresh".into()),
            ]
        );
        assert!(!request.contains("synthetic-old-access"));
        assert!(!request.contains("synthetic-signed-id-hint"));
    }
}

#[tokio::test]
async fn refresh_rejects_invalid_rotation_without_changing_prior_credentials() {
    for (field, value) in [
        ("expires_in", json!(0)),
        ("token_type", json!("Basic")),
        ("id_token", Value::Null),
    ] {
        let mut body: Value = serde_json::from_slice(&refresh_reply(
            "synthetic-new-access",
            "synthetic-new-refresh",
            "openid",
        ))
        .unwrap();
        body[field] = value;
        assert_refresh_rejected(
            field,
            json_reply("200 OK", serde_json::to_vec(&body).unwrap()),
            AuthorizationError::ExchangeFailed,
        )
        .await;
    }
    for (status, body, expected) in refresh_error_cases() {
        assert_refresh_rejected(status, json_reply(status, body.to_vec()), expected).await;
    }
    for code in [
        "refresh_token_expired",
        "refresh_token_invalidated",
        "refresh_token_reused",
    ] {
        assert_refresh_rejected(
            code,
            json_reply(
                "400 Bad Request",
                serde_json::to_vec(&json!({"error": code})).unwrap(),
            ),
            AuthorizationError::RefreshTokenUnusable,
        )
        .await;
    }
    for reply in rejected_transport_replies() {
        assert_refresh_rejected("transport", reply, AuthorizationError::ExchangeFailed).await;
    }
    let mut invalid = saved_credentials();
    invalid.client_id = "dynamic_agent_client".into();
    assert!(matches!(
        refresh_at(&invalid, Endpoints::production()).await,
        Err(AuthorizationError::InvalidIdentity)
    ));
}

fn json_reply(status: &'static str, body: Vec<u8>) -> Reply {
    Reply {
        status,
        body,
        content_type: "application/json",
        encoding: None,
        chunked: false,
    }
}

fn rejected_transport_replies() -> [Reply; 4] {
    let mut encoded = json_reply(
        "200 OK",
        refresh_reply("synthetic-new-access", "synthetic-new-refresh", "openid"),
    );
    encoded.encoding = Some("gzip");
    let declared = json_reply("200 OK", vec![b' '; MAX_TOKEN_RESPONSE_BYTES + 1]);
    let mut chunked = json_reply("200 OK", vec![b' '; MAX_TOKEN_RESPONSE_BYTES + 1]);
    chunked.chunked = true;
    [
        json_reply("302 Found", Vec::new()),
        encoded,
        declared,
        chunked,
    ]
}

fn refresh_error_cases() -> Vec<(&'static str, &'static [u8], AuthorizationError)> {
    vec![
        (
            "400 Bad Request",
            br#"{"error":"invalid_grant"}"#,
            AuthorizationError::RefreshTokenUnusable,
        ),
        (
            "401 Unauthorized",
            br#"{"error":{"code":"invalid_client"}}"#,
            AuthorizationError::InvalidClient,
        ),
        (
            "503 Service Unavailable",
            br#"{"error":"invalid_grant"}"#,
            AuthorizationError::RefreshOutcomeUnknown,
        ),
        (
            "429 Too Many Requests",
            br#"{"error":"invalid_grant"}"#,
            AuthorizationError::Unavailable,
        ),
        (
            "401 Unauthorized",
            b"",
            AuthorizationError::RefreshTokenUnusable,
        ),
        (
            "403 Forbidden",
            br#"{"error":"invalid_grant"}"#,
            AuthorizationError::ExchangeFailed,
        ),
        (
            "400 Bad Request",
            br#"{"error":"invalid_refresh_token"}"#,
            AuthorizationError::ExchangeFailed,
        ),
        (
            "400 Bad Request",
            br#"{"error":"token_expired"}"#,
            AuthorizationError::ExchangeFailed,
        ),
        (
            "400 Bad Request",
            br#"{"error":"unknown"}"#,
            AuthorizationError::ExchangeFailed,
        ),
        (
            "400 Bad Request",
            br#"{"detail":"invalid_grant"}"#,
            AuthorizationError::ExchangeFailed,
        ),
    ]
}

async fn assert_refresh_rejected(name: &str, reply: Reply, expected: AuthorizationError) {
    let original = saved_credentials();
    let (endpoints, task) = issuer(vec![reply]).await;
    assert!(
        matches!(refresh_at(&original, endpoints).await, Err(error) if error == expected),
        "{name}"
    );
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("issuer deadline")
        .expect("issuer task");
    assert_eq!(original.access_token, "synthetic-old-access", "{name}");
    assert_eq!(original.refresh_token, "synthetic-old-refresh", "{name}");
}

#[tokio::test]
async fn closed_token_port_is_unavailable_before_dispatch() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let token = Url::parse(&format!(
        "http://{}/oauth/token",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    drop(listener);
    let result = refresh_at(
        &saved_credentials(),
        Endpoints {
            token,
            jwks: Url::parse("http://127.0.0.1/.well-known/jwks.json").unwrap(),
            https_only: false,
        },
    )
    .await;
    assert!(matches!(result, Err(AuthorizationError::Unavailable)));
}

#[tokio::test]
async fn refresh_form_accepts_the_longest_admitted_escaped_token() {
    let mut original = saved_credentials();
    original.refresh_token = "%".repeat(crate::accounts::chatgpt::MAX_REFRESH_TOKEN_BYTES);
    let (replies, _) = renewal_replies("synthetic-new-refresh");
    let (endpoints, task) = issuer(replies).await;
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
    assert_eq!(
        fields[2].1.len(),
        crate::accounts::chatgpt::MAX_REFRESH_TOKEN_BYTES
    );
}
