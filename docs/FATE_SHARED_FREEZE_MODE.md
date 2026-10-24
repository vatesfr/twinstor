# Fate-shared freeze mode

On a fate-shared pool, TwinStor freezes guest I/O on both hosts the moment
they lose each other, so the host that is cut off never acknowledges a
write its peer lacks. Once the hosts decide which copy is kept, the other
one resyncs, without rebooting the isolated host and without restarting
guests that do not need it.

Freeze mode is on by default on fate-shared pools, and engages only when
both hosts run a release that has it (0.7.0 or later). Arbitrated pools
never use it.

## The problem it solves

On a fate-shared pool (one network path carries replication, management
and storage traffic) DRBD runs `fencing resource-only`. When the path
breaks, I/O continues while the fence-peer handler checks the gateway
(2.5 to 9 s). The host that cannot reach the gateway then yields, but by
then it has acknowledged a few seconds of guest writes its peer lacks. When
guests ran on both sides, the split-brain resolution discards those writes
(about 4 s of them in testing).

## Engagement

- **Both hosts.** Each host advertises `twinstor-fs-freeze=on|off` in its
  host `other_config` (with its liveness stamp) and engages freeze mode only
  once its peer advertises `on` from a release that has it.
- **The verdict is taken on a healthy pair.** It is refreshed only while
  the pair is Connected and UpToDate, and kept in
  `/var/lib/twinstor/peer-fs-freeze`, so a partition, or a daemon restart
  during one, keeps the policy the pair went into it with.
- **Rolling upgrades.** During an upgrade from an older release the pair
  keeps the behaviour without freeze mode until both hosts run the new one.
- **Opt-out.** `touch /etc/twinstor/fate-shared-freeze-off` on either host
  turns freeze mode off for the pair (that host advertises `off`); remove
  the file to turn it back on. Each host republishes its own setting and
  reads the peer's once a minute, while the pair is healthy, so a change
  takes effect within about two minutes.

## Restarts, upgrades and rollbacks

- The freeze settings are live DRBD options (`fencing resource-and-stonith`,
  `connect-int 2`). The daemon re-applies them before its bring-up, so a
  daemon restart or an upgrade keeps them.
- The fence-peer handler reads `/run/twinstor/fs-freeze`, which the daemon
  keeps only while that policy is live. The file holds the daemon's PID,
  and the handler takes its freeze-only paths only while that process runs
  (the installed `twinstor` binary running `daemon`, also between an
  upgrade's install and its restart).
- A stopped daemon leaves the policy armed. Its host's handler then decides
  as without freeze mode, so a cut shorter than the handler can end with
  two writers, as it can without freeze mode, and an isolated host freezes
  until the daemon is back.
- When the daemon stops because a release without freeze mode was installed
  (a rollback), it restores the configured policy on a healthy pair, so the
  older release never runs settings it does not manage. Roll back with the
  bundle's install script and `twinstor upgrade-apply`, which waits for a
  healthy pair; the policy is restored only on one.

## The pool setting `ha-reboot-vm-on-internal-shutdown`

- While freeze mode is engaged on the pair, the pool master's daemon turns
  this XAPI pool setting off, once, and records the value it found in the
  pool's `other_config` (`twinstor-internal-shutdown-prior`). XAPI raises
  one alert when it changes.
- TwinStor never changes it again: an operator may turn it back on, and
  `twinstor doctor` then warns.
- When freeze mode disengages (opt-out, a peer without it) or on
  `twinstor uninstall`, a setting TwinStor turned off is put back and the
  record dropped. A host rolled back to a release without freeze mode puts
  it back as its daemon stops for the older release; when XAPI does not
  answer then, the daemon logs the two `xe` commands to run.
- The effect for guests: a protected VM shut down from inside the guest is
  no longer restarted by pool HA, the same as one shut down through XAPI
  (its HA protection is dropped with the halt, and `xe vm-start` gives it
  back). VMs on a host that fails are restarted as before.
- Why: see [Guests stopped on a losing pool master](#guests-stopped-on-a-losing-pool-master).
- Arbitrated pools keep XAPI's default, because their isolated-host
  stand-downs rely on pool HA restarting the guests they stop. `doctor`
  warns when the setting is off on an arbitrated pool.

## Mechanism

### 1. Freeze at the disconnect

DRBD fencing is `resource-and-stonith`, set live and re-checked against the
kernel on every pass (re-creating the peer object re-applies the resource
file's `resource-only`). I/O freezes on both hosts until each host's
handler decides. The host that reaches the gateway resumes, after a pause
of a few seconds. The other stays frozen, with nothing acknowledged that its
peer lacks.

Cuts shorter than the handler, and other edge cases:

- **The link comes back while the handler is still deciding.** The peer's
  DRBD listens, but DRBD sits StandAlone (the handshake aborted during
  fencing). Without freeze mode the lower hostname then keeps serving. In
  freeze mode the handler yields frozen instead, to a live peer daemon (its
  beacon on port 7900 answers; the daemon keeps `/run/twinstor/fs-freeze`
  for it). The peer's own run has usually survived already, and two writers
  make a fork that the lineage settles by rolling one side back: 151 to 153
  acknowledged appends lost on a 5 s cut with guests on both hosts, against
  63 without freeze mode.
- **A serving host at StandAlone with no freeze decision** reconnects at
  once rather than after the 30 s grace.
- **A gateway that answers only after the handler's first probe was lost**
  means this host's own network just came back. In freeze mode it yields
  frozen rather than survive alongside a peer that already did. A healthy
  host that merely lost one echo yields for nothing worse than the give-way
  below.
- **Both copies StandAlone.** The peer's DRBD port is closed although its
  daemon answers. The handler waits up to 6 s for the daemon's reconnect to
  open it, then yields to it (a closed port alone read as a gone peer, and
  both runs could survive). A peer whose daemon is silent costs one more
  1 s probe.

### 2. Who keeps the copy

The healthy host records whether a guest (or a live datapath) on the SR ran
on it during the split, sampled while its I/O is live, plus a last look
before deciding.

- **Guests ran there.** It keeps its copy and publishes `W` on the beacon.
  The frozen host stops its guests (their frozen writes were never
  acknowledged), discards and resyncs. No reboot.
- **No guest ran there.** It gives way. It stops its target, demotes, and
  only then publishes `R` (no other writer remains). It offers
  `--discard-my-data` only once the frozen peer answers `W` with its new
  generation (step 3), and re-offers it on every StandAlone pass (an offer
  is consumed by any completed handshake). Offered earlier, the discard
  could meet the frozen copy while it is merely behind: DRBD then reverses
  the resync onto the frozen Primary, drops the connection and leaves the
  giving copy Outdated. Once the discard lands, the giving host takes the
  Primary role and its target back.
- Later rounds (a split-brain declared after the peer's new generation, a
  daemon restart) offer at once.
- A give-way that never lands is dropped after 170 s and the host returns
  to bring-up, unless the frozen peer was ever seen taking it (`W`): that
  peer may serve on the only full copy.

### 3. The frozen host keeps its copy

- While no decision is visible, the frozen host stays StandAlone (a plain
  connect could meet the give-way offer too early). It connects only to a
  peer that yielded too (frozen with guests, `G`, or guest-free and
  latched, `Y`: neither wrote, so the copies match), and leaves a connect in
  progress when `R` appears.
- Once it has taken the give-way, its win record follows the giver's
  generation across later handshakes while the giver still gives way.
- On `R`, the frozen host starts a new data generation (`drbdadm
  new-current-uuid`). Without it, the frozen copy is merely behind (it wrote
  nothing) and DRBD refuses a Primary as sync target; with it, the handshake
  is a split-brain that the peer's offer decides.
- The frozen I/O resumes right then, before the connect, and the guests
  carry on with no restart. The giver is Secondary with its target down and
  latched behind this copy, so nothing is served from its copy again
  unprompted. Connecting with the I/O still frozen would send the requests
  held during the freeze into the giver's fresh resync, which dropped the
  link after the handshake had used up the discard offer (a second
  split-brain round, 10 s more freeze).
- A kept copy still frozen at the handshake (the resume failed) resumes
  once it is the sync source, checked twice in a row.

### 4. Budget

A frozen host never keeps its guests frozen longer than its budget, counted
from its yield (dated from the start of the fence-peer run that recorded
it), in any connection state.

- The budget is 110 s, below the multipath queue (30 checker intervals of
  5 s), so a guest never receives an I/O error and then continues.
- With pool HA on, it also ends 25 s before the XHA statefile watchdog
  (HA timeout + 15 s, counted from the disconnect, about 10 s before the
  yield): 100 s at the fate-shared default timeout of 120 s. The watchdog
  reboots a host whose statefile writes stall that long, even after its
  network is back.
- Past the budget, a host that took a give-way keeps its copy and resumes
  its I/O (the peer's copy may already be Outdated by the discard;
  discarding this one too would leave no UpToDate copy), and the discard
  lands on the next handshake. Any other frozen host stops its guests and
  steps down.
- A frozen host steps down earlier only when the peer answers without any
  claim for 15 s after a 60 s wait (no freeze mode there).

### 5. Quiet during the freeze

- No multipath self-heal while this copy's I/O is frozen: a reconfigure
  under the frozen map fell back to the default retry budget and failed
  queued guest I/O after 50 s.
- While the I/O is frozen, and for the first two minutes of a disconnect
  (on both hosts, so the healthy one gives way the moment the link returns)
  and of a freeze episode, the steps that call XAPI wait. Pool-master XAPI
  does not answer during a cut, and each call's timeout would delay the
  decision by seconds. Their alerts are sent afterwards.
- DRBD retries the connection every 2 s instead of 10 s.
- The frozen guests resume about 10 s after the network returns (measured:
  18 s of freeze for a 10 s cut, 40 s for a 30 s cut).

### Beacon claims

On port 7900, in a compatible extension of the beacon line: `G<gen>` frozen
with guests, `R<gen>` giving way, alongside the existing `Y` (yielded) and
`W` (won). Each is bound to the generation pair of the last DRBD handshake.
A host without freeze mode reads `G` and `R` as an unknown answer and never
acts on them.

## Guests stopped on a losing pool master

Freeze mode makes "the pool master loses while holding guests" a routine
case: any cut of the master while guests run on both hosts. The losing host
stops its guests at the hypervisor first and tells XAPI only when XAPI
answers in time (see [RECOVERY_CRASH_BOUNDARY.md](RECOVERY_CRASH_BOUNDARY.md)).
On the pool master it usually cannot: its XAPI is slow right after the cut,
and its HA redo log sits on the frozen SR. XAPI later reads each stop as the
guest's own shutdown.

- **With XAPI's default setting**, pool HA then restarts the guest wherever
  it plans, sometimes on the losing host itself before its copy is
  withdrawn. The crash boundary must catch it there after the discard, stop
  it again and leave it halted with an alert, because it ran across the
  discard. Data was intact every time in testing, but the guest stays down
  until started by hand.
- **With the pool setting off**, XAPI drops the guest's HA protection when
  it records the halt (before the power state, so no HA restart can race
  it), and the boundary's own restart starts it on the winner.

In testing (pool master losing while holding three of four guests): with
the setting on, guests were left halted in 3 of 3 cuts (4 of about 40
master-loser cuts overall); with it off, 17 of 17 cuts ended with every
guest running and protected again and no acknowledged write lost, although
the master's XAPI missed the listing bound in all 17.

## Limits

- **Cuts longer than the budget** end as without freeze mode: the isolated
  host's guests stop and restart on the healthy host. With pool HA on, a
  host still isolated at the HA timeout is rebooted by XHA (its isolation
  fence), with or without freeze mode; the reboot ends the frozen guests,
  never an I/O error they carry on from.
- **XHA statefile writes** of the giving-way host during the split are
  discarded with its copy. For cuts approaching the HA timeout, XHA may
  later fence a host even though storage has converged (also seen without
  freeze mode).
- **Cuts from about 60 s up to the budget, with guests on the isolated
  host.** A freeze past about 75 s outlasts dom0's iSCSI error handling (a
  30 s command timeout, an unanswered abort, a session reset): the
  initiator takes the local path offline, it stays offline after the I/O
  resumes, and XHA's statefile watchdog reboots that host. No acknowledged
  write is lost and the guests restart on the peer. Without freeze mode a
  75 s cut ended the same way (XHA's own fence about four minutes after the
  cut).
- **Only one host runs freeze mode.** A pair where only one host runs it
  never engages it. For a minute or two while a pair turns it on or off
  (each host reads the peer's setting once a minute, only while the pair is
  healthy), one host may run it alone. A cut in that window ends as with a
  peer without freeze mode: the frozen host steps down after about 75 s and
  its guests restart on the peer, with nothing acknowledged lost.
- **The pool setting turned back on** (by an operator, or by a pool master
  running an older release): a guest pool HA restarts on the losing host
  before its copy is withdrawn is still possible. It is caught after the
  discard, stopped, marked `twinstor-disks-suspect` in its `other_config`
  and left halted, with an alert naming its disks and the checks to run
  before starting it by hand. TwinStor never starts it.
- **A fence-peer run that ends without a verdict** (it stood down for the
  peer's administrative replication bounce, or waited 60 s behind another
  run) leaves the I/O frozen without a recorded yield, so the budget does
  not apply: the freeze ends when the link returns or, with pool HA on, when
  XHA's statefile watchdog reboots the host. Both need a coincidence (a
  real cut during a bounce, or a run wedged on DRBD calls); nothing
  acknowledged is lost.

## Evidence

Testing on a fate-shared pool, HA on, 120 s HA timeout. Each case was run
with freeze mode off and on, and the acknowledged appends were checked
against the guests' disks afterwards:

| Cut | Guests | Freeze off | Freeze on |
|---|---|---|---|
| 10 s flap | all on the isolated host | - | 0 lost, no restart, no reboot |
| 25 s | all on the isolated host | 0 lost, no restart | 0 lost, no restart, no reboot |
| 25 s | none on the isolated host | - | 0 lost, no restart |
| 25 s | one on the isolated host, three on the other | 42 lost on the isolated guest, it restarted | 0 lost, only the isolated guest restarted |
| 50 s | all on the isolated host | 0 lost, no restart, no reboot | 0 lost, no restart, no reboot |
| 75 s | all on the isolated host | 0 lost; XHA fenced the host about 4 min after the cut, guests restarted | 0 lost; host rebooted 2 min 20 s after the cut (see Limits), guests restarted |
| 150 s | all on the isolated host | 43 lost per guest; XHA fenced the host, guests restarted | 0 lost; budget step-down, XHA fenced the host, guests restarted |

No guest saw an I/O error in any freeze-mode case. Arbitrated pools ran
their own regression campaign on the same release with freeze mode inert.

Physical faults on the same pool, freeze mode on (all guests on the host
whose cable was pulled; both hosts for a switch power cycle):

| Fault | Guests frozen | Result |
|---|---|---|
| Cable pulled 10 s | 33 s, on an earlier build (a 10 s cut now freezes about 18 s, see above) | 0 lost, no restart, no reboot |
| Cable pulled 30 s | 41 s | 0 lost, no restart, no reboot |
| Cable pulled 60 s | 92 s | 0 lost, no restart, no reboot (the local path failed on keepalive timeouts and came back with the session) |
| Switch power-cycled, 35 s without network | about 38 s | 0 lost, no restart, no reboot |
| Switch power-cycled, about 2 min 20 s without network | budget step-down at 100 s | 0 lost; XHA fenced both hosts (each isolated from the other for 2 min); guests running again 4.5 min after the outage began |

A whole-switch outage longer than the HA timeout ends with XHA rebooting
both hosts, with or without freeze mode: neither can tell it is not the
isolated one. No acknowledged write is lost, and the guests restart once
the pair is back.

Each host in each role: the same pool with each host as pool master in
turn, cutting the master, then the other host (5 s with one or three of the
four guests on the cut host, 25 s with one, 45 s with all): 16 of 16 with no
acknowledged write lost, on each of the final release candidates. Cuts
where the master loses while holding guests were repeated about 60 times on
both hosts, with no acknowledged write lost; they are also the cases behind
[Guests stopped on a losing pool master](#guests-stopped-on-a-losing-pool-master).
