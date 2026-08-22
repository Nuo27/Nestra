use super::*;

#[test]
fn terminal_sse_error_formats_per_wire() {
    // OpenAI chat dialect: a data: error object, then [DONE] — the AI SDK
    // parses the error instead of seeing a dead connection.
    let openai = terminal_sse_error(ProviderKind::Openai, "boom \"quote\"");
    assert!(openai.starts_with("data: {"));
    assert!(openai.ends_with("data: [DONE]\n\n"));
    assert!(openai.contains("boom \\\""), "message is JSON-escaped");
    // Anthropic dialect: an `error` event with a data payload.
    let anthropic = terminal_sse_error(ProviderKind::Anthropic, "boom");
    assert!(anthropic.starts_with("event: error\ndata: "));
    assert!(!anthropic.contains("[DONE]"));
}

#[test]
fn path_is_messages_accepts_both_forms() {
    // prefix-less path + agent-prefixed path both route here
    // (dispatch already splits agents, so ANY `/<agent>/v1/messages` is
    // this handler's business — including hypothetical non-claude agents).
    assert!(path_is_messages("/v1/messages"));
    assert!(path_is_messages("/v1/messages/"));
    assert!(path_is_messages("/v1/messages?foo=bar"));
    assert!(path_is_messages("/claude-code-cli/v1/messages"));
    assert!(path_is_messages("/pi/v1/messages"), "any agent prefix is accepted here");
    assert!(!path_is_messages("/v1/chat/completions"));
    assert!(!path_is_messages("/claude-code-cli/v1/messages/extra"));
}

#[test]
fn observe_text_window_handles_split_frames() {
    // One SSE data line split mid-JSON across two frames (the boundary
    // lands inside the JSON payload) must still yield its usage — the
    // carry buffer holds the partial until the newline arrives.
    let full = "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n\n";
    let (a, b) = full.split_at(full.len() / 2);
    let mut obs = StreamObservation::default();
    let mut carry = String::new();
    observe_text_window(ProviderKind::Anthropic, &mut carry, a.as_bytes(), &mut obs);
    assert_eq!(obs.usage.output, None, "partial line is not parsed yet");
    observe_text_window(ProviderKind::Anthropic, &mut carry, b.as_bytes(), &mut obs);
    assert_eq!(obs.usage.output, Some(9));
    assert!(carry.is_empty(), "complete lines leave no carry");

    // A window whose bytes end exactly on a newline observes immediately;
    // nothing is held back.
    let mut obs2 = StreamObservation::default();
    let mut carry2 = String::new();
    observe_text_window(
        ProviderKind::Anthropic,
        &mut carry2,
        &b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":4}}\n"[..],
        &mut obs2,
    );
    assert_eq!(obs2.usage.output, Some(4));
}