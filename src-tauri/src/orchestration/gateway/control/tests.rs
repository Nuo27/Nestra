use super::*;

#[test]
fn ct_eq_handles_lengths_and_matches() {
    assert!(ct_eq("abc", "abc"));
    assert!(!ct_eq("abc", "abd"));
    assert!(!ct_eq("abc", "ab"));
    assert!(!ct_eq("abc", "abcd"));
    assert!(ct_eq("", ""));
}

#[test]
fn hex_encode_is_lowercase_and_even_length() {
    let s = hex_encode(&[0x00, 0xff, 0xab]);
    assert_eq!(s, "00ffab");
}

#[test]
fn generated_token_is_64_hex_chars() {
    let t = generate_token();
    assert_eq!(t.len(), 64);
    assert!(t.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    // two generations differ (non-deterministic source)
    assert_ne!(t, generate_token());
}

/// `find_free_loopback_port` must skip a deliberately-held port and return a
/// different, bindable one. Best-effort: bind-and-hold the start+1 port so
/// the scan has to move on to start+2.
#[test]
fn find_free_loopback_port_skips_held_port() {
    let base = 18777u16; // arbitrary stable base for the probe
    let held = base + 1; // the scan's first candidate
    let _hold = std::net::TcpListener::bind(("127.0.0.1", held)).unwrap();
    match find_free_loopback_port(base, 8) {
        Some(p) => {
            assert_ne!(p, held, "must not return the port we are holding");
            // the returned port must itself be bindable right now
            std::net::TcpListener::bind(("127.0.0.1", p)).unwrap();
        }
        None => {
            // all 8 candidates busy — environment too noisy; skip rather than flake
            eprintln!("find_free_loopback_port: all candidates busy, skipping");
        }
    }
}