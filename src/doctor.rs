//! `twinstor doctor`: diagnose the two post-incident "toolstack knots" that
//! block VM starts while the storage layer is healthy: an HA-state
//! disagreement ("pool has HA enabled but this server has HA disabled") and
//! stale VDI `activating` locks (see FAQ, "Storage is healthy but VMs will not
//! start"). Both are mechanical to clear once diagnosed, but clearing them by
//! hand mid-incident is dangerous.
//!
//! `doctor` defers to auto-heal: it reports the safety gates and, with `--fix`,
//! changes something only when they pass and the operator confirms, so a
//! hurried run cannot interrupt an in-progress heal that was about to finish.
//! Without `--fix` it is read-only: it checks the gates by observation and
//! tells the operator whether a fix would be safe.

use crate::config::Config;
use crate::drbd::{ConnectionState, DiskState, DrbdControl, DrbdController, DrbdStatus};
use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Duration;

/// Gate 3. Before clearing a knot the daemon might still resolve, watch it for
/// this long and confirm it does not change; if it is moving, auto-heal is on it
/// and doctor stands down. Short enough for an interactive session, long enough
/// to catch an in-progress sweep.
const STABLE_SECS: u64 = 20;

/// Gate 1. Storage counts as quiescent (a toolstack-only knot, not a storage
/// heal) only when the pair is connected, both UpToDate, and not resyncing. If
/// storage is mid-heal the daemon is busy and manual toolstack changes must not
/// happen; this is the strongest gate, and it is free to check.
fn storage_quiescent(s: &DrbdStatus) -> bool {
    s.connection == ConnectionState::Connected
        && s.disk == DiskState::UpToDate
        && s.peer_disk == Some(DiskState::UpToDate)
        && s.replication.is_none()
        && s.resync_done.is_none()
}

/// Knot 1. The HA-state disagreement: the pool has HA enabled but this host
/// reads its local HA as disarmed (which blocks its xapi join, static-VDI
/// attach, and `xhad`). `None` on either side is unknown, never a knot: the
/// diagnosis must not invent one from an unreadable value.
fn ha_disagreement(local_armed: Option<bool>, pool_ha: Option<bool>) -> bool {
    local_armed == Some(false) && pool_ha == Some(true)
}

/// Everything the diagnosis gathered, once, read-only.
struct Diagnosis {
    storage: Option<DrbdStatus>, // None = drbdadm status unavailable
    heal: Option<crate::heal_state::HealState>, // Some = an auto-heal episode is active
    daemon_active: bool,
    pool_ha: Option<bool>,
    local_armed: Option<bool>,
    activating: Vec<String>,      // bare (host_-less) 'activating' VDI uuids
    sr_uuid: Option<String>,      // resolved twinstor SR uuid (None = unresolved)
    sr_recorded: bool, // true = from the recorded state file, false = name-label fallback
    sr_exists: bool,   // the resolved uuid actually resolves to an SR in XAPI
    peer_version: Option<String>, // peer daemon's advertised version (None = unreachable/unknown)
    reboot_guard: crate::reboot_drain::GuardState, // reboot-safety guard arming
    reboot_guard_unit_ok: Option<bool>, // installed unit is the correct one (None = unit file absent)
    arbitrated: bool,
    ha_timeout: Option<u32>, // pool ha-config:timeout, else the effective xhad.conf value
    // pool ha-reboot-vm-on-internal-shutdown and twinstor's record (None = unreadable / no such field)
    internal_shutdown: Option<(bool, Option<bool>)>,
    freeze_engaged: bool,
}

impl Diagnosis {
    fn ha_disagreement(&self) -> bool {
        ha_disagreement(self.local_armed, self.pool_ha)
    }
    fn any_knot(&self) -> bool {
        self.ha_disagreement() || !self.activating.is_empty()
    }
    fn quiescent(&self) -> Option<bool> {
        self.storage.as_ref().map(storage_quiescent)
    }
    /// The hard blocker (gate 1 or gate 2), if any, that makes a fix unsafe right
    /// now regardless of confirmation. `None` = the pool is in the safe state.
    fn fix_block_reason(&self) -> Option<String> {
        match self.quiescent() {
            Some(false) => {
                return Some(
                    "storage is still healing, so this is not a pure toolstack knot (gate 1)".into(),
                );
            }
            None => {
                return Some(
                    "storage state is unknown; drbdadm status did not answer (gate 1)".into(),
                );
            }
            Some(true) => {}
        }
        if let Some(h) = &self.heal {
            return Some(format!(
                "an auto-heal episode is in progress: {}, {} (gate 2)",
                h.stage, h.summary
            ));
        }
        None
    }
}

async fn daemon_active() -> bool {
    tokio::process::Command::new("systemctl")
        .args(["is-active", "--quiet", "twinstor"])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn diagnose(config: &Config) -> Diagnosis {
    let drbd = DrbdController::new(config);
    let storage = drbd.status().await.ok();
    let heal = crate::heal_state::read();
    let daemon = daemon_active().await;
    let pool_ha = crate::xapi::is_ha_enabled().await;
    let local_armed = crate::xapi::local_ha_armed();
    let sr_uuid = crate::xapi::twinstor_sr_uuid().await;
    // Was it resolved from the recorded state file, or the name-label fallback?
    let sr_recorded = crate::xapi::read_recorded_sr_uuid().is_some();
    // Verify the resolved uuid still names a real SR (a stale recorded uuid resolves
    // here but does not exist; don't print it as if it's live).
    let sr_exists = match &sr_uuid {
        Some(u) => crate::xapi::sr_uuid_exists(u).await,
        None => false,
    };
    let activating = match &sr_uuid {
        Some(sr) if sr_exists => crate::xapi::bare_activating_vdis(sr)
            .await
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let (_, _, peer_version) = crate::xapi::peer_daemon_alive(&config.node.peer).await;
    let reboot_guard = crate::reboot_drain::reboot_guard_state(config).await;
    let reboot_guard_unit_ok = crate::reboot_drain::reboot_guard_unit_ok();
    let arbitrated = is_arbitrated().await;
    let ha_timeout = match crate::xapi::pool_ha_timeout().await {
        Some(t) => Some(t),
        None => crate::setup::effective_xha_timeout(),
    };
    let internal_shutdown = crate::xapi::pool_internal_shutdown_policy().await;
    // Freeze mode engaged for the pair, by the daemon's own rule.
    let freeze_engaged = crate::state_machine::fs_freeze_engaged(
        arbitrated,
        &crate::state_machine::fs_freeze_off_marker(),
        &crate::state_machine::peer_fs_freeze_record(),
    );
    Diagnosis {
        storage,
        heal,
        daemon_active: daemon,
        pool_ha,
        local_armed,
        activating,
        sr_uuid,
        sr_recorded,
        sr_exists,
        peer_version,
        reboot_guard,
        reboot_guard_unit_ok,
        arbitrated,
        ha_timeout,
        internal_shutdown,
        freeze_engaged,
    }
}

// Plain, portable status glyphs (doctor output must read on a wedged dom0).
const OK: &str = "[ ok ]";
const BAD: &str = "[FAIL]";
const WARN: &str = "[warn]";
fn knot_mark(present: bool) -> &'static str {
    if present { BAD } else { OK }
}

fn ago(started_unix: u64) -> String {
    let secs = crate::heal_state::unix_now().saturating_sub(started_unix);
    if secs < 90 {
        format!("{secs}s ago")
    } else {
        format!("{}m ago", secs / 60)
    }
}

fn print_report(d: &Diagnosis) {
    println!("twinstor doctor: post-incident toolstack check\n");

    match &d.sr_uuid {
        Some(sr) => {
            let src = if d.sr_recorded {
                "recorded"
            } else {
                "name-label fallback"
            };
            if d.sr_exists {
                println!("twinstor SR: {sr} ({src})\n");
            } else {
                println!(
                    "twinstor SR: {sr} ({src}, but not found in XAPI, so it is stale; run `twinstor sr record`)\n"
                );
            }
        }
        None => println!("twinstor SR: unresolved (no recorded uuid, no name-label match)\n"),
    }

    let local_ver = env!("CARGO_PKG_VERSION");
    match &d.peer_version {
        Some(pv) if pv == local_ver => println!("versions:    local {local_ver}, peer {pv}\n"),
        Some(pv) => match crate::xapi::mixed_release_note(local_ver, pv) {
            Some(note) => println!(
                "versions:    local {local_ver}, peer {pv} (mixed releases: the peer runs {note})\n"
            ),
            None => println!(
                "versions:    local {local_ver}, peer {pv} (version skew: finish the rolling upgrade; both nodes should be on {local_ver})\n"
            ),
        },
        None => {
            println!("versions:    local {local_ver}, peer unknown (peer daemon unreachable)\n")
        }
    }

    // Reboot-safety guard: arming, and whether the installed unit is the correct one
    // (a stale unit from a partial upgrade can look "armed" yet not fire at reboot).
    use crate::reboot_drain::GuardState;
    match d.reboot_guard {
        GuardState::Armed if d.reboot_guard_unit_ok == Some(false) => println!(
            "reboot guard: armed, but the installed unit looks stale (missing After=xapi.service \
             or has DefaultDependencies=no), so it may not fire on a real reboot. Reinstall from \
             the current bundle (./install.sh).\n"
        ),
        GuardState::Armed => println!("reboot guard: armed\n"),
        GuardState::OffByConfig => {
            println!("reboot guard: off (disabled in config; the drain proceeds immediately)\n")
        }
        GuardState::NotArmed => match d.reboot_guard_unit_ok {
            None => println!(
                "reboot guard: not installed; reinstall from the current bundle (./install.sh) to arm it.\n"
            ),
            Some(_) => println!(
                "reboot guard: not armed; arm it with: systemctl enable --now twinstor-reboot-drain.service\n"
            ),
        },
    }

    // HA timeout advice: a fate-shared split that does not heal within the XHA
    // timeout gets one host rebooted by XHA (see setup::ha_timeout_secs).
    if d.pool_ha == Some(true) {
        let recommended = crate::setup::ha_timeout_secs(d.arbitrated);
        match d.ha_timeout {
            Some(t) if t < recommended => println!(
                "HA timeout:  {t}s; {recommended}s recommended for this mode: a replication split \
                 that heals within the timeout does not get a host rebooted by XHA (cost: a \
                 slower VM failover on a real host death). In a maintenance window, from the \
                 pool master: xe pool-ha-disable; xe pool-ha-enable heartbeat-sr-uuids={} \
                 ha-config:timeout={recommended}\n",
                d.sr_uuid.as_deref().unwrap_or("<twinstor SR uuid>")
            ),
            Some(t) => println!("HA timeout:  {t}s\n"),
            None => println!("HA timeout:  unknown (could not read it)\n"),
        }
    }

    // Fate-shared freeze mode wants the pool's ha-reboot-vm-on-internal-shutdown
    // off; the arbitrated stand-downs want it on (see
    // state_machine::internal_shutdown_action).
    if d.freeze_engaged {
        match d.internal_shutdown {
            Some((false, _)) => println!(
                "internal shutdown: pool HA does not restart a VM shut down from inside (ha-reboot-vm-on-internal-shutdown=false, as freeze mode wants)\n"
            ),
            Some((true, Some(true))) => println!(
                "internal shutdown: {WARN} pool ha-reboot-vm-on-internal-shutdown is on again (twinstor turned it off once and \
                 does not change it twice). With it on, guests a losing pool master stops can be restarted by HA on that \
                 host and left halted. To turn it off: xe pool-param-set uuid=<pool> ha-reboot-vm-on-internal-shutdown=false\n"
            ),
            Some((true, _)) => println!(
                "internal shutdown: {WARN} pool ha-reboot-vm-on-internal-shutdown is on; the pool master's daemon turns it off \
                 once (on the master, a minute after start). With it on, guests a losing pool master stops can be restarted \
                 by HA on that host and left halted.\n"
            ),
            None => println!(
                "internal shutdown: unknown (XAPI did not answer, or has no such setting)\n"
            ),
        }
    } else if d.arbitrated && matches!(d.internal_shutdown, Some((false, _))) {
        println!(
            "internal shutdown: {WARN} pool ha-reboot-vm-on-internal-shutdown is off on an arbitrated pool: the isolated \
             stand-downs stop their guests at the hypervisor and rely on pool HA to restart them on the peer, which it \
             then does not. Turn it on: xe pool-param-set uuid=<pool> ha-reboot-vm-on-internal-shutdown=true\n"
        );
    }

    println!("Knots (these block VM starts while storage is healthy):");
    let ha = d.ha_disagreement();
    println!(
        "  {} HA-state disagreement (pool HA on, this host disarmed)",
        knot_mark(ha)
    );
    if ha {
        println!(
            "       pool ha-enabled={:?}, this host ha-armed={:?}",
            d.pool_ha, d.local_armed
        );
    }
    let n = d.activating.len();
    println!(
        "  {} stale VDI 'activating' locks: {}",
        knot_mark(n > 0),
        if n > 0 {
            format!("{n} found")
        } else {
            "none".to_string()
        }
    );
    for u in &d.activating {
        println!("       {u}");
    }

    println!("\nSafety gates (a fix runs only when all pass):");
    match d.quiescent() {
        Some(true) => {
            println!("  {OK} storage quiescent (Connected, UpToDate/UpToDate, no resync)")
        }
        Some(false) => {
            println!("  {BAD} storage is not quiescent: it is healing, not a toolstack knot")
        }
        None => println!("  {WARN} storage state unknown (drbdadm status did not answer)"),
    }
    match &d.heal {
        None => println!("  {OK} no auto-heal episode in progress"),
        Some(h) => println!(
            "  {BAD} auto-heal in progress: {}, {} (since {})",
            h.stage,
            h.summary,
            ago(h.started_unix)
        ),
    }
    if d.daemon_active {
        println!("  {OK} daemon running: it clears these on its own; prefer letting it");
    } else {
        println!(
            "  {WARN} daemon not running (masked/stopped): auto-heal will not act; this is the fallback case"
        );
    }

    println!();
    if !d.any_knot() {
        println!("Verdict: no toolstack knot detected. Nothing to do.");
        return;
    }
    match d.fix_block_reason() {
        Some(reason) => {
            println!("Verdict: a knot is present, but a fix is not safe right now:");
            println!("         {reason}.");
            println!("         Let the automatic path run and re-check with `twinstor doctor`.");
        }
        None => {
            println!(
                "Verdict: a toolstack knot is present and the pool is in the safe state to clear it."
            );
            if d.daemon_active {
                println!(
                    "         The daemon is running and normally clears these itself; give it a few"
                );
                println!(
                    "         minutes first. If it persists, `twinstor doctor --fix` clears it (with confirmation)."
                );
            } else {
                println!(
                    "         The daemon is not running, so `twinstor doctor --fix` is the way to clear it (with confirmation)."
                );
            }
        }
    }
}

/// `twinstor doctor`: diagnose the post-incident toolstack knots and, with
/// `fix`, clear them behind the safety gates (confirm-first).
pub async fn run(config: Arc<Config>, fix: bool) -> Result<()> {
    let d = diagnose(&config).await;
    print_report(&d);
    if fix {
        fix_knots(&config, &d).await?;
    }
    Ok(())
}

async fn recheck_activating() -> Vec<String> {
    match crate::xapi::twinstor_sr_uuid().await {
        Some(sr) => crate::xapi::bare_activating_vdis(&sr)
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    }
}

/// Arbitrated pools carry a direct-link peer file; fate-shared do not. The HA
/// timeout differs (`setup::ha_timeout_secs`: 90 arbitrated / 120 fate-shared).
/// The file's existence decides, the same way the daemon decides its mode, so
/// doctor never judges a pool by the other mode's rules.
async fn is_arbitrated() -> bool {
    tokio::fs::try_exists(crate::setup::DIRECT_PEER_PATH)
        .await
        .unwrap_or(false)
}

async fn fix_knots(config: &Config, d: &Diagnosis) -> Result<()> {
    if !d.any_knot() {
        println!("\nNothing to fix.");
        return Ok(());
    }
    // Gate 4 precondition: confirmation requires a terminal. --fix is not
    // scriptable on purpose; the human check is part of the safety.
    if !crate::wizard::stdin_is_tty() {
        anyhow::bail!(
            "--fix needs an interactive terminal so you can confirm each action; \
             it is intentionally not scriptable. Run it directly, or follow the \
             manual runbook (FAQ: \"Storage is healthy but VMs will not start\")."
        );
    }
    // Gates 1 and 2: hard blockers, independent of any confirmation.
    if let Some(reason) = d.fix_block_reason() {
        println!("\nRefusing to fix: {reason}.");
        println!("Let the automatic path run and re-check with `twinstor doctor`.");
        return Ok(());
    }

    let mut acted = false;
    // Knot 2 first (cheap, common, daemon-owned): stale activating locks.
    if !d.activating.is_empty() {
        acted |= fix_activating_locks(d).await?;
    }
    // Knot 1: the HA-state disagreement, which the daemon does not touch.
    if d.ha_disagreement() {
        acted |= fix_ha_disagreement().await?;
    }

    if acted {
        println!("\nDone. Re-run `twinstor doctor` to confirm it is clear, then start your VMs.");
    } else {
        println!("\nNo changes made.");
    }
    let _ = config; // reserved for future per-knot config needs
    Ok(())
}

async fn fix_activating_locks(d: &Diagnosis) -> Result<bool> {
    println!(
        "\n-- stale VDI 'activating' locks: {} found --",
        d.activating.len()
    );
    // Gates 5 and 3: the running daemon sweeps these itself. Watch first, so we
    // never race a sweep that is already underway.
    if d.daemon_active {
        println!(
            "The daemon is running and clears these on its own. Watching for {STABLE_SECS}s to \
             be sure it is not already doing so..."
        );
        tokio::time::sleep(Duration::from_secs(STABLE_SECS)).await;
        let now = recheck_activating().await;
        if now.len() < d.activating.len() {
            println!(
                "The daemon is clearing them ({} -> {}). Letting it finish; re-check with \
                 `twinstor doctor`.",
                d.activating.len(),
                now.len()
            );
            return Ok(false);
        }
        println!(
            "Unchanged after {STABLE_SECS}s: the daemon is not clearing them; safe to step in."
        );
    }
    if !crate::wizard::ask_yes_no(
        &format!(
            "Clear {} stale 'activating' lock(s) via XenAPI now? (only the bare, half-acquired locks)",
            d.activating.len()
        ),
        false,
    )? {
        println!("Skipped.");
        return Ok(false);
    }
    let mut cleared = 0usize;
    for u in &d.activating {
        if crate::xapi::clear_activating_lock(u).await {
            println!("  cleared {u}");
            cleared += 1;
        } else {
            println!("  Failed to clear {u} (it may have resolved, or needs manual attention)");
        }
    }
    println!("Cleared {cleared}/{} lock(s).", d.activating.len());
    Ok(cleared > 0)
}

async fn fix_ha_disagreement() -> Result<bool> {
    println!("\n-- HA-state disagreement (pool HA on, this host disarmed) --");
    println!(
        "The daemon does not auto-fix this (cycling pool HA is disruptive and pool-wide). \
         The reset must run from the pool master; a host-level toolstack restart does not clear it."
    );
    let sr = crate::xapi::twinstor_sr_uuid()
        .await
        .context("cannot find the twinstor SR to re-enable HA against")?;
    // Keep the pool's configured timeout; a pool without one gets the mode's.
    let timeout = match crate::xapi::pool_ha_timeout().await {
        Some(t) => t,
        None => crate::setup::ha_timeout_secs(is_arbitrated().await),
    };

    let on_master = crate::xapi::is_pool_master_resilient().await == Some(true);
    if !on_master {
        println!(
            "Note: this host is not the pool master. The reset proxies to the master via xe, \
             which usually works, but if this host's xe is in emergency mode it will fail; \
             then run the two commands below on the master."
        );
    }
    println!("Plan (issued against the pool master):");
    println!("  xe pool-ha-disable");
    println!("  xe pool-ha-enable heartbeat-sr-uuids={sr} ha-config:timeout={timeout}");
    if !crate::wizard::ask_yes_no(
        "Reset pool HA now? VMs keep running; HA protection drops for the few seconds of the cycle",
        false,
    )? {
        println!("Skipped.");
        return Ok(false);
    }

    if let Err(e) = crate::cmd::run_cmd_for("xe", &["pool-ha-disable"], 90).await {
        println!("  pool-ha-disable failed: {e}");
        println!(
            "  Run the two commands above on the pool master instead (this host's xe is likely in emergency mode)."
        );
        return Ok(false);
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    if let Err(e) = crate::cmd::run_cmd_for(
        "xe",
        &[
            "pool-ha-enable",
            &format!("heartbeat-sr-uuids={sr}"),
            &format!("ha-config:timeout={timeout}"),
        ],
        120,
    )
    .await
    {
        println!("  pool-ha-enable failed: {e}");
        println!("  HA is now disabled pool-wide. Re-enable it from the master:");
        println!("    xe pool-ha-enable heartbeat-sr-uuids={sr} ha-config:timeout={timeout}");
        return Ok(false);
    }
    println!("Pool HA reset issued (timeout {timeout}s).");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drbd::{Replication, Role};

    fn healthy() -> DrbdStatus {
        DrbdStatus {
            role: Role::Primary,
            peer_role: Some(Role::Primary),
            connection: ConnectionState::Connected,
            disk: DiskState::UpToDate,
            peer_disk: Some(DiskState::UpToDate),
            suspended_user: false,
            suspended_fencing: false,
            replication: None,
            resync_done: None,
        }
    }

    #[test]
    fn quiescent_requires_connected_uptodate_and_no_resync() {
        assert!(storage_quiescent(&healthy()));
        // Mid-resync is not quiescent: the daemon is busy, gate 1 must block.
        let mut s = healthy();
        s.replication = Some(Replication::SyncTarget);
        s.resync_done = Some(72.0);
        assert!(!storage_quiescent(&s));
        // Not connected, or peer not UpToDate: not quiescent.
        let mut s = healthy();
        s.connection = ConnectionState::Connecting;
        assert!(!storage_quiescent(&s));
        let mut s = healthy();
        s.peer_disk = Some(DiskState::Inconsistent);
        assert!(!storage_quiescent(&s));
    }

    #[test]
    fn ha_disagreement_only_when_pool_on_and_host_disarmed() {
        assert!(ha_disagreement(Some(false), Some(true))); // the knot
        assert!(!ha_disagreement(Some(true), Some(true))); // host armed: fine
        assert!(!ha_disagreement(Some(false), Some(false))); // pool HA off: not the knot
        assert!(!ha_disagreement(None, Some(true))); // unknown host: never a knot
        assert!(!ha_disagreement(Some(false), None)); // unknown pool: never a knot
    }

    #[test]
    fn fix_blocked_while_healing_or_mid_episode() {
        let base = Diagnosis {
            storage: Some(healthy()),
            heal: None,
            daemon_active: false,
            pool_ha: Some(true),
            local_armed: Some(false),
            activating: vec![],
            sr_uuid: None,
            sr_recorded: false,
            sr_exists: false,
            peer_version: None,
            reboot_guard: crate::reboot_drain::GuardState::Armed,
            reboot_guard_unit_ok: Some(true),
            arbitrated: false,
            ha_timeout: Some(120),
            internal_shutdown: None,
            freeze_engaged: false,
        };
        // Quiescent, no episode, and a real knot: safe to fix (no blocker).
        assert!(base.any_knot());
        assert!(base.fix_block_reason().is_none());
        // Storage healing: blocked (gate 1).
        let mut d = Diagnosis {
            storage: Some(healthy()),
            ..cloneish(&base)
        };
        d.storage.as_mut().unwrap().replication = Some(Replication::SyncSource);
        assert!(d.fix_block_reason().unwrap().contains("gate 1"));
        // Unknown storage: blocked (gate 1).
        let d = Diagnosis {
            storage: None,
            ..cloneish(&base)
        };
        assert!(d.fix_block_reason().unwrap().contains("gate 1"));
        // Heal episode active: blocked (gate 2).
        let d = Diagnosis {
            heal: Some(crate::heal_state::HealState {
                stage: "survivor_keep".into(),
                summary: "waiting for peer".into(),
                next: None,
                attempts: 0,
                restarts: 0,
                holds: 0,
                started_unix: 100,
                updated_unix: 200,
            }),
            ..cloneish(&base)
        };
        assert!(d.fix_block_reason().unwrap().contains("gate 2"));
    }

    // DrbdStatus/HealState aren't Clone; rebuild the parts the tests vary.
    fn cloneish(b: &Diagnosis) -> Diagnosis {
        Diagnosis {
            storage: b.storage.as_ref().map(|_| healthy()),
            heal: None,
            daemon_active: b.daemon_active,
            pool_ha: b.pool_ha,
            local_armed: b.local_armed,
            activating: b.activating.clone(),
            sr_uuid: b.sr_uuid.clone(),
            sr_recorded: b.sr_recorded,
            sr_exists: b.sr_exists,
            peer_version: b.peer_version.clone(),
            reboot_guard: b.reboot_guard,
            reboot_guard_unit_ok: b.reboot_guard_unit_ok,
            arbitrated: b.arbitrated,
            ha_timeout: b.ha_timeout,
            internal_shutdown: b.internal_shutdown,
            freeze_engaged: b.freeze_engaged,
        }
    }
}
