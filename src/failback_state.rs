//! Replication-failback state, exported for operators.
//!
//! While replication runs on the switch fallback path the daemon is usually
//! *about* to do something (holding for link stability, backing off after a
//! flap, or latched waiting for a human), and `twinstor status` must say
//! which. The operator question is "I replugged the cable, can I take the
//! switch down now?", and a silent hold window reads as "already fixed". Same conventions as `heal_state.rs`: hand-written JSON
//! on /run (gone on reboot, cleared at daemon start), best-effort writes,
//! fail-quiet reads.
//!
//! The request file is the `twinstor failback-replication` handshake: the CLI
//! creates it, the daemon consumes it on the next tick and runs the guarded
//! dance with the stability hold (and the strike latch) bypassed: an
//! explicit operator request is the manual unlatch. Safety gates (UpToDate
//! both sides, no resync, side selection) always apply.

use tracing::debug;

pub const FAILBACK_STATE_PATH: &str = "/run/twinstor/failback-state.json";
pub const FAILBACK_REQUEST_PATH: &str = "/run/twinstor/failback-request";

/// A parsed snapshot for display. (Strikes and the update timestamp stay in
/// the raw JSON for `status --json`; the human renderer doesn't need them.)
pub struct FailbackState {
    pub mode: String,
    pub reason: String,
    pub eta_secs: u64,
}

/// Best-effort write; failback must never fail on observability.
pub fn write(mode: &str, reason: &str, eta_secs: u64, strikes: u32) {
    let body = format!(
        r#"{{"mode":"{mode}","reason":"{reason}","eta_secs":{eta_secs},"strikes":{strikes},"updated_unix":{}}}"#,
        crate::heal_state::unix_now(),
    );
    if let Some(dir) = std::path::Path::new(FAILBACK_STATE_PATH).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(FAILBACK_STATE_PATH, body) {
        debug!(error = %e, "could not write failback-state file");
    }
}

/// Remove the state file (back on the direct link, or daemon starting fresh).
pub fn clear() {
    let _ = std::fs::remove_file(FAILBACK_STATE_PATH);
}

/// Read and minimally parse for display. None when absent or garbled.
pub fn read() -> Option<FailbackState> {
    let text = std::fs::read_to_string(FAILBACK_STATE_PATH).ok()?;
    Some(FailbackState {
        mode: json_str(&text, "mode")?,
        reason: json_str(&text, "reason").unwrap_or_default(),
        eta_secs: json_u64(&text, "eta_secs").unwrap_or(0),
    })
}

/// Raw file content for `status --json` embedding (it is already JSON).
pub fn read_raw() -> Option<String> {
    std::fs::read_to_string(FAILBACK_STATE_PATH).ok()
}

/// CLI side: ask the daemon for an immediate guarded failback.
pub fn place_request() -> std::io::Result<()> {
    if let Some(dir) = std::path::Path::new(FAILBACK_REQUEST_PATH).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(FAILBACK_REQUEST_PATH, b"requested\n")
}

/// Daemon side: consume a pending request (returns whether one existed).
pub fn consume_request() -> bool {
    std::fs::remove_file(FAILBACK_REQUEST_PATH).is_ok()
}

fn json_str(text: &str, key: &str) -> Option<String> {
    let pat = format!(r#""{key}":""#);
    let start = text.find(&pat)? + pat.len();
    let rest = &text[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn json_u64(text: &str, key: &str) -> Option<u64> {
    let pat = format!(r#""{key}":"#);
    let start = text.find(&pat)? + pat.len();
    let digits: String = text[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_extraction() {
        let body = r#"{"mode":"holding","reason":"stability hold (strike 1)","eta_secs":412,"strikes":1,"updated_unix":99}"#;
        assert_eq!(json_str(body, "mode").as_deref(), Some("holding"));
        assert_eq!(
            json_str(body, "reason").as_deref(),
            Some("stability hold (strike 1)")
        );
        assert_eq!(json_u64(body, "eta_secs"), Some(412));
        assert_eq!(json_u64(body, "strikes"), Some(1));
    }
}
