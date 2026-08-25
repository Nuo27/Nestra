use super::*;

#[test]
fn preset_filters_widen_only_nestra_targets() {
    assert_eq!(LevelPreset::Info.filter(), "info,tauri=warn,nestra_lib=info");
    assert_eq!(
        LevelPreset::Debug.filter(),
        "info,tauri=warn,nestra_lib=debug"
    );
    assert_eq!(
        LevelPreset::Trace.filter(),
        "info,tauri=warn,nestra_lib=trace"
    );
    // Every preset keeps dependency chatter quiet — widening applies to
    // Nestra's own instrumentation only.
    for p in [LevelPreset::Info, LevelPreset::Debug, LevelPreset::Trace] {
        assert!(p.filter().contains("tauri=warn"), "{p:?}");
    }
}

#[test]
fn preset_parse_round_trip() {
    for p in [LevelPreset::Info, LevelPreset::Debug, LevelPreset::Trace] {
        assert_eq!(LevelPreset::parse(p.as_str()), Some(p));
    }
    assert_eq!(LevelPreset::parse("verbose"), None);
    assert_eq!(LevelPreset::parse(""), None);
}

#[test]
fn set_preset_updates_mirror_even_without_init() {
    // Before `init` there is no reload handle — the swap reports false but
    // the requested preset is still the recorded choice, so a later
    // `apply_persisted_preset` converges to it.
    assert!(!set_preset(LevelPreset::Trace));
    assert_eq!(current_preset(), LevelPreset::Trace);
    set_preset(LevelPreset::Info);
    assert_eq!(current_preset(), LevelPreset::Info);
}
