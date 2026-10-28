//! Autostart completion: start the VMs XCP-ng's own boot-time autostart left
//! halted after a whole-pool reboot.
//!
//! XCP-ng starts the VMs marked `other-config:auto_poweron=true` once per host
//! boot (`xapi-autostart-vms`, run by `xapi-domains.service`), only when the
//! pool carries the same flag, and discards every error. When both hosts of a
//! TwinStor pool reboot together, it runs before the pool is whole: with HA
//! armed for one host failure (TwinStor sets `ha-host-failures-to-tolerate=1`)
//! every start is refused (`HA_OPERATION_WOULD_BREAK_FAILOVER_PLAN`) and the
//! VMs stay halted. A clean shutdown also cleared their `ha-always-run`, so HA
//! does not start them either.
//!
//! The pool master's daemon finishes that job once, after the boot: it starts
//! the VMs XCP-ng would have started, and only those that have stayed halted
//! since the boot, once the pool is whole. This module holds the decisions;
//! the state machine does the reads and the starts.

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

/// The daemon's first read must come this soon after the host boot. A daemon
/// started later (a restart, an upgrade) cannot tell a VM left halted by the
/// boot from one an operator halted since, and does nothing this boot.
pub(crate) const FIRST_READ_WINDOW: Duration = Duration::from_secs(20 * 60);
/// No start before the host has been up this long: XCP-ng's own autostart and
/// HA's restarts go first.
pub(crate) const MIN_UPTIME: Duration = Duration::from_secs(5 * 60);
/// The pool must have stayed whole this long before the first start.
pub(crate) const WHOLE_HOLD: Duration = Duration::from_secs(2 * 60);
/// Past this uptime the completion gives up, reporting what stayed halted:
/// an operator is involved by then.
pub(crate) const DEADLINE: Duration = Duration::from_secs(60 * 60);
/// Starts per scan. Each start is bounded by [`START_TIMEOUT_SECS`] and runs
/// in the tick, on a different tick than the protected-VM watchdog's starts.
pub(crate) const STARTS_PER_SCAN: usize = 2;
/// Least time between two scans.
pub(crate) const SCAN_EVERY: Duration = Duration::from_secs(50);
/// Bound of one `xe vm-start`.
pub(crate) const START_TIMEOUT_SECS: u64 = 90;
/// Start attempts per VM before it is reported as failed.
pub(crate) const MAX_ATTEMPTS: u32 = 3;
/// A VM whose last start is later than the boot minus this margin ran since
/// the boot (host clocks may differ by a few seconds; the margin only ever
/// makes fewer VMs eligible).
pub(crate) const BOOT_MARGIN_SECS: i64 = 120;

/// What the daemon learned this boot. Kept on every host, master or not, so
/// a host that becomes master later in the boot knows which VMs have stayed
/// halted since its own first read.
#[derive(Debug, Default)]
pub(crate) struct Watch {
    /// Nothing more to do this boot.
    pub finished: bool,
    /// The VMs halted at the first read and halted at every read since.
    /// `None` until the first read.
    pub candidates: Option<BTreeSet<String>>,
    /// Start attempts per VM.
    pub attempts: HashMap<String, u32>,
    /// When the last scan ran: the tick phase alone stalls with the tick
    /// counter, so the scan also keeps its own minute.
    pub last_scan: Option<Instant>,
    /// Since when the pool has read whole, on this master.
    pub whole_since: Option<Instant>,
    /// Why the pool last read not whole, for the logs and the give-up alert.
    pub not_whole: Option<&'static str>,
    pub started: Vec<String>,
    pub failed: Vec<(String, String)>,
    pub left: Vec<(String, String)>,
}

/// The once-per-boot record: the boot it was written in, and what happened.
pub(crate) fn record_path() -> String {
    std::env::var("TWINSTOR_AUTOSTART_RECORD")
        .unwrap_or_else(|_| "/var/lib/twinstor/autostart-completion".to_string())
}

pub(crate) fn record_text(boot_id: &str, outcome: &str) -> String {
    format!("boot_id={boot_id}\noutcome={outcome}\n")
}

/// Whether the record was written in this boot. An empty current boot id
/// proves nothing and reads as not done; the caller does nothing without one.
pub(crate) fn done_this_boot(record: &str, boot_id: &str) -> bool {
    let boot_id = boot_id.trim();
    !boot_id.is_empty()
        && record
            .lines()
            .find_map(|l| l.trim().strip_prefix("boot_id="))
            .is_some_and(|b| b.trim() == boot_id)
}

/// What the master reads to call the pool whole.
#[derive(Debug, Clone, Default)]
pub(crate) struct PoolFacts {
    /// Hosts in the pool, enabled ones, live ones (XAPI's host metrics).
    pub hosts: usize,
    pub enabled: usize,
    pub live: usize,
    /// Pool HA on, and if so whether this host is Online in its xhad and the
    /// peer is a live member of its liveset.
    pub ha_on: bool,
    pub local_ha_online: bool,
    pub peer_in_liveset: bool,
    /// DRBD connected with both copies UpToDate.
    pub drbd_connected: bool,
    pub local_uptodate: bool,
    pub peer_uptodate: bool,
    /// PBDs of the TwinStor SR, and how many are attached.
    pub pbds: usize,
    pub pbds_attached: usize,
}

/// `Ok` when the pool is whole, else what is missing. Every input defaults
/// to "missing", so an unreadable fact keeps the pool not whole.
pub(crate) fn pool_whole(f: &PoolFacts) -> Result<(), &'static str> {
    if f.hosts != 2 {
        return Err("the pool does not list two hosts");
    }
    if f.enabled != 2 {
        return Err("a host is disabled");
    }
    if f.live != 2 {
        return Err("a host is not live");
    }
    if f.ha_on && !(f.local_ha_online && f.peer_in_liveset) {
        return Err("HA is on but both hosts are not in its liveset");
    }
    if !(f.drbd_connected && f.local_uptodate && f.peer_uptodate) {
        return Err("the two copies are not connected and UpToDate");
    }
    if f.pbds != 2 || f.pbds_attached != 2 {
        return Err("the TwinStor SR is not plugged on both hosts");
    }
    Ok(())
}

/// XAPI's date format (`20261028T09:06:36Z`, UTC) for a Unix time: fixed
/// width, so two such dates compare as strings.
pub(crate) fn xapi_date(unix: i64) -> Option<String> {
    let t = time::OffsetDateTime::from_unix_timestamp(unix).ok()?;
    Some(format!(
        "{:04}{:02}{:02}T{:02}:{:02}:{:02}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    ))
}

/// The date a VM must have last started before to count as halted since the
/// boot: the boot time minus [`BOOT_MARGIN_SECS`].
pub(crate) fn boot_threshold(now_unix: i64, uptime: Duration) -> Option<String> {
    xapi_date(now_unix - uptime.as_secs() as i64 - BOOT_MARGIN_SECS)
}

fn is_xapi_date(d: &str) -> bool {
    d.len() == 18 && d.as_bytes()[8] == b'T' && d.ends_with('Z')
}

/// What one VM looks like right before its start.
#[derive(Debug, Clone, Default)]
pub(crate) struct VmFacts {
    pub power_state: String,
    pub ha_always_run: String,
    /// `other-config:auto_poweron` (absent: empty).
    pub auto_poweron: String,
    /// XAPI's `start-time` of the VM's last start.
    pub start_time: String,
    /// Carries TwinStor's disks-suspect mark.
    pub disks_suspect: bool,
}

/// Whether a candidate may be started now, else why not. Re-read right
/// before each start, so a VM an operator touched since the last scan is
/// left alone.
pub(crate) fn start_guard(v: &VmFacts, boot_threshold: &str) -> Result<(), &'static str> {
    if v.power_state != "halted" {
        return Err("not halted any more");
    }
    if v.auto_poweron != "true" {
        return Err("auto_poweron is no longer set on the VM");
    }
    if v.ha_always_run != "false" {
        // HA starts a VM it still protects; an unreadable flag is no answer.
        return Err("HA protects it (ha-always-run is not false)");
    }
    if v.disks_suspect {
        return Err("its disks are marked suspect after a split-brain");
    }
    if !is_xapi_date(&v.start_time) || !is_xapi_date(boot_threshold) {
        return Err("its last start time cannot be read");
    }
    if v.start_time.as_str() > boot_threshold {
        return Err("it ran since this boot (stopped on purpose)");
    }
    Ok(())
}

/// The candidates to try this scan: the least tried first, at most
/// [`STARTS_PER_SCAN`].
pub(crate) fn pick_batch<'a>(
    candidates: impl IntoIterator<Item = &'a String>,
    attempts: impl Fn(&str) -> u32,
) -> Vec<String> {
    let mut v: Vec<(u32, String)> = candidates
        .into_iter()
        .map(|u| (attempts(u), u.clone()))
        .filter(|(a, _)| *a < MAX_ATTEMPTS)
        .collect();
    v.sort();
    v.into_iter()
        .take(STARTS_PER_SCAN)
        .map(|(_, u)| u)
        .collect()
}

/// The alert sent when the completion ends: the VMs started, those that could
/// not be, and those left alone with the reason. `names` gives a VM's label.
pub(crate) fn alert_body(
    started: &[String],
    failed: &[(String, String)],
    left: &[(String, String)],
    names: impl Fn(&str) -> String,
) -> String {
    let mut b = String::from(
        "TwinStor: after this pool's reboot, XCP-ng's autostart left VMs marked auto_poweron \
         halted (it runs once per host boot, before the pool is whole, and HA refused those \
         starts). Once the pool was whole, TwinStor started them:\n",
    );
    for u in started {
        b.push_str(&format!("- started: {} ({u})\n", names(u)));
    }
    for (u, why) in failed {
        b.push_str(&format!(
            "- could not start: {} ({u}): {why}. Start it with `xe vm-start uuid={u}`.\n",
            names(u)
        ));
    }
    for (u, why) in left {
        b.push_str(&format!("- left halted: {} ({u}): {why}\n", names(u)));
    }
    b.push_str(
        "This runs once per boot of the pool master; set [ha] complete_autostart = false in \
         /etc/twinstor/twinstor.toml to turn it off.",
    );
    b
}

/// The alert sent when the pool did not become whole before [`DEADLINE`].
pub(crate) fn give_up_body(left: &[String], why: &str, names: impl Fn(&str) -> String) -> String {
    let mut b = format!(
        "TwinStor: VMs marked auto_poweron are still halted an hour after this pool master \
         booted, and TwinStor did not start them because {why}. XCP-ng's own autostart runs \
         only once per host boot. Start them by hand once the pool is whole:\n"
    );
    for u in left {
        b.push_str(&format!("- {} ({u}): xe vm-start uuid={u}\n", names(u)));
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn whole() -> PoolFacts {
        PoolFacts {
            hosts: 2,
            enabled: 2,
            live: 2,
            ha_on: true,
            local_ha_online: true,
            peer_in_liveset: true,
            drbd_connected: true,
            local_uptodate: true,
            peer_uptodate: true,
            pbds: 2,
            pbds_attached: 2,
        }
    }

    #[test]
    fn a_whole_pool_needs_every_signal() {
        assert!(pool_whole(&whole()).is_ok());
        let cases: Vec<fn(&mut PoolFacts)> = vec![
            |f| f.hosts = 1,
            |f| f.enabled = 1,
            |f| f.live = 1,
            |f| f.peer_in_liveset = false,
            |f| f.local_ha_online = false,
            |f| f.drbd_connected = false,
            |f| f.peer_uptodate = false,
            |f| f.local_uptodate = false,
            |f| f.pbds_attached = 1,
            |f| f.pbds = 1,
        ];
        for c in cases {
            let mut f = whole();
            c(&mut f);
            assert!(pool_whole(&f).is_err(), "{f:?}");
        }
        // HA off: the liveset is not asked.
        let mut f = whole();
        f.ha_on = false;
        f.peer_in_liveset = false;
        f.local_ha_online = false;
        assert!(pool_whole(&f).is_ok());
        // Nothing read: not whole.
        assert!(pool_whole(&PoolFacts::default()).is_err());
    }

    #[test]
    fn the_record_binds_to_the_boot() {
        let r = record_text("abc", "started 2");
        assert!(done_this_boot(&r, "abc"));
        assert!(!done_this_boot(&r, "def"), "a new boot runs again");
        assert!(!done_this_boot(&r, ""), "no boot id proves nothing");
        assert!(!done_this_boot("", "abc"));
        assert!(!done_this_boot("outcome=x\n", "abc"));
    }

    fn vm() -> VmFacts {
        VmFacts {
            power_state: "halted".into(),
            ha_always_run: "false".into(),
            auto_poweron: "true".into(),
            start_time: "20261026T12:10:25Z".into(),
            disks_suspect: false,
        }
    }

    #[test]
    fn the_guard_leaves_operators_and_ha_alone() {
        let boot = "20261028T08:00:00Z";
        assert!(start_guard(&vm(), boot).is_ok());
        let never_started = VmFacts {
            start_time: "19700101T00:00:00Z".into(),
            ..vm()
        };
        assert!(start_guard(&never_started, boot).is_ok());
        let cases: Vec<fn(&mut VmFacts)> = vec![
            |v| v.power_state = "running".into(),
            |v| v.power_state = "suspended".into(),
            |v| v.auto_poweron = "false".into(),
            |v| v.auto_poweron = String::new(),
            |v| v.ha_always_run = "true".into(),
            |v| v.ha_always_run = String::new(),
            |v| v.disks_suspect = true,
            |v| v.start_time = "20261028T08:30:00Z".into(),
            |v| v.start_time = String::new(),
        ];
        for c in cases {
            let mut v = vm();
            c(&mut v);
            assert!(start_guard(&v, boot).is_err(), "{v:?}");
        }
        assert!(start_guard(&vm(), "garbage").is_err());
    }

    #[test]
    fn the_boot_threshold_has_a_margin() {
        // 2026-10-28T09:00:00Z, up 30 minutes: boot 08:30:00, threshold 08:28:00.
        let now = 1_793_178_000;
        assert_eq!(xapi_date(now).as_deref(), Some("20261028T09:00:00Z"));
        assert_eq!(
            boot_threshold(now, Duration::from_secs(1800)).as_deref(),
            Some("20261028T08:28:00Z")
        );
    }

    #[test]
    fn batches_take_the_least_tried_and_skip_the_exhausted() {
        let c: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        let tries = |u: &str| match u {
            "a" => 1,
            "b" => 0,
            "c" => MAX_ATTEMPTS,
            _ => 0,
        };
        assert_eq!(
            pick_batch(&c, tries),
            vec!["b".to_string(), "d".to_string()]
        );
        let only_exhausted = vec!["c".to_string()];
        assert!(pick_batch(&only_exhausted, tries).is_empty());
    }

    #[test]
    fn the_alerts_name_every_vm() {
        let b = alert_body(
            &["u1".into()],
            &[("u2".into(), "NO_HOSTS_AVAILABLE".into())],
            &[(
                "u3".into(),
                "it ran since this boot (stopped on purpose)".into(),
            )],
            |u| format!("vm-{u}"),
        );
        assert!(b.contains("started: vm-u1 (u1)"));
        assert!(b.contains("could not start: vm-u2 (u2): NO_HOSTS_AVAILABLE"));
        assert!(b.contains("xe vm-start uuid=u2"));
        assert!(b.contains("left halted: vm-u3 (u3)"));
        assert!(b.contains("complete_autostart = false"));
        let g = give_up_body(&["u4".into()], "a host is disabled", |u| format!("vm-{u}"));
        assert!(g.contains("because a host is disabled"));
        assert!(g.contains("vm-u4 (u4): xe vm-start uuid=u4"));
    }
}
