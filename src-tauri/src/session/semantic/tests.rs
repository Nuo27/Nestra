use super::*;

#[test]
fn payload_round_trips_through_internally_tagged_json() {
    let p = PartPayload::ToolInvocation {
        name: "Bash".into(),
        input: Some(r#"{"command":"ls"}"#.into()),
        mcp: Some(McpProvenance {
            server: Some("fs".into()),
            tool_name: None,
        }),
        child_session_id: None,
    };
    let s = serde_json::to_string(&p).unwrap();
    // internally tagged: {"kind":"tool_invocation","data":{...}}
    assert!(s.contains("\"kind\":\"tool_invocation\""));
    let back = parse_payload(&s).unwrap();
    assert_eq!(p, back);
}

#[test]
fn kind_tag_is_stable_per_variant() {
    assert_eq!(PartPayload::UserMessage { text: "".into() }.kind_tag(), "user_message");
    assert_eq!(
        PartPayload::Thinking { text: "".into(), signature: None }.kind_tag(),
        "thinking"
    );
    assert_eq!(
        PartPayload::Unknown { raw_json: "{}".into() }.kind_tag(),
        "unknown"
    );
}

#[test]
fn tool_invocation_projects_to_tool_message_with_name_and_input() {
    let part = Part {
        seq: 3,
        payload: PartPayload::ToolInvocation {
            name: "Bash".into(),
            input: Some(r#"{"command":"pwd"}"#.into()),
            mcp: None,
            child_session_id: None,
        },
        tool_call_id: Some("call_1".into()),
        message_id: None,
        parent_message_id: None,
        ts: Some(1000),
        raw_json: "{}".into(),
        provider_metadata_json: "{}".into(),
    };
    let m = part.to_message();
    assert_eq!(m.role, "tool");
    assert_eq!(m.tool_name.as_deref(), Some("Bash"));
    assert_eq!(m.tool_input.as_deref(), Some(r#"{"command":"pwd"}"#));
    assert_eq!(m.tool_call_id.as_deref(), Some("call_1"));
}

#[test]
fn tool_result_with_error_carries_is_error_in_metadata() {
    let part = Part {
        seq: 4,
        payload: PartPayload::ToolResult {
            output: "boom".into(),
            is_error: Some(true),
            mcp: None,
        },
        tool_call_id: Some("call_1".into()),
        message_id: None,
        parent_message_id: None,
        ts: Some(1001),
        raw_json: "{}".into(),
        provider_metadata_json: "{}".into(),
    };
    let m = part.to_message();
    assert_eq!(m.role, "tool");
    assert_eq!(m.tool_output.as_deref(), Some("boom"));
    assert!(m.provider_metadata_json.contains("\"is_error\":true"));
}

#[test]
fn thinking_projects_to_role_thinking_with_empty_body() {
    let part = Part {
        seq: 2,
        payload: PartPayload::Thinking {
            text: "reasoning here".into(),
            signature: None,
        },
        tool_call_id: None,
        message_id: None,
        parent_message_id: None,
        ts: None,
        raw_json: "{}".into(),
        provider_metadata_json: "{}".into(),
    };
    let m = part.to_message();
    assert_eq!(m.role, "thinking");
    assert_eq!(m.thinking.as_deref(), Some("reasoning here"));
    assert!(m.content_text.is_empty());
}

#[test]
fn unknown_part_is_lossless_in_projection() {
    let raw = r#"{"type":"mystery","payload":[1,2,3]}"#;
    let part = Part {
        seq: 9,
        payload: PartPayload::Unknown { raw_json: raw.into() },
        tool_call_id: None,
        message_id: None,
        parent_message_id: None,
        ts: None,
        raw_json: raw.into(),
        provider_metadata_json: "{}".into(),
    };
    let m = part.to_message();
    assert_eq!(m.role, "provider_event");
    // the raw json survives in content_text
    assert!(m.content_text.contains("mystery"));
}