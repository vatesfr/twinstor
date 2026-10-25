use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Primary,
    Secondary,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Connected,
    Connecting,
    StandAlone,
    Disconnecting,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskState {
    UpToDate,
    Consistent,
    Inconsistent,
    Outdated,
    Diskless,
    Unknown,
}

/// Replication direction toward the peer while a resync is running. `drbdadm
/// status` prints this only during a resync (the `replication:` field); when
/// the link is idle or Established the line is omitted, so callers see `None`.
/// `WFBitMap` is the bitmap-exchange step (WFBitMapS/WFBitMapT). It normally
/// lasts well under a minute, but a promote race can leave one side stuck there
/// indefinitely while the peer reports Established, so replication silently
/// runs one way. It is parsed explicitly so the stuck-WFBitMap auto-clear can
/// see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replication {
    Established,
    SyncSource,
    SyncTarget,
    WFBitMap,
    Other,
}

/// The `fencing` value of a `drbdsetup show` dump, if present.
pub fn parse_fencing(show: &str) -> Option<String> {
    show.lines().find_map(|l| {
        let t = l.trim();
        let rest = t.strip_prefix("fencing")?;
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        Some(rest.trim().trim_end_matches(';').trim().to_string())
    })
}

/// DRBD's default `connect-int` (seconds), which `drbdsetup show` omits.
pub const DRBD_DEFAULT_CONNECT_INT: u32 = 10;

/// The `connect-int` value from a `drbdsetup show` dump, when set.
pub fn parse_connect_int(show: &str) -> Option<u32> {
    show.lines().find_map(|l| {
        let rest = l.trim().strip_prefix("connect-int")?;
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        rest.trim().split(';').next()?.trim().parse().ok()
    })
}

#[derive(Debug, Clone)]
pub struct DrbdStatus {
    pub role: Role,
    pub peer_role: Option<Role>,
    pub connection: ConnectionState,
    pub disk: DiskState,
    pub peer_disk: Option<DiskState>,
    /// I/O suspended by an explicit user or script request (`suspended:user`),
    /// as opposed to DRBD's own fencing and quorum suspends, which resume on
    /// reconnect. The fence-peer handler's `drbdadm suspend-io` lands here and
    /// nothing resumes it automatically, so the daemon lifts it once the
    /// cluster is healthy again.
    pub suspended_user: bool,
    /// I/O suspended by DRBD's own fencing (`suspended:fencing`,
    /// resource-and-stonith): frozen at a connection loss until the fence
    /// handler delivers a verdict or the connection returns. A handler that
    /// exits indeterminate (the administrative-bounce stand-down) leaves this
    /// set until reconnect; if the reconnect never comes the freeze is
    /// unbounded and invisible to heals keyed on `suspended_user`, so the
    /// daemon must at least see and alert on it.
    pub suspended_fencing: bool,
    /// I/O suspended for any reason (`suspended:` with any value but `no`):
    /// user, fencing, and also no-data (`on-no-data-accessible suspend-io`,
    /// no reachable copy) and quorum. For callers that need "is local I/O
    /// running at all", not one specific suspend.
    pub suspended_any: bool,
    /// Replication direction toward the peer, or `None` when no resync is in
    /// progress (idle or Established).
    pub replication: Option<Replication>,
    /// Resync completion percentage (`done:NN.NN`), present only while a resync
    /// is in progress. Used to tell a stuck mutual-SyncSource limbo (no
    /// progress) from a legitimately advancing resync.
    pub resync_done: Option<f64>,
}

/// One replication path of the connection, as reported by
/// `drbdsetup events2 --now`: the addresses and whether the TCP transport
/// currently has this path established. In active-backup one path is
/// established while the connection is up. The raw `ipv4:IP:port` forms and
/// the peer-node-id are kept verbatim because `drbdsetup del-path/new-path`
/// take them as arguments (reconnect steering).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathState {
    /// Peer IP of this path, bare (no `ipv4:` prefix, no port).
    pub peer: String,
    pub established: bool,
    /// `peer-node-id:` field of the path line.
    pub peer_node_id: Option<u32>,
    /// Raw local address as printed, e.g. `ipv4:198.51.100.1:7789`.
    pub local_addr: String,
    /// Raw peer address as printed, e.g. `ipv4:198.51.100.2:7789`.
    pub peer_addr: String,
}

/// A DRBD generation fingerprint for one node: the four generation UUIDs
/// (Current, Bitmap-base, and two History) plus the out-of-sync block count,
/// as DRBD prints them on one side of a `drbd_sync_handshake` kernel block
/// (`self`/`peer` lines) or in `drbdadm get-gi`. The UUIDs are 64-bit.
///
/// During a split-brain the generation lineage is the only data-based answer
/// to "who holds the superset of committed writes". DRBD disk-state labels
/// (UpToDate/Consistent) say whether a node stayed a healthy Primary, not how
/// current its data is: a stale survivor stays UpToDate while a fresher node
/// that crashed while serving comes back Consistent. Only the UUID history
/// shows who advanced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GenId {
    /// Current data generation UUID.
    pub current: u64,
    /// Bitmap's base data generation UUID (the generation the peer was last
    /// known to be at; blocks written since are tracked out-of-sync).
    pub bitmap: u64,
    /// Younger and older history UUIDs.
    pub history: [u64; 2],
    /// Out-of-sync blocks toward the peer (`bits:N`): unreplicated writes,
    /// committed data the peer does not have. Higher means more at stake.
    pub bits: u64,
    /// DRBD's UUID flags (`flags:X`, printed in hex). 0 when absent or
    /// unparseable, which means no flag is assumed.
    pub flags: u64,
}

/// DRBD's `UUID_FLAG_CRASHED_PRIMARY` (drbd_protocol.h): set at attach on a
/// node whose metadata records it was Primary when it went down, and cleared
/// only once both disks are UpToDate together again; a later promotion does
/// not clear it.
pub const UUID_FLAG_CRASHED_PRIMARY: u64 = 1 << 1;

/// DRBD's `UUID_PRIMARY` (drbd.h): bit 0 of a generation UUID records that
/// the node was Primary when the UUID was set, not a different generation.
/// DRBD masks it out of every comparison (`uuid_compare`), so `...74` and
/// `...75` are the same generation and must compare equal here too.
const UUID_PRIMARY: u64 = 1;

/// Whether two generation UUIDs name the same generation, the way DRBD compares
/// them (bit 0 ignored). A zero UUID (unset slot) never matches.
pub fn same_generation(a: u64, b: u64) -> bool {
    let (a, b) = (a & !UUID_PRIMARY, b & !UUID_PRIMARY);
    a != 0 && a == b
}

impl GenId {
    /// Whether `other_current` is a generation this node has already
    /// superseded, that is, it appears as this node's bitmap-base or in its
    /// history. If so this node descends from `other_current`: it holds every
    /// write the node at `other_current` had, and then advanced past it. A
    /// zero UUID never matches (unset slot).
    pub fn descends_from(&self, other_current: u64) -> bool {
        same_generation(other_current, self.bitmap)
            || self
                .history
                .iter()
                .any(|&h| same_generation(other_current, h))
    }

    /// Whether this node came back from a crash while Primary and has not
    /// resynced with the peer since. On that attach DRBD marks every
    /// activity-log extent out of sync, so `bits` overstates its real writes.
    pub fn crashed_primary(&self) -> bool {
        self.flags & UUID_FLAG_CRASHED_PRIMARY != 0
    }
}

/// The `self`+`peer` generation snapshot from the most recent DRBD connection
/// handshake (`drbd_sync_handshake` in the kernel log). This is DRBD's own
/// divergence snapshot, taken when it compared the two nodes, and it carries
/// the peer's UUIDs, which a StandAlone node cannot read any other way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandshakeGen {
    pub local: GenId,
    pub peer: GenId,
}

/// One read of the kernel ring buffer for the split-brain resolver: the latest
/// complete handshake pair (if any) plus how many handshake headers the buffer
/// held for this resource. The count lets the resolver's telemetry tell "no
/// handshake at all" (the peer has not answered DRBD since the buffer last
/// wrapped) from "a handshake happened but its block was incomplete or
/// malformed", two failure classes that are measured separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LineageRead {
    pub pair: Option<HandshakeGen>,
    pub blocks: usize,
}

/// Coarse class of a lineage read, as logged by the resolver's evidence event.
pub fn lineage_class(read: &Result<LineageRead>) -> &'static str {
    match read {
        Err(_) => "collect_failed",
        Ok(r) if r.pair.is_some() => "present",
        Ok(r) if r.blocks > 0 => "incomplete",
        Ok(_) => "absent",
    }
}

#[async_trait::async_trait]
pub trait DrbdControl: Send + Sync {
    async fn status(&self) -> Result<DrbdStatus>;
    /// The local+peer generation UUIDs from the most recent DRBD connection
    /// handshake, read from the kernel ring buffer. `None` when no recent
    /// handshake is visible (peer never reached, or the buffer wrapped); the
    /// split-brain resolver then falls back to its role/keeper election.
    /// Default `None` so test doubles need not implement it.
    async fn handshake_gen(&self) -> Result<Option<HandshakeGen>> {
        Ok(None)
    }
    /// `handshake_gen` plus the handshake block count from the same buffer
    /// read (see `LineageRead`). Default derives from `handshake_gen` so test
    /// doubles keep working; the real controller reads dmesg once for both.
    async fn handshake_read(&self) -> Result<LineageRead> {
        let pair = self.handshake_gen().await?;
        Ok(LineageRead {
            blocks: usize::from(pair.is_some()),
            pair,
        })
    }
    /// This node's current DRBD generation UUID (`drbdadm show-gi`), bit 0
    /// masked. `None` when unreadable. Default `None` so test doubles need not
    /// implement it.
    async fn local_current_uuid(&self) -> Option<u64> {
        None
    }
    /// Per-path state of the connection (arbitrated mode has two paths;
    /// fate-shared one). Default is "unknown" (empty) so test doubles and
    /// transports without path visibility degrade to a no-op in callers.
    async fn paths(&self) -> Result<Vec<PathState>> {
        Ok(Vec::new())
    }
    /// Reconnect-time steering: while the connection is down, remove every
    /// configured path whose peer is not in `keep_peer_ips`, so the next
    /// connect can only use the kept (direct) paths. Returns how many paths
    /// were removed; 0 means nothing to steer (no fallback paths configured,
    /// addresses unknown, or a path still established). The kernel refuses
    /// del-path on an established transport, so callers must only steer from
    /// a disconnected state. Restore the removed standby paths afterwards with
    /// [`DrbdControl::restore_paths`].
    async fn steer_paths_to(&self, _keep_peer_ips: &[String]) -> Result<usize> {
        Ok(0)
    }
    /// Re-add any config-declared paths missing from the kernel (undo of
    /// [`DrbdControl::steer_paths_to`], via `drbdadm adjust`).
    async fn restore_paths(&self) -> Result<()> {
        Ok(())
    }
    /// Whether the kernel has no peer object for this resource while the
    /// resource file declares one (a dry-run `drbdadm adjust` would only
    /// re-create the connection). Default `false`.
    async fn peer_object_missing(&self) -> Result<bool> {
        Ok(false)
    }
    /// Re-create the peer object from the resource file without connecting
    /// (`new-peer`, `new-path`, `peer-device-options` from a dry-run adjust).
    /// The caller re-applies its live net options, then connects. Default
    /// no-op.
    async fn recreate_peer_object(&self) -> Result<()> {
        Ok(())
    }
    /// Whether this node's connection still carries a `--discard-my-data`
    /// offer. DRBD 9 only ever sets that flag on connect: it survives a
    /// disconnect and a `--discard-my-data=no` reconnect, and is cleared only
    /// by a handshake that completes, a successful promote, or deleting the
    /// peer object. Read from debugfs (`CONN_DISCARD_MY_DATA`); `None` when
    /// unreadable. Default `Some(false)` for test doubles.
    async fn discard_offer_pending(&self) -> Option<bool> {
        Some(false)
    }
    /// Withdraw a pending discard offer: delete the peer connection object
    /// and re-create it without connecting (see `recreate_peer_object`); the
    /// caller re-applies its live net options and connects plainly. Refused
    /// on a Primary, a connected or fenced node, or when the resource file
    /// differs from the kernel in more than connection and option lines.
    /// Default no-op.
    async fn withdraw_discard_offer(&self) -> Result<()> {
        Ok(())
    }
    async fn up(&self) -> Result<()>;
    async fn primary(&self) -> Result<()>;
    /// `drbdadm connect`: non-destructive reconnect. Never discards.
    async fn connect(&self) -> Result<()>;
    /// `drbdadm disconnect`: a plain disconnect that the kernel refuses on a
    /// Primary whose peer cannot be outdated (the resource-and-stonith
    /// refusal), which makes it safe to attempt from a demoted Secondary in
    /// the replication-failback dance. The refusal is the safety net; never
    /// bypass it here. `disconnect_force` does, and it creates a split-brain
    /// under dual-Primary write load.
    async fn disconnect(&self) -> Result<()>;
    /// `drbdadm disconnect --force`: drop the connection even when the
    /// resource refuses a plain disconnect ("Need access to UpToDate data").
    /// Paired with `connect` to clear the mutual-SyncSource limbo; never
    /// discards data (both sides are UpToDate).
    async fn disconnect_force(&self) -> Result<()>;
    /// `drbdadm net-options --fencing=<policy>`: set the fencing policy live.
    /// It applies on a Connected Primary with no state change. `dont-care` is
    /// the DRBD default, so `drbdsetup show` omits the line when it is set.
    /// Used only to bracket the stuck-WFBitMap bounce: an admin disconnect
    /// under an armed fencing policy makes the peer fence and outdate this
    /// node and can create a split-brain with no data written; `dont-care` for
    /// the seconds of the bounce removes that path. Callers must restore the
    /// configured policy afterwards: leaving `dont-care` in place removes the
    /// split-brain protection.
    async fn set_fencing(&self, policy: &str) -> Result<()>;
    /// The fencing policy the kernel runs now (`drbdsetup show`), `None`
    /// when unreadable. Re-creating the peer object re-applies the `.res`
    /// file's policy, so a policy set live does not stick by itself.
    async fn live_fencing(&self) -> Option<String>;
    /// DRBD's reconnect interval (`connect-int`, seconds) set live.
    async fn set_connect_int(&self, secs: u32) -> Result<()> {
        let _ = secs;
        Ok(())
    }
    /// The `connect-int` the kernel runs now (`drbdsetup show` leaves the
    /// default, 10, out), `None` when unreadable.
    async fn live_connect_int(&self) -> Option<u32> {
        None
    }
    /// `drbdadm new-current-uuid`: start a new data generation on this
    /// copy. Freeze mode: a frozen Primary that wrote nothing during a split
    /// is merely behind its peer, which DRBD will not resolve in its favour
    /// (a Primary never becomes a sync target); a new generation makes the
    /// next handshake a split-brain, which the peer's discard offer decides.
    async fn new_current_uuid(&self) -> Result<()>;
    /// `drbdadm resume-io`: lift an explicit I/O suspend. The fence-peer
    /// handler never runs it; the daemon does once the cluster heals.
    async fn resume_io(&self) -> Result<()>;
    /// `drbdadm suspend-io`: freeze this copy's I/O again. Only the crash
    /// boundary uses it: it re-freezes a yielded copy whose suspend it lifted
    /// when it then holds instead of discarding.
    async fn suspend_io(&self) -> Result<()>;
    /// `drbdadm connect --discard-my-data`: destructive, throws away this
    /// node's diverged writes and resyncs from the peer. Only the split-brain
    /// loser runs this.
    async fn connect_discard(&self) -> Result<()>;
    /// `drbdadm secondary`: demote (required before `--discard-my-data`, which
    /// needs the device closed).
    async fn secondary(&self) -> Result<()>;
    /// `drbdadm primary --force`: promote even if not UpToDate.
    async fn force_primary(&self) -> Result<()>;
    /// `drbdadm attach`: re-attach the backing disk (fails harmlessly while
    /// the device is absent; used to recover from a transient disk loss).
    async fn attach(&self) -> Result<()>;
}

pub struct DrbdController {
    resource: String,
}

impl DrbdController {
    pub fn new(config: &Config) -> Self {
        Self {
            resource: format!("twinstor-{}", config.storage.drbd_resource),
        }
    }
}

#[async_trait::async_trait]
impl DrbdControl for DrbdController {
    async fn local_current_uuid(&self) -> Option<u64> {
        let text = crate::cmd::cmd_output_for("drbdadm", &["show-gi", &self.resource], 15)
            .await
            .ok()?;
        parse_gi_current(&text)
    }

    async fn status(&self) -> Result<DrbdStatus> {
        // `drbdadm status` is the first await of nearly every tick and has
        // many callers, so it must fail fast: drbdsetup can hang indefinitely
        // on a wedged DRBD netlink, and an unbounded call would freeze the
        // loop. A healthy status returns in well under a second; 15 s means
        // netlink is wedged.
        let text = crate::cmd::cmd_output_for("drbdadm", &["status", &self.resource], 15)
            .await
            .context("running drbdadm status")?;
        parse_drbdadm_text(&text)
    }

    async fn up(&self) -> Result<()> {
        info!(resource = %self.resource, "bringing DRBD resource up");
        run_drbdadm(&["up", &self.resource]).await
    }

    async fn primary(&self) -> Result<()> {
        info!(resource = %self.resource, "promoting to primary");
        run_drbdadm(&["primary", &self.resource]).await
    }

    async fn connect(&self) -> Result<()> {
        info!(resource = %self.resource, "drbd connect (non-destructive)");
        // DRBD 9 keeps `--discard-my-data` as a sticky flag on the kernel peer
        // object, and the `=no` spelling below does not clear it: the kernel's
        // connect only ever sets the flag (DRBD 9.2.16 source). A node that
        // offered a discard earlier re-offers it on this connect unless the
        // offer was withdrawn first; see `discard_offer_pending` and
        // `withdraw_discard_offer`, which every plain connect after a discard
        // round goes through (`steered_connect`). The `=no` stays so the
        // intent reads plainly in the logs; drbd-utils that reject the
        // spelling fall back to the bare form.
        if run_drbdadm(&["connect", "--discard-my-data=no", &self.resource])
            .await
            .is_ok()
        {
            return Ok(());
        }
        run_drbdadm(&["connect", &self.resource]).await
    }

    async fn disconnect(&self) -> Result<()> {
        info!(resource = %self.resource, "drbd disconnect (plain, kernel-guarded)");
        run_drbdadm(&["disconnect", &self.resource]).await
    }

    async fn disconnect_force(&self) -> Result<()> {
        warn!(resource = %self.resource, "drbd disconnect --force (dropping the connection; a disconnect discards nothing)");
        run_drbdadm(&["disconnect", "--force", &self.resource]).await
    }

    async fn resume_io(&self) -> Result<()> {
        info!(resource = %self.resource, "drbd resume-io (lifting leftover I/O suspend)");
        run_drbdadm(&["resume-io", &self.resource]).await
    }

    async fn suspend_io(&self) -> Result<()> {
        warn!(resource = %self.resource, "drbd suspend-io (freezing this copy's I/O again)");
        run_drbdadm(&["suspend-io", &self.resource]).await
    }

    async fn connect_discard(&self) -> Result<()> {
        warn!(resource = %self.resource, "drbd connect --discard-my-data (discarding local diverged writes)");
        run_drbdadm(&["connect", "--discard-my-data", &self.resource]).await
    }

    async fn secondary(&self) -> Result<()> {
        info!(resource = %self.resource, "demoting to secondary");
        run_drbdadm(&["secondary", &self.resource]).await
    }

    async fn force_primary(&self) -> Result<()> {
        info!(resource = %self.resource, "force-promoting to primary");
        run_drbdadm(&["primary", "--force", &self.resource]).await
    }

    async fn set_fencing(&self, policy: &str) -> Result<()> {
        info!(resource = %self.resource, policy = %policy, "setting DRBD fencing policy live");
        let arg = format!("--fencing={policy}");
        run_drbdadm(&["net-options", &arg, &self.resource]).await
    }

    async fn live_fencing(&self) -> Option<String> {
        let out = crate::cmd::cmd_output_for("drbdsetup", &["show", &self.resource], 10)
            .await
            .ok()?;
        // `drbdsetup show` leaves the default out: a readable dump with a net
        // section and no fencing line runs `dont-care`.
        parse_fencing(&out).or_else(|| out.contains("net {").then(|| "dont-care".to_string()))
    }

    async fn set_connect_int(&self, secs: u32) -> Result<()> {
        // drbdsetup on the peer connection, not `drbdadm net-options`: drbdadm
        // re-sends every net option of the .res file with it, the fencing
        // policy included, and would put `resource-only` back under freeze
        // mode until the next pass re-armed it. drbdsetup changes only this
        // one.
        let node_id = peer_node_id(&self.resource)
            .await
            .context("peer node-id unreadable (drbdsetup events2)")?;
        info!(resource = %self.resource, secs, "setting DRBD connect-int live");
        let arg = format!("--connect-int={secs}");
        crate::cmd::run_cmd_for(
            "drbdsetup",
            &["net-options", &self.resource, &node_id.to_string(), &arg],
            30,
        )
        .await
    }

    async fn live_connect_int(&self) -> Option<u32> {
        let out = crate::cmd::cmd_output_for("drbdsetup", &["show", &self.resource], 10)
            .await
            .ok()?;
        parse_connect_int(&out)
            .or_else(|| out.contains("net {").then_some(DRBD_DEFAULT_CONNECT_INT))
    }

    async fn new_current_uuid(&self) -> Result<()> {
        warn!(resource = %self.resource, "drbd new-current-uuid (this copy starts a new data generation)");
        run_drbdadm(&["new-current-uuid", &self.resource]).await
    }

    async fn attach(&self) -> Result<()> {
        debug!(resource = %self.resource, "attempting to attach backing disk");
        run_drbdadm(&["attach", &self.resource]).await
    }

    async fn paths(&self) -> Result<Vec<PathState>> {
        // Bounded: drbdsetup can block on wedged DRBD netlink. `events2 --now`
        // is a one-shot snapshot (returns immediately when healthy).
        let text =
            crate::cmd::cmd_output_for("drbdsetup", &["events2", "--now", &self.resource], 15)
                .await
                .context("running drbdsetup events2 --now")?;
        Ok(parse_events2_paths(&text))
    }

    async fn steer_paths_to(&self, keep_peer_ips: &[String]) -> Result<usize> {
        let paths = self.paths().await?;
        // Refuse to steer a live transport: del-path is rejected on an
        // established path anyway, and a half-steered state helps nobody.
        if paths.iter().any(|p| p.established) {
            return Ok(0);
        }
        let keep: Vec<&PathState> = paths
            .iter()
            .filter(|p| keep_peer_ips.contains(&p.peer))
            .collect();
        if keep.is_empty() {
            // Deleting every path would strand the connection entirely.
            return Ok(0);
        }
        let mut removed = 0;
        for p in paths.iter().filter(|p| !keep_peer_ips.contains(&p.peer)) {
            let Some(node_id) = p.peer_node_id else {
                continue;
            };
            if p.local_addr.is_empty() || p.peer_addr.is_empty() {
                continue;
            }
            info!(resource = %self.resource, path = %p.peer,
                  "steering: removing fallback path so the connect lands on the direct link");
            // Bounded (see `paths`): a wedged drbdsetup must not hang the tick.
            match crate::cmd::run_cmd_for(
                "drbdsetup",
                &[
                    "del-path",
                    &self.resource,
                    &node_id.to_string(),
                    &p.local_addr,
                    &p.peer_addr,
                ],
                15,
            )
            .await
            {
                Ok(()) => removed += 1,
                Err(e) => warn!(resource = %self.resource, path = %p.peer, error = %e,
                      "steering: del-path failed; connect may land on the fallback path"),
            }
        }
        Ok(removed)
    }

    async fn restore_paths(&self) -> Result<()> {
        debug!(resource = %self.resource, "restoring standby paths (drbdadm adjust)");
        run_drbdadm(&["adjust", &self.resource]).await
    }

    async fn peer_object_missing(&self) -> Result<bool> {
        let dry = crate::cmd::cmd_output_for("drbdadm", &["-d", "adjust", &self.resource], 30)
            .await
            .context("running drbdadm -d adjust")?;
        Ok(peer_recreate_commands(&dry_run_commands(&dry)).is_some())
    }

    async fn recreate_peer_object(&self) -> Result<()> {
        let dry = crate::cmd::cmd_output_for("drbdadm", &["-d", "adjust", &self.resource], 30)
            .await
            .context("running drbdadm -d adjust")?;
        let cmds = dry_run_commands(&dry);
        let Some(create) = peer_recreate_commands(&cmds) else {
            anyhow::bail!(
                "a dry-run adjust does not propose re-creating the peer object: {cmds:?}"
            );
        };
        for line in create {
            let words = shell_words(line);
            let args: Vec<&str> = words.iter().skip(1).map(String::as_str).collect();
            crate::cmd::run_cmd_for("drbdsetup", &args, 15)
                .await
                .with_context(|| format!("drbdsetup {}", args.join(" ")))?;
        }
        Ok(())
    }

    async fn discard_offer_pending(&self) -> Option<bool> {
        let mounted = std::fs::read_to_string("/proc/mounts").is_ok_and(|m| {
            m.lines()
                .any(|l| l.split_whitespace().nth(2) == Some("debugfs"))
        });
        if !mounted {
            // dom0 normally has debugfs mounted; mount it once if not (bounded).
            let _ = crate::cmd::run_cmd_for(
                "mount",
                &["-t", "debugfs", "none", "/sys/kernel/debug"],
                5,
            )
            .await;
        }
        let dir = format!(
            "/sys/kernel/debug/drbd/resources/{}/connections",
            self.resource
        );
        let mut seen = false;
        let mut pending = false;
        for e in std::fs::read_dir(&dir).ok()?.flatten() {
            let text = std::fs::read_to_string(e.path().join("debug")).ok()?;
            pending |= debugfs_discard_flag(&text)?;
            seen = true;
        }
        // No connection object at all is not "no offer": it is a node whose
        // peer is missing (see `peer_object_missing`).
        seen.then_some(pending)
    }

    async fn withdraw_discard_offer(&self) -> Result<()> {
        // Only on a node that is not serving and whose connection is down:
        // a promote already clears the flag (a Primary never carries it),
        // and deleting the connection of a fenced or connected node would
        // lift its fencing freeze or disconnect it administratively.
        let st = self.status().await.context("drbdadm status")?;
        if st.role != Role::Secondary
            || st.suspended_fencing
            || !matches!(
                st.connection,
                ConnectionState::StandAlone | ConnectionState::Connecting
            )
        {
            anyhow::bail!(
                "not withdrawing: role {:?}, connection {:?}, suspended:fencing {}",
                st.role,
                st.connection,
                st.suspended_fencing
            );
        }
        // The re-add goes through `adjust`, which would also apply any drift
        // between the resource file and the kernel. Only the connection
        // itself may change here: refuse otherwise (the regenerate-config
        // hazard of applying a changed resource file to a live resource).
        let dry = crate::cmd::cmd_output_for("drbdadm", &["-d", "adjust", &self.resource], 30)
            .await
            .context("running drbdadm -d adjust")?;
        // Option lines are allowed: the daemon re-applies its live options
        // right after the re-creation, before it connects (a resource file
        // older than the live safety policies always shows them).
        if let Some(other) = dry_run_commands(&dry).into_iter().find(|l| {
            !["connect", "new-path", "net-options", "resource-options"]
                .iter()
                .any(|c| l.starts_with(&format!("drbdsetup {c} ")))
        }) {
            anyhow::bail!(
                "drbdadm adjust would change more than the connection ({other}); \
                 not re-creating the peer object"
            );
        }
        let node_id = peer_node_id(&self.resource)
            .await
            .context("peer node-id unreadable (drbdsetup events2)")?;
        warn!(resource = %self.resource, peer_node_id = node_id,
              "withdrawing this node's pending discard offer: re-creating the peer \
               connection object (del-peer, then new-peer/new-path without connecting)");
        crate::cmd::run_cmd_for(
            "drbdsetup",
            &["del-peer", &self.resource, &node_id.to_string()],
            30,
        )
        .await
        .context("drbdsetup del-peer")?;
        // The peer object is gone now: re-create it, retrying, because a node
        // left without one is unreplicated (the periodic check repairs it
        // too, see `peer_object_missing`).
        let mut last = Ok(());
        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            last = self.recreate_peer_object().await;
            if last.is_ok() {
                break;
            }
        }
        last.context("re-creating the peer object after del-peer")?;
        match self.discard_offer_pending().await {
            Some(true) => anyhow::bail!("discard offer still pending after re-creating the peer"),
            _ => Ok(()),
        }
    }

    async fn handshake_gen(&self) -> Result<Option<HandshakeGen>> {
        // The peer's generation UUIDs are only observable from DRBD's own
        // connection-handshake log, and the kernel ring buffer is the source.
        // Bounded so a slow or oversized dmesg cannot stall the tick.
        let text = crate::cmd::cmd_output_for("dmesg", &[], 10)
            .await
            .context("running dmesg for the DRBD handshake generations")?;
        Ok(parse_handshake_gen(&text, &self.resource))
    }
    async fn handshake_read(&self) -> Result<LineageRead> {
        let text = crate::cmd::cmd_output_for("dmesg", &[], 10)
            .await
            .context("running dmesg for the DRBD handshake generations")?;
        Ok(LineageRead {
            pair: parse_handshake_gen(&text, &self.resource),
            blocks: handshake_block_count(&text, &self.resource),
        })
    }
}

/// Parse the `path` lines of `drbdsetup events2 --now` output:
///
/// ```text
/// exists path name:twinstor-data peer-node-id:1 conn-name:twinstor-2 \
///     local:ipv4:198.51.100.1:7789 peer:ipv4:198.51.100.2:7789 established:yes
/// ```
///
/// Tolerant of unknown fields; a path line without a parseable `peer:` or
/// `established:` field is skipped rather than failing the whole read.
pub fn parse_events2_paths(text: &str) -> Vec<PathState> {
    let mut out = Vec::new();
    for line in text.lines() {
        if !line.split_whitespace().any(|w| w == "path") {
            continue;
        }
        let mut peer: Option<String> = None;
        let mut established: Option<bool> = None;
        let mut peer_node_id: Option<u32> = None;
        let mut local_addr = String::new();
        let mut peer_addr = String::new();
        for word in line.split_whitespace() {
            if let Some(addr) = word.strip_prefix("peer:") {
                peer_addr = addr.to_string();
                // `ipv4:198.51.100.2:7789` or `ipv6:[::1]:7789` to the bare address.
                let addr = addr
                    .strip_prefix("ipv4:")
                    .or_else(|| addr.strip_prefix("ipv6:"))
                    .unwrap_or(addr);
                let bare = addr.rsplit_once(':').map(|(ip, _port)| ip).unwrap_or(addr);
                peer = Some(bare.trim_matches(['[', ']']).to_string());
            } else if let Some(addr) = word.strip_prefix("local:") {
                local_addr = addr.to_string();
            } else if let Some(v) = word.strip_prefix("peer-node-id:") {
                peer_node_id = v.parse().ok();
            } else if let Some(v) = word.strip_prefix("established:") {
                established = Some(v == "yes");
            }
        }
        if let (Some(peer), Some(established)) = (peer, established) {
            out.push(PathState {
                peer,
                established,
                peer_node_id,
                local_addr,
                peer_addr,
            });
        }
    }
    out
}

/// Extract the peer's DRBD node-id from a `drbdsetup events2 --now` snapshot,
/// from the `connection` object line (`exists connection … peer-node-id:N …`),
/// which carries the id even while StandAlone, unlike the `path` lines, which
/// are absent when disconnected. DRBD does not necessarily assign node-ids in
/// hostname order (a `.res` with no explicit `node-id` can give the
/// lower-hostname node id 1), so callers that address the peer by node-id
/// (`drbdsetup net-options <id>`) read it here instead of guessing from
/// hostname order.
pub fn parse_events2_peer_node_id(text: &str) -> Option<u32> {
    for line in text.lines() {
        let mut is_connection = false;
        let mut id: Option<u32> = None;
        for word in line.split_whitespace() {
            match word {
                "connection" => is_connection = true,
                _ if word.starts_with("peer-node-id:") => {
                    id = word
                        .strip_prefix("peer-node-id:")
                        .and_then(|v| v.parse().ok());
                }
                _ => {}
            }
        }
        if is_connection && let Some(id) = id {
            return Some(id);
        }
    }
    None
}

/// The peer's DRBD node-id from a live `events2` snapshot, or `None` if DRBD is
/// unconfigured or the field is absent.
pub async fn peer_node_id(resource: &str) -> Option<u32> {
    let text = crate::cmd::cmd_output_for("drbdsetup", &["events2", "--now", resource], 15)
        .await
        .ok()?;
    parse_events2_peer_node_id(&text)
}

/// Parse a `self`/`peer` message (kernel prefix already removed) into
/// `(is_peer, GenId)`. Original line shape (DRBD 9.2):
///
/// ```text
/// drbd twinstor-data/0 drbd0 twinstor-2: self 816..:000..:769..:1D1.. bits:79872 flags:22
/// drbd twinstor-data/0 drbd0 twinstor-2: peer 1B8..:816..:449..:08F.. bits:44033 flags:1022
/// ```
///
/// Returns `None` for any line that is not a well-formed self/peer generation
/// line (the four colon-separated 64-bit hex UUIDs and dirty count must parse).
fn parse_gen_line(line: &str) -> Option<(bool, GenId)> {
    let toks: Vec<&str> = line.split_whitespace().collect();
    let is_peer = match *toks.first()? {
        "self" => false,
        "peer" => true,
        _ => return None,
    };
    let uuid_tok = *toks.get(1)?;
    if uuid_tok.matches(':').count() != 3 {
        return None;
    }
    let mut parts = uuid_tok.split(':');
    let current = u64::from_str_radix(parts.next()?, 16).ok()?;
    let bitmap = u64::from_str_radix(parts.next()?, 16).ok()?;
    let h0 = u64::from_str_radix(parts.next()?, 16).ok()?;
    let h1 = u64::from_str_radix(parts.next()?, 16).ok()?;
    let bits = toks
        .iter()
        .find_map(|w| w.strip_prefix("bits:"))
        .and_then(|v| v.parse().ok())?;
    // Flags are advisory: an absent or malformed value reads as "no flags"
    // rather than rejecting a pair whose UUIDs and dirty count parsed.
    let flags = toks
        .iter()
        .find_map(|w| w.strip_prefix("flags:"))
        .and_then(|v| u64::from_str_radix(v, 16).ok())
        .unwrap_or(0);
    Some((
        is_peer,
        GenId {
            current,
            bitmap,
            history: [h0, h1],
            bits,
            flags,
        },
    ))
}

/// How many `drbd_sync_handshake:` headers the buffer holds for this
/// resource's volume 0, with the same record filter as `parse_handshake_gen`,
/// so "blocks > 0 but no pair" means the latest handshake block was incomplete
/// or malformed rather than absent. Telemetry only, never a decision input.
pub fn handshake_block_count(dmesg: &str, resource: &str) -> usize {
    let volume = format!("{resource}/0");
    dmesg
        .lines()
        .filter_map(|line| line.split_once("drbd ").map(|(_, record)| record))
        .filter(|record| record.split_whitespace().next() == Some(volume.as_str()))
        .filter(|record| {
            record
                .split_once(": ")
                .is_some_and(|(_, message)| message.trim() == "drbd_sync_handshake:")
        })
        .count()
}

/// Parse the latest explicitly delimited handshake for volume 0. Only a
/// `drbd_sync_handshake` header opens a window; the next two records in that
/// resource/volume must be self, then peer, with the same device/peer prefix.
/// A newer header invalidates the previous result even if its block is
/// partial. UUID dumps outside a window are diagnostics, neither evidence nor
/// invalidation. This validates log structure, not currency: a complete
/// historical pair cannot by itself authorise a data discard.
pub fn parse_handshake_gen(dmesg: &str, resource: &str) -> Option<HandshakeGen> {
    enum Window<'a> {
        Closed,
        SelfRecord(&'a str),
        PeerRecord(&'a str, GenId),
    }
    let mut window = Window::Closed;
    let mut latest: Option<HandshakeGen> = None;
    let volume = format!("{resource}/0");
    for line in dmesg.lines() {
        let Some((_, record)) = line.split_once("drbd ") else {
            continue;
        };
        if record.split_whitespace().next() != Some(volume.as_str()) {
            continue;
        }
        let Some((context, message)) = record.split_once(": ") else {
            window = Window::Closed;
            continue;
        };
        if message.trim() == "drbd_sync_handshake:" {
            latest = None;
            window = Window::SelfRecord(context);
            continue;
        }
        // Consume the window before parsing: an unexpected or malformed record
        // closes it, so a later diagnostic dump cannot complete the handshake.
        match std::mem::replace(&mut window, Window::Closed) {
            Window::SelfRecord(expected) if context == expected => {
                if let Some((false, local)) = parse_gen_line(message) {
                    window = Window::PeerRecord(context, local);
                }
            }
            Window::PeerRecord(expected, local) if context == expected => {
                if let Some((true, peer)) = parse_gen_line(message) {
                    latest = Some(HandshakeGen { local, peer });
                }
            }
            _ => {}
        }
    }
    latest
}

/// Parse `drbdadm status` text output. Handles both DRBD 8.4 and 9.x formats:
///
/// ```text
/// twinstor-data role:Primary
///   disk:UpToDate
///   twinstor-2 role:Primary
///     replication:SyncSource peer-disk:Inconsistent done:1.52
/// ```
pub fn parse_drbdadm_text(text: &str) -> Result<DrbdStatus> {
    fn extract(line: &str, key: &str) -> Option<String> {
        line.find(key).map(|i| {
            let start = i + key.len();
            line[start..]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string()
        })
    }

    let mut role = Role::Unknown;
    let mut disk = DiskState::Unknown;
    let mut connection = ConnectionState::StandAlone;
    let mut peer_role: Option<Role> = None;
    let mut peer_disk: Option<DiskState> = None;
    let mut suspended_user = false;
    let mut suspended_fencing = false;
    let mut suspended_any = false;
    let mut replication: Option<Replication> = None;
    let mut resync_done: Option<f64> = None;
    let mut has_peer = false;
    // Distinguish "StandAlone by default (never parsed a connection)" from
    // "StandAlone stated explicitly". Only the former may be inferred to
    // Connected from peer presence below.
    let mut explicit_connection = false;

    for line in text.lines() {
        let trimmed = line.trim();

        if let Some(r) = extract(trimmed, "role:") {
            if role == Role::Unknown {
                role = parse_role(&r);
            } else {
                // DRBD 9: after the local "<resource> role:..." line, any
                // further "role:" line is the peer's
                // ("<peer-hostname> role:Primary").
                peer_role = Some(parse_role(&r));
                has_peer = true;
            }
        }
        if let Some(d) = extract(trimmed, "disk:")
            && disk == DiskState::Unknown
        {
            disk = parse_disk_state(&d);
        }
        // DRBD 8.4: the peer line starts with the hostname (indented), for
        // example "  xcp-ng-twinstor-1 role:Primary". Count a peer only when
        // peer-disk is present too.
        if extract(trimmed, "peer-disk:").is_some() {
            has_peer = true;
        }
        if let Some(c) = extract(trimmed, "connection:") {
            connection = parse_connection_state(&c);
            explicit_connection = true;
        }
        // DRBD 8.4: "replication:SyncSource peer-disk:Inconsistent"
        // DRBD 9.x: "peer-disk:UpToDate"
        if let Some(d) = extract(trimmed, "peer-disk:") {
            peer_disk = Some(parse_disk_state(&d));
        }
        // DRBD 8.4: "cs:Connected" or "cs:SyncSource"
        if let Some(c) = extract(trimmed, "cs:") {
            connection = parse_connection_state(&c);
            explicit_connection = true;
            // cs: doubles as the replication state in 8.4 (SyncSource/SyncTarget).
            if let Some(r) = parse_replication(&c) {
                replication = Some(r);
            }
        }
        // I/O suspend reason on the resource line (DRBD 9). Only an explicit
        // user/script suspend is actionable; fencing/quorum suspends self-resume.
        // Whole token only: the DRBD 9 peer-device field `resync-suspended:`
        // (a paused resync) is not an I/O suspend.
        if let Some(s) = trimmed
            .split_whitespace()
            .find_map(|t| t.strip_prefix("suspended:"))
            .map(str::to_string)
        {
            // The value can be a comma-joined set ("user,fencing").
            suspended_user = s.split(',').any(|v| v == "user");
            suspended_fencing = s.split(',').any(|v| v == "fencing");
            suspended_any = s.split(',').any(|v| !v.is_empty() && v != "no");
        }
        // DRBD 9 resync line: "replication:SyncSource ... done:NN.NN"
        if let Some(r) = extract(trimmed, "replication:") {
            replication = parse_replication(&r);
        }
        if let Some(d) = extract(trimmed, "done:") {
            resync_done = d.parse().ok();
        }
        // DRBD 8.4: "ds:UpToDate/Inconsistent"
        if let Some(ds) = extract(trimmed, "ds:") {
            let parts: Vec<&str> = ds.split('/').collect();
            if !parts.is_empty() && disk == DiskState::Unknown {
                disk = parse_disk_state(parts[0]);
            }
            if parts.len() > 1 {
                peer_disk = Some(parse_disk_state(parts[1]));
            }
        }
        // DRBD 8.4: "ro:Primary/Primary"
        if let Some(ro) = extract(trimmed, "ro:") {
            let parts: Vec<&str> = ro.split('/').collect();
            if !parts.is_empty() {
                role = parse_role(parts[0]);
            }
            if parts.len() > 1 {
                peer_role = Some(parse_role(parts[1]));
            }
        }
    }

    // DRBD 8.4 does not print "connection:Connected". A peer (role or disk)
    // with no explicit connection state means connected. An explicit
    // StandAlone (DRBD 9 disconnected) is left as is: inferring Connected
    // there could mask a diverged peer.
    if has_peer && !explicit_connection && connection == ConnectionState::StandAlone {
        connection = ConnectionState::Connected;
    }

    Ok(DrbdStatus {
        role,
        peer_role,
        connection,
        disk,
        peer_disk,
        suspended_user,
        suspended_fencing,
        suspended_any,
        replication,
        resync_done,
    })
}

/// Split a dry-run command line into words the way a POSIX shell would for
/// the quoting drbdadm emits (single and double quotes, backslash escapes).
fn shell_words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = line.chars();
    let (mut single, mut double) = (false, false);
    while let Some(c) = chars.next() {
        match c {
            '\'' if !double => {
                single = !single;
                in_word = true;
            }
            '"' if !single => {
                double = !double;
                in_word = true;
            }
            '\\' if !single => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
                in_word = true;
            }
            c if c.is_whitespace() && !single && !double => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

/// The `drbdsetup` subcommand of a dry-run command line, or "" for anything
/// else.
fn dry_run_kind(l: &str) -> &str {
    l.strip_prefix("drbdsetup ")
        .and_then(|r| r.split_whitespace().next())
        .unwrap_or("")
}

/// The commands that re-create a missing peer object, from a dry-run adjust
/// that proposes nothing else: `new-peer`, `new-path`, `peer-device-options`
/// (run in that order), with `connect` (left to the caller), `net-options`
/// and `resource-options` (re-applied live by the caller) tolerated. `None`
/// when there is no `new-peer` or anything else is proposed.
fn peer_recreate_commands<'a>(cmds: &[&'a str]) -> Option<Vec<&'a str>> {
    let kind = dry_run_kind;
    if !cmds.iter().any(|l| kind(l) == "new-peer") {
        return None;
    }
    let ok = [
        "new-peer",
        "new-path",
        "peer-device-options",
        "connect",
        "net-options",
        "resource-options",
    ];
    if !cmds.iter().all(|l| ok.contains(&kind(l))) {
        return None;
    }
    let mut out = Vec::new();
    for k in ["new-peer", "new-path", "peer-device-options"] {
        out.extend(cmds.iter().filter(|l| kind(l) == k).copied());
    }
    Some(out)
}

/// The commands a `drbdadm -d adjust` dry run would run (blank lines and
/// comments dropped).
fn dry_run_commands(text: &str) -> Vec<&str> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

/// Whether a DRBD debugfs connection `debug` file shows the sticky
/// `CONN_DISCARD_MY_DATA` flag. `None` when it has no `flags:` line.
fn debugfs_discard_flag(text: &str) -> Option<bool> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("flags:"))?;
    Some(
        line.split(|c: char| c == '|' || c.is_whitespace())
            .any(|t| t == "CONN_DISCARD_MY_DATA"),
    )
}

async fn run_drbdadm(args: &[&str]) -> Result<()> {
    debug!(args = ?args, "running drbdadm");
    // Bounded: every DRBD control operation (up, attach, connect,
    // disconnect, primary, secondary, force-primary) goes through here and
    // can run inline from the tick. drbdadm can block indefinitely on a
    // wedged DRBD netlink; 30 s allows a slow adjust while still bounding a
    // hang.
    crate::cmd::run_cmd_for("drbdadm", args, 30).await
}

fn parse_role(s: &str) -> Role {
    match s {
        "Primary" => Role::Primary,
        "Secondary" => Role::Secondary,
        _ => Role::Unknown,
    }
}

fn parse_connection_state(s: &str) -> ConnectionState {
    match s {
        "Connected" | "SyncSource" | "SyncTarget" => ConnectionState::Connected,
        "Connecting" | "WFConnection" | "WFReportParams" => ConnectionState::Connecting,
        "StandAlone" | "Unconnected" => ConnectionState::StandAlone,
        "Disconnecting" | "TearDown" => ConnectionState::Disconnecting,
        _ => ConnectionState::Unknown,
    }
}

fn parse_replication(s: &str) -> Option<Replication> {
    match s {
        "Established" => Some(Replication::Established),
        "SyncSource" => Some(Replication::SyncSource),
        "SyncTarget" => Some(Replication::SyncTarget),
        "WFBitMapS" | "WFBitMapT" => Some(Replication::WFBitMap),
        // Connected, WFConnection and the like are connection states, not
        // replication states.
        "Connected" | "Connecting" | "WFConnection" | "WFReportParams" | "StandAlone"
        | "Unconnected" | "Disconnecting" | "TearDown" => None,
        // Any other replication state DRBD reports (Paused*, Ahead, Behind, ...).
        _ => Some(Replication::Other),
    }
}

fn parse_disk_state(s: &str) -> DiskState {
    match s {
        "UpToDate" => DiskState::UpToDate,
        // Consistent: a valid point-in-time copy that is not current (such as
        // a node that crashed or rebooted while the peer kept serving). Kept
        // apart from Unknown so the split-brain freshness gate can treat it as
        // known stale and never let it keep its data over an UpToDate peer.
        "Consistent" => DiskState::Consistent,
        "Inconsistent" => DiskState::Inconsistent,
        "Outdated" => DiskState::Outdated,
        "Diskless" => DiskState::Diskless,
        _ => DiskState::Unknown,
    }
}

/// The two local metadata flags the sole-survivor escalation reads from
/// `drbdadm show-gi`. `crashed_primary` is DRBD's "this node was a crashed
/// primary and has not seen its peer since". It proves this node was the last
/// writer among the states this node itself saw, not the last writer overall:
/// a peer that served alone after this node crashed and then lost its disk is
/// invisible to the flag. So the flag is defence in depth behind the peer's
/// own attestation, never proof by itself. `peer_outdated` means the peer's
/// disk was outdated or inconsistent (set by a successful fence; a crashed
/// Primary that did outdate its peer boots UpToDate, not Consistent, and never
/// reaches the escalation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GiFlags {
    pub crashed_primary: bool,
    pub peer_outdated: bool,
}

/// Read the local generation-identifier flags. `None` on any parse or command
/// failure; callers treat `None` as "no evidence", never as a green light.
pub async fn local_gi_flags(resource: &str) -> Option<GiFlags> {
    let res = format!("twinstor-{resource}");
    let text = crate::cmd::cmd_output_for("drbdadm", &["show-gi", &res], 15)
        .await
        .ok()?;
    parse_gi_flags(&text)
}

/// The current generation UUID (bit 0 masked, as DRBD compares) from the
/// `drbdadm show-gi` data line, the same line `parse_gi_flags` reads.
pub fn parse_gi_current(text: &str) -> Option<u64> {
    let line = text.lines().find(|l| parse_gi_flags(l).is_some())?;
    u64::from_str_radix(line.trim().split(':').next()?, 16)
        .ok()
        .map(|u| u & !UUID_PRIMARY)
}

/// Parse the flag fields of a `drbdadm show-gi` data line. The line is four
/// 16-hex-digit UUIDs followed by (at least) 12 single-digit flags, all
/// colon-separated. Flag order per the DRBD 9 legend: 1 consistency,
/// 2 up-to-date, 3 currently-primary, 4 crashed-primary-unseen-peer,
/// 5 al-applied, 6 al-disabled, 7 lost-quorum-as-primary, 8 connected,
/// 9 peer-outdated, 10 fence-policy-used, 11 marking-out-of-sync,
/// 12 seen-attached. Anything that does not match this shape returns `None`.
fn parse_gi_flags(text: &str) -> Option<GiFlags> {
    for line in text.lines() {
        let fields: Vec<&str> = line.trim().split(':').collect();
        if fields.len() < 16 {
            continue;
        }
        let uuids_ok = fields[..4]
            .iter()
            .all(|f| f.len() == 16 && f.chars().all(|c| c.is_ascii_hexdigit()));
        let flags_ok = fields[4..16].iter().all(|f| *f == "0" || *f == "1");
        if !(uuids_ok && flags_ok) {
            continue;
        }
        return Some(GiFlags {
            crashed_primary: fields[7] == "1",
            peer_outdated: fields[12] == "1",
        });
    }
    None
}

/// What a node can prove locally about its own configured backing device.
/// Served to the peer over the backing beacon (see `peer_signal`). The peer's
/// sole-survivor escalation may force-promote automatically only on
/// `Destroyed`, the one state that proves no replica exists on this backing.
/// `Absent` (device node missing) does not prove that: the data may be intact
/// on an unassembled array or a disk that has not enumerated, so it maps to the
/// guided manual path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackingMeta {
    /// Local disk is attached (not Diskless): normal operation.
    Attached,
    /// Configured backing device node exists and is readable, but carries no
    /// valid DRBD metadata: there is provably no replica on it.
    Destroyed,
    /// Configured backing device node exists and carries valid DRBD metadata:
    /// the data exists but is not attached (attach failing or pending). The
    /// peer must never force-promote against this: the copy is real.
    Detached,
    /// Configured backing device node does not exist. The data may still exist
    /// (unassembled md array, disk not enumerated), so this is never proof of
    /// loss.
    Absent,
    /// Could not classify (probe error or timeout, unexpected drbdmeta output).
    Unknown,
}

/// Probe the local backing device's DRBD metadata. Call only while the local
/// disk is Diskless (an attached device belongs to the kernel; report
/// [`BackingMeta::Attached`] without probing).
pub async fn probe_backing_meta(backing: &std::path::Path, resource: &str) -> BackingMeta {
    if !backing.exists() {
        return BackingMeta::Absent;
    }
    let res = format!("twinstor-{resource}");
    // Two-step probe (drbd-utils 9.33):
    // 1. `get-gi` succeeding proves valid metadata exists (Detached: a real
    //    copy, never force against it). It is cheap and safe on a timer.
    // 2. A failing `get-gi` prints no diagnostic on a blank device (exit 255,
    //    empty stderr), so the failure is classified by running `dump-md`,
    //    whose "No valid meta data found" is the one reliable Destroyed
    //    signal. dump-md runs only after get-gi failed, when there is no
    //    metadata (and so no multi-megabyte bitmap) to dump, so its output
    //    stays small.
    // Anything unrecognised stays Unknown, which the peer's escalation treats
    // like Absent (no automatic action).
    let gi = crate::cmd::cmd_status_for("drbdadm", &["get-gi", &res], 15).await;
    match gi {
        Ok((true, _, _)) => BackingMeta::Detached,
        Ok((false, _, _)) => {
            let dump = crate::cmd::cmd_status_for("drbdadm", &["dump-md", &res], 15).await;
            match dump {
                // dump-md succeeding after get-gi failed is contradictory;
                // treat it as metadata present (the conservative reading).
                Ok((true, _, _)) => BackingMeta::Detached,
                Ok((false, stdout, stderr)) => classify_meta_probe_failure(&stdout, &stderr),
                Err(_) => BackingMeta::Unknown,
            }
        }
        Err(_) => BackingMeta::Unknown,
    }
}

fn classify_meta_probe_failure(stdout: &str, stderr: &str) -> BackingMeta {
    let all = format!("{stdout}\n{stderr}").to_ascii_lowercase();
    if all.contains("no valid meta data") || all.contains("no valid meta-data") {
        BackingMeta::Destroyed
    } else if all.contains("open(") && all.contains("no such file or directory") {
        // drbdmeta's device-open failure reads
        // `open(<device>) failed: No such file or directory`. Requiring
        // `open(` keeps an ENOENT about something else (a missing drbdadm
        // config file, say) from passing as a missing device; that case must
        // stay Unknown so the peer's guidance does not point at the wrong
        // problem.
        BackingMeta::Absent
    } else {
        // Includes the "Device ... is configured!" and "is attached" refusals
        // and any unknown drbdmeta wording: never guess Destroyed.
        BackingMeta::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_int_is_read_from_the_show_dump() {
        let show = "resource twinstor-data {\n    connection {\n        net {\n            connect-int     \t2; # seconds\n            fencing         \tresource-and-stonith;\n        }\n    }\n}\n";
        assert_eq!(super::parse_connect_int(show), Some(2));
        assert_eq!(
            super::parse_connect_int("net {\n  fencing resource-only;\n}"),
            None
        );
        assert_eq!(super::parse_connect_int("connect-interval 3;"), None);
    }

    #[test]
    fn debugfs_discard_flag_reads_the_sticky_bit() {
        // Real DRBD 9.2.16 debugfs lines.
        assert_eq!(
            debugfs_discard_flag(
                "flags: 0x81804 : TWOPC_YES|RECONNECT|CONN_DISCARD_MY_DATA|CONN_HANDSHAKE_READY\n"
            ),
            Some(true)
        );
        assert_eq!(
            debugfs_discard_flag("flags: 0x80804 : TWOPC_YES|RECONNECT|CONN_HANDSHAKE_READY\n"),
            Some(false)
        );
        assert_eq!(
            debugfs_discard_flag("other: 1\nflags: 0x0000 :\n"),
            Some(false)
        );
        assert_eq!(debugfs_discard_flag("no flags here"), None);
        assert_eq!(
            debugfs_discard_flag("flags: 0x1 : NOT_CONN_DISCARD_MY_DATA_X"),
            Some(false)
        );
    }

    #[test]
    fn shell_words_handles_drbdadm_quoting() {
        assert_eq!(
            shell_words("drbdsetup new-peer r 1 --_name=b --shared-secret='a b' --x=\"c d\""),
            vec![
                "drbdsetup",
                "new-peer",
                "r",
                "1",
                "--_name=b",
                "--shared-secret=a b",
                "--x=c d"
            ]
        );
        assert_eq!(shell_words("  a   b "), vec!["a", "b"]);
    }

    #[test]
    fn peer_recreation_runs_only_the_connection_commands_without_connecting() {
        let peer = "drbdsetup new-peer twinstor-data 1 --_name=b --protocol=C";
        let path =
            "drbdsetup new-path twinstor-data 1 ipv4:198.51.100.1:7789 ipv4:198.51.100.2:7789";
        let pdo = "drbdsetup peer-device-options twinstor-data 1 0 --c-min-rate=250M";
        let conn = "drbdsetup connect twinstor-data 1";
        assert_eq!(
            peer_recreate_commands(&[conn, pdo, path, peer]),
            Some(vec![peer, path, pdo]),
            "ordered, and never the connect"
        );
        assert_eq!(
            peer_recreate_commands(&[
                peer,
                path,
                conn,
                "drbdsetup resource-options twinstor-data --on-no-data-accessible=io-error"
            ]),
            Some(vec![peer, path])
        );
        assert_eq!(
            peer_recreate_commands(&[path, conn]),
            None,
            "no missing peer"
        );
        assert_eq!(peer_recreate_commands(&[]), None);
        assert_eq!(
            peer_recreate_commands(&[peer, "drbdsetup attach 0 /dev/x /dev/x internal"]),
            None,
            "anything beyond the connection is not ours"
        );
    }

    #[test]
    fn dry_run_commands_drop_noise() {
        let t = "\n# comment\n  drbdsetup new-path twinstor-data 1 ipv4:198.51.100.1:7789 ipv4:198.51.100.2:7789\ndrbdsetup connect twinstor-data 1\n";
        assert_eq!(
            dry_run_commands(t),
            vec![
                "drbdsetup new-path twinstor-data 1 ipv4:198.51.100.1:7789 ipv4:198.51.100.2:7789",
                "drbdsetup connect twinstor-data 1",
            ]
        );
        assert!(dry_run_commands("").is_empty());
    }

    #[test]
    fn parse_drbd84_connected() {
        let text = "0: cs:SyncSource ro:Primary/Primary ds:UpToDate/Inconsistent C r-----\n\
                     ns:1594616 nr:0 dw:0 dr:1596720";
        let status = parse_drbdadm_text(text).unwrap();
        assert_eq!(status.role, Role::Primary);
        assert_eq!(status.connection, ConnectionState::Connected);
        assert_eq!(status.peer_role, Some(Role::Primary));
        assert_eq!(status.disk, DiskState::UpToDate);
        assert_eq!(status.peer_disk, Some(DiskState::Inconsistent));
    }

    #[test]
    fn parse_drbd9_text() {
        let text = "twinstor-data role:Primary\n  disk:UpToDate\n  \
                     xcp-ng-twinstor-1 role:Primary\n    \
                     replication:SyncSource peer-disk:Inconsistent done:1.52";
        let status = parse_drbdadm_text(text).unwrap();
        assert_eq!(status.role, Role::Primary);
        assert_eq!(status.disk, DiskState::UpToDate);
        // The peer line is "<hostname> role:Primary", with no "peer role:"
        // literal.
        assert_eq!(status.peer_role, Some(Role::Primary));
        assert_eq!(status.peer_disk, Some(DiskState::Inconsistent));
    }

    #[test]
    fn parse_drbd9_healthy_dual_primary() {
        // The shape of `drbdadm status` output (DRBD 9.2).
        let text = "twinstor-data role:Primary\n  disk:UpToDate open:yes\n  \
                     twinstor-2 role:Primary\n    peer-disk:UpToDate";
        let status = parse_drbdadm_text(text).unwrap();
        assert_eq!(status.role, Role::Primary);
        assert_eq!(status.peer_role, Some(Role::Primary));
        assert_eq!(status.connection, ConnectionState::Connected);
        assert_eq!(status.peer_disk, Some(DiskState::UpToDate));
    }

    #[test]
    fn parse_standalone() {
        let text = "twinstor-data role:Secondary\n  disk:UpToDate\n  \
                     xcp-ng-twinstor-1 connection:StandAlone";
        let status = parse_drbdadm_text(text).unwrap();
        assert_eq!(status.role, Role::Secondary);
        assert_eq!(status.connection, ConnectionState::StandAlone);
        assert_eq!(status.disk, DiskState::UpToDate);
    }

    #[test]
    fn parse_suspended_user() {
        // The fence-peer handler's leftover `drbdadm suspend-io`, as it renders
        // in `drbdadm status` (DRBD 9.2): suspended:user on the resource line.
        let text = "twinstor-data role:Primary suspended:user\n  \
                     disk:UpToDate open:yes blocked:upper\n  \
                     twinstor-2 role:Primary\n    peer-disk:UpToDate";
        let status = parse_drbdadm_text(text).unwrap();
        assert!(status.suspended_user);
        assert_eq!(status.connection, ConnectionState::Connected);
        assert_eq!(status.disk, DiskState::UpToDate);
        assert_eq!(status.peer_disk, Some(DiskState::UpToDate));
        assert_eq!(status.replication, None);
    }

    #[test]
    fn parse_suspended_set_variants() {
        // `suspended:` can be a comma-joined set; fencing must be seen both on
        // its own and combined, or a fencing freeze is invisible to the heals.
        let t = |v: &str| {
            parse_drbdadm_text(&format!(
                "twinstor-data role:Primary suspended:{v}\n  disk:UpToDate\n"
            ))
            .unwrap()
        };
        let s = t("fencing");
        assert!(s.suspended_fencing && !s.suspended_user);
        assert!(s.suspended_any);
        // no-data (on-no-data-accessible suspend-io) and quorum are neither
        // user nor fencing, but local I/O is frozen all the same.
        let s = t("no-data");
        assert!(s.suspended_any && !s.suspended_fencing && !s.suspended_user);
        let s = t("quorum");
        assert!(s.suspended_any);
        assert!(!t("no").suspended_any);
        // A paused resync on the peer-device line is not an I/O suspend.
        let s = parse_drbdadm_text(
            "twinstor-data role:Primary suspended:no\n  disk:UpToDate\n  peer role:Primary\n    replication:PausedSyncS peer-disk:Inconsistent resync-suspended:user\n",
        )
        .unwrap();
        assert!(!s.suspended_any && !s.suspended_user);
        let s = t("user,fencing");
        assert!(s.suspended_fencing && s.suspended_user);
        let s = t("user");
        assert!(!s.suspended_fencing && s.suspended_user);
    }

    #[test]
    fn parse_healthy_not_suspended_no_replication() {
        // Established link: drbdadm status omits replication: and done:, and
        // prints suspended:no.
        let text = "twinstor-data role:Primary suspended:no\n  \
                     disk:UpToDate open:yes\n  \
                     twinstor-2 role:Primary\n    peer-disk:UpToDate";
        let status = parse_drbdadm_text(text).unwrap();
        assert!(!status.suspended_user);
        assert_eq!(status.replication, None);
        assert_eq!(status.resync_done, None);
    }

    #[test]
    fn parse_mutual_sync_source_limbo() {
        // The dual-reboot wedge as seen from one node: SyncSource toward an
        // Inconsistent peer, done frozen, while local disk is UpToDate.
        let text = "twinstor-data role:Primary\n  disk:UpToDate open:yes\n  \
                     twinstor-1 role:Primary\n    \
                     replication:SyncSource peer-disk:Inconsistent done:0.00";
        let status = parse_drbdadm_text(text).unwrap();
        assert_eq!(status.role, Role::Primary);
        assert_eq!(status.disk, DiskState::UpToDate);
        assert_eq!(status.replication, Some(Replication::SyncSource));
        assert_eq!(status.peer_disk, Some(DiskState::Inconsistent));
        assert_eq!(status.resync_done, Some(0.0));
        assert_eq!(status.connection, ConnectionState::Connected);
    }

    #[test]
    fn parse_diskless() {
        let text = "twinstor-data role:Secondary\n  disk:Diskless client:no";
        let status = parse_drbdadm_text(text).unwrap();
        assert_eq!(status.disk, DiskState::Diskless);
    }

    #[test]
    fn all_disk_states() {
        assert_eq!(parse_disk_state("UpToDate"), DiskState::UpToDate);
        assert_eq!(parse_disk_state("Inconsistent"), DiskState::Inconsistent);
        assert_eq!(parse_disk_state("Outdated"), DiskState::Outdated);
        assert_eq!(parse_disk_state("Diskless"), DiskState::Diskless);
        assert_eq!(parse_disk_state("SomethingNew"), DiskState::Unknown);
    }

    #[test]
    fn empty_input_fails_closed() {
        // Garbage in must never panic and must land on the safe defaults:
        // no peer, StandAlone, Unknown role/disk. Every transition keys off
        // these, so an unparseable status must look like "nothing is healthy".
        let status = parse_drbdadm_text("").unwrap();
        assert_eq!(status.role, Role::Unknown);
        assert_eq!(status.connection, ConnectionState::StandAlone);
        assert_eq!(status.disk, DiskState::Unknown);
        assert_eq!(status.peer_role, None);
        assert_eq!(status.peer_disk, None);
    }

    #[test]
    fn truncated_input_fails_closed() {
        // Output cut off mid-line (drbdadm killed, say): the local role parses,
        // but with no peer data the result must show no redundancy, not a
        // stale peer.
        let status = parse_drbdadm_text("twinstor-data role:Primary\n  disk:UpTo").unwrap();
        assert_eq!(status.role, Role::Primary);
        assert_eq!(status.disk, DiskState::Unknown);
        assert_eq!(status.peer_disk, None);
        assert_eq!(status.connection, ConnectionState::StandAlone);
    }

    #[test]
    fn explicit_standalone_is_not_flipped_to_connected() {
        // DRBD 9 disconnected: the peer block carries a role and an explicit
        // connection:StandAlone. The has_peer inference must not override it;
        // reporting a diverged node as Connected is the most dangerous
        // misclassification the parser can make.
        let text = "twinstor-data role:Primary\n  disk:UpToDate\n  \
                     twinstor-2 connection:StandAlone role:Secondary";
        let status = parse_drbdadm_text(text).unwrap();
        assert_eq!(status.role, Role::Primary);
        assert_eq!(status.connection, ConnectionState::StandAlone);
    }

    #[test]
    fn drbd84_peer_disk_unknown_when_unreachable() {
        // DRBD 8.4 with the peer down prints only the local disk in ds: (no
        // '/'). peer_disk must stay None so callers treat redundancy as absent.
        let text = "0: cs:WFConnection ro:Primary/Unknown ds:UpToDate r-----";
        let status = parse_drbdadm_text(text).unwrap();
        assert_eq!(status.disk, DiskState::UpToDate);
        assert_eq!(status.peer_disk, None);
        assert_eq!(status.connection, ConnectionState::Connecting);
    }

    #[test]
    fn events2_paths_parse_arbitrated_pair() {
        // Output of drbdsetup 9.2.16 `events2 --now` for an arbitrated pair.
        let text = "\
exists resource name:twinstor-data role:Primary suspended:no\n\
exists connection name:twinstor-data peer-node-id:1 conn-name:twinstor-2 connection:Connected role:Primary\n\
exists path name:twinstor-data peer-node-id:1 conn-name:twinstor-2 local:ipv4:198.51.100.1:7789 peer:ipv4:198.51.100.2:7789 established:yes\n\
exists path name:twinstor-data peer-node-id:1 conn-name:twinstor-2 local:ipv4:192.0.2.102:7789 peer:ipv4:192.0.2.101:7789 established:no\n\
exists -\n";
        let paths = parse_events2_paths(text);
        assert_eq!(
            paths,
            vec![
                PathState {
                    peer: "198.51.100.2".into(),
                    established: true,
                    peer_node_id: Some(1),
                    local_addr: "ipv4:198.51.100.1:7789".into(),
                    peer_addr: "ipv4:198.51.100.2:7789".into(),
                },
                PathState {
                    peer: "192.0.2.101".into(),
                    established: false,
                    peer_node_id: Some(1),
                    local_addr: "ipv4:192.0.2.102:7789".into(),
                    peer_addr: "ipv4:192.0.2.101:7789".into(),
                },
            ]
        );
    }

    #[test]
    fn events2_paths_none_established_and_fate_shared() {
        // Mid-reconnect both paths can be established:no; fate-shared (one
        // implicit path) emits one line. Non-path lines are ignored.
        let text = "\
exists connection name:twinstor-data peer-node-id:1 conn-name:twinstor-2 connection:Connecting role:Unknown\n\
exists path name:twinstor-data peer-node-id:1 conn-name:twinstor-2 local:ipv4:192.0.2.102:7789 peer:ipv4:192.0.2.101:7789 established:no\n";
        let paths = parse_events2_paths(text);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].peer, "192.0.2.101");
        assert!(!paths[0].established);
        assert_eq!(paths[0].peer_node_id, Some(1));
        assert!(parse_events2_paths("exists resource name:x role:Primary\n").is_empty());
    }

    #[test]
    fn events2_peer_node_id_reads_connection_line_even_standalone() {
        // A .res with no explicit node-id: DRBD gave the lower-hostname node
        // id 1 and the peer id 0, the opposite of hostname order. The
        // connection line reports peer-node-id:0 even while StandAlone, when
        // there are no path lines to read it from.
        let standalone = "\
exists resource name:twinstor-data role:Secondary\n\
exists connection name:twinstor-data peer-node-id:0 conn-name:node-b.example.com connection:StandAlone role:Unknown\n";
        assert_eq!(parse_events2_peer_node_id(standalone), Some(0));
        // Connected pair: peer is node-id 1.
        let connected = "\
exists connection name:twinstor-data peer-node-id:1 conn-name:twinstor-2 connection:Connected role:Primary\n\
exists path name:twinstor-data peer-node-id:1 conn-name:twinstor-2 local:ipv4:198.51.100.1:7789 peer:ipv4:198.51.100.2:7789 established:yes\n";
        assert_eq!(parse_events2_peer_node_id(connected), Some(1));
        // No connection object (unconfigured): None.
        assert_eq!(
            parse_events2_peer_node_id("exists resource name:x role:Primary\n"),
            None
        );
    }

    // A real `drbd_sync_handshake` block (DRBD 9.2). twinstor-2's current
    // (816..E4BC) is twinstor-1's bitmap-base, so node 1 is a strict
    // descendant of node 2 (clean ahead/behind).
    const HANDSHAKE_LOG: &str = "\
[   17.947758] drbd twinstor-data twinstor-2: Handshake to peer 1 successful: Agreed network protocol version 123\n\
[   18.019412] drbd twinstor-data/0 drbd0 twinstor-2: drbd_sync_handshake:\n\
[   18.019415] drbd twinstor-data/0 drbd0 twinstor-2: self 816592755DBCE4BC:0000000000000000:7698A626802BBEBA:1D1846F49D963244 bits:79872 flags:22\n\
[   18.019417] drbd twinstor-data/0 drbd0 twinstor-2: peer 1B8F6CEBED0D772D:816592755DBCE4BC:4492EB8F62D2D574:08FD5D24C8885D78 bits:44033 flags:1022\n";

    #[test]
    fn handshake_block_count_and_lineage_class() {
        use super::{LineageRead, handshake_block_count, lineage_class};
        assert_eq!(handshake_block_count(HANDSHAKE_LOG, "twinstor-data"), 1);
        assert_eq!(handshake_block_count(HANDSHAKE_LOG, "other-res"), 0);
        // A trailing header with no records: a second (incomplete) block.
        let partial = format!(
            "{HANDSHAKE_LOG}[19.0] drbd twinstor-data/0 drbd0 twinstor-2: drbd_sync_handshake:\n"
        );
        assert_eq!(handshake_block_count(&partial, "twinstor-data"), 2);
        assert!(parse_handshake_gen(&partial, "twinstor-data").is_none());
        // The class the resolver logs for each shape.
        let pair = parse_handshake_gen(HANDSHAKE_LOG, "twinstor-data");
        assert_eq!(
            lineage_class(&Ok(LineageRead { pair, blocks: 1 })),
            "present"
        );
        assert_eq!(
            lineage_class(&Ok(LineageRead {
                pair: None,
                blocks: 2
            })),
            "incomplete"
        );
        assert_eq!(
            lineage_class(&Ok(LineageRead {
                pair: None,
                blocks: 0
            })),
            "absent"
        );
        assert_eq!(
            lineage_class(&Err(anyhow::anyhow!("dmesg: timed out"))),
            "collect_failed"
        );
    }

    #[test]
    fn handshake_gen_parses_self_and_peer() {
        let hs = parse_handshake_gen(HANDSHAKE_LOG, "twinstor-data").expect("a pair");
        assert_eq!(hs.local.current, 0x816592755DBCE4BC);
        assert_eq!(hs.local.bitmap, 0);
        assert_eq!(hs.local.history, [0x7698A626802BBEBA, 0x1D1846F49D963244]);
        assert_eq!(hs.local.bits, 79872);
        assert_eq!(hs.peer.current, 0x1B8F6CEBED0D772D);
        assert_eq!(hs.peer.bitmap, 0x816592755DBCE4BC);
        assert_eq!(hs.peer.bits, 44033);
        // The peer descends from the local current (the local current is its
        // bitmap-base), so the peer is ahead.
        assert!(hs.peer.descends_from(hs.local.current));
        assert!(!hs.local.descends_from(hs.peer.current));
        // Flags are hex: 0x22 and 0x1022 both carry the crashed-primary bit.
        assert_eq!(hs.local.flags, 0x22);
        assert_eq!(hs.peer.flags, 0x1022);
        assert!(hs.local.crashed_primary() && hs.peer.crashed_primary());
    }

    #[test]
    fn gen_line_flags_are_hex_and_optional() {
        let line = |flags: &str| format!("self 100:0:0:0 bits:1266688{flags}");
        // Values seen in real handshakes: 20, 120, 1020 and 1120 clean; 22 and
        // 1022 after a crash while Primary (1237 activity-log extents of 1024
        // bits each).
        for (raw, crashed) in [
            ("20", false),
            ("120", false),
            ("1020", false),
            ("1120", false),
            ("22", true),
            ("1022", true),
        ] {
            let (_, g) = parse_gen_line(&line(&format!(" flags:{raw}"))).expect("parses");
            assert_eq!(g.flags, u64::from_str_radix(raw, 16).unwrap());
            assert_eq!(g.crashed_primary(), crashed, "flags:{raw}");
            assert_eq!(g.bits, 1266688);
        }
        // Absent or malformed flags never reject the line: no flags assumed.
        for tail in ["", " flags:", " flags:zz"] {
            let (_, g) = parse_gen_line(&line(tail)).expect("UUIDs and bits still parse");
            assert_eq!(g.flags, 0);
            assert!(!g.crashed_primary());
        }
    }

    #[test]
    fn handshake_gen_takes_the_latest_pair_for_the_resource() {
        // An older pair, then a newer one; lines for a different resource must
        // not leak in. The newest complete pair wins.
        let log = "\
[9.9] drbd twinstor-data/0 drbd0 twinstor-2: drbd_sync_handshake:\n\
[10.0] drbd twinstor-data/0 drbd0 twinstor-2: self AAAA000000000001:0:0:0 bits:1 flags:0\n\
[10.1] drbd twinstor-data/0 drbd0 twinstor-2: peer BBBB000000000002:0:0:0 bits:2 flags:0\n\
[20.0] drbd other-res/0 drbd9 other: self DEAD000000000009:0:0:0 bits:9 flags:0\n\
[20.1] drbd other-res/0 drbd9 other: peer BEEF00000000000A:0:0:0 bits:9 flags:0\n\
[29.9] drbd twinstor-data/0 drbd0 twinstor-2: drbd_sync_handshake:\n\
[30.0] drbd twinstor-data/0 drbd0 twinstor-2: self CCCC000000000003:0:0:0 bits:3 flags:0\n\
[30.1] drbd twinstor-data/0 drbd0 twinstor-2: peer DDDD000000000004:0:0:0 bits:4 flags:0\n";
        let hs = parse_handshake_gen(log, "twinstor-data").expect("a pair");
        assert_eq!(hs.local.current, 0xCCCC000000000003);
        assert_eq!(hs.peer.current, 0xDDDD000000000004);
    }

    #[test]
    fn handshake_gen_none_without_a_complete_pair() {
        assert!(parse_handshake_gen("no drbd here\n", "twinstor-data").is_none());
        // A self line with no following peer line is not a usable pair.
        let only_self = "[1.0] drbd twinstor-data/0 drbd0 twinstor-2: self AAAA000000000001:0:0:0 bits:1 flags:0\n";
        assert!(parse_handshake_gen(only_self, "twinstor-data").is_none());
    }

    #[test]
    fn descends_from_ignores_zero_and_matches_history() {
        let g = GenId {
            current: 5,
            bitmap: 0,
            history: [7, 9],
            bits: 0,
            flags: 0,
        };
        assert!(g.descends_from(7));
        assert!(g.descends_from(9));
        assert!(!g.descends_from(5));
        // A zero UUID (unset slot) must never count as a match.
        assert!(!g.descends_from(0));
        let z = GenId {
            current: 5,
            bitmap: 0,
            history: [0, 0],
            bits: 0,
            flags: 0,
        };
        assert!(!z.descends_from(0));
    }

    #[test]
    fn generation_compare_ignores_the_primary_bit() {
        // From a real handshake: the peer's current ...4074 is this node's
        // bitmap-base ...4075 with the Primary bit set, so the same generation
        // (DRBD resolved it bitmap-self, not split-brain).
        let g = GenId {
            current: 0x9A3F_11C0_77E2_5D10,
            bitmap: 0x5E21_C8B4_06A1_4075,
            history: [0x2B7D_90E3_14C8_A6F2, 0],
            bits: 31,
            flags: 0x120,
        };
        assert!(g.descends_from(0x5E21_C8B4_06A1_4074));
        assert!(g.descends_from(0x2B7D_90E3_14C8_A6F3));
        assert!(same_generation(0x10, 0x11));
        assert!(!same_generation(0x10, 0x12));
        // Masking must not turn an unset slot into a match.
        assert!(!same_generation(0, 1));
        assert!(!g.descends_from(1));
    }

    #[test]
    fn handshake_gen_rejects_incomplete_latest_attempt() {
        for tail in [
            "drbd twinstor-data/0 drbd0 twinstor-2: drbd_sync_handshake:\n",
            "drbd twinstor-data/0 drbd0 twinstor-2: self 100:0:0:0 bits:1 flags:0\n",
            "drbd twinstor-data/0 drbd0 twinstor-2: self truncated\n",
        ] {
            let log = format!(
                "{HANDSHAKE_LOG}drbd twinstor-data/0 drbd0 twinstor-2: drbd_sync_handshake:\n{tail}"
            );
            assert!(
                parse_handshake_gen(&log, "twinstor-data").is_none(),
                "{tail}"
            );
        }
    }

    #[test]
    fn handshake_gen_never_pairs_across_attempt_headers() {
        let log = "drbd twinstor-data/0 drbd0 twinstor-2: self 100:0:0:0 bits:1 flags:0\n\
                   drbd twinstor-data/0 drbd0 twinstor-2: drbd_sync_handshake:\n\
                   drbd twinstor-data/0 drbd0 twinstor-2: peer 200:0:0:0 bits:2 flags:0\n";
        assert!(parse_handshake_gen(log, "twinstor-data").is_none());
    }

    #[test]
    fn handshake_gen_rejects_invalid_dirty_counts_on_either_side() {
        for count in ["", "bits:garbage", "bits:18446744073709551616"] {
            for old_count in ["bits:79872", "bits:44033"] {
                let invalid = HANDSHAKE_LOG.replace(old_count, count);
                // A malformed latest attempt must not resurrect a previous valid one.
                let log = format!("{HANDSHAKE_LOG}{invalid}");
                assert!(parse_handshake_gen(&log, "twinstor-data").is_none());
            }
        }
    }

    #[test]
    fn handshake_gen_ignores_non_handshake_uuid_dumps() {
        // receive_state and bitmap-correction paths also emit these UUID lines.
        let diagnostics = "drbd twinstor-data/0 drbd0 twinstor-2: was SyncSource, missed the resync finished event, corrected myself:\n\
                           drbd twinstor-data/0 drbd0 twinstor-2: self 100:0:0:0 bits:1 flags:0\n\
                           drbd twinstor-data/0 drbd0 twinstor-2: my exposed UUID: 0000000000000000\n\
                           drbd twinstor-data/0 drbd0 twinstor-2: peer 200:0:0:0 bits:1266688 flags:0\n";
        assert!(parse_handshake_gen(diagnostics, "twinstor-data").is_none());
        let expected = parse_handshake_gen(HANDSHAKE_LOG, "twinstor-data");
        assert_eq!(
            parse_handshake_gen(&format!("{HANDSHAKE_LOG}{diagnostics}"), "twinstor-data"),
            expected
        );
        // Ring buffer beginning with an orphan peer must not create evidence.
        let orphan = "drbd twinstor-data/0 drbd0 twinstor-2: peer 200:0:0:0 bits:1 flags:0\n";
        assert!(parse_handshake_gen(orphan, "twinstor-data").is_none());
        assert_eq!(
            parse_handshake_gen(&format!("{HANDSHAKE_LOG}{orphan}"), "twinstor-data"),
            expected
        );
    }

    #[test]
    fn handshake_gen_requires_ordered_records_in_one_context() {
        let header = "drbd twinstor-data/0 drbd0 twinstor-2: drbd_sync_handshake:\n";
        let local = "drbd twinstor-data/0 drbd0 twinstor-2: self 100:0:0:0 bits:1 flags:0\n";
        let peer = "drbd twinstor-data/0 drbd0 twinstor-2: peer 200:0:0:0 bits:2 flags:0\n";
        for tail in [
            format!("{peer}{local}{peer}"),
            format!("{local}{local}{peer}"),
            format!(
                "{local}drbd twinstor-data/0 drbd0 twinstor-2: Lost last syncUUID packet, corrected:\n{peer}"
            ),
            format!("{local}{}", peer.replace("twinstor-2:", "other-peer:")),
        ] {
            assert!(
                parse_handshake_gen(&format!("{header}{tail}"), "twinstor-data").is_none(),
                "{tail}"
            );
        }
        // Interleaved unrelated kernel/resource messages do not break the window.
        let other = "[9.0] unrelated kernel message\ndrbd other/0 drbd9 other: peer 300:0:0:0 bits:3 flags:0\n";
        assert!(
            parse_handshake_gen(
                &format!("{header}{other}{local}{other}{peer}"),
                "twinstor-data"
            )
            .is_some()
        );
    }

    #[test]
    fn handshake_gen_matches_resource_and_volume_exactly() {
        for resource in ["other-twinstor-data", "twinstor-data-backup"] {
            let log = HANDSHAKE_LOG.replace("twinstor-data", resource);
            assert!(parse_handshake_gen(&log, "twinstor-data").is_none());
        }
        let log = HANDSHAKE_LOG.replace("twinstor-data/0", "twinstor-data/1");
        assert!(parse_handshake_gen(&log, "twinstor-data").is_none());
    }

    // Real `show-gi` output of a crashed Primary whose peer was never
    // outdated: crashed-primary set, peer-outdated clear, the metadata shape
    // the sole-survivor escalation keys on.
    const SHOW_GI_CRASHED_PRIMARY: &str = "\n\
       +--<  Current data generation UUID  >-\n\
       |               +--<  Bitmap's base data generation UUID  >-\n\
       V               V\n\
17F5075CE30B4AEA:A0AE9A52B2646D6F:868851C4E8CBB4BC:76BC1F306EEECBE2:1:1:0:1:0:0:0:1:0:1:0:1\n\
                                                                    ^ ^ ^\n";

    #[test]
    fn gi_flags_parse_crashed_primary_bundle_output() {
        let f = parse_gi_flags(SHOW_GI_CRASHED_PRIMARY).expect("must parse");
        assert!(f.crashed_primary, "the field node was a crashed primary");
        assert!(!f.peer_outdated, "fence exit 1 never outdated the peer");
    }

    #[test]
    fn gi_flags_peer_outdated_variant() {
        // Same line with flag 9 (peer-outdated) set: the healthy degraded-boot
        // shape, where fence exit 4 recorded the outdate.
        let text = "17F5075CE30B4AEA:A0AE9A52B2646D6F:868851C4E8CBB4BC:76BC1F306EEECBE2:1:1:0:1:0:0:0:1:1:1:0:1\n";
        let f = parse_gi_flags(text).expect("must parse");
        assert!(f.peer_outdated);
    }

    #[test]
    fn gi_flags_fail_closed_on_garbage() {
        // Legend and arrow lines, truncated flag sets, non-hex UUIDs: all
        // yield None (callers treat None as no evidence and refuse the
        // escalation), never an invented flag reading.
        assert!(parse_gi_flags("").is_none());
        assert_eq!(
            parse_gi_current(
                "17F5075CE30B4AEB:A0AE9A52B2646D6F:868851C4E8CBB4BC:76BC1F306EEECBE2:1:1:0:1:0:0:0:1:0:1:0:1\n"
            ),
            Some(0x17F5_075C_E30B_4AEA),
            "the current generation, Primary bit masked"
        );
        assert_eq!(
            parse_gi_current("       +--<  Current data generation UUID  >-\n"),
            None
        );
        assert!(parse_gi_flags("       +--<  Current data generation UUID  >-\n").is_none());
        assert!(
            parse_gi_flags(
                "17F5075CE30B4AEA:A0AE9A52B2646D6F:868851C4E8CBB4BC:76BC1F306EEECBE2:1:1:0\n"
            )
            .is_none()
        );
        assert!(parse_gi_flags("XYZ5075CE30B4AEA:A0AE9A52B2646D6F:868851C4E8CBB4BC:76BC1F306EEECBE2:1:1:0:1:0:0:0:1:0:1:0:1\n").is_none());
        assert!(parse_gi_flags("17F5075CE30B4AEA:A0AE9A52B2646D6F:868851C4E8CBB4BC:76BC1F306EEECBE2:1:1:0:2:0:0:0:1:0:1:0:1\n").is_none());
    }

    #[test]
    fn meta_probe_failure_classification() {
        // Real drbdmeta wordings (exit 20 with the device gone, and the
        // no-metadata refusal). Anything else, including the "configured!"
        // refusal on a device the kernel holds, must be Unknown, never
        // Destroyed.
        assert_eq!(
            classify_meta_probe_failure(
                "",
                "open(/dev/disk/by-id/md-uuid-5d3b948e) failed: No such file or directory\n"
            ),
            BackingMeta::Absent
        );
        assert_eq!(
            classify_meta_probe_failure(
                "",
                "Command 'drbdmeta 0 v09 /dev/x internal dump-md' terminated\nNo valid meta data found\n"
            ),
            BackingMeta::Destroyed
        );
        assert_eq!(
            classify_meta_probe_failure("", "Device '0' is configured!\n"),
            BackingMeta::Unknown
        );
        // An ENOENT not from the device open (drbdadm missing its config
        // file, say) stays Unknown and never passes as a missing device.
        assert_eq!(
            classify_meta_probe_failure("", "/etc/drbd.conf: No such file or directory\n"),
            BackingMeta::Unknown
        );
        assert_eq!(classify_meta_probe_failure("", ""), BackingMeta::Unknown);
        // drbd-utils 9.33 wordings: a detached loop device reads "is only 0
        // bytes. That's not enough.", a present but empty device whose data
        // may be elsewhere, so Unknown (manual path); dump-md's blank-device
        // failure is the Destroyed signal.
        assert_eq!(
            classify_meta_probe_failure(
                "",
                "/dev/loop7 is only 0 bytes. That's not enough.\n\
                 Command 'drbdmeta 9 v09 /dev/loop7 internal --node-id=0 get-gi' terminated with exit code 10\n"
            ),
            BackingMeta::Unknown
        );
        assert_eq!(
            classify_meta_probe_failure(
                "No valid meta data found\n",
                "Command 'drbdmeta 9 v09 /dev/loop7 internal dump-md' terminated with exit code 255\n"
            ),
            BackingMeta::Destroyed
        );
    }
}
