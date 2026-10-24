//! Crash boundary for the split-brain loser.
//!
//! When a true fork resolves (both copies advanced independently), the losing
//! node discards its diverged history and resyncs from the winner. Withdrawing
//! its target is not a boundary on its own: multipath moves the still-running
//! guests on this host onto the winner's disk history while their kernels keep
//! page cache and file-system state from the discarded one. That live history
//! switch leaves foreign bytes inside files without any error a file-system
//! check or a database integrity check would notice.
//!
//! The boundary closes that window. Before any admission change on the loser
//! (loopback logout, target/portal/LUN teardown, disconnect, discard), every
//! guest running on this host with a disk on the twinstor SR is hard-stopped
//! and confirmed gone locally. The authority is the local scan (`xl` plus the
//! block backends in xenstore), not XAPI: it also catches a domain XAPI does
//! not attribute to this host (an incoming migration, a stale resident-on).
//! XAPI supplies the names and the restart obligation, and is cross-checked
//! against the scan so an unrecognised local format can never read as "no
//! guests". Disabling the host is best-effort only: with pool HA armed and a
//! failures-to-tolerate of 1, XAPI refuses to disable either host of a
//! two-host pool, and that refusal must not wedge every resolution.
//!
//! The whole boundary runs in one call and in seconds: the guest domains are
//! destroyed locally and concurrently (`xl destroy`), with XAPI only as a
//! bounded, advisory cross-check. Speed matters: the split-brain lasts until
//! this node discards, and with pool HA armed the XHA statefile sits on the
//! replicated SR, so each host stops seeing the other's statefile updates for
//! that whole time. A slow boundary turns a short cut into an XHA fence. The
//! record is persisted before any destroy, so a restart resumes with the
//! obligation. Only an empty scan (read again right before the discard)
//! lets the discard proceed; the stopped guests are then restarted on the
//! winner from the authoritative copy, a clean crash and reboot instead of a
//! live rollback. XAPI records a local destroy leaves `running` are
//! reconciled before those restarts.
//!
//! Every step that cannot be completed or confirmed returns a [`Hold`]: the
//! caller keeps both copies intact (the same posture as an undecidable winner)
//! and alerts with the exact failed step. Nothing here relies on a timing
//! assumption.
//!
//! The record is the restart obligation: guests twinstor stopped are owed a
//! start even across a daemon restart. The record never authorizes a stop by
//! itself (each run re-scans live), so a stale record can only cause a
//! restart attempt of a guest twinstor stopped earlier, never a stop.

use std::path::{Path, PathBuf};

use tracing::{error, info, warn};

/// What the resolver knows about the divergence when it picks this node as
/// the loser. Decides whether the boundary is required.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkEvidence {
    /// The DRBD generation lineage shows a true fork: both copies advanced.
    Diverged,
    /// The lineage shows no divergence (this node is behind, or the
    /// generations are equal): nothing this node's guests saw is rolled back.
    NotDiverged,
    /// The local replica was already invalid (an interrupted resync toward
    /// this node): nobody could have been served a diverged history from it.
    LocalInvalid,
    /// This node's fence-peer yielded and its I/O stayed frozen: it served
    /// nobody during the partition. Its guests may still hold writes frozen
    /// in flight on this copy, which the discard path's resume would complete
    /// and acknowledge, so the boundary applies while the target is exported.
    Yielded,
    /// No lineage evidence (unreadable handshake): divergence cannot be
    /// excluded, so the boundary is required.
    Unknown,
}

/// Whether the loser must establish the crash boundary before discarding.
/// Required when divergence is proven or cannot be excluded, and for a
/// yielded loser (frozen in-flight writes would otherwise be completed and
/// acknowledged to its still-running guests, then discarded), unless this
/// node's target is positively not exported (a bring-up loser that never
/// served: no guest can be reading the losing history through it). An
/// unreadable export state counts as exported (fail closed).
pub fn boundary_required(evidence: ForkEvidence, target_exported: Option<bool>) -> bool {
    matches!(
        evidence,
        ForkEvidence::Diverged | ForkEvidence::Unknown | ForkEvidence::Yielded
    ) && target_exported != Some(false)
}

/// A guest twinstor may stop and owes a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestRef {
    pub uuid: String,
    pub name: String,
}

/// Progress of one boundary, persisted so a daemon restart resumes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Guests enumerated and recorded; stops may be in flight.
    Stopping,
    /// Every recorded guest is confirmed gone; the discard may proceed.
    Stopped,
    /// The discard was started: the owed restarts may run.
    Discarding,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Phase::Stopping => "stopping",
            Phase::Stopped => "stopped",
            Phase::Discarding => "discarding",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "stopping" => Some(Phase::Stopping),
            "stopped" => Some(Phase::Stopped),
            "discarding" => Some(Phase::Discarding),
            _ => None,
        }
    }
}

/// A guest owed a restart, with its per-guest attempt counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owed {
    pub guest: GuestRef,
    /// Stops issued so far in this boundary (bounded before holding).
    pub stop_tries: u32,
    /// Failed restart attempts so far (escalated, never given up).
    pub start_tries: u32,
}

impl Owed {
    fn new(guest: GuestRef) -> Self {
        Self {
            guest,
            stop_tries: 0,
            start_tries: 0,
        }
    }
}

/// The durable boundary record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Episode key (the fork's generation pair when known). Telemetry and
    /// mismatch detection only; it never authorizes a stop.
    pub key: String,
    pub phase: Phase,
    /// The loser verdict came from the DRBD lineage, so the winner's copy is
    /// authoritative: guests already stopped may be restarted on it even while
    /// this boundary holds. False when divergence could not be read.
    pub early_restart: bool,
    /// The split-brain episode is still open. Cleared only by a positive
    /// convergence (Connected, both copies UpToDate), never by the tmpfs
    /// marker vanishing, which a reboot also causes.
    pub open: bool,
    /// This node's XAPI agent was already restarted once for this record to
    /// clear stale `running` records (at most once per episode).
    pub agent_restarted: bool,
    /// The boundary disabled this host (it was enabled before), so the
    /// boundary may enable it again. Never set for a host already disabled by
    /// an operator or another path.
    pub host_disabled: bool,
    /// Guests stopped (or being stopped) by twinstor that are owed a start.
    pub owed: Vec<Owed>,
    /// SR VDIs on which XAPI's storage layer on this host may still hold a
    /// datapath of a destroyed domain (its release failed). No start of a
    /// guest using one happens here until a re-check proves it released; the
    /// record is kept until then.
    pub leaked: Vec<String>,
}

impl Record {
    fn render(&self) -> String {
        let mut s = format!(
            "key={}\nphase={}\nearly={}\nopen={}\nagent={}\ndisabled={}\n",
            self.key,
            self.phase.as_str(),
            u8::from(self.early_restart),
            u8::from(self.open),
            u8::from(self.agent_restarted),
            u8::from(self.host_disabled)
        );
        for o in &self.owed {
            s.push_str(&format!(
                "vm={} {} {} {}\n",
                o.guest.uuid, o.stop_tries, o.start_tries, o.guest.name
            ));
        }
        for v in &self.leaked {
            s.push_str(&format!("leaked={v}\n"));
        }
        s
    }

    /// Strict parse: any unknown or malformed line fails the whole record, so
    /// a torn or foreign file is never half-trusted. Also reads the older
    /// record format (`attempts=N`, `vm=<uuid> <name>`), whose guests come
    /// back with fresh counters and an open episode. `open`,
    /// `agent` and `disabled` default to open / not restarted / not disabled
    /// when absent.
    fn parse(text: &str) -> Option<Self> {
        let flag = |v: &str| match v {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        };
        let first_format = text.lines().any(|l| l.starts_with("attempts="));
        let (mut key, mut phase, mut early) = (None, None, first_format.then_some(false));
        let (mut open, mut agent, mut disabled) = (true, false, false);
        let mut owed = Vec::new();
        let mut leaked = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let (k, v) = line.split_once('=')?;
            match k {
                "leaked" => {
                    if !looks_like_uuid(v) {
                        return None;
                    }
                    leaked.push(v.to_ascii_lowercase());
                }
                "key" => key = Some(v.to_string()),
                "phase" => phase = Some(Phase::parse(v)?),
                "early" => early = Some(flag(v)?),
                "open" => open = flag(v)?,
                "agent" => agent = flag(v)?,
                "disabled" => disabled = flag(v)?,
                "attempts" if first_format => {
                    v.parse::<u32>().ok()?;
                }
                "vm" if first_format => {
                    let (uuid, name) = v.split_once(' ').unwrap_or((v, ""));
                    if !looks_like_uuid(uuid) {
                        return None;
                    }
                    owed.push(Owed::new(GuestRef {
                        uuid: uuid.to_string(),
                        name: name.to_string(),
                    }));
                }
                "vm" => {
                    let mut it = v.splitn(4, ' ');
                    let uuid = it.next()?;
                    let stop_tries = it.next()?.parse().ok()?;
                    let start_tries = it.next()?.parse().ok()?;
                    let name = it.next().unwrap_or("");
                    if !looks_like_uuid(uuid) {
                        return None;
                    }
                    owed.push(Owed {
                        guest: GuestRef {
                            uuid: uuid.to_string(),
                            name: name.to_string(),
                        },
                        stop_tries,
                        start_tries,
                    });
                }
                _ => return None,
            }
        }
        Some(Record {
            key: key?,
            phase: phase?,
            early_restart: early?,
            open,
            agent_restarted: agent,
            host_disabled: disabled,
            owed,
            leaked,
        })
    }

    fn guests(&self) -> Vec<GuestRef> {
        self.owed.iter().map(|o| o.guest.clone()).collect()
    }

    fn merge(&mut self, more: &[GuestRef]) {
        for g in more {
            if !self.owed.iter().any(|o| same_uuid(&o.guest.uuid, &g.uuid)) {
                self.owed.push(Owed::new(g.clone()));
            }
        }
    }
}

fn same_uuid(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Default record location; env-overridable like the other state files.
pub fn default_record_path() -> String {
    std::env::var("TWINSTOR_LOSER_BOUNDARY_FILE")
        .unwrap_or_else(|_| "/var/lib/twinstor/loser-boundary".to_string())
}

/// The durable store for the boundary record.
pub struct Store {
    path: PathBuf,
}

/// Serializes every write of a boundary record in this process: the daemon
/// tick and its off-tick restart step both write it, and a read-modify-write
/// ([`Store::update`]) must never interleave with another write.
static RECORD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn record_lock() -> std::sync::MutexGuard<'static, ()> {
    RECORD_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

impl Store {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// `Ok(None)` when absent; `Err` when present but unreadable or malformed
    /// (callers fail closed).
    pub fn load(&self) -> Result<Option<Record>, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => Record::parse(&text)
                .map(Some)
                .ok_or_else(|| format!("{} is malformed", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{} unreadable: {e}", self.path.display())),
        }
    }

    /// Write-temp, fsync, rename, fsync the directory: the record is either
    /// the old or the new one after a crash, never torn.
    pub fn save(&self, rec: &Record) -> Result<(), String> {
        let _g = record_lock();
        self.save_unlocked(rec)
    }

    fn save_unlocked(&self, rec: &Record) -> Result<(), String> {
        use std::io::Write;
        let dir = self.path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let tmp = self.path.with_extension("tmp");
        let mut f =
            std::fs::File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        f.write_all(rec.render().as_bytes())
            .and_then(|_| f.sync_all())
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| format!("rename {}: {e}", self.path.display()))?;
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn clear(&self) {
        let _g = record_lock();
        self.clear_unlocked();
    }

    fn clear_unlocked(&self) {
        let _ = std::fs::remove_file(&self.path);
        if let Some(Ok(d)) = self.path.parent().map(std::fs::File::open) {
            let _ = d.sync_all();
        }
    }

    /// Read-modify-write an existing record under the record lock, with no
    /// await inside, so a concurrent writer's change is never lost. `f`
    /// returns whether the record is now done (it is then cleared instead of
    /// saved). `Ok(None)` when there is no record (`f` does not run).
    pub fn update<T>(&self, f: impl FnOnce(&mut Record) -> (bool, T)) -> Result<Option<T>, String> {
        let _g = record_lock();
        let Some(mut rec) = self.load()? else {
            return Ok(None);
        };
        let (done, out) = f(&mut rec);
        if done {
            self.clear_unlocked();
        } else {
            self.save_unlocked(&rec)?;
        }
        Ok(Some(out))
    }

    /// Advance the phase of an existing record. Fails when absent.
    pub fn set_phase(&self, phase: Phase) -> Result<(), String> {
        self.update(|rec| {
            rec.phase = phase;
            (false, ())
        })?
        .ok_or_else(|| "boundary record vanished".to_string())
    }

    /// The split-brain episode converged (Connected, both copies UpToDate):
    /// owed restarts may now run on this host. No-op when absent or closed.
    pub fn close_episode(&self) -> Result<(), String> {
        self.update(|rec| {
            rec.open = false;
            (false, ())
        })
        .map(|_| ())
    }

    /// The verdict flipped: this node now keeps its data, so nothing may be
    /// started on the peer (which may be about to discard). Early restarts
    /// are revoked and the stopped guests wait for the episode's end, then
    /// start here. No-op when absent.
    pub fn revoke_for_flip(&self) -> Result<(), String> {
        self.update(|rec| {
            rec.phase = Phase::Stopped;
            rec.early_restart = false;
            (false, ())
        })
        .map(|_| ())
    }

    /// Record VDIs whose datapath release failed on this host, creating a
    /// record that owes no restart when none exists (the operator's
    /// discard-and-resync): the daemon keeps retrying the release, and no
    /// guest using one is started here by twinstor until it succeeds.
    pub fn note_leaked(&self, vdis: &[String]) -> Result<(), String> {
        if vdis.is_empty() {
            return Ok(());
        }
        let _g = record_lock();
        let mut rec = self.load()?.unwrap_or_else(|| Record {
            key: "leaked-datapaths".to_string(),
            phase: Phase::Stopped,
            early_restart: false,
            open: true,
            agent_restarted: false,
            host_disabled: false,
            owed: Vec::new(),
            leaked: Vec::new(),
        });
        for v in vdis {
            if !rec.leaked.iter().any(|x| same_uuid(x, v)) {
                rec.leaked.push(v.to_ascii_lowercase());
            }
        }
        self.save_unlocked(&rec)
    }

    /// Record that twinstor disabled this host (the operator's
    /// discard-and-resync), creating a record that owes no restart when none
    /// exists: the daemon enables the host again once the pair has converged
    /// and no stale datapath is left, the same rule as for a host the crash
    /// boundary disabled. Without it nothing re-enables the host, and the
    /// daemon's readiness gate keeps this node from serving while it stays
    /// disabled.
    pub fn note_host_disabled(&self) -> Result<(), String> {
        let _g = record_lock();
        let mut rec = self.load()?.unwrap_or_else(|| Record {
            key: "operator-discard".to_string(),
            phase: Phase::Stopped,
            early_restart: false,
            open: true,
            agent_restarted: false,
            host_disabled: false,
            owed: Vec::new(),
            leaked: Vec::new(),
        });
        rec.host_disabled = true;
        self.save_unlocked(&rec)
    }

    /// Allow the owed restarts to run during a hold (the verdict came from
    /// the lineage, so the winner is authoritative). No-op when absent.
    pub fn allow_early_restart(&self) -> Result<(), String> {
        self.update(|rec| {
            rec.early_restart = true;
            (false, ())
        })
        .map(|_| ())
    }
}

/// How a restart attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    Started,
    /// XAPI places it running on another host (HA or an operator got there
    /// first).
    AlreadyRunning,
    /// A live domain for it exists on this host: after the discard that is a
    /// fresh start here (HA, an operator); before it, a guest the boundary
    /// could not stop. `run_restarts` decides which.
    RunningHere,
    /// A domain for it exists here but is paused (a half-built rebuild): not
    /// running, not startable; retried.
    PausedHere,
    /// XAPI still records it live (`running` or `paused`), resident on this
    /// host, but no domain for it exists here: a stale record. Its domain was
    /// destroyed locally, so XAPI either never heard of the stop or began its
    /// crash action (a restart in place) and that rebuild failed once the
    /// loser's target went away, leaving it `paused`. Reconciled, never started
    /// over it.
    Stale,
    /// The start was refused because a disk of the guest is still held by
    /// this host (the local destroy skipped XAPI's disk teardown, so the
    /// activation lock and tapdisks stay here). This host can start it.
    VdiHeldHere(String),
    /// Not started; the reason is named in the escalation alert.
    Failed(String),
}

/// Whether a `vm-start` failure means a disk is still held where the guest
/// last ran: `SR_BACKEND_FAILURE_46` ("The VDI is not available") is what an
/// LVHD SR answers when another host holds the VDI's activation lock.
pub fn is_vdi_held_error(text: &str) -> bool {
    text.contains("SR_BACKEND_FAILURE_46") || text.contains("VDI_IN_USE")
}

/// XAPI power states that claim a domain exists (`VM.is_live`): a VM in either
/// cannot be started, and a record in either with no domain is stale.
pub fn xapi_state_is_live(power: &str) -> bool {
    matches!(power, "running" | "paused")
}

/// What XAPI's record of a guest the boundary stopped on `this_host` means for
/// its restart. Pure, so every row is tested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordView {
    /// A live domain for it exists on this host right now: never start it.
    LocalDomain,
    /// A paused domain for it exists on this host: never start it, never
    /// count it as running.
    LocalPaused,
    /// XAPI places it live on another host (HA or an operator got there).
    Elsewhere,
    /// XAPI records it live on this host but no domain exists: stale.
    StaleHere,
    /// Not live anywhere according to XAPI: may be started.
    Startable,
    /// Live, but XAPI did not say where.
    Unknown(String),
}

pub fn classify_record(
    local: LocalDomain,
    power: Option<&str>,
    resident: Option<&str>,
    this_host: &str,
) -> RecordView {
    match local {
        LocalDomain::Live => return RecordView::LocalDomain,
        LocalDomain::Paused => return RecordView::LocalPaused,
        LocalDomain::None => {}
    }
    if !power.is_some_and(xapi_state_is_live) {
        return RecordView::Startable;
    }
    match resident {
        // Only running elsewhere counts as back; paused elsewhere is retried.
        Some(h) if h != this_host && power == Some("running") => RecordView::Elsewhere,
        Some(h) if h != this_host => RecordView::Unknown(format!(
            "XAPI records it {} on another host",
            power.unwrap_or("?")
        )),
        Some(_) => RecordView::StaleHere,
        None => RecordView::Unknown("XAPI did not report where it runs".to_string()),
    }
}

/// Whether a domain for a guest exists on this host, and in what state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalDomain {
    None,
    /// Running or blocked (doing work).
    Live,
    /// Paused: built but not running (e.g. a crash-action rebuild that hung).
    Paused,
}

/// How long a stale-looking record is left to settle before it is treated as
/// stale: right after a local destroy, xenopsd may still be running the VM's
/// crash action (a rebuild in place), and its outcome decides the record.
pub const STALE_SETTLE: std::time::Duration = std::time::Duration::from_secs(10);

/// One way to clear a stale live (`running`/`paused`) record, tried in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleStep {
    /// `vm-shutdown --force` again now that XAPI answers: xenopsd finds no
    /// domain and the record turns halted.
    RetryHardShutdown,
    /// `vm-reset-powerstate --force`: XAPI accepts it only on the pool master
    /// for a VM resident there.
    ResetPowerState,
    /// Restart this host's XAPI agent (xapi only): on start it resyncs its
    /// records against xenopsd. At most once per episode.
    RestartAgent,
}

/// The reconciliation plan for a stale record on this host.
pub fn stale_reconcile_plan(
    this_is_master: Option<bool>,
    agent_restart_allowed: bool,
) -> Vec<StaleStep> {
    let mut plan = vec![StaleStep::RetryHardShutdown];
    if this_is_master == Some(true) {
        plan.push(StaleStep::ResetPowerState);
    } else if agent_restart_allowed {
        plan.push(StaleStep::RestartAgent);
    }
    plan
}

/// How a reconciliation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reconciled {
    /// The record is no longer `running` on this host.
    pub cleared: bool,
    /// The XAPI agent was restarted (spends the once-per-episode allowance).
    pub agent_restarted: bool,
    /// What was tried, for the log and the alert.
    pub detail: String,
}

/// Why [`GuestControl::poweroff_domains`] did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoweroffError {
    /// The binding cannot be used here: nothing was attempted.
    Unavailable(String),
    /// The call failed or timed out: some domains may be powered off.
    Unknown(String),
}

impl std::fmt::Display for PoweroffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PoweroffError::Unavailable(e) | PoweroffError::Unknown(e) => f.write_str(e),
        }
    }
}

/// This host's guest domains, read locally: every domain (domid, uuid) and
/// the subset with a block backend on the twinstor SR.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalScan {
    pub all: Vec<(u32, String)>,
    pub on_sr: Vec<(u32, String)>,
}

/// The side effects the boundary needs, behind a seam so the ordering is
/// tested without XAPI or Xen.
#[async_trait::async_trait]
pub trait GuestControl: Send + Sync {
    /// Whether this node's iSCSI target is exported in the kernel right now
    /// (`None` = unreadable).
    async fn local_target_exported(&self) -> Option<bool>;
    /// Guests XAPI lists as resident on this host with an attached disk on the
    /// twinstor SR (names and the restart obligation).
    async fn sr_guests(&self) -> Result<Vec<GuestRef>, String>;
    /// Disable this host if it is enabled, so nothing new is placed on it.
    /// Best-effort and bounded by the caller. `Ok(true)` when this call
    /// disabled it, `Ok(false)` when it was already disabled (left as it is).
    async fn disable_local_host(&self) -> Result<bool, String>;
    /// Enable this host again (only ever after the boundary disabled it).
    async fn enable_local_host(&self) -> Result<(), String>;
    /// Destroy these local domains (`(domid, uuid)`) with `xl destroy`, all
    /// concurrently and each bounded. Local only: never an XAPI call, which
    /// acts on the VM wherever XAPI places it and is not on the critical path.
    async fn destroy_domains(&self, doms: &[(u32, String)]);
    /// Power these local domains off at the hypervisor (`domain_shutdown` with
    /// reason poweroff, through the dom0 `xen.lowlevel.xc` binding), all in one
    /// bounded call. The vCPUs stop at once, as with a destroy, but the domain
    /// stays until xenopsd tears it down as a guest poweroff: its shutdown
    /// action (destroy), not its crash action (a rebuild in place), and its own
    /// disk teardown. [`PoweroffError::Unavailable`] when the binding cannot
    /// be used at all (nothing was attempted); [`PoweroffError::Unknown`] when
    /// the call failed or timed out after it may have stopped some domains. A
    /// domain the call did not stop is found by the re-scan.
    async fn poweroff_domains(&self, doms: &[(u32, String)]) -> Result<(), PoweroffError>;
    /// Every domid `xl list -v` lists on this host, in any state (shut down
    /// and dying included, dom0 included). Read locally.
    async fn present_domids(&self) -> Result<Vec<u32>, String>;
    /// Ask XAPI to hard-stop these guests, without waiting for it: fired right
    /// after the hypervisor poweroff so XAPI records it as an intended
    /// shutdown (HA does not restart the guest by itself; the owed restart
    /// places it). Never on the critical path; the local stop is the
    /// guaranteed one.
    fn request_xapi_hard_shutdown(&self, guests: &[GuestRef]);
    /// The local scan: the authority on what still runs here.
    async fn local_scan(&self) -> Result<LocalScan, String>;
    /// Tapdisks on this host that serve a VDI of the SR which no live domain
    /// (dom0 included) has attached through a block backend: left behind when
    /// a domain is destroyed outside xenopsd (they keep the VHD open with its
    /// metadata, BAT and sector bitmaps, cached from before the discard), or
    /// the tapdisk of a guest still being built, whose backend is not written
    /// yet (its datapath is live: see [`DpState::live_vdis`]). Read locally.
    async fn leftover_tapdisks(&self) -> Result<Vec<Tapdisk>, String>;
    /// Destroy one leftover tapdisk (close the image, detach, free the minor),
    /// bounded. The fallback only: it skips the storage layer's bookkeeping.
    async fn destroy_tapdisk(&self, t: &Tapdisk) -> Result<(), String>;
    /// [`Self::destroy_tapdisk`] for each, concurrently where the
    /// implementation can (the slowest close bounds the call, not the sum).
    /// One result per tapdisk, in order.
    async fn destroy_tapdisks(&self, taps: &[Tapdisk]) -> Vec<Result<(), String>> {
        let mut out = Vec::new();
        for t in taps {
            out.push(self.destroy_tapdisk(t).await);
        }
        out
    }
    /// What XAPI's storage layer on this host holds on SR VDIs, each datapath
    /// classified by the domain it belongs to (see [`DpState`]). `None`: every
    /// SR VDI the storage layer reports or a tapdisk serves; `Some`: only
    /// these VDIs. Read locally (the storage layer through the message switch,
    /// then `xl`: a datapath only exists once its domain does, so one created
    /// meanwhile reads as live). `Err` when any of it cannot be read.
    async fn datapaths(&self, vdis: Option<&[String]>) -> Result<DpState, String>;
    /// Release these datapaths through the storage layer (`DP.destroy`: SM
    /// deactivates and detaches the VDI as on a normal shutdown), concurrently
    /// across VDIs and each bounded. One result per datapath, in order.
    async fn destroy_datapaths(&self, dps: &[Datapath]) -> Vec<Result<(), String>>;
    /// Lift a DRBD I/O suspend on this node's copy (a fence-peer yield, or
    /// DRBD's own fencing wait). `Ok(true)` when one was in place and is now
    /// lifted, `Ok(false)` when there was none. Only called once no guest
    /// domain on the SR runs code on this host: the frozen writes it completes
    /// are acknowledged to no running guest, onto this node's own copy.
    async fn lift_io_suspend(&self) -> Result<bool, String>;
    /// Whether this node's copy has its I/O suspended right now (a fence-peer
    /// yield, or DRBD's own fencing wait); `None` when it cannot be read.
    async fn io_suspended(&self) -> Option<bool>;
    /// Freeze this node's copy's I/O again (`drbdadm suspend-io`): a boundary
    /// that lifted a yield's suspend and then holds puts it back, so the copy
    /// it is about to discard acknowledges no new write while it waits.
    async fn suspend_io(&self) -> Result<(), String>;
    /// Whether this host's multipath map for the SR's LUN has its active path
    /// on this node's own portal (`None`: no map, or it cannot be read). Every
    /// disk-handle close goes through that map, and a close writes the
    /// handle's cached VHD metadata: it must land on this node's copy.
    async fn local_path_active(&self) -> Option<bool>;
    /// Hard-stop these guests through XAPI, concurrently, each only when XAPI
    /// places it on this host (a stop by uuid acts wherever XAPI places the
    /// VM, and an instance on the winner must never be touched), and wait for
    /// the answers (each bounded): a stop XAPI records as intended, so pool HA
    /// does not restart the guest. One result per guest, in order.
    async fn hard_shutdown_here(&self, gs: &[GuestRef]) -> Vec<Result<(), String>>;
    /// The guest's VDIs on the SR, from XAPI.
    async fn guest_sr_vdis(&self, g: &GuestRef) -> Result<Vec<String>, String>;
    /// `(this host, peer host)` XAPI uuids, for restarts by host identity.
    async fn host_uuids(&self) -> Option<(String, String)>;
    /// Start a guest on `target_host` (an XAPI host uuid). `this_host` is
    /// where the boundary stopped it: a "running" record there is only
    /// trusted if a local domain exists.
    async fn start_guest(&self, g: &GuestRef, target_host: &str, this_host: &str) -> StartOutcome;
    /// Clear a stale `running` record of a guest the boundary stopped on
    /// `this_host` (see [`StartOutcome::Stale`]), per [`stale_reconcile_plan`].
    /// Never acts on a guest that has a local domain or that XAPI places on
    /// another host.
    async fn reconcile_stale(
        &self,
        g: &GuestRef,
        this_host: &str,
        agent_restart_allowed: bool,
    ) -> Reconciled;
}

/// The step a boundary failed at (named in the hold alert).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Record,
    LocalScan,
    ConfirmStopped,
    Tapdisks,
}

impl Step {
    pub fn as_str(self) -> &'static str {
        match self {
            Step::Record => "record the guest list durably",
            Step::LocalScan => {
                "read this host's guest domains and their disks locally (xl, xenstore)"
            }
            Step::ConfirmStopped => "stop the guests on this host that use the SR",
            Step::Tapdisks => {
                "release the disk handles (datapaths, tapdisks) this host still holds on the SR"
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundaryOutcome {
    /// Every guest that could read the losing history is gone; `stopped`
    /// lists the guests owed a restart (may be empty).
    Clear { stopped: Vec<GuestRef> },
    /// Do not discard. `stopped` lists guests already stopped (still owed).
    Hold {
        step: Step,
        detail: String,
        stopped: Vec<GuestRef>,
    },
}

/// Destroy rounds issued to a guest before the boundary holds instead of
/// retrying (each establishing round destroys, re-scans, and destroys the
/// survivors once more).
pub const MAX_STOP_TRIES: u32 = 2;

/// How long the advisory XAPI calls (the guest listing, the host disable) may
/// take before the boundary goes on without them. XAPI is not on the critical
/// path: the split-brain lasts until this node discards, and with pool HA
/// armed the XHA statefile on the replicated SR is split for that whole time.
pub const XAPI_ADVISORY_BOUND: std::time::Duration = std::time::Duration::from_secs(5);

/// How long XAPI's stop waits for a slow hypervisor poweroff call (see
/// [`stop_domains`]): the call itself is a python start and one hypercall.
const XAPI_STOP_AFTER: std::time::Duration = std::time::Duration::from_millis(500);

/// Bound on one `xl destroy`.
#[cfg_attr(test, allow(dead_code))]
const XL_DESTROY_BOUND_SECS: u64 = 15;

/// Bound on the hypervisor poweroff call (one process for every domain).
#[cfg_attr(test, allow(dead_code))]
const POWEROFF_BOUND_SECS: u64 = 5;

/// How long xenopsd is given to tear down the domains powered off at the
/// hypervisor (typically 2-4 s) before the survivors are destroyed directly.
/// Their vCPUs no longer run, so this is not a data window, only time on the
/// split: with pool HA armed the XHA statefile is split until the discard.
pub const XENOPSD_TEARDOWN_WAIT: std::time::Duration = std::time::Duration::from_secs(8);

/// Poll interval of that wait.
const TEARDOWN_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// How a set of guest domains was stopped (see [`stop_domains`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stopped {
    /// Domains still running guest code after every stop round.
    pub still_running: Vec<(u32, String)>,
    /// Domains destroyed directly (`xl destroy`): xenopsd reads such a
    /// destroy as a crash, and the crash action may rebuild the guest on this
    /// host. Empty when every domain was powered off and torn down by xenopsd.
    pub destroyed: Vec<(u32, String)>,
}

/// Stop guest domains on this host: a hypervisor poweroff of all of them at
/// once (they stop running at once and xenopsd tears each down as a guest
/// poweroff, without the in-place rebuild a crash gets), a re-scan, up to
/// [`MAX_STOP_TRIES`] concurrent `xl destroy` rounds for any domain still
/// running guest code, then a bounded wait ([`XENOPSD_TEARDOWN_WAIT`]) for
/// xenopsd to remove the powered-off domains. Only those still present after
/// that wait are destroyed directly: destroying a domain xenopsd has not yet
/// collected erases its poweroff reason and turns it back into a crash.
/// Without the hypervisor binding, every domain takes the `xl destroy` path.
///
/// `before_round` runs before the poweroff and before each destroy round,
/// with the domains about to be stopped (the caller persists its record
/// there); its `Err` aborts with [`Step::Record`]. A failed re-scan aborts
/// with [`Step::LocalScan`].
pub async fn stop_domains(
    ctl: &dyn GuestControl,
    targets: Vec<(u32, String)>,
    lift: Lift<'_>,
    after_poweroff: impl FnOnce(),
    mut before_round: impl FnMut(&[(u32, String)], bool) -> Result<(), String>,
) -> Result<Stopped, (Step, String)> {
    let mut out = Stopped::default();
    let mut targets = targets;
    if targets.is_empty() {
        return Ok(out);
    }
    before_round(&targets, false).map_err(|e| (Step::Record, e))?;
    let mut powered: Vec<(u32, String)> = Vec::new();
    // XAPI's own stop goes out once the poweroff returns, or after
    // `XAPI_STOP_AFTER` if the call is slow: it must reach XAPI before
    // xenopsd has collected a powered-off domain (about 3 s), or XAPI records
    // a guest-internal halt and, with pool HA armed, restarts the guest.
    let mut after_poweroff = Some(after_poweroff);
    let poweroff = {
        let call = ctl.poweroff_domains(&targets);
        tokio::pin!(call);
        tokio::select! {
            r = &mut call => r,
            _ = tokio::time::sleep(XAPI_STOP_AFTER) => {
                if let Some(f) = after_poweroff.take() {
                    f();
                }
                call.await
            }
        }
    };
    // Only now XAPI's own stop. On the pool master XAPI answers locally and
    // at once; asked first, its xenopsd pauses the domain mid-teardown, the
    // re-scan below reads it as still running, and the direct destroy then
    // pulls the devices from under xenopsd's unplug, which waits out its
    // 300 s xenstore timeout with every XAPI call for the guest queued behind
    // it. Powered off first, the domain is shut down and xenopsd tears it
    // down as a guest poweroff, with XAPI's stop queued harmlessly behind it,
    // as on a slave.
    if let Some(f) = after_poweroff.take() {
        f();
    }
    match poweroff {
        Ok(()) => {
            info!(heal_event = "loser_crash_boundary", step = "poweroff", domains = %domain_labels(&targets),
                  "guest domains using the SR powered off at the hypervisor; xenopsd tears them down \
                   as a guest shutdown (no rebuild in place)");
            powered = targets.clone();
            targets = ctl
                .local_scan()
                .await
                .map_err(|e| (Step::LocalScan, e))?
                .on_sr;
            if !targets.is_empty() {
                warn!(heal_event = "loser_crash_boundary", step = "poweroff", domains = %domain_labels(&targets),
                      "guest domains still running after the hypervisor poweroff; destroying them directly");
            }
        }
        Err(PoweroffError::Unavailable(e)) => {
            warn!(heal_event = "loser_crash_boundary", step = "poweroff", error = %e,
                  "the hypervisor poweroff is not available here; destroying the guest domains directly \
                   (xenopsd may read that as a crash and rebuild a guest on this host, which the checks \
                   before and after the discard catch)");
        }
        Err(PoweroffError::Unknown(e)) => {
            // The call may have powered some domains off before it failed or
            // timed out: those no longer run, and destroying them now would
            // erase their poweroff reason (a crash, i.e. a rebuild in place).
            // Only the ones still running are destroyed; the others get the
            // xenopsd window like a clean poweroff.
            let still = ctl
                .local_scan()
                .await
                .map_err(|e| (Step::LocalScan, e))?
                .on_sr;
            powered = targets
                .iter()
                .filter(|d| !still.contains(d))
                .cloned()
                .collect();
            warn!(heal_event = "loser_crash_boundary", step = "poweroff", error = %e,
                  stopped = %domain_labels(&powered), still_running = %domain_labels(&still),
                  "the hypervisor poweroff call did not complete; destroying directly only the guest \
                   domains still running");
            targets = still;
        }
    }
    for _round in 1..=MAX_STOP_TRIES {
        if targets.is_empty() {
            break;
        }
        before_round(&targets, true).map_err(|e| (Step::Record, e))?;
        ctl.destroy_domains(&targets).await;
        for d in &targets {
            if !out.destroyed.contains(d) {
                out.destroyed.push(d.clone());
            }
        }
        targets = ctl
            .local_scan()
            .await
            .map_err(|e| (Step::LocalScan, e))?
            .on_sr;
    }
    out.still_running = targets;
    if !out.still_running.is_empty() || powered.is_empty() {
        return Ok(out);
    }
    // No guest domain on the SR runs code here any more. On a yielded loser
    // the copy's I/O is still suspended: the stopped guests' frozen writes
    // then hold their disk handles, and xenopsd's own teardown with them,
    // until it is lifted (inside the loser's bracket, see [`LiftBracket`]).
    if let Some(b) = lift {
        b.lift_if_safe(ctl).await;
    }
    // The powered-off domains no longer run guest code. Give xenopsd the
    // window to collect them as a poweroff (its teardown also releases their
    // disk handles); destroy only what it left.
    let t0 = tokio::time::Instant::now();
    let left = loop {
        let present = ctl
            .present_domids()
            .await
            .map_err(|e| (Step::LocalScan, e))?;
        let left: Vec<(u32, String)> = powered
            .iter()
            .filter(|(d, _)| present.contains(d) && !out.destroyed.iter().any(|(x, _)| x == d))
            .cloned()
            .collect();
        if left.is_empty() || t0.elapsed() >= XENOPSD_TEARDOWN_WAIT {
            break left;
        }
        tokio::time::sleep(TEARDOWN_POLL).await;
    };
    if left.is_empty() {
        info!(
            heal_event = "loser_crash_boundary",
            step = "poweroff",
            waited_ms = t0.elapsed().as_millis() as u64,
            "xenopsd tore down every powered-off guest domain"
        );
    } else {
        warn!(heal_event = "loser_crash_boundary", step = "poweroff", domains = %domain_labels(&left),
              wait_secs = XENOPSD_TEARDOWN_WAIT.as_secs(),
              "xenopsd did not tear these powered-off domains down in time; destroying them directly \
               (xenopsd may read that as a crash and rebuild a guest on this host, which the checks \
               before and after the discard catch)");
        ctl.destroy_domains(&left).await;
        out.destroyed.extend(left);
    }
    Ok(out)
}

fn domain_labels(doms: &[(u32, String)]) -> String {
    doms.iter()
        .map(|(d, u)| format!("domid {d} ({u})"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Bound on each local read of the scan (`xl list -v`, `xenstore-ls`).
#[cfg_attr(test, allow(dead_code))]
const LOCAL_READ_BOUND_SECS: u64 = 5;

/// Bound on one `tap-ctl destroy` (its own close timeout is 10 s).
#[cfg_attr(test, allow(dead_code))]
const TAP_DESTROY_BOUND_SECS: u64 = 20;

/// The whole crash boundary in one call: read the advisory XAPI listing and
/// disable the host (both bounded, concurrently with the local scan), record
/// every guest with a disk on the SR durably, destroy all their domains
/// locally and concurrently, re-scan, destroy any survivor once more, and
/// return `Clear` only on an empty scan. Any failure is a hold; nothing in
/// here touches storage admission.
///
/// Why one round and local destroys: the split ends only when this node
/// discards, and with pool HA armed the statefile lives on the replicated SR,
/// so each host stops seeing the other's statefile updates for the whole
/// split. Stopping guests one per round through XAPI (tens of seconds each)
/// would let a short cut outlast the XHA statefile timeout and get a host
/// fenced.
/// XAPI records left `running` by a local destroy are reconciled after the
/// discard, before the owed restarts.
pub async fn establish(ctl: &dyn GuestControl, store: &Store, key: &str) -> BoundaryOutcome {
    establish_as(ctl, store, key, Side::Loser).await
}

/// Which side of the resolution runs the boundary: the loser before its
/// discard, or the winner of a resolution decided at bring-up (its guests
/// reach the SR through the peer's portal, i.e. the copy about to be
/// replaced). Only the operator-facing wording differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Loser,
    Winner,
}

impl Side {
    pub fn label(self) -> &'static str {
        match self {
            Side::Loser => "split-brain loser",
            Side::Winner => "split-brain winner (bring-up)",
        }
    }
}

/// A yielded loser's copy stays frozen until its discard: when it is found
/// running (a daemon killed inside a lift bracket, so `close` never ran),
/// suspend it again before anything else. Called before each boundary attempt
/// of a yielded loser.
pub async fn refreeze_yielded(ctl: &dyn GuestControl) {
    if ctl.io_suspended().await == Some(false) {
        match ctl.suspend_io().await {
            Ok(()) => warn!(
                heal_event = "loser_crash_boundary",
                step = "suspend_io",
                "this yielded copy's I/O was running (a lift that was not undone); \
                             suspended again before the boundary"
            ),
            Err(e) => error!(heal_event = "loser_crash_boundary", step = "suspend_io", error = %e,
                             "this yielded copy's I/O is running and could not be suspended again: \
                              it accepts writes that the discard will drop"),
        }
    }
}

/// [`establish`] for either side (see [`Side`]).
///
/// On the loser, a copy whose I/O is suspended (a fence-peer yield) may get
/// its suspend lifted inside, so the stopped guests' frozen writes drain and
/// their disk handles can close: a strict bracket (see [`LiftBracket`]),
/// opened and closed within this one call, so every outcome (the discard
/// path's own resume included) finds the copy as it was. The bring-up winner
/// never lifts: its copy is kept, and its stopped guests reached the SR
/// through the peer's portal, not this copy.
pub async fn establish_as(
    ctl: &dyn GuestControl,
    store: &Store,
    key: &str,
    side: Side,
) -> BoundaryOutcome {
    let bracket = LiftBracket::default();
    let lift = (side == Side::Loser).then_some(&bracket);
    let out = establish_inner(ctl, store, key, side, lift).await;
    bracket.close(ctl).await;
    out
}

/// Whether nothing on this host can acknowledge a write on this copy if its
/// suspend is lifted now: no guest domain on the SR runs here (local scan;
/// unreadable counts as "cannot tell", so no), and no domain is being built
/// with an SR disk, as far as XAPI's storage layer shows. An unreadable
/// storage layer does not refuse: the local scan is the evidence the
/// direct-close fallback already trusts, and refusing would leave the
/// stopped guests' handles frozen for good.
pub async fn quiet_to_lift(ctl: &dyn GuestControl) -> bool {
    match ctl.local_scan().await {
        Ok(scan) if scan.on_sr.is_empty() => {}
        _ => return false,
    }
    !matches!(ctl.datapaths(None).await, Ok(dps) if !dps.live_vdis.is_empty())
}

/// The operator's (`discard-and-resync`) version of [`quiet_to_lift`], with the
/// daemon stopped and the discard decided: lifting is refused only while a
/// guest domain runs on the SR here, the local scan cannot be read, or a guest
/// domain holds a live datapath on the SR (one being built: it could
/// acknowledge writes on the lifted copy). A dom0 or other non-guest consumer
/// (a backup or export the suspend froze) does not refuse: it cannot finish
/// while the copy stays frozen, and lifting only lets its pending I/O land on
/// the copy being discarded. `Ok` carries those consumers' VDIs, for a warning;
/// an unreadable storage layer reads as none.
pub async fn operator_lift_check(ctl: &dyn GuestControl) -> Result<Vec<String>, String> {
    match ctl.local_scan().await {
        Ok(scan) if scan.on_sr.is_empty() => {}
        Ok(_) => return Err("a VM on this host runs with a disk on the SR".into()),
        Err(e) => {
            return Err(format!(
                "the local scan of this host's domains cannot be read ({e})"
            ));
        }
    }
    match ctl.datapaths(None).await {
        Ok(dps) if !dps.guest_live_vdis.is_empty() => Err(format!(
            "a VM is being started on this host with a disk on the SR (VDI {})",
            dps.guest_live_vdis.join(", ")
        )),
        Ok(dps) => Ok(dps.live_vdis),
        Err(_) => Ok(Vec::new()),
    }
}

/// The loser's suspend bracket. A suspended copy blocks the close of the
/// stopped guests' disk handles on their frozen writes. The boundary lifts it
/// only while nothing can acknowledge a write on it (no guest domain on the
/// SR runs code here, no domain is being built with an SR disk, no other
/// consumer holds one, checked right before each lift), and [`Self::close`]
/// suspends it again before the boundary call returns, whatever it returns.
/// The discard's own resume (in the state machine) is the only resume that
/// outlives a boundary call.
#[derive(Default)]
pub struct LiftBracket {
    lifted: std::sync::atomic::AtomicBool,
}

impl LiftBracket {
    /// Lift a suspend now, if there is one and nothing can acknowledge a
    /// write on this copy (see the type). A failed check never lifts.
    pub async fn lift_if_safe(&self, ctl: &dyn GuestControl) {
        use std::sync::atomic::Ordering;
        if self.lifted.load(Ordering::SeqCst) || ctl.io_suspended().await != Some(true) {
            return;
        }
        if !quiet_to_lift(ctl).await {
            info!(
                heal_event = "loser_crash_boundary",
                step = "resume_io",
                "this copy's I/O stays suspended: a domain on this host runs or is being built \
                   with an SR disk (or the local scan cannot be read)"
            );
            return;
        }
        match ctl.lift_io_suspend().await {
            Ok(true) => {
                self.lifted.store(true, Ordering::SeqCst);
                info!(
                    heal_event = "loser_crash_boundary",
                    step = "resume_io",
                    "this copy's I/O was suspended (a fence-peer yield): lifted while no guest on \
                       the SR runs here, so the stopped guests' frozen writes drain onto this copy \
                       and their disk handles can close; it is suspended again before the boundary \
                       returns"
                );
            }
            Ok(false) => {}
            Err(e) => {
                // The command may have failed after DRBD already resumed (a
                // timeout, a late exit): whatever is not suspended now is
                // suspended again by `close`.
                if ctl.io_suspended().await != Some(true) {
                    self.lifted.store(true, Ordering::SeqCst);
                }
                warn!(heal_event = "loser_crash_boundary", step = "resume_io", error = %e,
                      "could not lift this copy's I/O suspend; disk handles holding frozen \
                       writes cannot close until it is");
            }
        }
    }

    /// Suspend the copy again if this bracket lifted it.
    pub async fn close(&self, ctl: &dyn GuestControl) {
        if !self.lifted.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        match ctl.suspend_io().await {
            Ok(()) => info!(
                heal_event = "loser_crash_boundary",
                step = "suspend_io",
                "this copy's I/O, lifted to release the stopped guests' disk handles, is \
                             suspended again"
            ),
            Err(e) => error!(heal_event = "loser_crash_boundary", step = "suspend_io", error = %e,
                             "this copy's I/O could not be suspended again: it accepts writes that \
                              the discard will drop"),
        }
    }
}

async fn establish_inner(
    ctl: &dyn GuestControl,
    store: &Store,
    key: &str,
    side: Side,
    loser_lift: Lift<'_>,
) -> BoundaryOutcome {
    let who = side.label();
    let hold = |step: Step, detail: String, stopped: Vec<GuestRef>| {
        warn!(heal_event = "loser_crash_boundary", step = step.as_str(), detail = %detail,
              "{who}: crash boundary NOT established; holding both copies intact");
        BoundaryOutcome::Hold {
            step,
            detail,
            stopped,
        }
    };
    let prior = match store.load() {
        Ok(p) => p,
        Err(e) => return hold(Step::Record, e, Vec::new()),
    };
    let mut rec = match prior {
        Some(p) if p.key == key => p,
        Some(p) => {
            info!(heal_event = "loser_crash_boundary", step = "record",
                  prior_key = %p.key, key = %key, owed = p.owed.len(),
                  "boundary record from another episode: keeping only its restart obligation");
            Record {
                key: key.to_string(),
                phase: Phase::Stopping,
                early_restart: false,
                open: true,
                agent_restarted: false,
                host_disabled: p.host_disabled,
                owed: p
                    .owed
                    .into_iter()
                    .map(|o| Owed { stop_tries: 0, ..o })
                    .collect(),
                leaked: p.leaked,
            }
        }
        None => Record {
            key: key.to_string(),
            phase: Phase::Stopping,
            early_restart: false,
            open: true,
            agent_restarted: false,
            host_disabled: false,
            owed: Vec::new(),
            leaked: Vec::new(),
        },
    };
    // The advisory XAPI calls run alongside the local scan and are cut off at
    // the bound. The listing supplies names, the restart obligation and a
    // cross-check of the scan; the disable is best-effort (with pool HA armed
    // and failures-to-tolerate 1, XAPI refuses to disable either host of a
    // two-host pool).
    let (listed, disabled, scan) = tokio::join!(
        tokio::time::timeout(XAPI_ADVISORY_BOUND, ctl.sr_guests()),
        tokio::time::timeout(XAPI_ADVISORY_BOUND, ctl.disable_local_host()),
        ctl.local_scan(),
    );
    let listed = match listed {
        Ok(Ok(g)) => Some(g),
        Ok(Err(e)) => {
            info!(heal_event = "loser_crash_boundary", step = "enumerate_unavailable", detail = %e,
                  "{who}: XAPI cannot list this host's guests; the local scan decides alone");
            None
        }
        Err(_) => {
            info!(
                heal_event = "loser_crash_boundary",
                step = "enumerate_unavailable",
                bound_secs = XAPI_ADVISORY_BOUND.as_secs(),
                "{who}: XAPI did not list this host's guests in time; the local scan \
                   decides alone"
            );
            None
        }
    };
    match disabled {
        Ok(Ok(true)) => rec.host_disabled = true,
        Ok(Ok(false)) => info!(
            heal_event = "loser_crash_boundary",
            step = "host_disable_skipped",
            "{who}: this host was already disabled; leaving it as it is"
        ),
        Ok(Err(e)) => {
            info!(heal_event = "loser_crash_boundary", step = "host_disable_skipped", detail = %e,
                            "{who}: could not disable this host (expected with pool HA \
                             armed); relying on the local scan")
        }
        // Cut off at the bound, the call can still land in XAPI (a losing
        // pool master's XAPI may answer late). It is recorded as done, so the
        // host is enabled again with the rest once the pair has converged (a
        // no-op if it never landed). Unrecorded, a disable that landed would
        // leave the host in maintenance, reported "critical" with nothing
        // placed on it, until the generic watchdog acts after 5 minutes.
        Err(_) => {
            rec.host_disabled = true;
            info!(
                heal_event = "loser_crash_boundary",
                step = "host_disable_unconfirmed",
                "{who}: the host disable did not answer in time; relying on the local scan \
                   (it may still land: enabled again once the pair has converged)"
            )
        }
    }
    let scan = match scan {
        Ok(s) => s,
        Err(e) => {
            rec.merge(listed.as_deref().unwrap_or_default());
            let _ = store.save(&rec);
            return hold(Step::LocalScan, e, rec.guests());
        }
    };
    // Cross-check: a guest XAPI lists with a disk on the SR that has a domain
    // here must also be seen on the SR by the local scan; otherwise the scan
    // does not understand this host and an empty answer could not be trusted.
    // (A listed guest with no domain here at all is already stopped: at most a
    // stale record, reconciled before its restart.)
    if let Some(g) = listed.iter().flatten().find(|g| {
        scan.all.iter().any(|(_, u)| same_uuid(u, &g.uuid))
            && !scan.on_sr.iter().any(|(_, u)| same_uuid(u, &g.uuid))
    }) {
        rec.merge(listed.as_deref().unwrap_or_default());
        let _ = store.save(&rec);
        return hold(
            Step::LocalScan,
            format!(
                "the local disk scan does not see {}'s disk on the SR; refusing to trust it",
                guest_label(g)
            ),
            rec.guests(),
        );
    }
    // Every guest XAPI lists here is owed a restart (a listed guest may
    // already be down), and so is every SR-backed domain the scan finds,
    // listed or not.
    rec.merge(listed.as_deref().unwrap_or_default());
    owe_unlisted(&mut rec, &scan.on_sr, who);
    rec.phase = Phase::Stopping;
    rec.open = true;
    let targets = scan.on_sr;
    // Guests XAPI itself places here and that have a domain here: ask XAPI to
    // hard-stop them in the background, right after the hypervisor poweroff
    // (see `stop_domains`). When XAPI answers, the stop is
    // recorded as intended (pool HA does not restart the guest on its own; the
    // owed restart places it) and xenopsd processes a shutdown whichever way
    // the domain went. The local poweroff below does not depend on it: it is
    // what keeps xenopsd from reading the stop as a crash and rebuilding the
    // guest in place when XAPI is slow. Only guests both views agree on, so
    // the call can never reach an instance running anywhere else.
    let agreed: Vec<GuestRef> = listed
        .as_ref()
        .map(|listed| {
            listed
                .iter()
                .filter(|g| targets.iter().any(|(_, u)| same_uuid(u, &g.uuid)))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    // Persist the list (and, per destroy round, the attempt) before stopping:
    // a daemon killed mid-stop resumes with the obligation and the count. A
    // second round runs only when the sweep finds a guest being started here
    // right now (its disk handle is never closed): it is stopped like the
    // others, then the sweep runs again.
    let mut targets = targets;
    for round in 0..2 {
        let stopped = {
            let rec = &mut rec;
            let ask_xapi = || {
                if round == 0 && !agreed.is_empty() {
                    ctl.request_xapi_hard_shutdown(&agreed);
                }
            };
            stop_domains(ctl, targets, loser_lift, ask_xapi, |doms, destroy| {
                if destroy {
                    for o in rec
                        .owed
                        .iter_mut()
                        .filter(|o| doms.iter().any(|(_, u)| same_uuid(u, &o.guest.uuid)))
                    {
                        o.stop_tries = o.stop_tries.saturating_add(1);
                    }
                }
                store.save(rec)?;
                let names: Vec<String> = doms
                    .iter()
                    .map(|(d, u)| {
                        rec.owed
                            .iter()
                            .find(|o| same_uuid(&o.guest.uuid, u))
                            .map_or(format!("domid {d}"), |o| guest_label(&o.guest))
                    })
                    .collect();
                info!(heal_event = "loser_crash_boundary", step = "stop", vms = %names.join(", "),
                      how = if destroy { "xl destroy" } else { "hypervisor poweroff" },
                      "{who}: stopping the guest domains that use the SR, all at once, before any \
                       withdrawal");
                Ok(())
            })
            .await
        };
        let still = match stopped {
            Ok(s) => s.still_running,
            Err((step, e)) => {
                if step != Step::Record {
                    let _ = store.save(&rec);
                }
                return hold(step, e, rec.guests());
            }
        };
        if !still.is_empty() {
            let names: Vec<String> = still
                .iter()
                .map(|(d, u)| {
                    rec.owed
                        .iter()
                        .find(|o| same_uuid(&o.guest.uuid, u))
                        .map_or(format!("domid {d}"), |o| guest_label(&o.guest))
                })
                .collect();
            let _ = store.save(&rec);
            return hold(
                Step::ConfirmStopped,
                format!(
                    "still running after {MAX_STOP_TRIES} destroy rounds: {}",
                    names.join(", ")
                ),
                rec.guests(),
            );
        }
        // The stopped domains' disk handles. When xenopsd's own teardown did
        // not run (or failed) with the domain already gone, XAPI's storage
        // layer keeps their datapaths "activated" and their tapdisks keep the
        // VHDs open with the metadata cached from the losing history. Release
        // them now, before the discard: a close writes the cached footer and
        // block map back, and that must land on the doomed copy. Left in
        // place, the next start of such a disk here would reuse the stale
        // attachment (a stale block map, or a backend node naming a blktap
        // minor another tapdisk now owns).
        match sweep_disk_handles(ctl, &rec.leaked, side, loser_lift).await {
            Ok(sweep) => {
                rec.leaked = sweep.leaked;
                break;
            }
            Err(f) if f.starting && round == 0 => {
                warn!(heal_event = "loser_crash_boundary", step = "tapdisks", detail = %f.detail,
                      "{who}: a guest is being started on this host during the boundary; stopping it \
                       too before releasing the disk handles");
                // It must not run on a lifted copy: freeze it again; the next
                // lift re-checks.
                if let Some(b) = loser_lift {
                    b.close(ctl).await;
                }
                tokio::time::sleep(STARTING_SETTLE).await;
                let scan = match ctl.local_scan().await {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = store.save(&rec);
                        return hold(Step::LocalScan, e, rec.guests());
                    }
                };
                owe_unlisted(&mut rec, &scan.on_sr, who);
                targets = scan.on_sr;
            }
            Err(f) => {
                let _ = store.save(&rec);
                return hold(Step::Tapdisks, f.detail, rec.guests());
            }
        }
    }
    rec.phase = Phase::Stopped;
    if let Err(e) = store.save(&rec) {
        return hold(Step::Record, e, rec.guests());
    }
    info!(
        heal_event = "loser_crash_boundary",
        step = "stopped",
        stopped = rec.owed.len(),
        "{who}: crash boundary established; the discard may proceed"
    );
    BoundaryOutcome::Clear {
        stopped: rec.guests(),
    }
}

/// How long the boundary lets a guest caught mid-start finish building (its
/// block backends written, one per disk after each SM attach) before
/// scanning for it again.
const STARTING_SETTLE: std::time::Duration = std::time::Duration::from_secs(2);

/// How long a close waits for this host's own storage path to come back
/// before refusing (see `require_own_path`).
const OWN_PATH_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// With the storage layer unreadable, how long a leftover tapdisk must stay
/// unattached before it is closed (see `sweep_inner`).
const UNKNOWN_DP_SETTLE: std::time::Duration = std::time::Duration::from_secs(2);

/// Every SR-backed domain the scan finds is owed a restart, listed by XAPI on
/// this host or not.
fn owe_unlisted(rec: &mut Record, on_sr: &[(u32, String)], who: &str) {
    for (domid, uuid) in on_sr {
        if !rec.owed.iter().any(|o| same_uuid(&o.guest.uuid, uuid)) {
            warn!(heal_event = "loser_crash_boundary", step = "unlisted_domain", domid = *domid, uuid = %uuid,
                  "{who}: a guest domain using the SR runs here although XAPI does not list it on this \
                   host; stopping it too");
            rec.owed.push(Owed::new(GuestRef {
                uuid: uuid.clone(),
                name: format!("domid {domid} (not listed on this host by XAPI)"),
            }));
        }
    }
}

/// The last check before the first admission change: the local scan must
/// still find no guest domain with a disk on the SR. `Err` names what runs.
pub async fn confirm_clear(ctl: &dyn GuestControl) -> Result<(), String> {
    let scan = ctl.local_scan().await?;
    if scan.on_sr.is_empty() {
        let left = ctl.leftover_tapdisks().await?;
        if left.is_empty() {
            return Ok(());
        }
        Err(format!(
            "tapdisks on the SR reappeared on this host: {}",
            tapdisk_labels(&left)
        ))
    } else {
        let ids: Vec<String> = scan
            .on_sr
            .iter()
            .map(|(d, u)| format!("domid {d} ({u})"))
            .collect();
        Err(format!(
            "guest domains using the SR appeared on this host: {}",
            ids.join(", ")
        ))
    }
}

/// A guest domain found running on the SR on this host right after the
/// discard that already existed when this copy was withdrawn: xenopsd (or
/// pool HA, or an operator) rebuilt it after the last check before the
/// discard. It may have run across the discard, i.e. through a disk handle
/// opened on the losing history; its VHDs are suspect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rebuilt {
    pub guest: GuestRef,
    pub domid: u32,
    /// Its VDIs on the SR (empty when XAPI could not say).
    pub vdis: Vec<String>,
    /// XAPI did not take the stop, so it was stopped at the hypervisor: pool
    /// HA may restart it on its own.
    pub ha_may_restart: bool,
}

/// The check right after the discard. A guest domain using the SR on this
/// host whose domid is at most `watermark` (the highest domid present when
/// this copy was withdrawn: target down, Secondary) existed before the
/// withdrawal and may have run across the discard: it is stopped at once,
/// through XAPI first (a stop XAPI records as intended is not restarted by
/// pool HA: its disks are suspect), at the hypervisor otherwise, removed from
/// the owed restarts (never restarted by twinstor) and returned for the
/// alert. A domain created after the withdrawal reaches the SR only through
/// the winner's portal and is left alone. `None` = unknown watermark: every
/// such domain is suspect. Its disk handles are left to xenopsd's teardown:
/// whatever they hold was already in use by the running guest. `Err` when
/// this host cannot be read or a domain does not stop.
pub async fn catch_rebuilt(
    ctl: &dyn GuestControl,
    store: &Store,
    watermark: Option<u32>,
) -> Result<Vec<Rebuilt>, String> {
    let scan = ctl.local_scan().await?;
    let (mut suspect, mut later): (Vec<(u32, String)>, Vec<(u32, String)>) = scan
        .on_sr
        .iter()
        .cloned()
        .partition(|(d, _)| watermark.is_none_or(|w| *d <= w));
    // A domain started after the withdrawal is safe only through a fresh
    // attachment: one using a VDI whose datapath on this host could not be
    // released reattached through the stale one, whatever its domid.
    // An unreadable record cannot say which datapaths leaked: every later
    // domain then counts as possibly reattached (fail closed).
    let loaded = store.load();
    if let Err(e) = &loaded {
        warn!(heal_event = "loser_crash_boundary", step = "rebuilt_after_discard", error = %e,
              "boundary record unreadable: every guest domain on the SR here counts as suspect");
    }
    let unreadable = loaded.is_err();
    let leaked = loaded.ok().flatten().map(|r| r.leaked).unwrap_or_default();
    if (unreadable || !leaked.is_empty()) && !later.is_empty() {
        let mut keep = Vec::new();
        for (d, u) in later {
            let g = GuestRef {
                uuid: u.clone(),
                name: format!("domid {d}"),
            };
            let vdis = ctl.guest_sr_vdis(&g).await.unwrap_or_default();
            if unreadable
                || vdis.is_empty()
                || vdis.iter().any(|v| leaked.iter().any(|l| same_uuid(l, v)))
            {
                suspect.push((d, u));
            } else {
                keep.push((d, u));
            }
        }
        later = keep;
    }
    if !later.is_empty() {
        info!(heal_event = "loser_crash_boundary", step = "started_after_withdrawal", domains = %domain_labels(&later),
              "guest domains using the SR started on this host after this copy was withdrawn; they reach \
               the SR through the winner's portal only, left running");
    }
    if suspect.is_empty() {
        return Ok(Vec::new());
    }
    let rec = store.load().ok().flatten();
    let guest_of = |domid: u32, uuid: &str| -> GuestRef {
        rec.as_ref()
            .and_then(|r| r.owed.iter().find(|o| same_uuid(&o.guest.uuid, uuid)))
            .map_or_else(
                || GuestRef {
                    uuid: uuid.to_string(),
                    name: format!("domid {domid}"),
                },
                |o| o.guest.clone(),
            )
    };
    let found: Vec<(u32, GuestRef)> = suspect.iter().map(|(d, u)| (*d, guest_of(*d, u))).collect();
    error!(heal_event = "loser_crash_boundary", step = "rebuilt_after_discard", domains = %domain_labels(&suspect),
           "split-brain loser: guest domains using the SR run on this host right after the discard, \
            started before this copy was withdrawn, or after it on a disk whose datapath here could not \
            be released (or whose disks could not be read); stopping them now, their disks are suspect");
    // Through XAPI first (a stop it records as intended keeps pool HA from
    // restarting a guest whose disks are suspect), all at once and under one
    // short bound: each of these guests may be writing stale VHD metadata
    // into the kept copy, so whatever XAPI has not stopped by then is
    // stopped at the hypervisor.
    let mut via_xapi: Vec<String> = Vec::new();
    let guests: Vec<GuestRef> = found.iter().map(|(_, g)| g.clone()).collect();
    match tokio::time::timeout(CATCH_XAPI_BOUND, ctl.hard_shutdown_here(&guests)).await {
        Ok(results) => {
            for (g, r) in guests.iter().zip(results) {
                match r {
                    Ok(()) => via_xapi.push(g.uuid.clone()),
                    Err(e) => {
                        warn!(heal_event = "loser_crash_boundary", step = "rebuilt_after_discard", vm = %guest_label(g),
                                    error = %e, "XAPI did not stop the suspect guest; stopping it at the hypervisor (pool \
                                    HA may then restart it)")
                    }
                }
            }
        }
        Err(_) => warn!(
            heal_event = "loser_crash_boundary",
            step = "rebuilt_after_discard",
            bound_secs = CATCH_XAPI_BOUND.as_secs(),
            "XAPI did not stop the suspect guests in time; stopping them at the hypervisor (pool HA \
                         may then restart them)"
        ),
    }
    let left: Vec<(u32, String)> = ctl
        .local_scan()
        .await?
        .on_sr
        .into_iter()
        .filter(|d| suspect.contains(d))
        .collect();
    let stopped = stop_domains(ctl, left, None, || {}, |_, _| Ok(()))
        .await
        .map_err(|(_, e)| e)?;
    if !stopped.still_running.is_empty() {
        return Err(format!(
            "still running after the discard: {}",
            domain_labels(&stopped.still_running)
        ));
    }
    let mut out = Vec::new();
    for (domid, guest) in found {
        let vdis = ctl.guest_sr_vdis(&guest).await.unwrap_or_default();
        let ha_may_restart = !via_xapi.iter().any(|u| same_uuid(u, &guest.uuid));
        out.push(Rebuilt {
            guest,
            domid,
            vdis,
            ha_may_restart,
        });
    }
    if rec.is_some()
        && let Err(e) = store.update(|r| {
            r.owed
                .retain(|o| !out.iter().any(|x| same_uuid(&x.guest.uuid, &o.guest.uuid)));
            (false, ())
        })
    {
        warn!(heal_event = "loser_crash_boundary", error = %e,
              "could not drop the suspect guests from the owed restarts");
    }
    Ok(out)
}

/// How long [`catch_rebuilt`] waits for XAPI to stop the suspect guests before
/// stopping the rest at the hypervisor.
const CATCH_XAPI_BOUND: std::time::Duration = std::time::Duration::from_secs(10);

/// The alert for [`catch_rebuilt`]: which guests, and how to check their
/// disks before starting them by hand.
pub fn rebuilt_alert_body(rebuilt: &[Rebuilt], sr: Option<&str>) -> String {
    let mut s = String::from(
        "Split-brain loser: right after this host discarded its diverged copy, guest domains using \
         the SR were running here that may hold a disk handle opened on the discarded history: \
         started before the copy was withdrawn (the toolstack, pool HA or an operator rebuilt them \
         after the crash boundary stopped them), or started after it on a disk whose datapath on \
         this host could not be released, so the start may have reused the stale attachment (a \
         guest whose disks could not be read is counted here too). They were stopped again and \
         twinstor does not restart them: each is marked `other-config:twinstor-disks-suspect=true` \
         (removed once it is started again).\n\n",
    );
    for r in rebuilt {
        s.push_str(&format!(
            "- {} ({}, domid {})",
            r.guest.name, r.guest.uuid, r.domid
        ));
        if r.vdis.is_empty() {
            s.push_str(": its VDIs on the SR could not be read; list them with `xe vbd-list vm-uuid=<vm> params=vdi-uuid`");
        } else {
            s.push_str(&format!(": VDIs {}", r.vdis.join(", ")));
        }
        if r.ha_may_restart {
            s.push_str(&format!(
                ". XAPI did not take the stop (it was stopped at the hypervisor), so pool HA may restart \
                 it on its own (not when the pool's ha-reboot-vm-on-internal-shutdown is off, as \
                 fate-shared freeze mode sets it): if it is HA-protected, remove its restart priority until its disks are \
                 checked (`xe vm-param-set uuid={} ha-restart-priority=`)",
                r.guest.uuid
            ));
        }
        s.push('\n');
    }
    let vg = sr.map_or("VG_XenStorage-<sr-uuid>".to_string(), |s| {
        format!("VG_XenStorage-{s}")
    });
    s.push_str(&format!(
        "\nBefore starting each one, once the pair has converged and with the guest halted, check \
         every VDI on this host. First make sure no datapath of a dead domain still holds it: `{}` \
         must not list the VDI under a `vbd/<domid>/` of a domain `xl list` no longer shows \
         (release such a datapath with `{}`). Then `lvchange -ay /dev/{vg}/VHD-<vdi>; vhd-util check \
         -n /dev/{vg}/VHD-<vdi>; lvchange -an /dev/{vg}/VHD-<vdi>`. A VDI that is not valid \
         (\"clobbers\" or a bad block map) must be restored from a backup; then check the guest's \
         file systems.\n",
        crate::xapi_storage::diagnostics_command(),
        crate::xapi_storage::destroy_command("vbd/<domid>/<device>")
    ));
    s
}

/// A datapath XAPI's storage layer holds on one VDI of the SR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Datapath {
    /// `vbd/<domid>/<device>`.
    pub dp: String,
    pub vdi: String,
}

/// What XAPI's storage layer on this host holds on SR VDIs, by the state of
/// the domain each datapath belongs to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DpState {
    /// Datapaths of domains that no longer exist: stale, released here.
    pub stale: Vec<Datapath>,
    /// Datapaths of domains still listed but running no guest code (shut
    /// down, crashed, dying): their own teardown may still be under way, so
    /// they are left to it, and kept as leaked until confirmed gone.
    pub stopping: Vec<Datapath>,
    /// VDIs held by a datapath of a domain that runs or is being built, or by
    /// any consumer whose datapath is not a guest disk's (`vbd/<domid>/...`):
    /// a tapdisk serving one of them is never closed here.
    pub live_vdis: Vec<String>,
    /// The subset of `live_vdis` held by a guest domain's datapath
    /// (`vbd/<domid>/...`, domid not 0, the domain running or being built):
    /// unlike a dom0 or other non-guest consumer, such a guest could
    /// acknowledge new writes on a lifted copy.
    pub guest_live_vdis: Vec<String>,
}

impl DpState {
    /// Fold a narrower read into this one: entries for `more`'s VDIs are
    /// replaced by `more`'s.
    fn merge(&mut self, more: DpState, vdis: &[String]) {
        let mine = |v: &str| vdis.iter().any(|x| same_uuid(x, v));
        self.stale.retain(|d| !mine(&d.vdi));
        self.stopping.retain(|d| !mine(&d.vdi));
        self.live_vdis.retain(|v| !mine(v));
        self.guest_live_vdis.retain(|v| !mine(v));
        for d in more.stale {
            if !self.stale.contains(&d) {
                self.stale.push(d);
            }
        }
        for d in more.stopping {
            if !self.stopping.contains(&d) {
                self.stopping.push(d);
            }
        }
        for v in more.guest_live_vdis {
            push_vdi(&mut self.guest_live_vdis, &v);
        }
        for v in more.live_vdis {
            push_vdi(&mut self.live_vdis, &v);
        }
    }
}

/// Classify a VDI's datapaths: `running` lists the domains that run (or are
/// being built), `present` every domain `xl` lists in any state.
pub fn classify_datapaths(
    vdi: &str,
    dps: &[(String, String)],
    running: &[u32],
    present: &[u32],
    out: &mut DpState,
) {
    for (dp, _) in dps {
        match crate::xapi_storage::dp_domid(dp) {
            Some(d) if d != 0 && !present.contains(&d) => out.stale.push(Datapath {
                dp: dp.clone(),
                vdi: vdi.to_string(),
            }),
            Some(d) if d != 0 && !running.contains(&d) => out.stopping.push(Datapath {
                dp: dp.clone(),
                vdi: vdi.to_string(),
            }),
            Some(d) if d != 0 => {
                push_vdi(&mut out.guest_live_vdis, vdi);
                push_vdi(&mut out.live_vdis, vdi);
            }
            _ => push_vdi(&mut out.live_vdis, vdi),
        }
    }
}

/// What a pre-discard sweep of this host's disk handles on the SR did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sweep {
    /// Datapaths of destroyed domains released through the storage layer.
    pub released: usize,
    /// Tapdisks closed directly because the storage layer did not release
    /// them.
    pub tapdisks_closed: usize,
    /// VDIs whose datapath could not be released (or not confirmed released):
    /// no guest using one is started here until a re-check proves it gone.
    pub leaked: Vec<String>,
}

/// Overall bound on [`sweep_disk_handles`]. It runs before the discard, on the
/// split's critical path (with pool HA armed the XHA statefile on the SR stays
/// split until the discard), so its per-call bounds must not add up past the
/// statefile timeout.
pub const SWEEP_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

/// How long the sweep lets a teardown already under way (a datapath whose
/// domain is still listed, shut down or dying) finish before going on.
const TEARDOWN_SETTLE: std::time::Duration = std::time::Duration::from_secs(5);

/// Why a sweep did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepFail {
    pub detail: String,
    /// A guest is being started on this host with a disk on the SR (its
    /// datapath is live, its block backend not written yet): its disk handle
    /// is never closed. Stop that guest, then sweep again.
    pub starting: bool,
}

impl SweepFail {
    fn other(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            starting: false,
        }
    }
}

impl std::fmt::Display for SweepFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

/// Release every disk handle destroyed guests left on the SR, before the
/// discard: first through XAPI's storage layer (`DP.destroy` does the whole
/// teardown, tapdisk close included, and keeps its bookkeeping straight);
/// then, only for a tapdisk still open, `tap-ctl destroy`, followed by one
/// more release of its datapath now that nothing is left to close.
/// `known_leaked` adds VDIs an earlier sweep could not release.
///
/// Precondition: no guest domain on the SR runs code on this host (the caller
/// re-scanned). A close writes the handle's cached VHD metadata, so:
/// - this node's copy must still be reachable: a suspend is lifted first (a
///   yielded loser), and nothing is closed unless this host's active storage
///   path is its own portal (never onto the peer's copy);
/// - a tapdisk serving a disk a live domain holds through the storage layer
///   (a guest being built here right now) is never closed: `starting`.
///
/// Bounded as a whole by [`SWEEP_DEADLINE`]. `Err` when a tapdisk on the SR is
/// still open (or the tapdisks cannot be read): the discard must not proceed.
/// A datapath that cannot be released (or whose domain is still being torn
/// down) is not a data risk once its tapdisk is closed; it is reported in
/// `leaked` and re-checked until it is gone.
pub async fn sweep_disk_handles(
    ctl: &dyn GuestControl,
    known_leaked: &[String],
    side: Side,
    lift: Lift<'_>,
) -> Result<Sweep, SweepFail> {
    match tokio::time::timeout(SWEEP_DEADLINE, sweep_inner(ctl, known_leaked, side, lift)).await {
        Ok(r) => r,
        Err(_) => Err(SweepFail::other(format!(
            "releasing the disk handles on the SR did not finish within {} s (the storage layer or \
             a tapdisk close is blocked)",
            SWEEP_DEADLINE.as_secs()
        ))),
    }
}

async fn sweep_inner(
    ctl: &dyn GuestControl,
    known_leaked: &[String],
    side: Side,
    lift: Lift<'_>,
) -> Result<Sweep, SweepFail> {
    let mut sweep = Sweep::default();
    let mut candidates: Vec<String> = known_leaked.to_vec();
    // Tapdisks first, then the datapaths: a guest being built has its datapath
    // recorded before its tapdisk exists, so a tapdisk read here is matched to
    // its live datapath below. An unreadable storage layer is not retried
    // within this sweep.
    let read = || async {
        let left = ctl.leftover_tapdisks().await.map_err(SweepFail::other)?;
        let state = match ctl.datapaths(None).await {
            Ok(mut st) => {
                if !known_leaked.is_empty()
                    && let Ok(more) = ctl.datapaths(Some(known_leaked)).await
                {
                    st.merge(more, known_leaked);
                }
                Some(st)
            }
            Err(e) => {
                warn!(heal_event = "loser_crash_boundary", step = "datapaths", error = %e,
                      "cannot read XAPI's storage datapaths on this host; closing leftover tapdisks \
                       directly instead");
                None
            }
        };
        Ok::<_, SweepFail>((left, state))
    };
    let (mut left, mut state) = read().await?;
    // A teardown still under way (a stopped domain still listed): give it a
    // moment, then read again in the same order.
    if let Some(st) = &state
        && !st.stopping.is_empty()
    {
        let vdis: Vec<String> = st.stopping.iter().map(|d| d.vdi.clone()).collect();
        // That teardown is a close too: on a suspended copy it waits on the
        // frozen writes, so lift first (loser only, and only while nothing
        // runs or is being built on the SR here).
        if let Some(b) = lift {
            b.lift_if_safe(ctl).await;
        }
        let t0 = tokio::time::Instant::now();
        while t0.elapsed() < TEARDOWN_SETTLE {
            tokio::time::sleep(TEARDOWN_POLL).await;
            match ctl.datapaths(Some(&vdis)).await {
                Ok(s) if s.stopping.is_empty() => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        (left, state) = read().await?;
    }
    let api_ok = state.is_some();
    let state = state.unwrap_or_default();
    // Without the storage layer a tapdisk being built for a guest starting
    // here (its datapath live, its block backend not written yet) looks like
    // a leftover. Its backend appears within moments: close only a tapdisk
    // that is still there, with the same pid and minor and still no backend,
    // after a settle.
    if !api_ok && !left.is_empty() {
        tokio::time::sleep(UNKNOWN_DP_SETTLE).await;
        let again = ctl.leftover_tapdisks().await.map_err(SweepFail::other)?;
        let (keep, gone): (Vec<Tapdisk>, Vec<Tapdisk>) =
            left.into_iter().partition(|t| again.contains(t));
        if !gone.is_empty() {
            info!(heal_event = "loser_crash_boundary", step = "tapdisks", tapdisks = %tapdisk_labels(&gone),
                  "tapdisks that looked left over were attached by a domain meanwhile; not closing them");
        }
        left = keep;
    }
    let starting: Vec<Tapdisk> = left
        .iter()
        .filter(|t| {
            vdi_in_path(&t.path).is_some_and(|v| state.live_vdis.iter().any(|x| same_uuid(x, &v)))
        })
        .cloned()
        .collect();
    if !starting.is_empty() {
        return Err(SweepFail {
            detail: format!(
                "a guest domain is being started on this host with disks on the SR ({}); its disk \
                 handles are never closed",
                tapdisk_labels(&starting)
            ),
            starting: true,
        });
    }
    // Something will be closed (a tapdisk by hand, or by SM through a datapath
    // release): a suspended copy would block the close on its frozen writes.
    // Lift it only now, and only then.
    if (!left.is_empty() || !state.stale.is_empty())
        && let Some(b) = lift
    {
        b.lift_if_safe(ctl).await;
    }
    if !left.is_empty() {
        require_close_path(ctl, side)
            .await
            .map_err(SweepFail::other)?;
    }
    if !state.stale.is_empty() {
        info!(heal_event = "loser_crash_boundary", step = "datapaths", datapaths = %datapath_labels(&state.stale),
              "releasing the datapaths destroyed guests left on the SR through XAPI's storage \
               layer, before the discard");
        sweep.released += released(&state.stale, &ctl.destroy_datapaths(&state.stale).await);
        if !left.is_empty() {
            left = ctl.leftover_tapdisks().await.map_err(SweepFail::other)?;
        }
    }
    if !left.is_empty() {
        warn!(heal_event = "loser_crash_boundary", step = "tapdisks", tapdisks = %tapdisk_labels(&left),
              "tapdisks on the SR are still open with no live guest; closing them directly before \
               the discard");
        for (t, r) in left.iter().zip(ctl.destroy_tapdisks(&left).await) {
            match r {
                Ok(()) => sweep.tapdisks_closed += 1,
                Err(e) => {
                    warn!(heal_event = "loser_crash_boundary", step = "tapdisks", pid = t.pid,
                                minor = t.minor, error = %e, "could not destroy a leftover tapdisk")
                }
            }
            if let Some(v) = vdi_in_path(&t.path) {
                push_vdi(&mut candidates, &v);
            }
        }
        let still = ctl.leftover_tapdisks().await.map_err(SweepFail::other)?;
        if !still.is_empty() {
            return Err(SweepFail::other(format!(
                "still open after destroy: {}",
                tapdisk_labels(&still)
            )));
        }
    }
    for d in state.stale.iter().chain(&state.stopping) {
        push_vdi(&mut candidates, &d.vdi);
    }
    if candidates.is_empty() {
        return Ok(sweep);
    }
    if !api_ok {
        sweep.leaked = candidates;
        return Ok(sweep);
    }
    // Every stale datapath released, nothing closed by hand, no teardown under
    // way, no earlier leak: nothing is left for a second pass to find.
    if sweep.released == state.stale.len()
        && sweep.tapdisks_closed == 0
        && state.stopping.is_empty()
        && known_leaked.is_empty()
    {
        return Ok(sweep);
    }
    // Nothing is open any more: a release now only does the bookkeeping.
    match release_pass(ctl, &candidates).await {
        Ok((n, remaining)) => {
            sweep.released += n;
            for d in &remaining {
                push_vdi(&mut sweep.leaked, &d.vdi);
            }
            if !remaining.is_empty() {
                warn!(heal_event = "loser_crash_boundary", step = "datapaths",
                      datapaths = %datapath_labels(&remaining),
                      "XAPI's storage layer still holds these datapaths (not released, or their domain \
                       is still being torn down); no guest using them is started on this host until \
                       they are gone");
            }
        }
        Err(e) => {
            warn!(heal_event = "loser_crash_boundary", step = "datapaths", error = %e,
                  "cannot confirm the datapaths on the SR are released; no guest using these disks \
                   is started on this host until a re-check confirms it");
            sweep.leaked = candidates;
        }
    }
    Ok(sweep)
}

/// The loser's suspend bracket, when the caller may lift (see [`LiftBracket`]);
/// `None` never lifts (the bring-up winner, and callers that resume the copy
/// themselves).
pub type Lift<'a> = Option<&'a LiftBracket>;

/// A disk-handle close writes its cached VHD metadata through this host's
/// multipath map, so it must land on the copy that is replaced, never on the
/// one that is kept. On the loser that is this node's own copy: its own
/// portal must be the active path. On the bring-up winner it is the peer's
/// copy (its guests reached the SR through the peer's portal, and this node
/// does not serve yet): this node's own target must not be exported, or its
/// own path not active.
async fn require_close_path(ctl: &dyn GuestControl, side: Side) -> Result<(), String> {
    match side {
        Side::Loser => require_own_path(ctl).await,
        Side::Winner => {
            if ctl.local_target_exported().await == Some(false) {
                return Ok(());
            }
            match ctl.local_path_active().await {
                Some(false) => Ok(()),
                Some(true) => Err("this node's own portal is the active storage path for the SR: closing a \
                                   disk handle now would write its cached VHD metadata onto the copy this \
                                   node keeps"
                    .to_string()),
                None => Err("this node's target is exported and its storage path cannot be read: closing a \
                             disk handle could write its cached VHD metadata onto the copy this node keeps"
                    .to_string()),
            }
        }
    }
}

async fn require_own_path(ctl: &dyn GuestControl) -> Result<(), String> {
    // A path failed during a long suspend comes back at multipathd's next
    // checker poll, seconds after the lift: wait for it briefly rather than
    // hold for a whole retry interval.
    let t0 = tokio::time::Instant::now();
    let mut active = ctl.local_path_active().await;
    while active != Some(true) && t0.elapsed() < OWN_PATH_WAIT {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        active = ctl.local_path_active().await;
    }
    match active {
        Some(true) => Ok(()),
        Some(false) => Err("this host's active storage path for the SR is the peer's portal: closing a \
                            disk handle now would write its cached VHD metadata onto the peer's copy"
            .to_string()),
        None => Err("cannot confirm that this host's active storage path for the SR is its own portal \
                     (no multipath map, or it cannot be read): closing a disk handle could write its \
                     cached VHD metadata onto the peer's copy"
            .to_string()),
    }
}

/// Release the stale datapaths on `vdis` and read them back: `(released,
/// still held)`, where a datapath of a domain still being torn down counts as
/// held. Never call it while a leftover tapdisk serves one of `vdis` after
/// the discard: the release closes it, and the close writes its cached VHD
/// metadata.
async fn release_pass(
    ctl: &dyn GuestControl,
    vdis: &[String],
) -> Result<(usize, Vec<Datapath>), String> {
    let st = ctl.datapaths(Some(vdis)).await?;
    if st.stale.is_empty() {
        return Ok((0, st.stopping));
    }
    let n = released(&st.stale, &ctl.destroy_datapaths(&st.stale).await);
    let after = ctl.datapaths(Some(vdis)).await?;
    Ok((n, after.stale.into_iter().chain(after.stopping).collect()))
}

fn released(dps: &[Datapath], results: &[Result<(), String>]) -> usize {
    let mut n = 0;
    for (d, r) in dps.iter().zip(results) {
        match r {
            Ok(()) => n += 1,
            Err(e) => {
                warn!(heal_event = "loser_crash_boundary", step = "datapaths", dp = %d.dp, vdi = %d.vdi,
                            error = %e, "XAPI's storage layer refused to release a datapath")
            }
        }
    }
    n
}

/// Before a guest the boundary stopped is started on this host (after the
/// discard, or once the episode ended): prove nothing stale holds its disks
/// here. Stale datapaths are released through the storage layer; a leftover
/// tapdisk is never closed here (it was opened before the discard, and its
/// close would write that history's cached VHD metadata into the resynced
/// disk), so it refuses instead. `Ok` with the guest's SR VDIs, all clean.
pub async fn prove_local_start_clean(
    ctl: &dyn GuestControl,
    g: &GuestRef,
) -> Result<Vec<String>, String> {
    let vdis = ctl
        .guest_sr_vdis(g)
        .await
        .map_err(|e| format!("cannot read which of its disks are on the SR ({e})"))?;
    if vdis.is_empty() {
        return Ok(vdis);
    }
    let taps: Vec<Tapdisk> = ctl
        .leftover_tapdisks()
        .await
        .map_err(|e| format!("cannot check for leftover tapdisks: {e}"))?
        .into_iter()
        .filter(|t| vdi_in_path(&t.path).is_some_and(|v| vdis.iter().any(|x| same_uuid(x, &v))))
        .collect();
    if !taps.is_empty() {
        return Err(format!(
            "disk handles left open with no live guest still hold its disks ({}); closing one now \
             would write VHD metadata cached before the discard into the resynced disk, so \
             twinstor does not. Kill each without a close (`kill -9 <pid>`), then release its \
             datapath (`{}` with the dp `tap-ctl list` no longer shows; `DP.diagnostics` lists \
             them), and check the disk with `vhd-util check -n <device>` before starting it",
            tapdisk_labels(&taps),
            crate::xapi_storage::destroy_command("vbd/<domid>/<device>")
        ));
    }
    let (_, remaining) = release_pass(ctl, &vdis)
        .await
        .map_err(|e| format!("cannot read XAPI's storage datapaths on this host ({e})"))?;
    if remaining.is_empty() {
        Ok(vdis)
    } else {
        let cmds: Vec<String> = remaining
            .iter()
            .map(|d| crate::xapi_storage::destroy_command(&d.dp))
            .collect();
        Err(format!(
            "XAPI's storage layer on this host still holds datapaths of destroyed domains on its \
             disks ({}), so a start here would reuse a stale attachment; release them on this host \
             with: {}",
            datapath_labels(&remaining),
            cmds.join(" ; ")
        ))
    }
}

/// Retry releasing datapaths recorded as leaked, skipping any VDI a leftover
/// tapdisk still serves (never closed after the discard). Returns the VDIs
/// still leaked.
async fn release_leaked(ctl: &dyn GuestControl, leaked: &[String]) -> Vec<String> {
    let held: Vec<String> = match ctl.leftover_tapdisks().await {
        Ok(taps) => taps.iter().filter_map(|t| vdi_in_path(&t.path)).collect(),
        Err(_) => return leaked.to_vec(),
    };
    let free: Vec<String> = leaked
        .iter()
        .filter(|v| !held.iter().any(|h| same_uuid(h, v)))
        .cloned()
        .collect();
    if free.is_empty() {
        return leaked.to_vec();
    }
    match release_pass(ctl, &free).await {
        Ok((n, remaining)) => {
            let still: Vec<String> = leaked
                .iter()
                .filter(|v| !free.contains(v) || remaining.iter().any(|d| same_uuid(&d.vdi, v)))
                .cloned()
                .collect();
            if still.len() < leaked.len() {
                info!(
                    heal_event = "loser_crash_boundary",
                    step = "datapaths",
                    released = n,
                    still_leaked = still.len(),
                    "datapaths left on the SR by destroyed guests are released on this host"
                );
            }
            still
        }
        Err(e) => {
            warn!(heal_event = "loser_crash_boundary", step = "datapaths", error = %e,
                  "cannot re-check the datapaths left on the SR; retrying later");
            leaked.to_vec()
        }
    }
}

fn push_vdi(list: &mut Vec<String>, v: &str) {
    if !list.iter().any(|x| same_uuid(x, v)) {
        list.push(v.to_ascii_lowercase());
    }
}

fn datapath_labels(dps: &[Datapath]) -> String {
    dps.iter()
        .map(|d| format!("{} on VDI {}", d.dp, d.vdi))
        .collect::<Vec<_>>()
        .join(", ")
}

fn tapdisk_labels(taps: &[Tapdisk]) -> String {
    taps.iter()
        .map(|t| format!("pid {} minor {} ({})", t.pid, t.minor, t.path))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The VDI uuid an image or backend path ends in, whatever its SR: an LVM
/// SR's `.../<FORMAT>-<vdi>` (`VHD-`, `LV-`, `QCOW2-`), SM's
/// `/dev/sm/backend/<sr>/<vdi>`, a file SR's `.../<vdi>.vhd` or `.qcow2`.
pub fn vdi_in_path(path: &str) -> Option<String> {
    let last = path.rsplit('/').next()?.to_ascii_lowercase();
    let last = last
        .strip_suffix(".vhd")
        .or_else(|| last.strip_suffix(".qcow2"))
        .unwrap_or(&last);
    let v = strip_image_prefix(last);
    looks_like_uuid(v).then(|| v.to_string())
}

/// Strip any image-format prefix SM puts before a VDI uuid on an LVM SR
/// (`VHD-`, `LV-`, `QCOW2-`, ...), turning `<FORMAT>-<vdi...>` into
/// `<vdi...>`; anything else is returned unchanged. Every format must match,
/// or a leftover tapdisk of that format would go unrecognised and survive
/// the discard. Expects lowercase input.
fn strip_image_prefix(s: &str) -> &str {
    match s.split_once('-') {
        Some((fmt, rest))
            if !fmt.is_empty()
                && fmt.chars().all(|c| c.is_ascii_alphanumeric())
                && rest.get(..36).is_some_and(looks_like_uuid) =>
        {
            rest
        }
        _ => s,
    }
}

/// One `tap-ctl list` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tapdisk {
    pub pid: u32,
    pub minor: u32,
    /// The image the tapdisk serves (`args=` without its driver prefix), or
    /// empty when none is open.
    pub path: String,
}

/// `tap-ctl list`, read until every line parses (at most
/// `TAP_LIST_TRIES` reads, 1 s apart). Right after a domain is destroyed, a
/// tapdisk being torn down is listed for a moment without its minor (or a
/// minor without its process). Read only once, that line would hold the
/// whole boundary and re-freeze the copy; the next read could then time out
/// on the frozen tapdisk and keep the host's XHA statefile frozen until its
/// watchdog reboots the host. A line still not understood after the retries
/// is an error naming it, and the caller holds.
async fn tap_ctl_list_settled() -> Result<Vec<Tapdisk>, String> {
    let mut last = String::new();
    for attempt in 0..TAP_LIST_TRIES {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        let list = crate::cmd::cmd_output_for("tap-ctl", &["list"], LOCAL_READ_BOUND_SECS)
            .await
            .map_err(|e| format!("tap-ctl list failed: {e}"))?;
        if let Some(taps) = parse_tap_ctl_list(&list) {
            // A bare `minor=N` is skipped by the parser as an orphan, but
            // tap-ctl prints the same line for a live tapdisk whose pid query
            // failed (its control socket gone, or refused): that one may hold
            // an image open. Accept the listing only when every tapdisk
            // process on the host is listed with its pid.
            let orphans = orphan_minors(&list);
            if !orphans.is_empty() {
                match unlisted_tapdisk_pids(&taps).await {
                    Ok(unlisted) if unlisted.is_empty() => {
                        info!(heal_event = "loser_crash_boundary", step = "tap_orphan_minors", minors = ?orphans,
                              "tap-ctl lists minors with no tapdisk process (left by a tapdisk that died); \
                               they hold no image");
                    }
                    Ok(unlisted) => {
                        last = format!(
                            "minor(s) {orphans:?} with tapdisk process(es) {unlisted:?} not listed"
                        );
                        continue;
                    }
                    Err(e) => {
                        last = format!("minor(s) {orphans:?}, tapdisk processes unreadable: {e}");
                        continue;
                    }
                }
            }
            if attempt > 0 {
                info!(heal_event = "loser_crash_boundary", step = "tap_list_settled", reads = attempt + 1,
                      unparsed = %last, "tap-ctl list settled after a tapdisk being torn down");
            }
            return Ok(taps);
        }
        last = first_unparsed_tap_line(&list)
            .unwrap_or_default()
            .to_string();
    }
    Err(format!("tap-ctl list output not understood: {last:?}"))
}

/// Running `tapdisk` processes the listing does not name by pid.
async fn unlisted_tapdisk_pids(taps: &[Tapdisk]) -> Result<Vec<u32>, String> {
    let (_, out, _) =
        crate::cmd::cmd_status_for("pgrep", &["-x", "tapdisk"], LOCAL_READ_BOUND_SECS)
            .await
            .map_err(|e| format!("pgrep failed: {e}"))?;
    Ok(out
        .split_whitespace()
        .filter_map(|p| p.parse::<u32>().ok())
        .filter(|p| !taps.iter().any(|t| t.pid == *p))
        .collect())
}

/// The minors `tap-ctl list` prints alone (`minor=N`, nothing else): a minor
/// with no tapdisk answering for it.
pub fn orphan_minors(text: &str) -> Vec<u32> {
    text.lines()
        .filter_map(|l| {
            let mut toks = l.split_whitespace();
            let m = toks.next()?.strip_prefix("minor=")?.parse().ok()?;
            toks.next().is_none().then_some(m)
        })
        .collect()
}

/// Reads of [`tap_ctl_list_settled`].
const TAP_LIST_TRIES: u32 = 5;

/// The first `tap-ctl list` line [`parse_tap_ctl_list`] rejects.
pub fn first_unparsed_tap_line(text: &str) -> Option<&str> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .find(|l| parse_tap_ctl_list(l).is_none())
}

/// Parse `tap-ctl list` (`pid=34167 minor=0 state=0 args=vhd:/dev/...`).
/// `None` when a line does not parse: an unexpected format must not read as
/// "no tapdisks".
pub fn parse_tap_ctl_list(text: &str) -> Option<Vec<Tapdisk>> {
    let mut out = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let mut pid = None;
        let mut minor = None;
        let mut path = String::new();
        for tok in line.split_whitespace() {
            if let Some(v) = tok.strip_prefix("pid=") {
                pid = Some(v.parse::<u32>().ok()?);
            } else if let Some(v) = tok.strip_prefix("minor=") {
                minor = Some(v.parse::<u32>().ok()?);
            } else if let Some(v) = tok.strip_prefix("args=") {
                path = v.split_once(':').map_or(v, |(_, p)| p).to_string();
            }
        }
        // `minor=N` alone: a minor no tapdisk answered for, which tap-ctl
        // splices into the list (a tapdisk that died without freeing it, or
        // a live one whose pid query failed: the reader checks the tapdisk
        // processes before trusting it). Rejecting it would hold every
        // boundary for as long as the orphan lasts (until a reboot).
        if line.split_whitespace().count() == 1 && minor.is_some() {
            continue;
        }
        out.push(Tapdisk {
            pid: pid?,
            minor: minor?,
            path,
        });
    }
    Some(out)
}

/// The VDI uuid an image or backend path names on `sr`: an LVM SR's
/// `/dev/VG_XenStorage-<sr>/VHD-<vdi>` (or `LV-<vdi>`), SM's
/// `/dev/sm/backend/<sr>/<vdi>`, or a file SR's `/sr-mount/<sr>/<vdi>.vhd`.
/// `None` for anything else, including other SRs.
pub fn sr_vdi_of_path(path: &str, sr: &str) -> Option<String> {
    let p = path.to_ascii_lowercase();
    let sr = sr.to_ascii_lowercase();
    let rest = [
        format!("vg_xenstorage-{sr}/"),
        format!("/dev/sm/backend/{sr}/"),
        format!("/sr-mount/{sr}/"),
    ]
    .iter()
    .find_map(|m| p.split_once(m.as_str()).map(|(_, r)| r.to_string()))?;
    let rest = strip_image_prefix(&rest);
    let vdi = rest.get(..36)?;
    looks_like_uuid(vdi).then(|| vdi.to_string())
}

/// VDIs of `sr` attached through a block backend of a live domain: any
/// `/local/domain/0/backend/<kind>/<domid>/<dev>/params` whose domid is dom0
/// or in `live`. Stale backend nodes of destroyed domains do not count.
pub fn live_sr_vdis(xenstore: &str, sr: &str, live: &[u32]) -> Vec<String> {
    let mut vdis = Vec::new();
    for line in xenstore.lines() {
        let Some((path, value)) = line.split_once(" = ") else {
            continue;
        };
        let Some(rest) = path.trim().strip_prefix("/local/domain/0/backend/") else {
            continue;
        };
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() != 4 || parts[3] != "params" {
            continue;
        }
        let Ok(domid) = parts[1].parse::<u32>() else {
            continue;
        };
        if domid != 0 && !live.contains(&domid) {
            continue;
        }
        if let Some(v) = sr_vdi_of_path(value.trim().trim_matches('"'), sr)
            && !vdis.contains(&v)
        {
            vdis.push(v);
        }
    }
    vdis
}

/// Tapdisks serving a VDI of `sr` that no live domain has attached.
pub fn leftover_sr_tapdisks(taps: &[Tapdisk], sr: &str, live_vdis: &[String]) -> Vec<Tapdisk> {
    taps.iter()
        .filter(|t| sr_vdi_of_path(&t.path, sr).is_some_and(|v| !live_vdis.contains(&v)))
        .cloned()
        .collect()
}

/// A guest in log lines and hold details: its name and uuid (names are not
/// unique in a pool).
fn guest_label(g: &GuestRef) -> String {
    if g.name.is_empty() {
        g.uuid.clone()
    } else {
        format!("{} ({})", g.name, g.uuid)
    }
}

/// Whether the owed restarts may run now: once the discard was started, once
/// the split-brain episode positively converged (`open` cleared; a vanished
/// tmpfs marker after a reboot is not an episode end), or during a hold whose
/// loser verdict came from the lineage (the winner is authoritative either
/// way).
pub fn restarts_due(rec: &Record) -> bool {
    rec.phase == Phase::Discarding || !rec.open || rec.early_restart
}

/// Where the owed restarts go. The winner (the peer) while this node is the
/// discarding side: once its discard started, or during a lineage-decided
/// hold. Otherwise the episode ended without this node discarding (it was
/// flipped to winner, or converged by another path): both copies are the same
/// again and this host, which kept its data, takes them back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartHost {
    Peer,
    ThisHost,
}

pub fn restart_host(rec: &Record) -> RestartHost {
    if rec.phase == Phase::Discarding || rec.early_restart {
        RestartHost::Peer
    } else {
        RestartHost::ThisHost
    }
}

/// Failed starts of one guest before the escalation alert. Retries continue
/// after it, at a slower pace; an owed restart is never silently dropped.
pub const MAX_START_ATTEMPTS: u32 = 10;

/// One owed-restart step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartTick {
    /// No record, or the restarts are not yet due.
    Idle,
    /// The record exists but cannot be read: alert, never act on it.
    Malformed(String),
    /// The XAPI host identities are unknown: nothing attempted.
    NoTarget,
    /// One guest handled. `start_tries` counts this guest's failures so far.
    Step {
        guest: GuestRef,
        host: RestartHost,
        outcome: StartOutcome,
        start_tries: u32,
        remaining: usize,
    },
}

/// Run one owed restart if due, persisting the record after it. A start that
/// succeeded (or found the guest already running elsewhere) is removed; a
/// failed one is counted and moved to the back of the queue. The record is
/// cleared once nothing is owed.
///
/// `rejoined`: this node finished its discard and is Connected again, its
/// copy resyncing from (or equal to) the winner's. A guest the winner cannot
/// start because this host still holds its disk is then started here instead:
/// a fresh boot on this host reads the winner's history (the resync, or the
/// winner's portal while this copy is not served), so no running guest ever
/// sees a history switch.
pub async fn run_restarts(
    ctl: &dyn GuestControl,
    store: &Store,
    rejoined: bool,
    converged: bool,
) -> RestartTick {
    run_restarts_ext(ctl, store, rejoined, converged, false).await
}

/// `run_restarts`, with `serving_alone`: this copy is UpToDate and serves on
/// its own (Primary, not frozen, the peer gone). Once the episode converged
/// (`open` cleared: this copy took the winner's history), a host the boundary
/// disabled is enabled again then too: a peer that died after the
/// convergence never comes back to converge again, and this host would
/// otherwise stay disabled, with every guest down and none placeable on the
/// only copy.
pub async fn run_restarts_ext(
    ctl: &dyn GuestControl,
    store: &Store,
    rejoined: bool,
    converged: bool,
    serving_alone: bool,
) -> RestartTick {
    let mut rec = match store.load() {
        Ok(Some(r)) => r,
        Ok(None) => return RestartTick::Idle,
        Err(e) => return RestartTick::Malformed(e),
    };
    // This step runs off the daemon tick, across long awaits: it works on
    // `rec` (a snapshot) and records only its own changes in `delta`, applied
    // at the end to the record as it is then (see `StepDelta::apply`).
    let mut delta = StepDelta::default();
    // Datapaths whose release failed earlier: retried on every step until
    // released (a VDI with a leftover tapdisk is never released here, see
    // `release_pass`), whatever is owed or due, so a later start here cannot
    // reuse them and a release done by hand is noticed: while any is listed
    // the generic host-enable watchdog holds, and a bring-up winner whose
    // episode is not closed yet would otherwise never get its host back.
    // Never while this copy's I/O is suspended (or that cannot be read): SM's
    // close would block on frozen writes, outside the boundary's bracket.
    if !rec.leaked.is_empty() && ctl.io_suspended().await == Some(false) {
        let still = release_leaked(ctl, &rec.leaked).await;
        delta.cleaned = rec
            .leaked
            .iter()
            .filter(|v| !still.iter().any(|x| same_uuid(x, v)))
            .cloned()
            .collect();
        rec.leaked = still;
    }
    if rec.owed.is_empty() {
        // Every guest is back. A host the boundary disabled is enabled again
        // once the pair has converged (both copies equal), or the episode
        // converged and this copy now serves alone, and no stale datapath is
        // left on it (a start or migration onto this host would reuse one),
        // not left to the generic watchdog's longer window.
        let settled = converged || (!rec.open && serving_alone);
        if rec.host_disabled && settled && rec.leaked.is_empty() {
            match ctl.enable_local_host().await {
                Ok(()) => {
                    info!(
                        heal_event = "loser_crash_boundary",
                        step = "host_enable",
                        converged,
                        serving_alone,
                        "split-brain loser: the pair has converged (or this copy, which took the \
                           winner's history, now serves alone); enabling this host again"
                    );
                    delta.enabled = true;
                }
                Err(e) => {
                    warn!(heal_event = "loser_crash_boundary", step = "host_enable", error = %e,
                                "split-brain loser: could not enable this host again; retrying")
                }
            }
        }
        let _ = delta.apply(store);
        return RestartTick::Idle;
    }
    if !restarts_due(&rec) {
        let _ = delta.apply(store);
        return RestartTick::Idle;
    }
    let Some((this_host, peer_host)) = ctl.host_uuids().await else {
        let _ = delta.apply(store);
        return RestartTick::NoTarget;
    };
    let host = restart_host(&rec);
    let target = match host {
        RestartHost::Peer => &peer_host,
        RestartHost::ThisHost => &this_host,
    };
    let mut o = rec.owed.remove(0);
    // Never start it here over a stale attachment of its disks; before a start
    // on the winner, release what this host still holds of them (best-effort:
    // the winner's SM refuses the disk while this host's key remains, and that
    // refusal is handled below).
    let gate = match host {
        RestartHost::ThisHost => prove_local_start_clean(ctl, &o.guest).await,
        RestartHost::Peer => {
            let r = prove_local_start_clean(ctl, &o.guest).await;
            if let Err(e) = &r {
                info!(heal_event = "loser_crash_boundary", step = "release_before_peer_start",
                      vm = %guest_label(&o.guest), detail = %e,
                      "split-brain loser: could not release this host's hold on the guest's disks \
                       before starting it on the winner");
            }
            r
        }
    };
    if let Ok(vdis) = &gate {
        rec.leaked.retain(|v| !vdis.iter().any(|x| same_uuid(x, v)));
        delta.cleaned.extend(vdis.iter().cloned());
    }
    // A host the boundary disabled is enabled before a start here, and only
    // once its disks are proven clean.
    if host == RestartHost::ThisHost && rec.host_disabled && gate.is_ok() {
        match ctl.enable_local_host().await {
            Ok(()) => {
                rec.host_disabled = false;
                delta.enabled = true;
            }
            Err(e) => warn!(heal_event = "loser_crash_boundary", step = "host_enable", error = %e,
                            "split-brain: could not enable this host before restarting a guest here"),
        }
    }
    let mut outcome = match gate {
        Err(e) if host == RestartHost::ThisHost => {
            StartOutcome::Failed(format!("not started here: {e}"))
        }
        _ => ctl.start_guest(&o.guest, target, &this_host).await,
    };
    if outcome == StartOutcome::Stale {
        let r = ctl
            .reconcile_stale(&o.guest, &this_host, !rec.agent_restarted)
            .await;
        rec.agent_restarted |= r.agent_restarted;
        delta.agent_restarted |= r.agent_restarted;
        info!(heal_event = "loser_crash_boundary", step = "reconcile_stale", vm = %guest_label(&o.guest),
              cleared = r.cleared, agent_restarted = r.agent_restarted, detail = %r.detail,
              "split-brain loser: stale live record of a guest stopped here");
        outcome = if r.cleared {
            match ctl.start_guest(&o.guest, target, &this_host).await {
                StartOutcome::Stale => {
                    StartOutcome::Failed(format!("the stale live record came back ({})", r.detail))
                }
                other => other,
            }
        } else {
            StartOutcome::Failed(format!(
                "XAPI still records it running or paused on this host but no domain exists ({}); \
                 restart XAPI on this host (systemctl restart xapi), or on the pool master run \
                 `xe vm-reset-powerstate uuid={} --force`, and it is started on the next round",
                r.detail, o.guest.uuid
            ))
        };
    }
    // A live domain here is a fresh start (HA, an operator) only once this
    // copy was given up; before that it is a guest the boundary could not
    // stop, still owed. A paused one is neither running nor startable.
    outcome = match outcome {
        StartOutcome::RunningHere if rec.phase == Phase::Discarding || !rec.open => {
            StartOutcome::AlreadyRunning
        }
        StartOutcome::RunningHere => StartOutcome::Failed(
            "still running on this host (the boundary could not stop it); owed until it is stopped"
                .to_string(),
        ),
        StartOutcome::PausedHere => StartOutcome::Failed(
            "a paused domain for it exists on this host (a rebuild that did not finish); retried"
                .to_string(),
        ),
        other => other,
    };
    let mut host = host;
    // Repeated failures on the winner (other than a disk held here) also fall
    // back to this host once it has rejoined after its discard.
    if host == RestartHost::Peer
        && rec.phase == Phase::Discarding
        && rejoined
        && o.start_tries >= 2
        && let StartOutcome::Failed(why) = &outcome
    {
        outcome = StartOutcome::VdiHeldHere(format!(
            "{why} (after {} attempts on the winner)",
            o.start_tries
        ));
    }
    if let StartOutcome::VdiHeldHere(why) = &outcome {
        outcome = if host == RestartHost::Peer && rec.phase == Phase::Discarding && rejoined {
            info!(heal_event = "loser_crash_boundary", step = "restart_here", vm = %guest_label(&o.guest),
                  detail = %why,
                  "split-brain loser: the winner cannot attach this guest's disk while this host \
                   holds it; this node has rejoined, so it is started here");
            host = RestartHost::ThisHost;
            // Never start it here through a disk handle left from before the
            // discard: SM would reuse the stale attachment (the losing
            // history's block map, or a backend node naming another tapdisk).
            let clean = match prove_local_start_clean(ctl, &o.guest).await {
                Ok(vdis) => {
                    rec.leaked.retain(|v| !vdis.iter().any(|x| same_uuid(x, v)));
                    delta.cleaned.extend(vdis);
                    Ok(())
                }
                Err(e) => Err(e),
            };
            let enabled = if let Err(e) = clean {
                Err(e)
            } else if rec.host_disabled {
                match ctl.enable_local_host().await {
                    Ok(()) => {
                        rec.host_disabled = false;
                        delta.enabled = true;
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            } else {
                Ok(())
            };
            match enabled {
                Ok(()) => match ctl.start_guest(&o.guest, &this_host, &this_host).await {
                    StartOutcome::VdiHeldHere(w) => StartOutcome::Failed(w),
                    other => other,
                },
                Err(e) => StartOutcome::Failed(format!("not started here: {e}")),
            }
        } else {
            StartOutcome::Failed(format!(
                "{why}; this host still holds the disk, so it is started here once this node \
                 has rejoined after its discard"
            ))
        };
    }
    info!(heal_event = "loser_crash_boundary", step = "restart", vm = %guest_label(&o.guest),
          host = ?host, outcome = ?outcome, key = %rec.key, phase = ?rec.phase,
          failed_before = o.start_tries, still_owed = rec.owed.len(),
          "split-brain loser: owed guest restart");
    let failed = matches!(outcome, StartOutcome::Failed(_));
    if failed {
        o.start_tries = o.start_tries.saturating_add(1);
    }
    delta.guest = Some((o.guest.uuid.clone(), failed));
    let remaining = match delta.apply(store) {
        Ok(Some(n)) => n,
        Ok(None) => 0,
        Err(e) => {
            warn!(heal_event = "loser_crash_boundary", step = "restart", error = %e,
                  "could not update the restart obligation");
            rec.owed.len() + usize::from(failed)
        }
    };
    RestartTick::Step {
        guest: o.guest,
        host,
        outcome,
        start_tries: o.start_tries,
        remaining,
    }
}

/// What one owed-restart step changed, applied to the record as it is once
/// the step's awaits are over: the daemon tick may have rewritten it
/// meanwhile (a new boundary round, the phase, the episode's end, a suspect
/// guest dropped), and a whole-record save of the step's snapshot would
/// erase that.
#[derive(Debug, Default)]
struct StepDelta {
    /// The guest handled: `(uuid, failed)`. Removed from the owed list; a
    /// failure is counted and moved to the back, unless the guest is no
    /// longer owed at all.
    guest: Option<(String, bool)>,
    agent_restarted: bool,
    /// This step enabled the host.
    enabled: bool,
    /// VDIs proven free of stale datapaths by this step.
    cleaned: Vec<String>,
}

impl StepDelta {
    /// `Ok(Some(owed left))`, `Ok(None)` when there is no record any more.
    fn apply(self, store: &Store) -> Result<Option<usize>, String> {
        store.update(|r| {
            if let Some((uuid, failed)) = &self.guest
                && let Some(i) = r.owed.iter().position(|o| same_uuid(&o.guest.uuid, uuid))
            {
                let mut o = r.owed.remove(i);
                if *failed {
                    o.start_tries = o.start_tries.saturating_add(1);
                    r.owed.push(o);
                }
            }
            r.agent_restarted |= self.agent_restarted;
            if self.enabled {
                r.host_disabled = false;
            }
            r.leaked
                .retain(|v| !self.cleaned.iter().any(|x| same_uuid(x, v)));
            let done = r.owed.is_empty() && !r.host_disabled && r.leaked.is_empty();
            (done, r.owed.len())
        })
    }
}

fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Parse `xl list -v` into `(domid, uuid)` pairs, Domain-0 excluded. The UUID
/// column is the last uuid-shaped token of a row (it follows the name, which
/// may itself contain spaces or a uuid) and the domid sits five fields before
/// it (`<name...> ID Mem VCPUs State Time UUID ...`). A domain whose State
/// carries `s` (shut down), `c` (crashed) or `d` (dying) runs no code any more
/// and is left out: a guest destroyed while its frozen I/O is still pending
/// lingers as a dying domain until that I/O drains, and must not hold the
/// boundary forever. `None` when a row does not parse (an unexpected format
/// must not read as "no guests").
pub fn parse_xl_verbose(text: &str) -> Option<Vec<(u32, String)>> {
    let mut out = Vec::new();
    for line in text.lines().skip(1).filter(|l| !l.trim().is_empty()) {
        let toks: Vec<&str> = line.split_whitespace().collect();
        let i = toks.iter().rposition(|t| looks_like_uuid(t))?;
        let domid: u32 = toks.get(i.checked_sub(5)?)?.parse().ok()?;
        let state = toks.get(i.checked_sub(2)?)?;
        if !state.chars().all(|c| "rbpscd-".contains(c)) {
            return None;
        }
        if domid == 0 || state.contains(['s', 'c', 'd']) {
            continue;
        }
        out.push((domid, toks[i].to_ascii_lowercase()));
    }
    Some(out)
}

/// The dom0 helper behind [`GuestControl::poweroff_domains`]: power each domid
/// off at the hypervisor (`SHUTDOWN_poweroff` = 0), never domain 0. Prints
/// `noxc <why>` and exits 3 when the binding is missing, else one line
/// `done` followed by `fail <domid> <why>` lines.
const POWEROFF_PY: &str = "import sys
try:
    import xen.lowlevel.xc as x
    c = x.xc()
except Exception as e:
    print('noxc %s' % e)
    sys.exit(3)
print('done')
for a in sys.argv[1:]:
    d = int(a)
    if d == 0:
        continue
    try:
        c.domain_shutdown(d, 0)
    except Exception as e:
        print('fail %d %s' % (d, e))
";

/// What the poweroff helper reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoweroffReport {
    /// The binding is missing or the helper did not run as expected.
    Unavailable,
    /// The calls were made; `failed` = `<domid> <why>` per refused domain.
    Done { failed: Vec<String> },
}

/// Parse the poweroff helper's output (`ok` = exit status 0). Anything but a
/// clean `done` reads as unavailable, so the caller falls back.
pub fn parse_poweroff_output(ok: bool, out: &str) -> PoweroffReport {
    let mut lines = out.lines().map(str::trim).filter(|l| !l.is_empty());
    if !ok || lines.next() != Some("done") {
        return PoweroffReport::Unavailable;
    }
    let mut failed = Vec::new();
    for l in lines {
        match l.strip_prefix("fail ") {
            Some(f) => failed.push(f.to_string()),
            None => return PoweroffReport::Unavailable,
        }
    }
    PoweroffReport::Done { failed }
}

/// Every domid `xl list -v` lists, in any state (dom0 included): a dying
/// domain still owns its datapaths while xenopsd tears it down. `None` when a
/// row does not parse.
pub fn parse_xl_all_domids(text: &str) -> Option<Vec<u32>> {
    let mut out = Vec::new();
    for line in text.lines().skip(1).filter(|l| !l.trim().is_empty()) {
        let toks: Vec<&str> = line.split_whitespace().collect();
        let i = toks.iter().rposition(|t| looks_like_uuid(t))?;
        out.push(toks.get(i.checked_sub(5)?)?.parse().ok()?);
    }
    Some(out)
}

/// A guest's VDIs on the SR: the `vdi-uuid`s of its disk VBDs (`vbd-list
/// vm-uuid=<vm> type=Disk params=vdi-uuid --minimal`) that are in the SR's
/// `vdi-list --minimal`.
pub fn guest_vdis_from_minimal(vbd_vdis: &str, sr_vdis: &str) -> Vec<String> {
    let on_sr: Vec<&str> = sr_vdis
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let mut out = Vec::new();
    for v in vbd_vdis.split(',').map(str::trim) {
        if looks_like_uuid(v) && on_sr.iter().any(|s| same_uuid(s, v)) {
            push_vdi(&mut out, v);
        }
    }
    out
}

/// The state of `uuid`'s domain on this host from `xl list -v`: `Paused` when
/// its State column carries `p`, `Live` for any other running state, `None`
/// when absent or no longer running code (shut down, crashed, dying). `None`
/// (the Option) when the listing does not parse.
pub fn local_domain_state(text: &str, uuid: &str) -> Option<LocalDomain> {
    for line in text.lines().skip(1).filter(|l| !l.trim().is_empty()) {
        let toks: Vec<&str> = line.split_whitespace().collect();
        let i = toks.iter().rposition(|t| looks_like_uuid(t))?;
        let state = toks.get(i.checked_sub(2)?)?;
        if !state.chars().all(|c| "rbpscd-".contains(c)) {
            return None;
        }
        if !same_uuid(toks[i], uuid) {
            continue;
        }
        return Some(if state.contains(['s', 'c', 'd']) {
            LocalDomain::None
        } else if state.contains('p') {
            LocalDomain::Paused
        } else {
            LocalDomain::Live
        });
    }
    Some(LocalDomain::None)
}

/// Domids whose block backend (any `backend/<kind>/<domid>/<dev>/...` node in
/// `xenstore-ls -f /local/domain/0/backend`) references the SR uuid: the
/// device path of an SR-backed VDI (`/dev/sm/backend/<sr>/<vdi>`, or the
/// `VG_XenStorage-<sr>` volume of an LVM SR) carries it. Network backends
/// never do.
pub fn sr_domids_from_xenstore(text: &str, sr: &str) -> Vec<u32> {
    let sr = sr.to_ascii_lowercase();
    let mut ids = Vec::new();
    for line in text.lines() {
        let Some((path, value)) = line.split_once(" = ") else {
            continue;
        };
        if !value.to_ascii_lowercase().contains(&sr) {
            continue;
        }
        let Some(rest) = path.trim().strip_prefix("/local/domain/0/backend/") else {
            continue;
        };
        let mut parts = rest.split('/');
        let (Some(_kind), Some(domid)) = (parts.next(), parts.next()) else {
            continue;
        };
        if let Ok(d) = domid.parse::<u32>()
            && d != 0
            && !ids.contains(&d)
        {
            ids.push(d);
        }
    }
    ids
}

/// Parse multi-record `xe` output (records separated by blank lines, lines
/// `key ( RO): value`) into key/value maps.
pub fn parse_xe_records(text: &str) -> Vec<Vec<(String, String)>> {
    let mut recs = Vec::new();
    let mut cur: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            if !cur.is_empty() {
                recs.push(std::mem::take(&mut cur));
            }
            continue;
        }
        let Some((lhs, value)) = line.split_once(':') else {
            continue;
        };
        let key = lhs.split(" (").next().unwrap_or(lhs).trim().to_string();
        cur.push((key, value.trim().to_string()));
    }
    if !cur.is_empty() {
        recs.push(cur);
    }
    recs
}

fn field<'a>(rec: &'a [(String, String)], key: &str) -> Option<&'a str> {
    rec.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// Guests (from `vm-list resident-on=<this host> params=uuid,name-label`) that
/// have an attached VBD (`vbd-list currently-attached=true
/// params=vm-uuid,vdi-uuid`) on a VDI of the SR (`vdi-list sr-uuid=<sr>
/// --minimal`).
pub fn select_sr_guests(vms: &str, sr_vdis_minimal: &str, vbds: &str) -> Vec<GuestRef> {
    let vdis: Vec<&str> = sr_vdis_minimal
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let attached: Vec<(String, String)> = parse_xe_records(vbds)
        .iter()
        .filter_map(|r| {
            Some((
                field(r, "vm-uuid")?.to_string(),
                field(r, "vdi-uuid")?.to_string(),
            ))
        })
        .collect();
    parse_xe_records(vms)
        .iter()
        .filter_map(|r| {
            let uuid = field(r, "uuid")?;
            let on_sr = attached
                .iter()
                .any(|(vm, vdi)| vm == uuid && vdis.contains(&vdi.as_str()));
            on_sr.then(|| GuestRef {
                uuid: uuid.to_string(),
                name: field(r, "name-label").unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// The production [`GuestControl`]: XAPI for the pool-level facts and
/// actions, `xl` and xenstore for the local scan and the stop backstop.
#[cfg_attr(test, allow(dead_code))]
pub struct RealGuests {
    /// LIO target name, to read the kernel export state.
    pub iqn: String,
    /// This node's DRBD resource (I/O suspend state).
    drbd: crate::drbd::DrbdController,
    /// The peer's storage address (which multipath path is this node's own).
    peer_ip: String,
}

#[cfg_attr(test, allow(dead_code))]
impl RealGuests {
    pub fn new(config: &crate::config::Config) -> Self {
        Self {
            iqn: config.iscsi.target_iqn.clone(),
            drbd: crate::drbd::DrbdController::new(config),
            peer_ip: config.network.peer_address.to_string(),
        }
    }

    async fn xl_verbose() -> Result<Vec<(u32, String)>, String> {
        let out = crate::cmd::cmd_output_for("xl", &["list", "-v"], LOCAL_READ_BOUND_SECS)
            .await
            .map_err(|e| format!("xl list -v failed: {e}"))?;
        parse_xl_verbose(&out).ok_or_else(|| "xl list -v output not understood".to_string())
    }
}

#[async_trait::async_trait]
impl GuestControl for RealGuests {
    async fn local_target_exported(&self) -> Option<bool> {
        target_exported_at(Path::new(LIO_ISCSI_CONFIGFS), &self.iqn)
    }

    async fn sr_guests(&self) -> Result<Vec<GuestRef>, String> {
        let sr = crate::xapi::twinstor_sr_uuid()
            .await
            .ok_or("twinstor SR uuid unknown")?;
        crate::xapi::local_guests_on_sr(&sr)
            .await
            .ok_or_else(|| "XAPI did not answer".to_string())
    }

    async fn disable_local_host(&self) -> Result<bool, String> {
        // Only a host that is enabled now is disabled (and so may be enabled
        // again by the boundary); an operator's disable is left alone.
        match crate::xapi::is_host_enabled().await {
            Some(false) => Ok(false),
            None => Err("could not read whether this host is enabled".into()),
            Some(true) if crate::xapi::disable_local_host().await => Ok(true),
            Some(true) => Err("xe host-disable failed".into()),
        }
    }

    async fn enable_local_host(&self) -> Result<(), String> {
        if crate::xapi::enable_local_host().await {
            Ok(())
        } else {
            Err("xe host-enable failed".into())
        }
    }

    async fn destroy_domains(&self, doms: &[(u32, String)]) {
        // One bounded `xl destroy` per domain, all in flight at once: the
        // slowest domain bounds the round, not the sum.
        let mut set = tokio::task::JoinSet::new();
        for (domid, _) in doms {
            let domid = domid.to_string();
            set.spawn(async move {
                let _ = crate::cmd::run_cmd_for("xl", &["destroy", &domid], XL_DESTROY_BOUND_SECS)
                    .await;
            });
        }
        while set.join_next().await.is_some() {}
    }

    async fn poweroff_domains(&self, doms: &[(u32, String)]) -> Result<(), PoweroffError> {
        let mut args: Vec<String> = vec!["-c".into(), POWEROFF_PY.into()];
        args.extend(doms.iter().map(|(d, _)| d.to_string()));
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let (ok, out, err) = crate::cmd::cmd_status_for("python3", &argv, POWEROFF_BOUND_SECS)
            .await
            .map_err(|e| PoweroffError::Unknown(format!("python3 failed: {e}")))?;
        match parse_poweroff_output(ok, &out) {
            PoweroffReport::Unavailable => Err(PoweroffError::Unavailable(format!(
                "no usable xen.lowlevel.xc binding: {} {}",
                out.trim(),
                err.trim()
            ))),
            PoweroffReport::Done { failed } => {
                if !failed.is_empty() {
                    warn!(heal_event = "loser_crash_boundary", step = "poweroff", failed = %failed.join(", "),
                          "the hypervisor refused to power off some domains");
                }
                Ok(())
            }
        }
    }

    async fn present_domids(&self) -> Result<Vec<u32>, String> {
        let out = crate::cmd::cmd_output_for("xl", &["list", "-v"], LOCAL_READ_BOUND_SECS)
            .await
            .map_err(|e| format!("xl list -v failed: {e}"))?;
        parse_xl_all_domids(&out).ok_or_else(|| "xl list -v output not understood".to_string())
    }

    fn request_xapi_hard_shutdown(&self, guests: &[GuestRef]) {
        for g in guests {
            let uuid = g.uuid.clone();
            // Detached and bounded: the boundary never waits on XAPI.
            tokio::spawn(async move {
                let _ = crate::xapi::hard_shutdown_vm(&uuid).await;
            });
        }
    }

    async fn local_scan(&self) -> Result<LocalScan, String> {
        let sr = crate::xapi::twinstor_sr_uuid()
            .await
            .ok_or("twinstor SR uuid unknown")?;
        let all = Self::xl_verbose().await?;
        if all.is_empty() {
            return Ok(LocalScan::default());
        }
        let xs = crate::cmd::cmd_output_for(
            "xenstore-ls",
            &["-f", "/local/domain/0/backend"],
            LOCAL_READ_BOUND_SECS,
        )
        .await
        .map_err(|e| format!("xenstore-ls failed: {e}"))?;
        let ids = sr_domids_from_xenstore(&xs, &sr);
        let on_sr = all
            .iter()
            .filter(|(d, _)| ids.contains(d))
            .cloned()
            .collect();
        Ok(LocalScan { all, on_sr })
    }

    async fn leftover_tapdisks(&self) -> Result<Vec<Tapdisk>, String> {
        let sr = crate::xapi::twinstor_sr_uuid()
            .await
            .ok_or("twinstor SR uuid unknown")?;
        let taps = tap_ctl_list_settled().await?;
        if !taps.iter().any(|t| sr_vdi_of_path(&t.path, &sr).is_some()) {
            return Ok(Vec::new());
        }
        let live: Vec<u32> = Self::xl_verbose()
            .await?
            .into_iter()
            .map(|(d, _)| d)
            .collect();
        let xs = crate::cmd::cmd_output_for(
            "xenstore-ls",
            &["-f", "/local/domain/0/backend"],
            LOCAL_READ_BOUND_SECS,
        )
        .await
        .map_err(|e| format!("xenstore-ls failed: {e}"))?;
        Ok(leftover_sr_tapdisks(
            &taps,
            &sr,
            &live_sr_vdis(&xs, &sr, &live),
        ))
    }

    async fn destroy_tapdisk(&self, t: &Tapdisk) -> Result<(), String> {
        let (pid, minor) = (t.pid.to_string(), t.minor.to_string());
        crate::cmd::run_cmd_for(
            "tap-ctl",
            &["destroy", "-p", &pid, "-m", &minor, "-t", "10"],
            TAP_DESTROY_BOUND_SECS,
        )
        .await
        .map(|_| ())
        .map_err(|e| format!("tap-ctl destroy failed: {e}"))
    }

    async fn destroy_tapdisks(&self, taps: &[Tapdisk]) -> Vec<Result<(), String>> {
        let mut set = tokio::task::JoinSet::new();
        for (i, t) in taps.iter().enumerate() {
            let (pid, minor) = (t.pid.to_string(), t.minor.to_string());
            set.spawn(async move {
                let r = crate::cmd::run_cmd_for(
                    "tap-ctl",
                    &["destroy", "-p", &pid, "-m", &minor, "-t", "10"],
                    TAP_DESTROY_BOUND_SECS,
                )
                .await
                .map(|_| ())
                .map_err(|e| format!("tap-ctl destroy failed: {e}"));
                (i, r)
            });
        }
        let mut out: Vec<Result<(), String>> = vec![Err("not attempted".to_string()); taps.len()];
        while let Some(done) = set.join_next().await {
            if let Ok((i, r)) = done {
                out[i] = r;
            }
        }
        out
    }

    async fn lift_io_suspend(&self) -> Result<bool, String> {
        use crate::drbd::DrbdControl;
        let st = self
            .drbd
            .status()
            .await
            .map_err(|e| format!("drbdadm status failed: {e}"))?;
        if !st.suspended_user && !st.suspended_fencing {
            return Ok(false);
        }
        self.drbd
            .resume_io()
            .await
            .map_err(|e| format!("drbdadm resume-io failed: {e}"))?;
        Ok(true)
    }

    async fn io_suspended(&self) -> Option<bool> {
        use crate::drbd::DrbdControl;
        let st = self.drbd.status().await.ok()?;
        Some(st.suspended_user || st.suspended_fencing)
    }

    async fn suspend_io(&self) -> Result<(), String> {
        use crate::drbd::DrbdControl;
        self.drbd
            .suspend_io()
            .await
            .map_err(|e| format!("drbdadm suspend-io failed: {e}"))
    }

    async fn local_path_active(&self) -> Option<bool> {
        let local = crate::multipath::local_ip_toward(&self.peer_ip).await?;
        crate::multipath::probe(&local, &self.peer_ip)
            .await
            .local_path_active
    }

    async fn hard_shutdown_here(&self, gs: &[GuestRef]) -> Vec<Result<(), String>> {
        let Some(this) = crate::xapi::local_host_uuid().await else {
            return vec![Err("XAPI cannot name this host".to_string()); gs.len()];
        };
        let mut set = tokio::task::JoinSet::new();
        for (i, g) in gs.iter().enumerate() {
            let (uuid, this) = (g.uuid.clone(), this.clone());
            set.spawn(async move {
                let r = match crate::xapi::vm_resident_on(&uuid).await {
                    Some(h) if h == this => {
                        if crate::xapi::hard_shutdown_vm(&uuid).await {
                            Ok(())
                        } else {
                            Err("xe vm-shutdown --force failed or timed out".to_string())
                        }
                    }
                    Some(h) => Err(format!("XAPI places it on host {h}, not this one")),
                    None => Err("XAPI does not place it on this host".to_string()),
                };
                (i, r)
            });
        }
        let mut out: Vec<Result<(), String>> = vec![Err("not attempted".to_string()); gs.len()];
        while let Some(done) = set.join_next().await {
            if let Ok((i, r)) = done {
                out[i] = r;
            }
        }
        out
    }

    async fn datapaths(&self, vdis: Option<&[String]>) -> Result<DpState, String> {
        let sr = crate::xapi::twinstor_sr_uuid()
            .await
            .ok_or("twinstor SR uuid unknown")?;
        let vdis: Vec<String> = match vdis {
            Some(v) => v.to_vec(),
            None => {
                let report = crate::xapi_storage::diagnostics().await?;
                let mut all = crate::xapi_storage::diagnostics_vdis(&report, &sr)
                    .ok_or("the storage layer's DP.diagnostics report is not understood")?;
                for t in tap_ctl_list_settled().await? {
                    if let Some(v) = sr_vdi_of_path(&t.path, &sr) {
                        push_vdi(&mut all, &v);
                    }
                }
                all
            }
        };
        if vdis.is_empty() {
            return Ok(DpState::default());
        }
        let mut held = Vec::new();
        for v in &vdis {
            held.push((v.clone(), crate::xapi_storage::stat_vdi(&sr, v).await?));
        }
        // Domains are read after the datapaths: a datapath only ever exists
        // once its domain does, so one created meanwhile is seen live here and
        // never mistaken for stale.
        let out = crate::cmd::cmd_output_for("xl", &["list", "-v"], LOCAL_READ_BOUND_SECS)
            .await
            .map_err(|e| format!("xl list -v failed: {e}"))?;
        let present = parse_xl_all_domids(&out).ok_or("xl list -v output not understood")?;
        let running: Vec<u32> = parse_xl_verbose(&out)
            .ok_or("xl list -v output not understood")?
            .into_iter()
            .map(|(d, _)| d)
            .collect();
        let mut st = DpState::default();
        for (vdi, dps) in held {
            classify_datapaths(&vdi, &dps, &running, &present, &mut st);
        }
        Ok(st)
    }

    async fn destroy_datapaths(&self, dps: &[Datapath]) -> Vec<Result<(), String>> {
        // One task per VDI (the storage layer serializes a VDI's operations
        // anyway), all VDIs at once; each call is bounded.
        let mut groups: Vec<(String, Vec<(usize, String)>)> = Vec::new();
        for (i, d) in dps.iter().enumerate() {
            match groups.iter_mut().find(|(v, _)| same_uuid(v, &d.vdi)) {
                Some(g) => g.1.push((i, d.dp.clone())),
                None => groups.push((d.vdi.clone(), vec![(i, d.dp.clone())])),
            }
        }
        let mut set = tokio::task::JoinSet::new();
        for (_, items) in groups {
            set.spawn(async move {
                let mut out = Vec::new();
                for (i, dp) in items {
                    out.push((i, crate::xapi_storage::destroy_dp(&dp).await));
                }
                out
            });
        }
        let mut results: Vec<Result<(), String>> =
            vec![Err("not attempted".to_string()); dps.len()];
        while let Some(done) = set.join_next().await {
            if let Ok(items) = done {
                for (i, r) in items {
                    results[i] = r;
                }
            }
        }
        results
    }

    async fn guest_sr_vdis(&self, g: &GuestRef) -> Result<Vec<String>, String> {
        let sr = crate::xapi::twinstor_sr_uuid()
            .await
            .ok_or("twinstor SR uuid unknown")?;
        crate::xapi::guest_vdis_on_sr(&g.uuid, &sr)
            .await
            .ok_or_else(|| "XAPI did not answer".to_string())
    }

    async fn host_uuids(&self) -> Option<(String, String)> {
        let this = crate::xapi::local_host_uuid().await?;
        let peer = crate::xapi::peer_host_uuid(&this).await?;
        Some((this, peer))
    }

    async fn start_guest(&self, g: &GuestRef, target_host: &str, this_host: &str) -> StartOutcome {
        // Never trust "running" on the host the boundary stopped it on unless a
        // domain really exists there (an xl-destroyed domain can leave a stale
        // running record behind), and never start a guest that still has a
        // live local domain, whatever XAPI says.
        let local_domain = || async {
            let out = crate::cmd::cmd_output_for("xl", &["list", "-v"], LOCAL_READ_BOUND_SECS)
                .await
                .map_err(|e| format!("xl list -v failed: {e}"))?;
            local_domain_state(&out, &g.uuid)
                .ok_or_else(|| "xl list -v output not understood".to_string())
        };
        let state = || async {
            let local = local_domain().await?;
            let power = crate::xapi::vm_power_state(&g.uuid).await;
            let resident = if power.as_deref().is_some_and(xapi_state_is_live) {
                crate::xapi::vm_resident_on(&g.uuid).await
            } else {
                None
            };
            Ok::<_, String>(
                match classify_record(local, power.as_deref(), resident.as_deref(), this_host) {
                    RecordView::LocalDomain => Some(StartOutcome::RunningHere),
                    RecordView::LocalPaused => Some(StartOutcome::PausedHere),
                    RecordView::Elsewhere => Some(StartOutcome::AlreadyRunning),
                    RecordView::StaleHere => Some(StartOutcome::Stale),
                    RecordView::Startable => None,
                    RecordView::Unknown(e) => return Err(e),
                },
            )
        };
        match state().await {
            Ok(Some(StartOutcome::Stale)) => {
                // Let a crash action that is still rebuilding the guest here
                // finish (or fail) before calling the record stale.
                tokio::time::sleep(STALE_SETTLE).await;
                match state().await {
                    Ok(Some(o)) => return o,
                    Ok(None) => {}
                    Err(e) => return StartOutcome::Failed(e),
                }
            }
            Ok(Some(o)) => return o,
            Ok(None) => {}
            Err(e) => return StartOutcome::Failed(e),
        }
        let why = match crate::xapi::start_vm_on(&g.uuid, target_host).await {
            Ok(()) => return StartOutcome::Started,
            Err(why) => why,
        };
        match state().await {
            Ok(Some(o)) => o,
            Ok(None) if target_host != this_host && is_vdi_held_error(&why) => {
                StartOutcome::VdiHeldHere(format!("vm-start on host {target_host} failed: {why}"))
            }
            Ok(None) => {
                StartOutcome::Failed(format!("vm-start on host {target_host} failed: {why}"))
            }
            Err(e) => StartOutcome::Failed(e),
        }
    }

    async fn reconcile_stale(
        &self,
        g: &GuestRef,
        this_host: &str,
        agent_restart_allowed: bool,
    ) -> Reconciled {
        // Still stale? A live local domain, or a placement on another host,
        // means there is nothing to reconcile and nothing may be touched.
        let still_stale = || async {
            match Self::xl_verbose().await {
                Ok(doms) if doms.iter().any(|(_, u)| same_uuid(u, &g.uuid)) => return Some(false),
                Ok(_) => {}
                Err(_) => return None,
            }
            let power = crate::xapi::vm_power_state(&g.uuid).await;
            if !power.as_deref().is_some_and(xapi_state_is_live) {
                return Some(false);
            }
            match crate::xapi::vm_resident_on(&g.uuid).await {
                Some(h) if h == this_host => Some(true),
                _ => Some(false),
            }
        };
        let mut done = Vec::new();
        let mut agent_restarted = false;
        let master = crate::xapi::is_pool_master_resilient().await;
        for step in stale_reconcile_plan(master, agent_restart_allowed) {
            match still_stale().await {
                Some(true) => {}
                Some(false) => {
                    return Reconciled {
                        cleared: true,
                        agent_restarted,
                        detail: done.join(", "),
                    };
                }
                None => {
                    done.push("local domains unreadable: stopped".to_string());
                    return Reconciled {
                        cleared: false,
                        agent_restarted,
                        detail: done.join(", "),
                    };
                }
            }
            let ok = match step {
                StaleStep::RetryHardShutdown => crate::xapi::hard_shutdown_vm(&g.uuid).await,
                StaleStep::ResetPowerState => crate::xapi::reset_vm_powerstate(&g.uuid).await,
                StaleStep::RestartAgent => {
                    warn!(heal_event = "loser_crash_boundary", step = "restart_xapi", vm = %guest_label(g),
                          "split-brain loser: restarting this host's XAPI agent (xapi only) so it \
                           drops the stale running record of a guest destroyed here");
                    agent_restarted = true;
                    crate::xapi::restart_local_agent_and_wait(std::time::Duration::from_secs(180))
                        .await
                }
            };
            done.push(format!("{step:?}: {}", if ok { "ok" } else { "failed" }));
        }
        let cleared = still_stale().await == Some(false);
        Reconciled {
            cleared,
            agent_restarted,
            detail: done.join(", "),
        }
    }
}

/// LIO's iSCSI fabric directory in configfs.
#[cfg_attr(test, allow(dead_code))]
const LIO_ISCSI_CONFIGFS: &str = "/sys/kernel/config/target/iscsi";

/// Whether target `iqn` is exported under `root` (LIO's iSCSI configfs
/// directory): some `tpgt_*` enabled with at least one LUN. configfs is the
/// kernel's own view, independent of the controller's in-memory adoption flag
/// (a daemon restart leaves that unset). A missing directory at any level
/// (the target never created this boot, the LIO module not loaded, configfs
/// not mounted) means nothing is exported through it: `Some(false)`. Only a
/// read that fails for another reason is unknown (`None`, fail closed).
pub fn target_exported_at(root: &Path, iqn: &str) -> Option<bool> {
    match std::fs::read_dir(root.join(iqn)) {
        Ok(entries) => {
            for e in entries {
                let e = e.ok()?;
                if !e.file_name().to_string_lossy().starts_with("tpgt_") {
                    continue;
                }
                let enabled = std::fs::read_to_string(e.path().join("enable")).ok()?;
                let has_lun = std::fs::read_dir(e.path().join("lun"))
                    .ok()?
                    .next()
                    .is_some();
                if enabled.trim() == "1" && has_lun {
                    return Some(true);
                }
            }
            Some(false)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
}

/// Test double: scripted answers, records every call.
#[cfg(test)]
pub mod fake {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct FakeGuests {
        pub exported: Option<bool>,
        /// Successive `sr_guests` answers (the last one repeats).
        pub listings: Mutex<Vec<Result<Vec<GuestRef>, String>>>,
        pub disable_fails: bool,
        /// The host is already disabled (by an operator) before the boundary.
        pub already_disabled: bool,
        pub enable_fails: bool,
        /// Domains running on this host with a disk on the SR (uuid), and the
        /// ones without (all others). Stopped guests leave `running`.
        pub running: Mutex<Vec<String>>,
        pub running_off_sr: Vec<String>,
        /// Guests whose domain survives a stop.
        pub unkillable: Vec<String>,
        /// The scan cannot be read.
        pub scan_fails: bool,
        /// The scan sees these domains but not their SR disk (format not understood).
        pub scan_blind: Vec<String>,
        pub no_hosts: bool,
        pub start: Mutex<Vec<StartOutcome>>,
        /// Guests whose record stays stale until a reconcile that may restart
        /// the agent (or always, when `stale_sticky`).
        pub stale: Mutex<Vec<String>>,
        pub stale_sticky: bool,
        pub calls: Mutex<Vec<String>>,
        /// XAPI (listing and host disable) answers only after this long.
        pub xapi_delay: Option<std::time::Duration>,
        /// Names for domains XAPI does not list (labels in `stops()`).
        pub extra_names: Vec<GuestRef>,
        /// Called at every stop, so a test can assert what had (not) happened
        /// elsewhere by then.
        pub on_stop: Option<Box<dyn Fn() + Send + Sync>>,
        /// Called when `lift_io_suspend` resumes the I/O (DRBD starts a new
        /// current generation there).
        pub on_lift: Option<Box<dyn Fn() + Send + Sync>>,
        /// Leftover tapdisks on the SR (destroyed ones leave the list).
        pub taps: Mutex<Vec<Tapdisk>>,
        /// These tapdisk pids survive a destroy.
        pub taps_stuck: Vec<u32>,
        /// `tap-ctl list` cannot be read.
        pub taps_fail: bool,
        /// The leftover tapdisks gain their block backend (a guest being
        /// built) once `leftover_tapdisks` has been read this many times.
        pub taps_attached_after_reads: Option<usize>,
        pub tap_reads: Mutex<usize>,
        /// Stale datapaths the storage layer holds. Releasing the last one of
        /// a VDI closes that VDI's tapdisk, as SM's deactivate does.
        pub dps: Mutex<Vec<Datapath>>,
        /// These datapaths cannot be released.
        pub dps_stuck: Vec<String>,
        /// The storage layer cannot be read.
        pub dps_fail: bool,
        /// Datapaths of domains still listed but not running (their teardown
        /// under way); `stopping_clears_after` reads clear them.
        pub stopping_dps: Mutex<Vec<Datapath>>,
        pub stopping_clears_after: Mutex<Option<usize>>,
        /// VDIs a live domain holds through the storage layer (a guest being
        /// built here); `live_clears_at_scan`: they clear once that scan ran.
        pub live_dp_vdis: Mutex<Vec<String>>,
        pub live_clears_at_scan: Option<usize>,
        /// This copy's I/O is suspended (lifted by `lift_io_suspend`).
        pub suspended: Mutex<bool>,
        /// `lift_io_suspend` resumes the I/O and still reports an error (a
        /// timeout after DRBD acted).
        pub lift_errs_after_resume: bool,
        /// `(n, vdis)`: these VDIs gain a live datapath (a guest being built
        /// here) from the n-th storage-layer read on (1-based).
        pub live_from_read: Option<(usize, Vec<String>)>,
        pub dp_reads: Mutex<usize>,
        /// A tapdisk close blocks while the I/O is suspended (it fails).
        pub close_blocks_while_suspended: bool,
        /// The active multipath path is the peer's (`Some(false)`), unknown
        /// (`None`); default: this node's own.
        pub path_active: Option<Option<bool>>,
        /// These guests' XAPI hard-stop fails.
        pub hard_stop_fails: Vec<String>,
        /// Every tapdisk close takes this long.
        pub tap_destroy_delay: Option<std::time::Duration>,
        /// Called at every start attempt (a test rewrites the record there, as
        /// the daemon tick may while the off-tick step awaits).
        pub on_start: Option<Box<dyn Fn() + Send + Sync>>,
        /// The poweroff call fails after stopping these (an ambiguous error).
        pub poweroff_partial: Option<Vec<String>>,
        /// Each guest's VDIs on the SR, as `(guest uuid, VDIs)`; absent means
        /// none.
        pub guest_vdis: Vec<(String, Vec<String>)>,
        /// The hypervisor poweroff binding works (off by default, so tests
        /// take the `xl destroy` path unless they opt in).
        pub xc: bool,
        /// Domains powered off and not yet collected by xenopsd (uuid): no
        /// longer in the scan, still in `present_domids` until the next
        /// collection pass.
        pub shut: Mutex<Vec<String>>,
        /// xenopsd never collects these powered-off domains.
        pub teardown_stuck: Vec<String>,
        /// These domains keep running after the poweroff call.
        pub poweroff_ignored: Vec<String>,
        /// The poweroff call takes this long (a stalled hypervisor call).
        pub poweroff_delay: Option<std::time::Duration>,
        /// `(n, uuid)`: the toolstack rebuilds `uuid` just before the n-th
        /// local scan (1-based) answers, as xenopsd does after a destroy it
        /// reads as a crash.
        pub rebuild_at_scan: Mutex<Vec<(usize, String)>>,
        pub scans: Mutex<usize>,
    }

    impl FakeGuests {
        /// XAPI lists `g` on this host and each has a running domain on the SR.
        pub fn with_guests(g: Vec<GuestRef>) -> Self {
            let running = g.iter().map(|g| g.uuid.clone()).collect();
            Self {
                exported: Some(true),
                listings: Mutex::new(vec![Ok(g)]),
                running: Mutex::new(running),
                ..Default::default()
            }
        }
        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        /// The destroy rounds issued, each as `destroy a,b`.
        pub fn stops(&self) -> Vec<String> {
            self.calls()
                .into_iter()
                .filter(|c| c.starts_with("destroy "))
                .collect()
        }
        fn name_of(&self, uuid: &str) -> String {
            let l = self.listings.lock().unwrap();
            l.iter()
                .flat_map(|r| r.iter().flatten())
                .chain(self.extra_names.iter())
                .find(|g| g.uuid == uuid)
                .map_or_else(|| format!("domid {}", domid(uuid)), |g| g.name.clone())
        }
    }

    fn domid(uuid: &str) -> u32 {
        u32::from_str_radix(&uuid[..8], 16).unwrap_or(99)
    }

    #[async_trait::async_trait]
    impl GuestControl for FakeGuests {
        async fn local_target_exported(&self) -> Option<bool> {
            self.exported
        }
        async fn sr_guests(&self) -> Result<Vec<GuestRef>, String> {
            self.calls.lock().unwrap().push("list".into());
            if let Some(d) = self.xapi_delay {
                tokio::time::sleep(d).await;
            }
            let mut l = self.listings.lock().unwrap();
            if l.len() > 1 {
                l.remove(0)
            } else {
                l.first().cloned().unwrap_or(Ok(Vec::new()))
            }
        }
        async fn disable_local_host(&self) -> Result<bool, String> {
            self.calls.lock().unwrap().push("disable".into());
            if let Some(d) = self.xapi_delay {
                tokio::time::sleep(d).await;
            }
            if self.disable_fails {
                Err("HA_OPERATION_WOULD_BREAK_FAILOVER_PLAN".into())
            } else {
                Ok(!self.already_disabled)
            }
        }
        async fn enable_local_host(&self) -> Result<(), String> {
            self.calls.lock().unwrap().push("enable".into());
            if self.enable_fails {
                Err("xe host-enable failed".into())
            } else {
                Ok(())
            }
        }
        async fn destroy_domains(&self, doms: &[(u32, String)]) {
            let names: Vec<String> = doms.iter().map(|(_, u)| self.name_of(u)).collect();
            self.calls
                .lock()
                .unwrap()
                .push(format!("destroy {}", names.join(",")));
            if let Some(hook) = &self.on_stop {
                hook();
            }
            self.running
                .lock()
                .unwrap()
                .retain(|u| !doms.iter().any(|(_, d)| d == u) || self.unkillable.contains(u));
            self.shut
                .lock()
                .unwrap()
                .retain(|u| !doms.iter().any(|(_, d)| d == u));
        }
        async fn poweroff_domains(&self, doms: &[(u32, String)]) -> Result<(), PoweroffError> {
            if let Some(partial) = &self.poweroff_partial {
                self.calls.lock().unwrap().push("poweroff timed out".into());
                let mut running = self.running.lock().unwrap();
                let mut shut = self.shut.lock().unwrap();
                for u in partial {
                    if running.contains(u) {
                        running.retain(|x| x != u);
                        shut.push(u.clone());
                    }
                }
                return Err(PoweroffError::Unknown("python3 failed: timed out".into()));
            }
            if let Some(d) = self.poweroff_delay {
                self.calls.lock().unwrap().push("poweroff slow".into());
                tokio::time::sleep(d).await;
            }
            if !self.xc {
                self.calls
                    .lock()
                    .unwrap()
                    .push("poweroff unavailable".into());
                return Err(PoweroffError::Unavailable(
                    "noxc No module named 'xen'".into(),
                ));
            }
            let names: Vec<String> = doms.iter().map(|(_, u)| self.name_of(u)).collect();
            self.calls
                .lock()
                .unwrap()
                .push(format!("poweroff {}", names.join(",")));
            if let Some(hook) = &self.on_stop {
                hook();
            }
            let mut running = self.running.lock().unwrap();
            let mut shut = self.shut.lock().unwrap();
            for (_, u) in doms {
                if running.contains(u) && !self.poweroff_ignored.contains(u) {
                    running.retain(|x| x != u);
                    shut.push(u.clone());
                }
            }
            Ok(())
        }
        async fn present_domids(&self) -> Result<Vec<u32>, String> {
            self.calls.lock().unwrap().push("present".into());
            let mut shut = self.shut.lock().unwrap();
            let mut ids: Vec<u32> = self
                .running
                .lock()
                .unwrap()
                .iter()
                .map(|u| domid(u))
                .collect();
            ids.extend(shut.iter().map(|u| domid(u)));
            // xenopsd collects the non-stuck ones between two reads.
            shut.retain(|u| self.teardown_stuck.contains(u));
            Ok(ids)
        }
        fn request_xapi_hard_shutdown(&self, guests: &[GuestRef]) {
            let names: Vec<String> = guests.iter().map(|g| g.name.clone()).collect();
            self.calls
                .lock()
                .unwrap()
                .push(format!("xapi-shutdown {}", names.join(",")));
        }
        async fn local_scan(&self) -> Result<LocalScan, String> {
            self.calls.lock().unwrap().push("scan".into());
            let n = {
                let mut c = self.scans.lock().unwrap();
                *c += 1;
                *c
            };
            for (_, u) in self
                .rebuild_at_scan
                .lock()
                .unwrap()
                .iter()
                .filter(|(k, _)| *k == n)
            {
                self.running.lock().unwrap().push(u.clone());
            }
            if self.scan_fails {
                return Err("xl unreadable".into());
            }
            let running = self.running.lock().unwrap().clone();
            let mut all: Vec<(u32, String)> =
                running.iter().map(|u| (domid(u), u.clone())).collect();
            all.extend(self.running_off_sr.iter().map(|u| (domid(u), u.clone())));
            let on_sr = running
                .iter()
                .filter(|u| !self.scan_blind.contains(u))
                .map(|u| (domid(u), u.clone()))
                .collect();
            Ok(LocalScan { all, on_sr })
        }
        async fn leftover_tapdisks(&self) -> Result<Vec<Tapdisk>, String> {
            self.calls.lock().unwrap().push("taps".into());
            if self.taps_fail {
                return Err("tap-ctl unreadable".into());
            }
            let n = {
                let mut r = self.tap_reads.lock().unwrap();
                *r += 1;
                *r
            };
            if self.taps_attached_after_reads.is_some_and(|k| n > k) {
                return Ok(Vec::new());
            }
            Ok(self.taps.lock().unwrap().clone())
        }
        async fn destroy_tapdisk(&self, t: &Tapdisk) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("tap-destroy {}", t.pid));
            if let Some(d) = self.tap_destroy_delay {
                tokio::time::sleep(d).await;
            }
            if self.taps_stuck.contains(&t.pid)
                || (self.close_blocks_while_suspended && *self.suspended.lock().unwrap())
            {
                return Err("tap-ctl destroy failed".into());
            }
            self.taps.lock().unwrap().retain(|x| x.pid != t.pid);
            Ok(())
        }
        async fn datapaths(&self, vdis: Option<&[String]>) -> Result<DpState, String> {
            self.calls.lock().unwrap().push(match vdis {
                None => "dps".to_string(),
                Some(v) => format!("dps {}", v.join(",")),
            });
            if self.dps_fail {
                return Err("message-cli: connection refused".into());
            }
            {
                let mut n = self.dp_reads.lock().unwrap();
                *n += 1;
                if let Some((at, v)) = &self.live_from_read
                    && *n == *at
                {
                    *self.live_dp_vdis.lock().unwrap() = v.clone();
                }
            }
            {
                let mut c = self.stopping_clears_after.lock().unwrap();
                if let Some(n) = c.as_mut() {
                    if *n == 0 {
                        self.stopping_dps.lock().unwrap().clear();
                    } else {
                        *n -= 1;
                    }
                }
            }
            if self
                .live_clears_at_scan
                .is_some_and(|n| *self.scans.lock().unwrap() >= n)
            {
                self.live_dp_vdis.lock().unwrap().clear();
            }
            let pick = |v: &str| vdis.is_none_or(|l| l.iter().any(|x| same_uuid(x, v)));
            let live: Vec<String> = self
                .live_dp_vdis
                .lock()
                .unwrap()
                .iter()
                .filter(|v| pick(v))
                .cloned()
                .collect();
            Ok(DpState {
                stale: self
                    .dps
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|d| pick(&d.vdi))
                    .cloned()
                    .collect(),
                stopping: self
                    .stopping_dps
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|d| pick(&d.vdi))
                    .cloned()
                    .collect(),
                guest_live_vdis: live.clone(),
                live_vdis: live,
            })
        }
        async fn lift_io_suspend(&self) -> Result<bool, String> {
            let mut s = self.suspended.lock().unwrap();
            if self.lift_errs_after_resume {
                *s = false;
                self.calls.lock().unwrap().push("resume-io".into());
                return Err("drbdadm resume-io: timed out".into());
            }
            if *s {
                *s = false;
                self.calls.lock().unwrap().push("resume-io".into());
                if let Some(hook) = &self.on_lift {
                    hook();
                }
                Ok(true)
            } else {
                Ok(false)
            }
        }
        async fn io_suspended(&self) -> Option<bool> {
            Some(*self.suspended.lock().unwrap())
        }
        async fn suspend_io(&self) -> Result<(), String> {
            *self.suspended.lock().unwrap() = true;
            self.calls.lock().unwrap().push("suspend-io".into());
            Ok(())
        }
        async fn local_path_active(&self) -> Option<bool> {
            self.path_active.unwrap_or(Some(true))
        }
        async fn hard_shutdown_here(&self, gs: &[GuestRef]) -> Vec<Result<(), String>> {
            if let Some(d) = self.xapi_delay {
                tokio::time::sleep(d).await;
            }
            let mut out = Vec::new();
            for g in gs {
                let name = self.name_of(&g.uuid);
                self.calls.lock().unwrap().push(format!("xapi-stop {name}"));
                if self.hard_stop_fails.contains(&g.uuid) {
                    out.push(Err("xe vm-shutdown --force failed".into()));
                    continue;
                }
                self.running
                    .lock()
                    .unwrap()
                    .retain(|u| u != &g.uuid || self.unkillable.contains(u));
                out.push(Ok(()));
            }
            out
        }
        async fn destroy_datapaths(&self, dps: &[Datapath]) -> Vec<Result<(), String>> {
            let mut out = Vec::new();
            for d in dps {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("dp-destroy {}", d.dp));
                if self.dps_stuck.contains(&d.dp) {
                    out.push(Err("Backend_error: SR_BACKEND_FAILURE_46".into()));
                    continue;
                }
                let mut held = self.dps.lock().unwrap();
                held.retain(|x| x != d);
                if !held.iter().any(|x| same_uuid(&x.vdi, &d.vdi)) {
                    self.taps
                        .lock()
                        .unwrap()
                        .retain(|t| vdi_in_path(&t.path).is_none_or(|v| !same_uuid(&v, &d.vdi)));
                }
                out.push(Ok(()));
            }
            out
        }
        async fn guest_sr_vdis(&self, g: &GuestRef) -> Result<Vec<String>, String> {
            Ok(self
                .guest_vdis
                .iter()
                .find(|(u, _)| u == &g.uuid)
                .map(|(_, v)| v.clone())
                .unwrap_or_default())
        }
        async fn host_uuids(&self) -> Option<(String, String)> {
            (!self.no_hosts).then(|| ("this".to_string(), "peer".to_string()))
        }
        async fn start_guest(
            &self,
            g: &GuestRef,
            target_host: &str,
            _this_host: &str,
        ) -> StartOutcome {
            self.calls
                .lock()
                .unwrap()
                .push(format!("start {} on {target_host}", g.name));
            if let Some(hook) = &self.on_start {
                hook();
            }
            if self.stale.lock().unwrap().contains(&g.uuid) {
                return StartOutcome::Stale;
            }
            let mut s = self.start.lock().unwrap();
            if s.len() > 1 {
                s.remove(0)
            } else {
                s.first().cloned().unwrap_or(StartOutcome::Started)
            }
        }
        async fn reconcile_stale(
            &self,
            g: &GuestRef,
            _this_host: &str,
            agent_restart_allowed: bool,
        ) -> Reconciled {
            self.calls.lock().unwrap().push(format!(
                "reconcile {} agent_allowed={agent_restart_allowed}",
                g.name
            ));
            let cleared = agent_restart_allowed && !self.stale_sticky;
            if cleared {
                self.stale.lock().unwrap().retain(|u| u != &g.uuid);
            }
            Reconciled {
                cleared,
                agent_restarted: agent_restart_allowed,
                detail: "fake".into(),
            }
        }
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl GuestControl for std::sync::Arc<fake::FakeGuests> {
    async fn local_target_exported(&self) -> Option<bool> {
        self.as_ref().local_target_exported().await
    }
    async fn sr_guests(&self) -> Result<Vec<GuestRef>, String> {
        self.as_ref().sr_guests().await
    }
    async fn disable_local_host(&self) -> Result<bool, String> {
        self.as_ref().disable_local_host().await
    }
    async fn enable_local_host(&self) -> Result<(), String> {
        self.as_ref().enable_local_host().await
    }
    async fn destroy_domains(&self, doms: &[(u32, String)]) {
        self.as_ref().destroy_domains(doms).await
    }
    async fn poweroff_domains(&self, doms: &[(u32, String)]) -> Result<(), PoweroffError> {
        self.as_ref().poweroff_domains(doms).await
    }
    async fn present_domids(&self) -> Result<Vec<u32>, String> {
        self.as_ref().present_domids().await
    }
    fn request_xapi_hard_shutdown(&self, guests: &[GuestRef]) {
        self.as_ref().request_xapi_hard_shutdown(guests)
    }
    async fn local_scan(&self) -> Result<LocalScan, String> {
        self.as_ref().local_scan().await
    }
    async fn leftover_tapdisks(&self) -> Result<Vec<Tapdisk>, String> {
        self.as_ref().leftover_tapdisks().await
    }
    async fn destroy_tapdisk(&self, t: &Tapdisk) -> Result<(), String> {
        self.as_ref().destroy_tapdisk(t).await
    }
    async fn datapaths(&self, vdis: Option<&[String]>) -> Result<DpState, String> {
        self.as_ref().datapaths(vdis).await
    }
    async fn lift_io_suspend(&self) -> Result<bool, String> {
        self.as_ref().lift_io_suspend().await
    }
    async fn io_suspended(&self) -> Option<bool> {
        self.as_ref().io_suspended().await
    }
    async fn suspend_io(&self) -> Result<(), String> {
        self.as_ref().suspend_io().await
    }
    async fn local_path_active(&self) -> Option<bool> {
        self.as_ref().local_path_active().await
    }
    async fn hard_shutdown_here(&self, gs: &[GuestRef]) -> Vec<Result<(), String>> {
        self.as_ref().hard_shutdown_here(gs).await
    }
    async fn destroy_datapaths(&self, dps: &[Datapath]) -> Vec<Result<(), String>> {
        self.as_ref().destroy_datapaths(dps).await
    }
    async fn guest_sr_vdis(&self, g: &GuestRef) -> Result<Vec<String>, String> {
        self.as_ref().guest_sr_vdis(g).await
    }
    async fn host_uuids(&self) -> Option<(String, String)> {
        self.as_ref().host_uuids().await
    }
    async fn start_guest(&self, g: &GuestRef, target_host: &str, this_host: &str) -> StartOutcome {
        self.as_ref().start_guest(g, target_host, this_host).await
    }
    async fn reconcile_stale(
        &self,
        g: &GuestRef,
        this_host: &str,
        agent_restart_allowed: bool,
    ) -> Reconciled {
        self.as_ref()
            .reconcile_stale(g, this_host, agent_restart_allowed)
            .await
    }
}

#[cfg(test)]
mod tests {
    /// Verbatim `xl list -v` from an XCP-ng 8.3 dom0 (Xen 4.17): guest names
    /// with spaces, tab-separated trailing columns.
    #[test]
    fn parses_real_dom0_xl_list_verbose() {
        let out = "Name                                        ID   Mem VCPUs\tState\tTime(s)   UUID                            Reason-Code\tSecurity Label\n\
Domain-0                                     0  1424     2     r-----     284.8 5e5e0009-0000-4000-8000-000000000009        -                -\n\
Test 4                                       1  1023     1     -b----      16.6 5e5e0010-0000-4000-8000-000000000010        -                -\n\
Test 1                                       2  1023     1     -b----      15.9 5e5e000b-0000-4000-8000-00000000000b        -                -\n";
        assert_eq!(
            parse_xl_verbose(out),
            Some(vec![
                (1, "5e5e0010-0000-4000-8000-000000000010".to_string()),
                (2, "5e5e000b-0000-4000-8000-00000000000b".to_string()),
            ])
        );
    }

    #[test]
    fn xl_parse_skips_domains_that_run_no_code_and_rejects_odd_state() {
        let out = "Name ID Mem VCPUs\tState\tTime(s) UUID Reason-Code\tSecurity Label\n\
web 1 1023 1 -b---- 16.6 5e5e0010-0000-4000-8000-000000000010 - -\n\
(null) 2 0 1 -----d 15.9 5e5e000b-0000-4000-8000-00000000000b - -\n\
db 3 512 1 ---s-- 1.0 11111111-0e92-8ef9-e8bc-c04ab12160da - -\n";
        assert_eq!(
            parse_xl_verbose(out),
            Some(vec![(
                1,
                "5e5e0010-0000-4000-8000-000000000010".to_string()
            )])
        );
        let odd = "Name ID Mem VCPUs State Time(s) UUID\nweb 1 1023 1 ?b???? 16.6 5e5e0010-0000-4000-8000-000000000010\n";
        assert_eq!(
            parse_xl_verbose(odd),
            None,
            "an unknown State column is not understood"
        );
    }

    use super::fake::FakeGuests;
    use super::*;

    fn g(n: u8, name: &str) -> GuestRef {
        GuestRef {
            uuid: format!("{n:08x}-0000-0000-0000-000000000000"),
            name: name.to_string(),
        }
    }

    fn store(tag: &str) -> Store {
        let p = std::env::temp_dir().join(format!("twinstor-test-lb-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        Store::new(p)
    }

    fn rec(phase: Phase, early: bool, owed: Vec<GuestRef>) -> Record {
        Record {
            key: "K".into(),
            phase,
            early_restart: early,
            open: true,
            agent_restarted: false,
            host_disabled: false,
            owed: owed.into_iter().map(Owed::new).collect(),
            leaked: Vec::new(),
        }
    }

    /// One establishing round always ends in Clear or Hold.
    async fn run_to_end(ctl: &FakeGuests, s: &Store, key: &str) -> BoundaryOutcome {
        establish(ctl, s, key).await
    }

    #[test]
    fn required_only_for_possible_divergence_on_exported_target() {
        use ForkEvidence::*;
        // A yielded loser's guests may hold frozen in-flight writes that the
        // discard's resume would acknowledge on the doomed copy.
        for ev in [Diverged, Unknown, Yielded] {
            assert!(boundary_required(ev, Some(true)));
            assert!(
                boundary_required(ev, None),
                "unreadable export state fails closed"
            );
            assert!(
                !boundary_required(ev, Some(false)),
                "a never-exported bring-up loser served nobody"
            );
        }
        for ev in [NotDiverged, LocalInvalid] {
            assert!(!boundary_required(ev, Some(true)));
            assert!(!boundary_required(ev, None));
        }
    }

    #[test]
    fn a_target_never_created_this_boot_is_not_exported() {
        let base = std::env::temp_dir().join(format!("twinstor-lio-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let iqn = "iqn.2026-01.example:twinstor";
        // LIO module not loaded / configfs not mounted: no fabric directory.
        assert_eq!(target_exported_at(&base.join("absent"), iqn), Some(false));
        // Fabric present, this target never created.
        std::fs::create_dir_all(&base).unwrap();
        assert_eq!(target_exported_at(&base, iqn), Some(false));
        // Created, TPG disabled: not exported.
        let tpg = base.join(iqn).join("tpgt_1");
        std::fs::create_dir_all(tpg.join("lun").join("lun_0")).unwrap();
        std::fs::write(tpg.join("enable"), "0\n").unwrap();
        assert_eq!(target_exported_at(&base, iqn), Some(false));
        // Enabled with a LUN: exported.
        std::fs::write(tpg.join("enable"), "1\n").unwrap();
        assert_eq!(target_exported_at(&base, iqn), Some(true));
        // A TPG whose state cannot be read: unknown, never "not exported".
        std::fs::remove_file(tpg.join("enable")).unwrap();
        assert_eq!(target_exported_at(&base, iqn), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn record_round_trips_and_rejects_foreign_content() {
        let mut r = rec(Phase::Stopped, true, vec![g(1, "web 1"), g(2, "db")]);
        r.owed[0].stop_tries = 2;
        r.owed[1].start_tries = 7;
        assert_eq!(Record::parse(&r.render()), Some(r));
        assert_eq!(Record::parse("key=x\nphase=bogus\nearly=0\n"), None);
        assert_eq!(
            Record::parse("key=x\nphase=stopped\nearly=0\nvm=not-a-uuid 0 0 x\n"),
            None
        );
        assert_eq!(
            Record::parse(
                "key=x\nphase=stopped\nearly=0\nvm=00000001-0000-0000-0000-000000000000 x 0 a\n"
            ),
            None
        );
        assert_eq!(Record::parse("key=x\nphase=stopped\nearly=2\n"), None);
        assert_eq!(
            Record::parse("key=x\nphase=stopped\nearly=0\nextra=1\n"),
            None
        );
        assert_eq!(Record::parse("phase=stopped\nearly=0\n"), None);
        let mut r = rec(Phase::Discarding, false, vec![g(3, "x")]);
        r.open = false;
        r.agent_restarted = true;
        assert_eq!(Record::parse(&r.render()), Some(r));
        assert_eq!(
            Record::parse("key=x\nphase=stopped\nearly=0\nopen=2\n"),
            None
        );
    }

    #[test]
    fn the_boundary_disable_is_recorded_and_defaults_off_in_older_records() {
        let mut r = rec(Phase::Discarding, false, vec![g(1, "a")]);
        r.host_disabled = true;
        assert_eq!(Record::parse(&r.render()), Some(r));
        let old = Record::parse("key=x\nphase=stopped\nearly=0\nopen=1\nagent=0\n").unwrap();
        assert!(
            !old.host_disabled,
            "a record written before the field never claims the disable"
        );
    }

    #[test]
    fn a_disk_held_by_another_host_is_told_apart_from_other_start_failures() {
        assert!(is_vdi_held_error("Error code: SR_BACKEND_FAILURE_46"));
        assert!(is_vdi_held_error("VDI_IN_USE: OpaqueRef:1"));
        assert!(!is_vdi_held_error("HOST_DISABLED"));
        assert!(!is_vdi_held_error("SR_BACKEND_FAILURE_47"));
    }

    #[tokio::test]
    async fn a_boundary_disable_is_recorded_only_when_it_disabled_an_enabled_host() {
        let s = store("disable-recorded");
        let ctl = FakeGuests::with_guests(vec![g(1, "a")]);
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        assert!(s.load().unwrap().unwrap().host_disabled);
        s.clear();
        let mut ctl = FakeGuests::with_guests(vec![g(1, "a")]);
        ctl.already_disabled = true;
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        assert!(
            !s.load().unwrap().unwrap().host_disabled,
            "an operator's disable is never claimed"
        );
        s.clear();
    }

    #[tokio::test]
    async fn a_disk_still_held_here_is_started_here_once_this_node_has_rejoined() {
        // A local destroy can leave the loser's tapdisks and VDI locks, so
        // the winner's start keeps failing with SR_BACKEND_FAILURE_46.
        let held = || {
            StartOutcome::VdiHeldHere(
                "vm-start on host peer failed: Error code: SR_BACKEND_FAILURE_46".into(),
            )
        };
        let s = store("held-here");
        let mut r = rec(Phase::Discarding, false, vec![g(1, "a")]);
        r.host_disabled = true;
        s.save(&r).unwrap();
        let ctl = FakeGuests::default();
        *ctl.start.lock().unwrap() = vec![held(), StartOutcome::Started];
        assert!(matches!(
            run_restarts(&ctl, &s, true, false).await,
            RestartTick::Step {
                host: RestartHost::ThisHost,
                outcome: StartOutcome::Started,
                remaining: 0,
                ..
            }
        ));
        assert_eq!(
            ctl.calls(),
            ["start a on peer", "enable", "start a on this"],
            "a guest with no SR disk passes the gate"
        );
        assert!(!s.exists());

        // Not rejoined yet (still StandAlone / discarding): owed, nothing tried here.
        s.save(&r).unwrap();
        let ctl = FakeGuests::default();
        *ctl.start.lock().unwrap() = vec![held()];
        assert!(matches!(
            run_restarts(&ctl, &s, false, false).await,
            RestartTick::Step {
                host: RestartHost::Peer,
                outcome: StartOutcome::Failed(_),
                start_tries: 1,
                ..
            }
        ));
        assert_eq!(ctl.calls(), ["start a on peer"]);
        assert!(
            s.load().unwrap().unwrap().host_disabled,
            "the disable is kept until the start here"
        );

        // An operator's disable (not the boundary's) is never undone.
        let mut r2 = rec(Phase::Discarding, false, vec![g(1, "a")]);
        r2.host_disabled = false;
        s.save(&r2).unwrap();
        let ctl = FakeGuests::default();
        *ctl.start.lock().unwrap() = vec![held(), StartOutcome::Failed("HOST_DISABLED".into())];
        let _ = run_restarts(&ctl, &s, true, false).await;
        assert_eq!(ctl.calls(), ["start a on peer", "start a on this"]);

        // Before the discard (a lineage hold restarts early on the winner) the
        // fallback never applies: this copy is still the diverged one.
        s.save(&rec(Phase::Stopped, true, vec![g(1, "a")])).unwrap();
        let ctl = FakeGuests::default();
        *ctl.start.lock().unwrap() = vec![held()];
        let _ = run_restarts(&ctl, &s, true, false).await;
        assert_eq!(ctl.calls(), ["start a on peer"]);
        s.clear();
    }

    #[test]
    fn local_domain_state_reads_the_state_column() {
        let out = "Name ID Mem VCPUs\tState\tTime(s) UUID Reason-Code\tSecurity Label\n\
web 1 1023 1 -b---- 16.6 5e5e0010-0000-4000-8000-000000000010 - -\n\
db 2 1023 1 --p--- 0.1 5e5e000b-0000-4000-8000-00000000000b - -\n\
old 3 0 1 -----d 9.9 11111111-0e92-8ef9-e8bc-c04ab12160da - -\n";
        assert_eq!(
            local_domain_state(out, "5e5e0010-0000-4000-8000-000000000010"),
            Some(LocalDomain::Live)
        );
        assert_eq!(
            local_domain_state(out, "5e5e000b-0000-4000-8000-00000000000b"),
            Some(LocalDomain::Paused)
        );
        assert_eq!(
            local_domain_state(out, "11111111-0e92-8ef9-e8bc-c04ab12160da"),
            Some(LocalDomain::None)
        );
        assert_eq!(
            local_domain_state(out, "22222222-0e92-8ef9-e8bc-c04ab12160da"),
            Some(LocalDomain::None)
        );
        assert_eq!(
            local_domain_state(
                "h\nweb 1 1 1 ?b 1 5e5e0010-0000-4000-8000-000000000010",
                "x"
            ),
            None
        );
    }

    #[tokio::test]
    async fn a_guest_still_running_here_before_the_discard_stays_owed() {
        // A lineage hold restarts early; a guest the boundary could not stop
        // still has its domain here. Never dropped as "already running".
        let s = store("running-here");
        s.save(&rec(Phase::Stopped, true, vec![g(1, "a")])).unwrap();
        let ctl = FakeGuests::default();
        *ctl.start.lock().unwrap() = vec![StartOutcome::RunningHere];
        assert!(matches!(
            run_restarts(&ctl, &s, false, false).await,
            RestartTick::Step {
                outcome: StartOutcome::Failed(_),
                remaining: 1,
                ..
            }
        ));
        // After the discard, a domain here is a fresh start (HA, an operator).
        s.save(&rec(Phase::Discarding, false, vec![g(1, "a")]))
            .unwrap();
        *ctl.start.lock().unwrap() = vec![StartOutcome::RunningHere];
        assert!(matches!(
            run_restarts(&ctl, &s, false, false).await,
            RestartTick::Step {
                outcome: StartOutcome::AlreadyRunning,
                remaining: 0,
                ..
            }
        ));
        // A paused shell is never "running".
        s.save(&rec(Phase::Discarding, false, vec![g(1, "a")]))
            .unwrap();
        *ctl.start.lock().unwrap() = vec![StartOutcome::PausedHere];
        assert!(matches!(
            run_restarts(&ctl, &s, false, false).await,
            RestartTick::Step {
                outcome: StartOutcome::Failed(_),
                remaining: 1,
                ..
            }
        ));
        s.clear();
    }

    #[tokio::test]
    async fn repeated_failures_on_the_winner_fall_back_here_once_rejoined() {
        let s = store("fallback-failed");
        let mut r = rec(Phase::Discarding, false, vec![g(1, "a")]);
        r.owed[0].start_tries = 2;
        s.save(&r).unwrap();
        let ctl = FakeGuests::default();
        *ctl.start.lock().unwrap() = vec![
            StartOutcome::Failed("NO_HOSTS_AVAILABLE".into()),
            StartOutcome::Started,
        ];
        assert!(matches!(
            run_restarts(&ctl, &s, true, false).await,
            RestartTick::Step {
                host: RestartHost::ThisHost,
                outcome: StartOutcome::Started,
                ..
            }
        ));
        assert_eq!(ctl.calls(), ["start a on peer", "start a on this"]);
        s.clear();
    }

    #[tokio::test]
    async fn disabled_host_reenabled_when_serving_alone_after_episode() {
        let s = store("reenable-alone");
        let mut r = rec(Phase::Discarding, false, Vec::new());
        r.host_disabled = true;
        // The episode has not converged: serving alone is not enough (this
        // copy may still be the one being given up).
        r.open = true;
        s.save(&r).unwrap();
        let ctl = FakeGuests::default();
        assert_eq!(
            run_restarts_ext(&ctl, &s, false, false, true).await,
            RestartTick::Idle
        );
        assert!(!ctl.calls().contains(&"enable".to_string()));
        // Converged once, then the peer died: enabled while serving alone.
        r.open = false;
        s.save(&r).unwrap();
        assert_eq!(
            run_restarts_ext(&ctl, &s, false, false, false).await,
            RestartTick::Idle
        );
        assert!(
            !ctl.calls().contains(&"enable".to_string()),
            "not serving: still held"
        );
        assert_eq!(
            run_restarts_ext(&ctl, &s, false, false, true).await,
            RestartTick::Idle
        );
        assert!(ctl.calls().contains(&"enable".to_string()));
        assert!(!s.exists());
        // A leaked datapath still holds it.
        let mut r = rec(Phase::Discarding, false, Vec::new());
        r.host_disabled = true;
        r.open = false;
        r.leaked = vec!["11111111-2222-3333-4444-555555555555".into()];
        s.save(&r).unwrap();
        let ctl = FakeGuests {
            dps_fail: true,
            ..FakeGuests::default()
        };
        assert_eq!(
            run_restarts_ext(&ctl, &s, false, false, true).await,
            RestartTick::Idle
        );
        assert!(!ctl.calls().contains(&"enable".to_string()));
        s.clear();
    }

    #[tokio::test]
    async fn a_boundary_disabled_host_is_enabled_once_the_pair_has_converged() {
        // Otherwise the loser stays disabled after its guests are back.
        let s = store("reenable");
        let mut r = rec(Phase::Discarding, false, vec![g(1, "a")]);
        r.host_disabled = true;
        s.save(&r).unwrap();
        let ctl = FakeGuests::default();
        assert!(matches!(
            run_restarts(&ctl, &s, true, false).await,
            RestartTick::Step {
                outcome: StartOutcome::Started,
                remaining: 0,
                ..
            }
        ));
        assert!(s.exists(), "kept until the host is enabled again");
        // Not converged yet (resyncing): nothing enabled.
        assert_eq!(run_restarts(&ctl, &s, true, false).await, RestartTick::Idle);
        assert!(!ctl.calls().contains(&"enable".to_string()));
        // Converged: enabled, record gone.
        assert_eq!(run_restarts(&ctl, &s, true, true).await, RestartTick::Idle);
        assert!(ctl.calls().contains(&"enable".to_string()));
        assert!(!s.exists());
        // A restart on this host (after a flip) enables it first.
        let mut r = rec(Phase::Stopped, false, vec![g(2, "b")]);
        r.open = false;
        r.host_disabled = true;
        s.save(&r).unwrap();
        let ctl = FakeGuests::default();
        let _ = run_restarts(&ctl, &s, true, false).await;
        assert_eq!(ctl.calls(), ["enable", "start b on this"]);
        s.clear();
    }

    #[tokio::test]
    async fn a_leftover_tapdisk_blocks_the_start_here() {
        // Without this check a guest restarted on the loser can come up
        // through a tapdisk opened before the discard. Such a handle is never
        // closed after the discard (its close writes the cached VHD
        // metadata): the start here is refused and nothing is released.
        let s = store("held-here-taps");
        s.save(&rec(Phase::Discarding, false, vec![g(1, "a")]))
            .unwrap();
        let ctl = FakeGuests {
            guest_vdis: vec![(g(1, "a").uuid, vec![vdi(0)])],
            ..Default::default()
        };
        *ctl.start.lock().unwrap() = vec![StartOutcome::VdiHeldHere(
            "Error code: SR_BACKEND_FAILURE_46".into(),
        )];
        *ctl.taps.lock().unwrap() = vec![tap(34167, 0)];
        ctl.dps.lock().unwrap().push(dp("vbd/3/xvda", 0));
        assert!(matches!(run_restarts(&ctl, &s, true, false).await,
            RestartTick::Step { outcome: StartOutcome::Failed(why), .. } if why.contains("kill -9") && why.contains("pid 34167")));
        assert_eq!(
            ctl.calls(),
            ["taps", "start a on peer", "taps"],
            "never released, never started here, nothing enabled"
        );
        assert_eq!(ctl.taps.lock().unwrap().len(), 1);
        s.clear();
    }

    fn vdi(n: u32) -> String {
        format!("0b1c2d3e-0000-0000-0000-00000000000{n}")
    }

    fn dp(name: &str, n: u32) -> Datapath {
        Datapath {
            dp: name.to_string(),
            vdi: vdi(n),
        }
    }

    #[tokio::test]
    async fn boundary_releases_datapaths_via_storage_layer_before_discard() {
        // A destroyed guest's datapath can stay "activated" with its tapdisk
        // open; the storage layer's release closes both.
        let s = store("dps-released");
        let ctl = FakeGuests::with_guests(vec![g(1, "a")]);
        *ctl.taps.lock().unwrap() = vec![tap(34167, 0), tap(34235, 1)];
        *ctl.dps.lock().unwrap() = vec![dp("vbd/3/xvda", 0), dp("vbd/3/xvdb", 1)];
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        let calls = ctl.calls();
        let destroy = calls
            .iter()
            .position(|c| c.starts_with("destroy "))
            .unwrap();
        let release = calls
            .iter()
            .position(|c| c == "dp-destroy vbd/3/xvda")
            .unwrap();
        assert!(
            destroy < release,
            "domains first, then their datapaths: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.starts_with("tap-destroy")),
            "no tapdisk killed by hand: {calls:?}"
        );
        assert!(ctl.taps.lock().unwrap().is_empty() && ctl.dps.lock().unwrap().is_empty());
        let r = s.load().unwrap().unwrap();
        assert_eq!((r.phase, r.leaked.len()), (Phase::Stopped, 0));
        s.clear();
    }

    #[tokio::test]
    async fn a_datapath_the_storage_layer_keeps_falls_back_to_tap_ctl_and_is_recorded() {
        let s = store("dps-stuck");
        let ctl = FakeGuests {
            dps_stuck: vec!["vbd/3/xvda".into()],
            ..FakeGuests::with_guests(vec![g(1, "a")])
        };
        *ctl.taps.lock().unwrap() = vec![tap(34167, 0)];
        *ctl.dps.lock().unwrap() = vec![dp("vbd/3/xvda", 0)];
        assert!(
            matches!(
                establish(&ctl, &s, "K").await,
                BoundaryOutcome::Clear { .. }
            ),
            "the tapdisk is closed: the discard may go on"
        );
        assert!(ctl.calls().contains(&"tap-destroy 34167".to_string()));
        assert_eq!(s.load().unwrap().unwrap().leaked, vec![vdi(0)]);

        // After the discard, the guest is never started here over it.
        s.set_phase(Phase::Discarding).unwrap();
        let ctl = FakeGuests {
            dps_stuck: vec!["vbd/3/xvda".into()],
            guest_vdis: vec![(g(1, "a").uuid, vec![vdi(0)])],
            ..Default::default()
        };
        *ctl.dps.lock().unwrap() = vec![dp("vbd/3/xvda", 0)];
        *ctl.start.lock().unwrap() =
            vec![StartOutcome::VdiHeldHere("SR_BACKEND_FAILURE_46".into())];
        assert!(matches!(run_restarts(&ctl, &s, true, false).await,
            RestartTick::Step { outcome: StartOutcome::Failed(why), .. }
                if why.contains("vbd/3/xvda") && why.contains("\"method\":\"DP.destroy\"")));
        assert!(!ctl.calls().contains(&"start a on this".to_string()));

        // Once the storage layer releases it, the guest starts here and the
        // record ends.
        let ctl = FakeGuests {
            guest_vdis: vec![(g(1, "a").uuid, vec![vdi(0)])],
            ..Default::default()
        };
        *ctl.dps.lock().unwrap() = vec![dp("vbd/3/xvda", 0)];
        *ctl.start.lock().unwrap() = vec![
            StartOutcome::VdiHeldHere("SR_BACKEND_FAILURE_46".into()),
            StartOutcome::Started,
        ];
        assert!(matches!(
            run_restarts(&ctl, &s, true, false).await,
            RestartTick::Step {
                host: RestartHost::ThisHost,
                outcome: StartOutcome::Started,
                remaining: 0,
                ..
            }
        ));
        let calls = ctl.calls();
        assert!(
            calls.iter().position(|c| c == "dp-destroy vbd/3/xvda")
                < calls.iter().position(|c| c == "start a on this"),
            "{calls:?}"
        );
        assert!(!s.exists(), "nothing owed, nothing leaked");
    }

    #[tokio::test(start_paused = true)]
    async fn an_unreadable_storage_layer_still_closes_tapdisks_before_the_discard() {
        let s = store("dps-unreadable");
        let ctl = FakeGuests {
            dps_fail: true,
            ..FakeGuests::with_guests(vec![g(1, "a")])
        };
        *ctl.taps.lock().unwrap() = vec![tap(34167, 0)];
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        assert!(ctl.calls().contains(&"tap-destroy 34167".to_string()));
        assert_eq!(
            s.load().unwrap().unwrap().leaked,
            vec![vdi(0)],
            "unconfirmed counts as leaked"
        );
        s.clear();
        // A tapdisk that gains its block backend during the settle belongs to
        // a guest being started here: never closed.
        let s = store("dps-unreadable-starting");
        let ctl = FakeGuests {
            dps_fail: true,
            taps_attached_after_reads: Some(1),
            ..FakeGuests::with_guests(vec![g(1, "a")])
        };
        *ctl.taps.lock().unwrap() = vec![tap(34167, 0)];
        let _ = establish(&ctl, &s, "K").await;
        assert!(
            !ctl.calls().iter().any(|c| c.starts_with("tap-destroy")),
            "{:?}",
            ctl.calls()
        );
        s.clear();
    }

    #[tokio::test]
    async fn a_start_here_releases_stale_datapaths_first() {
        // Episode over without a discard here: the guests start on this host.
        let s = store("dps-before-start-here");
        let mut r = rec(Phase::Stopped, false, vec![g(1, "a")]);
        r.open = false;
        s.save(&r).unwrap();
        let ctl = FakeGuests {
            guest_vdis: vec![(g(1, "a").uuid, vec![vdi(0), vdi(1)])],
            ..Default::default()
        };
        *ctl.dps.lock().unwrap() = vec![dp("vbd/3/xvda", 0), dp("vbd/9/xvda", 5)];
        assert!(matches!(
            run_restarts(&ctl, &s, false, true).await,
            RestartTick::Step {
                host: RestartHost::ThisHost,
                outcome: StartOutcome::Started,
                ..
            }
        ));
        let calls = ctl.calls();
        let release = calls
            .iter()
            .position(|c| c == "dp-destroy vbd/3/xvda")
            .expect("released");
        assert!(
            release < calls.iter().position(|c| c == "start a on this").unwrap(),
            "{calls:?}"
        );
        assert_eq!(
            *ctl.dps.lock().unwrap(),
            vec![dp("vbd/9/xvda", 5)],
            "another guest's disk is not touched"
        );
        s.clear();
    }

    #[tokio::test]
    async fn a_leaked_record_is_retried_until_released_then_ends() {
        let s = store("dps-leaked-retry");
        s.note_leaked(&[vdi(0), vdi(1)]).unwrap();
        let ctl = FakeGuests {
            dps_stuck: vec!["vbd/4/xvda".into()],
            ..Default::default()
        };
        *ctl.dps.lock().unwrap() = vec![dp("vbd/3/xvda", 0), dp("vbd/4/xvda", 1)];
        assert_eq!(
            run_restarts(&ctl, &s, false, false).await,
            RestartTick::Idle
        );
        assert_eq!(s.load().unwrap().unwrap().leaked, vec![vdi(1)]);
        // A VDI a leftover tapdisk still serves is not released (no close after the discard).
        let ctl = FakeGuests::default();
        *ctl.dps.lock().unwrap() = vec![dp("vbd/4/xvda", 1)];
        *ctl.taps.lock().unwrap() = vec![tap(7, 1)];
        assert_eq!(
            run_restarts(&ctl, &s, false, false).await,
            RestartTick::Idle
        );
        assert!(!ctl.calls().iter().any(|c| c.starts_with("dp-destroy")));
        assert!(s.exists());
        let ctl = FakeGuests::default();
        *ctl.dps.lock().unwrap() = vec![dp("vbd/4/xvda", 1)];
        assert_eq!(
            run_restarts(&ctl, &s, false, false).await,
            RestartTick::Idle
        );
        assert!(!s.exists());
    }

    #[tokio::test]
    async fn a_leak_is_retried_while_restarts_are_not_due_yet() {
        // A bring-up winner (or a loser before its episode closes) keeps an
        // open record whose owed restarts are not due: its leaked datapaths
        // must still be retried, or the generic host-enable watchdog holds
        // for good and a release done by hand is never noticed.
        let s = store("leak-not-due");
        let mut r = rec(Phase::Stopped, false, vec![g(1, "a")]);
        r.host_disabled = true;
        r.leaked = vec![vdi(0)];
        s.save(&r).unwrap();
        assert!(!restarts_due(&r));
        let ctl = FakeGuests {
            dps_stuck: vec!["vbd/3/xvda".into()],
            ..Default::default()
        };
        *ctl.dps.lock().unwrap() = vec![dp("vbd/3/xvda", 0)];
        assert_eq!(run_restarts(&ctl, &s, false, true).await, RestartTick::Idle);
        assert_eq!(
            s.load().unwrap().unwrap().leaked,
            vec![vdi(0)],
            "still held"
        );
        // Released (here, or by hand): the record no longer lists it, so the
        // watchdog may enable the host; the owed restart waits for its turn.
        let ctl = FakeGuests::default();
        *ctl.dps.lock().unwrap() = vec![dp("vbd/3/xvda", 0)];
        assert_eq!(run_restarts(&ctl, &s, false, true).await, RestartTick::Idle);
        let after = s.load().unwrap().unwrap();
        assert!(
            after.leaked.is_empty() && after.owed.len() == 1,
            "{after:?}"
        );
        assert!(
            !ctl.calls().iter().any(|c| c.starts_with("start ")),
            "{:?}",
            ctl.calls()
        );
        s.clear();
    }

    #[tokio::test]
    async fn a_host_the_operator_discard_disabled_is_enabled_once_converged() {
        // Otherwise, after `discard-and-resync` the host stays disabled and
        // the daemon's readiness gate keeps the node from serving.
        let s = store("operator-disabled");
        s.note_host_disabled().unwrap();
        let r = s.load().unwrap().unwrap();
        assert!(r.host_disabled && r.owed.is_empty() && r.leaked.is_empty());
        assert_eq!(r.key, "operator-discard");
        let ctl = FakeGuests::default();
        // Still resyncing: left disabled.
        assert_eq!(run_restarts(&ctl, &s, true, false).await, RestartTick::Idle);
        assert!(!ctl.calls().contains(&"enable".to_string()));
        assert!(s.exists());
        // Converged: enabled, and the record ends.
        assert_eq!(run_restarts(&ctl, &s, true, true).await, RestartTick::Idle);
        assert!(ctl.calls().contains(&"enable".to_string()));
        assert!(!s.exists());
        // Added to an existing record, its other content is kept.
        s.note_leaked(&[vdi(2)]).unwrap();
        s.note_host_disabled().unwrap();
        let r = s.load().unwrap().unwrap();
        assert!(r.host_disabled);
        assert_eq!(r.leaked, vec![vdi(2)]);
        assert_eq!(r.key, "leaked-datapaths");
        // A leaked datapath keeps the host disabled even once converged.
        let ctl = FakeGuests {
            dps_stuck: vec!["vbd/4/xvda".into()],
            ..Default::default()
        };
        *ctl.dps.lock().unwrap() = vec![dp("vbd/4/xvda", 2)];
        assert_eq!(run_restarts(&ctl, &s, true, true).await, RestartTick::Idle);
        assert!(!ctl.calls().contains(&"enable".to_string()));
        s.clear();
    }

    #[test]
    fn leaked_vdis_round_trip_and_paths_name_their_vdi() {
        let mut r = rec(Phase::Discarding, false, vec![g(1, "a")]);
        r.leaked = vec![vdi(0), vdi(1)];
        assert_eq!(Record::parse(&r.render()), Some(r));
        assert_eq!(
            Record::parse("key=x\nphase=stopped\nearly=0\nleaked=nope\n"),
            None
        );
        let v = "5e5e000e-0000-4000-8000-00000000000e";
        for p in [
            format!("/dev/VG_XenStorage-{SR}/VHD-{v}"),
            format!("/dev/VG_XenStorage-{SR}/LV-{v}"),
            format!("/dev/sm/backend/{SR}/{v}"),
            format!("/var/run/sr-mount/{SR}/{v}.vhd"),
        ] {
            assert_eq!(vdi_in_path(&p).as_deref(), Some(v), "{p}");
        }
        assert_eq!(vdi_in_path(""), None);
        assert_eq!(vdi_in_path("/dev/xvda"), None);
        assert_eq!(
            guest_vdis_from_minimal(
                &format!("{v},<not in database>,{}", vdi(0)),
                &format!("{},{v}", vdi(9))
            ),
            vec![v.to_string()]
        );
    }

    #[test]
    fn every_listed_domain_is_live_for_datapaths_dying_ones_included() {
        let out = "Name                                        ID   Mem VCPUs\tState\tTime(s)   UUID                            Reason-Code\tSecurity Label\n\
Domain-0                                     0  1424     2     r-----    2759.7 00000000-0000-0000-0000-000000000000        -                -\n\
Test 2                                      13  1023     1     --p--d       5.8 5e5e000a-0000-4000-8000-00000000000a        -                -\n\
Test 3                                      14  1023     1     -b----      12.1 5e5e000c-0000-4000-8000-00000000000c        -                -\n";
        assert_eq!(parse_xl_all_domids(out), Some(vec![0, 13, 14]));
        assert_eq!(
            parse_xl_verbose(out).unwrap().len(),
            1,
            "the guest scan still skips the dying one"
        );
        assert_eq!(parse_xl_all_domids("header\ngarbage row\n"), None);
    }

    fn tap(pid: u32, minor: u32) -> Tapdisk {
        Tapdisk {
            pid,
            minor,
            path: format!("/dev/VG_XenStorage-{SR}/VHD-0b1c2d3e-0000-0000-0000-00000000000{minor}"),
        }
    }

    const SR: &str = "5e5e000f-0000-4000-8000-00000000000f";

    #[test]
    fn real_tap_ctl_list_and_paths_parse() {
        // Verbatim `tap-ctl list` from an XCP-ng 8.3 loser after a split-brain
        // discard.
        let out = "pid=34167 minor=0 state=0 args=vhd:/dev/VG_XenStorage-5e5e000f-0000-4000-8000-00000000000f/VHD-5e5e000d-0000-4000-8000-00000000000d\n\
pid=34235 minor=1 state=0 args=vhd:/dev/VG_XenStorage-5e5e000f-0000-4000-8000-00000000000f/VHD-5e5e000e-0000-4000-8000-00000000000e\n";
        let taps = parse_tap_ctl_list(out).unwrap();
        assert_eq!(taps.len(), 2);
        assert_eq!((taps[0].pid, taps[0].minor), (34167, 0));
        assert_eq!(
            sr_vdi_of_path(&taps[1].path, SR).as_deref(),
            Some("5e5e000e-0000-4000-8000-00000000000e")
        );
        assert_eq!(parse_tap_ctl_list("").unwrap(), vec![]);
        assert_eq!(
            parse_tap_ctl_list("something else entirely"),
            None,
            "an unknown format never reads as none"
        );
        assert_eq!(
            parse_tap_ctl_list("pid=12 minor=3 state=0\n").unwrap()[0].path,
            "",
            "no image open"
        );
        // A tapdisk being torn down, listed without its minor: the line is
        // named so the error says what was not understood (the read retries).
        let tearing = "pid=12 minor=3 state=0\npid=40 state=0x2 args=vhd:/dev/x\n";
        assert_eq!(parse_tap_ctl_list(tearing), None);
        assert_eq!(
            first_unparsed_tap_line(tearing),
            Some("pid=40 state=0x2 args=vhd:/dev/x")
        );
        assert_eq!(first_unparsed_tap_line("pid=12 minor=3 state=0\n"), None);
        // An orphan minor (no process, no image) holds nothing: skipped. A
        // minor with an image but no process still fails closed.
        assert_eq!(
            parse_tap_ctl_list("pid=12 minor=3 state=0\nminor=7\n")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            parse_tap_ctl_list("minor=8 state=0\n"),
            None,
            "only a bare minor is an orphan"
        );
        assert_eq!(
            orphan_minors("pid=12 minor=3 state=0\nminor=7\nminor=8 state=0\n"),
            vec![7]
        );
        assert_eq!(parse_tap_ctl_list("minor=7 args=vhd:/dev/x\n"), None);
        // QCOW2 (and any other format prefix) on an LVM SR, and a file SR's .qcow2.
        let vdi = "0b1c2d3e-0000-0000-0000-000000000001";
        assert_eq!(
            sr_vdi_of_path(&format!("qcow2:/dev/VG_XenStorage-{SR}/QCOW2-{vdi}"), SR).as_deref(),
            Some(vdi)
        );
        assert_eq!(
            vdi_in_path(&format!("/dev/VG_XenStorage-{SR}/QCOW2-{vdi}")).as_deref(),
            Some(vdi)
        );
        assert_eq!(
            vdi_in_path(&format!("/var/run/sr-mount/{SR}/{vdi}.qcow2")).as_deref(),
            Some(vdi)
        );
        assert_eq!(
            sr_vdi_of_path(&format!("/dev/sm/backend/{SR}/{vdi}"), SR).as_deref(),
            Some(vdi),
            "a bare uuid stays whole"
        );
        // The other path forms, and another SR.
        let vdi = "0b1c2d3e-0000-0000-0000-000000000001";
        assert_eq!(
            sr_vdi_of_path(&format!("/dev/sm/backend/{SR}/{vdi}"), SR).as_deref(),
            Some(vdi)
        );
        assert_eq!(
            sr_vdi_of_path(&format!("aio:/dev/VG_XenStorage-{SR}/LV-{vdi}"), SR).as_deref(),
            Some(vdi)
        );
        assert_eq!(
            sr_vdi_of_path(&format!("/var/run/sr-mount/{SR}/{vdi}.vhd"), SR).as_deref(),
            Some(vdi)
        );
        assert_eq!(
            sr_vdi_of_path(
                &format!("/dev/sm/backend/99999999-0000-0000-0000-000000000000/{vdi}"),
                SR
            ),
            None
        );
    }

    #[test]
    fn a_tapdisk_is_left_over_only_when_no_live_domain_has_its_vdi() {
        // Real layout: vbd3 backends, params = /dev/sm/backend/<sr>/<vdi>.
        let xs = format!(
            "/local/domain/0/backend/vbd3/7/768/params = \"/dev/sm/backend/{SR}/0b1c2d3e-0000-0000-0000-000000000000\"\n\
/local/domain/0/backend/vbd3/9/768/params = \"/dev/sm/backend/{SR}/0b1c2d3e-0000-0000-0000-000000000001\"\n\
/local/domain/0/backend/vbd3/0/51712/params = \"/dev/sm/backend/{SR}/0b1c2d3e-0000-0000-0000-000000000002\"\n\
/local/domain/0/backend/vbd3/7/5696/params = \"\"\n"
        );
        // Domain 7 is live, 9 was destroyed (its backend node lingers), dom0 is always live.
        let live = live_sr_vdis(&xs, SR, &[7]);
        assert_eq!(live.len(), 2);
        let left = leftover_sr_tapdisks(&[tap(1, 0), tap(2, 1), tap(3, 2)], SR, &live);
        assert_eq!(
            left.iter().map(|t| t.pid).collect::<Vec<_>>(),
            [2],
            "only the destroyed domain's disk"
        );
        // A tapdisk on another SR is never touched.
        let other = Tapdisk { pid: 4, minor: 4, path: "/dev/VG_XenStorage-99999999-0000-0000-0000-000000000000/VHD-0b1c2d3e-0000-0000-0000-000000000009".into() };
        assert!(leftover_sr_tapdisks(&[other], SR, &[]).is_empty());
    }

    #[tokio::test]
    async fn the_boundary_closes_leftover_tapdisks_before_it_clears() {
        let s = store("taps-swept");
        let ctl = FakeGuests::with_guests(vec![g(1, "a")]);
        *ctl.taps.lock().unwrap() = vec![tap(34167, 0), tap(34235, 1)];
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        let calls = ctl.calls();
        let destroy = calls
            .iter()
            .position(|c| c.starts_with("destroy "))
            .unwrap();
        let first_tap = calls
            .iter()
            .position(|c| c.starts_with("tap-destroy"))
            .unwrap();
        assert!(
            destroy < first_tap,
            "domains first, then their tapdisks: {calls:?}"
        );
        assert!(ctl.taps.lock().unwrap().is_empty());
        assert_eq!(s.load().unwrap().unwrap().phase, Phase::Stopped);
        assert!(confirm_clear(&ctl).await.is_ok());

        // A tapdisk that will not close holds the boundary: nothing is discarded.
        let s2 = store("taps-stuck");
        let ctl = FakeGuests {
            taps_stuck: vec![34167],
            ..FakeGuests::with_guests(vec![g(1, "a")])
        };
        *ctl.taps.lock().unwrap() = vec![tap(34167, 0)];
        assert!(matches!(
            establish(&ctl, &s2, "K").await,
            BoundaryOutcome::Hold {
                step: Step::Tapdisks,
                ..
            }
        ));
        assert!(
            confirm_clear(&ctl).await.is_err(),
            "the last check sees it too"
        );
        // An unreadable tap-ctl holds as well.
        let s3 = store("taps-unreadable");
        let ctl = FakeGuests {
            taps_fail: true,
            ..FakeGuests::with_guests(vec![g(1, "a")])
        };
        assert!(matches!(
            establish(&ctl, &s3, "K").await,
            BoundaryOutcome::Hold {
                step: Step::Tapdisks,
                ..
            }
        ));
        for st in [s, s2, s3] {
            st.clear();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_close_never_goes_through_the_peers_path() {
        // A disk-handle close writes its cached VHD metadata through this
        // host's multipath map: only ever onto this node's own copy.
        for (path, tag) in [(Some(false), "path-peer"), (None, "path-unknown")] {
            let s = store(tag);
            let ctl = FakeGuests {
                path_active: Some(path),
                ..FakeGuests::with_guests(vec![g(1, "a")])
            };
            *ctl.taps.lock().unwrap() = vec![tap(34167, 0)];
            *ctl.dps.lock().unwrap() = vec![dp("vbd/3/xvda", 0)];
            let out = establish(&ctl, &s, "K").await;
            assert!(
                matches!(&out, BoundaryOutcome::Hold { step: Step::Tapdisks, detail, .. } if detail.contains("peer's copy")),
                "{out:?}"
            );
            assert!(
                !ctl.calls()
                    .iter()
                    .any(|c| c.starts_with("tap-destroy") || c.starts_with("dp-destroy")),
                "{:?}",
                ctl.calls()
            );
            s.clear();
        }
        // With nothing open, the path does not matter: a datapath without a
        // tapdisk is released (no write).
        let s = store("path-peer-nothing-open");
        let ctl = FakeGuests {
            path_active: Some(Some(false)),
            ..FakeGuests::with_guests(vec![g(1, "a")])
        };
        *ctl.dps.lock().unwrap() = vec![dp("vbd/3/xvda", 0)];
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        assert!(ctl.calls().contains(&"dp-destroy vbd/3/xvda".to_string()));
        s.clear();
    }

    #[tokio::test]
    async fn the_bring_up_winner_closes_leftovers_onto_the_peers_copy() {
        // Its guests reached the SR through the peer's portal (this node does
        // not serve yet): their handles hold the peer's history, and a close
        // belongs on the peer's copy, the one replaced. The loser's own-path
        // rule would hold it for good.
        let s = store("winner-close");
        let ctl = FakeGuests {
            exported: Some(false),
            path_active: Some(Some(false)),
            ..FakeGuests::with_guests(vec![g(1, "a")])
        };
        *ctl.taps.lock().unwrap() = vec![tap(34167, 0)];
        assert!(
            matches!(
                establish_as(&ctl, &s, "K", Side::Winner).await,
                BoundaryOutcome::Clear { .. }
            ),
            "{:?}",
            ctl.calls()
        );
        assert!(ctl.calls().contains(&"tap-destroy 34167".to_string()));
        s.clear();
        // Exported with its own path active: a close would land on the kept copy.
        let s = store("winner-own-path");
        let ctl = FakeGuests {
            exported: Some(true),
            path_active: Some(Some(true)),
            ..FakeGuests::with_guests(vec![g(1, "a")])
        };
        *ctl.taps.lock().unwrap() = vec![tap(34167, 0)];
        let out = establish_as(&ctl, &s, "K", Side::Winner).await;
        assert!(
            matches!(&out, BoundaryOutcome::Hold { step: Step::Tapdisks, detail, .. } if detail.contains("keeps")),
            "{out:?}"
        );
        assert!(!ctl.calls().iter().any(|c| c.starts_with("tap-destroy")));
        s.clear();
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_yielded_boundary_suspends_its_copy_again() {
        // The suspend lifted to release the handles must not stay lifted
        // while the boundary waits: the copy about to be discarded would
        // acknowledge new writes for a whole retry interval.
        let s = store("yield-hold-resuspend");
        let ctl = FakeGuests {
            path_active: Some(Some(false)),
            ..FakeGuests::with_guests(vec![g(1, "a")])
        };
        *ctl.suspended.lock().unwrap() = true;
        *ctl.taps.lock().unwrap() = vec![tap(34167, 0)];
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Hold {
                step: Step::Tapdisks,
                ..
            }
        ));
        let calls = ctl.calls();
        let lift = calls
            .iter()
            .position(|c| c == "resume-io")
            .expect("lifted for the close");
        let again = calls
            .iter()
            .position(|c| c == "suspend-io")
            .expect("suspended again on the hold");
        assert!(lift < again, "{calls:?}");
        assert!(*ctl.suspended.lock().unwrap());
        s.clear();
        // Nothing to close: the suspend is not touched (the discard lifts it).
        let s = store("yield-nothing-open");
        let ctl = FakeGuests::with_guests(Vec::new());
        *ctl.suspended.lock().unwrap() = true;
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        assert!(
            !ctl.calls()
                .iter()
                .any(|c| c == "resume-io" || c == "suspend-io"),
            "{:?}",
            ctl.calls()
        );
        s.clear();
        // A copy that was not suspended is never suspended by a hold.
        let s = store("not-yielded-hold");
        let ctl = FakeGuests {
            path_active: Some(Some(false)),
            ..FakeGuests::with_guests(vec![g(1, "a")])
        };
        *ctl.taps.lock().unwrap() = vec![tap(34167, 0)];
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Hold { .. }
        ));
        assert!(!ctl.calls().iter().any(|c| c == "suspend-io"));
        s.clear();
    }

    #[tokio::test(start_paused = true)]
    async fn every_boundary_call_leaves_a_yielded_copy_suspended() {
        // The lift is a bracket inside one boundary call: a call that clears
        // also suspends the copy again, so whatever follows before the
        // discard (the last check finding a guest rebuilt in place, a hold
        // in a later attempt, a daemon restart) finds it frozen. Only the
        // discard's own resume outlives a boundary call.
        let s = store("yield-bracket");
        let ctl = FakeGuests::with_guests(vec![g(1, "a")]);
        *ctl.suspended.lock().unwrap() = true;
        *ctl.taps.lock().unwrap() = vec![tap(34167, 0)];
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        let calls = ctl.calls();
        let lift = calls
            .iter()
            .position(|c| c == "resume-io")
            .expect("lifted for the close");
        let close = calls.iter().position(|c| c == "tap-destroy 34167").unwrap();
        let again = calls
            .iter()
            .rposition(|c| c == "suspend-io")
            .expect("suspended again");
        assert!(lift < close && close < again, "{calls:?}");
        assert!(*ctl.suspended.lock().unwrap());
        // A later attempt of the episode that holds: lifted and suspended
        // again within that call too.
        let ctl = FakeGuests {
            path_active: Some(Some(false)),
            ..FakeGuests::with_guests(vec![g(1, "a")])
        };
        *ctl.suspended.lock().unwrap() = true;
        *ctl.taps.lock().unwrap() = vec![tap(34200, 1)];
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Hold { .. }
        ));
        assert!(*ctl.suspended.lock().unwrap(), "{:?}", ctl.calls());
        s.clear();
    }

    #[tokio::test(start_paused = true)]
    async fn an_unreadable_storage_layer_does_not_block_the_lift() {
        // The sweep's direct-close fallback needs the frozen writes drained;
        // the local scan is the evidence, as for that fallback.
        let s = store("lift-dps-unreadable");
        let ctl = FakeGuests {
            dps_fail: true,
            close_blocks_while_suspended: true,
            ..FakeGuests::with_guests(vec![g(1, "a")])
        };
        *ctl.suspended.lock().unwrap() = true;
        *ctl.taps.lock().unwrap() = vec![tap(34167, 0)];
        let _ = establish(&ctl, &s, "K").await;
        let calls = ctl.calls();
        assert!(calls.contains(&"resume-io".to_string()), "{calls:?}");
        assert!(
            *ctl.suspended.lock().unwrap(),
            "suspended again before returning: {calls:?}"
        );
        // A live datapath read successfully still refuses; an unreadable scan
        // refuses.
        let ctl = FakeGuests::default();
        *ctl.live_dp_vdis.lock().unwrap() = vec![vdi(7)];
        assert!(!quiet_to_lift(&ctl).await);
        let ctl = FakeGuests {
            scan_fails: true,
            ..Default::default()
        };
        assert!(!quiet_to_lift(&ctl).await);
        let ctl = FakeGuests {
            dps_fail: true,
            ..Default::default()
        };
        assert!(quiet_to_lift(&ctl).await);
        s.clear();
    }

    #[tokio::test(start_paused = true)]
    async fn a_guest_starting_mid_boundary_never_runs_on_a_lifted_copy() {
        // Lifted after the poweroff; the sweep then finds a guest being built
        // here: the copy is frozen again before that guest is waited for.
        let s = store("starting-refreeze");
        let ctl = FakeGuests {
            xc: true,
            live_from_read: Some((2, vec![vdi(4)])),
            ..FakeGuests::with_guests(vec![g(1, "a")])
        };
        *ctl.suspended.lock().unwrap() = true;
        *ctl.taps.lock().unwrap() = vec![tap(40000, 4)];
        let _ = establish(&ctl, &s, "K").await;
        let calls = ctl.calls();
        let lift = calls
            .iter()
            .position(|c| c == "resume-io")
            .expect("lifted after the poweroff");
        let refreeze = calls
            .iter()
            .skip(lift)
            .position(|c| c == "suspend-io")
            .map(|i| i + lift)
            .expect("frozen again when the starting guest is found");
        // Frozen before the settle and the re-scan that looks for the new
        // guest, and its tapdisk never closed.
        let rescan = calls
            .iter()
            .skip(refreeze)
            .position(|c| c == "scan")
            .map(|i| i + refreeze);
        assert!(lift < refreeze && rescan.is_some(), "{calls:?}");
        assert!(
            !calls.contains(&"tap-destroy 40000".to_string()),
            "{calls:?}"
        );
        assert!(*ctl.suspended.lock().unwrap(), "{calls:?}");
        s.clear();
    }

    #[tokio::test]
    async fn a_lift_that_errors_after_resuming_is_undone() {
        let ctl = FakeGuests {
            lift_errs_after_resume: true,
            ..Default::default()
        };
        *ctl.suspended.lock().unwrap() = true;
        let b = LiftBracket::default();
        b.lift_if_safe(&ctl).await;
        assert!(!*ctl.suspended.lock().unwrap());
        b.close(&ctl).await;
        assert!(*ctl.suspended.lock().unwrap(), "{:?}", ctl.calls());
    }

    #[tokio::test]
    async fn a_running_yielded_copy_is_frozen_before_the_boundary() {
        // A daemon killed inside a lift bracket never ran `close`.
        let ctl = FakeGuests::default();
        refreeze_yielded(&ctl).await;
        assert_eq!(ctl.calls(), ["suspend-io"]);
        assert!(*ctl.suspended.lock().unwrap());
        // Already frozen: nothing to do.
        refreeze_yielded(&ctl).await;
        assert_eq!(ctl.calls(), ["suspend-io"]);
    }

    #[tokio::test]
    async fn leaked_datapaths_are_not_retried_while_the_copy_is_suspended() {
        // SM's close would block on the frozen writes, outside any bracket.
        let s = store("leak-suspended");
        let mut r = rec(Phase::Stopped, false, vec![g(1, "a")]);
        r.leaked = vec![vdi(0)];
        s.save(&r).unwrap();
        let ctl = FakeGuests::default();
        *ctl.suspended.lock().unwrap() = true;
        *ctl.dps.lock().unwrap() = vec![dp("vbd/3/xvda", 0)];
        assert_eq!(run_restarts(&ctl, &s, false, true).await, RestartTick::Idle);
        assert_eq!(s.load().unwrap().unwrap().leaked, vec![vdi(0)]);
        assert!(
            !ctl.calls()
                .iter()
                .any(|c| c.starts_with("dps") || c.starts_with("dp-destroy")),
            "{:?}",
            ctl.calls()
        );
        // Resumed (the discard ran): retried and released.
        *ctl.suspended.lock().unwrap() = false;
        assert_eq!(run_restarts(&ctl, &s, false, true).await, RestartTick::Idle);
        assert!(s.load().unwrap().unwrap().leaked.is_empty());
        s.clear();
    }

    #[tokio::test(start_paused = true)]
    async fn the_bring_up_winner_never_lifts_or_suspends_its_copy() {
        let s = store("winner-no-suspend");
        let ctl = FakeGuests {
            path_active: Some(Some(true)),
            ..FakeGuests::with_guests(vec![g(1, "a")])
        };
        *ctl.suspended.lock().unwrap() = true;
        *ctl.taps.lock().unwrap() = vec![tap(34167, 0)];
        let _ = establish_as(&ctl, &s, "K", Side::Winner).await;
        assert!(
            !ctl.calls()
                .iter()
                .any(|c| c == "resume-io" || c == "suspend-io"),
            "{:?}",
            ctl.calls()
        );
        s.clear();
    }

    #[tokio::test]
    async fn a_yielded_losers_suspend_is_lifted_before_its_handles_close() {
        // A yielded loser's copy stays suspended until the discard: without
        // the lift, the stopped guests' frozen writes hold their tapdisks
        // open, no close can finish, and the resolution holds for good.
        let s = store("yield-lift");
        let ctl = FakeGuests {
            close_blocks_while_suspended: true,
            ..FakeGuests::with_guests(vec![g(1, "a")])
        };
        *ctl.suspended.lock().unwrap() = true;
        *ctl.taps.lock().unwrap() = vec![tap(34167, 0)];
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        let calls = ctl.calls();
        let stop = calls
            .iter()
            .position(|c| c.starts_with("destroy "))
            .unwrap();
        let lift = calls.iter().position(|c| c == "resume-io").expect("lifted");
        let close = calls.iter().position(|c| c == "tap-destroy 34167").unwrap();
        assert!(
            stop < lift && lift < close,
            "guests stopped, then the suspend lifted, then the close: {calls:?}"
        );
        s.clear();
    }

    #[tokio::test(start_paused = true)]
    async fn the_suspend_is_lifted_before_xenopsd_is_waited_for() {
        // xenopsd's own teardown also blocks on the frozen writes.
        let ctl = FakeGuests {
            xc: true,
            ..FakeGuests::with_guests(vec![g(1, "a")])
        };
        *ctl.suspended.lock().unwrap() = true;
        let b = LiftBracket::default();
        let out = stop_domains(
            &ctl,
            vec![(1, g(1, "a").uuid)],
            Some(&b),
            || {},
            |_, _| Ok(()),
        )
        .await
        .unwrap();
        assert!(out.destroyed.is_empty() && out.still_running.is_empty());
        let calls = ctl.calls();
        let lift = calls.iter().position(|c| c == "resume-io").expect("lifted");
        let wait = calls.iter().position(|c| c == "present").unwrap();
        assert!(lift < wait, "{calls:?}");
        b.close(&ctl).await;
        assert!(
            *ctl.suspended.lock().unwrap(),
            "the bracket suspends it again"
        );
        // Never lifted while a guest still runs.
        let ctl = FakeGuests {
            unkillable: vec![g(1, "a").uuid],
            ..FakeGuests::with_guests(vec![g(1, "a")])
        };
        *ctl.suspended.lock().unwrap() = true;
        let out = stop_domains(
            &ctl,
            vec![(1, g(1, "a").uuid)],
            Some(&LiftBracket::default()),
            || {},
            |_, _| Ok(()),
        )
        .await
        .unwrap();
        assert_eq!(out.still_running.len(), 1);
        assert!(
            !ctl.calls().contains(&"resume-io".to_string()),
            "{:?}",
            ctl.calls()
        );
        // Nor while a domain is being built here with an SR disk.
        let ctl = FakeGuests {
            xc: true,
            ..FakeGuests::with_guests(vec![g(1, "a")])
        };
        *ctl.suspended.lock().unwrap() = true;
        *ctl.live_dp_vdis.lock().unwrap() = vec![vdi(7)];
        stop_domains(
            &ctl,
            vec![(1, g(1, "a").uuid)],
            Some(&LiftBracket::default()),
            || {},
            |_, _| Ok(()),
        )
        .await
        .unwrap();
        assert!(
            !ctl.calls().contains(&"resume-io".to_string()),
            "{:?}",
            ctl.calls()
        );
        // No lift without a bracket (the winner, catch-rebuilt).
        let ctl = FakeGuests {
            xc: true,
            ..FakeGuests::with_guests(vec![g(1, "a")])
        };
        *ctl.suspended.lock().unwrap() = true;
        stop_domains(&ctl, vec![(1, g(1, "a").uuid)], None, || {}, |_, _| Ok(()))
            .await
            .unwrap();
        assert!(
            !ctl.calls().contains(&"resume-io".to_string()),
            "{:?}",
            ctl.calls()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn timed_out_poweroff_destroys_only_running_domains() {
        // Destroying a domain the call already powered off would turn its
        // poweroff back into a crash, i.e. a rebuild in place.
        let ctl = FakeGuests {
            poweroff_partial: Some(vec![g(1, "a").uuid]),
            ..FakeGuests::with_guests(vec![g(1, "a"), g(2, "b")])
        };
        let out = stop_domains(
            &ctl,
            vec![(1, g(1, "a").uuid), (2, g(2, "b").uuid)],
            None,
            || {},
            |_, _| Ok(()),
        )
        .await
        .unwrap();
        assert_eq!(ctl.stops(), ["destroy b"], "{:?}", ctl.calls());
        assert_eq!(out.destroyed, vec![(2, g(2, "b").uuid)]);
        assert!(out.still_running.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_guest_being_started_here_is_stopped_its_tapdisk_never_closed_first() {
        // A guest started on this host while the boundary runs (HA, an
        // operator): its tapdisk exists and its datapath is live, its block
        // backend not written yet. Closing that tapdisk would leave the new
        // domain's backend naming a freed blktap minor.
        let s = store("starting");
        let ctl = FakeGuests {
            live_clears_at_scan: Some(3),
            extra_names: vec![g(9, "new")],
            ..FakeGuests::with_guests(vec![g(1, "a")])
        };
        *ctl.taps.lock().unwrap() = vec![tap(40000, 4)];
        *ctl.live_dp_vdis.lock().unwrap() = vec![vdi(4)];
        ctl.rebuild_at_scan
            .lock()
            .unwrap()
            .push((3, g(9, "new").uuid));
        assert!(
            matches!(
                establish(&ctl, &s, "K").await,
                BoundaryOutcome::Clear { .. }
            ),
            "{:?}",
            ctl.calls()
        );
        let calls = ctl.calls();
        let stop_new = calls
            .iter()
            .position(|c| c == "destroy new")
            .expect("the starting guest is stopped");
        let close = calls
            .iter()
            .position(|c| c == "tap-destroy 40000")
            .expect("closed once its guest is stopped");
        assert!(stop_new < close, "{calls:?}");
        assert!(
            s.load()
                .unwrap()
                .unwrap()
                .owed
                .iter()
                .any(|o| o.guest.uuid == g(9, "new").uuid),
            "owed a restart too"
        );
        s.clear();
        // A start that does not settle holds; its tapdisk is never closed.
        let s = store("starting-holds");
        let ctl = FakeGuests::with_guests(vec![g(1, "a")]);
        *ctl.taps.lock().unwrap() = vec![tap(40000, 4)];
        *ctl.live_dp_vdis.lock().unwrap() = vec![vdi(4)];
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Hold {
                step: Step::Tapdisks,
                ..
            }
        ));
        assert!(
            !ctl.calls().iter().any(|c| c.starts_with("tap-destroy")),
            "{:?}",
            ctl.calls()
        );
        s.clear();
    }

    #[tokio::test(start_paused = true)]
    async fn datapath_of_dying_domain_is_waited_for_then_kept_as_leaked() {
        let s = store("stopping-dps");
        let ctl = FakeGuests::with_guests(vec![g(1, "a")]);
        *ctl.stopping_dps.lock().unwrap() = vec![dp("vbd/7/xvda", 0)];
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        assert_eq!(
            s.load().unwrap().unwrap().leaked,
            vec![vdi(0)],
            "kept until confirmed gone"
        );
        assert!(
            !ctl.calls().iter().any(|c| c.starts_with("dp-destroy")),
            "its own teardown is left to it"
        );
        s.clear();
        // A teardown that finishes within the settle window leaves nothing.
        let s = store("stopping-dps-settles");
        let ctl = FakeGuests::with_guests(vec![g(1, "a")]);
        *ctl.stopping_dps.lock().unwrap() = vec![dp("vbd/7/xvda", 0)];
        *ctl.stopping_clears_after.lock().unwrap() = Some(2);
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        assert!(s.load().unwrap().unwrap().leaked.is_empty());
        s.clear();
    }

    #[tokio::test(start_paused = true)]
    async fn the_sweep_is_bounded_as_a_whole() {
        let ctl = FakeGuests {
            tap_destroy_delay: Some(std::time::Duration::from_secs(60)),
            ..Default::default()
        };
        *ctl.taps.lock().unwrap() = vec![tap(1, 0)];
        let t0 = tokio::time::Instant::now();
        let r = sweep_disk_handles(&ctl, &[], Side::Loser, None).await;
        assert!(
            matches!(&r, Err(f) if f.detail.contains("within 20 s") && !f.starting),
            "{r:?}"
        );
        assert!(t0.elapsed() <= SWEEP_DEADLINE + std::time::Duration::from_secs(1));
    }

    #[tokio::test]
    async fn a_clean_release_reads_the_storage_layer_once() {
        let ctl = FakeGuests::default();
        *ctl.dps.lock().unwrap() = vec![dp("vbd/3/xvda", 0)];
        let sw = sweep_disk_handles(&ctl, &[], Side::Loser, None)
            .await
            .unwrap();
        assert_eq!((sw.released, sw.leaked.len()), (1, 0));
        assert_eq!(
            ctl.calls().iter().filter(|c| c.starts_with("dps")).count(),
            1,
            "{:?}",
            ctl.calls()
        );
    }

    #[tokio::test]
    async fn the_operator_lift_refuses_only_for_guests() {
        let ctl = FakeGuests::default();
        assert_eq!(
            operator_lift_check(&ctl).await,
            Ok(vec![]),
            "nothing on the SR: lift"
        );
        ctl.live_dp_vdis.lock().unwrap().push(vdi(2));
        assert!(
            operator_lift_check(&ctl)
                .await
                .unwrap_err()
                .contains("being started"),
            "a guest being built refuses"
        );
    }

    #[test]
    fn datapaths_are_classified_by_their_domain() {
        let dps: Vec<(String, String)> = [
            "vbd/3/xvda",
            "vbd/0/xvdb",
            "vbd/15/xvda",
            "copy/7",
            "vbd/14/xvdb",
        ]
        .iter()
        .map(|d| (d.to_string(), "Activated".to_string()))
        .collect();
        let mut st = DpState::default();
        // 14 runs, 15 is listed but dying, 3 is gone.
        classify_datapaths(&vdi(0), &dps, &[14], &[0, 14, 15], &mut st);
        assert_eq!(st.stale, vec![dp("vbd/3/xvda", 0)]);
        assert_eq!(st.stopping, vec![dp("vbd/15/xvda", 0)]);
        assert_eq!(
            st.live_vdis,
            vec![vdi(0)],
            "dom0, a running domain and a non-guest consumer hold it live"
        );
        assert_eq!(
            st.guest_live_vdis,
            vec![vdi(0)],
            "the running guest domain 14 holds it"
        );
        let mut only_dom0 = DpState::default();
        let dom0: Vec<(String, String)> = ["vbd/0/xvdb", "copy/7"]
            .iter()
            .map(|d| (d.to_string(), "Activated".to_string()))
            .collect();
        classify_datapaths(&vdi(1), &dom0, &[14], &[0, 14], &mut only_dom0);
        assert_eq!(only_dom0.live_vdis, vec![vdi(1)]);
        assert!(
            only_dom0.guest_live_vdis.is_empty(),
            "dom0 and non-guest consumers are not guests"
        );
    }

    #[tokio::test]
    async fn a_restart_step_never_erases_what_the_tick_wrote_meanwhile() {
        // The step runs off the tick and awaits a start for up to 45 s; a
        // whole-record save of its snapshot would erase a new boundary
        // round's guests, a phase change or the episode's end written
        // meanwhile.
        let s = store("delta-merge");
        s.save(&rec(Phase::Discarding, false, vec![g(1, "a"), g(2, "b")]))
            .unwrap();
        let path = s.path.clone();
        let ctl = FakeGuests {
            on_start: Some(Box::new(move || {
                Store::new(&path)
                    .update(|r| {
                        r.open = false;
                        r.owed.push(Owed::new(g(3, "c")));
                        (false, ())
                    })
                    .unwrap();
            })),
            ..Default::default()
        };
        assert!(matches!(
            run_restarts(&ctl, &s, false, false).await,
            RestartTick::Step {
                outcome: StartOutcome::Started,
                remaining: 2,
                ..
            }
        ));
        let r = s.load().unwrap().unwrap();
        assert!(!r.open, "the episode's end survives");
        assert_eq!(
            r.owed
                .iter()
                .map(|o| o.guest.name.as_str())
                .collect::<Vec<_>>(),
            ["b", "c"]
        );
        s.clear();

        // A failed start of a guest dropped meanwhile (a suspect one) is not
        // put back.
        let s = store("delta-dropped");
        s.save(&rec(Phase::Discarding, false, vec![g(1, "a"), g(2, "b")]))
            .unwrap();
        let path = s.path.clone();
        let ctl = FakeGuests {
            on_start: Some(Box::new(move || {
                Store::new(&path)
                    .update(|r| {
                        r.owed.retain(|o| o.guest.name != "a");
                        (false, ())
                    })
                    .unwrap();
            })),
            ..Default::default()
        };
        *ctl.start.lock().unwrap() = vec![StartOutcome::Failed("busy".into())];
        assert!(matches!(
            run_restarts(&ctl, &s, false, false).await,
            RestartTick::Step { remaining: 1, .. }
        ));
        assert_eq!(
            s.load()
                .unwrap()
                .unwrap()
                .owed
                .iter()
                .map(|o| o.guest.name.as_str())
                .collect::<Vec<_>>(),
            ["b"]
        );
        s.clear();
    }

    #[tokio::test]
    async fn the_host_is_not_enabled_while_a_stale_datapath_remains_on_it() {
        // An enabled host takes HA restarts, migrations and plain starts,
        // which would reuse a stale datapath.
        let s = store("enable-leaked");
        let mut r = rec(Phase::Discarding, false, Vec::new());
        r.host_disabled = true;
        r.leaked = vec![vdi(0)];
        s.save(&r).unwrap();
        let ctl = FakeGuests {
            dps_stuck: vec!["vbd/3/xvda".into()],
            ..Default::default()
        };
        *ctl.dps.lock().unwrap() = vec![dp("vbd/3/xvda", 0)];
        assert_eq!(run_restarts(&ctl, &s, true, true).await, RestartTick::Idle);
        assert!(
            !ctl.calls().contains(&"enable".to_string()),
            "{:?}",
            ctl.calls()
        );
        // Released: enabled, and the record ends.
        let ctl = FakeGuests::default();
        *ctl.dps.lock().unwrap() = vec![dp("vbd/3/xvda", 0)];
        assert_eq!(run_restarts(&ctl, &s, true, true).await, RestartTick::Idle);
        assert!(ctl.calls().contains(&"enable".to_string()));
        assert!(!s.exists());

        // A start here that its disk check refuses does not enable the host.
        let s = store("enable-after-gate");
        let mut r = rec(Phase::Stopped, false, vec![g(1, "a")]);
        r.open = false;
        r.host_disabled = true;
        s.save(&r).unwrap();
        let ctl = FakeGuests {
            guest_vdis: vec![(g(1, "a").uuid, vec![vdi(0)])],
            ..Default::default()
        };
        *ctl.taps.lock().unwrap() = vec![tap(34167, 0)];
        assert!(matches!(
            run_restarts(&ctl, &s, false, true).await,
            RestartTick::Step {
                outcome: StartOutcome::Failed(_),
                ..
            }
        ));
        assert!(
            !ctl.calls().contains(&"enable".to_string()),
            "{:?}",
            ctl.calls()
        );
        assert!(s.load().unwrap().unwrap().host_disabled);
        s.clear();
    }

    #[test]
    fn the_first_release_candidates_record_still_reads() {
        // `attempts=N`, `vm=<uuid> <name>`, no `early` line.
        let text =
            "key=K\nphase=stopping\nattempts=3\nvm=00000001-0000-0000-0000-000000000000 Test 3\n";
        let r = Record::parse(text).expect("the earlier format is accepted");
        assert_eq!(r.phase, Phase::Stopping);
        assert!(!r.early_restart && r.open && !r.agent_restarted);
        assert_eq!(r.owed, vec![Owed::new(g(1, "Test 3"))]);
        assert_eq!(Record::parse("key=K\nphase=stopping\nattempts=x\n"), None);
    }

    #[test]
    fn a_flip_revokes_early_restarts_and_a_convergence_closes_the_episode() {
        let s = store("flip-revoke");
        s.save(&rec(Phase::Discarding, true, vec![g(1, "a")]))
            .unwrap();
        s.revoke_for_flip().unwrap();
        let r = s.load().unwrap().unwrap();
        assert!(!r.early_restart && r.phase == Phase::Stopped && r.open);
        assert!(
            !restarts_due(&r),
            "nothing starts on the peer that may be discarding"
        );
        s.close_episode().unwrap();
        let r = s.load().unwrap().unwrap();
        assert!(restarts_due(&r) && restart_host(&r) == RestartHost::ThisHost);
        s.clear();
    }

    #[test]
    fn unreadable_record_is_an_error_not_absent() {
        let s = store("malformed");
        std::fs::write(&s.path, "garbage").unwrap();
        assert!(s.load().is_err());
        s.clear();
        assert_eq!(s.load(), Ok(None));
    }

    #[tokio::test]
    async fn one_round_destroys_every_guest_at_once_and_clears_on_an_empty_scan() {
        let s = store("happy");
        let ctl = FakeGuests::with_guests(vec![g(1, "a"), g(2, "b"), g(3, "c")]);
        let out = establish(&ctl, &s, "K").await;
        assert_eq!(
            out,
            BoundaryOutcome::Clear {
                stopped: vec![g(1, "a"), g(2, "b"), g(3, "c")]
            }
        );
        assert_eq!(
            ctl.stops(),
            ["destroy a,b,c"],
            "a single concurrent destroy round"
        );
        let r = s.load().unwrap().unwrap();
        assert_eq!(r.phase, Phase::Stopped);
        assert!(
            r.owed.iter().all(|o| o.stop_tries == 1),
            "attempts persisted"
        );
        s.clear();
    }

    #[tokio::test]
    async fn the_record_is_written_before_any_domain_is_destroyed() {
        let s = std::sync::Arc::new(store("persist-first"));
        let s2 = s.clone();
        let mut ctl = FakeGuests::with_guests(vec![g(1, "a"), g(2, "b")]);
        ctl.on_stop = Some(Box::new(move || {
            let r = s2.load().unwrap().expect("record exists at destroy time");
            assert_eq!(r.owed.len(), 2);
            assert_eq!(r.phase, Phase::Stopping);
        }));
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        s.clear();
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_xapi_never_holds_the_boundary_up() {
        // XAPI takes a minute to answer the listing and the disable: the
        // boundary goes on at the bound with the local scan alone.
        let s = store("slow-xapi");
        let mut ctl = FakeGuests::with_guests(vec![g(1, "a")]);
        ctl.xapi_delay = Some(std::time::Duration::from_secs(60));
        ctl.extra_names = vec![g(1, "a")];
        let t0 = tokio::time::Instant::now();
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        assert!(
            t0.elapsed() <= XAPI_ADVISORY_BOUND + std::time::Duration::from_secs(1),
            "{:?}",
            t0.elapsed()
        );
        assert_eq!(ctl.stops(), ["destroy a"]);
        // The disable may still land in XAPI: the host is enabled again with
        // the rest, not left to the generic watchdog.
        assert!(
            s.load().unwrap().unwrap().host_disabled,
            "a disable cut off at the bound counts as done"
        );
        s.clear();
    }

    #[tokio::test]
    async fn a_survivor_is_destroyed_once_more_then_holds() {
        let s = store("survivor");
        let mut ctl = FakeGuests::with_guests(vec![g(1, "a"), g(2, "b")]);
        ctl.unkillable = vec![g(1, "a").uuid];
        match establish(&ctl, &s, "K").await {
            BoundaryOutcome::Hold {
                step: Step::ConfirmStopped,
                detail,
                stopped,
            } => {
                assert!(detail.contains('a') && !detail.contains('b'), "{detail}");
                assert_eq!(stopped.len(), 2, "both still owed a restart");
            }
            o => panic!("expected a confirm hold, got {o:?}"),
        }
        assert_eq!(
            ctl.stops(),
            ["destroy a,b", "destroy a"],
            "only the survivor is retried"
        );
        assert_eq!(
            s.load().unwrap().unwrap().phase,
            Phase::Stopping,
            "never recorded as stopped"
        );
        s.clear();
    }

    #[tokio::test]
    async fn a_refused_host_disable_does_not_hold() {
        // Pool HA armed with failures-to-tolerate 1: XAPI refuses the disable.
        let s = store("disable");
        let mut ctl = FakeGuests::with_guests(vec![g(1, "a")]);
        ctl.disable_fails = true;
        assert_eq!(
            run_to_end(&ctl, &s, "K").await,
            BoundaryOutcome::Clear {
                stopped: vec![g(1, "a")]
            }
        );
        assert_eq!(ctl.stops(), ["destroy a"]);
        s.clear();
    }

    #[tokio::test]
    async fn a_domain_xapi_does_not_list_is_stopped_and_owed() {
        let s = store("unlisted");
        let ctl = FakeGuests::with_guests(vec![g(1, "a")]);
        ctl.running.lock().unwrap().push(g(7, "x").uuid);
        match run_to_end(&ctl, &s, "K").await {
            BoundaryOutcome::Clear { stopped } => {
                assert_eq!(stopped.len(), 2);
                assert!(
                    stopped
                        .iter()
                        .any(|o| o.uuid == g(7, "x").uuid && o.name.contains("not listed"))
                );
            }
            o => panic!("expected clear, got {o:?}"),
        }
        assert_eq!(
            ctl.stops().len(),
            1,
            "one round covers listed and unlisted domains"
        );
        s.clear();
    }

    #[tokio::test]
    async fn no_guests_is_a_fast_verified_clear() {
        let s = store("empty");
        let ctl = FakeGuests::with_guests(Vec::new());
        assert_eq!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear {
                stopped: Vec::new()
            }
        );
        assert!(ctl.calls().contains(&"scan".to_string()));
        s.clear();
    }

    #[tokio::test]
    async fn without_xapi_the_local_scan_decides() {
        // A slave cut off from its master: XAPI cannot list, the local scan
        // still finds the guest, destroys it, and clears.
        let s = store("no-xapi");
        let ctl = FakeGuests {
            exported: Some(true),
            listings: std::sync::Mutex::new(vec![Err("xapi down".into())]),
            running: std::sync::Mutex::new(vec![g(1, "a").uuid]),
            ..Default::default()
        };
        assert!(matches!(
            run_to_end(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        assert_eq!(ctl.stops().len(), 1);
        s.clear();
    }

    #[tokio::test]
    async fn a_listed_guest_with_no_domain_here_is_owed_but_not_destroyed() {
        // XAPI still lists "a" here (a stale record); only "b" really runs.
        let s = store("listed-gone");
        let ctl = FakeGuests::with_guests(vec![g(1, "a"), g(2, "b")]);
        ctl.running.lock().unwrap().retain(|u| *u != g(1, "a").uuid);
        match run_to_end(&ctl, &s, "K").await {
            BoundaryOutcome::Clear { stopped } => assert_eq!(stopped.len(), 2),
            o => panic!("expected clear, got {o:?}"),
        }
        assert_eq!(ctl.stops(), ["destroy b"]);
        s.clear();
    }

    #[test]
    fn a_paused_record_with_no_domain_is_stale_like_a_running_one() {
        // The crash action can begin rebuilding the destroyed guest here and
        // fail once the target goes away, leaving XAPI with `paused`,
        // resident here, and no domain. A paused VM cannot be started, so it
        // must be reconciled like a stale `running` one.
        use RecordView::*;
        let here = "this";
        assert_eq!(
            classify_record(super::LocalDomain::None, Some("paused"), Some(here), here),
            StaleHere
        );
        assert_eq!(
            classify_record(super::LocalDomain::None, Some("running"), Some(here), here),
            StaleHere
        );
        assert!(
            matches!(
                classify_record(super::LocalDomain::None, Some("paused"), Some("peer"), here),
                Unknown(_)
            ),
            "paused elsewhere is not back: retried"
        );
        assert_eq!(
            classify_record(
                super::LocalDomain::None,
                Some("running"),
                Some("peer"),
                here
            ),
            Elsewhere
        );
        assert_eq!(
            classify_record(super::LocalDomain::None, Some("halted"), Some(here), here),
            Startable
        );
        assert_eq!(
            classify_record(super::LocalDomain::None, Some("suspended"), None, here),
            Startable
        );
        assert_eq!(
            classify_record(super::LocalDomain::None, None, None, here),
            Startable
        );
        assert!(matches!(
            classify_record(super::LocalDomain::None, Some("paused"), None, here),
            Unknown(_)
        ));
        // A live local domain wins over whatever XAPI says.
        assert_eq!(
            classify_record(super::LocalDomain::Live, Some("halted"), None, here),
            LocalDomain
        );
        assert_eq!(
            classify_record(super::LocalDomain::Live, Some("paused"), Some(here), here),
            LocalDomain
        );
        assert_eq!(
            classify_record(
                super::LocalDomain::Paused,
                Some("running"),
                Some(here),
                here
            ),
            LocalPaused
        );
    }

    #[tokio::test]
    async fn hard_stop_only_guests_both_views_place_here() {
        // a and b: XAPI lists them here and they run here. x runs here but XAPI
        // does not list it on this host: only the local destroy touches it, so
        // the XAPI call can never reach an instance running somewhere else.
        let s = store("xapi-shutdown");
        let ctl = FakeGuests::with_guests(vec![g(1, "a"), g(2, "b")]);
        ctl.running.lock().unwrap().push(g(7, "x").uuid);
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        let calls = ctl.calls();
        let ask = calls
            .iter()
            .position(|c| c == "xapi-shutdown a,b")
            .expect(&format!("{calls:?}"));
        let destroy = calls
            .iter()
            .position(|c| c.starts_with("destroy "))
            .unwrap();
        assert!(ask < destroy, "asked before the destroy: {calls:?}");
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.starts_with("xapi-shutdown"))
                .count(),
            1,
            "once, not per round"
        );
        s.clear();

        // XAPI cannot list: nothing is asked of it, the destroy still runs.
        let s = store("xapi-shutdown-unlisted");
        let mut ctl = FakeGuests::with_guests(vec![g(1, "a")]);
        *ctl.listings.lock().unwrap() = vec![Err("xe timed out".into())];
        ctl.extra_names = vec![g(1, "a")];
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        assert!(
            !ctl.calls().iter().any(|c| c.starts_with("xapi-shutdown")),
            "{:?}",
            ctl.calls()
        );
        assert_eq!(ctl.stops(), ["destroy a"]);
        s.clear();
    }

    #[tokio::test]
    async fn xapi_is_asked_to_hard_stop_only_after_the_hypervisor_poweroff() {
        // On the pool master XAPI answers at once: asked first, its xenopsd
        // pauses the domain mid-teardown and a direct destroy that follows
        // leaves the guest's XAPI calls blocked for 300 s. The poweroff goes
        // first, then XAPI, once, and a powered-off guest is never destroyed
        // directly.
        let s = store("xapi-after-poweroff");
        let ctl = FakeGuests {
            xc: true,
            ..FakeGuests::with_guests(vec![g(1, "a"), g(2, "b")])
        };
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        let calls = ctl.calls();
        let off = calls
            .iter()
            .position(|c| c == "poweroff a,b")
            .expect(&format!("{calls:?}"));
        let ask = calls
            .iter()
            .position(|c| c == "xapi-shutdown a,b")
            .expect(&format!("{calls:?}"));
        assert!(off < ask, "XAPI asked after the poweroff: {calls:?}");
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.starts_with("xapi-shutdown"))
                .count(),
            1,
            "{calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.starts_with("destroy ")),
            "{calls:?}"
        );
        s.clear();

        // A stalled poweroff call: XAPI is asked while it still runs (before
        // xenopsd could collect a domain the call did power off and XAPI
        // record a guest-internal halt that pool HA restarts).
        let s = store("xapi-slow-poweroff");
        let ctl = FakeGuests {
            xc: true,
            poweroff_delay: Some(std::time::Duration::from_secs(2)),
            ..FakeGuests::with_guests(vec![g(1, "a")])
        };
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        let calls = ctl.calls();
        let slow = calls
            .iter()
            .position(|c| c == "poweroff slow")
            .expect(&format!("{calls:?}"));
        let ask = calls
            .iter()
            .position(|c| c == "xapi-shutdown a")
            .expect(&format!("{calls:?}"));
        let off = calls
            .iter()
            .position(|c| c == "poweroff a")
            .expect(&format!("{calls:?}"));
        assert!(
            slow < ask && ask < off,
            "asked during the slow call: {calls:?}"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.starts_with("xapi-shutdown"))
                .count(),
            1,
            "{calls:?}"
        );
        s.clear();
    }

    #[test]
    fn stale_record_reconciled_by_retry_reset_or_agent_restart() {
        use StaleStep::*;
        assert_eq!(
            stale_reconcile_plan(Some(true), true),
            [RetryHardShutdown, ResetPowerState]
        );
        assert_eq!(
            stale_reconcile_plan(Some(false), true),
            [RetryHardShutdown, RestartAgent]
        );
        assert_eq!(
            stale_reconcile_plan(None, true),
            [RetryHardShutdown, RestartAgent]
        );
        assert_eq!(
            stale_reconcile_plan(Some(false), false),
            [RetryHardShutdown],
            "the agent restarts once per episode"
        );
    }

    #[tokio::test]
    async fn stale_running_record_reconciled_with_one_agent_restart() {
        let s = store("stale-record");
        s.save(&rec(Phase::Discarding, false, vec![g(1, "a"), g(2, "b")]))
            .unwrap();
        let ctl = FakeGuests::default();
        *ctl.stale.lock().unwrap() = vec![g(1, "a").uuid, g(2, "b").uuid];
        // "a": stale, reconciled (agent restart allowed), then started.
        assert!(matches!(run_restarts(&ctl, &s, false, false).await,
            RestartTick::Step { ref guest, outcome: StartOutcome::Started, .. } if guest.name == "a"));
        assert!(
            s.load().unwrap().unwrap().agent_restarted,
            "the once-per-episode restart is recorded"
        );
        // "b": the allowance is spent and the reconcile cannot clear it, so
        // it stays owed and the start fails.
        assert!(matches!(
            run_restarts(&ctl, &s, false, false).await,
            RestartTick::Step {
                outcome: StartOutcome::Failed(_),
                start_tries: 1,
                ..
            }
        ));
        assert_eq!(
            ctl.calls(),
            [
                "start a on peer",
                "reconcile a agent_allowed=true",
                "start a on peer",
                "start b on peer",
                "reconcile b agent_allowed=false",
            ]
        );
        s.clear();
    }

    #[tokio::test]
    async fn every_failed_step_holds_without_reaching_later_steps() {
        // Local scan unreadable: hold.
        let s = store("xl");
        let mut ctl = FakeGuests::with_guests(vec![g(1, "a")]);
        ctl.scan_fails = true;
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Hold {
                step: Step::LocalScan,
                ..
            }
        ));
        assert!(ctl.stops().is_empty());
        s.clear();

        // The scan sees the guest's domain but not its SR disk: the scan does
        // not understand this host, so its (empty) answer is not trusted.
        let s = store("blind");
        let mut ctl = FakeGuests::with_guests(vec![g(1, "a")]);
        ctl.scan_blind = vec![g(1, "a").uuid];
        match establish(&ctl, &s, "K").await {
            BoundaryOutcome::Hold {
                step: Step::LocalScan,
                detail,
                ..
            } => assert!(detail.contains("does not see"), "{detail}"),
            o => panic!("expected a scan hold, got {o:?}"),
        }
        s.clear();

        // Record unwritable: hold before any action.
        let s = Store::new("/proc/twinstor-cannot-write-here/loser-boundary");
        let ctl = FakeGuests::with_guests(vec![g(1, "a")]);
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Hold {
                step: Step::Record,
                ..
            }
        ));
        assert!(
            ctl.stops().is_empty(),
            "nothing destroyed without a durable record"
        );
    }

    #[tokio::test]
    async fn the_final_confirm_catches_a_guest_started_after_the_clear() {
        let ctl = FakeGuests::with_guests(Vec::new());
        assert_eq!(confirm_clear(&ctl).await, Ok(()));
        ctl.running.lock().unwrap().push(g(3, "late").uuid);
        assert!(confirm_clear(&ctl).await.unwrap_err().contains("domid 3"));
    }

    #[tokio::test]
    async fn restart_mid_sequence_keeps_obligation_never_stops_from_record() {
        // A previous daemon life recorded and stopped "a"; after the restart
        // the live scan no longer shows it (it is halted). The new run must
        // still owe its restart, and must not issue a stop from the record.
        let s = store("resume");
        let mut old = rec(Phase::Stopping, false, vec![g(1, "a")]);
        old.key = "OLD".into();
        s.save(&old).unwrap();
        let ctl = FakeGuests::with_guests(vec![g(2, "b")]);
        let out = run_to_end(&ctl, &s, "NEW").await;
        assert_eq!(
            out,
            BoundaryOutcome::Clear {
                stopped: vec![g(1, "a"), g(2, "b")]
            }
        );
        assert_eq!(
            ctl.stops(),
            ["destroy b"],
            "the record never authorizes a stop"
        );
        assert_eq!(s.load().unwrap().unwrap().key, "NEW");
        s.clear();
    }

    #[tokio::test]
    async fn malformed_record_holds_before_any_action() {
        let s = store("bad");
        std::fs::write(&s.path, "phase=stopped").unwrap();
        let ctl = FakeGuests::with_guests(vec![g(1, "a")]);
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Hold {
                step: Step::Record,
                ..
            }
        ));
        assert!(ctl.calls().is_empty());
        s.clear();
    }

    #[test]
    fn restarts_wait_for_discard_episode_end_or_lineage_hold() {
        let closed = |phase| Record {
            open: false,
            ..rec(phase, false, vec![])
        };
        assert!(!restarts_due(&rec(Phase::Stopping, false, vec![])));
        assert!(!restarts_due(&rec(Phase::Stopped, false, vec![])));
        assert!(restarts_due(&rec(Phase::Discarding, false, vec![])));
        assert!(
            restarts_due(&closed(Phase::Stopped)),
            "episode positively converged"
        );
        assert!(
            restarts_due(&rec(Phase::Stopping, true, vec![])),
            "lineage-decided hold"
        );
        assert_eq!(
            restart_host(&rec(Phase::Discarding, false, vec![])),
            RestartHost::Peer
        );
        assert_eq!(
            restart_host(&rec(Phase::Stopping, true, vec![])),
            RestartHost::Peer
        );
        assert_eq!(
            restart_host(&rec(Phase::Stopped, false, vec![])),
            RestartHost::ThisHost,
            "flipped to winner / converged without discarding: this host kept its data"
        );
    }

    #[tokio::test]
    async fn restarts_one_per_round_keep_failures_owed_and_never_give_up() {
        let s = store("restart");
        s.save(&rec(
            Phase::Discarding,
            false,
            vec![g(1, "a"), g(2, "b"), g(3, "c")],
        ))
        .unwrap();
        let ctl = FakeGuests::default();
        *ctl.start.lock().unwrap() = vec![
            StartOutcome::Started,
            StartOutcome::AlreadyRunning,
            StartOutcome::Failed("no room".into()),
        ];
        assert!(matches!(run_restarts(&ctl, &s, false, false).await,
            RestartTick::Step { ref guest, outcome: StartOutcome::Started, remaining: 2, host: RestartHost::Peer, .. } if guest.name == "a"));
        assert!(matches!(
            run_restarts(&ctl, &s, false, false).await,
            RestartTick::Step {
                outcome: StartOutcome::AlreadyRunning,
                remaining: 1,
                ..
            }
        ));
        assert!(matches!(
            run_restarts(&ctl, &s, false, false).await,
            RestartTick::Step {
                outcome: StartOutcome::Failed(_),
                start_tries: 1,
                remaining: 1,
                ..
            }
        ));
        assert_eq!(
            ctl.calls(),
            ["start a on peer", "start b on peer", "start c on peer"]
        );
        // Far past the escalation threshold the obligation is still there.
        for _ in 0..MAX_START_ATTEMPTS + 2 {
            let _ = run_restarts(&ctl, &s, false, false).await;
        }
        assert!(s.exists(), "an owed restart is never silently dropped");
        *ctl.start.lock().unwrap() = vec![StartOutcome::Started];
        assert!(matches!(
            run_restarts(&ctl, &s, false, false).await,
            RestartTick::Step { remaining: 0, .. }
        ));
        assert!(!s.exists(), "nothing owed: record cleared");
    }

    #[tokio::test]
    async fn restarts_idle_until_due_and_report_a_malformed_record() {
        let s = store("notdue");
        s.save(&rec(Phase::Stopped, false, vec![g(1, "a")]))
            .unwrap();
        let ctl = FakeGuests::default();
        assert_eq!(
            run_restarts(&ctl, &s, false, false).await,
            RestartTick::Idle
        );
        assert!(ctl.calls().is_empty());
        let mut ctl2 = FakeGuests::default();
        ctl2.no_hosts = true;
        s.close_episode().unwrap();
        assert_eq!(
            run_restarts(&ctl2, &s, false, false).await,
            RestartTick::NoTarget
        );
        assert!(
            ctl2.calls().is_empty(),
            "no start without a resolved target host"
        );
        std::fs::write(&s.path, "garbage").unwrap();
        assert!(matches!(
            run_restarts(&ctl, &s, false, false).await,
            RestartTick::Malformed(_)
        ));
        s.clear();
    }

    #[test]
    fn xl_verbose_parses_spaced_names_and_skips_dom0() {
        let text = "Name                                        ID   Mem VCPUs\tState\tTime(s)   UUID                            Reason-Code\tSecurity Label\n\
Domain-0                                     0  4096     4     r-----    1234.5 00000000-0000-0000-0000-000000000000        -                -\n\
HVM domU                                    12  2048     2     -b----      10.1 AABBCCDD-0000-4000-8000-000000000001        -                -\n\
web                                         13  1024     1     -b----       5.0 aabbccdd-0000-4000-8000-000000000002        -                -\n";
        assert_eq!(
            parse_xl_verbose(text),
            Some(vec![
                (12, "aabbccdd-0000-4000-8000-000000000001".into()),
                (13, "aabbccdd-0000-4000-8000-000000000002".into()),
            ])
        );
        assert_eq!(
            parse_xl_verbose("Name ID\nsomething unexpected here\n"),
            None,
            "an unknown format must not read as no guests"
        );
        assert_eq!(parse_xl_verbose("Name ID Mem\n"), Some(Vec::new()));
    }

    #[test]
    fn xl_verbose_takes_the_uuid_column_not_a_uuid_in_the_name() {
        let text = "Name                                        ID   Mem VCPUs\tState\tTime(s)   UUID                            Reason-Code\tSecurity Label\n\
restore-11111111-2222-3333-4444-555555555555 7  1024     1     -b----       5.0 aabbccdd-0000-4000-8000-000000000007        -                -\n\
11111111-2222-3333-4444-555555555555        8  1024     1     -b----       5.0 aabbccdd-0000-4000-8000-000000000008        -                -\n";
        assert_eq!(
            parse_xl_verbose(text),
            Some(vec![
                (7, "aabbccdd-0000-4000-8000-000000000007".into()),
                (8, "aabbccdd-0000-4000-8000-000000000008".into()),
            ])
        );
    }

    #[test]
    fn xenstore_scan_finds_domains_with_a_block_backend_on_the_sr() {
        let sr = "5e5e0008-0000-4000-8000-000000000008";
        let text = format!(
            "/local/domain/0/backend/vbd3/5/768/params = \"/dev/sm/backend/{sr}/0b1c2d3e-0000-0000-0000-000000000001\"\n\
/local/domain/0/backend/vbd3/5/768/frontend-id = \"5\"\n\
/local/domain/0/backend/vbd3/6/768/params = \"/dev/sm/backend/99999999-0000-0000-0000-000000000000/vdi\"\n\
/local/domain/0/backend/vif/6/0/mac = \"aa:bb:cc:dd:ee:ff\"\n\
/local/domain/0/backend/vbd/9/51712/params = \"/dev/VG_XenStorage-{}/VHD-x\"\n",
            sr.to_uppercase()
        );
        assert_eq!(sr_domids_from_xenstore(&text, sr), vec![5, 9]);
        assert_eq!(sr_domids_from_xenstore("", sr), Vec::<u32>::new());
    }

    #[test]
    fn selects_only_resident_guests_with_an_attached_disk_on_the_sr() {
        let vms = "uuid ( RO)           : 11111111-0000-0000-0000-000000000001\n     name-label ( RW): on sr\n\n\n\
uuid ( RO)           : 11111111-0000-0000-0000-000000000002\n     name-label ( RW): local only\n\n";
        let vdis = "vdi-sr-1,vdi-sr-2";
        let vbds = "vm-uuid ( RO): 11111111-0000-0000-0000-000000000001\n    vdi-uuid ( RO): vdi-sr-2\n\n\n\
vm-uuid ( RO): 11111111-0000-0000-0000-000000000002\n    vdi-uuid ( RO): vdi-local\n\n\n\
vm-uuid ( RO): 99999999-0000-0000-0000-000000000009\n    vdi-uuid ( RO): vdi-sr-1\n";
        assert_eq!(
            select_sr_guests(vms, vdis, vbds),
            vec![GuestRef {
                uuid: "11111111-0000-0000-0000-000000000001".into(),
                name: "on sr".into()
            }]
        );
    }

    /// `xl list -v` rows of a guest powered off at the hypervisor, as seen on
    /// an XCP-ng 8.3 dom0 right after the call (`-b-s--`) and while xenopsd
    /// tears it down (`--ps--`): no longer running code, still present.
    #[test]
    fn a_powered_off_domain_runs_no_code_but_is_still_present() {
        let out = "Name                                        ID   Mem VCPUs\tState\tTime(s)   UUID                            Reason-Code\tSecurity Label\n\
Domain-0                                     0  1424     2     r-----    2759.7 5e5e0009-0000-4000-8000-000000000009        -                -\n\
Test 3                                      14  1023     1     -b----       9.1 5e5e000c-0000-4000-8000-00000000000c        -                -\n\
Test 2                                      15  1023     1     --ps--      15.0 5e5e000a-0000-4000-8000-00000000000a        -                -\n\
Test 4                                      16  1023     1     -b-s--       3.2 5e5e0010-0000-4000-8000-000000000010        -                -\n";
        assert_eq!(
            parse_xl_verbose(out),
            Some(vec![(
                14,
                "5e5e000c-0000-4000-8000-00000000000c".to_string()
            )])
        );
        assert_eq!(parse_xl_all_domids(out), Some(vec![0, 14, 15, 16]));
        assert_eq!(
            local_domain_state(out, "5e5e000a-0000-4000-8000-00000000000a"),
            Some(LocalDomain::None)
        );
    }

    #[test]
    fn the_poweroff_helper_output_is_read_strictly() {
        assert_eq!(
            parse_poweroff_output(true, "done\n"),
            PoweroffReport::Done { failed: vec![] }
        );
        assert_eq!(
            parse_poweroff_output(true, "done\nfail 15 (3, 'No such process')\n"),
            PoweroffReport::Done {
                failed: vec!["15 (3, 'No such process')".into()]
            }
        );
        assert_eq!(
            parse_poweroff_output(false, "noxc No module named 'xen'\n"),
            PoweroffReport::Unavailable
        );
        assert_eq!(
            parse_poweroff_output(true, ""),
            PoweroffReport::Unavailable,
            "no output is not success"
        );
        assert_eq!(
            parse_poweroff_output(true, "done\nTraceback (most recent call last):\n"),
            PoweroffReport::Unavailable
        );
        assert_eq!(
            parse_poweroff_output(false, "done\n"),
            PoweroffReport::Unavailable,
            "a failed exit is never success"
        );
    }

    #[test]
    fn the_helper_never_powers_off_dom0() {
        assert!(POWEROFF_PY.contains("if d == 0:"));
        assert!(
            POWEROFF_PY.contains("domain_shutdown(d, 0)"),
            "reason 0 = poweroff"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn guests_are_powered_off_not_destroyed_when_the_hypervisor_call_works() {
        let s = std::sync::Arc::new(store("poweroff"));
        let s2 = s.clone();
        let mut ctl = FakeGuests::with_guests(vec![g(1, "a"), g(2, "b")]);
        ctl.xc = true;
        ctl.on_stop = Some(Box::new(move || {
            let r = s2
                .load()
                .unwrap()
                .expect("record exists before the poweroff");
            assert_eq!(r.owed.len(), 2);
        }));
        let out = establish(&ctl, &s, "K").await;
        assert_eq!(
            out,
            BoundaryOutcome::Clear {
                stopped: vec![g(1, "a"), g(2, "b")]
            }
        );
        assert!(
            ctl.calls().contains(&"poweroff a,b".to_string()),
            "{:?}",
            ctl.calls()
        );
        assert!(
            ctl.stops().is_empty(),
            "no direct destroy: xenopsd collected both"
        );
        assert!(
            ctl.calls().contains(&"present".to_string()),
            "waited for xenopsd's teardown"
        );
        let calls = ctl.calls();
        let (p, sweep) = (
            calls.iter().position(|c| c == "poweroff a,b").unwrap(),
            calls.iter().position(|c| c == "dps").unwrap(),
        );
        assert!(p < sweep, "the disk handles are swept after the stop");
        s.clear();
    }

    #[tokio::test(start_paused = true)]
    async fn a_domain_still_running_after_the_poweroff_is_destroyed_at_once() {
        let s = store("poweroff-ignored");
        let mut ctl = FakeGuests::with_guests(vec![g(1, "a"), g(2, "b")]);
        ctl.xc = true;
        ctl.poweroff_ignored = vec![g(1, "a").uuid];
        let t0 = tokio::time::Instant::now();
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        assert_eq!(ctl.stops(), ["destroy a"], "only the one still running");
        assert!(
            t0.elapsed() < XENOPSD_TEARDOWN_WAIT,
            "b was collected; no full wait"
        );
        assert_eq!(
            s.load()
                .unwrap()
                .unwrap()
                .owed
                .iter()
                .find(|o| o.guest.name == "a")
                .unwrap()
                .stop_tries,
            1
        );
        s.clear();
    }

    #[tokio::test(start_paused = true)]
    async fn a_powered_off_domain_is_destroyed_only_after_xenopsd_had_its_window() {
        let s = store("teardown-stuck");
        let mut ctl = FakeGuests::with_guests(vec![g(1, "a"), g(2, "b")]);
        ctl.xc = true;
        ctl.teardown_stuck = vec![g(2, "b").uuid];
        let t0 = tokio::time::Instant::now();
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        assert!(t0.elapsed() >= XENOPSD_TEARDOWN_WAIT, "{:?}", t0.elapsed());
        assert_eq!(
            ctl.stops(),
            ["destroy b"],
            "only the uncollected one, and only after the window"
        );
        let calls = ctl.calls();
        let destroy = calls.iter().position(|c| c == "destroy b").unwrap();
        let polls = calls[..destroy].iter().filter(|c| *c == "present").count();
        assert!(
            polls as u128 >= XENOPSD_TEARDOWN_WAIT.as_millis() / TEARDOWN_POLL.as_millis(),
            "{polls} polls"
        );
        s.clear();
    }

    #[tokio::test]
    async fn without_the_hypervisor_binding_the_domains_are_destroyed_directly() {
        let s = store("no-xc");
        let ctl = FakeGuests::with_guests(vec![g(1, "a")]);
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        assert!(ctl.calls().contains(&"poweroff unavailable".to_string()));
        assert_eq!(ctl.stops(), ["destroy a"]);
        assert!(
            !ctl.calls().contains(&"present".to_string()),
            "no teardown wait on the destroy path"
        );
        s.clear();
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_on_the_sr_after_the_discard_is_a_quiet_pass() {
        let s = store("rebuilt-none");
        let mut ctl = FakeGuests::with_guests(vec![g(1, "a")]);
        ctl.xc = true;
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        assert_eq!(catch_rebuilt(&ctl, &s, None).await, Ok(Vec::new()));
        assert_eq!(s.load().unwrap().unwrap().owed.len(), 1, "still owed");
        s.clear();
    }

    #[tokio::test(start_paused = true)]
    async fn guest_rebuilt_across_discard_is_stopped_and_named() {
        let s = store("rebuilt");
        let mut ctl = FakeGuests::with_guests(vec![g(1, "a"), g(2, "b")]);
        ctl.xc = true;
        ctl.guest_vdis = vec![(g(1, "a").uuid, vec![vdi(7)])];
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        // The toolstack rebuilt "a" in place after the last pre-discard check.
        ctl.running.lock().unwrap().push(g(1, "a").uuid);
        let found = catch_rebuilt(&ctl, &s, Some(5)).await.unwrap();
        assert_eq!(
            found,
            vec![Rebuilt {
                guest: g(1, "a"),
                domid: 1,
                vdis: vec![vdi(7)],
                ha_may_restart: false
            }]
        );
        assert!(ctl.running.lock().unwrap().is_empty(), "stopped again");
        assert!(
            ctl.calls().contains(&"xapi-stop a".to_string()),
            "through XAPI first, so pool HA leaves it down: {:?}",
            ctl.calls()
        );
        let r = s.load().unwrap().unwrap();
        assert_eq!(
            r.guests(),
            vec![g(2, "b")],
            "the suspect guest is no longer owed a restart"
        );
        let body = rebuilt_alert_body(&found, Some("5e5e000f-0000-4000-8000-00000000000f"));
        assert!(
            body.contains(&g(1, "a").uuid) && body.contains(&vdi(7)),
            "{body}"
        );
        assert!(body.contains("vhd-util check -n /dev/VG_XenStorage-5e5e000f-0000-4000-8000-00000000000f/VHD-<vdi>"), "{body}");
        assert!(
            body.contains("twinstor does not restart them")
                && !body.contains("ha-restart-priority"),
            "{body}"
        );
        assert!(
            body.contains("\"method\":\"DP.diagnostics\"")
                && body.contains("\"method\":\"DP.destroy\""),
            "the datapath check comes before the LV is activated by hand: {body}"
        );
        s.clear();
    }

    #[tokio::test(start_paused = true)]
    async fn unstoppable_suspect_guest_is_powered_off_and_alert_warns_ha() {
        let s = store("rebuilt-no-xapi");
        let mut ctl = FakeGuests::with_guests(vec![g(1, "a")]);
        ctl.xc = true;
        ctl.hard_stop_fails = vec![g(1, "a").uuid];
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        ctl.running.lock().unwrap().push(g(1, "a").uuid);
        let found = catch_rebuilt(&ctl, &s, None).await.unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[0].ha_may_restart);
        assert!(ctl.running.lock().unwrap().is_empty());
        let body = rebuilt_alert_body(&found, None);
        assert!(
            body.contains(&format!(
                "xe vm-param-set uuid={} ha-restart-priority=",
                g(1, "a").uuid
            )),
            "{body}"
        );
        s.clear();
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_xapi_never_keeps_a_suspect_guest_running() {
        // Each suspect guest may be writing stale VHD metadata into the kept
        // copy: XAPI gets one short bound for all of them, not one per guest,
        // then the hypervisor stops what is left.
        let s = store("rebuilt-slow-xapi");
        let mut ctl = FakeGuests::with_guests(vec![g(1, "a"), g(2, "b")]);
        ctl.xc = true;
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        ctl.xapi_delay = Some(std::time::Duration::from_secs(120));
        ctl.running
            .lock()
            .unwrap()
            .extend([g(1, "a").uuid, g(2, "b").uuid]);
        let t0 = tokio::time::Instant::now();
        let found = catch_rebuilt(&ctl, &s, None).await.unwrap();
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(30),
            "{:?}",
            t0.elapsed()
        );
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|r| r.ha_may_restart));
        assert!(ctl.running.lock().unwrap().is_empty());
        s.clear();
    }

    #[tokio::test(start_paused = true)]
    async fn a_guest_started_after_the_withdrawal_is_left_running() {
        // Its domid is above the highest one present when this copy was
        // withdrawn: it reaches the SR through the winner's portal only (an
        // owed restart, pool HA after the discard).
        let s = store("rebuilt-later");
        s.save(&rec(Phase::Discarding, false, vec![g(9, "late")]))
            .unwrap();
        let ctl = FakeGuests::with_guests(vec![g(9, "late")]);
        assert_eq!(catch_rebuilt(&ctl, &s, Some(8)).await, Ok(Vec::new()));
        assert_eq!(ctl.running.lock().unwrap().len(), 1, "{:?}", ctl.calls());
        assert!(!ctl.calls().iter().any(|c| c.starts_with("xapi-stop")
            || c.starts_with("poweroff")
            || c.starts_with("destroy")));
        assert_eq!(
            s.load().unwrap().unwrap().owed.len(),
            1,
            "still owed, untouched"
        );
        s.clear();
        // Unless its disk's datapath on this host could not be released: it
        // then reattached through the stale one, whatever its domid.
        let s = store("rebuilt-later-leaked");
        let mut r = rec(Phase::Discarding, false, vec![g(9, "late")]);
        r.leaked = vec![vdi(3)];
        s.save(&r).unwrap();
        let ctl = FakeGuests {
            guest_vdis: vec![(g(9, "late").uuid, vec![vdi(3)])],
            ..FakeGuests::with_guests(vec![g(9, "late")])
        };
        let found = catch_rebuilt(&ctl, &s, Some(8)).await.unwrap();
        assert_eq!(found.len(), 1, "{:?}", ctl.calls());
        assert!(ctl.running.lock().unwrap().is_empty());
        s.clear();
    }

    #[tokio::test(start_paused = true)]
    async fn a_rebuilt_guest_that_will_not_stop_is_an_error() {
        let s = store("rebuilt-stuck");
        let mut ctl = FakeGuests::with_guests(vec![g(1, "a")]);
        ctl.xc = true;
        assert!(matches!(
            establish(&ctl, &s, "K").await,
            BoundaryOutcome::Clear { .. }
        ));
        ctl.running.lock().unwrap().push(g(1, "a").uuid);
        ctl.poweroff_ignored = vec![g(1, "a").uuid];
        ctl.unkillable = vec![g(1, "a").uuid];
        let e = catch_rebuilt(&ctl, &s, None).await.unwrap_err();
        assert!(e.contains("still running"), "{e}");
        s.clear();
    }

    #[test]
    fn the_rebuilt_alert_without_vdis_says_how_to_list_them() {
        let r = Rebuilt {
            guest: GuestRef {
                uuid: "u".into(),
                name: "domid 9".into(),
            },
            domid: 9,
            vdis: vec![],
            ha_may_restart: false,
        };
        let body = rebuilt_alert_body(&[r], None);
        assert!(
            body.contains("xe vbd-list vm-uuid=<vm>") && body.contains("VG_XenStorage-<sr-uuid>"),
            "{body}"
        );
    }
}
