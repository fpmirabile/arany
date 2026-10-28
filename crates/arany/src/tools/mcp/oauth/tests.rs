use super::*;

fn grant() -> Grant {
    Grant {
        url: "https://mcp.example.com/mcp".into(),
        client_id: "client".into(),
        token_endpoint: "https://auth.example.com/token".into(),
        revocation_endpoint: None,
        access_token: "access".into(),
        refresh_token: Some("refresh".into()),
        expires_at: None,
        renewal: Renewal::Ready,
        claim: None,
    }
}

fn record(grant: Grant) -> Vec<u8> {
    serde_json::to_vec(&Tokens {
        schema: 1,
        servers: BTreeMap::from([("remote".to_owned(), grant)]),
    })
    .unwrap()
}

#[test]
fn pkce_challenge_matches_the_rfc_7636_vector() {
    assert_eq!(
        challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

#[test]
fn callback_requires_the_exact_path_state_and_no_error() {
    let head = |target: &str| format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1:4100\r\n\r\n");
    assert_eq!(
        parse_callback(head("/callback?code=abc&state=s1").as_bytes(), "s1", 4100),
        Ok("abc".to_owned())
    );
    assert_eq!(
        parse_callback(
            b"GET /callback?code=abc&state=s1 HTTP/1.1\r\nHost: evil.example\r\n\r\n",
            "s1",
            4100
        ),
        Err(McpAuthError::Rejected)
    );
    for rejected in [
        "/callback?code=abc&state=other",
        "/callback?code=abc",
        "/callback?state=s1",
        "/callback?error=access_denied&state=s1&code=abc",
        "/elsewhere?code=abc&state=s1",
    ] {
        assert_eq!(
            parse_callback(head(rejected).as_bytes(), "s1", 4100),
            Err(McpAuthError::Rejected),
            "{rejected}"
        );
    }
    assert_eq!(
        parse_callback(
            b"POST /callback?code=abc&state=s1 HTTP/1.1\r\nHost: 127.0.0.1:4100\r\n\r\n",
            "s1",
            4100
        ),
        Err(McpAuthError::Rejected)
    );
}

#[test]
fn token_record_round_trips_and_rejects_unsafe_content() {
    assert!(Tokens::decode(&record(grant())).is_ok());
    assert!((MCP_OAUTH.accepts)(&record(grant())));
    for unsafe_grant in [
        Grant {
            token_endpoint: "http://auth.example.com/token".into(),
            ..grant()
        },
        Grant {
            access_token: "has space".into(),
            ..grant()
        },
        Grant {
            access_token: "a".repeat(MAX_ACCESS_TOKEN_BYTES + 1),
            ..grant()
        },
        Grant {
            url: "https://user:pass@mcp.example.com/mcp".into(),
            ..grant()
        },
    ] {
        assert!(Tokens::decode(&record(unsafe_grant)).is_err());
    }
    let mut value: Value = serde_json::from_slice(&record(grant())).unwrap();
    value["servers"]["remote"]["unknown"] = true.into();
    assert!(Tokens::decode(&serde_json::to_vec(&value).unwrap()).is_err());
    let mut crowded = Tokens {
        schema: 1,
        servers: BTreeMap::new(),
    };
    for index in 0..=MAX_MCP_SERVERS {
        crowded.servers.insert(format!("server{index}"), grant());
    }
    assert!(Tokens::decode(&serde_json::to_vec(&crowded).unwrap()).is_err());
}

#[test]
fn expiry_is_renewed_before_the_skew_window_closes() {
    let at = |expires_at| Grant {
        expires_at,
        ..grant()
    };
    assert!(at(None).fresh());
    assert!(at(Some(now() + 3600)).fresh());
    assert!(!at(Some(now() + EXPIRY_SKEW_SECS - 1)).fresh());
    assert!(!at(Some(now().saturating_sub(1))).fresh());
}

#[test]
fn token_reply_must_be_a_bounded_bearer_grant() {
    let reply = json!({"access_token":"new","token_type":"bearer","expires_in":u64::MAX});
    let renewed = grant_from(&reply, Some("kept".into())).unwrap();
    assert_eq!(renewed.refresh_token.as_deref(), Some("kept"));
    assert!(renewed.expires_at.unwrap() <= now() + MAX_EXPIRES_IN_SECS);
    let rotated = json!({"access_token":"new","token_type":"Bearer","refresh_token":"next"});
    assert_eq!(
        grant_from(&rotated, Some("kept".into()))
            .unwrap()
            .refresh_token
            .as_deref(),
        Some("next")
    );
    assert_eq!(
        grant_from(&json!({"access_token":"new","token_type":"mac"}), None).err(),
        Some(McpAuthError::Rejected)
    );
}

fn account() -> (tempfile::TempDir, StateRoot, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = StateRoot::admit(&temp.path().join("account")).unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    (temp, root, workspace)
}

#[test]
fn a_contended_account_lock_refuses_without_corrupting_the_record() {
    let (_temp, root, workspace) = account();
    mutate(&root, &workspace, |tokens| {
        tokens.servers.insert("first".into(), grant());
        Ok(())
    })
    .unwrap();
    let held = root
        .with_account_replacement_lock(&workspace, || {
            std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        mutate(&root, &workspace, |tokens| {
                            tokens.servers.insert("second".into(), grant());
                            Ok(())
                        })
                    })
                    .join()
                    .unwrap()
            })
        })
        .unwrap();
    assert_eq!(held, Err(McpAuthError::Storage), "the lock is not waited for");
    let after_refusal = load(&root).unwrap();
    assert_eq!(after_refusal.servers.keys().collect::<Vec<_>>(), ["first"]);
    mutate(&root, &workspace, |tokens| {
        tokens.servers.insert("second".into(), grant());
        Ok(())
    })
    .unwrap();
    assert_eq!(load(&root).unwrap().servers.len(), 2);
}

#[test]
fn an_unconfirmed_renewal_blocks_reuse_of_the_refresh_token() {
    let (_temp, root, workspace) = account();
    mutate(&root, &workspace, |tokens| {
        tokens.servers.insert("remote".into(), grant());
        Ok(())
    })
    .unwrap();
    let seen = grant();
    let claimed = mutate(&root, &workspace, |tokens| {
        claim_renewal(tokens, "remote", &seen)
    })
    .unwrap();
    assert_eq!(claimed.renewal, Renewal::Renewing);
    assert_eq!(
        mutate(&root, &workspace, |tokens| claim_renewal(
            tokens, "remote", &seen
        ))
        .err(),
        Some(McpAuthError::Rejected),
        "a second claim would resubmit the old refresh token"
    );
    assert_eq!(
        load(&root).unwrap().servers["remote"].renewal,
        Renewal::Renewing,
        "a lost response or cancellation leaves the grant blocked"
    );
}

#[test]
fn renewal_outcomes_never_overwrite_a_logout_or_new_login() {
    let (_temp, root, workspace) = account();
    mutate(&root, &workspace, |tokens| {
        tokens.servers.insert("remote".into(), grant());
        Ok(())
    })
    .unwrap();
    let claimed = mutate(&root, &workspace, |tokens| {
        claim_renewal(tokens, "remote", &grant())
    })
    .unwrap();
    let renewed = Grant {
        access_token: "next".into(),
        ..grant()
    };
    mutate(&root, &workspace, |tokens| {
        Ok(tokens.servers.remove("remote"))
    })
    .unwrap();
    assert_eq!(
        mutate(&root, &workspace, |tokens| settle_renewal(
            tokens,
            "remote",
            &claimed,
            Renewal::Ready,
            Some(renewed.clone())
        )),
        Err(McpAuthError::Rejected)
    );
    assert!(load(&root).unwrap().servers.is_empty());
    mutate(&root, &workspace, |tokens| {
        tokens.servers.insert("remote".into(), renewed.clone());
        Ok(())
    })
    .unwrap();
    assert!(
        mutate(&root, &workspace, |tokens| settle_renewal(
            tokens,
            "remote",
            &claimed,
            Renewal::Rejected,
            None
        ))
        .is_err()
    );
    assert_eq!(load(&root).unwrap().servers["remote"].renewal, Renewal::Ready);
}

#[test]
fn a_stale_claim_cannot_settle_after_a_replacement_login_reuses_the_token() {
    let (_temp, root, workspace) = account();
    mutate(&root, &workspace, |tokens| {
        tokens.servers.insert("remote".into(), grant());
        Ok(())
    })
    .unwrap();
    let stale = mutate(&root, &workspace, |tokens| {
        claim_renewal(tokens, "remote", &grant())
    })
    .unwrap();
    mutate(&root, &workspace, |tokens| {
        tokens.servers.insert("remote".into(), grant());
        Ok(())
    })
    .unwrap();
    let current = mutate(&root, &workspace, |tokens| {
        claim_renewal(tokens, "remote", &grant())
    })
    .unwrap();
    assert_eq!(stale.refresh_token, current.refresh_token);
    let overwrite = Grant {
        access_token: "stale-result".into(),
        ..grant()
    };
    assert!(
        mutate(&root, &workspace, |tokens| settle_renewal(
            tokens,
            "remote",
            &stale,
            Renewal::Ready,
            Some(overwrite)
        ))
        .is_err()
    );
    let saved = load(&root).unwrap();
    assert_eq!(saved.servers["remote"].renewal, Renewal::Renewing);
    assert_eq!(saved.servers["remote"].claim, current.claim);
}

#[test]
fn claim_and_renewal_state_must_agree_in_a_saved_record() {
    let renewing = Grant {
        renewal: Renewal::Renewing,
        claim: None,
        ..grant()
    };
    assert!(!renewing.valid());
    let ready = Grant {
        claim: Some("claim".into()),
        ..grant()
    };
    assert!(!ready.valid());
}

#[test]
fn a_replacement_grant_for_another_url_cannot_be_claimed_by_an_older_inspection() {
    let (_temp, root, workspace) = account();
    let inspected = grant();
    mutate(&root, &workspace, |tokens| {
        tokens.servers.insert(
            "remote".into(),
            Grant {
                url: "https://other.example.com/mcp".into(),
                ..grant()
            },
        );
        Ok(())
    })
    .unwrap();
    assert_eq!(
        mutate(&root, &workspace, |tokens| claim_renewal(
            tokens, "remote", &inspected
        ))
        .err(),
        Some(McpAuthError::Rejected)
    );
    assert_eq!(
        load(&root).unwrap().servers["remote"].renewal,
        Renewal::Ready
    );
}

#[test]
fn fallback_issuer_is_the_bare_origin_even_for_a_query_bearing_resource() {
    let resource = Url::parse("https://example.com/mcp?tenant=demo").unwrap();
    assert_eq!(origin_of(&resource).as_str(), "https://example.com/");
}
