//! Storage readiness assessment: the per-host guard on VM placement.
//!
//! This is the per-host, runtime judgment of whether this node's local DRBD
//! replica is safe to serve. It is a function over a [`DrbdStatus`] snapshot
//! with no I/O of its own, so it can be reviewed and unit-tested in isolation.
//!
//! The verdict **gates iSCSI target startup**
//! (`state_machine::try_go_online` refuses to start the LIO target while
//! not ready, so a not-ready node is not a usable multipath path and the
//! existing scheduler/multipath logic avoids it, with no SM/XAPI hook
//! required) and is reported by `twinstor status`.
//!
//! ## Lone-survivor escape hatch
//!
//! The verdict keys on *local* serving ability (`Primary` + `UpToDate`), never
//! on peer connectivity. A node whose peer is dead must still be able to
//! serve; gating on peer `Connected` would strand the survivor when it is
//! needed most. So a healthy but alone node is [`Readiness::ServingWithoutRedundancy`],
//! not [`Readiness::NotReady`]. That state is only safe if the peer has been
//! fenced (XHA's job), which this predicate cannot confirm; it reports the
//! condition rather than blocking on it.
//!
//! ## Scope
//!
//! This covers the local replica (`Primary`/`UpToDate`), the peer
//! (`Connected`/`UpToDate`, or lone survivor), and toolstack convergence (host
//! `enabled=true`). DRBD data safety dominates: if the local replica is not
//! safe to serve, nothing else matters. Toolstack convergence is layered on
//! top because a VM placed on a host whose DRBD is already `UpToDate` but
//! whose toolstack is still initialising can crash.
//!
//! The multipath map and iSCSI sessions are the consumer-side axis
//! (`multipath::degradation`). They downgrade only a fully-`Ready` verdict to
//! [`Readiness::DegradedPaths`] and **never** make a node `NotReady`: the
//! target must stay up regardless (it is the peer's second path, and the
//! consumer stack cannot even exist before the target it rides on; gating
//! would deadlock bootstrap). The iSCSI gate therefore passes `None` for this
//! signal; status reporting and the daemon's path watchdog provide it.

use crate::drbd::{ConnectionState, DiskState, DrbdStatus, Role};

/// Whether this node's local DRBD replica is safe to serve, and whether it is
/// redundant. Ordered loosely best-to-worst.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    /// Local replica is serving (`Primary` + `UpToDate`) and the peer is
    /// `Connected` + `UpToDate`: fully redundant. Safe to place/start VMs.
    Ready,
    /// Data is fully redundant, but this node's *consumer-side* path stack is
    /// impaired (no multipath map, a missing path, or the remote path active).
    /// The node keeps serving; the risk is that a peer failure freezes local
    /// I/O (VMs and the XHA statefile) for the whole path-failure-detection
    /// window. Reported and self-healed, never enforced.
    DegradedPaths { reason: String },
    /// Local replica can serve (`Primary` + `UpToDate`) but the peer is not a
    /// healthy redundant copy. The node is serving **alone**. Only safe if the
    /// peer has actually been fenced (by XHA), which this predicate
    /// cannot confirm. Reported, not enforced.
    ServingWithoutRedundancy { reason: String },
    /// Local replica is not safe to serve: not `Primary`, not `UpToDate`, disk
    /// detached, or DRBD status unavailable. VMs must not be placed here.
    NotReady { reason: String },
}

impl Readiness {
    /// Short, stable label for status output and JSON.
    pub fn label(&self) -> &'static str {
        match self {
            Readiness::Ready => "ready",
            Readiness::DegradedPaths { .. } => "degraded-paths",
            Readiness::ServingWithoutRedundancy { .. } => "serving-without-redundancy",
            Readiness::NotReady { .. } => "not-ready",
        }
    }

    /// Human-readable explanation, empty for the fully-`Ready` case.
    pub fn reason(&self) -> &str {
        match self {
            Readiness::Ready => "",
            Readiness::DegradedPaths { reason }
            | Readiness::ServingWithoutRedundancy { reason }
            | Readiness::NotReady { reason } => reason,
        }
    }

    /// True when the local replica is safe to serve at all (fully redundant or
    /// serving alone). This is the predicate the iSCSI gate keys on; the
    /// lone-survivor case is included on purpose (see module docs).
    pub fn can_serve(&self) -> bool {
        !matches!(self, Readiness::NotReady { .. })
    }
}

/// Assess storage readiness.
///
/// `drbd` is the DRBD status snapshot, `None` when `drbdadm status` could not be
/// read (treated as [`Readiness::NotReady`], fail closed). `host_enabled` is
/// XAPI's `host.enabled` for this host: `Some(true)` toolstack converged,
/// `Some(false)` still initialising/disabled, `None` could not be determined.
/// `path_degraded` is the consumer-side verdict from `multipath::degradation`:
/// `Some(reason)` when the path stack is impaired, `None` when healthy or not
/// probed (the iSCSI gate always passes `None`, see module docs).
///
/// DRBD data-safety dominates: an unsafe replica is [`Readiness::NotReady`]
/// regardless of `host_enabled`. On a safe-to-serve replica, `Some(false)`
/// downgrades the verdict to [`Readiness::NotReady`] (toolstack not converged),
/// while `None` falls back to the DRBD verdict: we never block on a signal we
/// could not read, so a momentarily unreachable `xe` at boot cannot strand
/// storage permanently. Path degradation only downgrades a fully-`Ready`
/// verdict: any more severe condition already carries the more urgent reason.
pub fn assess(
    drbd: Option<&DrbdStatus>,
    host_enabled: Option<bool>,
    path_degraded: Option<&str>,
) -> Readiness {
    let drbd_verdict = assess_drbd(drbd);
    if matches!(drbd_verdict, Readiness::NotReady { .. }) {
        return drbd_verdict;
    }
    if host_enabled == Some(false) {
        return Readiness::NotReady {
            reason: "toolstack not converged (XAPI host enabled=false)".to_string(),
        };
    }
    if matches!(drbd_verdict, Readiness::Ready)
        && let Some(reason) = path_degraded
    {
        return Readiness::DegradedPaths {
            reason: reason.to_string(),
        };
    }
    drbd_verdict
}

/// The DRBD-only serving verdict (local replica and peer). [`assess`] layers
/// host convergence on top of it.
fn assess_drbd(status: Option<&DrbdStatus>) -> Readiness {
    let status = match status {
        Some(s) => s,
        None => {
            return Readiness::NotReady {
                reason: "DRBD status unavailable".to_string(),
            };
        }
    };

    // The local replica must be Primary + UpToDate to serve, with one
    // exception: a Diskless Primary with a healthy peer still serves correctly
    // (DRBD forwards all I/O to the peer's disk), so tearing the target down
    // would force a needless path failover. It is serving, just without
    // redundancy.
    match status.disk {
        DiskState::UpToDate => {}
        DiskState::Diskless => {
            if status.role == Role::Primary
                && status.connection == ConnectionState::Connected
                && status.peer_disk == Some(DiskState::UpToDate)
            {
                return Readiness::ServingWithoutRedundancy {
                    reason: "local disk failed (Diskless); serving through the peer".to_string(),
                };
            }
            return Readiness::NotReady {
                reason: "local disk Diskless and no healthy peer to serve through".to_string(),
            };
        }
        other => {
            return Readiness::NotReady {
                reason: format!("local disk {other:?}, not UpToDate (still converging/syncing)"),
            };
        }
    }
    if status.role != Role::Primary {
        return Readiness::NotReady {
            reason: format!("local role {:?}, not Primary", status.role),
        };
    }

    // Peer Connected + UpToDate: fully redundant.
    let peer_connected = status.connection == ConnectionState::Connected;
    let peer_uptodate = status.peer_disk == Some(DiskState::UpToDate);
    if peer_connected && peer_uptodate {
        return Readiness::Ready;
    }

    // Local can serve, but the peer is not a healthy redundant copy. This is
    // lone-survivor territory: never block local serving on peer state, just
    // report that redundancy is gone. Connectivity is the more fundamental gap,
    // so it takes precedence in the message.
    let reason = if !peer_connected {
        format!("peer connection {:?} (not Connected)", status.connection)
    } else {
        format!(
            "peer disk {:?} (not UpToDate)",
            status.peer_disk.unwrap_or(DiskState::Unknown)
        )
    };
    Readiness::ServingWithoutRedundancy { reason }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(
        role: Role,
        disk: DiskState,
        connection: ConnectionState,
        peer_disk: Option<DiskState>,
    ) -> DrbdStatus {
        DrbdStatus {
            role,
            peer_role: peer_disk.map(|_| Role::Primary),
            connection,
            disk,
            peer_disk,
            suspended_user: false,
            suspended_fencing: false,
            suspended_any: false,
            replication: None,
            resync_done: None,
        }
    }

    // A fully-redundant, serving-ready DRBD snapshot.
    fn redundant() -> DrbdStatus {
        status(
            Role::Primary,
            DiskState::UpToDate,
            ConnectionState::Connected,
            Some(DiskState::UpToDate),
        )
    }

    #[test]
    fn fully_redundant_is_ready() {
        let s = redundant();
        assert_eq!(assess(Some(&s), Some(true), None), Readiness::Ready);
        assert!(assess(Some(&s), Some(true), None).can_serve());
    }

    #[test]
    fn missing_status_is_not_ready() {
        let r = assess(None, Some(true), None);
        assert!(matches!(r, Readiness::NotReady { .. }));
        assert!(!r.can_serve());
        assert_eq!(r.label(), "not-ready");
    }

    #[test]
    fn syncing_local_is_not_ready() {
        // Inconsistent disk = resync in progress; must not serve yet (the
        // post-reboot convergence window).
        let s = status(
            Role::Primary,
            DiskState::Inconsistent,
            ConnectionState::Connected,
            Some(DiskState::UpToDate),
        );
        let r = assess(Some(&s), Some(true), None);
        assert!(matches!(r, Readiness::NotReady { .. }));
        assert!(!r.can_serve());
    }

    #[test]
    fn diskless_with_healthy_peer_serves_without_redundancy() {
        // DRBD diskless mode: a Primary with no local disk forwards I/O to
        // the peer. It must keep serving (tearing the target down would
        // force a path failover for no benefit), but never count as Ready.
        let s = status(
            Role::Primary,
            DiskState::Diskless,
            ConnectionState::Connected,
            Some(DiskState::UpToDate),
        );
        let r = assess(Some(&s), Some(true), None);
        assert!(matches!(r, Readiness::ServingWithoutRedundancy { .. }));
        assert!(r.can_serve());
        assert!(r.reason().contains("serving through the peer"));
    }

    #[test]
    fn diskless_without_healthy_peer_is_not_ready() {
        // No local disk and no healthy peer copy: nothing can serve.
        let s = status(
            Role::Primary,
            DiskState::Diskless,
            ConnectionState::StandAlone,
            None,
        );
        assert!(matches!(
            assess(Some(&s), Some(true), None),
            Readiness::NotReady { .. }
        ));
        // Peer connected but itself not UpToDate: also nothing to serve from.
        let s = status(
            Role::Primary,
            DiskState::Diskless,
            ConnectionState::Connected,
            Some(DiskState::Inconsistent),
        );
        assert!(matches!(
            assess(Some(&s), Some(true), None),
            Readiness::NotReady { .. }
        ));
    }

    #[test]
    fn secondary_local_is_not_ready() {
        // UpToDate but still Secondary: not promoted, cannot serve the LUN.
        let s = status(
            Role::Secondary,
            DiskState::UpToDate,
            ConnectionState::Connected,
            Some(DiskState::UpToDate),
        );
        assert!(matches!(
            assess(Some(&s), Some(true), None),
            Readiness::NotReady { .. }
        ));
    }

    #[test]
    fn lone_survivor_serves_without_redundancy() {
        // Peer gone (StandAlone) but local Primary + UpToDate: must still be
        // allowed to serve. This is the escape hatch, not NotReady.
        let s = status(
            Role::Primary,
            DiskState::UpToDate,
            ConnectionState::StandAlone,
            None,
        );
        let r = assess(Some(&s), Some(true), None);
        assert!(matches!(r, Readiness::ServingWithoutRedundancy { .. }));
        assert!(r.can_serve());
        assert_eq!(r.label(), "serving-without-redundancy");
    }

    #[test]
    fn peer_connected_but_behind_serves_without_redundancy() {
        // Peer is back and connected but still resyncing (Inconsistent): we are
        // serving, but a peer failure now would lose redundancy.
        let s = status(
            Role::Primary,
            DiskState::UpToDate,
            ConnectionState::Connected,
            Some(DiskState::Inconsistent),
        );
        let r = assess(Some(&s), Some(true), None);
        assert!(matches!(r, Readiness::ServingWithoutRedundancy { .. }));
        assert!(r.reason().contains("not UpToDate"));
    }

    #[test]
    fn disconnected_message_takes_precedence() {
        let s = status(
            Role::Primary,
            DiskState::UpToDate,
            ConnectionState::Connecting,
            None,
        );
        let r = assess(Some(&s), Some(true), None);
        assert!(r.reason().contains("not Connected"));
    }

    #[test]
    fn disabled_host_is_not_ready_even_when_drbd_ready() {
        // DRBD UpToDate on both nodes, but the host's toolstack is still
        // initialising: host.enabled=false must hold off serving.
        let s = redundant();
        let r = assess(Some(&s), Some(false), None);
        assert!(matches!(r, Readiness::NotReady { .. }));
        assert!(!r.can_serve());
        assert!(r.reason().contains("toolstack"));
    }

    #[test]
    fn unknown_host_enabled_falls_back_to_drbd_verdict() {
        // xe unreachable (None): we do not block on a signal we cannot read.
        let s = redundant();
        assert_eq!(assess(Some(&s), None, None), Readiness::Ready);
        assert!(assess(Some(&s), None, None).can_serve());
    }

    #[test]
    fn path_degradation_downgrades_ready() {
        let s = redundant();
        let r = assess(Some(&s), Some(true), Some("no multipath map"));
        assert!(matches!(r, Readiness::DegradedPaths { .. }));
        // Still serving: path health is reported, never enforced.
        assert!(r.can_serve());
        assert_eq!(r.label(), "degraded-paths");
        assert!(r.reason().contains("multipath"));
    }

    #[test]
    fn path_degradation_does_not_mask_worse_verdicts() {
        // Lone survivor: redundancy loss is the more urgent condition.
        let s = status(
            Role::Primary,
            DiskState::UpToDate,
            ConnectionState::StandAlone,
            None,
        );
        let r = assess(Some(&s), Some(true), Some("only 1 iSCSI path"));
        assert!(matches!(r, Readiness::ServingWithoutRedundancy { .. }));
        // Not safe to serve: NotReady must survive untouched.
        let r = assess(None, Some(true), Some("only 1 iSCSI path"));
        assert!(matches!(r, Readiness::NotReady { .. }));
    }

    #[test]
    fn absent_path_signal_keeps_ready() {
        // None = healthy or not probed (e.g. the iSCSI gate): fail open.
        let s = redundant();
        assert_eq!(assess(Some(&s), Some(true), None), Readiness::Ready);
    }

    #[test]
    fn drbd_unsafe_dominates_host_enabled() {
        // Even with the toolstack fully up, an unsafe replica is NotReady, and
        // the reason is the DRBD one, not the toolstack one.
        let s = status(
            Role::Primary,
            DiskState::Inconsistent,
            ConnectionState::Connected,
            Some(DiskState::UpToDate),
        );
        let r = assess(Some(&s), Some(true), None);
        assert!(matches!(r, Readiness::NotReady { .. }));
        assert!(!r.reason().contains("toolstack"));
    }
}
