
#[test]
fn build_resume_command_substitutes_native_id() {
    use crate::session::provider::{
        build_resume_command, default_provider_registry,
    };
    let reg = default_provider_registry();
    // Each provider's native CLI accepts its own id.
    assert_eq!(
        build_resume_command(&reg, "claude-code-cli", "claude-code-cli", "abc").unwrap(),
        "claude --resume abc"
    );
    // Pi uses the corrected flag.
    assert_eq!(
        build_resume_command(&reg, "pi-cli", "pi-cli", "uuid-pi").unwrap(),
        "pi --session uuid-pi"
    );
    // opencode-desktop is intentionally non-resumable (no resume_command),
    // so it's absent from the resumable registry; OpenCode sessions surface
    // as browse/delete-only.
}

#[test]
fn build_resume_command_refuses_cross_provider() {
    use crate::session::provider::{
        build_resume_command, default_provider_registry,
    };
    let reg = default_provider_registry();
    // Claude session cannot be opened in Pi's CLI (cross-provider refused).
    let err = build_resume_command(&reg, "claude-code-cli", "pi-cli", "x").unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("source 'claude-code-cli'"), "{msg}");
}

#[test]
fn build_resume_command_refuses_unsupported_cli() {
    use crate::session::provider::{
        build_resume_command, default_provider_registry,
    };
    let reg = default_provider_registry();
    // An agent id not in the registry at all — resume must be refused.
    let err = build_resume_command(&reg, "nonexistent-cli", "custom", "x").unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("is not a registered agent"),
        "{msg}"
    );
}

#[test]
fn terminal_command_wraps_resume_in_kept_open_cmd_window() {
    use super::terminal_command;
    assert_eq!(
        terminal_command("claude --resume abc"),
        "/K claude --resume abc"
    );
    // Even when the resume command itself contains spaces it passes through
    // verbatim; `cmd /K` runs it then keeps the window open.
    assert_eq!(
        terminal_command("pi --session x y z"),
        "/K pi --session x y z"
    );
}

#[test]
fn reveal_target_resolves_file_to_parent_dir() {
    use crate::agents::reveal_target;
    // A session's source_path is its backing file → reveal the folder.
    assert_eq!(
        reveal_target("C:\\Users\\me\\.claude\\projects\\p\\abc.jsonl"),
        std::path::PathBuf::from("C:\\Users\\me\\.claude\\projects\\p")
    );
}

#[test]
fn reveal_target_resolves_directory_to_itself() {
    use crate::agents::reveal_target;
    // A session whose source_path IS a directory reveals that directory.
    let dir = std::env::temp_dir().join(format!("nestra-reveal-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.to_string_lossy().to_string();
    assert_eq!(reveal_target(&p), std::path::PathBuf::from(&p));
    std::fs::remove_dir_all(&dir).unwrap();
}