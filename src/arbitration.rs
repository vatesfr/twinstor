//! Partition views: what this host can tell, from local and replication-side
//! signals only, about a partition of the client and management network
//! between the two hosts, and the operator verdict for each.
//!
//! The view is computed off the serial tick (every 10 s), published under
//! /run/twinstor for `twinstor status`, and kept in memory for the alert
//! filter: while this host or its peer is parked by XAPI, an alert that
//! advises `pool-ha-disable` or `host-emergency-ha-disable` is suppressed
//! (either releases a parked host as a second master); otherwise such advice
//! is prefixed with the condition under which it is safe.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PartitionView {
    /// None of the views below: nothing to report (also while the P window
    /// is still filling after a daemon start).
    None = 0,
    /// This host reaches neither its clients nor the peer's management
    /// address; the peer is alive on replication and still in the liveset.
    ClientIsolated,
    /// This host reaches its clients; the peer's management address is
    /// silent while the peer is alive on replication.
    PeerClientIsolated,
    /// The gateway is silent but the peer's management address answers.
    GatewayOutage,
    /// Parked by XAPI (a failed HA join seen while armed and cut off), peer
    /// alive, this host's own clients silent; pool.conf still says master.
    ParkedExMaster,
    /// The same, as a pool member.
    ParkedMember,
    /// Parked and the peer unreachable on every channel.
    ParkedDark,
    /// Local xhad Online and holding the master lock, the peer not live in
    /// it, alive on replication, its management address silent: the peer is
    /// parked and its protected VMs run here.
    PeerParked,
    /// The management network is back for less than the settle and this host
    /// is not yet answering normally.
    Rejoining,
    /// The management network is back for less than the settle and the peer
    /// is not yet back in the liveset.
    PeerRejoining,
    /// The peer is unreachable on every channel; this host reaches its
    /// clients.
    PeerGone,
    /// This host's xapi answers as pool master while its own liveset names
    /// the peer as master: a second master.
    SecondMaster,
    /// This host's xhad is in the liveset but its local armed flag is false
    /// under an armed pool: HA protects nothing here.
    DisarmedMember,
    /// Parked by XAPI as ex-master, but this host's own clients answer: the
    /// peer is the side cut off from the clients, and XHA kept it by host
    /// order.
    ParkedExMasterPeerCut,
    /// The same, as a pool member.
    ParkedMemberPeerCut,
    /// This host's own clients are silent, yet XHA kept it and removed the
    /// peer (the host-order rule): the VMs here are likely unreachable.
    IsolatedSurvivor,
    /// The peer has left this host's liveset while its management address is
    /// silent, but this host does not hold the master lock yet: XHA is
    /// expected to restart the peer's protected VMs here.
    PeerParkedPending,
    /// This host reaches neither its clients nor the peer on any channel.
    Blind,
    /// Both hosts lost their client network: the whole-switch hold keeps
    /// both, locally disarmed, with their VMs where they are.
    WholeSwitchHold,
    /// This host stood its own storage down after losing its clients (it was
    /// not restarted by XHA).
    StoodDown,
    /// The peer stood its own storage down (Secondary while still
    /// Connected, out of the liveset): not restarted by XHA.
    PeerStoodDown,
    /// A transition with no claim: the peer's management address answers
    /// again but P has not matured, or the shown view's premise (the peer
    /// unreachable on every channel) just ended. Shown at once; the next view
    /// still holds its evaluations.
    Settling,
    /// Armed, not in the liveset, the peer's management address silent and
    /// the peer alive, before any failed HA join: XAPI keeps this host out of
    /// the pool while it tries to join (the startup-loop condition).
    Joining,
    /// The peer's own XAPI answers that it is locally disarmed under the armed
    /// pool (seen by the peer-emergency probe while P holds).
    PeerDisarmed,
}

impl PartitionView {
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ClientIsolated => "ClientIsolated",
            Self::PeerClientIsolated => "PeerClientIsolated",
            Self::GatewayOutage => "GatewayOutage",
            Self::ParkedExMaster | Self::ParkedExMasterPeerCut => "Parked (ex-master)",
            Self::ParkedMember | Self::ParkedMemberPeerCut => "Parked (member)",
            Self::ParkedDark => "ParkedDark",
            Self::PeerParked | Self::PeerParkedPending => "PeerParked",
            Self::Rejoining => "Rejoining",
            Self::PeerRejoining => "PeerRejoining",
            Self::PeerGone => "PeerGone",
            Self::SecondMaster => "SecondMaster",
            Self::DisarmedMember => "DisarmedMember",
            Self::IsolatedSurvivor => "IsolatedSurvivor",
            Self::Blind => "Blind",
            Self::WholeSwitchHold => "WholeSwitchHold",
            Self::StoodDown => "StoodDown",
            Self::PeerStoodDown => "PeerStoodDown",
            Self::Settling => "Settling",
            Self::Joining => "Joining",
            Self::PeerDisarmed => "PeerDisarmed",
        }
    }

    fn from_u8(v: u8) -> Self {
        const ALL: [PartitionView; 24] = [
            PartitionView::None,
            PartitionView::ClientIsolated,
            PartitionView::PeerClientIsolated,
            PartitionView::GatewayOutage,
            PartitionView::ParkedExMaster,
            PartitionView::ParkedMember,
            PartitionView::ParkedDark,
            PartitionView::PeerParked,
            PartitionView::Rejoining,
            PartitionView::PeerRejoining,
            PartitionView::PeerGone,
            PartitionView::SecondMaster,
            PartitionView::DisarmedMember,
            PartitionView::ParkedExMasterPeerCut,
            PartitionView::ParkedMemberPeerCut,
            PartitionView::IsolatedSurvivor,
            PartitionView::PeerParkedPending,
            PartitionView::Blind,
            PartitionView::WholeSwitchHold,
            PartitionView::StoodDown,
            PartitionView::PeerStoodDown,
            PartitionView::Settling,
            PartitionView::Joining,
            PartitionView::PeerDisarmed,
        ];
        ALL.get(v as usize).copied().unwrap_or(Self::None)
    }

    /// A view in which a host is held out of the pool by XAPI: advising a
    /// pool HA disable or a local emergency disable would release it as a
    /// second master.
    pub fn parked(self) -> bool {
        matches!(
            self,
            Self::ParkedExMaster
                | Self::ParkedMember
                | Self::ParkedExMasterPeerCut
                | Self::ParkedMemberPeerCut
                | Self::ParkedDark
                | Self::Joining
                | Self::PeerParked
                | Self::PeerParkedPending
                | Self::IsolatedSurvivor
        )
    }
}

/// The inputs of one classification, all local or replication-side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ViewInputs {
    /// The P window holds its full count of probes (no view before: P is
    /// false at every daemon start until the probes have run).
    pub p_window_full: bool,
    /// This host's clients (the fence-peer probe set: the gateway or a
    /// ping-target) answer; `None` when no target is configured.
    pub own_clients: Option<bool>,
    /// P: the peer's management address answers.
    pub p: bool,
    /// The prober's most recent probe of the peer's management address
    /// answered (P may still be re-maturing).
    pub last_probe_answered: bool,
    /// P has held for the settle.
    pub settled: bool,
    /// The peer answers on a replication path (or the direct link).
    pub peer_alive: bool,
    /// local.db `ha.armed`.
    pub local_armed: Option<bool>,
    /// The local xhad is Online (in the liveset).
    pub xhad_online: bool,
    /// The peer is live in this host's liveset (`None`: unreadable).
    pub peer_live_in_liveset: Option<bool>,
    /// This host holds the master lock in its own liveset.
    pub holds_master_lock: bool,
    /// A failed HA join was seen in xha.log in this boot while this host was
    /// not in the liveset and cut off, and it has not joined, seen P, or read
    /// disarmed since (`ParkMemory`): XAPI holds it in its HA startup loop.
    pub park_confirmed: bool,
    /// pool.conf says master.
    pub conf_master: bool,
    /// The local xapi answers.
    pub local_xapi_answers: bool,
    /// The local xapi answers as pool master (`None`: not read).
    pub local_xapi_master: Option<bool>,
    /// This host's own liveset shows the peer live and holding the master
    /// lock, and this host not holding it.
    pub liveset_names_peer_master: bool,
    /// Pool HA (`None`: not read).
    pub pool_ha: Option<bool>,
    /// This host's own XAPI answers a login with
    /// HA_POOL_IS_ENABLED_BUT_HOST_IS_DISABLED: local evidence of a host
    /// disarmed under the armed pool, whether or not its xhad runs.
    pub own_xapi_armed_mismatch: bool,
    /// The peer's XAPI recently answered the same (the peer-emergency probe).
    pub peer_disarmed: bool,
    /// A whole-switch hold stands on this host.
    pub whole_switch_hold: bool,
    /// This host's own isolated stand-down marker stands.
    pub stood_down: bool,
    /// The peer left this host's liveset while the DRBD link was Connected,
    /// the link has not dropped since, and the peer is Secondary on it: a
    /// stood-down peer keeps replicating as Secondary, while a fenced one
    /// reboots and drops the link before it reconnects (see
    /// `StandDownLink`).
    pub peer_left_on_held_link: bool,
}

/// Classify the partition from local and replication-side inputs. This
/// host's own client probe tells the two sides of a cut apart: the host whose
/// clients answer is not the cut-off one, whatever XHA decided.
pub fn classify_partition(i: &ViewInputs) -> PartitionView {
    use PartitionView as V;
    if !i.p_window_full {
        return V::None;
    }
    if i.local_xapi_master == Some(true) && i.liveset_names_peer_master {
        return V::SecondMaster;
    }
    if i.whole_switch_hold {
        return V::WholeSwitchHold;
    }
    if i.stood_down {
        return V::StoodDown;
    }
    if i.local_armed == Some(false)
        && ((i.xhad_online && i.pool_ha == Some(true)) || i.own_xapi_armed_mismatch)
    {
        return V::DisarmedMember;
    }
    if i.p && i.peer_disarmed {
        return V::PeerDisarmed;
    }
    // The peer's management address answers again while P re-matures (after
    // a cut is restored, or a peer reboots): neither side is shown cut off.
    if !i.p && i.last_probe_answered {
        return V::Settling;
    }
    // An unreadable armed flag keeps a confirmed park (`ParkMemory` ends it
    // once the flag reads disarmed for good).
    let parked = i.park_confirmed && i.local_armed != Some(false) && !i.xhad_online && !i.p;
    if parked {
        return match (i.peer_alive, i.own_clients, i.conf_master) {
            (false, _, _) => V::ParkedDark,
            (true, Some(true), true) => V::ParkedExMasterPeerCut,
            (true, Some(true), false) => V::ParkedMemberPeerCut,
            (true, _, true) => V::ParkedExMaster,
            (true, _, false) => V::ParkedMember,
        };
    }
    if i.local_armed == Some(true) && !i.xhad_online && !i.p && i.peer_alive {
        return V::Joining;
    }
    if !i.p && i.peer_alive && i.xhad_online && i.peer_live_in_liveset == Some(false) {
        return if i.peer_left_on_held_link {
            V::PeerStoodDown
        } else if i.own_clients == Some(false) {
            V::IsolatedSurvivor
        } else if i.holds_master_lock {
            V::PeerParked
        } else {
            V::PeerParkedPending
        };
    }
    if !i.p && i.peer_alive {
        match i.own_clients {
            Some(false) => return V::ClientIsolated,
            Some(true) => return V::PeerClientIsolated,
            None => {}
        }
    }
    if i.p && i.own_clients == Some(false) {
        return V::GatewayOutage;
    }
    if !i.p && !i.peer_alive {
        match i.own_clients {
            Some(true) => return V::PeerGone,
            Some(false) => return V::Blind,
            None => {}
        }
    }
    if i.p && !i.settled {
        if (i.local_armed == Some(true) && !i.xhad_online) || !i.local_xapi_answers {
            return V::Rejoining;
        }
        if i.xhad_online && i.peer_live_in_liveset == Some(false) {
            return V::PeerRejoining;
        }
    }
    V::None
}

/// Tells a peer that stood its own storage down (it leaves the liveset while
/// the DRBD link stays Connected, and stays Secondary on it) from one XHA
/// fenced (its reboot drops the link, and it may reconnect as Secondary,
/// parked). Fed every evaluation; a link drop between two samples of a
/// stood-down peer is not expected (a reboot takes far longer than one).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StandDownLink {
    epoch: u64,
    was_connected: bool,
    peer_was_live: Option<bool>,
    left_epoch: Option<u64>,
}

impl StandDownLink {
    /// `connected`: the DRBD link is Connected now; `peer_live`: the peer is
    /// live in this host's liveset (`None` unreadable). Returns whether the
    /// peer left the liveset on a link that has held since.
    pub fn observe(&mut self, connected: bool, peer_live: Option<bool>) -> bool {
        if self.was_connected && !connected {
            self.epoch = self.epoch.wrapping_add(1);
        }
        self.was_connected = connected;
        match peer_live {
            Some(true) => self.left_epoch = None,
            Some(false) if self.peer_was_live == Some(true) => {
                self.left_epoch = connected.then_some(self.epoch);
            }
            _ => {}
        }
        if peer_live.is_some() {
            self.peer_was_live = peer_live;
        }
        connected && self.left_epoch == Some(self.epoch)
    }
}

/// The park, remembered for the boot: once a failed HA join was seen while
/// this host was out of the liveset and cut off, it stays parked until it
/// joins (xhad Online), P holds, or its armed flag reads disarmed for
/// `VIEW_HOLD_EVALUATIONS` evaluations in a row (released). XAPI's join cycle
/// (an xhad restart every few minutes, with unreadable reads in between) does
/// not end it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ParkMemory {
    confirmed: bool,
    disarmed_evaluations: u8,
    /// Consecutive evaluations with this host's HA daemon reading Online.
    online_evaluations: u8,
}

impl ParkMemory {
    pub fn remembered() -> Self {
        Self { confirmed: true, disarmed_evaluations: 0, online_evaluations: 0 }
    }

    pub fn confirmed(&self) -> bool {
        self.confirmed
    }

    /// `failed_join`: a failed HA join was seen since the last evaluation.
    pub fn observe(
        &mut self,
        failed_join: bool,
        xhad_online: bool,
        p: bool,
        local_armed: Option<bool>,
    ) -> bool {
        // P back ends it at once (P is itself a 4-of-6 window). An Online
        // read must hold for `VIEW_HOLD_EVALUATIONS` evaluations: XAPI's
        // startup loop restarts the HA daemon, which can read Online for a
        // sample before its join fails again.
        if p {
            *self = Self::default();
            return false;
        }
        if xhad_online {
            self.online_evaluations = self.online_evaluations.saturating_add(1);
            if self.online_evaluations >= VIEW_HOLD_EVALUATIONS {
                *self = Self::default();
                return false;
            }
            return self.confirmed;
        }
        self.online_evaluations = 0;
        if local_armed == Some(false) {
            self.disarmed_evaluations = self.disarmed_evaluations.saturating_add(1);
            if self.disarmed_evaluations >= VIEW_HOLD_EVALUATIONS {
                self.confirmed = false;
            }
        } else {
            self.disarmed_evaluations = 0;
            self.confirmed |= failed_join;
        }
        self.confirmed
    }
}

/// Where the view task remembers a confirmed park for this boot (tmpfs, so a
/// reboot clears it; a daemon restart keeps it).
pub static PARK_PATH: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| crate::heal_state::state_path("/run/twinstor/park-confirmed"));

/// How many consecutive evaluations (10 s apart) a new view must hold before
/// it replaces the shown one: a host in its boot-join loop flips for one or
/// two evaluations, and the survivor's liveset changes ahead of XAPI.
pub const VIEW_HOLD_EVALUATIONS: u8 = 3;

/// The shown view, replaced only by a view that has held
/// `VIEW_HOLD_EVALUATIONS` consecutive evaluations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewHysteresis {
    shown: PartitionView,
    candidate: PartitionView,
    count: u8,
}

impl Default for ViewHysteresis {
    fn default() -> Self {
        Self {
            shown: PartitionView::None,
            candidate: PartitionView::None,
            count: 0,
        }
    }
}

/// Whether the shown view's premise is contradicted by these inputs: a view
/// saying the peer is unreachable on every channel, while the peer answers.
fn contradicted(shown: PartitionView, i: &ViewInputs) -> bool {
    use PartitionView as V;
    matches!(shown, V::PeerGone | V::Blind | V::ParkedDark) && i.peer_alive
}

impl ViewHysteresis {
    /// Feed one evaluation with its inputs; returns the view to show.
    /// `Settling` makes no claim, so it is shown at once; so it is when the
    /// shown view's premise has just ended. The next real view still holds
    /// its evaluations.
    pub fn update_with(&mut self, view: PartitionView, i: &ViewInputs) -> PartitionView {
        if view != self.shown
            && (view == PartitionView::Settling || contradicted(self.shown, i))
        {
            self.shown = PartitionView::Settling;
            self.candidate = view;
            self.count = u8::from(view != PartitionView::Settling);
            return self.shown;
        }
        self.update(view)
    }

    /// Feed one evaluation; returns the view to show.
    pub fn update(&mut self, view: PartitionView) -> PartitionView {
        if view == self.shown {
            self.candidate = view;
            self.count = 0;
            return self.shown;
        }
        if view == self.candidate {
            self.count = self.count.saturating_add(1);
        } else {
            self.candidate = view;
            self.count = 1;
        }
        if self.count >= VIEW_HOLD_EVALUATIONS {
            self.shown = view;
            self.count = 0;
        }
        self.shown
    }
}

/// The operator verdict for a view. Increment 1 manages no XHA weight, so a
/// cut-off host's fate is XHA's host order.
/// The PeerDisarmed view's text, its reset budget taken from the budget's
/// own constants.
static PEER_DISARMED_VERDICT: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let (count, window, spacing, after_futile) = crate::state_machine::peer_emergency_budget_rules();
    format!(
        "The peer's XAPI answers that it is locally disarmed under the armed pool: HA does not \
         protect the peer, and a reboot or toolstack restart of it can leave it in XAPI's HA \
         emergency mode or turn pool HA off. TwinStor resets pool HA (one disable and enable) \
         only if its conditions hold: the answer seen again on a later probe, the peer's \
         management address answering, no pool HA operation pending, \
         auto_resolve_peer_emergency on, and its reset budget (at most {count} resets in {} \
         hours, {} minutes apart, {} minutes after a reset that did not free the peer).",
        window / 3600,
        spacing / 60,
        after_futile / 60
    )
});

pub fn verdict(view: PartitionView) -> &'static str {
    match view {
        PartitionView::None => "",
        PartitionView::ClientIsolated => {
            "This host cannot reach its clients or the peer's management address; the peer is \
             alive on replication. XHA decides by host order which host it restarts. Restore this \
             host's network; do not disable pool HA."
        }
        PartitionView::PeerClientIsolated => {
            "The peer's management address is silent while this host reaches its clients and the \
             peer is alive on replication: the peer is the side cut off. No action here; do not \
             disable pool HA."
        }
        PartitionView::GatewayOutage => {
            "The gateway is silent but the peer's management address answers: a gateway or \
             router problem. No HA action."
        }
        PartitionView::ParkedExMaster => {
            "XHA restarted this host and its own network is still cut off from its clients. XAPI \
             holds it out of the pool (no API, no VMs; pool.conf still says master, as expected). \
             It rejoins as a member when the network returns. Do not disable pool HA or run \
             host-emergency-ha-disable: either releases it as a second master. VMs that ran here \
             and are not HA-protected must be started by hand afterwards."
        }
        PartitionView::ParkedMember => {
            "XHA restarted this host; its own network is still cut off from its clients. XAPI \
             holds it out of the pool; it rejoins by itself (up to about 5 minutes after the \
             network returns). Do not disable pool HA or run host-emergency-ha-disable. VMs that \
             ran here and are not HA-protected must be started by hand afterwards."
        }
        PartitionView::ParkedExMasterPeerCut => {
            "This host's own network is fine: the peer is the side cut off from the clients. XHA \
             kept the peer by host order and restarted this host; XAPI holds this host out of the \
             pool while the peer's management address does not answer (pool.conf still says \
             master, as expected). It rejoins as a member when the peer's management network \
             returns; until then the VMs on the peer are likely unreachable by clients: fix the \
             peer's network or switch port. Do not disable pool HA or run \
             host-emergency-ha-disable: either releases this host as a second master."
        }
        PartitionView::ParkedMemberPeerCut => {
            "This host's own network is fine: the peer is the side cut off from the clients. XHA \
             kept the peer by host order and restarted this host; XAPI holds this host out of the \
             pool and it rejoins by itself when the peer's management network returns. Until \
             then the VMs on the peer are likely unreachable by clients: fix the peer's network or \
             switch port. Do not disable pool HA or run host-emergency-ha-disable."
        }
        PartitionView::ParkedDark => {
            "This host reaches neither its clients nor its peer. XAPI holds it out of the pool; \
             its storage is frozen or held, and its copy is resynced from the peer when \
             replication returns. Nothing to do until a network returns."
        }
        PartitionView::IsolatedSurvivor => {
            "This host cannot reach its clients, yet XHA kept it by host order and removed the \
             peer. The VMs it runs, including those XHA restarts here from the peer, are likely \
             unreachable by clients. Fix this host's network or switch port; do not disable pool \
             HA."
        }
        PartitionView::PeerParked => {
            "The peer was restarted by XHA and is still cut off; this host holds the HA master \
             lock and runs the protected VMs; pool HA protects this host alone. The peer rejoins \
             by itself. Do not disable pool HA."
        }
        PartitionView::PeerParkedPending => {
            "The peer has left the HA liveset and its management address is silent; XHA is \
             expected to make this host master and restart the peer's protected VMs here. Do not \
             disable pool HA."
        }
        PartitionView::Rejoining => {
            "The peer's management address answers; this host's XAPI or HA daemon is still \
             starting or reconnecting (usually under 5 minutes). Repairs wait until then."
        }
        PartitionView::PeerRejoining => {
            "The peer's management address answers; the peer is not back in the HA liveset yet \
             (usually under 5 minutes). Repairs wait until then."
        }
        PartitionView::Settling => {
            "The peer has just come back on its management address or on replication; the view \
             settles within about a minute. No action."
        }
        PartitionView::Joining => {
            "This host is armed for HA but not in the HA liveset, and the peer's management \
             address does not answer: XAPI keeps it out of the pool while it tries to join. Do \
             not disable pool HA or run host-emergency-ha-disable: either releases it as a \
             second master."
        }
        PartitionView::PeerDisarmed => PEER_DISARMED_VERDICT.as_str(),
        PartitionView::PeerGone => {
            "The peer is unreachable on every channel; this host reaches its clients."
        }
        PartitionView::Blind => {
            "This host reaches neither its clients nor the peer on any channel: either its own \
             networks are down, or the peer and the gateway are both gone. No HA action; observe."
        }
        PartitionView::WholeSwitchHold => {
            "Both hosts lost their client network (the switch or the router): TwinStor holds \
             both hosts, locally disarmed so XHA fences neither, and keeps the VMs running where \
             they are. Restore the switch. On release TwinStor re-arms HA by itself (one pool HA \
             cycle) unless auto_resolve_peer_emergency is off, in which case an alert asks for \
             that cycle by hand."
        }
        PartitionView::StoodDown => {
            "This host lost its clients while the peer stayed alive, and stood its own storage \
             down: it left the HA liveset (XHA did not restart it), its VMs restart on the peer, \
             and it stays parked until its client network returns, then rejoins by itself."
        }
        PartitionView::PeerStoodDown => {
            "The peer lost its clients and stood its own storage down: it left the HA liveset \
             (XHA did not restart it), and its protected VMs run here or are restarted here by \
             XHA. It rejoins by itself when its client network returns. Do not disable pool HA."
        }
        PartitionView::SecondMaster => {
            "This host answers as pool master while its own HA liveset names the peer as \
             master: two masters. It must not start VMs; TwinStor demotes it to a slave of the \
             peer once the peer's management address answers."
        }
        PartitionView::DisarmedMember => {
            "This host is locally disarmed under the armed pool: HA does not protect it, and a \
             reboot or toolstack restart of it can leave it in XAPI's HA emergency mode or turn \
             pool HA off. A pool HA cycle re-arms it. TwinStor runs \
             one only if its conditions hold: it disarmed this host itself, or the master \
             confirms this state; the other host's management address answering; and \
             auto_resolve_peer_emergency on."
        }
    }
}

/// Views in which `twinstor status` must not call the host healthy, whatever
/// storage says: every view. Each one is a partition, a park, a second master
/// or a host HA does not protect, seen from this side.
pub fn degrades_health(view_name: &str) -> bool {
    !view_name.is_empty() && view_name != PartitionView::None.name()
}

static CURRENT_VIEW: AtomicU8 = AtomicU8::new(0);

/// Publish the current view (the alert filter reads it). A view that shows
/// the peer away also sets the "peer was away" latch (see
/// [`take_peer_away_seen`]).
pub fn set_current(view: PartitionView) {
    CURRENT_VIEW.store(view as u8, Ordering::Relaxed);
    if peer_away_view(view) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(1)
            .max(1);
        let _ = PEER_AWAY_SINCE.compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed);
    }
}

/// Views (as shown, after the view's own hysteresis) in which the peer is
/// away from the pool as this host sees it: gone, parked, or cut off from
/// its clients.
pub fn peer_away_view(view: PartitionView) -> bool {
    matches!(
        view,
        PartitionView::PeerGone
            | PartitionView::PeerParked
            | PartitionView::PeerParkedPending
            | PartitionView::PeerClientIsolated
    )
}

/// When a peer-away view was first shown since the latch was last taken
/// (unix; 0: none); the master's rejoin watch consumes it at its next HA
/// sample, which samples only every 10 minutes and can miss a shorter
/// absence.
static PEER_AWAY_SINCE: AtomicU64 = AtomicU64::new(0);

/// When a peer-away view was first shown since the last call, if one was
/// (and clear it).
pub fn take_peer_away_since() -> Option<u64> {
    match PEER_AWAY_SINCE.swap(0, Ordering::Relaxed) {
        0 => None,
        t => Some(t),
    }
}

/// Forget a peer absence seen so far (this host is no longer master).
pub fn clear_peer_away_seen() {
    PEER_AWAY_SINCE.store(0, Ordering::Relaxed);
}

pub fn current() -> PartitionView {
    PartitionView::from_u8(CURRENT_VIEW.load(Ordering::Relaxed))
}

static ADVICE_HOLD: AtomicBool = AtomicBool::new(false);

/// Whether a host is held out of the pool, from the inputs rather than the
/// shown view: a confirmed park (the park memory), or any parked view the
/// inputs give with the `Settling` rule set aside. A gray management link
/// whose latest probe answers now and then shows `Settling` most of the time,
/// yet the host is still held, and advice to disable HA would still release
/// it as a second master.
pub fn holds_ha_disable_advice(i: &ViewInputs) -> bool {
    i.park_confirmed
        || classify_partition(&ViewInputs { last_probe_answered: false, ..*i }).parked()
}

/// Publish the input-based hold (the view task, every evaluation).
pub fn set_advice_hold(hold: bool) {
    ADVICE_HOLD.store(hold, Ordering::Relaxed);
}

/// The alert filter's hold: the shown view is a parked one, or the inputs
/// say a host is held (`holds_ha_disable_advice`). The display can only add
/// to the hold, never release it.
pub fn advice_held() -> bool {
    current().parked() || ADVICE_HOLD.load(Ordering::Relaxed)
}

/// The prefix put on advice to disable HA when no host is parked.
pub const HA_DISABLE_ADVICE_PREFIX: &str =
    "Only when both hosts' management networks are up: ";

/// Identity-key prefix marking an alert that advises an HA disable
/// (`pool-ha-disable` or `host-emergency-ha-disable`): only alerts sent
/// through the dedicated senders carry it, never one matched by its text.
pub const HA_DISABLE_ADVICE_KEY: &str = "ha-disable-advice:";

/// What to do with one alert now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlertRoute {
    /// Deliver this body.
    Send(String),
    /// Keep it held back: a host is parked, and the advice would release it
    /// as a second master. Delivered once no host is parked.
    Hold,
}

/// Route an alert: one that advises an HA disable is held while a host is
/// held out of the pool (`held`, from `advice_held`) and otherwise prefixed
/// with when the advice is safe; any other alert passes unchanged.
pub fn route_alert(held: bool, advice: bool, body: &str) -> AlertRoute {
    if !advice {
        return AlertRoute::Send(body.to_string());
    }
    if held {
        return AlertRoute::Hold;
    }
    if body.starts_with(HA_DISABLE_ADVICE_PREFIX) {
        return AlertRoute::Send(body.to_string());
    }
    AlertRoute::Send(format!("{HA_DISABLE_ADVICE_PREFIX}{body}"))
}

/// Where the view task publishes the view for `twinstor status`.
pub static STATUS_PATH: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| crate::heal_state::state_path("/run/twinstor/partition-view"));
/// Where the tick publishes its last withheld HA action.
pub static WITHHELD_PATH: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| crate::heal_state::state_path("/run/twinstor/ha-withheld"));
/// Where the HA watchdog publishes why an owed re-arm is held.
pub static REARM_HOLD_PATH: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| crate::heal_state::state_path("/run/twinstor/ha-rearm-hold"));

/// Write a small `key=value` status file (best effort, tmpfs).
pub fn write_status(path: &str, pairs: &[(&str, &str)]) {
    if let Some(dir) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut body = String::new();
    for (k, v) in pairs {
        body.push_str(k);
        body.push('=');
        body.push_str(&v.replace('\n', " "));
        body.push('\n');
    }
    body.push_str(&format!("updated_unix={now}\n"));
    let _ = std::fs::write(path, body);
}

/// Read one key of a status file written by `write_status`.
pub fn status_value(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}=")))
        .map(|v| v.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> ViewInputs {
        ViewInputs {
            p_window_full: true,
            own_clients: Some(true),
            p: true,
            // Rows that cut P keep the latest probe unanswered too.
            last_probe_answered: false,
            settled: true,
            peer_alive: true,
            local_armed: Some(true),
            xhad_online: true,
            peer_live_in_liveset: Some(true),
            holds_master_lock: false,
            park_confirmed: false,
            conf_master: false,
            local_xapi_answers: true,
            local_xapi_master: Some(false),
            liveset_names_peer_master: false,
            pool_ha: Some(true),
            whole_switch_hold: false,
            stood_down: false,
            peer_left_on_held_link: false,
            own_xapi_armed_mismatch: false,
            peer_disarmed: false,
        }
    }

    #[test]
    fn every_row_of_the_view_table() {
        use PartitionView::*;
        // Healthy pool.
        assert_eq!(classify_partition(&base()), None);
        // ClientIsolated: own clients unreachable, P false, peer alive.
        let i = ViewInputs { own_clients: Some(false), p: false, settled: false, ..base() };
        assert_eq!(classify_partition(&i), ClientIsolated);
        // PeerClientIsolated: own clients reachable, P false, peer alive.
        let i = ViewInputs { p: false, settled: false, ..base() };
        assert_eq!(classify_partition(&i), PeerClientIsolated);
        // GatewayOutage: own clients unreachable, P true.
        let i = ViewInputs { own_clients: Some(false), ..base() };
        assert_eq!(classify_partition(&i), GatewayOutage);
        // Parked (a failed join seen), own clients silent: this host is cut.
        let parked = ViewInputs {
            p: false,
            settled: false,
            xhad_online: false,
            park_confirmed: true,
            own_clients: Some(false),
            ..base()
        };
        assert_eq!(classify_partition(&ViewInputs { conf_master: true, ..parked }), ParkedExMaster);
        assert_eq!(classify_partition(&parked), ParkedMember);
        // ParkedDark: the same with the peer unreachable everywhere.
        assert_eq!(classify_partition(&ViewInputs { peer_alive: false, ..parked }), ParkedDark);
        // PeerParked: master lock held here, peer not live, peer alive, P false.
        let i = ViewInputs {
            p: false,
            settled: false,
            peer_live_in_liveset: Some(false),
            holds_master_lock: true,
            ..base()
        };
        assert_eq!(classify_partition(&i), PeerParked);
        // Rejoining: P back for less than the settle, this host not answering.
        let i = ViewInputs { settled: false, xhad_online: false, ..base() };
        assert_eq!(classify_partition(&i), Rejoining);
        let i = ViewInputs { settled: false, local_xapi_answers: false, ..base() };
        assert_eq!(classify_partition(&i), Rejoining);
        // PeerRejoining: P back for less than the settle, peer not yet live.
        let i = ViewInputs { settled: false, peer_live_in_liveset: Some(false), ..base() };
        assert_eq!(classify_partition(&i), PeerRejoining);
        // Settled and normal: nothing.
        let i = ViewInputs { settled: false, ..base() };
        assert_eq!(classify_partition(&i), None);
        // PeerGone: peer unreachable everywhere, own clients reachable.
        let gone = ViewInputs {
            p: false,
            settled: false,
            peer_alive: false,
            local_armed: Some(false),
            xhad_online: false,
            ..base()
        };
        assert_eq!(classify_partition(&gone), PeerGone);
        // Blind: the same with this host's own clients silent too.
        assert_eq!(classify_partition(&ViewInputs { own_clients: Some(false), ..gone }), Blind);
        // SecondMaster: the local xapi answers as master, the own liveset
        // names the peer master (a stand-down release, or a partial heal).
        let i = ViewInputs { local_xapi_master: Some(true), liveset_names_peer_master: true,
                             conf_master: true, local_armed: Some(false), ..base() };
        assert_eq!(classify_partition(&i), SecondMaster);
        // DisarmedMember: in the liveset, locally disarmed, pool HA on (a
        // whole-switch release).
        let i = ViewInputs { local_armed: Some(false), ..base() };
        assert_eq!(classify_partition(&i), DisarmedMember);
        // Pool HA off: not a disarmed member.
        let i = ViewInputs { local_armed: Some(false), pool_ha: Some(false), ..base() };
        assert_eq!(classify_partition(&i), None);
        // An unarmed host never reads parked.
        let i = ViewInputs { local_armed: Some(false), ..parked };
        assert_eq!(classify_partition(&i), ClientIsolated);
        assert!(degrades_health("ClientIsolated") && degrades_health("PeerParked"));
        assert!(!degrades_health("none") && !degrades_health(""));
    }

    /// XHA's host-order rule kept the host whose clients are cut off and
    /// fenced the healthy one (equal weights, the cut host first in order).
    /// The cut host, now master of a liveset without the peer and running all
    /// the VMs, reads that it is the cut-off side; the healthy host, parked
    /// after its reboot with its own gateway answering, reads that the peer
    /// is the cut-off side.
    #[test]
    fn the_host_order_rule_keeping_the_cut_host_is_told_truthfully_on_both_sides() {
        use PartitionView::*;
        let winner = ViewInputs {
            own_clients: Some(false),
            p: false,
            settled: false,
            peer_alive: true,
            xhad_online: true,
            peer_live_in_liveset: Some(false),
            holds_master_lock: true,
            conf_master: true,
            local_xapi_master: Some(true),
            ..base()
        };
        assert_eq!(classify_partition(&winner), IsolatedSurvivor);
        assert!(verdict(IsolatedSurvivor).contains("This host cannot reach its clients"));
        assert!(!verdict(IsolatedSurvivor).contains("peer was restarted"));
        assert!(degrades_health(IsolatedSurvivor.name()));
        // Before the lock moved here: still this host's side, never PeerParked.
        assert_eq!(classify_partition(&ViewInputs { holds_master_lock: false, ..winner }), IsolatedSurvivor);
        for conf_master in [true, false] {
            let fenced = ViewInputs {
                own_clients: Some(true),
                p: false,
                settled: false,
                peer_alive: true,
                local_armed: Some(true),
                xhad_online: false,
                park_confirmed: true,
                conf_master,
                local_xapi_answers: false,
                local_xapi_master: Option::None,
                ..base()
            };
            let v = classify_partition(&fenced);
            assert_eq!(v, if conf_master { ParkedExMasterPeerCut } else { ParkedMemberPeerCut });
            assert!(verdict(v).contains("This host's own network is fine"));
            assert!(verdict(v).contains("host-emergency-ha-disable"));
            assert!(!verdict(v).contains("its own network is still cut off"));
        }
    }

    /// The healthy survivor whose liveset dropped the peer before XAPI made it
    /// master: XHA is expected to restart the VMs here; they do not run yet.
    #[test]
    fn peer_parked_claims_the_vms_only_with_the_master_lock() {
        use PartitionView::*;
        let i = ViewInputs {
            p: false,
            settled: false,
            peer_live_in_liveset: Some(false),
            holds_master_lock: false,
            ..base()
        };
        assert_eq!(classify_partition(&i), PeerParkedPending);
        assert!(verdict(PeerParkedPending).contains("expected to"));
        assert!(!verdict(PeerParkedPending).contains("runs the protected VMs"));
        assert_eq!(classify_partition(&ViewInputs { holds_master_lock: true, ..i }), PeerParked);
    }

    /// After a daemon start P is false until its probes have run: a healthy
    /// pool shows no view while the window fills. A host armed and not yet in
    /// the liveset, cut from the peer's management address, is not parked
    /// without a failed join: it reads `Joining`, which claims nothing about
    /// which side is cut or who XHA restarted.
    #[test]
    fn no_view_while_p_warms_up_and_no_park_without_a_failed_join() {
        use PartitionView::*;
        let start = ViewInputs { p_window_full: false, p: false, settled: false, ..base() };
        assert_eq!(classify_partition(&start), None);
        let rebooted = ViewInputs {
            p: false,
            settled: false,
            xhad_online: false,
            park_confirmed: false,
            ..base()
        };
        assert_ne!(classify_partition(&rebooted), ParkedMember);
        assert_ne!(classify_partition(&rebooted), ParkedMemberPeerCut);
        assert_eq!(classify_partition(&rebooted), Joining);
        assert_eq!(classify_partition(&ViewInputs { own_clients: Some(false), ..rebooted }), Joining);
        assert!(!verdict(Joining).contains("restarted"));
        assert!(!verdict(Joining).contains("side cut off"));
        assert!(Joining.parked());
        // Not armed (pool HA off): the cut views as before.
        let unarmed = ViewInputs { local_armed: Some(false), pool_ha: Some(false), ..rebooted };
        assert_eq!(classify_partition(&unarmed), PeerClientIsolated);
    }

    /// After a cut is restored, P re-matures over several probes while the
    /// latest probe already answers: both hosts read `Settling`, shown at
    /// once, never "the peer is the side cut off".
    #[test]
    fn after_a_revert_both_hosts_settle_without_naming_a_cut_side() {
        use PartitionView::*;
        let reverted = ViewInputs { p: false, settled: false, last_probe_answered: true, ..base() };
        // The formerly cut survivor (master lock, peer out of its liveset)
        // and the parked or fenced healthy host.
        let cut_survivor = ViewInputs {
            peer_live_in_liveset: Some(false),
            holds_master_lock: true,
            ..reverted
        };
        let parked = ViewInputs { xhad_online: false, park_confirmed: true, ..reverted };
        for (i, before) in [
            (reverted, PeerClientIsolated),
            (cut_survivor, IsolatedSurvivor),
            (parked, ParkedExMasterPeerCut),
        ] {
            assert_eq!(classify_partition(&i), Settling);
            let mut h = ViewHysteresis { shown: before, candidate: before, count: 0 };
            assert_eq!(h.update_with(classify_partition(&i), &i), Settling);
        }
        assert!(!verdict(Settling).contains("cut off"));
        // P matured, settle running: the rejoin views (their texts do not say
        // the network left), then nothing; each holds its evaluations.
        let mut h = ViewHysteresis { shown: Settling, candidate: Settling, count: 0 };
        let rejoin = ViewInputs { settled: false, xhad_online: false, ..base() };
        assert_eq!(classify_partition(&rejoin), Rejoining);
        assert!(!verdict(Rejoining).contains("network is back"));
        assert!(!verdict(PeerRejoining).contains("network is back"));
        assert_eq!(h.update_with(Rejoining, &rejoin), Settling);
        assert_eq!(h.update_with(Rejoining, &rejoin), Settling);
        assert_eq!(h.update_with(Rejoining, &rejoin), Rejoining);
    }

    /// A view saying the peer is unreachable on every channel gives way at
    /// once when the peer answers on replication again (a fenced or rebooted
    /// peer coming back), to `Settling` until the next view has held.
    /// Replays a clean reboot of the slave, seen from the master.
    #[test]
    fn peer_gone_and_blind_end_when_the_peer_answers_again() {
        use PartitionView::*;
        let mut h = ViewHysteresis::default();
        let gone = ViewInputs { p: false, settled: false, peer_alive: false, ..base() };
        for _ in 0..VIEW_HOLD_EVALUATIONS {
            h.update_with(classify_partition(&gone), &gone);
        }
        assert_eq!(h.update_with(classify_partition(&gone), &gone), PeerGone);
        // Back on replication, its management address not answering yet.
        let booting = ViewInputs { peer_alive: true, ..gone };
        assert_eq!(classify_partition(&booting), PeerClientIsolated);
        assert_eq!(h.update_with(classify_partition(&booting), &booting), Settling);
        // Its address answers (refused or XAPI up) while P re-matures.
        let answering = ViewInputs { last_probe_answered: true, ..booting };
        assert_eq!(h.update_with(classify_partition(&answering), &answering), Settling);
        // P matured: nothing to say once that has held.
        let healed = base();
        for _ in 0..VIEW_HOLD_EVALUATIONS - 1 {
            assert_eq!(h.update_with(classify_partition(&healed), &healed), Settling);
        }
        assert_eq!(h.update_with(classify_partition(&healed), &healed), None);
        // Blind, cut host whose fenced peer is back on replication.
        let blind = ViewInputs { own_clients: Some(false), ..gone };
        let mut h = ViewHysteresis { shown: Blind, candidate: Blind, count: 0 };
        let back = ViewInputs { peer_alive: true, ..blind };
        assert_eq!(classify_partition(&back), ClientIsolated);
        assert_eq!(h.update_with(classify_partition(&back), &back), Settling);
        // A parked host whose peer went dark, then came back.
        let mut h = ViewHysteresis { shown: ParkedDark, candidate: ParkedDark, count: 0 };
        let parked = ViewInputs { xhad_online: false, park_confirmed: true, ..back };
        assert_eq!(h.update_with(classify_partition(&parked), &parked), Settling);
    }

    /// Once a failed join was seen, the park holds through XAPI's join cycle
    /// (an xhad restart with unreadable reads, a short disarmed read) until
    /// the host joins, P holds, or it reads disarmed for good.
    #[test]
    fn a_confirmed_park_holds_through_the_join_cycle() {
        let mut m = ParkMemory::default();
        // Before any failed join: not parked (the view reads Joining).
        assert!(!m.observe(false, false, false, Some(true)));
        assert!(m.observe(true, false, false, Some(true)));
        // The join cycle: no new failure line, armed unreadable, xhad down.
        for _ in 0..20 {
            assert!(m.observe(false, false, false, Option::None));
            assert!(m.observe(false, false, false, Some(true)));
        }
        // Short disarmed reads do not end it.
        for _ in 0..VIEW_HOLD_EVALUATIONS - 1 {
            assert!(m.observe(false, false, false, Some(false)));
        }
        assert!(m.observe(false, false, false, Some(true)));
        // Released (disarmed for good): ends it.
        for _ in 0..VIEW_HOLD_EVALUATIONS - 1 {
            m.observe(false, false, false, Some(false));
        }
        assert!(!m.observe(false, false, false, Some(false)));
        // P back ends it at once; joined (Online) once that holds.
        let mut m = ParkMemory::remembered();
        assert!(!m.observe(false, false, true, Some(true)));
        assert!(!m.confirmed());
        let mut m = ParkMemory::remembered();
        // One Online sample of XAPI's restarted daemon in its startup loop
        // does not end the park.
        assert!(m.observe(false, true, false, Some(true)));
        assert!(m.observe(false, false, false, Some(true)));
        for _ in 0..VIEW_HOLD_EVALUATIONS - 1 {
            assert!(m.observe(false, true, false, Some(true)));
        }
        assert!(!m.observe(false, true, false, Some(true)), "joined for good");
        assert!(!m.confirmed());
        // The view keeps the park while armed reads unreadable.
        use PartitionView::*;
        let parked = ViewInputs {
            p: false,
            settled: false,
            xhad_online: false,
            park_confirmed: true,
            local_armed: Option::None,
            ..base()
        };
        assert_eq!(classify_partition(&parked), ParkedMemberPeerCut);
    }

    /// A gray management link: a host held in XAPI's HA startup loop whose
    /// latest probe of the peer answers every other evaluation. The display
    /// shows Settling most of the time, but the advice to disable HA stays
    /// held at every evaluation, from the park memory or, before the first
    /// failed join, from the startup-loop condition (Joining). The survivor
    /// on the other side holds it too while the peer is out of its liveset.
    #[test]
    fn a_gray_link_keeps_the_ha_disable_advice_held_on_a_parked_host() {
        use PartitionView::*;
        let advice = "on the master run 'xe pool-ha-disable'";
        for (confirmed, shown_before) in [(true, ParkedMemberPeerCut), (false, Joining)] {
            let mut memory =
                if confirmed { ParkMemory::remembered() } else { ParkMemory::default() };
            let mut h = ViewHysteresis { shown: shown_before, candidate: shown_before, count: 0 };
            let mut settling_shown = 0;
            for n in 0..30 {
                let answered = n % 2 == 0;
                let park_confirmed = memory.observe(false, false, false, Some(true));
                let i = ViewInputs {
                    p: false,
                    settled: false,
                    last_probe_answered: answered,
                    xhad_online: false,
                    park_confirmed,
                    ..base()
                };
                let shown = h.update_with(classify_partition(&i), &i);
                settling_shown += usize::from(shown == Settling);
                let held = shown.parked() || holds_ha_disable_advice(&i);
                assert!(held, "evaluation {n}: shown {shown:?}");
                assert_eq!(route_alert(held, true, advice), AlertRoute::Hold);
            }
            assert!(settling_shown > 20, "the display mostly reads Settling");
        }
        // The survivor: it holds the master lock, the peer is out of its
        // liveset, and the peer's address answers now and then.
        let survivor = ViewInputs {
            p: false,
            settled: false,
            last_probe_answered: true,
            peer_live_in_liveset: Some(false),
            holds_master_lock: true,
            ..base()
        };
        assert_eq!(classify_partition(&survivor), Settling);
        assert!(holds_ha_disable_advice(&survivor));
        // Healed: P holds, nothing parked, the advice goes out (prefixed).
        assert!(!holds_ha_disable_advice(&base()));
    }

    /// A host disarmed under the armed pool shows it from local evidence even
    /// with its xhad stopped (its own XAPI refuses logins with the
    /// armed-mismatch code), and the master shows the peer's state from the
    /// same answer read over the peer's management address.
    #[test]
    fn a_disarmed_host_is_shown_on_both_sides_without_liveset_membership() {
        use PartitionView::*;
        let wedged = ViewInputs {
            local_armed: Some(false),
            xhad_online: false,
            pool_ha: Option::None,
            peer_live_in_liveset: Option::None,
            own_xapi_armed_mismatch: true,
            ..base()
        };
        assert_eq!(classify_partition(&wedged), DisarmedMember);
        assert!(!verdict(DisarmedMember).contains("is in the HA liveset"));
        // Without that answer (pool HA off, xhad stopped): nothing.
        assert_eq!(
            classify_partition(&ViewInputs { own_xapi_armed_mismatch: false, ..wedged }),
            None
        );
        // The master: healthy otherwise, the peer answers as disarmed.
        let master = ViewInputs { peer_disarmed: true, conf_master: true, ..base() };
        assert_eq!(classify_partition(&master), PeerDisarmed);
        assert!(degrades_health(PeerDisarmed.name()));
        // Neither text promises an HA cycle without its conditions.
        for v in [PeerDisarmed, DisarmedMember] {
            assert!(verdict(v).contains("only if its conditions hold"), "{v:?}");
        }
        assert!(!PeerDisarmed.parked());
        // Only while P holds (the answer is read over the management address).
        let cut = ViewInputs { p: false, settled: false, ..master };
        assert_ne!(classify_partition(&cut), PeerDisarmed);
    }

    /// The arbitrated shapes: a master that stood its own storage down, and
    /// its survivor; both hosts during a whole-switch hold. None of them says
    /// XHA restarted a host or decides by host order.
    #[test]
    fn stand_down_and_whole_switch_views_never_claim_an_xha_restart() {
        use PartitionView::*;
        // The stood-down master: own clients silent, P false, disarmed by its
        // own stand-down, out of the liveset.
        let stood = ViewInputs {
            own_clients: Some(false),
            p: false,
            settled: false,
            local_armed: Some(false),
            xhad_online: false,
            conf_master: true,
            stood_down: true,
            ..base()
        };
        assert_eq!(classify_partition(&stood), StoodDown);
        assert!(!verdict(StoodDown).contains("XHA restarted this host"));
        // The survivor: the peer left its liveset on a link that held, and is
        // Secondary on it.
        let survivor = ViewInputs {
            p: false,
            settled: false,
            peer_live_in_liveset: Some(false),
            holds_master_lock: true,
            peer_left_on_held_link: true,
            ..base()
        };
        assert_eq!(classify_partition(&survivor), PeerStoodDown);
        assert!(!verdict(PeerStoodDown).contains("restarted by XHA and"));
        // A fenced peer that rebooted and reconnected as Secondary (parked):
        // the link dropped since it left, so it is not shown as stood down.
        let fenced = ViewInputs { peer_left_on_held_link: false, ..survivor };
        assert_eq!(classify_partition(&fenced), PeerParked);
        assert!(verdict(PeerParked).contains("restarted by XHA"));
        // Whole-switch hold, both hosts: own clients silent, P false, peer
        // alive on the direct link, disarmed by the hold.
        let hold = ViewInputs {
            own_clients: Some(false),
            p: false,
            settled: false,
            local_armed: Some(false),
            xhad_online: false,
            whole_switch_hold: true,
            ..base()
        };
        assert_eq!(classify_partition(&hold), WholeSwitchHold);
        assert!(!verdict(WholeSwitchHold).contains("host order"));
        assert_ne!(classify_partition(&ViewInputs { whole_switch_hold: false, ..hold }), WholeSwitchHold);
    }

    #[test]
    fn stand_down_link_tells_a_stood_down_peer_from_a_fenced_one() {
        // Stood down: leaves the liveset while the link stays Connected.
        let mut l = StandDownLink::default();
        assert!(!l.observe(true, Some(true)));
        assert!(l.observe(true, Some(false)));
        assert!(l.observe(true, Some(false)));
        // ...until the link drops; reconnecting later does not restore it.
        assert!(!l.observe(false, Some(false)));
        assert!(!l.observe(true, Some(false)));
        // Fenced: the link drops (reboot), then the liveset drops it, then it
        // reconnects as Secondary (parked).
        let mut l = StandDownLink::default();
        l.observe(true, Some(true));
        assert!(!l.observe(false, Some(true)));
        assert!(!l.observe(false, Some(false)));
        assert!(!l.observe(true, Some(false)));
        // Fenced with the liveset dropping it first and the link a sample later.
        let mut l = StandDownLink::default();
        l.observe(true, Some(true));
        l.observe(true, Some(false));
        assert!(!l.observe(false, Some(false)));
        assert!(!l.observe(true, Some(false)));
        // Already out of the liveset when first seen: never shown as stood down.
        let mut l = StandDownLink::default();
        assert!(!l.observe(true, Some(false)));
        // Back in the liveset clears it.
        let mut l = StandDownLink::default();
        l.observe(true, Some(true));
        l.observe(true, Some(false));
        assert!(!l.observe(true, Some(true)));
    }

    /// A parked host whose xhad briefly reads Online flips for one or two
    /// evaluations; the shown view holds.
    #[test]
    fn a_view_must_hold_three_evaluations_to_replace_another() {
        use PartitionView::*;
        let mut h = ViewHysteresis::default();
        for _ in 0..VIEW_HOLD_EVALUATIONS - 1 {
            assert_eq!(h.update(ParkedMemberPeerCut), None);
        }
        assert_eq!(h.update(ParkedMemberPeerCut), ParkedMemberPeerCut);
        // A flap of one or two evaluations never shows.
        assert_eq!(h.update(PeerParkedPending), ParkedMemberPeerCut);
        assert_eq!(h.update(PeerParkedPending), ParkedMemberPeerCut);
        assert_eq!(h.update(ParkedMemberPeerCut), ParkedMemberPeerCut);
        // A change that holds replaces it.
        for _ in 0..VIEW_HOLD_EVALUATIONS - 1 {
            assert_eq!(h.update(None), ParkedMemberPeerCut);
        }
        assert_eq!(h.update(None), None);
    }

    #[test]
    fn every_view_has_a_verdict_and_parked_views_suppress_ha_disable_advice() {
        use PartitionView::*;
        for v in [
            ClientIsolated, PeerClientIsolated, GatewayOutage, ParkedExMaster, ParkedMember,
            ParkedDark, PeerParked, Rejoining, PeerRejoining, PeerGone, SecondMaster,
            DisarmedMember, ParkedExMasterPeerCut, ParkedMemberPeerCut, IsolatedSurvivor,
            PeerParkedPending, Blind, WholeSwitchHold, StoodDown, PeerStoodDown, Settling,
            Joining, PeerDisarmed,
        ] {
            assert!(!verdict(v).is_empty(), "{v:?}");
            assert_eq!(PartitionView::from_u8(v as u8), v);
        }
        // The process-wide view is left alone here: other tests send alerts.
        assert_eq!(PartitionView::from_u8(0), None);
        let advice = "Re-align it: on the pool master run 'xe pool-ha-disable', then enable.";
        // From the inputs of each held shape, as the view task publishes the
        // hold: a parked member, a parked ex-master, a parked host whose peer
        // is dark, the survivor holding the lock for a parked peer, a host
        // joining before its first failed join.
        let cut = ViewInputs { p: false, settled: false, ..base() };
        let parked = ViewInputs { xhad_online: false, park_confirmed: true, ..cut };
        let shapes = [
            (parked, ParkedMemberPeerCut),
            (ViewInputs { conf_master: true, own_clients: Some(false), ..parked }, ParkedExMaster),
            (ViewInputs { peer_alive: false, ..parked }, ParkedDark),
            (
                ViewInputs { peer_live_in_liveset: Some(false), holds_master_lock: true, ..cut },
                PeerParked,
            ),
            (ViewInputs { xhad_online: false, ..cut }, Joining),
        ];
        for (i, expect) in shapes {
            assert_eq!(classify_partition(&i), expect);
            let held = holds_ha_disable_advice(&i);
            assert_eq!(route_alert(held, true, advice), AlertRoute::Hold, "{expect:?}");
            // Not marked as advice: unchanged, whatever its text says.
            assert_eq!(route_alert(held, false, advice), AlertRoute::Send(advice.to_string()));
        }
        // Healthy, or a cut with no host held out of the pool: sent, prefixed
        // with when the advice is safe, and idempotent.
        for i in [base(), cut] {
            let AlertRoute::Send(out) = route_alert(holds_ha_disable_advice(&i), true, advice) else {
                panic!("nothing held: sent");
            };
            assert!(out.starts_with(HA_DISABLE_ADVICE_PREFIX));
            assert_eq!(route_alert(false, true, &out), AlertRoute::Send(out.clone()));
        }
    }

    #[test]
    fn status_values_round_trip() {
        let text = "view=PeerParked\nverdict=a b\nupdated_unix=5\n";
        assert_eq!(status_value(text, "view").as_deref(), Some("PeerParked"));
        assert_eq!(status_value(text, "verdict").as_deref(), Some("a b"));
        assert_eq!(status_value(text, "missing"), Option::None);
    }
}
