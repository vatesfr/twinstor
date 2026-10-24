use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::config::Config;

/// A DRBD device presenting fewer than this many 512-byte sectors is a
/// half-attached device briefly reporting a phantom size (the source of
/// "cmd exceeds last lba 64" errors), not a real TwinStor volume, which is
/// GB-scale (hundreds of millions of sectors). A backstore built on a phantom
/// publishes a malformed LUN that the node's own loopback initiator reads past
/// the end of, giving I/O errors and then a fence or reboot. The 1 MiB floor
/// is far above the phantom and far below any real SR volume.
const MIN_SANE_LUN_SECTORS: u64 = 2048;
/// Bounded wait for the DRBD device to present its real size before building.
/// A force_primary that just ran usually settles in under a second, so a short
/// retry closes the window without a full tick's delay; if it never settles,
/// start() bails and the caller holds and retries next tick.
const DEVICE_READY_TRIES: u32 = 5;
const DEVICE_READY_GAP: Duration = Duration::from_millis(200);

/// Whether a reported sector count is the phantom (half-attached) size rather
/// than a real volume. Separate so the floor is unit-tested without a device.
fn is_phantom_size(sectors: u64) -> bool {
    sectors < MIN_SANE_LUN_SECTORS
}

/// The sector count the kernel is currently presenting for `device`, or None if
/// it cannot be read. Resolves symlinks (such as /dev/drbd/by-res/…) to /dev/drbdN
/// so the sysfs lookup lands on the real block device.
fn device_sectors(device: &Path) -> Option<u64> {
    let real = std::fs::canonicalize(device).ok()?;
    let name = real.file_name()?.to_str()?;
    std::fs::read_to_string(format!("/sys/block/{name}/size"))
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

#[async_trait::async_trait]
pub trait IscsiControl: Send + Sync {
    async fn start(&mut self, config: &Config) -> Result<()>;
    async fn stop(&mut self) -> Result<()>;
}

/// The targetcli commands the controller issues, behind a seam so the
/// start/stop bookkeeping is unit-tested without LIO.
#[async_trait::async_trait]
trait TargetCli: Send + Sync {
    /// Run one targetcli command; Err on failure or timeout.
    async fn run(&self, cmd: &str) -> Result<()>;
    /// `targetcli ls <path>` output; None on failure or timeout.
    async fn ls(&self, path: &str) -> Option<String>;
}

struct RealTargetCli;

#[async_trait::async_trait]
impl TargetCli for RealTargetCli {
    async fn run(&self, cmd: &str) -> Result<()> {
        run_targetcli(cmd).await
    }
    async fn ls(&self, path: &str) -> Option<String> {
        // Bounded so a wedged targetcli cannot freeze the tick.
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            tokio::process::Command::new("targetcli")
                .arg(format!("ls {path}"))
                .kill_on_drop(true)
                .output(),
        )
        .await
        .ok()?
        .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

pub struct IscsiController {
    active: bool,
    // Remembered from the last start() so stop() can close the listening
    // portal first during teardown (see stop()).
    portal_spec: Option<(String, u16)>,
    // Whether this process has already settled if a target it did not build
    // itself exists (see stop()). `active` starts false on every daemon start
    // while LIO keeps whatever an earlier run exported.
    adoption_checked: bool,
    cli: Box<dyn TargetCli>,
}

impl IscsiController {
    pub fn new() -> Self {
        Self::with_cli(Box::new(RealTargetCli))
    }

    fn with_cli(cli: Box<dyn TargetCli>) -> Self {
        Self {
            active: false,
            portal_spec: None,
            adoption_checked: false,
            cli,
        }
    }
}

#[async_trait::async_trait]
impl IscsiControl for IscsiController {
    async fn start(&mut self, config: &Config) -> Result<()> {
        if self.active {
            return Ok(());
        }

        let iqn = &config.iscsi.target_iqn;
        let device = &config.storage.drbd_device;
        let port = config.iscsi.portal_port;
        let serial = &config.iscsi.lun_serial;
        info!(iqn = %iqn, device = %device.display(), serial = %serial, "starting iSCSI target");

        // Check if iSCSI is already configured and serving
        let chap_expected = config.iscsi.chap_user.is_some()
            && config.iscsi.chap_password.is_some()
            && config.iscsi.initiator_iqns.is_some();
        if self.is_configured(iqn, chap_expected).await {
            info!("iSCSI target already configured, skipping setup");
            self.active = true;
            self.adoption_checked = true;
            self.portal_spec = Some((iqn.clone(), port));
            return Ok(());
        }

        // From here this process mutates LIO itself. Until the build completes
        // (it can fail, or be cancelled by the caller's timeout, after the
        // backstore exists) the next stop() must check for what was left
        // instead of trusting that nothing is exported.
        self.adoption_checked = false;

        // Clean slate and configure from scratch
        info!("clearing existing iSCSI configuration");
        if let Err(e) = self.cli.run("clearconfig confirm=True").await {
            warn!(error = %e, "clearconfig failed (may be empty, continuing)");
        }

        // Atomic-publish gate: never build the backstore on a device that is
        // not presenting its real size. LIO records the device size when the
        // backstore is created, so a device caught mid-promote (a phantom of
        // about 64 sectors) produces a malformed LUN that the local loopback
        // initiator reads past, giving I/O errors and a fence. The check runs
        // right before create (with a short retry for a just-issued
        // force_primary to settle), so only one command separates them. With
        // the portal opened last (below), publication is atomic: an initiator
        // can only reach a correctly sized LUN. On a persistent phantom, bail;
        // the caller holds and retries, and the cleared config gives the
        // loopback connection-refused, never a malformed LUN.
        let mut sane = false;
        for attempt in 0..DEVICE_READY_TRIES {
            match device_sectors(device) {
                Some(s) if !is_phantom_size(s) => {
                    sane = true;
                    break;
                }
                other => warn!(
                    device = %device.display(),
                    sectors = ?other,
                    "DRBD device not presenting its real size yet (phantom/half-attached); \
                     waiting before building the iSCSI target"
                ),
            }
            if attempt + 1 < DEVICE_READY_TRIES {
                tokio::time::sleep(DEVICE_READY_GAP).await;
            }
        }
        if !sane {
            bail!(
                "iSCSI target not built: DRBD device {} is not presenting its real size \
                 (phantom/half-attached); holding to avoid publishing a malformed LUN",
                device.display()
            );
        }

        self.cli
            .run(&format!(
                "/backstores/block create twinstor {}",
                device.display()
            ))
            .await?;

        let serial_path = "/sys/kernel/config/target/core/iblock_0/twinstor/wwn/vpd_unit_serial";
        tokio::fs::write(serial_path, format!("{serial}\n"))
            .await
            .with_context(|| format!("setting LUN serial at {serial_path}"))?;

        self.cli.run(&format!("/iscsi create {iqn}")).await?;
        self.cli
            .run(&format!(
                "/iscsi/{iqn}/tpg1/luns create /backstores/block/twinstor"
            ))
            .await?;

        if let (Some(user), Some(pass), Some(initiators)) = (
            &config.iscsi.chap_user,
            &config.iscsi.chap_password,
            &config.iscsi.initiator_iqns,
        ) {
            self.cli.run(&format!(
                "/iscsi/{iqn}/tpg1 set attribute authentication=1 demo_mode_write_protect=0 generate_node_acls=0"
            )).await?;
            for initiator in initiators {
                self.cli
                    .run(&format!("/iscsi/{iqn}/tpg1/acls create {initiator}"))
                    .await?;
                self.cli
                    .run(&format!(
                        "/iscsi/{iqn}/tpg1/acls/{initiator} set auth userid={user} password={pass}"
                    ))
                    .await?;
            }
            // XCP-ng's SM performs CHAP-authenticated SendTargets discovery
            // whenever the SR has CHAP credentials; LIO rejects that unless
            // discovery auth is enabled with the same credentials (otherwise
            // sr-probe/sr-create fail with "discovery login rejected").
            self.cli
                .run(&format!(
                    "/iscsi set discovery_auth enable=1 userid={user} password={pass}"
                ))
                .await?;
        } else {
            self.cli.run(&format!(
                "/iscsi/{iqn}/tpg1 set attribute authentication=0 demo_mode_write_protect=0 generate_node_acls=1"
            )).await?;
        }
        self.cli
            .run(&format!("/iscsi/{iqn}/tpg1/portals delete 0.0.0.0 {port}"))
            .await
            .ok();
        self.cli
            .run(&format!("/iscsi/{iqn}/tpg1/portals create 0.0.0.0 {port}"))
            .await?;
        self.cli.run("saveconfig").await?;

        self.active = true;
        self.adoption_checked = true;
        self.portal_spec = Some((iqn.clone(), port));
        info!("iSCSI target started");
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if !self.active {
            // After a daemon restart nothing has adopted the target an earlier
            // run exported (only start() does), yet LIO still holds the DRBD
            // device open through it. A teardown path reached before start()
            // (the isolated stand-down, the split-brain loser, a stuck-Primary
            // demote) would otherwise skip the stop and fail its demote with
            // the device held open. Check once per process; when in doubt, tear
            // down (deleting an absent target is harmless). Later stops on a
            // target this process stopped itself stay a cheap no-op.
            if self.adoption_checked {
                return Ok(());
            }
            // Settled only once the probe answered "nothing here", or the
            // teardown below completed: a caller timeout that cancels this
            // stop part-way must leave the next one looking again.
            if !self.target_left_over().await {
                self.adoption_checked = true;
                return Ok(());
            }
            warn!("an iSCSI target from an earlier daemon run is still exported; stopping it");
        }

        info!("stopping iSCSI target");

        // Close the listening portal before dismantling the target: with the
        // socket gone, retrying initiators get connection-refused at once
        // instead of parking 15 s login threads inside LIO. A login storm
        // against a half-torn-down target can delay the next start()'s
        // configfs writes past the wedge watchdog, which then self-fences the
        // node.
        let spec = self.portal_spec.take();
        if let Some((iqn, port)) = &spec {
            self.cli
                .run(&format!("/iscsi/{iqn}/tpg1/portals delete 0.0.0.0 {port}"))
                .await
                .ok();
        }
        // Deleting the backstore is what releases the DRBD device: rtslib
        // removes every LUN that maps the storage object with it. Already
        // absent counts as released.
        let backstore_gone = self
            .cli
            .run("/backstores/block delete twinstor")
            .await
            .is_ok()
            || !self.target_left_over().await;
        // Then drop the target itself, now LUN-less, so no listener or ACL
        // lingers. By IQN when this process built it; a target adopted from an
        // earlier run has no known IQN, and clearconfig is what start() runs
        // anyway. A failure here holds no device: start() rebuilds from a
        // cleared config whatever is left.
        let target_cmd = match &spec {
            Some((iqn, _)) => format!("/iscsi delete {iqn}"),
            None => "clearconfig confirm=True".to_string(),
        };
        if let Err(e) = self.cli.run(&target_cmd).await {
            warn!(error = %e, "iSCSI backstore released, but the empty target could not be removed");
        }
        // The live state no longer depends on saveconfig: whatever it answers,
        // this process is not exporting. A backstore that is still there leaves
        // the next stop() to look again rather than trust a no-op.
        self.active = false;
        self.adoption_checked = backstore_gone;
        if let Err(e) = self.cli.run("saveconfig").await {
            // Only the persisted copy is stale; the live target is down, which
            // is what callers act on.
            warn!(error = %e, "iSCSI target stopped, but saveconfig failed");
        }
        if !backstore_gone {
            bail!(
                "iSCSI teardown incomplete (backstore delete failed): LIO may still hold the device"
            );
        }
        info!("iSCSI target stopped");
        Ok(())
    }
}

impl IscsiController {
    async fn is_configured(&self, iqn: &str, chap_expected: bool) -> bool {
        // Check that our own target exists with a LUN mapped. Return false
        // only when it is certainly not configured; on any error (targetcli
        // crash, timeout) assume configured to avoid a destructive
        // clearconfig.
        match self.cli.ls("/iscsi").await {
            Some(out) => target_is_configured(&out, iqn, chap_expected),
            None => true,
        }
    }

    /// Whether LIO still exports anything this daemon owns: the `twinstor`
    /// block backstore. Unreadable reads as present (a teardown of nothing is
    /// harmless; skipping one that is needed is not).
    async fn target_left_over(&self) -> bool {
        match self.cli.ls("/backstores/block").await {
            Some(out) => backstore_present(&out),
            None => true,
        }
    }
}

/// Whether `targetcli ls /backstores/block` lists the `twinstor` backstore.
fn backstore_present(ls_output: &str) -> bool {
    ls_output
        .lines()
        .any(|l| l.split_whitespace().any(|w| w == "twinstor"))
}

/// Decide whether our target is already configured and serving, from the
/// `targetcli ls /iscsi` output. Returns true only when the target is present
/// with a LUN mapped (and, when CHAP is expected, has at least one ACL).
///
/// A target with auth enforced but no ACLs is a half-built leftover (an
/// earlier run failed on an invalid initiator IQN, say): nobody can log in, so
/// rebuilding is safe and required; treating it as present would leave it
/// serving no one indefinitely.
fn target_is_configured(ls_output: &str, iqn: &str, chap_expected: bool) -> bool {
    // Judge only this target's own subtree: another LIO target on the host
    // without a portal or ACL must not make a healthy twinstor target look
    // half torn down. An unfamiliar layout falls back to the whole listing
    // for presence and never triggers a rebuild.
    let (scope, scoped) = match target_subtree(ls_output, iqn) {
        Some(sub) => (sub, true),
        None => (ls_output.to_string(), false),
    };
    let present = ls_output.contains(iqn) && (scope.contains("LUNs: 1") || scope.contains("lun0"));
    if !scoped {
        return present;
    }
    let ls_output = scope.as_str();
    if present && chap_expected && ls_output.contains("[ACLs: 0]") {
        warn!("iSCSI target exists but has no ACLs (half-configured); rebuilding");
        return false;
    }
    // A teardown that closed the portal and then failed leaves target and LUN
    // without a listening portal: adopting it would export nothing. Only an
    // explicit zero count rebuilds (an unfamiliar listing still adopts).
    if present && ls_output.contains("[Portals: 0]") {
        warn!("iSCSI target exists but has no portal (half torn down); rebuilding");
        return false;
    }
    present
}

/// The lines of `targetcli ls` that belong to the node naming `iqn`: that
/// line and every following line indented deeper. `None` when no line names
/// it.
fn target_subtree(ls_output: &str, iqn: &str) -> Option<String> {
    let indent = |l: &str| l.find("o-").unwrap_or(usize::MAX);
    let mut lines = ls_output.lines();
    let head = lines
        .by_ref()
        .find(|l| l.contains(iqn) && l.contains("o-"))?;
    let base = indent(head);
    let mut out = vec![head];
    out.extend(lines.take_while(|l| indent(l) > base && indent(l) != usize::MAX));
    Some(out.join("\n"))
}

/// Whether an iSCSI session line for `portal` ("ip:port") is present in
/// `iscsiadm -m session` output. Session lines look like
/// `tcp: [1] 192.0.2.101:3260,1 iqn...`; the match is anchored on the
/// surrounding space and comma so one portal cannot match inside a longer
/// address (`192.0.2.10` must not match `192.0.2.101`).
fn session_line_present(iscsiadm_output: &str, portal: &str) -> bool {
    iscsiadm_output.contains(&format!(" {portal},"))
}

/// iSCSI session recovery timeout for the TwinStor sessions, in seconds.
///
/// A failed path must be reported to multipath well inside XHA's statefile
/// timeout (the pool's HA timeout: 120 s or 90 s with TwinStor's defaults, as
/// low as 30 s on a pool tuned down): with two paths, I/O on the failed one is
/// queued until
/// session recovery gives up, and if that window plus the noop detection
/// (noop_out_interval + noop_out_timeout, 5 + 5 s stock) exceeds the XHA
/// budget, a single path failure freezes the statefile long enough to get
/// the host self-fenced. 5 + 5 + 5 = 15 s worst case, a 2x margin even at 30 s. The stock
/// XCP-ng values land past the budget (replacement_timeout 120 in
/// iscsid.conf, 25 set live on the sessions at PBD plug), which is why the
/// node records are overridden at discovery and the live value is enforced
/// by the daemon's path watchdog.
pub const REPLACEMENT_TIMEOUT_SECS: u32 = 5;

/// The OUTPUT rule that fences this node off the peer's iSCSI portal during
/// a fate-shared split (see the fence-peer handler, which adds the same rule).
fn peer_portal_rule(peer_ip: &str, port: u16) -> Vec<String> {
    format!(
        "-d {peer_ip}/32 -p tcp -m tcp --dport {port} -m comment --comment \
         twinstor-peer-portal-fence -j REJECT --reject-with tcp-reset"
    )
    .split_whitespace()
    .map(str::to_string)
    .collect()
}

async fn iptables_output(op: &str, peer_ip: &str, port: u16) -> bool {
    let mut args: Vec<String> = vec!["-w".into(), "3".into(), op.into(), "OUTPUT".into()];
    if op == "-I" {
        args.push("1".into());
    }
    args.extend(peer_portal_rule(peer_ip, port));
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    crate::cmd::run_cmd_for("iptables", &refs, 10).await.is_ok()
}

/// Whether the peer-portal fence rule is installed. Only iptables' "no such
/// rule" (exit 1) reads as absent: a lock timeout or any other failure reads
/// as present, so a removal is never reported done on an error.
pub async fn peer_portal_rule_present(peer_ip: &str, port: u16) -> bool {
    peer_portal_rule_state(peer_ip, port).await != Some(false)
}

/// The peer-portal fence rule as iptables reports it: `Some(true)` installed
/// (exit 0), `Some(false)` absent (exit 1), `None` unknown (a lock timeout,
/// any other failure). Each caller fails closed its own way: a removal is not
/// done on `None`, an installation is not either.
pub async fn peer_portal_rule_state(peer_ip: &str, port: u16) -> Option<bool> {
    let mut args: Vec<String> = vec!["-w".into(), "3".into(), "-C".into(), "OUTPUT".into()];
    args.extend(peer_portal_rule(peer_ip, port));
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new("iptables")
            .args(&args)
            .kill_on_drop(true)
            .output(),
    )
    .await;
    match out {
        Ok(Ok(o)) if o.status.code() == Some(0) => Some(true),
        Ok(Ok(o)) if o.status.code() == Some(1) => Some(false),
        _ => None,
    }
}

/// Install the peer-portal fence rule (idempotent). `false` on failure. An
/// unknown state is not taken for installed: the insert runs (a duplicate is
/// harmless, the removal loops until none is left).
pub async fn add_peer_portal_rule(peer_ip: &str, port: u16) -> bool {
    peer_portal_rule_state(peer_ip, port).await == Some(true)
        || iptables_output("-I", peer_ip, port).await
}

/// Remove every copy of the peer-portal fence rule. `false` if one stays.
pub async fn remove_peer_portal_rule(peer_ip: &str, port: u16) -> bool {
    for _ in 0..8 {
        if !peer_portal_rule_present(peer_ip, port).await {
            return true;
        }
        let _ = iptables_output("-D", peer_ip, port).await;
    }
    !peer_portal_rule_present(peer_ip, port).await
}

/// The SCSI disks (`sdc`) attached through the session(s) to `portal`, from
/// `iscsiadm -m session -P 3` (bounded). Empty on any failure.
pub async fn session_disks(portal: &str) -> Vec<String> {
    match crate::cmd::cmd_output_for("iscsiadm", &["-m", "session", "-P", "3"], 10).await {
        Ok(text) => parse_session_disks(&text, portal),
        Err(_) => Vec::new(),
    }
}

/// Parser for `session_disks`.
pub fn parse_session_disks(text: &str, portal: &str) -> Vec<String> {
    let mut cur = false;
    let mut v = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if let Some(p) = l.strip_prefix("Current Portal:") {
            cur = p.trim().starts_with(portal);
        } else if cur
            && let Some(rest) = l.strip_prefix("Attached scsi disk")
            && let Some(d) = rest.split_whitespace().next()
        {
            v.push(d.to_string());
        }
    }
    v
}

/// Whether an iSCSI session to `portal` ("ip:port") is established (see
/// `session_line_present`).
pub async fn session_exists(portal: &str) -> bool {
    // Bounded: reached inline from the tick (restore, plug); a wedged iscsid
    // must not hang the loop. On any failure or timeout, report no session.
    match crate::cmd::cmd_output_for("iscsiadm", &["-m", "session"], 10).await {
        Ok(text) => session_line_present(&text, portal),
        Err(_) => false,
    }
}

/// Discover an iSCSI portal and log in to the discovered targets, with CHAP
/// when configured. With CHAP, plain `iscsiadm -m discovery` no longer works:
/// the target enforces discovery auth (see `start`), so discovery must go
/// through discoverydb with credentials, and the node records need session
/// credentials before login. Best-effort: failures are logged and tolerated
/// (the local portal is down while this node is still syncing, for example,
/// because the readiness gate holds it down).
pub async fn discover_and_login(portal: &str, chap: Option<(&str, &str)>) {
    // Quiet runner: discovery often races a peer target that is still
    // starting; callers expect and retry those failures, so they are not
    // logged as warnings.
    use crate::cmd::run_cmd_quiet as run_cmd;
    if let Some((user, pass)) = chap {
        let db = ["-m", "discoverydb", "-t", "sendtargets", "-p", portal];
        let _ = run_cmd("iscsiadm", &[&db[..], &["--op", "new"]].concat()).await;
        for (key, val) in [
            ("discovery.sendtargets.auth.authmethod", "CHAP"),
            ("discovery.sendtargets.auth.username", user),
            ("discovery.sendtargets.auth.password", pass),
        ] {
            let _ = run_cmd(
                "iscsiadm",
                &[&db[..], &["--op", "update", "-n", key, "-v", val]].concat(),
            )
            .await;
        }
        let _ = run_cmd("iscsiadm", &[&db[..], &["--discover"]].concat()).await;
        for (key, val) in [
            ("node.session.auth.authmethod", "CHAP"),
            ("node.session.auth.username", user),
            ("node.session.auth.password", pass),
        ] {
            let _ = run_cmd(
                "iscsiadm",
                &[
                    "-m", "node", "-p", portal, "--op", "update", "-n", key, "-v", val,
                ],
            )
            .await;
        }
    } else {
        let _ = run_cmd("iscsiadm", &["-m", "discovery", "-t", "st", "-p", portal]).await;
    }
    // Failure-detection budget (see REPLACEMENT_TIMEOUT_SECS): the node
    // record covers this login and future ones; the path watchdog enforces
    // the live session value against later overrides.
    let _ = run_cmd(
        "iscsiadm",
        &[
            "-m",
            "node",
            "-p",
            portal,
            "--op",
            "update",
            "-n",
            "node.session.timeo.replacement_timeout",
            "-v",
            &REPLACEMENT_TIMEOUT_SECS.to_string(),
        ],
    )
    .await;
    let _ = run_cmd("iscsiadm", &["-m", "node", "-p", portal, "--login"]).await;
}

/// Log out of all iSCSI initiator sessions on this host, cutting this node's
/// client access to the SR entirely. Used by the isolated-master stand-down: a
/// node that has lost the client network must stop writing the shared LUN by
/// any path (its own target, or the peer's target through multipath over the
/// still-up direct link), so its VMs lose their disks and XHA restarts them on
/// the reachable peer. Demoting local DRBD alone is not enough: the multipath
/// path to the peer's target would keep the VMs writing. Best-effort and
/// idempotent: "no active sessions" is success.
pub async fn logout_all_sessions() {
    use crate::cmd::run_cmd_quiet as run_cmd;
    // `-u` on the whole node database logs out every session; the "no
    // matching sessions" non-zero exit is tolerated. Node records are kept
    // (deleting them risks the XCP-ng SR's own reconnection): the daemon's
    // park logic suppresses re-login while stood down, and discovery and
    // login re-establish the sessions on rejoin.
    let _ = run_cmd("iscsiadm", &["-m", "node", "-u"]).await;
}

/// Log out this host's initiator session to a single portal, leaving every
/// other portal's session up. Used by the split-brain loser to drop its own
/// local loopback path before its DRBD device goes Secondary (read-only) or is
/// torn down. Otherwise the loopback session lingers and its in-flight writes
/// hit the write-protected or mis-sized local device, returning SCSI
/// Data-Protect or "exceeds last lba" hard errors that multipath passes to the
/// guests (which remount read-only) instead of failing the path over to the
/// winner's remote portal. The node record is kept so the post-convergence
/// `plug_and_multipath` logs in again once this node is a healthy Primary.
/// Best-effort and idempotent: "no matching sessions" is success.
pub async fn logout_portal(portal: &str) {
    use crate::cmd::run_cmd_quiet as run_cmd;
    let _ = run_cmd("iscsiadm", &["-m", "node", "-p", portal, "-u"]).await;
}

async fn run_targetcli(cmd: &str) -> Result<()> {
    // Bounded: targetcli can wedge on a stuck backstore or a login storm, and
    // iSCSI start/stop runs inline from the tick at sites that do not all wrap
    // it in a timeout, so an unbounded call would freeze the loop. The bound
    // is inline rather than through crate::cmd to keep both stdout and stderr
    // in the error. 60 s means targetcli is wedged; kill_on_drop reaps the
    // abandoned child.
    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tokio::process::Command::new("targetcli")
            .arg(cmd)
            .kill_on_drop(true)
            .output(),
    )
    .await
    {
        Ok(r) => r.with_context(|| format!("running targetcli {cmd}"))?,
        Err(_) => bail!("targetcli {cmd}: timed out after 60s"),
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!("targetcli {cmd} failed: {stdout} {stderr}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_disks_are_read_per_portal() {
        let t = "Target: iqn.x (non-flash)\n\tCurrent Portal: 192.0.2.49:3260,1\n\tPersistent Portal: 192.0.2.49:3260,1\n\t\tAttached scsi disk sdb\t\tState: running\nTarget: iqn.x (non-flash)\n\tCurrent Portal: 192.0.2.47:3260,1\n\t\tAttached scsi disk sdc\t\tState: running\n";
        assert_eq!(parse_session_disks(t, "192.0.2.49:3260"), vec!["sdb"]);
        assert_eq!(parse_session_disks(t, "192.0.2.47:3260"), vec!["sdc"]);
        assert!(parse_session_disks(t, "10.0.0.1:3260").is_empty());
    }

    const IQN: &str = "iqn.2026-01.com.xcp-ng:twinstor-data";

    /// Records every command; `ls` answers from a fixed listing (None = the
    /// targetcli call failed).
    struct FakeCli {
        listing: Option<&'static str>,
        log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        /// Commands starting with this fail.
        fail_on: Option<&'static str>,
        /// Commands starting with this never return (a wedged targetcli).
        hang_on: Option<&'static str>,
    }

    #[async_trait::async_trait]
    impl TargetCli for FakeCli {
        async fn run(&self, cmd: &str) -> Result<()> {
            self.log.lock().unwrap().push(cmd.to_string());
            if self.hang_on.is_some_and(|h| cmd.starts_with(h)) {
                std::future::pending::<()>().await;
            }
            match self.fail_on {
                Some(f) if cmd.starts_with(f) => bail!("{cmd} failed"),
                _ => Ok(()),
            }
        }
        async fn ls(&self, path: &str) -> Option<String> {
            self.log.lock().unwrap().push(format!("ls {path}"));
            self.listing.map(str::to_string)
        }
    }

    fn fresh(
        listing: Option<&'static str>,
    ) -> (
        IscsiController,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            IscsiController::with_cli(Box::new(FakeCli {
                listing,
                log: log.clone(),
                fail_on: None,
                hang_on: None,
            })),
            log,
        )
    }

    fn failing(
        listing: Option<&'static str>,
        fail_on: &'static str,
    ) -> (
        IscsiController,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            IscsiController::with_cli(Box::new(FakeCli {
                listing,
                log: log.clone(),
                fail_on: Some(fail_on),
                hang_on: None,
            })),
            log,
        )
    }

    fn cfg() -> Config {
        toml::from_str(
            r#"
[node]
name = "node-a"
peer = "node-b"
[network]
peer_address = "10.0.0.2"
drbd_port = 7789
[storage]
drbd_resource = "data"
backing_device = "/dev/null"
drbd_device = "/dev/twinstor-test-no-such-drbd"
[iscsi]
target_iqn = "iqn.2026-01.com.xcp-ng:twinstor-data"
lun_serial = "twinstor0001"
[timers]
poll_interval_ms = 10
"#,
        )
        .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn a_start_that_fails_after_touching_lio_leaves_the_next_stop_effective() {
        // This process already stopped a target (adoption settled), then a
        // start clears the config and fails part-way: whatever it left must
        // be torn down by the next stop, not skipped as "nothing exported".
        let (mut c, log) = fresh(Some(LEFT_OVER));
        c.stop().await.unwrap();
        assert!(
            c.start(&cfg()).await.is_err(),
            "the test device never presents a real size"
        );
        log.lock().unwrap().clear();
        c.stop().await.unwrap();
        let cmds = log.lock().unwrap().clone();
        assert!(
            cmds.contains(&"ls /backstores/block".to_string())
                && cmds.contains(&"/backstores/block delete twinstor".to_string()),
            "{cmds:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_saveconfig_is_still_a_stop_but_a_failed_delete_is_reported() {
        // The live target is down: callers (the failback dance restores
        // serving on Ok) must not read a stale persisted copy as "not stopped".
        let (mut c, _log) = failing(Some(LEFT_OVER), "saveconfig");
        assert!(c.stop().await.is_ok());
        assert!(
            !c.active,
            "the live state is torn down whatever saveconfig answered"
        );
        // A delete that failed is an error, and the next stop looks again.
        let (mut c, log) = failing(Some(LEFT_OVER), "/backstores/block delete");
        assert!(c.stop().await.is_err(), "LIO may still hold the device");
        log.lock().unwrap().clear();
        let _ = c.stop().await;
        assert!(
            log.lock()
                .unwrap()
                .contains(&"ls /backstores/block".to_string())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_cancelled_mid_teardown_leaves_the_next_stop_looking_again() {
        // The leftover teardown wedges and the caller's timeout cancels it:
        // the next stop must look again, not trust a settled no-op.
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut c = IscsiController::with_cli(Box::new(FakeCli {
            listing: Some(LEFT_OVER),
            log: log.clone(),
            fail_on: None,
            hang_on: Some("/backstores/block delete"),
        }));
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(30), c.stop())
                .await
                .is_err()
        );
        assert!(!c.adoption_checked);
        c.cli = Box::new(FakeCli {
            listing: Some(LEFT_OVER),
            log: log.clone(),
            fail_on: None,
            hang_on: None,
        });
        log.lock().unwrap().clear();
        c.stop().await.unwrap();
        assert!(
            log.lock()
                .unwrap()
                .contains(&"ls /backstores/block".to_string()),
            "{:?}",
            log.lock().unwrap()
        );
    }

    const LEFT_OVER: &str = "o- block .......................... [Storage Objects: 1]\n  \
        o- twinstor ........ [/dev/drbd0 (100.0GiB) write-thru activated]\n";
    const EMPTY: &str = "o- block .......................... [Storage Objects: 0]\n";

    #[tokio::test]
    async fn stop_tears_down_a_target_left_by_an_earlier_daemon_run() {
        // A restarted daemon has not run start(), so nothing adopted the
        // target its predecessor exported. stop() must still withdraw it, or
        // the demote that follows fails with the device held open.
        let (mut c, log) = fresh(Some(LEFT_OVER));
        c.stop().await.unwrap();
        let cmds = log.lock().unwrap().clone();
        assert_eq!(
            cmds,
            [
                "ls /backstores/block",
                "/backstores/block delete twinstor",
                "clearconfig confirm=True",
                "saveconfig"
            ]
        );
        // Stopped by this process now: further stops (the Degraded tick calls
        // it every tick) cost nothing, no probe and no command.
        c.stop().await.unwrap();
        assert_eq!(log.lock().unwrap().len(), cmds.len());
    }

    #[tokio::test]
    async fn stop_probes_once_and_skips_when_nothing_is_exported() {
        let (mut c, log) = fresh(Some(EMPTY));
        c.stop().await.unwrap();
        c.stop().await.unwrap();
        assert_eq!(*log.lock().unwrap(), ["ls /backstores/block"]);
    }

    #[tokio::test]
    async fn stop_tears_down_when_the_listing_is_unreadable() {
        let (mut c, log) = fresh(None);
        c.stop().await.unwrap();
        assert!(
            log.lock()
                .unwrap()
                .iter()
                .any(|c| c == "/backstores/block delete twinstor")
        );
    }

    #[tokio::test]
    async fn stop_releases_the_backstore_then_removes_its_own_target_by_iqn() {
        // targetcli has no `/iscsi delete_all`; the device is released by the
        // backstore delete (its LUNs go with it), then the empty target is
        // removed by the IQN this process built it with.
        let (mut c, log) = fresh(Some(EMPTY));
        c.active = true;
        c.adoption_checked = true;
        c.portal_spec = Some((IQN.to_string(), 3260));
        c.stop().await.unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            [
                format!("/iscsi/{IQN}/tpg1/portals delete 0.0.0.0 3260"),
                "/backstores/block delete twinstor".to_string(),
                format!("/iscsi delete {IQN}"),
                "saveconfig".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn undeletable_target_after_backstore_delete_is_not_a_failed_stop() {
        // A failed target delete after a successful backstore delete: the
        // device is released, so the discard must not read it as LIO holding
        // it.
        let (mut c, _log) = failing(Some(EMPTY), "clearconfig");
        c.active = true;
        assert!(c.stop().await.is_ok());
        assert!(c.adoption_checked);
        // A backstore delete that failed while the listing still shows it is.
        let (mut c, _log) = failing(Some(LEFT_OVER), "/backstores/block delete");
        c.active = true;
        assert!(c.stop().await.is_err());
        assert!(!c.adoption_checked);
    }

    #[test]
    fn backstore_listing_parse() {
        assert!(backstore_present(LEFT_OVER));
        assert!(!backstore_present(EMPTY));
        assert!(!backstore_present("o- twinstor-old ..... [/dev/drbd1]\n"));
    }

    #[test]
    fn phantom_size_floor_separates_half_attached_from_real_volumes() {
        // The phantom size ("cmd exceeds last lba 64") and 0 are rejected; a
        // real ~211 GB volume (413,071,616 sectors) and anything at or above
        // the 1 MiB floor is accepted.
        assert!(is_phantom_size(0));
        assert!(is_phantom_size(64));
        assert!(is_phantom_size(MIN_SANE_LUN_SECTORS - 1));
        assert!(!is_phantom_size(MIN_SANE_LUN_SECTORS));
        assert!(!is_phantom_size(413_071_616));
    }

    #[test]
    fn configured_when_target_present_with_lun() {
        let ls = format!("o- iscsi\n  o- {IQN} ......\n    o- tpg1\n      o- luns [LUNs: 1]\n");
        assert!(target_is_configured(&ls, IQN, false));
    }

    #[test]
    fn configured_via_lun0_marker() {
        // Older targetcli prints individual "lun0" entries rather than a count.
        let ls = format!("o- {IQN}\n  o- lun0 .....\n");
        assert!(target_is_configured(&ls, IQN, false));
    }

    #[test]
    fn not_configured_when_iqn_absent() {
        let ls = "o- iscsi\n  o- iqn.2026-01.com.xcp-ng:other [LUNs: 1]\n";
        assert!(!target_is_configured(ls, IQN, false));
    }

    #[test]
    fn not_configured_when_no_lun() {
        let ls = format!("o- iscsi\n  o- {IQN} [LUNs: 0]\n");
        assert!(!target_is_configured(&ls, IQN, false));
    }

    #[test]
    fn half_built_with_chap_and_no_acls_triggers_rebuild() {
        // Auth expected but zero ACLs: nobody can log in. Must rebuild.
        let ls = format!("o- {IQN} [LUNs: 1]\n  o- acls [ACLs: 0]\n");
        assert!(!target_is_configured(&ls, IQN, true));
    }

    #[test]
    fn zero_acls_without_chap_expected_is_still_configured() {
        // generate_node_acls=1 (no CHAP): [ACLs: 0] is normal, not a leftover.
        let ls = format!("o- {IQN} [LUNs: 1]\n  o- acls [ACLs: 0]\n");
        assert!(target_is_configured(&ls, IQN, false));
    }

    #[test]
    fn another_targets_missing_portal_does_not_rebuild_ours() {
        let ls = format!(
            "o- iscsi [Targets: 2]\n  o- iqn.2003-01.org.other:x [TPGs: 1]\n  | o- tpg1 [no-gen-acls]\n  |   o- acls [ACLs: 0]\n  |   o- portals [Portals: 0]\n  o- {IQN} [TPGs: 1]\n    o- tpg1 [gen-acls]\n      o- luns [LUNs: 1]\n      o- portals [Portals: 1]\n"
        );
        assert!(target_is_configured(&ls, IQN, false));
        assert!(
            target_is_configured(&ls, IQN, true),
            "the other target's ACLs do not count either"
        );
    }

    #[test]
    fn a_target_without_a_portal_is_rebuilt_not_adopted() {
        let ls = format!("o- {IQN} [LUNs: 1]\n  o- portals [Portals: 0]\n");
        assert!(!target_is_configured(&ls, IQN, false));
        let ls = format!("o- {IQN} [LUNs: 1]\n  o- portals [Portals: 1]\n");
        assert!(target_is_configured(&ls, IQN, false));
    }

    #[test]
    fn chap_expected_with_acls_present_is_configured() {
        let ls = format!("o- {IQN} [LUNs: 1]\n  o- acls [ACLs: 1]\n");
        assert!(target_is_configured(&ls, IQN, true));
    }

    #[test]
    fn session_match_is_anchored_on_comma() {
        let out = "tcp: [1] 192.0.2.101:3260,1 iqn.2026-01.com.xcp-ng:twinstor-data\n";
        assert!(session_line_present(out, "192.0.2.101:3260"));
        // A shorter portal must not match inside a longer address.
        assert!(!session_line_present(out, "192.0.2.10:3260"));
    }

    #[test]
    fn session_absent_when_no_match() {
        assert!(!session_line_present("", "192.0.2.101:3260"));
        let out = "tcp: [1] 10.0.0.5:3260,1 iqn...\n";
        assert!(!session_line_present(out, "192.0.2.101:3260"));
    }
}
