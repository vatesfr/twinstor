# Recovery crash boundary

When a split-brain is resolved, the losing host discards its copy and
resyncs from the winner. Before it does, TwinStor stops every guest on the
losing host that uses the SR and confirms that each one is gone. The guests
are then started again on the winner's copy. This document explains why,
describes the sequence the daemon runs (`src/loser_boundary.rs` and the
loser path in `src/state_machine.rs`), and lists what it does not cover.

Which side keeps its data is a separate decision, described in
[ARCHITECTURE.md](ARCHITECTURE.md#online-health-monitoring-check_health-check_paths)
and [BEHAVIOR.md](BEHAVIOR.md).

## The rule

**A guest must not keep running, with its kernel and application state,
while the disk under it switches to a different history.**

In testing, a guest on the losing host that stayed running through the
resolution kept its processes, PIDs and boot id. It lost the acknowledged
writes of the divergence window, as any fork does, and also ended up with
foreign content inside its files: regions of its append log held SQLite
b-tree pages, because its cached extent map pointed at blocks the winning
copy had allocated to another file. There was no I/O error and `fsck` was
clean; only content checks found it (8 and 62 such regions in two runs).
The guest on the winning host was clean.

Two things that look like a boundary are not one:

- **Pausing and resuming** a guest keeps its memory.
- **Withdrawing the losing target** does not stop the guest: multipath
  moves it to the peer's target with its page cache and file-system state
  intact.

So the loser must crash every consumer of its history before that history
is withdrawn or overwritten, and must never let one resume onto the
winner's history with old memory.

## Who can hold state from the losing history

Call the losing host L and the winner W.

| Consumer | How it reached L's copy | What TwinStor does |
|---|---|---|
| Guests running on L | Their multipath prefers L's own portal | Stopped and confirmed gone by the boundary on L, then restarted on W |
| A guest being started on L during the boundary (an HA restart, a migration) | Its disks attach while the boundary runs | Found by the disk-handle sweep (a live datapath with no block backend yet), stopped like the others, then the sweep runs again |
| A guest on L that existed before the withdrawal and is found running right after the discard (rebuilt by xenopsd or restarted by pool HA) | It may have run across the discard | Stopped, marked `twinstor-disks-suspect`, left halted with an alert; never restarted by TwinStor |
| Guests on W whose active path was L's portal | W's local path was down while L's was up | Fate-shared: prevented, since a disconnected Primary fences the path to the peer's portal. Otherwise: an alert from W naming its VMs, no action (L cannot crash them) |
| A guest on W when W won a resolution decided at bring-up | W was not serving, so its guests reached the SR through L's portal | The same boundary runs on W before L's discard; they are restarted on W once the pair has converged |
| dom0 consumers on L: XHA statefile, SR LVM metadata | Through L's path, like a guest | Not stopped. XHA and LVM re-read their metadata |
| dom0 disk operations on L (VM copy, import, export, backup) | Their tapdisk holds a disk open | Not stopped; see [Limits](#limits) |

The pool database is not the authority for any of these: during these
incidents it is forwarded to a pool master that may be cut off or forked.
The stop decision rests on local reads (Xen, xenstore, the local storage
layer, configfs).

## When the boundary runs

The resolver tells the boundary what it knows about the divergence:

| Evidence | Boundary |
|---|---|
| The DRBD generation lineage shows a true fork (both copies advanced) | Required |
| No lineage could be read | Required (divergence cannot be excluded) |
| This host's fence-peer handler yielded and its I/O stayed frozen | Required: its guests may hold writes frozen in flight, which the discard path's resume would complete and acknowledge just before discarding them |
| The lineage shows no divergence (this host is behind, or the generations are equal) | Not required: nothing its guests saw is rolled back |
| The local copy was already invalid (an interrupted resync) | Not required: nobody could read a diverged history from it |

Even when required, the boundary is skipped if this host's target is
positively not exported (a host resolving at bring-up that never served).
An unreadable export state counts as exported.

## The sequence on the losing host

The whole boundary runs in one call and normally takes seconds. Speed
matters: the split lasts until the loser discards, and with pool HA armed
the XHA statefile lives on the replicated SR, so each host stops seeing the
other's statefile updates for that whole time. A slow boundary can turn a
short cut into an XHA fence.

1. **Record.** The guest list (and, per stop round, the attempt count) is
   written to `/var/lib/twinstor/loser-boundary` before anything is
   stopped. This record is the restart obligation: a daemon killed mid-way
   resumes with it. The record never authorizes a stop by itself; every
   attempt re-scans the host.
2. **Scan, with XAPI as advisory.** In parallel:
   - the local scan: `xl` and the block backends in xenstore name every
     guest domain with a disk on the SR. This is the authority, and it also
     catches a domain XAPI does not attribute to this host (an incoming
     migration, a stale `resident-on`);
   - XAPI's list of this host's guests (names, restart obligation), bounded
     at 5 s;
   - a host disable, bounded at 5 s and best-effort: with pool HA armed and
     a failures-to-tolerate of 1, XAPI refuses to disable either host of a
     two-host pool.

   A guest XAPI lists with a domain here must also appear in the local scan;
   otherwise the scan does not understand this host and an empty answer
   could not be trusted, so the boundary holds.
3. **Stop.** All guest domains on the SR are powered off at the hypervisor
   at once, so xenopsd tears each down as a guest poweroff rather than a
   crash it would rebuild in place. XAPI's own hard stop goes out right
   after (or after 500 ms if the poweroff call is slow), for guests both
   views agree are here, so that XAPI records the stop as intended and pool
   HA leaves the restart to TwinStor. The host is re-scanned; any domain
   still running gets up to 2 rounds of `xl destroy`, run concurrently.
   xenopsd is then given up to 8 s to collect the powered-off domains; only
   those it leaves are destroyed directly.
4. **Yielded copy.** On a host whose fence-peer yielded, the copy's I/O is
   suspended, and the stopped guests' frozen writes keep their disk handles
   (and xenopsd's teardown) blocked. The boundary lifts the suspend inside a
   strict bracket, only while nothing on the host can acknowledge a write on
   the copy (no guest domain on the SR runs, none is being built with an SR
   disk), and suspends it again before returning, whatever the outcome.
5. **Release disk handles.** A domain destroyed outside xenopsd leaves its
   datapaths "activated" in XAPI's storage layer and its tapdisks holding the
   VHDs open with metadata cached from the losing history. They are released
   before the discard, so the cached footer and block map are written to
   the doomed copy and a later start on this host cannot reuse a stale
   attachment: first through the local storage layer (`DP.destroy`, which
   does the normal SM detach and deactivate), then `tap-ctl destroy` for a
   tapdisk still open. Nothing is closed unless this host's active storage
   path is its own portal. The sweep is bounded at 20 s. A tapdisk still
   open holds the boundary; a datapath that could not be released is
   recorded and re-checked, and no guest using it is started on this host
   until it is gone.
6. **Stopped.** The boundary is established only on an empty scan
   (`loser_crash_boundary step="stopped"`).

The discard then proceeds, in this order (steps 7 and 8 only when the
boundary ran):

7. Release the fate-shared fence on the path to the peer's portal (the
   loser's own target is about to go down, and the peer's portal becomes its
   only path to the SR, the XHA statefile included).
8. **Confirm.** The local scan must still find no guest domain on the SR.
   If one is found (most likely a guest rebuilt in place after a direct
   destroy), the boundary is established again at once.
9. Log out this host's own loopback session, so frozen writes can no longer
   complete through it.
10. Resume I/O (a backstore delete flushes and would block forever on
    suspended I/O).
11. Stop the target, bounded at 30 s. A teardown that wedges forces a
    reboot of this host rather than leaving the pair StandAlone. A backstore
    that is still exported defers the discard to the next round.
12. Demote to Secondary. A failed demote also defers the discard: a Primary
    must never be left offering `--discard-my-data`.
13. `connect --discard-my-data`, and the resync from the winner starts. If
    nobody takes the offer yet, a yielded loser stays down (Secondary,
    target off) with the offer parked, recorded on disk so a daemon restart
    re-adopts it instead of promoting the discarded copy.

## After the discard

- **Guests running across the discard.** Right after the discard, the host
  is scanned again. A guest domain on the SR that existed when the copy was
  withdrawn (its domain id is at or below the highest one present then) may
  have run across the discard. It is stopped (through XAPI first, so pool HA
  does not restart it), removed from the owed restarts, marked
  `other-config:twinstor-disks-suspect` and left halted, with an alert that
  names its disks and the checks to run before starting it by hand. The mark
  goes once it has run again. TwinStor never starts it. A domain created
  after the withdrawal reaches the SR only through the winner's portal and
  is left alone.
- **Stale XAPI records.** A local destroy can leave XAPI showing a guest as
  `running` on this host. These records are reconciled before the restarts;
  if needed, this host's XAPI agent is restarted once per episode (xapi
  only, not the whole toolstack).
- **Owed restarts.** Each stopped guest is started again, one per step,
  with the record updated after each:
  - on the winner, once the discard has started;
  - on the winner during a hold, when the loser verdict came from the
    lineage (the winner's copy is authoritative either way);
  - on this host, if the episode ended without this host discarding (it was
    flipped to winner, or converged another way), since both copies are the
    same again;
  - on this host, after it has rejoined, for a guest the winner cannot
    start because this host still holds its disk: a fresh boot here reads
    the winner's history.

  A failed start is retried; after 10 failures for one guest an alert goes
  out and the retries continue at a slower pace. An owed restart is never
  dropped silently. If the record cannot be read, owed restarts are not run
  automatically, and an alert asks for the halted VMs to be started by hand.
- **Host enable.** A host the boundary disabled is enabled again once the
  pair has converged (or the episode converged and this copy serves alone)
  and no unreleased datapath is left. A host already disabled by an operator
  is never enabled by the boundary.

## When the boundary cannot be established

Any step that cannot be completed or confirmed (the record cannot be
written, the local scan cannot be read or disagrees with XAPI, a guest does
not stop after its rounds, a tapdisk stays open) is a **hold**:

- nothing is discarded and both copies are kept, the same posture as an
  undecidable winner;
- one alert per episode names the failed step and the guests already
  stopped;
- the boundary is retried every 60 s;
- if the loser verdict came from the lineage, guests already stopped are
  restarted on the winner during the hold; if divergence could not be read,
  they stay down and the alert says so.

`twinstor status` shows the hold and what to do: stop the listed VMs on
this host (the boundary then completes by itself), or stop the daemon and
run `twinstor discard-and-resync` on this host. On a winner holding at
bring-up, the guidance never sends the operator to discard: that host holds
the copy that is kept.

Nothing in the boundary relies on a timing assumption: each bound above
only decides how long to wait before holding.

## Daemon restarts

The record in `/var/lib/twinstor/loser-boundary` survives a daemon restart
and a reboot. Its episode stays open until the pair has positively
converged (Connected, both copies UpToDate); a vanished split-brain marker
(it lives in `/run`, which a reboot clears) does not close it. The record
is parsed strictly: a torn or foreign file is not half-trusted.

## Limits

- **Dom0 disk operations.** Disks dom0 itself holds open on the SR when
  this host loses (a VM copy, import, restore or export running in dom0 at
  the split) are not stopped. Their tapdisks count as live and are not
  closed, so one still writing after the discard writes with the block map
  of the discarded copy into the copy that is kept. Such an operation must
  run on the losing host at the moment of the split, and earlier releases
  did not protect against it either. Avoid VM copies and imports during
  network maintenance.
- **A guest pool HA restarts on the losing host before its copy is
  withdrawn.** When the losing host is the pool master, its XAPI is often
  too slow right after the cut to list this host's guests within the 5 s
  bound. They are then stopped at the hypervisor without XAPI being told
  the stop is intended, and XAPI later reads a guest-internal halt. With the
  pool's `ha-reboot-vm-on-internal-shutdown` on (XAPI's default), pool HA
  restarts the guest wherever it plans, sometimes on this host. The guest is
  caught running here right after the discard, stopped, marked
  `twinstor-disks-suspect` and left halted with an alert: check its disks,
  then start it by hand. In testing this happened in 4 of about 40 cuts in
  which the master lost while holding guests, with data intact every time.
  Fate-shared freeze mode turns that pool setting off (see
  [FATE_SHARED_FREEZE_MODE.md](FATE_SHARED_FREEZE_MODE.md)); XAPI then drops
  the guest's HA protection with the halt, and the boundary's own restart
  places it on the winner: 0 such cases in 17 cuts. Arbitrated pools keep
  the setting on.
- **A tapdisk that does not answer.** `tap-ctl list` queries every tapdisk
  on the host in turn, each bounded at 10 s by `tap-ctl` itself, while the
  boundary bounds each listing at 5 s. One tapdisk that does not answer (a
  hung NFS SR, or one stuck on this copy's frozen I/O) holds every boundary
  until it answers.
- **The writes of the divergence window** are lost on the losing side by
  definition of a fork. The boundary only removes the additional damage of
  a live history switch.
- **Guests on the winner that read the losing copy** through the loser's
  portal outside fate-shared mode are reported, not stopped.

## In the logs

Every step logs `heal_event="loser_crash_boundary"` with a `step` (`start`,
`stop`, `poweroff`, `tapdisks`, `datapaths`, `stopped`, `confirm`, `hold`,
`rebuilt_after_discard`, `restart`, `host_enable`, ...), naming guests by
name and uuid. The discard itself is `loser_discard`. See
[LOG_REFERENCE.md](LOG_REFERENCE.md).
