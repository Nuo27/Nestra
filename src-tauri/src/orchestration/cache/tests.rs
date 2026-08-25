use super::*;

#[test]
fn disabled_policy_returns_unchanged() {
    let body = br#"{"model":"x","messages":[]}"#;
    let out = inject_cache_control(body, 0);
    assert_eq!(out.as_ref(), body.as_slice());
}

#[test]
fn marks_last_tool_definition() {
    let body = br#"{"model":"x","tools":[{"name":"a","input_schema":{}},{"name":"b","input_schema":{}}],"messages":[]}"#;
    let out = inject_cache_control(body, 1);
    let v: Value = serde_json::from_slice(&out).unwrap();
    let tools = v["tools"].as_array().unwrap();
    assert!(tools[1].get("cache_control").is_some(), "last tool marked");
    assert!(tools[0].get("cache_control").is_none(), "first tool untouched");
}

#[test]
fn marks_last_system_block_when_no_tools() {
    let body = r#"{"model":"x","system":[{"type":"text","text":"sys1"},{"type":"text","text":"sys2"}],"messages":[]}"#;
    let out = inject_cache_control(body.as_bytes(), 1);
    let v: Value = serde_json::from_slice(&out).unwrap();
    let sys = v["system"].as_array().unwrap();
    assert!(sys[1].get("cache_control").is_some(), "last system block marked");
    assert!(sys[0].get("cache_control").is_none());
}

#[test]
fn marks_last_user_message_last_text_block_when_no_tools_or_system() {
    let body = r#"{"model":"x","messages":[
            {"role":"user","content":[{"type":"text","text":"hello"}]},
            {"role":"assistant","content":[{"type":"text","text":"hi"}]},
            {"role":"user","content":[{"type":"text","text":"follow-up"}]}
        ]}"#;
    let out = inject_cache_control(body.as_bytes(), 1);
    let v: Value = serde_json::from_slice(&out).unwrap();
    // Anthropic only honors the breakpoint on the FINAL message — the
    // planner must mark the last user turn, never an earlier one.
    assert!(v["messages"][0]["content"][0].get("cache_control").is_none());
    assert!(v["messages"][2]["content"][0].get("cache_control").is_some());
}

#[test]
fn existing_breakpoints_eat_into_the_budget() {
    // The request already carries 4 breakpoints (tools + 3 system blocks):
    // the API caps the TOTAL at 4, so Nestra must add nothing.
    let body = r#"{"model":"x",
            "tools":[{"name":"a","input_schema":{},"cache_control":{"type":"ephemeral"}}],
            "system":[
                {"type":"text","text":"s1","cache_control":{"type":"ephemeral"}},
                {"type":"text","text":"s2","cache_control":{"type":"ephemeral"}},
                {"type":"text","text":"s3","cache_control":{"type":"ephemeral"}}
            ],
            "messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]}"#;
    let out = inject_cache_control(body.as_bytes(), 1);
    assert_eq!(
        out.as_ref(),
        body.as_bytes(),
        "budget exhausted by pre-existing breakpoints → unchanged"
    );
    // 3 existing (2 system + 1 tool) + budget 1 → exactly ONE new
    // breakpoint allowed; the remaining system block gets it.
    let body3 = r#"{"model":"x",
            "tools":[{"name":"a","input_schema":{},"cache_control":{"type":"ephemeral"}}],
            "system":[
                {"type":"text","text":"s1","cache_control":{"type":"ephemeral"}},
                {"type":"text","text":"s2","cache_control":{"type":"ephemeral"}},
                {"type":"text","text":"s3"}
            ],
            "messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]}"#;
    let out3 = inject_cache_control(body3.as_bytes(), 1);
    let v3: Value = serde_json::from_slice(&out3).unwrap();
    // The budget's single slot lands on the LAST system block (s3) —
    // s1/s2/tool keep their pre-existing breakpoints, nothing else is
    // touched, and the total stays at the API's 4-cap.
    assert!(
        v3["system"][2].get("cache_control").is_some(),
        "new breakpoint lands on the last system block"
    );
    assert!(v3["system"][0].get("cache_control").is_some(), "s1 untouched");
    assert!(v3["system"][1].get("cache_control").is_some(), "s2 untouched");
    assert!(
        v3["tools"][0].get("cache_control").is_some(),
        "tool breakpoint untouched"
    );
    let mut total = 0usize;
    for s in v3["system"].as_array().unwrap() {
        if s.get("cache_control").is_some() {
            total += 1;
        }
    }
    for m in v3["messages"].as_array().unwrap() {
        if let Some(c) = m.get("content").and_then(|c| c.as_array()) {
            for block in c {
                if block.get("cache_control").is_some() {
                    total += 1;
                }
            }
        }
    }
    assert_eq!(total, 3, "one new + two pre-existing system breakpoints");
}

#[test]
fn does_not_mark_empty_text_blocks() {
    let body = r#"{"model":"x","messages":[{"role":"user","content":[{"type":"text","text":""}]}]}"#;
    let out = inject_cache_control(body.as_bytes(), 1);
    // Nothing cacheable → unchanged bytes.
    assert_eq!(out.as_ref(), body.as_bytes());
}

#[test]
fn respects_max_breakpoints_budget() {
    let body = r#"{"model":"x","tools":[{"name":"a","input_schema":{}}],"system":[{"type":"text","text":"sys"}],"messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]}"#;
    // Budget 1 → only tools marked.
    let out = inject_cache_control(body.as_bytes(), 1);
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert!(v["tools"][0].get("cache_control").is_some());
    assert!(v["system"][0].get("cache_control").is_none());
    // Budget 2 → tools + system.
    let out = inject_cache_control(body.as_bytes(), 2);
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert!(v["tools"][0].get("cache_control").is_some());
    assert!(v["system"][0].get("cache_control").is_some());
}

#[test]
fn never_exceeds_hard_cap_of_four() {
    // The planner marks ONE breakpoint per cacheable category (last tool,
    // last system block, first-user-message last text block) — a maximum
    // of 3, well under the API's hard cap of 4. This test pins that bound:
    // even with a huge budget, we never exceed HARD_MAX_BREAKPOINTS.
    let body = r#"{"model":"x","tools":[{"name":"a","input_schema":{}},{"name":"b","input_schema":{}},{"name":"c","input_schema":{}}],"system":[{"type":"text","text":"s1"},{"type":"text","text":"s2"}],"messages":[{"role":"user","content":[{"type":"text","text":"m1"},{"type":"text","text":"m2"}]}]}"#;
    let out = inject_cache_control(body.as_bytes(), 99);
    let v: Value = serde_json::from_slice(&out).unwrap();
    let mut count = 0usize;
    for t in v["tools"].as_array().unwrap() {
        if t.get("cache_control").is_some() {
            count += 1;
        }
    }
    for s in v["system"].as_array().unwrap() {
        if s.get("cache_control").is_some() {
            count += 1;
        }
    }
    for m in v["messages"].as_array().unwrap() {
        if let Some(c) = m.get("content").and_then(|c| c.as_array()) {
            for block in c {
                if block.get("cache_control").is_some() {
                    count += 1;
                }
            }
        }
    }
    assert!(
        count <= HARD_MAX_BREAKPOINTS,
        "planner must never exceed the API's 4-breakpoint cap (got {count})"
    );
    // One per category: tools (1) + system (1) + first-user-message (1).
    assert_eq!(count, 3);
    // The budget did not cause multiple marks within a category.
    let tool_marks = v["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t.get("cache_control").is_some())
        .count();
    assert_eq!(tool_marks, 1, "only the LAST tool is marked");
}

#[test]
fn idempotent_when_already_marked() {
    let body = r#"{"model":"x","tools":[{"name":"a","input_schema":{},"cache_control":{"type":"ephemeral"}}]}"#;
    let out = inject_cache_control(body.as_bytes(), 1);
    let v: Value = serde_json::from_slice(&out).unwrap();
    let tools = v["tools"].as_array().unwrap();
    // Already marked → no double insert, budget untouched but no change.
    assert!(tools[0].get("cache_control").is_some());
    // Budget was NOT consumed (contains_key check) — but with nothing
    // else cacheable the output is still valid.
    let out2 = inject_cache_control(out.as_ref(), 1);
    assert_eq!(out2.as_ref(), out.as_ref(), "second pass is a no-op");
}

#[test]
fn malformed_body_unchanged() {
    let body = b"not json";
    let out = inject_cache_control(body, 1);
    assert_eq!(out.as_ref(), body);
}

#[test]
fn breakpoints_from_policy_maps_flag() {
    assert_eq!(breakpoints_from_policy(false), 0);
    assert_eq!(breakpoints_from_policy(true), DEFAULT_MAX_BREAKPOINTS);
}