use super::*;
use crate::agents::agents;

#[test]
fn every_manageable_writer_resolves() {
    let unresolved: Vec<&'static str> = agents()
        .iter()
        .filter(|a| a.manageable())
        .map(|a| a.config.writer)
        .filter(|w| adapter_for(w).is_none())
        .collect();
    assert!(
        unresolved.is_empty(),
        "manageable writer keys without an adapter: {unresolved:?}"
    );
}