//! Pure unit tests for the inbound auth gate. The fail-closed + match
//! logic is exercised WITHOUT spawning a listener or doing network IO —
//! `dispatch` is `token.is_empty() || !request_token_matches(...)`, so the
//! security-critical behaviour reduces to these two pure functions.
use super::*;

fn hdr(name: &str, val: &str) -> hyper::HeaderMap {
    let mut h = hyper::HeaderMap::new();
    h.insert(
        name.parse::<http::header::HeaderName>().unwrap(),
        http::HeaderValue::from_str(val).unwrap(),
    );
    h
}

#[test]
fn bearer_match_passes() {
    assert!(request_token_matches(&hdr("authorization", "Bearer sekret"), "sekret"));
}

#[test]
fn x_api_key_match_passes() {
    assert!(request_token_matches(&hdr("x-api-key", "sekret"), "sekret"));
}

#[test]
fn wrong_bearer_rejected() {
    assert!(!request_token_matches(&hdr("authorization", "Bearer nope"), "sekret"));
}

#[test]
fn missing_header_rejected() {
    assert!(!request_token_matches(&hyper::HeaderMap::new(), "sekret"));
}

#[test]
fn empty_token_is_fail_closed() {
    // dispatch guards `token.is_empty()` explicitly first; this asserts the
    // match function ALSO never accepts anything against an empty token
    // (defence-in-depth — a present header must not match "").
    assert!(!request_token_matches(&hdr("authorization", "Bearer anything"), ""));
}

#[test]
fn case_sensitive_prefix() {
    // "bearer" (lowercase) is NOT the Bearer scheme; must not match.
    assert!(!request_token_matches(&hdr("authorization", "bearer sekret"), "sekret"));
}