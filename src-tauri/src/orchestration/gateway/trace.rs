//! Gateway logging vocabulary — the event contract for request tracing.
//!
//! Every inbound request runs inside a `gw_request{task=…, agent=…}` span,
//! every upstream attempt inside a nested
//! `gw_attempt{request=…, endpoint=…, model=…, reason=…, attempt=N}` span,
//! so ALL events raised during a request (including the older scattered
//! warns) carry the correlation prefix in the text log and the span chain
//! in the JSON log. Events raised while the agent-facing body is still
//! being polled (stream relay) run OUTSIDE those spans and therefore carry
//! an explicit `request=…` field instead.
//!
//! Lifecycle milestones (info level — successful requests are visible):
//!
//! | Event | Where | Fields |
//! |---|---|---|
//! | `gw.request inbound` | each `handle_bytes` | task, agent, wire, bytes, model, role, side_effect |
//! | `gw.route` | forward loop after resolve | endpoint, model, protocol, reason — or `eligible=false` on a fail-closed resolve |
//! | `gw.attempt outcome` | forward loop after forward | status, class, duration_ms, generation_started — fires at request level (the `gw_attempt` span has closed; the `attempt`/ordinal field carries the count) |
//! | `gw.decide` | forward loop after decide | decision (retry/migrate/surface), backoff or reason |
//! | `gw.done` | success exit | status, total_ms |
//! | `gw.abort` | 499 finalize | request, phase |
//! | `gw.stream done` | ObservingBody finish | request, usage_in, usage_out, duration_ms |
//!
//! Wire-level evidence (debug level only — never on by default, bodies are
//! user content): `gw.request body` (inbound), `gw.upstream request`
//! (URL + converted body), `gw.upstream body` (buffered upstream response).
//! Headers are NEVER logged — Authorization / x-api-key / cookie secrecy is
//! an absolute rule.

/// Maximum bytes of a body captured into a debug event by default. Enough
/// to identify shape and error payloads; the full body stays out of the
/// logs unless full-body capture is switched on (below).
const SNIPPET_MAX: usize = 2048;

/// Full-body capture opt-in (Settings live via `diag_log_full_bodies_set`):
/// when on, `capture` returns the WHOLE body — the user explicitly asked to
/// see complete forwarded requests. Daily-rotated, retention-bounded files
/// keep the log growth survivable.
// ponytail: a process-global switch, not per-endpoint granularity — add
// scoping if full capture ever needs to target one misbehaving endpoint.
static FULL_BODIES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn set_full_bodies(on: bool) {
    FULL_BODIES.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn full_bodies() -> bool {
    FULL_BODIES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Lossy body capture for debug events: the whole body when full-body
/// capture is on, otherwise a truncation-marked 2 KiB snippet.
pub(crate) fn capture(bytes: &[u8]) -> String {
    if full_bodies() || bytes.len() <= SNIPPET_MAX {
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        format!(
            "{}… ({} bytes total, first {} shown — enable full-body capture for everything)",
            String::from_utf8_lossy(&bytes[..SNIPPET_MAX]),
            bytes.len(),
            SNIPPET_MAX
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_passes_short_bodies_through() {
        assert_eq!(capture(b"{\"model\":\"m\"}"), "{\"model\":\"m\"}");
    }

    #[test]
    fn capture_truncates_and_marks_by_default() {
        let big = vec![b'x'; 10_000];
        let out = capture(&big);
        assert!(out.contains("10000 bytes total"), "{out}");
        assert!(out.starts_with('x'));
        assert!(out.len() < 10_000, "truncated, not the raw body");
    }

    #[test]
    fn capture_returns_everything_when_full_bodies_enabled() {
        set_full_bodies(true);
        let big = vec![b'x'; 10_000];
        let out = capture(&big);
        set_full_bodies(false); // reset — the flag is process-global
        assert_eq!(out.len(), 10_000, "no truncation in full mode");
    }

    #[test]
    fn capture_is_lossy_for_invalid_utf8() {
        let out = capture(&[0xff, 0xfe, b'a']);
        assert!(out.contains('a'), "valid tail kept: {out}");
    }
}
