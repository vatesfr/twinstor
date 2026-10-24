//! Host-side reboot-safety backstop.
//!
//! `twinstor reboot-drain` runs from a systemd `ExecStop` early in the shutdown
//! transaction (see `config/twinstor-reboot-drain.service`). Its job is a single,
//! narrow safety property:
//!
//!   **Never let a clean reboot take down the last UpToDate replica while the
//!   peer is still resyncing.**
//!
//! During a resync the SyncTarget is Inconsistent (a patchwork, not a usable
//! copy) and, in TwinStor's dual-Primary model, cannot serve, so the UpToDate
//! SyncSource is both the only current copy and the only node serving guest I/O.
//! Rebooting it mid-resync strands the rebuild (no coherent copy until it
//! returns) and drops all guest I/O, with tail-loss risk if it does not come back
//! (a resync often follows a disk replacement).
//!
//! This covers the reboot channels that never call a `twinstor` command: a manual
//! `reboot`, a kernel-update reboot, `yum`, `xe host-reboot`, and XO RPU. It is
//! the local mirror of the daemon's `twinstor-synced` advertisement (which paces
//! RPU at the orchestrator layer). Every clean reboot funnels through the systemd
//! shutdown transaction, so one unit catches them all; only `reboot -f`, panic,
//! power cut, and an XHA hard fence bypass it.
//!
//! Two design rules that shape the code:
//!
//! * **It engages only on the dangerous side.** The safe side (the returning
//!   SyncTarget, or any state where the peer already holds a connected UpToDate
//!   copy) proceeds at once. A healthy pool reboots with one cheap local DRBD
//!   read and no delay.
//! * **It fails open.** This is the opposite of the daemon's VM-wipe gate: a
//!   wedged host is worse than the risk being mitigated, and a mid-resync reboot
//!   is usually recoverable. Any uncertainty in the drain itself (DRBD
//!   unreadable, config missing, an error) proceeds. Only a positively confirmed
//!   "I am the last UpToDate copy and the peer is mid-resync" holds, and only
//!   for a bounded window.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{info, warn};

use crate::config::Config;
use crate::drbd::{
    ConnectionState, DiskState, DrbdControl, DrbdController, DrbdStatus, Replication,
};

/// Operator escape hatch. In `/run` so it is cleared on boot (a one-shot for this
/// shutdown); touching it makes the drain proceed without waiting. Re-checked each
/// poll, so a stuck hold can be released from another shell.
const OVERRIDE_PATH: &str = "/run/twinstor/allow-unsafe-reboot";

/// Breadcrumb the drain leaves for the daemon to pick up on the next boot and turn
/// into an XO alert. In `/var/lib` so it survives the reboot. Written only when the
/// drain proceeded while this host was the last UpToDate copy (the concerning
/// cases); consumed (deleted) once by the daemon at startup.
const BREADCRUMB_PATH: &str = "/var/lib/twinstor/last-reboot-drain";

/// How often to re-read DRBD state while holding.
const POLL: Duration = Duration::from_secs(5);

/// `TimeoutStopSec` of `twinstor-reboot-drain.service`. systemd SIGKILLs the
/// ExecStop at this point regardless of our own cap, so the drain must proceed
/// (and write the breadcrumb) well before it, or the next-boot alert is lost to
/// the SIGKILL. Keep this in sync with the unit file.
const UNIT_TIMEOUT_STOP_SECS: u64 = 660;

/// Headroom below `UNIT_TIMEOUT_STOP_SECS` for our clean cap-proceed: one `POLL`
/// plus the 15s DRBD-read bound plus margin. The effective hold is clamped to
/// `UNIT_TIMEOUT_STOP_SECS - CAP_HEADROOM_SECS`, so a large `max_secs` degrades to
/// a clean, alerted proceed rather than an abrupt SIGKILL. To hold longer,
/// raise both `[reboot_drain] max_secs` and the unit's `TimeoutStopSec`.
const CAP_HEADROOM_SECS: u64 = 45;

/// Consecutive holding polls with no active resync before we stop waiting: a peer
/// that is Connected but not a live SyncTarget (Consistent/Outdated, a paused
/// sync, or a stuck-Inconsistent) will not reach UpToDate by waiting, so holding
/// the full cap just delays the reboot and fires a misleading "no current copy"
/// alert. The grace lets a resync that is merely about to start still count. At
/// `POLL`=5s this is ~15s.
const STALL_GRACE_POLLS: u32 = 3;

/// The effective hold cap: the operator's `max_secs`, clamped so a clean
/// cap-proceed always lands before the unit's `TimeoutStopSec` SIGKILL.
fn effective_cap_secs(max_secs: u64) -> u64 {
    max_secs.min(UNIT_TIMEOUT_STOP_SECS.saturating_sub(CAP_HEADROOM_SECS))
}

/// Whether the peer is a live resync target that waiting will bring to UpToDate:
/// we are actively feeding it (`SyncSource`). A paused sync reports `Other`
/// (`PausedSyncS`), and a stable non-current peer (Consistent/Outdated) reports no
/// replication at all; neither resolves by waiting.
fn is_active_resync(s: &DrbdStatus) -> bool {
    matches!(s.replication, Some(Replication::SyncSource))
}

/// Whether the pool can safely lose this node right now, from a single local DRBD
/// read. Free of I/O and total so it can be exhaustively unit-tested; the loop in
/// `run` does the I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebootSafety {
    /// A complete, current copy survives without this node: proceed at once.
    /// Either the peer is a connected UpToDate replica, or this node is not itself
    /// the current copy (Inconsistent SyncTarget, Diskless, Outdated, …), so
    /// removing it strands nothing.
    Safe,
    /// This node is the last UpToDate copy and the peer is connected but still
    /// catching up (resync in flight). Staying up lets the resync finish before we
    /// remove the only source: hold, bounded.
    HoldForResync,
    /// This node is the last UpToDate copy and the peer is not connected, so it
    /// cannot resync from us while we wait. A hold cannot make the reboot safe:
    /// warn and proceed.
    UnsafeNoPeer,
}

/// The decision, from one `DrbdStatus`. See [`RebootSafety`] for the rationale of
/// each arm. Ordering matters: the "peer holds a current copy" and "I am not the
/// current copy" checks both short-circuit to `Safe` before concluding that the
/// pool depends on this node.
pub fn reboot_safety(s: &DrbdStatus) -> RebootSafety {
    // A connected, UpToDate peer holds a complete current copy: taking this node
    // down removes redundancy but never the last good copy. (Consistent/Outdated
    // are not current, so UpToDate is required.)
    if s.connection == ConnectionState::Connected && s.peer_disk == Some(DiskState::UpToDate) {
        return RebootSafety::Safe;
    }
    // This node is not itself a current copy (Inconsistent SyncTarget, Diskless,
    // Outdated, …): it is not what the pool depends on for currency, so rebooting
    // it strands nothing. This is the safe side: the returning/target node, which
    // can always reboot freely.
    if s.disk != DiskState::UpToDate {
        return RebootSafety::Safe;
    }
    // From here: this node is UpToDate and the peer is not a connected UpToDate
    // copy, so this node is the last current copy in the pool.
    if s.connection == ConnectionState::Connected {
        // Peer present but behind (Inconsistent / mid-resync). Hold so the resync
        // can finish before we remove its only source.
        RebootSafety::HoldForResync
    } else {
        // Peer absent/disconnected: it cannot resync from us while we wait, so a
        // hold cannot help. Proceed with a warning.
        RebootSafety::UnsafeNoPeer
    }
}

/// Outcome recorded in the breadcrumb, so the daemon can word the next-boot alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Cap reached with the peer still resyncing.
    ForcedCap,
    /// Peer not connected (nothing to wait for) while we held the only copy.
    ForcedNoPeer,
    /// Operator override taken while the peer was still catching up.
    OverrideDuringHold,
    /// DRBD became unreadable mid-hold; failed open.
    ReadFailedDuringHold,
    /// Peer stayed Connected but was not actively resyncing (Consistent/Outdated,
    /// a paused sync, or a stuck-Inconsistent); waiting could not have helped.
    NoProgress,
}

impl Outcome {
    fn tag(self) -> &'static str {
        match self {
            Outcome::ForcedCap => "forced-cap",
            Outcome::ForcedNoPeer => "forced-no-peer",
            Outcome::OverrideDuringHold => "override-during-hold",
            Outcome::ReadFailedDuringHold => "read-failed-during-hold",
            Outcome::NoProgress => "no-progress",
        }
    }
}

fn override_present() -> bool {
    Path::new(OVERRIDE_PATH).exists()
}

fn pct(s: &DrbdStatus) -> String {
    match s.resync_done {
        Some(d) => format!("peer resync {d:.1}%"),
        None => "peer not UpToDate".to_string(),
    }
}

/// Best-effort breadcrumb write. Failure is swallowed: a reboot must not be held
/// up because we could not record it, and the alert is a nicety, not a guarantee.
fn write_breadcrumb(outcome: Outcome, done: Option<f64>) {
    let done = done
        .map(|d| format!("{d:.1}"))
        .unwrap_or_else(|| "-".to_string());
    let line = format!("{} {}\n", outcome.tag(), done);
    // Ensure the parent dir exists: at shutdown the daemon may not have run this
    // boot (so /var/lib/twinstor might be absent), and a missing dir would silently
    // drop the next-boot alert.
    if let Some(parent) = Path::new(BREADCRUMB_PATH).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(BREADCRUMB_PATH, line) {
        warn!("reboot-drain: could not write breadcrumb {BREADCRUMB_PATH}: {e}");
    }
}

/// Build the next-boot alert body from the breadcrumb a previous shutdown's drain
/// left (if any). Returns `None` when there is no breadcrumb or it is malformed.
/// Does not delete the file; delivery is confirmed first (see
/// `deliver_pending_reboot_alert`).
fn reboot_alert_body() -> Option<String> {
    let raw = std::fs::read_to_string(BREADCRUMB_PATH).ok()?;
    let mut it = raw.split_whitespace();
    let tag = it.next()?;
    let done = it.next().unwrap_or("-");
    if tag.is_empty() {
        return None;
    }
    let where_at = if done == "-" {
        "the peer was not UpToDate".to_string()
    } else {
        format!("the peer was at {done}% resync")
    };
    let why = match tag {
        "forced-cap" => "the safety hold reached its time cap",
        "forced-no-peer" => "the peer was unreachable (a hold could not have helped)",
        "override-during-hold" => "an operator override was set",
        "read-failed-during-hold" => "DRBD state became unreadable (failed open)",
        "no-progress" => "the peer was connected but not resyncing toward UpToDate",
        _ => "an unexpected reason",
    };
    Some(format!(
        "TwinStor: the last reboot of this host proceeded while it was the last UpToDate \
         replica and {where_at}. The reboot was allowed because {why}. During that window \
         the pool may have had no current copy online and the peer's rebuild was stranded \
         until this host returned. If this reboot was deliberate, no action is needed. \
         Otherwise check that both nodes are now Connected and both UpToDate (twinstor \
         status), and avoid rebooting the UpToDate node again until a resync completes."
    ))
}

/// Deliver the next-boot alert for a previous shutdown's unsafe-proceed, if any.
/// The daemon starts before the toolstack is ready on a fresh boot, so a single
/// startup attempt at `xe message-create` would often be dropped. It therefore
/// retries (backing off) until the toolstack accepts it, and deletes the
/// breadcrumb only after a confirmed send.
/// Spawned once at daemon startup; returns immediately when there is nothing to do.
pub async fn deliver_pending_reboot_alert() {
    let body = match reboot_alert_body() {
        Some(b) => b,
        None => return, // no (valid) breadcrumb
    };
    info!(
        "reboot-drain: a previous reboot proceeded while last UpToDate copy; delivering next-boot alert"
    );
    let mut backoff = Duration::from_secs(5);
    loop {
        if crate::xapi::send_reboot_unsafe_alert(&body).await {
            let _ = std::fs::remove_file(BREADCRUMB_PATH);
            info!("reboot-drain: next-boot alert delivered");
            return;
        }
        warn!("reboot-drain: next-boot alert not yet accepted (toolstack warming up?); will retry");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(120));
    }
}

/// The systemd unit that arms the drain (installed by `install.sh`).
const UNIT_NAME: &str = "twinstor-reboot-drain.service";
const UNIT_PATH: &str = "/usr/lib/systemd/system/twinstor-reboot-drain.service";

/// Whether the reboot-safety guard is armed on this host. Shown by
/// `status`/`doctor` so a guard that is not armed (unit disabled, or a stale
/// unit from a partial upgrade) is noticed before a reboot, not after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardState {
    /// systemd unit enabled and active, and enabled in config: the drain will run.
    Armed,
    /// Turned off: `[reboot_drain] enabled = false` in the config.
    OffByConfig,
    /// Config wants it but the systemd unit is not enabled and active: it will
    /// not run at shutdown until armed.
    NotArmed,
}

impl GuardState {
    /// Short machine tag for `status --json` / `--plain`.
    pub fn tag(self) -> &'static str {
        match self {
            GuardState::Armed => "armed",
            GuardState::OffByConfig => "off",
            GuardState::NotArmed => "not-armed",
        }
    }
}

/// Guard state from the config flag and the unit's enabled/active state.
/// Config-off wins (an explicit opt-out is not a misconfiguration to warn about).
pub fn guard_state(config_enabled: bool, unit_enabled: bool, unit_active: bool) -> GuardState {
    if !config_enabled {
        GuardState::OffByConfig
    } else if unit_enabled && unit_active {
        GuardState::Armed
    } else {
        GuardState::NotArmed
    }
}

/// One `systemctl is-enabled|is-active` state string, tolerant of the non-zero
/// exit these return for disabled/inactive (they still print the state). Empty on
/// timeout or spawn failure, read as "not that state".
async fn systemctl_state(verb: &str) -> String {
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("systemctl")
            .arg(verb)
            .arg(UNIT_NAME)
            .kill_on_drop(true)
            .output(),
    )
    .await;
    match out {
        Ok(Ok(o)) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    }
}

/// The live guard state on this host (reads systemd and the config flag).
pub async fn reboot_guard_state(config: &Config) -> GuardState {
    let enabled = systemctl_state("is-enabled").await == "enabled";
    let active = systemctl_state("is-active").await == "active";
    guard_state(config.reboot_drain.enabled, enabled, active)
}

/// Whether the installed unit file is the current one (for `doctor`): it must
/// not carry an active `DefaultDependencies=no` (which stops the ExecStop from
/// firing on a real reboot) and must order `After=...xapi.service`. `None` if
/// the unit file is absent. Catches a stale unit left by a partial upgrade that
/// would do nothing at shutdown. The
/// explanatory comment in the shipped unit that mentions "DefaultDependencies=no"
/// is a comment line (starts with `#`), so it does not match the directive check.
pub fn reboot_guard_unit_ok() -> Option<bool> {
    let content = std::fs::read_to_string(UNIT_PATH).ok()?;
    let has_default_deps_no = content.lines().any(|l| {
        let squashed: String = l.split_whitespace().collect();
        squashed
            .to_lowercase()
            .starts_with("defaultdependencies=no")
    });
    let after_xapi = content.lines().any(|l| {
        let t = l.trim();
        t.starts_with("After=") && t.contains("xapi.service")
    });
    Some(!has_default_deps_no && after_xapi)
}

/// Entry point for the `reboot-drain` subcommand. Reads DRBD once, decides, and,
/// only in the `HoldForResync` case, waits (bounded) for the peer to reach
/// UpToDate, an override, or the cap. Always returns `Ok(())`: the caller is a
/// systemd `ExecStop`, and letting the reboot proceed is the fail-open default.
pub async fn run(config: &Config) -> Result<()> {
    if !config.reboot_drain.enabled {
        info!("reboot-drain: disabled by config; proceeding");
        return Ok(());
    }
    if override_present() {
        // Operator escape taken before the drain looked at DRBD: proceed without a
        // wait and without a breadcrumb (we never determined it was unsafe).
        warn!(
            "reboot-drain: operator override present ({OVERRIDE_PATH}); proceeding without a storage-safety wait"
        );
        return Ok(());
    }

    let drbd = DrbdController::new(config);
    // Fail open: any trouble reading DRBD must not wedge a reboot.
    let status = match drbd.status().await {
        Ok(s) => s,
        Err(e) => {
            warn!("reboot-drain: could not read DRBD status ({e}); proceeding (fail-open)");
            return Ok(());
        }
    };

    match reboot_safety(&status) {
        RebootSafety::Safe => {
            info!(
                "reboot-drain: a current replica survives this host (peer UpToDate, or this node is not the current copy); safe to reboot"
            );
            Ok(())
        }
        RebootSafety::UnsafeNoPeer => {
            // The peer's disk is not visible while disconnected, so do not claim it
            // holds no copy, only that no current copy is confirmed reachable.
            warn!(
                "reboot-drain: this host holds an UpToDate replica and the peer is not connected; \
                 rebooting may leave no reachable current copy online, and a wait cannot help \
                 while the peer is unreachable. Proceeding."
            );
            write_breadcrumb(Outcome::ForcedNoPeer, status.resync_done);
            Ok(())
        }
        RebootSafety::HoldForResync => {
            hold_for_resync(&drbd, &status, config.reboot_drain.max_secs).await
        }
    }
}

/// The bounded wait. Precondition: `initial` was `HoldForResync`.
async fn hold_for_resync(drbd: &DrbdController, initial: &DrbdStatus, max_secs: u64) -> Result<()> {
    let cap_secs = effective_cap_secs(max_secs);
    if cap_secs < max_secs {
        warn!(
            "reboot-drain: configured max_secs={max_secs} exceeds the safe ceiling; clamping to \
             {cap_secs}s to cap-proceed cleanly before the unit's TimeoutStopSec={UNIT_TIMEOUT_STOP_SECS}. \
             To hold genuinely longer, also raise TimeoutStopSec in twinstor-reboot-drain.service."
        );
    }
    let cap = Duration::from_secs(cap_secs);
    let start = Instant::now();
    warn!(
        "reboot-drain: holding this host's reboot: it holds the only UpToDate replica and the \
         peer is still resyncing ({}). Rebooting now would drop all guest I/O and strand the \
         rebuild. Waiting up to {cap_secs}s for the peer to become UpToDate. Override: touch {OVERRIDE_PATH}",
        pct(initial)
    );

    // Consecutive holding polls with no active resync; see STALL_GRACE_POLLS.
    let mut stall_polls: u32 = 0;
    loop {
        // Evaluate before sleeping: max_secs=0 must proceed at once, and a fast
        // peer transition to UpToDate should be caught without a POLL of lag.
        if override_present() {
            warn!(
                "reboot-drain: override appeared ({OVERRIDE_PATH}); proceeding while the peer is still catching up"
            );
            write_breadcrumb(Outcome::OverrideDuringHold, None);
            return Ok(());
        }

        let s = match drbd.status().await {
            Ok(s) => s,
            Err(e) => {
                // Fail open even mid-hold: a wedged read must not turn a bounded
                // hold into a wedged host.
                warn!("reboot-drain: DRBD read failed mid-hold ({e}); proceeding (fail-open)");
                write_breadcrumb(Outcome::ReadFailedDuringHold, None);
                return Ok(());
            }
        };

        match reboot_safety(&s) {
            RebootSafety::Safe => {
                info!(
                    "reboot-drain: peer reached UpToDate after {}s; a full replica now survives; proceeding",
                    start.elapsed().as_secs()
                );
                return Ok(());
            }
            RebootSafety::UnsafeNoPeer => {
                warn!(
                    "reboot-drain: peer connection dropped mid-hold; it can no longer resync from us. Proceeding (a wait cannot help)."
                );
                write_breadcrumb(Outcome::ForcedNoPeer, s.resync_done);
                return Ok(());
            }
            RebootSafety::HoldForResync => {
                if start.elapsed() >= cap {
                    warn!(
                        "reboot-drain: cap of {cap_secs}s reached with the peer still resyncing ({}); \
                         proceeding. Guest I/O will pause and the rebuild is stranded until this host returns.",
                        pct(&s)
                    );
                    write_breadcrumb(Outcome::ForcedCap, s.resync_done);
                    return Ok(());
                }
                // Stall detection: hold only while the peer is a live resync target
                // (we are its SyncSource). A Connected-but-not-syncing peer
                // (Consistent/Outdated, a paused sync, a stuck-Inconsistent) will not
                // reach UpToDate by waiting, so after a short grace stop waiting
                // rather than use the whole cap and fire a misleading alert. Using
                // the SyncSource flag (not resync %) keeps a slow resync held: its %
                // may not move within a poll, but the flag stays set.
                if is_active_resync(&s) {
                    stall_polls = 0;
                    info!(
                        "reboot-drain: still holding: {} ({}s / {cap_secs}s)",
                        pct(&s),
                        start.elapsed().as_secs()
                    );
                } else {
                    stall_polls += 1;
                    if stall_polls >= STALL_GRACE_POLLS {
                        warn!(
                            "reboot-drain: peer is Connected but not resyncing toward UpToDate (a stale, \
                             paused, or stuck copy); a wait cannot help. Proceeding after {}s.",
                            start.elapsed().as_secs()
                        );
                        write_breadcrumb(Outcome::NoProgress, s.resync_done);
                        return Ok(());
                    }
                    info!(
                        "reboot-drain: holding, peer Connected but no active resync yet ({stall_polls}/{STALL_GRACE_POLLS} grace polls)"
                    );
                }
            }
        }

        tokio::time::sleep(POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drbd::{Replication, Role};

    /// Build a DrbdStatus with the fields the decision reads; the rest are inert.
    fn st(
        disk: DiskState,
        connection: ConnectionState,
        peer_disk: Option<DiskState>,
        replication: Option<Replication>,
        resync_done: Option<f64>,
    ) -> DrbdStatus {
        DrbdStatus {
            role: Role::Primary,
            peer_role: Some(Role::Primary),
            connection,
            disk,
            peer_disk,
            suspended_user: false,
            suspended_fencing: false,
            replication,
            resync_done,
        }
    }

    #[test]
    fn healthy_dual_uptodate_is_safe() {
        // Both UpToDate, Connected: either node can reboot, peer carries on.
        let s = st(
            DiskState::UpToDate,
            ConnectionState::Connected,
            Some(DiskState::UpToDate),
            None,
            None,
        );
        assert_eq!(reboot_safety(&s), RebootSafety::Safe);
    }

    #[test]
    fn i_am_the_synctarget_is_safe() {
        // I'm Inconsistent and catching up from the peer: rebooting me strands
        // nothing (the source stays UpToDate). The safe side.
        let s = st(
            DiskState::Inconsistent,
            ConnectionState::Connected,
            Some(DiskState::UpToDate),
            Some(Replication::SyncTarget),
            Some(42.0),
        );
        assert_eq!(reboot_safety(&s), RebootSafety::Safe);
    }

    #[test]
    fn i_am_the_syncsource_with_inconsistent_peer_holds() {
        // The dangerous case the drain exists for: I'm UpToDate and the only
        // current copy, peer is Connected but Inconsistent (mid-resync).
        let s = st(
            DiskState::UpToDate,
            ConnectionState::Connected,
            Some(DiskState::Inconsistent),
            Some(Replication::SyncSource),
            Some(42.0),
        );
        assert_eq!(reboot_safety(&s), RebootSafety::HoldForResync);
    }

    #[test]
    fn last_uptodate_copy_with_peer_gone_is_unsafe_no_peer() {
        // I'm the only UpToDate copy and the peer is not connected: a hold can't
        // make it safe (nothing to wait for), so proceed with a warning.
        for conn in [
            ConnectionState::StandAlone,
            ConnectionState::Connecting,
            ConnectionState::Unknown,
            ConnectionState::Disconnecting,
        ] {
            let s = st(
                DiskState::UpToDate,
                conn,
                Some(DiskState::Inconsistent),
                None,
                None,
            );
            assert_eq!(
                reboot_safety(&s),
                RebootSafety::UnsafeNoPeer,
                "conn={conn:?}"
            );
        }
    }

    #[test]
    fn peer_gone_but_peer_disk_unknown_still_unsafe_no_peer() {
        // Peer entirely absent (peer_disk None), I'm UpToDate: last copy, nothing
        // to wait for.
        let s = st(
            DiskState::UpToDate,
            ConnectionState::StandAlone,
            None,
            None,
            None,
        );
        assert_eq!(reboot_safety(&s), RebootSafety::UnsafeNoPeer);
    }

    #[test]
    fn consistent_peer_is_not_a_current_copy() {
        // A Consistent (not UpToDate) peer is not a safe fallback: if I'm the
        // UpToDate one and connected, I still hold.
        let s = st(
            DiskState::UpToDate,
            ConnectionState::Connected,
            Some(DiskState::Consistent),
            None,
            None,
        );
        assert_eq!(reboot_safety(&s), RebootSafety::HoldForResync);
    }

    #[test]
    fn diskless_local_is_safe_regardless_of_peer() {
        // I have no local disk at all: I'm not the current copy, rebooting me
        // strands nothing, whatever the peer looks like.
        let s = st(
            DiskState::Diskless,
            ConnectionState::Connected,
            Some(DiskState::Inconsistent),
            None,
            None,
        );
        assert_eq!(reboot_safety(&s), RebootSafety::Safe);
        let s = st(
            DiskState::Diskless,
            ConnectionState::StandAlone,
            None,
            None,
            None,
        );
        assert_eq!(reboot_safety(&s), RebootSafety::Safe);
    }

    #[test]
    fn both_inconsistent_is_safe_nothing_to_protect() {
        // Neither side is a current copy: there is no last-good-copy to protect and
        // waiting cannot create one, so the drain must not hold.
        let s = st(
            DiskState::Inconsistent,
            ConnectionState::Connected,
            Some(DiskState::Inconsistent),
            None,
            None,
        );
        assert_eq!(reboot_safety(&s), RebootSafety::Safe);
    }

    #[test]
    fn is_active_resync_only_when_we_are_syncsource() {
        // Live resync (we feed the peer): hold-worthy.
        let s = st(
            DiskState::UpToDate,
            ConnectionState::Connected,
            Some(DiskState::Inconsistent),
            Some(Replication::SyncSource),
            Some(3.0),
        );
        assert!(is_active_resync(&s));
        // Paused sync (PausedSyncS parses to Other), stable non-current peer (no
        // replication line), and Established/SyncTarget from our side are not a
        // resync we are driving to UpToDate: not hold-worthy past the grace.
        for repl in [
            None,
            Some(Replication::Other),
            Some(Replication::Established),
            Some(Replication::SyncTarget),
        ] {
            let s = st(
                DiskState::UpToDate,
                ConnectionState::Connected,
                Some(DiskState::Consistent),
                repl,
                None,
            );
            assert!(!is_active_resync(&s), "repl={repl:?}");
        }
    }

    #[test]
    fn guard_state_matrix() {
        use GuardState::*;
        // config off wins regardless of unit state (an opt-out, not a warning).
        assert_eq!(guard_state(false, true, true), OffByConfig);
        assert_eq!(guard_state(false, false, false), OffByConfig);
        // config on: armed only when the unit is both enabled and active.
        assert_eq!(guard_state(true, true, true), Armed);
        assert_eq!(guard_state(true, true, false), NotArmed); // enabled, not active
        assert_eq!(guard_state(true, false, true), NotArmed); // active, not enabled
        assert_eq!(guard_state(true, false, false), NotArmed);
        assert_eq!(Armed.tag(), "armed");
        assert_eq!(OffByConfig.tag(), "off");
        assert_eq!(NotArmed.tag(), "not-armed");
    }

    #[test]
    fn effective_cap_clamps_above_ceiling_but_passes_normal_values() {
        let ceiling = UNIT_TIMEOUT_STOP_SECS - CAP_HEADROOM_SECS;
        assert_eq!(effective_cap_secs(0), 0); // "don't wait" is preserved
        assert_eq!(effective_cap_secs(600), 600); // the default is under the ceiling
        assert_eq!(effective_cap_secs(ceiling), ceiling);
        assert_eq!(effective_cap_secs(900), ceiling); // an over-large value is clamped
        assert_eq!(effective_cap_secs(u64::MAX), ceiling);
    }
}
