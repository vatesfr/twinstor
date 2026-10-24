//! Auto-heal episode state, exported for operators.
//!
//! The split-brain / dual-master resolution ladder is slow on purpose (safety
//! margins), and during an episode `twinstor status` would otherwise show
//! only StandAlone + serving-without-redundancy: true, but it reads as
//! "broken and idle" when the real state is "auto-heal stage 2 of 3, next
//! escalation in ~90s, hands off". The most damaging thing an operator can
//! do mid-ladder is run manual recovery
//! commands that fight the daemon, so the state machine exports its episode
//! state here on every resolution pass, `status` renders it, and the support
//! bundle captures it.
//!
//! The file is hand-written JSON (same convention as `status --json` and the
//! bundle manifest: fixed keys, caller-controlled strings with no quotes), so
//! there is no parser dependency in either direction. It lives on /run, so
//! it is gone on reboot; it is also removed at daemon start (in-memory counters
//! reset with the process, so a stale file must not outlive them) and when an
//! episode ends.

use tracing::debug;

pub const HEAL_STATE_PATH: &str = "/run/twinstor/heal-state.json";

/// A parsed snapshot for display. Numeric fields default to 0 when absent so
/// the renderer never needs to distinguish "old file" from "no counter".
pub struct HealState {
    pub stage: String,
    pub summary: String,
    pub next: Option<String>,
    pub attempts: u64,
    pub restarts: u64,
    pub holds: u64,
    pub started_unix: u64,
    pub updated_unix: u64,
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Best-effort write; the resolution must never fail on observability.
#[allow(clippy::too_many_arguments)]
pub fn write(
    stage: &str,
    summary: &str,
    next: Option<&str>,
    attempts: u32,
    restarts: u32,
    holds: u32,
    started_unix: u64,
) {
    let next_json = match next {
        Some(n) => format!(r#""{n}""#),
        None => "null".to_string(),
    };
    let body = format!(
        r#"{{"stage":"{stage}","summary":"{summary}","next":{next_json},"attempts":{attempts},"restarts":{restarts},"holds":{holds},"started_unix":{started_unix},"updated_unix":{}}}"#,
        unix_now(),
    );
    if let Some(dir) = std::path::Path::new(HEAL_STATE_PATH).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(HEAL_STATE_PATH, body) {
        debug!(error = %e, "could not write heal-state file");
    }
}

/// Remove the state file (episode over, or daemon starting fresh).
pub fn clear() {
    let _ = std::fs::remove_file(HEAL_STATE_PATH);
}

/// Read and minimally parse the state file for display. Returns None when the
/// file is absent (no episode) or unreadable/garbled (fail quiet: status must
/// never break on a half-written snapshot).
pub fn read() -> Option<HealState> {
    let text = std::fs::read_to_string(HEAL_STATE_PATH).ok()?;
    Some(HealState {
        stage: json_str(&text, "stage")?,
        summary: json_str(&text, "summary")?,
        next: json_str(&text, "next"),
        attempts: json_u64(&text, "attempts").unwrap_or(0),
        restarts: json_u64(&text, "restarts").unwrap_or(0),
        holds: json_u64(&text, "holds").unwrap_or(0),
        started_unix: json_u64(&text, "started_unix").unwrap_or(0),
        updated_unix: json_u64(&text, "updated_unix").unwrap_or(0),
    })
}

/// Raw file content for embedding into `status --json` (it is already JSON).
pub fn read_raw() -> Option<String> {
    std::fs::read_to_string(HEAL_STATE_PATH).ok()
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
    fn roundtrip_field_extraction() {
        let body = r#"{"stage":"survivor_keep","summary":"waiting for peer","next":"HA-reset at attempt 11","attempts":7,"restarts":1,"holds":2,"started_unix":100,"updated_unix":200}"#;
        assert_eq!(json_str(body, "stage").as_deref(), Some("survivor_keep"));
        assert_eq!(
            json_str(body, "summary").as_deref(),
            Some("waiting for peer")
        );
        assert_eq!(
            json_str(body, "next").as_deref(),
            Some("HA-reset at attempt 11")
        );
        assert_eq!(json_u64(body, "attempts"), Some(7));
        assert_eq!(json_u64(body, "updated_unix"), Some(200));
    }

    #[test]
    fn null_next_and_missing_keys_fail_quiet() {
        let body = r#"{"stage":"loser_discard","summary":"discarding","next":null,"attempts":0,"restarts":0,"holds":0,"started_unix":1,"updated_unix":2}"#;
        // `next` is null (not a string): extraction returns None, renderer skips it.
        assert_eq!(json_str(body, "next"), None);
        assert_eq!(json_str(body, "absent"), None);
        assert_eq!(json_u64(body, "absent"), None);
    }
}
