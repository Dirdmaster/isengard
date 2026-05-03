use isengard_core::labels::parse_labels;
use std::collections::HashMap;

fn lbl(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

#[test]
fn parses_default_unnamed_rule() {
    let rules = parse_labels(&lbl(&[
        ("isengard.expose", "blog.example.com"),
        ("isengard.expose.port", "8080"),
        ("isengard.expose.tls", "edge"),
    ]));
    assert_eq!(rules.len(), 1);
    let r = &rules[0];
    assert_eq!(r.name, None);
    assert_eq!(r.hostname, "blog.example.com");
    assert_eq!(r.port, Some(8080));
    assert_eq!(r.tls.as_deref(), Some("edge"));
}

#[test]
fn parses_multi_named_rules() {
    let rules = parse_labels(&lbl(&[
        ("isengard.expose.web", "blog.example.com"),
        ("isengard.expose.web.port", "8080"),
        ("isengard.expose.api", "api.example.com"),
        ("isengard.expose.api.port", "8081"),
        ("isengard.expose.api.auth", "cf-access"),
    ]));
    assert_eq!(rules.len(), 2);
    let web = rules
        .iter()
        .find(|r| r.name.as_deref() == Some("web"))
        .unwrap();
    assert_eq!(web.hostname, "blog.example.com");
    assert_eq!(web.port, Some(8080));
    let api = rules
        .iter()
        .find(|r| r.name.as_deref() == Some("api"))
        .unwrap();
    assert_eq!(api.hostname, "api.example.com");
    assert_eq!(api.auth.as_deref(), Some("cf-access"));
}

#[test]
fn ignores_unrelated_labels() {
    let rules = parse_labels(&lbl(&[
        ("traefik.enable", "true"),
        ("isengard.expose", "x.test"),
    ]));
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].hostname, "x.test");
}

#[test]
fn empty_or_no_isengard_labels_returns_empty() {
    let rules = parse_labels(&lbl(&[("foo", "bar")]));
    assert!(rules.is_empty());
}
