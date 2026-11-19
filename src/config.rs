use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct Config {
    pub node: NodeConfig,
    pub network: NetworkConfig,
    pub storage: StorageConfig,
    pub iscsi: IscsiConfig,
    #[serde(default)]
    pub timers: TimerConfig,
    #[serde(default)]
    pub failback: FailbackConfig,
    #[serde(default)]
    pub ha: HaConfig,
    #[serde(default)]
    pub reboot_drain: RebootDrainConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NodeConfig {
    pub name: String,
    pub peer: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct NetworkConfig {
    pub peer_address: IpAddr,
    pub drbd_port: u16,
    /// The operator deployed with `--allow-no-tiebreaker`: an empty fence-peer
    /// probe set (no default gateway, no ping-targets) is an accepted choice for
    /// this site (air-gapped or no-gateway segment), so the tiebreaker-health
    /// monitor does not alert about it every 30 min. Defaults to false when
    /// absent, so an empty set that was not opted into still alerts (the
    /// dangerous misconfiguration).
    #[serde(default)]
    pub allow_no_tiebreaker: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct StorageConfig {
    pub drbd_resource: String,
    pub backing_device: PathBuf,
    pub drbd_device: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IscsiConfig {
    pub target_iqn: String,
    #[serde(default = "default_portal_port")]
    pub portal_port: u16,
    pub lun_serial: String,
    #[serde(default)]
    pub chap_user: Option<String>,
    #[serde(default)]
    pub chap_password: Option<String>,
    #[serde(default)]
    pub initiator_iqns: Option<Vec<String>>,
}

fn default_portal_port() -> u16 {
    3260
}

#[derive(Debug, Clone, Deserialize)]
pub struct TimerConfig {
    #[serde(default = "default_poll_ms")]
    pub poll_interval_ms: u64,
    /// How long DRBD must stay StandAlone before the daemon attempts
    /// recovery. Guards against acting on transient disconnects.
    #[serde(default = "default_standalone_grace_secs")]
    pub standalone_grace_secs: u64,
}

impl Default for TimerConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: default_poll_ms(),
            standalone_grace_secs: default_standalone_grace_secs(),
        }
    }
}

fn default_poll_ms() -> u64 {
    1000
}
fn default_standalone_grace_secs() -> u64 {
    30
}

/// Replication failback behavior (arbitrated mode only; ignored in
/// fate-shared, which has a single path).
#[derive(Debug, Clone, Deserialize)]
pub struct FailbackConfig {
    /// Whether the daemon may run the guarded failback dance by itself once
    /// the direct link has been stable past the (escalating) hold. `false`
    /// still steers natural reconnects and still honors the explicit
    /// `twinstor failback-replication` command; it only disables the
    /// daemon-initiated dance.
    #[serde(default = "default_failback_auto")]
    pub auto: bool,
}

impl Default for FailbackConfig {
    fn default() -> Self {
        Self {
            auto: default_failback_auto(),
        }
    }
}

fn default_failback_auto() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct HaConfig {
    /// Whether the daemon may run the XHA re-admission dance by itself when it
    /// detects the staggered-restart join deadlock (both xhad instances
    /// cycling through exit 12/13 after a fence storm). On by default, as
    /// the product runs on unattended sites: `false` keeps the detection and
    /// alert (naming the manual quiesce-and-sync-start runbook) and only
    /// disables the dance. The dance never runs unless local storage is fully
    /// healthy and the peer is reachable, is capped at once per hour, and each
    /// of its two phases fires only while the peer's management address has
    /// answered for the settle period (phase 1 is cancelled otherwise). Before
    /// its local disarm each host checks only itself: pool HA on, no pending
    /// HA task (an unreadable task list holds the disarm), not held in
    /// XAPI's startup loop, its HA daemon
    /// out of the liveset. The dead-xhad revive is governed by
    /// `rejoin_assist`.
    #[serde(default = "default_readmission_assist")]
    pub readmission_assist: bool,
    /// Whether the daemon may run its rejoin repairs: the stranded
    /// former-master toolstack yield and its forced reset, the returning-slave
    /// reconnect, the HA-deadlock exit, the masterless active rejoin, the
    /// dual-master auto-heal yield, the local self-unwedge, the dead-xhad
    /// revive, and the second-master demote. Each still needs its own
    /// evidence and, at the moment it fires, the peer's management address
    /// answering (and, for repairs triggered by an absence of answer, answering
    /// for the settle period). `false` keeps every detection, log and alert
    /// and only disables these actions. Does not gate the lone-survivor
    /// transition or an owed HA re-arm. Default on.
    #[serde(default = "default_true")]
    pub rejoin_assist: bool,
    /// Whether the daemon may start a protected VM it finds halted while
    /// the pool is fully healthy (HA armed, hosts enabled, storage
    /// converged): the state XAPI leaves behind when its bounded HA
    /// restart attempts ran out mid-recovery. On by default because only
    /// halts first observed during an incident (pool unhealthy, heal episode
    /// open, or bracketed by an unhealthy scan) are restarted; a halt that
    /// appears between two healthy scans looks operator-initiated and only
    /// ever raises an alert. Only VMs XAPI itself still counts as protected
    /// are started (`ha-always-run` true as well as the restart priority).
    /// XAPI clears that flag when a VM is shut down through it, and on a halt
    /// the hypervisor reports while the pool's
    /// `ha-reboot-vm-on-internal-shutdown` is off (fate-shared freeze mode),
    /// so such a VM is never started, whenever the daemon first sees it (a
    /// dead host's VMs keep the flag). A guest the crash boundary marked
    /// `twinstor-disks-suspect` is never started either. The alert still
    /// covers every halted VM with the priority, saying per VM whether it is
    /// started and why not. Per VM: at most one start an hour, and a failed
    /// start is retried after a back-off (one minute, doubling up to 30
    /// minutes; one alert after 5 failures).
    #[serde(default = "default_true")]
    pub restart_halted_protected_vms: bool,
    /// Whether the daemon may run its two pool-HA-cycle repairs (a pool HA
    /// disable, then the re-enable through the persisted re-arm obligation),
    /// both for a host left locally disarmed under the armed pool. Default on.
    /// - Peer-emergency reset, from the pool master: the peer's xapi answers
    ///   HA_POOL_IS_ENABLED_BUT_HOST_IS_DISABLED on two probes at least 30 s
    ///   apart (its xapi restarted while disarmed, a state xapi does not leave
    ///   by itself), with the peer's management address answering; at most 3
    ///   resets in 6 hours, at least 10 minutes apart (kept for the boot
    ///   across daemon restarts).
    /// - Recorded-disarm re-arm: a host TwinStor disarmed itself (a
    ///   whole-switch hold, a stand-down) is back in the liveset still locally
    ///   disarmed; one cycle per disarm record, from the pool master when both
    ///   hosts hold one, with both hosts live in the liveset, the peer's
    ///   management address answering, a single master and no HA operation
    ///   pending.
    ///
    /// `false` keeps the detection and alerts (with the manual pool HA cycle)
    /// and disables both actions.
    #[serde(default = "default_true")]
    pub auto_resolve_peer_emergency: bool,
    /// Auto-resolve the isolated-master trap by a durable storage stand-down.
    /// When this pool master loses the client network while the peer is alive
    /// on the replication link (arbitrated only) and past the confirmed
    /// window, it makes its own storage unavailable (stops its target, logs
    /// out its iSCSI client sessions, then demotes) so its VMs cannot write
    /// via any path and XHA restarts them on the reachable peer. Persisted via
    /// a marker so a reboot cannot undo it while still isolated. Data-safe
    /// because it only ever removes a writer, and gated on the peer being
    /// UpToDate; falls back to detect-and-alert when it cannot confirm.
    /// It replaces an earlier reboot-based self-fence, which let the fenced
    /// node come back and re-run the VMs (a dual master); the key is new so a
    /// stale `self_fence_isolated_master` setting does not re-enable that
    /// behavior.
    #[serde(default = "default_true")]
    pub self_standdown_isolated_master: bool,
    /// Whether the daemon may stop a stuck or orphaned local xhad so the HA
    /// re-arm can succeed. Signature: the pool flag reads HA-off but the local
    /// xhad is still running and joined, so `pool-ha-enable` keeps failing with
    /// `Xha_error(15)`/`MTC_EXIT_DAEMON_IS_PRESENT` (its `ha_set_pool_state init`
    /// refuses while the daemon is present) and the re-arm obligation repeats
    /// the same failing enable. Stopping the local xhad lets the next enable
    /// initialise cleanly (the automated form of "toolstack-restart both, then
    /// pool-ha-enable"). Default on: control-plane only (gated on healthy
    /// storage), stops the daemon only when the pool intent is already off, and
    /// requires the mismatch to last past a normal enable/disable transition so
    /// it never interferes with a daemon that is re-arming.
    #[serde(default = "default_true")]
    pub reconcile_stuck_xhad: bool,
    /// No effect any more; accepted so existing configuration files still
    /// parse. It bridged flaps of the peer-emergency resolver's older
    /// signature (the peer's agent not answering), which is retired: the
    /// resolver now acts only on the peer's xapi answering that it is locally
    /// disarmed under the armed pool.
    #[serde(default = "default_true")]
    #[allow(dead_code)]
    pub readmission_flap_debounce: bool,
    /// Whether the pool master starts, once after its boot, the VMs marked
    /// `other-config:auto_poweron=true` that XCP-ng's own autostart left
    /// halted. XCP-ng tries them once per host boot, before the pool is
    /// whole, and HA refuses those starts while it cannot keep its failover
    /// plan, so after a reboot of both hosts they stay halted. Only VMs that
    /// stayed halted since the boot are started, once both hosts are enabled,
    /// live and in the HA liveset, both copies are UpToDate and the SR is
    /// plugged on both; never a VM HA still protects, one that ran since the
    /// boot, or one whose disks are marked suspect. Needs the pool's
    /// `auto_poweron` too, as XCP-ng does. Default on.
    #[serde(default = "default_true")]
    pub complete_autostart: bool,
}

impl Default for HaConfig {
    fn default() -> Self {
        Self {
            readmission_assist: default_readmission_assist(),
            rejoin_assist: true,
            restart_halted_protected_vms: true,
            auto_resolve_peer_emergency: true,
            self_standdown_isolated_master: true,
            reconcile_stuck_xhad: true,
            readmission_flap_debounce: true,
            complete_autostart: true,
        }
    }
}

/// Host-side reboot safety backstop (`twinstor reboot-drain`, run from a systemd
/// `ExecStop` early in the shutdown transaction). It holds a reboot only when
/// this host is the last UpToDate replica and the peer is still resyncing, so a
/// rolling reboot, `yum` reboot or kernel-update reboot cannot take down the only
/// current copy mid-catch-up. Transparent otherwise: the safe side (the returning
/// SyncTarget, or any state where the peer already holds a current copy) proceeds
/// at once. Complements the orchestrator-side `twinstor-synced` advertisement that
/// paces XO RPU; this covers every clean reboot XO never sees.
#[derive(Debug, Clone, Deserialize)]
pub struct RebootDrainConfig {
    /// Master switch for the shutdown-time hold. When off, the drain command always
    /// proceeds immediately (the systemd unit becomes a no-op).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Upper bound on the hold, in seconds. A rolling-reboot delta resync (only
    /// the blocks that changed while the host was down) usually finishes well
    /// inside this; a full rebuild (after a disk swap) will not, so the cap turns
    /// that case into a bounded, alerted proceed rather than a wedged host.
    /// Default 600s.
    #[serde(default = "default_reboot_drain_max_secs")]
    pub max_secs: u64,
}

fn default_reboot_drain_max_secs() -> u64 {
    600
}

impl Default for RebootDrainConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_secs: default_reboot_drain_max_secs(),
        }
    }
}

fn default_readmission_assist() -> bool {
    true
}
fn default_true() -> bool {
    true
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Self =
            toml::from_str(&content).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.node.name == self.node.peer {
            bail!("node.name and node.peer must differ");
        }
        // CHAP is all-or-nothing: the iSCSI start path only enforces auth when
        // user, password, and initiator_iqns are all present (see iscsi.rs).
        // A config with only some of them would silently start an
        // unauthenticated target open to every initiator, so reject it.
        let chap_fields = [
            self.iscsi.chap_user.is_some(),
            self.iscsi.chap_password.is_some(),
            self.iscsi.initiator_iqns.is_some(),
        ];
        if chap_fields.iter().any(|&set| set) && !chap_fields.iter().all(|&set| set) {
            bail!(
                "iscsi CHAP config is all-or-nothing: set chap_user, chap_password, \
                 and initiator_iqns together, or none of them"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL_CONFIG: &str = r#"
[node]
name = "node-a"
peer = "node-b"

[network]
bond = "bond0"
peer_address = "10.0.0.2"
drbd_port = 7789

[storage]
drbd_resource = "data"
backing_device = "/dev/disk/by-id/ata-EXAMPLE_SSD_1TB_SN0000-part4"
drbd_device = "/dev/drbd0"

[iscsi]
target_iqn = "iqn.2026-01.com.xcp-ng:twinstor-data"
lun_serial = "twinstor0001"
"#;

    fn parse(toml: &str) -> Result<Config> {
        let config: Config = toml::from_str(toml)?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn parse_minimal_config() {
        let config = parse(MINIMAL_CONFIG).unwrap();
        assert_eq!(config.node.name, "node-a");
        assert_eq!(config.node.peer, "node-b");
        assert_eq!(config.iscsi.portal_port, 3260);
        assert_eq!(config.timers.poll_interval_ms, 1000);
    }

    #[test]
    fn reject_same_node_and_peer() {
        let toml = MINIMAL_CONFIG.replace("peer = \"node-b\"", "peer = \"node-a\"");
        assert!(parse(&toml).is_err());
    }

    #[test]
    fn custom_poll_interval() {
        let toml = format!("{MINIMAL_CONFIG}\n[timers]\npoll_interval_ms = 500\n");
        let config = parse(&toml).unwrap();
        assert_eq!(config.timers.poll_interval_ms, 500);
    }

    #[test]
    fn reject_missing_required_section() {
        // Drop the whole [storage] section.
        let toml = MINIMAL_CONFIG
            .split("[storage]")
            .next()
            .unwrap()
            .to_string()
            + "[iscsi]\ntarget_iqn = \"iqn.x\"\nlun_serial = \"s\"\n";
        assert!(parse(&toml).is_err());
    }

    #[test]
    fn reject_missing_required_field() {
        // Remove lun_serial, a required field with no default.
        let toml = MINIMAL_CONFIG.replace("lun_serial = \"twinstor0001\"\n", "");
        assert!(parse(&toml).is_err());
    }

    #[test]
    fn full_chap_config_is_accepted() {
        let toml = format!(
            "{MINIMAL_CONFIG}\nchap_user = \"u\"\nchap_password = \"p\"\n\
             initiator_iqns = [\"iqn.2026-01.com.xcp-ng:init\"]\n"
        );
        let config = parse(&toml).unwrap();
        assert!(config.iscsi.chap_user.is_some());
    }

    #[test]
    fn reject_partial_chap_config() {
        // chap_user without chap_password / initiator_iqns would silently
        // start an unauthenticated, wide-open target.
        let toml = format!("{MINIMAL_CONFIG}\nchap_user = \"u\"\n");
        assert!(parse(&toml).is_err());
    }

    /// A key this version does not know (one a test build had, since
    /// removed) is ignored, never a parse error.
    #[test]
    fn an_unknown_ha_key_is_ignored() {
        let toml = format!("{MINIMAL_CONFIG}\n[ha]\ndisarmed_host_fast_revive = true\n");
        assert!(parse(&toml).is_ok());
    }

    /// `rejoin_assist` and `readmission_assist` (the dance) both default on;
    /// an explicit value in the file is kept either way.
    #[test]
    fn ha_assist_keys_default_and_explicit_values_are_kept() {
        let config = parse(MINIMAL_CONFIG).unwrap();
        assert!(config.ha.rejoin_assist);
        assert!(config.ha.readmission_assist);
        let toml = format!("{MINIMAL_CONFIG}\n[ha]\nrejoin_assist = false\n");
        let config = parse(&toml).unwrap();
        assert!(!config.ha.rejoin_assist);
        assert!(config.ha.readmission_assist);
        let toml = format!("{MINIMAL_CONFIG}\n[ha]\nreadmission_assist = false\n");
        assert!(!parse(&toml).unwrap().ha.readmission_assist, "an explicit opt-out is kept");
        let toml = format!("{MINIMAL_CONFIG}\n[ha]\nreadmission_assist = true\n");
        assert!(parse(&toml).unwrap().ha.readmission_assist);
    }

    #[test]
    fn no_chap_config_is_accepted() {
        // The minimal config sets none of the CHAP fields.
        let config = parse(MINIMAL_CONFIG).unwrap();
        assert!(config.iscsi.chap_user.is_none());
    }
}
