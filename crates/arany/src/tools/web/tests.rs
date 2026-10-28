use super::*;
use serde_json::json;

fn fetch(url: &str) -> ToolCall {
    ToolCall::WebFetch { url: url.into() }
}

#[test]
fn fetch_urls_admit_only_https_domains_and_numeric_loopback() {
    for url in [
        "https://example.com/a?b=1",
        "https://docs.example.com/",
        "http://127.0.0.1:8080/x",
        "https://[::1]:8443/x",
    ] {
        assert!(fetch(url).valid(), "{url}");
    }
    for url in [
        "http://example.com/",
        "https://example.com:8443/",
        "https://user@example.com/",
        "https://example.com/#frag",
        "https://10.0.0.1/",
        "https://localhost/",
        "ftp://example.com/",
        "file:///etc/passwd",
        "not a url",
        &format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES)),
    ] {
        assert!(!fetch(url).valid(), "{url}");
    }
}

#[test]
fn search_queries_are_bounded_single_line_text() {
    let search = |query: &str| ToolCall::WebSearch {
        query: query.into(),
    };
    assert!(search("rust async").valid());
    assert!(!search("  ").valid());
    assert!(!search("a\nb").valid());
    assert!(!search(&"q".repeat(MAX_QUERY_BYTES + 1)).valid());
}

#[cfg(target_os = "linux")]
#[test]
fn host_of_a_fetch_is_its_bare_name() {
    assert_eq!(
        host(&fetch("https://Docs.Example.com/x")).unwrap(),
        "docs.example.com"
    );
    assert_eq!(host(&fetch("http://127.0.0.1:9/")).unwrap(), "127.0.0.1");
    assert_eq!(host(&fetch("https://[::1]:9/")).unwrap(), "::1");
}

#[cfg(target_os = "linux")]
#[test]
fn fetch_admits_any_host_the_deny_list_leaves_and_lists_only_configured_ones() {
    let mut config: Config = serde_json::from_value(serde_json::json!({
        "version": 1, "workspace_paths": ["."], "write": false,
        "commands": [], "skills": [], "mcp": [],
        "network": {"allow": ["*.example.com"], "deny": ["bad.example.com"]},
    }))
    .unwrap();
    assert!(config.admits_web(&fetch("https://docs.example.com/")));
    assert!(!config.admits_web(&fetch("https://bad.example.com/")));
    assert!(config.admits_web(&fetch("https://example.org/")));
    assert!(config.lists_web_host(&fetch("https://docs.example.com/")));
    assert!(!config.lists_web_host(&fetch("https://example.org/")));
    let grant_for = grant(&config, &fetch("https://example.org/")).unwrap();
    assert!(grant_for.lists("example.org") && grant_for.lists("docs.example.com"));
    assert!(!grant_for.lists("bad.example.com"));
    assert!(!config.admits_web(&ToolCall::WebSearch { query: "q".into() }));
    config.web_search = Some(WebSearch {
        endpoint: "https://api.search.brave.com/res/v1/web/search".into(),
        key: "ARANY_WEB_SEARCH_KEY".into(),
    });
    assert!(config.admits_web(&ToolCall::WebSearch { query: "q".into() }));
    assert_eq!(
        grant(&config, &ToolCall::WebSearch { query: "q".into() })
            .unwrap()
            .allow,
        ["api.search.brave.com"]
    );
}

#[test]
fn search_backend_keys_use_the_reserved_prefix() {
    let backend = |key: &str, endpoint: &str| WebSearch {
        endpoint: endpoint.into(),
        key: key.into(),
    };
    let https = "https://api.search.brave.com/res/v1/web/search";
    assert!(backend("ARANY_WEB_SEARCH_KEY", https).valid());
    assert!(backend("ARANY_WEB_SEARCH_KEY", "http://127.0.0.1:9/s").valid());
    assert!(!backend("OPENAI_API_KEY", https).valid());
    assert!(!backend("ARANY_WEB_SEARCH_", https).valid());
    assert!(!backend("ARANY_WEB_SEARCH_key", https).valid());
    assert!(!backend("ARANY_WEB_SEARCH_KEY", "http://example.com/s").valid());
}

#[test]
fn html_reduces_to_text_without_scripts_styles_or_entities() {
    let html = "<html><head><title>T</title></head><body><h1>Head &amp; tail</h1>\
        <script>alert(1)</script><style>p{}</style><!-- c --><p>One &lt;b&gt; &#65;\
        &#x42; &bogus;</p><p>Two</p></body></html>";
    assert_eq!(
        extract_text(html),
        "Head & tail\n\nOne <b> AB &bogus;\n\nTwo"
    );
}

#[test]
fn unterminated_markup_never_panics_or_leaks_tags() {
    for html in [
        "<p>a<",
        "<p>a<!-- x",
        "<script>x",
        "a & b",
        "<>",
        "&#99999999999;",
    ] {
        let _ = extract_text(html);
    }
    assert_eq!(extract_text("<p>a</p><scr"), "a");
}

#[test]
fn fit_cuts_at_the_character_cap_and_the_result_budget_and_marks_both() {
    let render = |text: &str, truncated: bool| format!("{{\"t\":{truncated},\"x\":{text:?}}}");
    let short = fit(render, "hello", false, 1024);
    assert!(short.contains("\"t\":false") && !short.contains("truncated]"));
    let long = "é".repeat(WEB_FETCH_CHARS + 5);
    let capped = fit(render, &long, false, usize::MAX);
    assert!(capped.contains("\"t\":true") && capped.contains(TRUNCATION_MARKER.trim()));
    assert_eq!(capped.matches('é').count(), WEB_FETCH_CHARS);
    let tight = fit(render, &long, false, 400);
    assert!(tight.len() <= 400 && tight.contains("\"t\":true"));
    let download_cut = fit(render, "page", true, 1024);
    assert!(download_cut.contains("\"t\":true"));
}

#[test]
fn web_calls_round_trip_and_match_their_declared_schemas() {
    let schema = jsonschema::options()
        .offline()
        .build(&serde_json::json!({"anyOf": crate::tools::catalog::call_schemas()}))
        .unwrap();
    for row in [
        serde_json::json!({"operation": "web_fetch", "url": "https://example.com/docs"}),
        serde_json::json!({"operation": "web_search", "query": "rust reqwest proxy"}),
    ] {
        assert!(schema.is_valid(&row));
        let call: ToolCall = serde_json::from_value(row.clone()).unwrap();
        assert!(call.valid());
        assert_eq!(serde_json::to_value(&call).unwrap(), row);
        let mut unknown = row.clone();
        unknown["network"] = serde_json::json!(true);
        assert!(serde_json::from_value::<ToolCall>(unknown).is_err());
    }
}

#[test]
fn search_results_keep_the_first_eight_bounded_rows() {
    let rows: Vec<_> = (0..12)
        .map(|index| json!({"title": format!("t{index}"), "url": "https://e.example/", "description": "d".repeat(MAX_SEARCH_FIELD_CHARS + 9), "extra": 1}))
        .collect();
    let body = json!({"web": {"results": rows}}).to_string();
    let parsed = results(body.as_bytes()).unwrap();
    assert_eq!(parsed.len(), WEB_SEARCH_RESULTS);
    assert_eq!(parsed[0]["title"], "t0");
    assert_eq!(
        parsed[0]["description"].as_str().unwrap().len(),
        MAX_SEARCH_FIELD_CHARS
    );
    assert!(parsed[0].get("extra").is_none());
    assert!(results(b"not json").is_none());
    assert!(results(br#"{"web":{}}"#).is_none());
}

#[test]
fn entity_scanning_is_bounded_on_hostile_input() {
    let hostile = "&".repeat(WEB_DOWNLOAD_BYTES);
    let text = extract_text(&hostile);
    assert_eq!(text.len(), WEB_DOWNLOAD_BYTES);
}

#[test]
fn host_patterns_follow_the_grant_grammar() {
    assert!(host_matches("example.com", "example.com"));
    assert!(!host_matches("example.com", "docs.example.com"));
    assert!(host_matches("*.example.com", "docs.example.com"));
    assert!(!host_matches("*.example.com", "example.com"));
}

#[test]
fn decoded_results_that_repeat_the_key_are_detected() {
    let key = "ARANYKEY123";
    let escaped =
        r#"{"web":{"results":[{"title":"\u0041RANYKEY123","url":"u","description":"d"}]}}"#;
    let rows = results(escaped.as_bytes()).unwrap();
    assert!(reflects_key(&rows[0], key));
    let clean =
        results(br#"{"web":{"results":[{"title":"t","url":"u","description":"d"}]}}"#).unwrap();
    assert!(!reflects_key(&clean[0], key));
}

#[test]
fn plain_http_clients_build_without_ambient_roots() {
    assert!(client(false, Duration::from_secs(1)).is_ok());
}

#[cfg(target_os = "linux")]
#[test]
fn researchers_need_an_explicit_grant_and_cannot_widen_parent_domains() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = crate::tools::tests::config_value();
    config["network"] = json!({"allow": ["*.example.com"], "deny": ["bad.example.com"]});
    config["web_search"] = json!({
        "endpoint": "https://search.example.com/", "key": "ARANY_WEB_SEARCH_KEY",
    });
    let mut runtime = crate::tools::tests::in_process_runtime(temp.path(), config);
    let listed = fetch("https://docs.example.com/");
    let search = ToolCall::WebSearch { query: "q".into() };
    assert!(!runtime.allows_research_web(&listed));
    assert!(!runtime.allows_research_web(&search));
    let catalog: Value =
        serde_json::from_str(runtime.research_context(3).unwrap().catalog()).unwrap();
    assert!(catalog.get("web").is_none());
    runtime.config.child_web = true;
    assert!(runtime.receipt().child_web);
    assert!(runtime.allows_research_web(&listed));
    assert!(runtime.allows_research_web(&search));
    assert!(!runtime.allows_research_web(&fetch("https://bad.example.com/")));
    assert!(!runtime.allows_research_web(&fetch("https://elsewhere.example/")));
    let context = runtime.research_context(3).unwrap();
    let catalog: Value = serde_json::from_str(context.catalog()).unwrap();
    let calls = crate::tools::catalog::catalog_call_schemas(&catalog);
    for operation in ["web_fetch", "web_search"] {
        assert!(
            calls
                .iter()
                .any(|call| call["properties"]["operation"]["enum"] == json!([operation]))
        );
    }
    let before = runtime.config.digest();
    let mut intent = runtime.intent(crate::RunId::new(), crate::AgentRunId::new(), listed);
    runtime.bind_egress(&mut intent).unwrap();
    assert_eq!(intent.limits.network, crate::tools::NetworkGrant::Proxy);
    assert_eq!(before, runtime.config.digest());
    assert!(
        !runtime
            .config
            .lists_web_host(&fetch("https://elsewhere.example/"))
    );
}

#[cfg(target_os = "linux")]
#[test]
fn remote_mcp_servers_need_exactly_one_transport_and_bind_the_proxy() {
    let parse = |mcp: serde_json::Value| {
        Config::parse(
            &serde_json::to_vec(&json!({
                "version": 1, "workspace_paths": ["."], "write": false,
                "commands": [], "skills": [], "mcp": mcp,
            }))
            .unwrap(),
        )
    };
    let remote = |url: &str| json!({"name": "remote", "tools": ["echo"], "url": url, "auth": "oauth"});
    let config = parse(json!([remote("https://mcp.example.com/mcp")])).unwrap();
    let call = ToolCall::McpList {
        server: "remote".into(),
    };
    assert_eq!(
        config.remote_server(&call).and_then(|server| server.host()),
        Some("mcp.example.com".to_owned())
    );
    assert!(
        config
            .remote_server(&ToolCall::McpList {
                server: "other".into()
            })
            .is_none()
    );
    assert!(parse(json!([remote("http://127.0.0.1:9/mcp")])).is_ok());
    for rejected in [
        remote("http://mcp.example.com/mcp"),
        remote("https://mcp.example.com:8443/mcp"),
        remote("https://10.0.0.1/mcp"),
        json!({"name": "remote", "tools": ["echo"]}),
        json!({"name": "remote", "tools": ["echo"], "url": "https://mcp.example.com/",
            "args": ["--flag"]}),
        json!({"name": "remote", "tools": ["echo"], "url": "https://mcp.example.com/",
            "auth": "basic"}),
    ] {
        assert!(parse(json!([rejected])).is_err());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn remote_mcp_is_identified_so_children_can_be_denied() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = crate::tools::tests::config_value();
    config["mcp"] = json!([{"name": "remote", "tools": ["echo"],
        "url": "https://mcp.example.com/mcp", "auth": "oauth"}]);
    let runtime = crate::tools::tests::in_process_runtime(temp.path(), config);
    let list = ToolCall::McpList {
        server: "remote".into(),
    };
    assert!(runtime.is_remote_mcp(&list));
    assert!(!runtime.is_remote_mcp(&ToolCall::McpList {
        server: "other".into()
    }));
}
