//! Consumer-side (initiator) multipath management and health for the
//! twinstor device.
//!
//! The producer side (the LIO target) is gated by `readiness::assess`; this
//! module watches the *initiator* stack that consumes it: iSCSI sessions to
//! both portals, the multipath map over them, and which path group is
//! active. A broken initiator stack is silent (VMs and the XHA statefile
//! keep working over whatever single path LVM grabbed) until the peer dies
//! and the lone path dies with it, freezing statefile I/O long enough for
//! XHA to self-fence the host. So this health is **reported and
//! self-healed, never used to gate the target**: the target must stay up
//! regardless (it is the *peer's* second path), and at boot the map cannot
//! exist before the target it rides on (gating would deadlock bootstrap).

use tracing::{info, warn};

/// Snapshot of the initiator-side path stack for the twinstor LUN.
///
/// `None` fields mean the signal could not be read; callers must fail open
/// on those (same rule as `host_enabled` in `readiness::assess`).
#[derive(Debug, Clone, Default)]
pub struct PathHealth {
    /// Multipath-format WWID (`3` + NAA id) of the twinstor LUN, when at
    /// least one path device is visible.
    pub wwid: Option<String>,
    /// SCSI disks backed by the twinstor LIO LUN currently visible,
    /// whether or not multipath has claimed them.
    pub session_paths: usize,
    /// Whether a multipath map exists for the WWID.
    pub map_present: bool,
    /// Whether the map's active path group rides the local portal. `None`
    /// when there is no map or the local portal could not be identified.
    pub local_path_active: Option<bool>,
    /// Whether the WWID is registered in `/etc/multipath/wwids` (required
    /// for the map to assemble at boot). `None` when the file is unreadable.
    pub wwid_registered: Option<bool>,
    /// Whether the live map queues for fewer checker intervals than
    /// `NO_PATH_RETRY` with all paths gone. A reconfigure while a path is
    /// missing can rebuild the map with the default retry count (10, ~50 s)
    /// and nothing restores it: the next total path loss fails guest I/O at
    /// 50 s instead of 150 s. `None` when unreadable.
    pub queueing_short: Option<bool>,
}

/// Checker intervals the twinstor map queues I/O for with all paths gone
/// (see `configure_local_prio`).
pub const NO_PATH_RETRY: u32 = 30;

/// The degradation verdict over a [`PathHealth`] snapshot: `None` when
/// healthy, otherwise the most severe problem. Free of I/O, so it is
/// unit-testable.
///
/// Severity order: storage on a raw device (no map: no failover at all),
/// then a missing path (no redundancy), then a remote active path (a peer
/// failure freezes I/O for the full failure-detection window, most of the
/// XHA statefile budget), then an unregistered WWID (latent: breaks the
/// *next* boot).
pub fn degradation(h: &PathHealth) -> Option<String> {
    if h.session_paths == 0 {
        return Some("no iSCSI path devices visible".to_string());
    }
    if !h.map_present {
        return Some("no multipath map: storage I/O rides a raw single-path device".to_string());
    }
    if h.session_paths < 2 {
        return Some(format!(
            "only {} iSCSI path (local or peer portal session missing)",
            h.session_paths
        ));
    }
    if h.local_path_active == Some(false) {
        return Some(
            "active multipath path is the REMOTE one (local-path priority not in effect)"
                .to_string(),
        );
    }
    if h.queueing_short == Some(true) {
        return Some(format!(
            "multipath map queues I/O for fewer than {NO_PATH_RETRY} checker intervals with all paths gone"
        ));
    }
    if h.wwid_registered == Some(false) {
        return Some(
            "WWID not registered in /etc/multipath/wwids (map will not assemble at next boot)"
                .to_string(),
        );
    }
    None
}

/// The queueing column (`%Q`) of `multipathd show maps format "%n %Q"` for
/// `wwid`: `30 chk` (retries left to a checker count), `on` (queue
/// forever), `off`, or `-`.
fn parse_map_queueing(text: &str, wwid: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        (it.next()? == wwid).then(|| it.collect::<Vec<_>>().join(" "))
    })
}

/// Whether a `%Q` value queues for fewer than `want` checker intervals.
/// `on` (forever) is not short; `off` is; anything unrecognised is unknown.
fn queueing_short(q: &str, want: u32) -> Option<bool> {
    let first = q.split_whitespace().next()?;
    match first {
        "on" => Some(false),
        "off" => Some(true),
        n => n.parse::<u32>().ok().map(|n| n < want),
    }
}

/// Probe the initiator-side path stack. `local_ip` is this node's address on
/// the storage network and `peer_ip` the peer's; the probe only counts path
/// devices whose iSCSI session targets one of those two portals, so foreign
/// LIO sessions on the same host cannot skew the verdict or the WWID. Both
/// addresses are required: without knowing which portal is local the verdict
/// would miscount paths, so callers that cannot resolve the local address
/// must skip the probe (fail open) instead.
pub async fn probe(local_ip: &str, peer_ip: &str) -> PathHealth {
    let mut h = PathHealth::default();

    let hosts = iscsi_host_targets();
    let twinstor_hosts: Vec<&str> = hosts
        .iter()
        .filter(|(_, ip)| ip == local_ip || ip == peer_ip)
        .map(|(host, _)| host.as_str())
        .collect();

    let devices = twinstor_path_devices(&twinstor_hosts);
    h.session_paths = devices.len();
    h.wwid = consensus_wwid(&devices);
    let Some(wwid) = h.wwid.clone() else {
        return h;
    };

    h.wwid_registered = std::fs::read_to_string("/etc/multipath/wwids")
        .ok()
        .map(|t| t.contains(&format!("/{wwid}/")));

    // Bounded: multipath -ll talks to multipathd, which can hang when the
    // daemon is wedged; a stuck probe must not stall the caller forever.
    // kill_on_drop: without it the abandoned child outlives the timeout and
    // is reparented to init, leaking one `multipath -ll` per probe on a
    // wedged node (both `twinstor status` and the daemon tick run this probe).
    let ll = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new("multipath")
            .args(["-ll"])
            .kill_on_drop(true)
            .output(),
    )
    .await;
    if let Ok(Ok(out)) = ll {
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        if let Some(topo) = parse_map_topology(&text, &wwid) {
            h.map_present = true;
            h.local_path_active = local_path_in_active_group(&hosts, local_ip, &topo.active_hosts);
        }
    }
    if h.map_present {
        let q = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::process::Command::new("multipathd")
                .args(["show", "maps", "format", "%n %Q"])
                .kill_on_drop(true)
                .output(),
        )
        .await;
        if let Ok(Ok(out)) = q {
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            h.queueing_short =
                parse_map_queueing(&text, &wwid).and_then(|v| queueing_short(&v, NO_PATH_RETRY));
        }
    }
    h
}

/// The WWID shared by the path devices. The two twinstor portals export the
/// same LUN, so disagreement means a foreign or stale device slipped through
/// the portal filter; majority wins and the anomaly is logged.
fn consensus_wwid(devices: &[PathDevice]) -> Option<String> {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for d in devices {
        *counts.entry(d.wwid.as_str()).or_default() += 1;
    }
    if counts.len() > 1 {
        warn!(wwids = ?counts.keys().collect::<Vec<_>>(), "twinstor path devices disagree on WWID");
    }
    counts
        .into_iter()
        .max_by_key(|&(w, n)| (n, w))
        .map(|(w, _)| w.to_string())
}

/// Run the safe self-heal steps for a degraded path stack: re-register the
/// WWID, re-run discovery/login for missing sessions, and re-assert the
/// local-path-preferred multipath config. Discovery against a dead portal
/// blocks for the full iscsiadm timeout, so callers in latency-sensitive
/// loops must run this in a background task.
///
/// `peer_login` false keeps the peer's portal logged out (a fate-shared
/// Primary whose copy is disconnected from the peer's: its guests must not
/// reach the peer's copy, see the fence-peer handler).
pub async fn self_heal(
    health: PathHealth,
    local_ip: String,
    peer_ip: String,
    port: u16,
    chap: Option<(String, String)>,
    peer_login: bool,
) {
    if health.wwid_registered == Some(false)
        && let Some(wwid) = &health.wwid
    {
        register_wwid(wwid).await;
    }
    if health.session_paths < 2 {
        // A missing session is usually a failed discovery/login (e.g. the
        // CHAP race against a target still starting up): retry it.
        let chap_ref = chap.as_ref().map(|(u, p)| (u.as_str(), p.as_str()));
        for ip in [local_ip.as_str(), peer_ip.as_str()] {
            if ip == peer_ip && !peer_login {
                continue;
            }
            let portal = format!("{ip}:{port}");
            if !crate::iscsi::session_exists(&portal).await {
                info!(portal = %portal, "path self-heal: re-running iSCSI discovery/login");
                crate::iscsi::discover_and_login(&portal, chap_ref).await;
            }
        }
    }
    // A short retry count is restored only with both paths up: a reconfigure
    // with one missing is what shortens it (callers never self-heal while
    // this copy's I/O is frozen).
    let short_queue = health.queueing_short == Some(true) && health.session_paths >= 2;
    if short_queue {
        warn!(
            heal_event = "multipath_queue_restore",
            "multipath map queues fewer retries than configured; reconfiguring with both paths up"
        );
    }
    if !health.map_present || health.local_path_active == Some(false) || short_queue {
        // Re-asserts local-path priority (failback immediate) and reconfigures
        // multipathd; with both sessions present and the WWID registered this
        // is what assembles a missing map. It cannot claim a device LVM
        // already holds raw; that case persists and reaches the alert with
        // the manual fix.
        configure_local_prio(Some(&local_ip), &peer_ip).await;
    }
}

/// Register the WWID in `/etc/multipath/wwids` so multipathd assembles the
/// map at boot. Safe to repeat (`multipath -a` is idempotent).
pub async fn register_wwid(wwid: &str) {
    let out = tokio::process::Command::new("multipath")
        .args(["-a", wwid])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            info!(wwid = %wwid, "registered WWID in /etc/multipath/wwids")
        }
        Ok(o) => warn!(
            wwid = %wwid,
            error = %String::from_utf8_lossy(&o.stderr).trim(),
            "failed to register WWID"
        ),
        Err(e) => warn!(wwid = %wwid, error = %e, "failed to register WWID"),
    }
}

/// This node's source address toward `peer` (i.e. its address on the
/// storage network), from the kernel's routing decision.
pub async fn local_ip_toward(peer: &str) -> Option<String> {
    // Bounded (reached inline from the rejoin path): a wedged `ip` must not
    // hang the tick.
    let s = crate::cmd::cmd_output_for("ip", &["route", "get", peer], 10)
        .await
        .ok()?;
    s.split_whitespace()
        .skip_while(|w| *w != "src")
        .nth(1)
        .map(|ip| ip.to_string())
}

/// Write the multipath config that prefers the local path (weighted prio +
/// `failback immediate`) and reconfigure multipathd. Re-runnable: it is the
/// self-heal for a map with the remote path active. Only sessions to the two
/// twinstor portals are considered; a foreign iSCSI session must not end up
/// weighted as the peer path.
pub async fn configure_local_prio(local_ip: Option<&str>, peer_ip: &str) {
    let Some(local_ip) = local_ip else {
        warn!("configure_local_prio: local storage IP unknown, skipping");
        return;
    };

    let mut local_host: Option<String> = None;
    let mut remote_host: Option<String> = None;
    for (host, addr) in iscsi_host_targets() {
        if addr == local_ip {
            local_host = Some(host);
        } else if addr == peer_ip {
            remote_host = Some(host);
        }
    }

    let (local_h, remote_h) = match (local_host, remote_host) {
        (Some(l), Some(r)) => (l, r),
        (l, r) => {
            warn!(
                local = l.is_some(),
                remote = r.is_some(),
                "configure_local_prio: missing iSCSI session (need both portals), skipping"
            );
            return;
        }
    };

    let devices = twinstor_path_devices(&[local_h.as_str(), remote_h.as_str()]);
    let wwid = match consensus_wwid(&devices) {
        Some(w) => w,
        None => {
            warn!("configure_local_prio: twinstor WWID not found, skipping");
            return;
        }
    };

    // no_path_retry 30: with all paths gone, queue I/O for 30 checker intervals
    // (~150s) and then fail it with EIO, instead of the stock queue_if_no_path
    // queueing forever. Total path loss means no data source at all; XHA fences
    // the host on the frozen statefile long before the 150s expire, and
    // surviving processes get clean errors rather than unkillable D-state.
    // Irrelevant while any path works.
    //
    // no_path_retry does not address guest read-only remounts under a
    // split-brain. Those are not a no-path condition: a split-brain target
    // rebuild against a not-yet-UpToDate DRBD device briefly publishes a phantom
    // 64-block LUN, so the initiator gets a SCSI Illegal Request (LBA out of
    // range), which is a hard I/O error, not a path failure; multipath passes it
    // straight through and no queue value can mask it. The fix for that lives in
    // the split-brain resolver (a correctly elected loser holds down instead of
    // thrashing target rebuilds) and the atomic iSCSI publish, not here. For
    // context, a Windows guest's PV disk driver (win-xenvbd) has no timeout of
    // its own and waits on the ring like Linux blkfront; the bound is the
    // disk-class TimeOutValue the XCP-ng INF sets to 120s, so a stall under that
    // is ridden out on Windows too.
    let conf = format!(
        "multipaths {{\n    multipath {{\n        wwid \"{wwid}\"\n        \
         path_grouping_policy group_by_prio\n        \
         path_selector \"queue-length 0\"\n        \
         prio weightedpath\n        \
         prio_args \"hbtl {local_h}:.*:.*:.* 50 {remote_h}:.*:.*:.* 10\"\n        \
         failback immediate\n        \
         no_path_retry {NO_PATH_RETRY}\n    }}\n}}\n"
    );

    let conf_dir = std::path::Path::new("/etc/multipath/conf.d");
    if !conf_dir.exists() {
        let _ = std::fs::create_dir_all(conf_dir);
    }

    if let Err(e) = std::fs::write(conf_dir.join("twinstor.conf"), &conf) {
        warn!(error = %e, "failed to write multipath config");
        return;
    }

    info!(local_host = %local_h, remote_host = %remote_h, "multipath: local path preferred");
    // Bounded (reached inline from the rejoin path): a wedged multipathd must
    // not hang the tick.
    let _ = crate::cmd::run_cmd_quiet("multipathd", &["reconfigure"]).await;

    // The multipath map was just reloaded out of band (re-pinning local-path prio
    // after a path re-logged in under a new SCSI host). A reload brings the path
    // to `active` via a `change` uevent without DM_ACTION=PATH_REINSTATED, and
    // XCP-ng's 65-multipath.rules only recounts on PATH_REINSTATED/PATH_FAILED or
    // a path add/remove, never on a bare reload. So the PBD `mpath-<id>` path
    // count XO/operators read would stay frozen at its pre-reload value (e.g.
    // [1,2]) while the map is actually [2,2]. Kick XCP-ng's own recount the same
    // way udev does; it no-ops safely if the pipe/daemon is absent.
    let _ = crate::cmd::run_cmd_quiet("/opt/xensource/libexec/kickpipe", &["mpathcount"]).await;
}

struct PathDevice {
    wwid: String,
}

/// SCSI disks reached through the given iSCSI hosts (the twinstor portals'
/// sessions), from sysfs. Sees the paths whether or not multipath has
/// claimed them, because the broken state to detect is "sessions up, no map".
fn twinstor_path_devices(allowed_hosts: &[&str]) -> Vec<PathDevice> {
    let mut v = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/block") else {
        return v;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("sd") {
            continue;
        }
        let dev = e.path().join("device");
        let read = |f: &str| {
            std::fs::read_to_string(dev.join(f))
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        // The device symlink resolves to .../<H:C:T:L>; H is the SCSI host.
        let host = std::fs::read_link(&dev)
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
            .and_then(|hctl| hctl.split(':').next().map(|h| h.to_string()))
            .unwrap_or_default();
        if !allowed_hosts.contains(&host.as_str()) {
            continue;
        }
        if read("vendor") != "LIO-ORG" || !read("model").starts_with("twinstor") {
            continue;
        }
        // sysfs reports "naa.6001405..."; multipath names it "36001405...".
        if let Some(naa) = read("wwid").strip_prefix("naa.") {
            v.push(PathDevice {
                wwid: format!("3{naa}"),
            });
        }
    }
    v
}

/// Every twinstor LUN disk (any session) currently in the `offline` SCSI
/// state. Only the split fence sets these disks offline, so all of them are
/// the fence's to bring back.
pub fn offline_twinstor_disks() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/sys/block") else {
        return Vec::new();
    };
    let mut v = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("sd") {
            continue;
        }
        let dev = e.path().join("device");
        let read = |f: &str| {
            std::fs::read_to_string(dev.join(f))
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        if read("vendor") == "LIO-ORG"
            && read("model").starts_with("twinstor")
            && read("state") == "offline"
        {
            v.push(name);
        }
    }
    v.sort();
    v
}

/// A SCSI disk's state (`running`, `offline`, `transport-offline`,
/// `blocked`...), from sysfs.
pub fn device_state(dev: &str) -> Option<String> {
    std::fs::read_to_string(format!("/sys/block/{dev}/device/state"))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Set a SCSI disk's state (`offline` / `running`). An offline disk fails
/// every command at once, so multipath fails the path and its checker keeps
/// it failed; the iSCSI session and the device itself stay, so setting it
/// back to `running` lets the checker reinstate the same path, with no new
/// login and no map reload. The kernel refuses `offline` on a disk whose
/// session already timed out (`transport-offline`).
pub fn set_device_state(dev: &str, state: &str) -> bool {
    std::fs::write(format!("/sys/block/{dev}/device/state"), state).is_ok()
}

/// Enforce the session recovery timeout on the twinstor sessions, live via
/// sysfs. The node records (set at discovery) cover future logins; this
/// covers sessions already established and later overrides (the XCP-ng
/// stack resets `recovery_tmo` on PBD plug). Only sessions to the two
/// twinstor portals are touched. See `iscsi::REPLACEMENT_TIMEOUT_SECS` for
/// the budget math.
pub fn enforce_recovery_tmo(local_ip: &str, peer_ip: &str, secs: u32) {
    let Ok(entries) = std::fs::read_dir("/sys/class/iscsi_host") else {
        return;
    };
    for entry in entries.flatten() {
        let host_id = entry.file_name().to_string_lossy().to_string();
        let device_dir = format!("/sys/class/iscsi_host/{host_id}/device");
        let Some(ip) = find_session_target_ip(&device_dir) else {
            continue;
        };
        if ip != local_ip && ip != peer_ip {
            continue;
        }
        let Ok(sessions) = std::fs::read_dir(&device_dir) else {
            continue;
        };
        for session in sessions.flatten() {
            let name = session.file_name().to_string_lossy().to_string();
            if !name.starts_with("session") {
                continue;
            }
            let path = format!("/sys/class/iscsi_session/{name}/recovery_tmo");
            let current = std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok());
            if current == Some(secs) {
                continue;
            }
            match std::fs::write(&path, secs.to_string()) {
                Ok(()) => info!(
                    session = %name,
                    portal = %ip,
                    from = ?current,
                    to = secs,
                    "enforced iSCSI session recovery_tmo"
                ),
                Err(e) => warn!(session = %name, error = %e, "failed to set recovery_tmo"),
            }
        }
    }
}

/// All iSCSI sessions on this node as (SCSI host number, target portal IP).
fn iscsi_host_targets() -> Vec<(String, String)> {
    let mut v = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/iscsi_host") else {
        return v;
    };
    for entry in entries.flatten() {
        let host_id = entry.file_name().to_string_lossy().to_string();
        let device_dir = format!("/sys/class/iscsi_host/{host_id}/device");
        if let Some(addr) = find_session_target_ip(&device_dir) {
            v.push((host_id.trim_start_matches("host").to_string(), addr));
        }
    }
    v
}

/// Whether a session to the local portal carries the active path group.
/// Any of them counts: a duplicate login to the local portal (a give-way's
/// logout raced by two re-logins) can leave one local host prioritized and
/// active while the first one listed is not, and checking only the first
/// would report "remote path active" indefinitely. `None` with no local
/// session.
fn local_path_in_active_group(
    hosts: &[(String, String)],
    local_ip: &str,
    active: &[String],
) -> Option<bool> {
    let mut local = hosts.iter().filter(|(_, ip)| ip == local_ip).peekable();
    local.peek()?;
    Some(local.any(|(host, _)| active.contains(host)))
}

fn find_session_target_ip(device_dir: &str) -> Option<String> {
    for session in std::fs::read_dir(device_dir).ok()?.flatten() {
        let name = session.file_name();
        if !name.to_string_lossy().starts_with("session") {
            continue;
        }
        for conn in std::fs::read_dir(session.path()).ok()?.flatten() {
            if !conn.file_name().to_string_lossy().starts_with("connection") {
                continue;
            }
            let iscsi_conn = conn.path().join("iscsi_connection");
            for ic in std::fs::read_dir(iscsi_conn).ok()?.flatten() {
                let addr_file = ic.path().join("persistent_address");
                if let Ok(addr) = std::fs::read_to_string(&addr_file) {
                    return Some(addr.trim().to_string());
                }
            }
        }
    }
    None
}

struct MapTopology {
    /// SCSI host numbers of the paths in the `status=active` path group.
    active_hosts: Vec<String>,
}

/// Parse `multipath -ll` output for the map named `wwid`. Returns `None`
/// when no such map exists. Free of I/O, so the topology cases are
/// unit-testable.
fn parse_map_topology(text: &str, wwid: &str) -> Option<MapTopology> {
    let mut in_map = false;
    let mut found = false;
    let mut group_active = false;
    let mut topo = MapTopology {
        active_hosts: Vec::new(),
    };

    for line in text.lines() {
        let is_subline = line.starts_with([' ', '\t', '|', '`']) || line.starts_with("size=");
        if !is_subline {
            // A map header line: enter/leave our map's section.
            in_map = line.starts_with(wwid);
            if in_map {
                found = true;
            }
            continue;
        }
        if !in_map {
            continue;
        }
        if line.contains("policy=") {
            group_active = line.contains("status=active");
            continue;
        }
        // Path line: the H:C:T:L token identifies it.
        let hctl = line.split_whitespace().find(|w| {
            w.split(':').count() == 4 && w.chars().next().is_some_and(|c| c.is_ascii_digit())
        });
        if let Some(hctl) = hctl
            && group_active
        {
            topo.active_hosts
                .push(hctl.split(':').next().unwrap_or("").to_string());
        }
    }
    found.then_some(topo)
}

#[cfg(test)]
mod queueing_tests {
    use super::*;

    #[test]
    fn map_queueing_is_read_and_judged() {
        let text = "name                              queueing paths\n\
                    36001405daa0000000000000000000000 10 chk   1    \n\
                    3600other 30 chk 2\n";
        let q = parse_map_queueing(text, "36001405daa0000000000000000000000").unwrap();
        assert_eq!(q, "10 chk 1");
        assert_eq!(queueing_short(&q, NO_PATH_RETRY), Some(true));
        assert_eq!(queueing_short("30 chk 2", NO_PATH_RETRY), Some(false));
        assert_eq!(queueing_short("on 2", NO_PATH_RETRY), Some(false));
        assert_eq!(queueing_short("off 0", NO_PATH_RETRY), Some(true));
        assert_eq!(queueing_short("- 2", NO_PATH_RETRY), None);
        assert!(parse_map_queueing(text, "3600missing").is_none());
        let h = PathHealth {
            session_paths: 2,
            map_present: true,
            local_path_active: Some(true),
            queueing_short: Some(true),
            ..Default::default()
        };
        assert!(degradation(&h).unwrap().contains("fewer than 30"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_local_session_in_the_active_group_is_the_local_path() {
        let hosts = vec![
            ("2".to_string(), "192.0.2.47".to_string()),
            ("3".to_string(), "192.0.2.49".to_string()),
            ("4".to_string(), "192.0.2.49".to_string()),
        ];
        // The duplicate local session (host 4) is the prioritized, active one.
        assert_eq!(
            local_path_in_active_group(&hosts, "192.0.2.49", &["4".to_string()]),
            Some(true)
        );
        assert_eq!(
            local_path_in_active_group(&hosts, "192.0.2.49", &["2".to_string()]),
            Some(false)
        );
        assert_eq!(
            local_path_in_active_group(&hosts, "10.0.0.1", &["2".to_string()]),
            None
        );
    }

    const WWID: &str = "36001405daa0000000000000000000000";

    // Real `multipath -ll` output: local path (host 7) prio 50 active,
    // remote path (host 6) prio 10 enabled.
    fn ll_local_active() -> &'static str {
        "36001405daa0000000000000000000000 dm-0 LIO-ORG ,twinstor        \n\
         size=466G features='1 queue_if_no_path' hwhandler='1 alua' wp=rw\n\
         |-+- policy='queue-length 0' prio=50 status=active\n\
         | `- 7:0:0:0 sdb 8:16 active ready running\n\
         `-+- policy='queue-length 0' prio=10 status=enabled\n\
         \x20 `- 6:0:0:0 sda 8:0  active ready running\n"
    }

    fn ll_remote_active() -> &'static str {
        "36001405daa0000000000000000000000 dm-0 LIO-ORG ,twinstor        \n\
         size=466G features='1 queue_if_no_path' hwhandler='1 alua' wp=rw\n\
         |-+- policy='queue-length 0' prio=10 status=active\n\
         | `- 6:0:0:0 sda 8:0  active ready running\n\
         `-+- policy='queue-length 0' prio=50 status=enabled\n\
         \x20 `- 7:0:0:0 sdb 8:16 active ready running\n"
    }

    fn healthy() -> PathHealth {
        PathHealth {
            wwid: Some(WWID.to_string()),
            session_paths: 2,
            map_present: true,
            local_path_active: Some(true),
            wwid_registered: Some(true),
            queueing_short: Some(false),
        }
    }

    #[test]
    fn parses_active_group_hosts() {
        let topo = parse_map_topology(ll_local_active(), WWID).unwrap();
        assert_eq!(topo.active_hosts, vec!["7"]);
        let topo = parse_map_topology(ll_remote_active(), WWID).unwrap();
        assert_eq!(topo.active_hosts, vec!["6"]);
    }

    #[test]
    fn missing_map_is_none() {
        assert!(parse_map_topology("", WWID).is_none());
        // Another map's output must not be mistaken for ours.
        let other = ll_local_active().replace(WWID, "3600140599999999999999999999999");
        assert!(parse_map_topology(&other, WWID).is_none());
    }

    #[test]
    fn ignores_other_maps_sublines() {
        // Our map absent, another map present with paths: still None, and
        // the other map's paths must not leak into a found result when ours
        // is present later in the output.
        let text = format!(
            "3600140599999999999999999999999 dm-1 LIO-ORG ,other\n\
             size=10G features='0' hwhandler='1 alua' wp=rw\n\
             `-+- policy='queue-length 0' prio=50 status=active\n\
             \x20 `- 9:0:0:0 sdz 8:64 active ready running\n\
             {}",
            ll_local_active()
        );
        let topo = parse_map_topology(&text, WWID).unwrap();
        assert_eq!(topo.active_hosts, vec!["7"]);
    }

    #[test]
    fn healthy_has_no_degradation() {
        assert_eq!(degradation(&healthy()), None);
    }

    #[test]
    fn missing_map_dominates() {
        // No map is the worst condition and must win over the others.
        let h = PathHealth {
            map_present: false,
            local_path_active: None,
            wwid_registered: Some(false),
            ..healthy()
        };
        assert!(degradation(&h).unwrap().contains("no multipath map"));
    }

    #[test]
    fn single_path_is_degraded() {
        let h = PathHealth {
            session_paths: 1,
            ..healthy()
        };
        assert!(degradation(&h).unwrap().contains("only 1 iSCSI path"));
    }

    #[test]
    fn remote_active_path_is_degraded() {
        let h = PathHealth {
            local_path_active: Some(false),
            ..healthy()
        };
        assert!(degradation(&h).unwrap().contains("REMOTE"));
    }

    #[test]
    fn unregistered_wwid_is_degraded() {
        let h = PathHealth {
            wwid_registered: Some(false),
            ..healthy()
        };
        assert!(degradation(&h).unwrap().contains("wwids"));
    }

    #[test]
    fn unreadable_signals_fail_open() {
        // Undetermined local-path / wwids signals must not degrade.
        let h = PathHealth {
            local_path_active: None,
            wwid_registered: None,
            ..healthy()
        };
        assert_eq!(degradation(&h), None);
    }

    #[test]
    fn no_paths_at_all_is_degraded() {
        let h = PathHealth::default();
        assert!(degradation(&h).unwrap().contains("no iSCSI path"));
    }
}
