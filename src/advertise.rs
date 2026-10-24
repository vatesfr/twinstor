//! Advertise twinstor's presence and live storage state to XO (and any XAPI
//! consumer) through this host's `other_config`, under the `twinstor-*`
//! namespace. XO reads these to detect twinstor pools, display storage health,
//! and pace rolling pool updates: wait for `twinstor-synced=true` before
//! rebooting the next host, so a returning node's DRBD resync always finishes
//! before the pool loses its other copy.
//!
//! `stamp_daemon_alive` already publishes `twinstor-alive` (a 60s liveness
//! heartbeat, the staleness signal) and `twinstor-version`; this module
//! adds the storage-state keys. All values are strings (other_config maps
//! string to string). twinstor owns these keys and xapi never overwrites
//! them, so there is no boot race like the one on `host.enabled` (which xapi
//! force-sets on boot). The keys default to the conservative value
//! (`synced=false` until proven UpToDate).

use crate::drbd::{ConnectionState, DiskState, DrbdStatus};

/// other_config key-set schema version. Bump on an incompatible change so XO can
/// branch on it.
pub const SCHEMA: &str = "1";

/// Stateful resync-completion estimator. DRBD 9 exposes no finish-time, so the
/// ETA is derived here: smooth the done%/second rate (EWMA) from the status
/// stream and project it to 100%. Lives on the state machine (not thread-shared).
#[derive(Debug, Default)]
pub struct ResyncEta {
    last_done: Option<f64>,
    last_at: Option<std::time::Instant>,
    /// done-percent per second, EWMA-smoothed.
    rate: Option<f64>,
    /// When done% last stopped advancing (for stall detection).
    no_progress_since: Option<std::time::Instant>,
}

impl ResyncEta {
    const ALPHA: f64 = 0.3;

    /// Feed the current resync `done` percentage (`None` when no resync is
    /// running) sampled at `now`. Returns `Some(seconds)` remaining when a
    /// positive smoothed rate is known, else `None` (first sample, or no forward
    /// progress this interval).
    pub fn update(&mut self, done: Option<f64>, now: std::time::Instant) -> Option<i64> {
        let Some(done) = done else {
            *self = Self::default();
            return None;
        };
        let mut eta = None;
        if let (Some(prev_done), Some(prev_at)) = (self.last_done, self.last_at) {
            let dt = now.duration_since(prev_at).as_secs_f64();
            if dt > 0.0 {
                let inst = (done - prev_done) / dt;
                if inst > 0.0 {
                    self.no_progress_since = None;
                    let r = self
                        .rate
                        .map_or(inst, |p| Self::ALPHA * inst + (1.0 - Self::ALPHA) * p);
                    self.rate = Some(r);
                    eta = Some((((100.0 - done) / r).max(0.0)).round() as i64);
                } else if self.no_progress_since.is_none() {
                    self.no_progress_since = Some(now);
                }
            }
        }
        self.last_done = Some(done);
        self.last_at = Some(now);
        eta
    }

    /// True when done% has not advanced for at least `after` (a stuck resync).
    pub fn stalled(&self, now: std::time::Instant, after: std::time::Duration) -> bool {
        self.no_progress_since
            .is_some_and(|t| now.duration_since(t) >= after)
    }
}

/// The `synced` predicate advertised for RPU pacing: fully redundant (both
/// copies UpToDate and Connected, no resync running, and I/O not suspended).
/// This is the condition under which XO may safely reboot the next host. The
/// `!suspended_user` term matters: after a fence-handler `suspend-io` the link
/// can be Connected with both disks UpToDate while I/O is still frozen
/// pool-wide. Data is redundant, but a reboot would prolong that freeze, so
/// synced is not reported until the suspend is lifted.
pub fn is_synced(s: &DrbdStatus) -> bool {
    s.disk == DiskState::UpToDate
        && s.peer_disk == Some(DiskState::UpToDate)
        && s.connection == ConnectionState::Connected
        && s.resync_done.is_none()
        && !s.suspended_user
        // A replication state other than idle/Established is not synced even
        // when both disks read UpToDate: a connection stuck in WFBitMapS
        // replicates one way only, and must not advertise
        // twinstor-synced=true to XO for RPU pacing.
        && matches!(s.replication, None | Some(crate::drbd::Replication::Established))
}

/// Build the full storage-advertisement key set from a DRBD status snapshot, the
/// current ETA (`None` when unknown), and whether the resync is stalled. Always
/// returns the same fixed set of keys (so a diff-publish never leaves a stale
/// key behind, e.g. a `sync-pct` lingering after the resync finished).
pub fn storage_keys(
    s: &DrbdStatus,
    eta_s: Option<i64>,
    stalled: bool,
) -> Vec<(&'static str, String)> {
    let synced = is_synced(s);
    let resyncing = s.resync_done.is_some();
    let state = if synced {
        "synced"
    } else if resyncing {
        if stalled { "stalled" } else { "syncing" }
    } else if s.disk == DiskState::UpToDate {
        // UpToDate locally but the peer is not a redundant copy: serving alone.
        "degraded"
    } else {
        "not-ready"
    };
    // Coarsen pct (integer percent) and eta (floored to 15s) so a sub-percent /
    // per-tick drift does not rewrite the SR/host record on every 5s advertise
    // tick for the whole resync; the diff-publish only fires on a real change.
    let pct = if synced {
        "100".to_string()
    } else if let Some(d) = s.resync_done {
        format!("{:.0}", d.clamp(0.0, 100.0))
    } else {
        "0".to_string()
    };
    let eta = if synced {
        "0".to_string()
    } else if stalled {
        "-1".to_string()
    } else {
        eta_s.map_or_else(|| "-1".to_string(), |e| (e.max(0) / 15 * 15).to_string())
    };
    vec![
        ("twinstor-schema", SCHEMA.to_string()),
        ("twinstor-synced", synced.to_string()),
        ("twinstor-storage-state", state.to_string()),
        ("twinstor-sync-pct", pct),
        ("twinstor-sync-eta", eta),
        ("twinstor-drbd-disk", format!("{:?}", s.disk)),
        (
            "twinstor-drbd-peer",
            s.peer_disk
                .map_or_else(|| "None".to_string(), |d| format!("{d:?}")),
        ),
        ("twinstor-drbd-conn", format!("{:?}", s.connection)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drbd::{Replication, Role};

    fn st(
        disk: DiskState,
        peer: Option<DiskState>,
        conn: ConnectionState,
        done: Option<f64>,
    ) -> DrbdStatus {
        DrbdStatus {
            role: Role::Primary,
            peer_role: Some(Role::Primary),
            connection: conn,
            disk,
            peer_disk: peer,
            suspended_user: false,
            suspended_fencing: false,
            replication: done.map(|_| Replication::SyncTarget),
            resync_done: done,
        }
    }
    fn get<'a>(kv: &'a [(&'static str, String)], k: &str) -> &'a str {
        kv.iter()
            .find(|(kk, _)| *kk == k)
            .map(|(_, v)| v.as_str())
            .unwrap()
    }

    #[test]
    fn synced_when_dual_uptodate_connected_no_resync() {
        let s = st(
            DiskState::UpToDate,
            Some(DiskState::UpToDate),
            ConnectionState::Connected,
            None,
        );
        assert!(is_synced(&s));
        let kv = storage_keys(&s, None, false);
        assert_eq!(get(&kv, "twinstor-synced"), "true");
        assert_eq!(get(&kv, "twinstor-storage-state"), "synced");
        assert_eq!(get(&kv, "twinstor-sync-pct"), "100");
        assert_eq!(get(&kv, "twinstor-sync-eta"), "0");
    }

    #[test]
    fn syncing_reports_pct_and_eta() {
        let s = st(
            DiskState::Inconsistent,
            Some(DiskState::UpToDate),
            ConnectionState::Connected,
            Some(42.5),
        );
        assert!(!is_synced(&s));
        let kv = storage_keys(&s, Some(37), false);
        assert_eq!(get(&kv, "twinstor-synced"), "false");
        assert_eq!(get(&kv, "twinstor-storage-state"), "syncing");
        assert_eq!(get(&kv, "twinstor-sync-pct"), "42"); // integer percent
        assert_eq!(get(&kv, "twinstor-sync-eta"), "30"); // floored to 15s
    }

    #[test]
    fn stalled_resync_reports_minus_one() {
        let s = st(
            DiskState::Inconsistent,
            Some(DiskState::UpToDate),
            ConnectionState::Connected,
            Some(42.5),
        );
        let kv = storage_keys(&s, None, true);
        assert_eq!(get(&kv, "twinstor-storage-state"), "stalled");
        assert_eq!(get(&kv, "twinstor-sync-eta"), "-1");
    }

    #[test]
    fn degraded_when_serving_alone() {
        let s = st(
            DiskState::UpToDate,
            Some(DiskState::Diskless),
            ConnectionState::Connected,
            None,
        );
        let kv = storage_keys(&s, None, false);
        assert_eq!(get(&kv, "twinstor-synced"), "false");
        assert_eq!(get(&kv, "twinstor-storage-state"), "degraded");
    }

    #[test]
    fn eta_projects_from_smoothed_rate() {
        let t0 = std::time::Instant::now();
        let mut e = ResyncEta::default();
        assert_eq!(e.update(Some(50.0), t0), None); // first sample, no rate
        // +10% over 10s is 1%/s; from the current 60%, 40% remains, so ~40s
        let eta = e
            .update(Some(60.0), t0 + std::time::Duration::from_secs(10))
            .unwrap();
        assert!((35..=45).contains(&eta), "eta was {eta}");
    }

    #[test]
    fn no_progress_marks_stalled() {
        let t0 = std::time::Instant::now();
        let mut e = ResyncEta::default();
        e.update(Some(50.0), t0);
        e.update(Some(50.0), t0 + std::time::Duration::from_secs(5)); // no advance
        assert!(e.stalled(
            t0 + std::time::Duration::from_secs(70),
            std::time::Duration::from_secs(60)
        ));
    }

    #[test]
    fn resync_none_resets_estimator() {
        let t0 = std::time::Instant::now();
        let mut e = ResyncEta::default();
        e.update(Some(90.0), t0);
        assert_eq!(e.update(None, t0 + std::time::Duration::from_secs(1)), None);
        // A brand-new resync starts fresh (no stale rate carried over).
        assert_eq!(
            e.update(Some(10.0), t0 + std::time::Duration::from_secs(2)),
            None
        );
    }
}
