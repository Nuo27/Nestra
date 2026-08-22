use super::*;
use std::time::Instant;

/// A fake `pi --mode rpc`: answers a `prompt` with a canned event
/// sequence (start → assistant verdict text → settled); exits on `abort`
/// or stdin close. Runs under `node -e` so the test needs no binaries
/// beyond Node (guaranteed in this repo's toolchain).
const SHIM: &str = r#"
const readline = require("readline");
const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
  let msg = {}; try { msg = JSON.parse(line); } catch {}
  if (msg.type === "prompt") {
    process.stdout.write(JSON.stringify({type:"session_start", session_id:"sess-native-42"}) + "\n");
    process.stdout.write(JSON.stringify({type:"agent_start"}) + "\n");
    process.stdout.write(JSON.stringify({type:"message_update", message:{role:"assistant", content:[{type:"text",text:"VERDICT: pass — change is sound"}]}}) + "\n");
    process.stdout.write(JSON.stringify({type:"agent_settled"}) + "\n");
  }
  if (msg.type === "abort" || msg.type === "get_messages") {
    if (msg.type === "get_messages") {
      process.stdout.write(JSON.stringify({type:"messages", messages:[{role:"user",content:"?"},{role:"assistant",content:"fallback text"}]}) + "\n");
    } else {
      process.exit(0);
    }
  }
});
"#;

#[test]
fn supervisor_round_trip_and_reap() {
    let sup = PiSupervisor::spawn(
        "node",
        &["-e".to_string(), SHIM.to_string()],
        None,
    )
    .unwrap();
    sup.send(&serde_json::json!({ "type": "prompt", "text": "review this" })).unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut events = Vec::new();
    while Instant::now() < deadline {
        match sup.next_event(Duration::from_millis(200)) {
            Some(v) => {
                events.push(v);
                if has_settled(&events) {
                    break;
                }
            }
            None => {
                assert!(!sup.is_finished(), "child exited before settling");
            }
        }
    }
    assert!(has_settled(&events), "expected agent_settled, got {events:?}");
    let text = final_assistant_text(&events).expect("assistant verdict text");
    assert!(text.contains("VERDICT: pass"), "{text}");
    assert_eq!(sup.events_snapshot().len(), events.len());
    // The native session id revealed by the stream is extractable…
    assert_eq!(session_id_of(&events).as_deref(), Some("sess-native-42"));
    // …including the nested `session.id` shape.
    let nested = vec![serde_json::json!({"type":"session","session":{"id":"nested-7"}})];
    assert_eq!(session_id_of(&nested).as_deref(), Some("nested-7"));
    assert_eq!(session_id_of(&[]), None);

    // get_messages fallback shape also parses.
    sup.send(&serde_json::json!({ "type": "get_messages" })).unwrap();
    let mut again = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while again.len() < 1 && Instant::now() < deadline {
        if let Some(v) = sup.next_event(Duration::from_millis(200)) {
            again.push(v);
        }
    }
    let mut all = sup.events_snapshot();
    assert_eq!(final_assistant_text(&all).as_deref(), Some("fallback text"));

    // Shutdown reaps (idempotent).
    sup.shutdown();
    sup.shutdown();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !sup.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(sup.is_finished(), "child must be reaped after shutdown");
    let _ = &mut all;
}