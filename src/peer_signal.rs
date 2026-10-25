//! Peer signal beacon: gateway state and backing attestation on one port
//! (7900).
//!
//! Each node serves a single reply line per connection:
//!
//! ```text
//! <g> twinstor1 <node-name> <resource> <backing>[ <claim>][ B]\n
//! ```
//!
//! The first byte is the gateway byte (`'1'`/`'0'`/`'?'`, this node's
//! gateway_ok). Daemons before 0.6.11 read only that one byte, so they keep
//! working against a newer responder. Newer daemons read the whole line for
//! the backing attestation; an older responder's bare byte fails that parse
//! and reads as no attestation. A mixed-version pool degrades safely in both
//! directions, and the firewall requirement stays a single port.
//!
//! Gateway semantics (arbitrated mode, where this tells a whole-switch outage
//! from a single-host one):
//!
//!   * **single-host isolation**: peer reachable on the direct link and its
//!     gateway is up. The peer can still serve clients, so this node stands
//!     down and the peer reclaims its VMs.
//!   * **whole-switch outage**: peer reachable on the direct link but its
//!     gateway is also down. Moving VMs is pointless, so both hold and keep
//!     running (suppressing the XHA state-file fence) until the gateway
//!     returns. A router outage with the switch still up reads the same way
//!     and holds the same way on both hosts: the hold uses only this beacon,
//!     which both hosts read alike, so they always hold together.
//!   * **gateway lost on one host only**: the peer's gateway is up, but the
//!     peer still answers over the gateway network. This host is not cut off
//!     from the client network, so it is not stood down. That check pings the
//!     peer address out of the default route's device, so it applies only when
//!     the peer address sits directly on the gateway (management) network, and
//!     on the XHA heartbeat network when xhad.conf names one; with the peer
//!     address on a separate storage network the stand-down uses the gateway
//!     alone.
//!   * **true host death**: peer not reachable on the direct link at all. The
//!     real failover path.
//!
//! Backing attestation (for the sole-survivor escalation, both topologies):
//! what this node can prove locally about its own backing device,
//! identity-checked by the querier; see [`PeerBacking`]. The responder binds
//! every peer-facing local address (direct link and replication path) and
//! sends one reply per connection, then drops it; it is not a general peer
//! channel.
//!
//! Split-brain claim (optional `<claim>` token, served only during a
//! split-brain episode): `Y<uuid>` means this node's fence-peer yielded and it
//! will not claim the win for the fork in which its current generation is
//! `<uuid>`; `W<uuid>` means this node took the win for that fork. Freeze mode
//! adds `G` and `R` (see [`ForkClaim`]). Each is bound to a generation UUID
//! (16 hex digits) that the querier compares with the generations in its own
//! handshake read, so a claim from an earlier fork can never match. Absent
//! outside an episode, so the everyday line is unchanged. An older querier
//! parses a line carrying a claim as "no backing attestation" (guided-manual
//! only), which is harmless for the seconds to minutes of an episode. The
//! token never starts with `B`, so the fence-peer script's bounce glob cannot
//! match it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Fixed TCP port for the peer-signal responder (gateway byte and backing
/// attestation). The only peer-facing port this daemon requires.
pub const GATEWAY_SIGNAL_PORT: u16 = 7900;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const READ_TIMEOUT: Duration = Duration::from_secs(3);

/// The peer's client-gateway reachability, learned over the direct link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerGateway {
    /// Peer reachable on the direct link and its gateway is up (single-host case).
    Up,
    /// Peer reachable on the direct link and its gateway is confirmed down: it
    /// was up since boot and has since been lost. The only reading that may
    /// drive a whole-switch keep-VMs hold.
    Down,
    /// Peer reachable on the direct link but it has not yet confirmed a gateway
    /// state since boot (still coming up). A rebooting peer whose network is
    /// not up yet must not read as `Down`: an isolated node would take a
    /// single-host isolation for a whole-switch outage, keep its VMs, and
    /// dual-run them when the healthy peer's XHA restarts them. `Unknown` is
    /// never a whole-switch; it falls through to the stand-down / failover path.
    Unknown,
    /// Peer not reachable on the direct link at all (true host death / link down).
    Unreachable,
}

/// Shared, cheaply updated peer-signal state served by the responder: the
/// gateway state (0 = down, 1 = up, 2 = unknown / not confirmed up since
/// boot), the backing attestation, and the node identity included in every
/// reply.
#[derive(Clone)]
pub struct PeerBeacon {
    gw_state: Arc<AtomicU8>,
    /// Latched once the local gateway has been reachable at least once since this
    /// process started. `down` (0) is served only after this is set, so only
    /// for a real loss (was up, now down), never for a still-booting node whose
    /// network is not up yet. A process restart or reboot resets it.
    ever_up: Arc<std::sync::atomic::AtomicBool>,
    /// backing attestation wire char (`O`/`D`/`M`/`A`/`?`).
    backing: Arc<AtomicU8>,
    /// Administrative-bounce flag: raised for the seconds of an intentional
    /// replication bounce so the peer's fence-peer handler (which probes this
    /// beacon) stands down instead of outdating the bouncing node. An
    /// unannounced admin disconnect makes the peer fence and outdate the
    /// disconnecting node, which can create a split-brain with no diverged
    /// data.
    bouncing: Arc<std::sync::atomic::AtomicBool>,
    /// This node's configured name and resource, included in every reply so
    /// the querier can reject an answer from the wrong host or pool.
    identity: Arc<(String, String)>,
    /// Split-brain claim served for the current fork (see the module docs).
    claim: Arc<std::sync::Mutex<Option<ForkClaim>>>,
}

/// A split-brain claim this node attests about the current fork, bound to its
/// own current generation UUID (bit 0 masked) so it cannot outlive the fork.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkClaim {
    /// This node's fence-peer yielded: it is frozen and never claims the win
    /// for this fork.
    Yielded(u64),
    /// This node took the win for this fork: the peer must not keep its copy.
    Won(u64),
    /// Freeze mode: this node's fence-peer yielded with guests on the SR,
    /// frozen since the disconnect (nothing acknowledged the peer lacks); its
    /// guests continue if the peer gives way.
    FrozenGuests(u64),
    /// Freeze mode: this node gives way to the peer's frozen guests (it ran
    /// none during the split); carries the peer's generation.
    GiveWay(u64),
}

impl ForkClaim {
    fn token(self) -> String {
        match self {
            ForkClaim::Yielded(u) => format!("Y{u:016X}"),
            ForkClaim::Won(u) => format!("W{u:016X}"),
            ForkClaim::FrozenGuests(u) => format!("G{u:016X}"),
            ForkClaim::GiveWay(u) => format!("R{u:016X}"),
        }
    }
}

impl PeerBeacon {
    pub fn new(node_name: &str, resource: &str) -> Self {
        Self {
            gw_state: Arc::new(AtomicU8::new(2)),
            ever_up: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            backing: Arc::new(AtomicU8::new(b'?')),
            bouncing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            identity: Arc::new((node_name.to_string(), resource.to_string())),
            claim: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Set (or clear) the split-brain claim served on the beacon line.
    pub fn set_claim(&self, claim: Option<ForkClaim>) {
        if let Ok(mut c) = self.claim.lock() {
            *c = claim;
        }
    }

    /// Update from the daemon tick's local `ping_default_gateway()` result.
    /// `down` is advertised only once the gateway has been up since boot;
    /// before that it is `unknown`, so a booting node never tells an isolated
    /// peer "my gateway is down" (which reads as a whole-switch outage).
    pub fn set_gateway(&self, gateway_ok: bool) {
        if gateway_ok {
            self.ever_up.store(true, Ordering::Relaxed);
            self.gw_state.store(1, Ordering::Relaxed);
        } else if self.ever_up.load(Ordering::Relaxed) {
            self.gw_state.store(0, Ordering::Relaxed); // genuine loss: was up, now down
        } else {
            self.gw_state.store(2, Ordering::Relaxed); // never up since boot: unknown
        }
    }

    /// Latch `ever_up` without changing the advertised state. Called on daemon
    /// start when a runtime marker shows the gateway was already confirmed up
    /// this boot, so a daemon restart (e.g. a binary upgrade) mid-outage does not
    /// start advertising `unknown`, which would defeat a surviving peer's
    /// whole-switch hold. A reboot clears the tmpfs marker, so a fresh boot
    /// still starts with `ever_up=false`.
    pub fn note_confirmed_up_since_boot(&self) {
        self.ever_up.store(true, Ordering::Relaxed);
    }

    /// Update the backing attestation from the daemon's periodic local
    /// probe. The caller (the beacon updater task) applies the publish
    /// hysteresis for `Destroyed`; this is a plain store.
    pub fn set_backing(&self, meta: crate::drbd::BackingMeta) {
        self.backing
            .store(backing_wire_char(meta), Ordering::Relaxed);
    }

    fn gw_wire_byte(&self) -> u8 {
        match self.gw_state.load(Ordering::Relaxed) {
            1 => b'1',
            0 => b'0',
            _ => b'?',
        }
    }

    /// Raise/clear the administrative-bounce flag (appends a trailing
    /// `B` token to the reply line while raised).
    pub fn set_bouncing(&self, on: bool) {
        self.bouncing.store(on, Ordering::Relaxed);
    }

    /// The full reply line. Byte 0 is the gateway byte, which is all that
    /// daemons before 0.6.11 read; never put anything before it.
    fn wire_line(&self) -> String {
        let (node, res) = &*self.identity;
        let bounce = if self.bouncing.load(Ordering::Relaxed) {
            " B"
        } else {
            ""
        };
        let claim = self
            .claim
            .lock()
            .ok()
            .and_then(|c| *c)
            .map(|c| format!(" {}", c.token()))
            .unwrap_or_default();
        format!(
            "{} twinstor1 {node} {res} {}{claim}{bounce}\n",
            self.gw_wire_byte() as char,
            self.backing.load(Ordering::Relaxed) as char,
        )
    }
}

/// Spawn a small TCP responder bound to one local peer-facing address that
/// replies to each connection with the beacon line (see [`PeerBeacon`]; byte
/// 0 is the gateway byte older queriers read). Best-effort: if the bind fails
/// (interface not up yet) it retries. Never blocks the daemon.
pub fn spawn_responder(bind_ip: String, beacon: PeerBeacon) {
    tokio::spawn(async move {
        loop {
            match TcpListener::bind((bind_ip.as_str(), GATEWAY_SIGNAL_PORT)).await {
                Ok(listener) => loop {
                    match listener.accept().await {
                        Ok((mut sock, _)) => {
                            // Serve each connection on its own task: both the
                            // whole-switch and the isolated-slave checks probe
                            // the peer every tick, so a serial accept loop can
                            // queue queries until they time out (read as a
                            // spurious Unreachable). One line, then drop.
                            let line = beacon.wire_line();
                            tokio::spawn(async move {
                                let _ = sock.write_all(line.as_bytes()).await;
                                let _ = sock.shutdown().await;
                            });
                        }
                        // Transient per-connection errors (ECONNABORTED) do
                        // not invalidate the listener; keep accepting.
                        Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                    }
                },
                Err(_) => {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        }
    });
}

/// Query the peer's gateway_ok over the direct link. Any transport failure
/// (unreachable, timeout, short read) reads as [`PeerGateway::Unreachable`]; a
/// peer that answers "unknown" (`'?'`, not confirmed up since boot) reads as
/// [`PeerGateway::Unknown`], never as `Down`, so a still-booting peer cannot
/// be mistaken for a whole-switch outage and cause a dual-run.
pub async fn peer_gateway(peer_dl_ip: &str) -> PeerGateway {
    match query(peer_dl_ip).await {
        Some(b'1') => PeerGateway::Up,
        Some(b'0') => PeerGateway::Down,
        Some(_) => PeerGateway::Unknown, // '?' not confirmed since boot: never a whole-switch
        None => PeerGateway::Unreachable,
    }
}

async fn query(peer_dl_ip: &str) -> Option<u8> {
    query_port(peer_dl_ip, GATEWAY_SIGNAL_PORT).await
}

async fn query_port(peer_ip: &str, port: u16) -> Option<u8> {
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((peer_ip, port)))
        .await
        .ok()?
        .ok()?;
    let mut stream = stream;
    let mut buf = [0u8; 1];
    tokio::time::timeout(READ_TIMEOUT, stream.read_exact(&mut buf))
        .await
        .ok()?
        .ok()?;
    Some(buf[0])
}

// ---------------------------------------------------------------------------
// Backing attestation, carried on the same beacon line and port as the
// gateway signal. The peer's sole-survivor escalation may auto-force-promote
// only on a positive, identity-verified `Destroyed` attestation, the one
// reading that proves no replica exists on the peer's backing. Every
// transport, parse or identity failure reads as "no attestation" (no
// automatic action), so an older peer whose responder serves only the bare
// gateway byte degrades to the guided-manual path.
// ---------------------------------------------------------------------------

/// The peer's attestation about its own backing device, learned over the
/// beacon. Mirrors [`crate::drbd::BackingMeta`] plus the transport failure
/// case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerBacking {
    /// Peer's disk is attached (it is not the Diskless peer we thought, or it
    /// just recovered). Never escalate.
    Attached,
    /// Peer proves its device node exists but carries no valid DRBD metadata:
    /// no replica exists on it. The only reading that may permit auto-force.
    Destroyed,
    /// Peer has valid metadata on the device (data exists, attach pending).
    /// Never escalate.
    Detached,
    /// Peer's configured device node is missing; data plausibly intact
    /// elsewhere (unassembled array, unenumerated disk). Guided-manual only.
    Absent,
    /// Peer answered but could not classify its own state. Guided-manual only.
    Unknown,
    /// No answer on the beacon (old daemon, port blocked, peer down).
    /// Guided-manual only.
    Unreachable,
}

/// Wire character for the backing attestation served by the responder.
/// State chars: `O` attached, `D` destroyed, `M` metadata-present (detached),
/// `A` absent, `?` unknown.
///
/// The attestation rides the shared beacon line (see [`PeerBeacon`]):
/// `<g> twinstor1 <node-name> <resource> <state-char>\n`. The identity fields
/// let the querier reject an answer from the wrong host or the wrong pool
/// (stale direct-peer file, IP reuse, cross-pool cabling): an attestation as
/// consequential as `Destroyed` must never be accepted from a node we cannot
/// name. Anything that does not parse (including an older daemon's bare
/// one-byte gateway reply) or names the wrong node/resource reads as
/// `Unknown`, never as `Destroyed`.
fn backing_wire_char(meta: crate::drbd::BackingMeta) -> u8 {
    match meta {
        crate::drbd::BackingMeta::Attached => b'O',
        crate::drbd::BackingMeta::Destroyed => b'D',
        crate::drbd::BackingMeta::Detached => b'M',
        crate::drbd::BackingMeta::Absent => b'A',
        crate::drbd::BackingMeta::Unknown => b'?',
    }
}

/// Parse a beacon reply's attestation against the expected peer identity.
/// Token 0 is the gateway byte (any single char; its own consumer validates
/// it, not this function); every malformed, truncated, or
/// wrong-identity reply is `Unknown` (an answer arrived, but not one we may
/// act on).
fn parse_backing_reply(line: &str, expect_node: &str, expect_resource: &str) -> PeerBacking {
    // The optional claim token and the optional trailing `B` (administrative
    // bounce) do not change the attestation reading.
    match parse_tail(line, expect_node, expect_resource) {
        Some(("O", _)) => PeerBacking::Attached,
        Some(("D", _)) => PeerBacking::Destroyed,
        Some(("M", _)) => PeerBacking::Detached,
        Some(("A", _)) => PeerBacking::Absent,
        _ => PeerBacking::Unknown,
    }
}

/// Split a reply line into its backing token and optional claim token after
/// checking the header and identity. `None` for anything malformed: wrong
/// identity, unknown or repeated tokens, or text after the bounce flag.
fn parse_tail<'a>(
    line: &'a str,
    expect_node: &str,
    expect_resource: &str,
) -> Option<(&'a str, Option<&'a str>)> {
    let mut parts = line.trim_end().split(' ');
    let gw_ok = parts.next().is_some_and(|g| g.chars().count() == 1);
    if !gw_ok
        || parts.next() != Some("twinstor1")
        || parts.next() != Some(expect_node)
        || parts.next() != Some(expect_resource)
    {
        return None;
    }
    let backing = parts.next()?;
    let rest: Vec<&str> = parts.collect();
    let claim = match rest.as_slice() {
        [] | ["B"] => None,
        [c] | [c, "B"] if parse_claim_token(c).is_some() => Some(*c),
        _ => return None,
    };
    Some((backing, claim))
}

/// `Y`, `W`, `G` or `R` followed by 16 hex digits; anything else is not a
/// claim.
fn parse_claim_token(t: &str) -> Option<ForkClaim> {
    let (kind, hex) = t.split_at_checked(1)?;
    if hex.len() != 16 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let u = u64::from_str_radix(hex, 16).ok()?;
    match kind {
        "Y" => Some(ForkClaim::Yielded(u)),
        "W" => Some(ForkClaim::Won(u)),
        "G" => Some(ForkClaim::FrozenGuests(u)),
        "R" => Some(ForkClaim::GiveWay(u)),
        _ => None,
    }
}

/// What the peer's beacon says about the current split-brain fork.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerClaim {
    /// The peer answered with a valid line carrying no claim.
    None,
    /// The peer attests a claim (see [`ForkClaim`]).
    Claim(ForkClaim),
    /// The peer answered, but not with a line we may act on (older daemon's
    /// bare byte, wrong identity, malformed).
    Unknown,
    /// No answer on the beacon.
    Unreachable,
}

fn parse_claim_reply(line: &str, expect_node: &str, expect_resource: &str) -> PeerClaim {
    match parse_tail(line, expect_node, expect_resource) {
        Some((_, None)) => PeerClaim::None,
        Some((_, Some(t))) => parse_claim_token(t).map_or(PeerClaim::Unknown, PeerClaim::Claim),
        None => PeerClaim::Unknown,
    }
}

/// Query the peer's split-brain claim over the beacon port, identity-checked
/// like [`peer_backing`].
#[cfg_attr(test, allow(dead_code))]
pub async fn peer_claim(peer_ip: &str, expect_node: &str, expect_resource: &str) -> PeerClaim {
    match query_line(peer_ip, GATEWAY_SIGNAL_PORT).await {
        Some(line) => parse_claim_reply(&line, expect_node, expect_resource),
        None => PeerClaim::Unreachable,
    }
}

/// Whether the node at `ip` serves the line-format beacon for our resource:
/// the capability probe for the administrative-bounce guard (stuck WFBitMap
/// auto-clear). An older daemon answers one bare gateway byte and its
/// fence-peer script has no bounce stand-down, so an automatic bounce against
/// it would let it fence and outdate the bouncing node and roll back acked
/// writes on reconnect. The bounce therefore fails closed unless the peer
/// speaks the line format (which shipped together with the guarded script).
pub async fn peer_speaks_line_format(
    peer_ip: &str,
    expect_node: &str,
    expect_resource: &str,
) -> bool {
    match query_line(peer_ip, GATEWAY_SIGNAL_PORT).await {
        Some(line) => {
            let mut parts = line.trim_end().split(' ');
            parts.next().is_some_and(|g| g.chars().count() == 1)
                && parts.next() == Some("twinstor1")
                && parts.next() == Some(expect_node)
                && parts.next() == Some(expect_resource)
        }
        None => false,
    }
}

/// Self-check that our own beacon at `ip` is reachable and currently serving the
/// bounce flag (trailing `B`). Run by the bouncing node immediately before
/// the disconnect: if the flag is not actually being served (responder not
/// bound, firewall dropped the rule, truncation), the peer's fence handler
/// will fence us, so the bounce must be aborted.
pub async fn self_check_bounce_flag(ip: &str, node: &str, resource: &str) -> bool {
    match query_line(ip, GATEWAY_SIGNAL_PORT).await {
        Some(line) => {
            let t = line.trim_end();
            let mut parts = t.split(' ');
            parts.next().is_some_and(|g| g.chars().count() == 1)
                && parts.next() == Some("twinstor1")
                && parts.next() == Some(node)
                && parts.next() == Some(resource)
                && t.ends_with(" B")
        }
        None => false,
    }
}

/// Query the peer's backing attestation over the shared beacon port and
/// verify the reply names the expected peer node and resource. Any transport
/// failure reads as `Unreachable`; any parse or identity mismatch (including
/// an older daemon's bare gateway byte) as `Unknown`, never as `Destroyed`.
pub async fn peer_backing(peer_ip: &str, expect_node: &str, expect_resource: &str) -> PeerBacking {
    match query_line(peer_ip, GATEWAY_SIGNAL_PORT).await {
        Some(line) => parse_backing_reply(&line, expect_node, expect_resource),
        None => PeerBacking::Unreachable,
    }
}

/// Read one short reply line (bounded size and time). `None` on any transport
/// failure.
async fn query_line(peer_ip: &str, port: u16) -> Option<String> {
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((peer_ip, port)))
        .await
        .ok()?
        .ok()?;
    let mut stream = stream;
    let mut buf = Vec::with_capacity(256);
    let read = async {
        let mut byte = [0u8; 1];
        while buf.len() < 256 {
            match stream.read(&mut byte).await {
                Ok(0) => break,
                Ok(_) => {
                    if byte[0] == b'\n' {
                        break;
                    }
                    buf.push(byte[0]);
                }
                Err(_) => return None,
            }
        }
        Some(())
    };
    tokio::time::timeout(READ_TIMEOUT, read).await.ok()??;
    String::from_utf8(buf).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_beacon() -> PeerBeacon {
        PeerBeacon::new("peer-node", "data")
    }

    #[test]
    fn beacon_wire_bytes() {
        let b = test_beacon();
        assert_eq!(b.gw_wire_byte(), b'?'); // unknown until first sample
        b.set_gateway(true);
        assert_eq!(b.gw_wire_byte(), b'1');
        b.set_gateway(false);
        assert_eq!(b.gw_wire_byte(), b'0'); // real loss (was up, now down)
    }

    #[test]
    fn beacon_wire_line_first_byte_is_the_gateway_byte() {
        // Backward compatibility: byte 0 of the reply line is the gateway byte.
        // Daemons before 0.6.11 read only that byte, so nothing may be put
        // before it.
        let b = test_beacon();
        assert_eq!(b.wire_line().as_bytes()[0], b'?');
        b.set_gateway(true);
        assert_eq!(b.wire_line().as_bytes()[0], b'1');
        b.set_backing(crate::drbd::BackingMeta::Destroyed);
        assert_eq!(b.wire_line(), "1 twinstor1 peer-node data D\n");
        b.set_gateway(false);
        assert_eq!(b.wire_line(), "0 twinstor1 peer-node data D\n");
    }

    #[test]
    fn beacon_never_serves_down_before_gateway_confirmed_up() {
        // A still-booting node (gateway never up yet) must advertise unknown,
        // not down; otherwise an isolated peer takes a single-host isolation
        // for a whole-switch outage, keeps its VMs, and dual-runs.
        let b = test_beacon();
        assert_eq!(b.gw_wire_byte(), b'?'); // fresh: unknown
        b.set_gateway(false); // gateway ping fails during boot: still unknown, not down
        assert_eq!(b.gw_wire_byte(), b'?');
        b.set_gateway(false);
        assert_eq!(b.gw_wire_byte(), b'?');
        // Only after the gateway is confirmed up does a later loss read as down.
        b.set_gateway(true);
        assert_eq!(b.gw_wire_byte(), b'1');
        b.set_gateway(false);
        assert_eq!(b.gw_wire_byte(), b'0');
    }

    #[test]
    fn ever_up_restored_from_boot_marker_serves_down_not_unknown() {
        // A daemon restart mid-outage: the updater sees the tmpfs marker (gateway
        // was up earlier this boot) and restores the latch, so the node advertises
        // `down` and does not defeat a surviving peer's whole-switch hold.
        let b = test_beacon();
        b.set_gateway(false); // isolated, latch not set: unknown
        assert_eq!(b.gw_wire_byte(), b'?');
        b.note_confirmed_up_since_boot(); // restored from the boot marker
        b.set_gateway(false); // still isolated, but now a known real loss
        assert_eq!(b.gw_wire_byte(), b'0');
    }

    #[tokio::test]
    async fn booting_peer_reads_unknown_not_down() {
        // A peer that has never confirmed its gateway up serves '?', read as Unknown.
        // Separate loopback IP so the responder's port does not collide with the
        // other tests that bind 127.0.0.1:7900 (they run in parallel).
        let beacon = test_beacon();
        beacon.set_gateway(false); // boot: gateway not up yet
        spawn_responder("127.0.0.3".to_string(), beacon.clone());
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(peer_gateway("127.0.0.3").await, PeerGateway::Unknown);
    }

    #[tokio::test]
    async fn responder_and_query_roundtrip() {
        // The one-byte gateway read still works against the line-serving
        // responder in each state.
        let beacon = test_beacon();
        beacon.set_gateway(true);
        spawn_responder("127.0.0.1".to_string(), beacon.clone());
        // give the responder a moment to bind
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(peer_gateway("127.0.0.1").await, PeerGateway::Up);
        beacon.set_gateway(false);
        assert_eq!(peer_gateway("127.0.0.1").await, PeerGateway::Down);
    }

    #[tokio::test]
    async fn unreachable_when_nobody_listens() {
        // Port with no responder: Unreachable (the true-death reading).
        assert_eq!(peer_gateway("127.0.0.2").await, PeerGateway::Unreachable);
    }

    #[tokio::test]
    async fn backing_attestation_roundtrip_all_states_same_port() {
        use crate::drbd::BackingMeta;
        // One responder, one port: the gateway byte and the attestation share
        // the same reply. Separate loopback IP for parallel tests.
        let beacon = test_beacon();
        beacon.set_gateway(true);
        spawn_responder("127.0.0.4".to_string(), beacon.clone());
        // Bounded retry instead of a fixed sleep: a slow bind under CI load
        // must not read as Unreachable and flake the first assertion.
        let mut first = PeerBacking::Unreachable;
        for _ in 0..40 {
            first = peer_backing("127.0.0.4", "peer-node", "data").await;
            if first != PeerBacking::Unreachable {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Fresh beacon: unknown. A peer that has not probed yet must never
        // read as Destroyed.
        assert_eq!(first, PeerBacking::Unknown);
        for (meta, expect) in [
            (BackingMeta::Attached, PeerBacking::Attached),
            (BackingMeta::Destroyed, PeerBacking::Destroyed),
            (BackingMeta::Detached, PeerBacking::Detached),
            (BackingMeta::Absent, PeerBacking::Absent),
            (BackingMeta::Unknown, PeerBacking::Unknown),
        ] {
            beacon.set_backing(meta);
            assert_eq!(peer_backing("127.0.0.4", "peer-node", "data").await, expect);
        }
        // Identity mismatch: an answer from the wrong node or pool must read
        // Unknown even when its state says Destroyed.
        beacon.set_backing(BackingMeta::Destroyed);
        assert_eq!(
            peer_backing("127.0.0.4", "other-node", "data").await,
            PeerBacking::Unknown
        );
        assert_eq!(
            peer_backing("127.0.0.4", "peer-node", "other-res").await,
            PeerBacking::Unknown
        );
        // The one-byte gateway read against the same responder still works.
        assert_eq!(peer_gateway("127.0.0.4").await, PeerGateway::Up);
    }

    #[tokio::test]
    async fn backing_unreachable_when_nobody_listens() {
        // A dead peer (nothing on the port) reads as Unreachable: no
        // attestation, so never grounds for an automatic force-promote.
        assert_eq!(
            peer_backing("127.0.0.5", "peer-node", "data").await,
            PeerBacking::Unreachable
        );
    }

    #[tokio::test]
    async fn old_daemon_bare_byte_reads_as_no_attestation() {
        // A responder from before the backing attestation serves one gateway
        // byte. The attestation query against it must read Unknown, never
        // Destroyed.
        tokio::spawn(async {
            let l = TcpListener::bind(("127.0.0.6", GATEWAY_SIGNAL_PORT))
                .await
                .unwrap();
            loop {
                if let Ok((mut s, _)) = l.accept().await {
                    let _ = s.write_all(b"1").await;
                    let _ = s.shutdown().await;
                }
            }
        });
        let mut got = PeerBacking::Unreachable;
        for _ in 0..40 {
            got = peer_backing("127.0.0.6", "peer-node", "data").await;
            if got != PeerBacking::Unreachable {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(got, PeerBacking::Unknown);
        // The gateway reading on the same reply is intact.
        assert_eq!(peer_gateway("127.0.0.6").await, PeerGateway::Up);
    }

    #[test]
    fn backing_reply_parse_fails_closed() {
        // Malformed, truncated, wrong-protocol, and trailing-junk replies all
        // read Unknown, never Destroyed. Token 0 is the gateway byte.
        for bad in [
            "",
            "D",
            "1", // older daemon: bare gateway byte
            "1 twinstor1",
            "1 twinstor1 peer data",         // no state
            "1 twinstor1 peer data D extra", // trailing junk
            "1 twinstor2 peer data D",       // wrong protocol tag
            "1 twinstor1 other data D",      // wrong node
            "1 twinstor1 peer other D",      // wrong resource
            "1 twinstor1 peer data X",       // unknown state char
            "1 twinstor1 peer data DD",      // not a single char
            "11 twinstor1 peer data D",      // malformed gateway token
            "twinstor1 peer data D",         // missing gateway token
        ] {
            assert_eq!(
                parse_backing_reply(bad, "peer", "data"),
                PeerBacking::Unknown,
                "reply {bad:?} must fail closed"
            );
        }
        assert_eq!(
            parse_backing_reply("1 twinstor1 peer data D\n", "peer", "data"),
            PeerBacking::Destroyed
        );
        assert_eq!(
            parse_backing_reply("? twinstor1 peer data A", "peer", "data"),
            PeerBacking::Absent
        );
        // The bounce flag (stuck-WFBitMap auto-clear): the trailing B must not
        // change the attestation reading, and junk after it still fails closed.
        assert_eq!(
            parse_backing_reply("1 twinstor1 peer data O B\n", "peer", "data"),
            PeerBacking::Attached
        );
        assert_eq!(
            parse_backing_reply("1 twinstor1 peer data D B", "peer", "data"),
            PeerBacking::Destroyed
        );
        assert_eq!(
            parse_backing_reply("1 twinstor1 peer data D B x", "peer", "data"),
            PeerBacking::Unknown
        );
    }

    #[test]
    fn bounce_flag_wire_format() {
        // Byte 0 stays the gateway byte with the flag up (older daemons
        // unaffected), and the flag is a trailing " B" token.
        let b = test_beacon();
        b.set_gateway(true);
        b.set_backing(crate::drbd::BackingMeta::Attached);
        assert_eq!(b.wire_line(), "1 twinstor1 peer-node data O\n");
        b.set_bouncing(true);
        assert_eq!(b.wire_line(), "1 twinstor1 peer-node data O B\n");
        assert_eq!(b.wire_line().as_bytes()[0], b'1');
        b.set_bouncing(false);
        assert_eq!(b.wire_line(), "1 twinstor1 peer-node data O\n");
    }

    #[test]
    fn claim_token_rides_between_backing_and_bounce() {
        let b = test_beacon();
        b.set_gateway(true);
        b.set_backing(crate::drbd::BackingMeta::Attached);
        assert_eq!(
            b.wire_line(),
            "1 twinstor1 peer-node data O\n",
            "no claim outside an episode"
        );
        b.set_claim(Some(ForkClaim::Yielded(0x35F4_1E02_F8DB_6890)));
        assert_eq!(
            b.wire_line(),
            "1 twinstor1 peer-node data O Y35F41E02F8DB6890\n"
        );
        b.set_bouncing(true);
        assert_eq!(
            b.wire_line(),
            "1 twinstor1 peer-node data O Y35F41E02F8DB6890 B\n"
        );
        b.set_bouncing(false);
        b.set_claim(Some(ForkClaim::Won(1)));
        assert_eq!(
            b.wire_line(),
            "1 twinstor1 peer-node data O W0000000000000001\n"
        );
        b.set_claim(None);
        assert_eq!(b.wire_line(), "1 twinstor1 peer-node data O\n");
    }

    #[test]
    fn claim_reply_parse_fails_closed() {
        let y = "1 twinstor1 peer data O Y35F41E02F8DB6890";
        assert_eq!(
            parse_claim_reply(y, "peer", "data"),
            PeerClaim::Claim(ForkClaim::Yielded(0x35F4_1E02_F8DB_6890))
        );
        assert_eq!(
            parse_claim_reply(
                "1 twinstor1 peer data O W00000000000000FF B\n",
                "peer",
                "data"
            ),
            PeerClaim::Claim(ForkClaim::Won(0xFF))
        );
        assert_eq!(
            parse_claim_reply("1 twinstor1 peer data O\n", "peer", "data"),
            PeerClaim::None
        );
        assert_eq!(
            parse_claim_reply("1 twinstor1 peer data O B", "peer", "data"),
            PeerClaim::None
        );
        for bad in [
            "1",                                                           // older daemon: bare gateway byte
            "1 twinstor1 other data O Y35F41E02F8DB6890",                  // wrong node
            "1 twinstor1 peer other O Y35F41E02F8DB6890",                  // wrong resource
            "1 twinstor1 peer data O Y35F41E02F8DB689",                    // 15 hex digits
            "1 twinstor1 peer data O Y35F41E02F8DB6890X",                  // 17 chars of payload
            "1 twinstor1 peer data O Z35F41E02F8DB6890",                   // unknown kind
            "1 twinstor1 peer data O YZZZZZZZZZZZZZZZZ",                   // not hex
            "1 twinstor1 peer data O B Y35F41E02F8DB6890", // claim after the bounce flag
            "1 twinstor1 peer data O Y35F41E02F8DB6890 W35F41E02F8DB6890", // two claims
        ] {
            assert_eq!(
                parse_claim_reply(bad, "peer", "data"),
                PeerClaim::Unknown,
                "{bad}"
            );
        }
    }

    #[test]
    fn a_claim_does_not_change_the_backing_reading_or_the_bounce_flag() {
        assert_eq!(
            parse_backing_reply("1 twinstor1 peer data D Y35F41E02F8DB6890", "peer", "data"),
            PeerBacking::Destroyed
        );
        assert_eq!(
            parse_backing_reply(
                "1 twinstor1 peer data O W35F41E02F8DB6890 B",
                "peer",
                "data"
            ),
            PeerBacking::Attached
        );
        assert_eq!(
            parse_backing_reply("1 twinstor1 peer data O Q35F41E02F8DB6890", "peer", "data"),
            PeerBacking::Unknown
        );
        // The fence-peer script detects the bounce with the glob *" B"*: a claim
        // token never starts with B, so it cannot fake the flag, and the line
        // still ends with " B" while bouncing.
        for c in [
            ForkClaim::Yielded(0xBBBB_BBBB_BBBB_BBBB),
            ForkClaim::Won(0xB000_0000_0000_0000),
        ] {
            let b = test_beacon();
            b.set_claim(Some(c));
            assert!(!b.wire_line().contains(" B"), "{}", b.wire_line());
            b.set_bouncing(true);
            assert!(b.wire_line().trim_end().ends_with(" B"));
        }
    }

    #[tokio::test]
    async fn claim_roundtrip_over_the_shared_port() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let b = PeerBeacon::new("peer", "data");
        b.set_claim(Some(ForkClaim::Yielded(0xABCDEF)));
        let line = b.wire_line();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let _ = s.write_all(line.as_bytes()).await;
        });
        let got = query_line("127.0.0.1", port).await.unwrap();
        assert_eq!(
            parse_claim_reply(&got, "peer", "data"),
            PeerClaim::Claim(ForkClaim::Yielded(0xABCDEF))
        );
    }
}
