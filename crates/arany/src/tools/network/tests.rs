use super::*;

fn grant(allow: &[&str], deny: &[&str]) -> Network {
    Network {
        allow: allow.iter().map(|item| (*item).to_owned()).collect(),
        deny: deny.iter().map(|item| (*item).to_owned()).collect(),
    }
}

#[test]
fn pattern_grammar_is_closed_and_narrow() {
    for valid in [
        "example.com",
        "*.pypi.org",
        "a-b.example.co",
        "127.0.0.1",
        "::1",
    ] {
        assert!(valid_pattern(valid), "{valid}");
    }
    for invalid in [
        "",
        "localhost",
        "*.com",
        "*",
        "**.example.com",
        "foo.*.example.com",
        "Example.com",
        "example.com.",
        "example.com:443",
        "https://example.com",
        "-a.example.com",
        "a-.example.com",
        "exa_mple.com",
        "10.0.0.1",
        "8.8.8.8",
        "1.2.3.4",
        "[::1]",
        "0:0:0:0:0:0:0:1",
        "xn--é.example.com",
    ] {
        assert!(!valid_pattern(invalid), "{invalid}");
    }
    assert!(!valid_pattern(&format!("{}.com", "a".repeat(64))));
    assert!(!valid_pattern(&format!("{}.com", "a.".repeat(130))));
}

#[test]
fn effective_grant_merges_rules_without_duplicates() {
    let merged = grant(&["example.com"], &[]).merged(
        &["example.com".into(), "*.pypi.org".into()],
        &["bad.example.com".into()],
    );
    assert_eq!(merged.allow, ["example.com", "*.pypi.org"]);
    assert_eq!(merged.deny, ["bad.example.com"]);
    assert!(merged.valid());
    let many: Vec<String> = (0..=2 * MAX_NETWORK_PATTERNS)
        .map(|index| format!("h{index}.example.com"))
        .collect();
    assert!(!grant(&[], &[]).merged(&many, &[]).valid());
}
