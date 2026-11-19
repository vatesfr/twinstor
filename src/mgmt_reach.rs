//! Management reachability of the peer (the predicate P) and its settle clock.
//!
//! A TCP connect to the peer's management address on port 443, bounded at
//! 3 s, every 10 s. The connection completing, or being refused, counts as an
//! answer: the host is there and its network path works. A timeout, or the
//! network reporting the host or network unreachable, does not.
//!
//! P holds when at least 4 of the last 6 probes answered and the last probe
//! answered. The settle clock `p_true_since` starts when P first becomes true
//! and resets only after k consecutive failed probes, with
//! k = clamp(heartbeat timeout / 20 s, 3, 6). A real cut fails every probe and
//! resets it within k x 10 s, at most half the heartbeat timeout, so any
//! outage long enough to start an XHA fault handler restarts the settle; an
//! isolated lost probe on a lossy link does not.
//!
//! P measures the network only. stunnel keeps answering on 443 through a
//! peer reboot (after its first seconds) and an xapi restart, so every repair
//! keeps its own maturity on top of P.
//!
//! Every HA action that can change the pool across a management partition is
//! checked against P where it fires (see `gate`). P uses the management
//! address the pool already uses and the port xapi already listens on: no new
//! port and no new deployment requirement.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Probe cadence.
pub const PROBE_EVERY: Duration = Duration::from_secs(10);
/// Bound on one TCP connect.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// Probes kept in the window.
pub const WINDOW: usize = 6;
/// Answered probes the window needs for P.
pub const ANSWERS_NEEDED: usize = 4;
/// The settle S: a repair triggered by the absence of an answer waits this
/// long after P became true. It covers the natural rejoins observed on the
/// rig (20 s for an ex-master, up to 274 s for a slave) and a running slave's
/// master-connection backoff (256 s plus about 23 s of host enable).
pub const SETTLE: Duration = Duration::from_secs(330);
/// Returning-slave toolstack restart: the local xapi must have stayed silent
/// this long, with the same process ID, after P became true. Longer than the
/// master-connection backoff plus two xapi emergency self-restarts.
pub const STUCK_XAPI_AFTER: Duration = Duration::from_secs(600);
/// The management port xapi serves through stunnel.
pub const XAPI_PORT: u16 = 443;
/// A snapshot whose last probe is older than this reads P false: the prober
/// runs every 10 s (plus at most a few bounded probes and one bounded xe
/// read), so an older value means it stopped.
pub const FRESH_MAX: Duration = Duration::from_secs(35);
/// Address-cache refresh pace, in probe rounds (5 minutes), whatever P says.
const CACHE_REFRESH_EVERY: u32 = 30;

/// Where the address-source cache lives (refreshed whenever a live
/// `xe host-param-get address` answers).
pub fn address_cache_path() -> String {
    std::env::var("TWINSTOR_PEER_MGMT_CACHE")
        .unwrap_or_else(|_| crate::heal_state::state_path("/var/lib/twinstor/peer-mgmt-address"))
}

/// The snapshot the prober publishes for `twinstor status` (tmpfs).
pub static STATUS_PATH: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| crate::heal_state::state_path("/run/twinstor/mgmt-reach"));

/// How many consecutive failed probes reset the settle clock, from the XHA
/// heartbeat timeout: clamp(T_hb / 20 s, 3, 6), which is 6 at T_hb = 120 s.
pub fn settle_reset_after(heartbeat_timeout_secs: u32) -> u32 {
    (heartbeat_timeout_secs / 20).clamp(3, 6)
}

/// Probe history and settle clock. Pure: the prober feeds it, the daemon reads
/// `view` snapshots.
#[derive(Debug, Clone)]
pub struct ReachState {
    /// Most recent probe last; `true` = answered.
    history: VecDeque<bool>,
    consecutive_failures: u32,
    p_true_since: Option<Instant>,
    reset_after: u32,
    address: Option<String>,
    last_probe: Option<Instant>,
}

impl Default for ReachState {
    fn default() -> Self {
        Self::new(settle_reset_after(120))
    }
}

impl ReachState {
    pub fn new(reset_after: u32) -> Self {
        Self {
            history: VecDeque::with_capacity(WINDOW),
            consecutive_failures: 0,
            p_true_since: None,
            reset_after: reset_after.max(1),
            address: None,
            last_probe: None,
        }
    }

    /// Change k (the heartbeat timeout can be read only while HA runs).
    pub fn set_reset_after(&mut self, k: u32) {
        self.reset_after = k.max(1);
    }

    pub fn set_address(&mut self, addr: Option<String>) {
        self.address = addr;
    }

    pub fn address(&self) -> Option<&str> {
        self.address.as_deref()
    }

    /// Record one probe. A probe that could not be sent for want of an
    /// address counts as unanswered: with no trustworthy address P is
    /// unknown, which is treated as false.
    pub fn record(&mut self, answered: bool, now: Instant) {
        self.last_probe = Some(now);
        if self.history.len() == WINDOW {
            self.history.pop_front();
        }
        self.history.push_back(answered);
        if answered {
            self.consecutive_failures = 0;
        } else {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
            if self.consecutive_failures >= self.reset_after {
                self.p_true_since = None;
            }
        }
        if self.p() && self.p_true_since.is_none() {
            self.p_true_since = Some(now);
        }

    }

    /// The predicate P: at least 4 of the last 6 probes answered and the last
    /// one did.
    pub fn p(&self) -> bool {
        self.history.back() == Some(&true) && self.answered() >= ANSWERS_NEEDED
    }

    fn answered(&self) -> usize {
        self.history.iter().filter(|a| **a).count()
    }

    /// The snapshot at `now`. A snapshot whose last probe is older than
    /// `FRESH_MAX` (the prober stopped) reads P false and `stale`.
    pub fn view(&self, now: Instant) -> PView {
        let stale = self
            .last_probe
            .is_none_or(|t| now.saturating_duration_since(t) > FRESH_MAX);
        PView {
            p: self.p() && !stale,
            last_answered: self.history.back() == Some(&true) && !stale,
            stale: stale && self.last_probe.is_some(),
            answered: self.answered() as u8,
            probes: self.history.len() as u8,
            true_for: self.p_true_since.map(|t| now.saturating_duration_since(t)),

            address_known: self.address.is_some(),
        }
    }
}

/// A snapshot of P for one decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PView {
    /// The predicate itself.
    pub p: bool,
    /// The most recent probe answered (fresh), whatever the window says.
    pub last_answered: bool,
    /// Answered probes in the window, and probes in the window.
    pub answered: u8,
    pub probes: u8,
    /// How long the settle clock has run (`None` while it is reset).
    pub true_for: Option<Duration>,

    pub address_known: bool,
    /// The prober's last probe is older than `FRESH_MAX`.
    pub stale: bool,
}

impl PView {
    /// P holds now ("P matured" in the design: its own 4-of-6 window).
    pub fn matured(&self) -> bool {
        self.p
    }

    /// P holds and has held, by the settle clock, for at least S.
    pub fn settled(&self) -> bool {
        self.p && self.true_for.is_some_and(|d| d >= SETTLE)
    }

    /// For tests and status: P matured and settled.
    #[cfg(test)]
    pub fn settled_for_test() -> Self {
        Self {
            p: true,
            last_answered: true,
            answered: WINDOW as u8,
            probes: WINDOW as u8,
            true_for: Some(SETTLE + Duration::from_secs(1)),
            address_known: true,
            stale: false,
        }
    }
}

/// What triggers a repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The absence of an answer: the peer's xapi not answering, the local
    /// xhad not Online, a join error, the peer reading not live. Waits S.
    Absence,
    /// Positive evidence XAPI never leaves by itself (`ArmedMismatch`, this
    /// host disarmed under an armed pool). Does not wait S.
    Positive,
}

/// The fire-time verdict of the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Pass,
    /// P is false (or unknown): the peer's management address is not
    /// answering reliably.
    NotReachable,
    /// P holds but the settle has this much left.
    Settling { remaining: Duration },
}

/// The gate an HA action checks at the moment it fires. An action that finds
/// it closed retries at a later tick.
pub fn gate(view: &PView, trigger: Trigger) -> Gate {
    if !view.matured() {
        return Gate::NotReachable;
    }
    match trigger {
        Trigger::Positive => Gate::Pass,
        Trigger::Absence => {
            let run = view.true_for.unwrap_or_default();
            if run >= SETTLE {
                Gate::Pass
            } else {
                Gate::Settling {
                    remaining: SETTLE - run,
                }
            }
        }
    }
}

/// The startup-loop lock: on a host that is locally armed, whose xhad is not
/// Online, and for which P is not true, no local disarm, master transition or
/// reset-master runs. XAPI parks such a host in its HA startup loop, and the
/// armed flag is what keeps it parked; clearing it releases a second master or
/// sets up the armed-mismatch wedge. Whatever the peer's replication state
/// says: a host cut off from everything cannot see its peer.
pub fn startup_loop_locked(local_armed: Option<bool>, xhad_online: bool, p: bool) -> bool {
    local_armed == Some(true) && !xhad_online && !p
}

/// The returning-slave stuck-xapi test: the local xapi has not answered,
/// with the same process ID (or with no xapi process at all), for
/// `STUCK_XAPI_AFTER` since P became true. A process ID change means xapi is
/// restarting itself (its emergency loop), which a toolstack restart would
/// only duplicate, so it restarts the clock. The clock starts only while P
/// holds and resets only when xapi answers or the settle clock resets; one
/// lost probe does not reset it (the caller checks P again when it fires).
#[derive(Debug, Clone, Default)]
pub struct StuckXapi {
    since: Option<Instant>,
    pid: String,
}

impl StuckXapi {
    /// Feed one observation; returns whether xapi is stuck. `pid` is empty
    /// when no xapi process runs. `settle_running`: the settle clock has not
    /// reset (see `PView::true_for`).
    pub fn observe(
        &mut self,
        xapi_answered: bool,
        pid: &str,
        p: bool,
        settle_running: bool,
        now: Instant,
    ) -> bool {
        if xapi_answered || !settle_running {
            self.since = None;
            self.pid.clear();
            return false;
        }
        match self.since {
            None if !p => return false,
            None => {
                self.since = Some(now);
                self.pid = pid.to_string();
            }
            Some(_) if self.pid != pid => {
                self.since = Some(now);
                self.pid = pid.to_string();
            }
            Some(_) => {}
        }
        self.since
            .is_some_and(|t| now.saturating_duration_since(t) >= STUCK_XAPI_AFTER)
    }

    pub fn silent_for(&self, now: Instant) -> Option<Duration> {
        self.since.map(|t| now.saturating_duration_since(t))
    }
}

/// The peer's management address from the sources the design names, freshest
/// first: pool.conf `slave:<addr>` when the peer is this host's master (not
/// one of this host's own addresses), the TwinStor cache, and local.db
/// `ha_peers`. Returns the address and, when another source disagrees, a
/// description for a WARN. The replication address is never a fallback.
pub fn resolve_address(
    pool_conf_master: Option<&str>,
    cache: Option<&str>,
    ha_peers: Option<&str>,
) -> (Option<String>, Option<String>) {
    let sources = [
        ("pool.conf", pool_conf_master),
        ("cache", cache),
        ("ha_peers", ha_peers),
    ];
    let present: Vec<(&str, &str)> = sources
        .iter()
        .filter_map(|(n, a)| a.filter(|s| !s.trim().is_empty()).map(|a| (*n, a.trim())))
        .collect();
    let Some((name, chosen)) = present.first().copied() else {
        return (None, None);
    };
    let disagree: Vec<String> = present
        .iter()
        .skip(1)
        .filter(|(_, a)| *a != chosen)
        .map(|(n, a)| format!("{n}={a}"))
        .collect();
    let warn = (!disagree.is_empty()).then(|| {
        format!(
            "peer management address sources disagree: using {name}={chosen}, also {}",
            disagree.join(", ")
        )
    });
    (Some(chosen.to_string()), warn)
}

/// The distinct addresses the sources name, freshest first.
pub fn distinct_candidates(
    pool_conf_master: Option<&str>,
    cache: Option<&str>,
    ha_peers: Option<&str>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for a in [pool_conf_master, cache, ha_peers].into_iter().flatten() {
        let a = a.trim();
        if !a.is_empty() && !out.iter().any(|o| o == a) {
            out.push(a.to_string());
        }
    }
    out
}

/// The address to probe this round. While P holds, keep the address it was
/// built on (`current`): it answers, so there is nothing to switch to. While P
/// is false, the freshest candidate that answers (`answers[i]` for
/// `candidates[i]`); when none does, the freshest. Returns the address and
/// whether it answered.
pub fn pick_address(
    p: bool,
    current: Option<&str>,
    candidates: &[String],
    answers: &[bool],
) -> (Option<String>, bool) {
    if p && let Some(c) = current {
        let answered = candidates
            .iter()
            .position(|x| x == c)
            .and_then(|i| answers.get(i).copied())
            .unwrap_or(false);
        return (Some(c.to_string()), answered);
    }
    if let Some(i) = answers.iter().position(|a| *a) {
        return (candidates.get(i).cloned(), true);
    }
    (candidates.first().cloned(), false)
}

/// The peer's address in local.db's `ha_peers` row, which holds
/// `(('<uuid>' '<addr>') ('<uuid>' '<addr>'))` for every host and is written
/// only at an HA enable. `None` when the row is absent, unparseable, or does
/// not name exactly one other host.
pub fn ha_peers_address(local_db: &str, own_uuid: &str) -> Option<String> {
    let idx = local_db.find("\"ha_peers\"")?;
    let rest = &local_db[idx..];
    let vidx = rest.find("value=\"")? + "value=\"".len();
    let value = rest[vidx..].split('"').next()?;
    let value = value.replace("&apos;", "'").replace("&#39;", "'");
    let quoted: Vec<&str> = value.split('\'').skip(1).step_by(2).collect();
    let others: Vec<&str> = quoted
        .chunks(2)
        .filter(|c| c.len() == 2 && c[0] != own_uuid)
        .map(|c| c[1])
        .collect();
    match others.as_slice() {
        [one] if !one.is_empty() => Some((*one).to_string()),
        _ => None,
    }
}

/// Whether local.db's `ha_peers` names the given host. `None` when the row
/// cannot be read.
pub fn ha_peers_names(local_db: &str, uuid: &str) -> Option<bool> {
    let idx = local_db.find("\"ha_peers\"")?;
    let rest = &local_db[idx..];
    let vidx = rest.find("value=\"")? + "value=\"".len();
    let value = rest[vidx..].split('"').next()?;
    Some(value.contains(uuid))
}

/// `HeartbeatTimeout` from xhad.conf, the XHA heartbeat timeout T_hb.
pub fn heartbeat_timeout(xhad_conf: &str) -> Option<u32> {
    let start = xhad_conf.find("<HeartbeatTimeout>")? + "<HeartbeatTimeout>".len();
    let end = xhad_conf[start..].find('<')? + start;
    xhad_conf[start..end].trim().parse().ok()
}

/// Whether a connect error still proves the host answered: a refused
/// connection comes from the host itself.
pub fn refused_counts_as_answer(kind: std::io::ErrorKind) -> bool {
    kind == std::io::ErrorKind::ConnectionRefused
}

/// One probe of `addr:443`. Answered = connected, or refused.
pub async fn probe(addr: &str) -> bool {
    let target = match addr.parse::<std::net::IpAddr>() {
        Ok(ip) => std::net::SocketAddr::new(ip, XAPI_PORT),
        Err(_) => return false,
    };
    match tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect(target)).await {
        Ok(Ok(_)) => true,
        Ok(Err(e)) => refused_counts_as_answer(e.kind()),
        Err(_) => false,
    }
}

/// Render the status snapshot (one `key=value` per line).
pub fn status_text(view: &PView, address: Option<&str>, k: u32, now_unix: u64) -> String {
    format!(
        "p={}\nanswered={}\nprobes={}\nsettled_secs={}\nsettle_secs={}\nreset_after={}\naddress={}\nupdated_unix={}\n",
        view.p,
        view.answered,
        view.probes,
        view.true_for.map(|d| d.as_secs().to_string()).unwrap_or_default(),
        SETTLE.as_secs(),
        k,
        address.unwrap_or(""),
        now_unix
    )
}

/// The status snapshot parsed back for `twinstor status`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatusSnapshot {
    pub p: bool,
    pub answered: u8,
    pub probes: u8,
    pub settled_secs: Option<u64>,
    pub address: Option<String>,
    pub updated_unix: u64,
}

pub fn parse_status(text: &str) -> Option<StatusSnapshot> {
    let get = |k: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{k}=")))
            .map(str::trim)
    };
    Some(StatusSnapshot {
        p: get("p")? == "true",
        answered: get("answered")?.parse().ok()?,
        probes: get("probes")?.parse().ok()?,
        settled_secs: get("settled_secs").and_then(|s| s.parse().ok()),
        address: get("address").filter(|s| !s.is_empty()).map(str::to_string),
        updated_unix: get("updated_unix").and_then(|s| s.parse().ok()).unwrap_or(0),
    })
}

/// Shared handle the daemon reads and the prober writes.
pub type Shared = std::sync::Arc<std::sync::Mutex<ReachState>>;

const LOCAL_DB: &str = "/var/lib/xcp/local.db";
const XHAD_CONF: &str = "/etc/xensource/xhad.conf";
const POOL_CONF: &str = "/etc/xensource/pool.conf";

fn is_local_ip(addr: &str) -> bool {
    addr.parse::<std::net::IpAddr>()
        .ok()
        .map(|ip| std::net::UdpSocket::bind((ip, 0)).is_ok())
        .unwrap_or(false)
}

/// pool.conf's master address when this host is a slave of the peer (never
/// one of this host's own addresses: a slave-of-self names nobody).
fn pool_conf_peer_master() -> Option<String> {
    let text = std::fs::read_to_string(POOL_CONF).ok()?;
    let addr = text.trim().strip_prefix("slave:")?.trim().to_string();
    (!addr.is_empty() && !is_local_ip(&addr)).then_some(addr)
}

fn read_cache() -> Option<String> {
    std::fs::read_to_string(address_cache_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| s.parse::<std::net::IpAddr>().is_ok())
}

fn write_cache(addr: &str) {
    let p = address_cache_path();
    if let Some(dir) = std::path::Path::new(&p).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&p, format!("{addr}\n"));
}

/// The background prober: one TCP connect every 10 s, the address resolved
/// before each probe, the settle k re-read from xhad.conf, and a status
/// snapshot written for `twinstor status`. Runs off the serial tick so a lost
/// probe never stalls it.
#[cfg_attr(test, allow(dead_code))]
pub fn spawn_prober(state: Shared, peer_hostname: String, arbitrated: bool) {
    tokio::spawn(async move {
        let mut round: u32 = 0;
        let mut last_warn: Option<String> = None;
        let mut last_p: Option<bool> = None;
        loop {
            let own_uuid = crate::xapi::local_host_uuid().await.unwrap_or_default();
            let (p_now, current) = state
                .lock()
                .map(|s| (s.p(), s.address().map(str::to_string)))
                .unwrap_or((false, None));
            // Refresh the cache from a live answer: at start, every 6 rounds
            // while no address is known at all, and every CACHE_REFRESH_EVERY
            // rounds whatever P says (a stale cache must not outlive a
            // readdressing). Off the tick, so a timeout costs nothing.
            if (round == 0
                || (current.is_none() && round.is_multiple_of(6))
                || round.is_multiple_of(CACHE_REFRESH_EVERY))
                && let Some(addr) = crate::xapi::peer_management_address(&peer_hostname).await
                && addr.parse::<std::net::IpAddr>().is_ok()
                && read_cache().as_deref() != Some(addr.as_str())
            {
                tracing::info!(heal_event = "mgmt_address_cache", address = %addr,
                               "peer management address cache refreshed from xapi");
                write_cache(&addr);
            }
            let ha_peers = std::fs::read_to_string(LOCAL_DB)
                .ok()
                .and_then(|db| ha_peers_address(&db, &own_uuid));
            let (pc, cache) = (pool_conf_peer_master(), read_cache());
            let (_, disagree) =
                resolve_address(pc.as_deref(), cache.as_deref(), ha_peers.as_deref());
            if disagree != last_warn {
                if let Some(w) = &disagree {
                    tracing::warn!(heal_event = "mgmt_address_disagree", "{w}");
                }
                last_warn = disagree;
            }
            let candidates = distinct_candidates(pc.as_deref(), cache.as_deref(), ha_peers.as_deref());
            // Probe: the current address while P holds; every candidate,
            // freshest first until one answers, while it does not.
            let mut answers = vec![false; candidates.len()];
            if p_now && let Some(c) = &current {
                if let Some(i) = candidates.iter().position(|x| x == c) {
                    answers[i] = probe(c).await;
                }
            } else {
                for (i, c) in candidates.iter().enumerate() {
                    answers[i] = probe(c).await;
                    if answers[i] {
                        break;
                    }
                }
            }
            let (addr, answered) = if p_now && current.as_ref().is_some_and(|c| !candidates.contains(c)) {
                // The address P was built on is no longer named by any source.
                pick_address(false, None, &candidates, &answers)
            } else {
                pick_address(p_now, current.as_deref(), &candidates, &answers)
            };
            if addr != current && current.is_some() {
                tracing::warn!(heal_event = "mgmt_address_switch",
                               from = current.as_deref().unwrap_or(""),
                               to = addr.as_deref().unwrap_or(""), answered,
                               "peer management address switched to the source that answers");
            }
            let k = std::fs::read_to_string(XHAD_CONF)
                .ok()
                .and_then(|c| heartbeat_timeout(&c))
                .unwrap_or_else(|| crate::setup::ha_timeout_secs(arbitrated));
            let k = settle_reset_after(k);
            let now = Instant::now();
            let view = {
                let Ok(mut s) = state.lock() else {
                    return;
                };
                s.set_reset_after(k);
                s.set_address(addr.clone());
                s.record(answered, now);
                s.view(now)
            };
            if last_p != Some(view.p) {
                tracing::info!(
                    heal_event = "mgmt_reach",
                    p = view.p,
                    answered = view.answered,
                    probes = view.probes,
                    address = addr.as_deref().unwrap_or("unknown"),
                    "peer management reachability changed (P: at least {ANSWERS_NEEDED} of the \
                     last {WINDOW} TCP connects to port {XAPI_PORT} answered, the last one included)"
                );
                last_p = Some(view.p);
            }
            let now_unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if let Some(dir) = std::path::Path::new(STATUS_PATH.as_str()).parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(STATUS_PATH.as_str(), status_text(&view, addr.as_deref(), k, now_unix));
            round = round.wrapping_add(1);
            tokio::time::sleep(PROBE_EVERY).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(st: &mut ReachState, t0: Instant, seq: &[bool]) -> Instant {
        let mut now = t0;
        for a in seq {
            now += PROBE_EVERY;
            st.record(*a, now);
        }
        now
    }

    #[test]
    fn p_needs_four_of_six_and_the_last() {
        let t0 = Instant::now();
        let mut st = ReachState::new(6);
        feed(&mut st, t0, &[true, true, true]);
        assert!(!st.p(), "3 answers are not enough");
        feed(&mut st, t0, &[true]);
        assert!(st.p(), "4 of 4 with the last answered");
        let mut st = ReachState::new(6);
        feed(&mut st, t0, &[true, true, true, true, true, false]);
        assert!(!st.p(), "the last probe must have answered");
        let mut st = ReachState::new(6);
        feed(&mut st, t0, &[false, false, true, true, true, true]);
        assert!(st.p(), "4 of 6 with the last answered");
        let mut st = ReachState::new(6);
        feed(&mut st, t0, &[false, true, false, true, false, true]);
        assert!(!st.p(), "3 of 6");
    }

    #[test]
    fn refused_counts_as_an_answer() {
        assert!(refused_counts_as_answer(std::io::ErrorKind::ConnectionRefused));
        assert!(!refused_counts_as_answer(std::io::ErrorKind::TimedOut));
        assert!(!refused_counts_as_answer(std::io::ErrorKind::HostUnreachable));
        assert!(!refused_counts_as_answer(std::io::ErrorKind::NetworkUnreachable));
    }

    #[test]
    fn settle_resets_only_after_k_consecutive_failures() {
        let t0 = Instant::now();
        let mut st = ReachState::new(6);
        let start = feed(&mut st, t0, &[true, true, true, true]);
        assert_eq!(st.p_true_since, Some(start));
        // Single losses, and even five in a row, keep the clock.
        feed(&mut st, start, &[false, true, true, false, false, false, false, false, true]);
        assert_eq!(st.p_true_since, Some(start));
        // Six in a row reset it.
        let end = feed(&mut st, start, &[false; 6]);
        assert_eq!(st.p_true_since, None);
        // It restarts when P is next true.
        let back = feed(&mut st, end, &[true, true, true, true]);
        assert_eq!(st.p_true_since, Some(back));
    }

    #[test]
    fn settle_reset_k_from_heartbeat_timeout() {
        assert_eq!(settle_reset_after(120), 6);
        assert_eq!(settle_reset_after(90), 4);
        assert_eq!(settle_reset_after(30), 3);
        assert_eq!(settle_reset_after(600), 6);
        // k x 10 s never exceeds half the heartbeat timeout in the normal range.
        for t in [60u32, 90, 120] {
            assert!(settle_reset_after(t) * 10 <= t / 2 || settle_reset_after(t) == 3);
        }
    }

    /// Replays of independent loss. A deterministic generator keeps the test
    /// stable; at 20 % loss the settle must still mature in most 330 s windows
    /// and P must be true most of the time.
    fn loss_replay(loss_pct: u64, seed: u64) -> (usize, usize, usize) {
        let t0 = Instant::now();
        let mut st = ReachState::new(6);
        let mut x = seed;
        let mut now = t0;
        let (mut p_true, mut resets, mut settled_ticks) = (0, 0, 0);
        let mut had_since = false;
        for _ in 0..3600 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let lost = (x >> 33) % 100 < loss_pct;
            now += PROBE_EVERY;
            st.record(!lost, now);
            if st.p() {
                p_true += 1;
            }
            if had_since && st.p_true_since.is_none() {
                resets += 1;
            }
            had_since = st.p_true_since.is_some();
            if st.view(now).settled() {
                settled_ticks += 1;
            }
        }
        (p_true, resets, settled_ticks)
    }

    #[test]
    fn p_and_settle_under_five_and_twenty_percent_loss() {
        let (p5, resets5, settled5) = loss_replay(5, 7);
        assert!(p5 > 3300, "P true most of the time at 5 % loss: {p5}");
        assert_eq!(resets5, 0, "no settle reset at 5 % loss");
        assert!(settled5 > 3300, "settled most of the time at 5 % loss: {settled5}");
        let (p20, resets20, settled20) = loss_replay(20, 11);
        assert!(p20 > 2500, "P true most of the time at 20 % loss: {p20}");
        assert!(resets20 <= 1, "the settle almost never resets at 20 % loss: {resets20}");
        assert!(settled20 > 2500, "settled most of the time at 20 % loss: {settled20}");
    }

    #[test]
    fn gate_absence_waits_the_settle_positive_does_not() {
        let mut v = PView {
            p: true,
            last_answered: true,
            answered: 6,
            probes: 6,
            true_for: Some(Duration::from_secs(100)),
            address_known: true,
            stale: false,
        };
        assert_eq!(gate(&v, Trigger::Positive), Gate::Pass);
        assert_eq!(
            gate(&v, Trigger::Absence),
            Gate::Settling {
                remaining: SETTLE - Duration::from_secs(100)
            }
        );
        v.true_for = Some(SETTLE);
        assert_eq!(gate(&v, Trigger::Absence), Gate::Pass);
        v.p = false;
        assert_eq!(gate(&v, Trigger::Positive), Gate::NotReachable);
        assert_eq!(gate(&v, Trigger::Absence), Gate::NotReachable);
        assert_eq!(gate(&PView::default(), Trigger::Positive), Gate::NotReachable);
    }

    #[test]
    fn startup_loop_lock_matrix() {
        // Locked: armed, not Online, P false.
        assert!(startup_loop_locked(Some(true), false, false));
        // Any of the three released lets the action through to its own gates.
        assert!(!startup_loop_locked(Some(true), false, true));
        assert!(!startup_loop_locked(Some(true), true, false));
        assert!(!startup_loop_locked(Some(false), false, false));
        assert!(!startup_loop_locked(None, false, false));
    }

    #[test]
    fn stuck_xapi_needs_600s_same_pid_after_p() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut s = StuckXapi::default();
        // The clock starts only while P holds.
        assert!(!s.observe(false, "100", false, true, t0));
        assert_eq!(s.silent_for(t0), None);
        assert!(!s.observe(false, "100", true, true, t0));
        assert!(!s.observe(false, "100", true, true, at(599)));
        assert!(s.observe(false, "100", true, true, at(600)));
        // A PID change restarts the clock (xapi restarting itself).
        assert!(!s.observe(false, "200", true, true, at(700)));
        assert!(!s.observe(false, "200", true, true, at(1299)));
        // One lost probe (P false, settle still running) keeps the clock.
        assert!(s.observe(false, "200", false, true, at(1300)));
        // An answer resets it; so does a settle reset.
        assert!(!s.observe(true, "200", true, true, at(1301)));
        assert!(!s.observe(false, "200", true, true, at(1302)));
        assert!(!s.observe(false, "200", true, false, at(1903)));
        assert_eq!(s.silent_for(at(1903)), None);
    }

    #[test]
    fn no_xapi_process_counts_as_stuck_after_600s() {
        let t0 = Instant::now();
        let mut s = StuckXapi::default();
        assert!(!s.observe(false, "", true, true, t0));
        assert!(!s.observe(false, "", true, true, t0 + Duration::from_secs(599)));
        assert!(s.observe(false, "", true, true, t0 + Duration::from_secs(600)));
        // xapi comes back under a new PID: its own start, the clock restarts.
        assert!(!s.observe(false, "300", true, true, t0 + Duration::from_secs(601)));
    }

    #[test]
    fn p_reads_false_once_the_prober_stops() {
        let t0 = Instant::now();
        let mut st = ReachState::new(6);
        let last = feed(&mut st, t0, &[true; 6]);
        assert!(st.view(last).p && !st.view(last).stale);
        assert!(st.view(last + FRESH_MAX).p);
        let v = st.view(last + FRESH_MAX + Duration::from_secs(1));
        assert!(!v.p && v.stale);
        // Never probed: P false, not reported stale (nothing to be stale).
        let v = ReachState::new(6).view(t0);
        assert!(!v.p && !v.stale);
    }

    #[test]
    fn a_stale_cached_address_gives_way_to_a_source_that_answers() {
        let c = distinct_candidates(Some(" "), Some("10.0.0.9"), Some("10.0.0.2"));
        assert_eq!(c, vec!["10.0.0.9".to_string(), "10.0.0.2".to_string()]);
        assert_eq!(distinct_candidates(Some("a"), Some("a"), None), vec!["a".to_string()]);
        // P false: the cache (freshest) does not answer, ha_peers does.
        assert_eq!(
            pick_address(false, Some("10.0.0.9"), &c, &[false, true]),
            (Some("10.0.0.2".to_string()), true)
        );
        // Nothing answers: keep the freshest.
        assert_eq!(
            pick_address(false, None, &c, &[false, false]),
            (Some("10.0.0.9".to_string()), false)
        );
        // P holds: stay on the address it was built on.
        assert_eq!(
            pick_address(true, Some("10.0.0.2"), &c, &[false, true]),
            (Some("10.0.0.2".to_string()), true)
        );
        assert_eq!(pick_address(false, None, &[], &[]), (None, false));
    }

    #[test]
    fn address_sources_freshest_first() {
        assert_eq!(resolve_address(None, None, None), (None, None));
        let (a, w) = resolve_address(Some("10.0.0.2"), Some("10.0.0.2"), Some("10.0.0.2"));
        assert_eq!(a.as_deref(), Some("10.0.0.2"));
        assert!(w.is_none());
        let (a, w) = resolve_address(None, Some("10.0.0.3"), Some("10.0.0.2"));
        assert_eq!(a.as_deref(), Some("10.0.0.3"));
        assert!(w.unwrap().contains("ha_peers=10.0.0.2"));
        let (a, _) = resolve_address(None, None, Some("10.0.0.2"));
        assert_eq!(a.as_deref(), Some("10.0.0.2"));
        let (a, _) = resolve_address(Some(" "), None, Some("10.0.0.2"));
        assert_eq!(a.as_deref(), Some("10.0.0.2"));
    }

    #[test]
    fn ha_peers_row_parses() {
        let db = r#"<localdb><row key="ha.armed" value="true"/><row key="ha_peers" value="(('11111111-1111-4111-8111-111111111111' '198.51.100.47') ('22222222-2222-4222-8222-222222222222' '198.51.100.49'))"/></localdb>"#;
        assert_eq!(
            ha_peers_address(db, "11111111-1111-4111-8111-111111111111").as_deref(),
            Some("198.51.100.49")
        );
        assert_eq!(
            ha_peers_address(db, "22222222-2222-4222-8222-222222222222").as_deref(),
            Some("198.51.100.47")
        );
        // A row edited to drop the peer: only this host left.
        let edited = r#"<row key="ha_peers" value="(('22222222-2222-4222-8222-222222222222' '198.51.100.49'))"/>"#;
        assert_eq!(ha_peers_address(edited, "22222222-2222-4222-8222-222222222222"), None);
        assert_eq!(
            ha_peers_names(edited, "11111111-1111-4111-8111-111111111111"),
            Some(false)
        );
        assert_eq!(ha_peers_names(db, "11111111-1111-4111-8111-111111111111"), Some(true));
        assert_eq!(ha_peers_address("<localdb/>", "x"), None);
        let escaped = r#"<row key="ha_peers" value="((&apos;a&apos; &apos;10.0.0.1&apos;) (&apos;b&apos; &apos;10.0.0.2&apos;))"/>"#;
        assert_eq!(ha_peers_address(escaped, "a").as_deref(), Some("10.0.0.2"));
    }

    #[test]
    fn heartbeat_timeout_from_xhad_conf() {
        let conf = "<parameters><HeartbeatInterval>4</HeartbeatInterval>\
                    <HeartbeatTimeout>120</HeartbeatTimeout><StateFileTimeout>120</StateFileTimeout>";
        assert_eq!(heartbeat_timeout(conf), Some(120));
        assert_eq!(heartbeat_timeout("<x/>"), None);
    }

    #[test]
    fn status_round_trip() {
        let v = PView {
            p: true,
            last_answered: true,
            answered: 5,
            probes: 6,
            true_for: Some(Duration::from_secs(42)),
            address_known: true,
            stale: false,
        };
        let s = parse_status(&status_text(&v, Some("10.0.0.2"), 6, 99)).unwrap();
        assert!(s.p);
        assert_eq!((s.answered, s.probes), (5, 6));
        assert_eq!(s.settled_secs, Some(42));
        assert_eq!(s.address.as_deref(), Some("10.0.0.2"));
        assert_eq!(s.updated_unix, 99);
    }
}
