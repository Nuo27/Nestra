use super::*;

/// A realistic JSON-layer line: span chain (gw_attempt inside gw_request),
/// message + structured fields, target, timestamp.
const SPANNED_LINE: &str = r#"{"timestamp":"2026-08-24T07:46:28.739637Z","level":"WARN","fields":{"message":"gateway: upstream in-band stream error","endpoint":"opencode-go","model":"ox-alpha-free"},"target":"nestra_lib::orchestration::gateway::protocol_anthropic","span":{"request":"6f0f","endpoint":"opencode-go"},"spans":[{"name":"gw_attempt","fields":{"request":"6f0f","endpoint":"opencode-go","model":"ox-alpha-free","attempt":1}},{"name":"gw_request","fields":{"task":"c9f0","agent":"zcode-desktop","model":"\"ox-alpha-free\""}}]}"#;

const PLAIN_LINE: &str = r#"{"timestamp":"2026-08-24T07:46:15.955592Z","level":"INFO","fields":{"message":"nestra gateway listening on http://127.0.0.1:18777"},"target":"nestra_lib::orchestration::gateway"}"#;

#[test]
fn parse_lifts_correlation_ids_and_fields() {
    let entries = parse_log_entries(&format!("{SPANNED_LINE}\n{PLAIN_LINE}"));
    assert_eq!(entries.len(), 2);

    let spanned = &entries[0];
    assert_eq!(spanned.level, "WARN");
    assert_eq!(
        spanned.target,
        "nestra_lib::orchestration::gateway::protocol_anthropic"
    );
    assert_eq!(spanned.task.as_deref(), Some("c9f0"));
    assert_eq!(spanned.request.as_deref(), Some("6f0f"));
    // Structured fields ride along k=v so search hits them.
    assert!(
        spanned.message.contains("endpoint=opencode-go"),
        "{}",
        spanned.message
    );
    assert!(spanned.message.starts_with("gateway: upstream in-band stream error"));

    let plain = &entries[1];
    assert_eq!(plain.task, None);
    assert_eq!(plain.request, None);
    assert_eq!(plain.level, "INFO");
}

#[test]
fn parse_skips_malformed_and_empty_lines() {
    let torn = r#"{"timestamp":"2026-08-24T07:00:00Z","level":"INF"#;
    let entries = parse_log_entries(&format!("\n{torn}\n{PLAIN_LINE}\n"));
    assert_eq!(entries.len(), 1, "torn/empty lines must not hide the rest");
    assert_eq!(entries[0].level, "INFO");
}

#[test]
fn json_log_files_sorts_newest_first_and_filters_family() {
    let tmp = tempfile::tempdir().unwrap();
    for name in [
        "nestra.2026-08-22.json",
        "nestra.2026-08-24.json",
        "nestra.2026-08-23.json",
        "nestra.2026-08-23.log",
        "crash.log",
        "other.json",
    ] {
        std::fs::write(tmp.path().join(name), "").unwrap();
    }
    let files = json_log_files(tmp.path());
    assert_eq!(
        files,
        vec![
            "nestra.2026-08-24.json".to_string(),
            "nestra.2026-08-23.json".to_string(),
            "nestra.2026-08-22.json".to_string(),
        ]
    );
}
