//! Tests for the shared command-layer infrastructure.

use super::*;

#[tokio::test]
async fn agent_switch_locks_serialize_same_agent_and_isolate_others() {
    let locks = AgentSwitchLocks::default();

    let a1 = locks.lock_of("pi-cli").await;
    let a2 = locks.lock_of("pi-cli").await;
    assert!(Arc::ptr_eq(&a1, &a2), "same agent must share one lock");

    let b = locks.lock_of("zcode-desktop").await;
    assert!(!Arc::ptr_eq(&a1, &b), "different agents get different locks");

    // Held guard blocks the same agent, never another one.
    let g1 = a1.lock().await;
    assert!(a2.try_lock().is_err(), "second acquirer for same agent must wait");
    let g2 = b.try_lock().expect("other agent proceeds unblocked");
    drop(g2);
    drop(g1);
    assert!(a2.try_lock().is_ok(), "lock is free again after release");
}
