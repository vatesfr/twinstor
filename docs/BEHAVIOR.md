# TwinStor behaviour reference

This is the index of failure and lifecycle cases. For each case it says what
triggers it, what DRBD and the daemon do, what keeps serving, what happens to
VMs, whether you still have redundancy, and what the operator does (if
anything).

Related documents:

- [CONVERGENCE.md](CONVERGENCE.md): whether each failure heals on its own once
  the hosts are back, or stops at a safe hold for an operator.
- [NETWORK_TOPOLOGY_SAFETY.md](NETWORK_TOPOLOGY_SAFETY.md): network and
  split-brain mechanics.
- [VM_PLACEMENT_SAFETY.md](VM_PLACEMENT_SAFETY.md): placement safety.
- [FAQ.md](FAQ.md): walkthroughs and recovery procedures.

## How to read this

Unless a case says otherwise, it assumes pool HA is armed
(`pool.ha-enabled=true`), both hosts were healthy beforehand, and the
deployment satisfies the network invariant (the peer and the fence target
share a physical path). A case names the mode (fate-shared or arbitrated) only
where the two differ.

**A** is the host that stays authoritative (the survivor, or the sync source).
**B** is the affected host. The **keeper** is the host whose configured node
name sorts first; it wins the deterministic tiebreaks (see
NETWORK_TOPOLOGY_SAFETY.md, "The keeper").

Readiness labels (from `readiness.rs`, shown by `twinstor status`):

- **ready**: local `Primary`+`UpToDate`, peer `Connected`+`UpToDate`. Safe to
  place VMs.
- **serving-without-redundancy**: this host can serve, but the peer is not a
  healthy redundant copy (it is gone, or this host serves through the peer
  while diskless).
- **degraded-paths**: the data is fully redundant, but the consumer-side path
  stack is impaired. Reported and self-healed, never enforced.
- **not-ready**: the local replica is not safe to serve (not `Primary`, not
  `UpToDate`, resyncing, or the toolstack has not converged). The daemon does
  not advertise the local iSCSI target, so placement avoids this host.

Invariants that recur below:

- The local iSCSI target is gated on readiness at bring-up: a `not-ready` host
  does not advertise its target, so it is not a usable path. A host that was
  already serving when it goes `Inconsistent` (a resync) keeps its target up;
  a resync is never a reason to leave Online.
- A true split-brain is resolved from the DRBD generation lineage, with the
  pool master only as a fallback. The loser's VMs are stopped before its copy
  is discarded and are restarted from the winner's copy (case 21). If the
  winner cannot be decided, both copies are left intact and an alert is
  raised.

## Summary

The **Occurrence** column rates how often each case is expected over the life
of a typical edge or branch-office deployment: a small office, shop or
factory, one switch, no operator on site, repairs by scheduled visit. This
differs from a datacenter. A power outage hits both hosts (one circuit, often
no UPS), so cases 4 and 8 are likely, not rare. And because nobody is on site,
a degraded window lasts until someone visits, so the slow single-host and
capacity cases matter more than the exotic split-brain ones.

| # | Case | Occurrence | DRBD state | What serves | VMs / HA | Redundancy |
|---|------|-----------|-----------|-------------|----------|-----------|
| 1 | Steady state | baseline | both Primary/UpToDate/Connected | both, full speed | normal | full |
| 2 | Daemon crash/restart | likely | unchanged (kernel state) | both, no gap | no impact | full |
| 3 | Graceful reboot / RPU | likely | B down then resync | A | VMs evacuated first, return after | none while B is down |
| 4 | Cold start (both boot) | likely | connect before promote | both once up | start after sync | full once synced |
| 5 | Staggered double reboot | less likely | later host defers its promote | A | VMs start when ready | divergence avoided |
| 6 | Single host death (HA armed) | likely | A alone Primary/UpToDate | A | XHA restarts B's VMs on A | none until B is back |
| 7 | Single host death (HA disarmed) | less likely | A alone | A | B's VMs not restarted | none, and no fencing |
| 8 | Simultaneous dual reboot / power outage | likely | both down then up | none until back | HA restarts protected VMs | restored after sync |
| 9 | Local disk failure (one host) | less likely | B Diskless, serves via A | both (B forwards to A) | no path change, no impact | none until the disk is back |
| 10 | Transient disk error | likely | B Diskless then re-attach | both | no impact | restored on re-attach |
| 11 | Disk replacement + resync | less likely | A SyncSource, B SyncTarget/Inconsistent | A full; B via A | VMs can start on B, over A | **none until B is UpToDate** |
| 12 | During any resync | likely | one Inconsistent | source full; target via source | placement avoids the `not-ready` host | none until UpToDate |
| 13 | Network flap (< HA timeout) | likely | stays Connected | both | no impact | full |
| 14 | Inter-host link loss (invariant holds) | less likely | split, both also lose the gateway | none (both freeze) | clean double outage | restored on reconnect |
| 15 | Switch failure, fate-shared | less likely | both lose peer + gateway | none | full pool outage, VMs restart on recovery | restored after |
| 16 | Switch failure, gateway on the switch, arbitrated | rare | DRBD rides the cable (no split) | both hold (beacon: neither can serve, so no fence) | VMs keep running, guest network down until the switch is back | data safe, compute kept |
| 17 | Switch failure, gateway survives, arbitrated | rare | DRBD rides the cable | A (and B once reconnected) | brief replication pause | kept |
| 18 | Direct-link failure, arbitrated | rare | reconnect on the switch path; a loss over ~4 s is refused | keeper | short flap: a stall; longer: non-keeper's VMs frozen ~70 s, then restarted on the keeper | data full; redundancy after the resync |
| 19 | Both host-to-host paths down, gateway up | rare | split, both reach the gateway | both keep serving | split-brain risk | resolved with an alert; loser's recent writes lost |
| 20 | StandAlone clean rejoin | likely | B was down, no divergence | A then both | none | restored by a plain connect |
| 21 | True split-brain (winner decidable) | less likely | both wrote while split | winner | loser's VMs restart on the winner | restored after resync; loser's writes discarded |
| 22 | Split-brain, winner undecidable | rare | both StandAlone | both, each its own copy | manual review | both copies kept, nothing discarded |
| 23 | Host not-ready | likely | local not Primary/UpToDate | other host | placement avoids this host | depends on cause |
| 24 | HA disarmed past grace | less likely | any | as is | no fencing authority | exposed |
| 25 | Backing store full (SR out of space) | likely | unchanged; I/O errors | guest I/O fails | VMs may crash or pause | full (both copies equally full) |
| 26 | Abrupt power loss (durability) | likely | both down then up | none until back | acknowledged writes are on media | restored after sync |
| 27 | Brownout / repeated boot interruption | less likely | re-waits for connect each boot | A once one host settles | start when ready | restored after sync |
| 28 | Double disk failure (both backing disks dead) | rare | both Diskless, no peer | nothing (target stops) | VMs lose storage | **total data loss** |
| 29 | Long-term single-host operation | likely | A alone, indefinitely | A (`serving-without-redundancy`) | runs; no failover target | none until the peer returns |
| 30 | Upgrade / version or kernel-module skew | less likely | may fail to connect | the reachable host | as a disconnected peer | none while skewed |
| 31 | Partial setup failure (one host fails) | less likely | incomplete on one host | possibly neither | SR may not be created | n/a (not yet redundant) |
| 32 | Silent corruption / bitrot (no scrub) | rare | states look healthy | both (serving bad blocks) | undetected | nominal but unverified |
| 33 | XHA re-admission join deadlock | less likely | storage fine, HA join cycling | A (storage unaffected) | run, no HA protection | none (control plane only) |
| 34 | Mutual-yield recovery (both yielded) | rare | both frozen, serving nobody | neither until resolved | stalled | none (operator picks the survivor) |
| 35 | Single-breaker rack (host + switch) | less likely | full outage, clean rejoin | nothing during the outage | down with the switch | none |
| 36 | xhad dead after boot raced storage | less likely | healthy, but no HA on that host | A (storage unaffected) | run, no failover cover | none (control plane only) |
| 37 | Total switch blackout | less likely | arbitrated: both hold (beacon), keep VMs; fate-shared: both fence | arbitrated: VMs stay up (guest network down); fate-shared: nothing until the switch returns | arbitrated: no restart; fate-shared: restart on reassembly | none (fate-shared: empty divergence discarded) |
| 38 | Stale serving Primary | rare | one side loops aborted reconnects | A (keeper side) | run via multipath | none (catch-up resync) |
| 39 | Isolated pool master, arbitrated | rare | Connected over the cable | the reachable host | VMs restart on the reachable host | kept (master stands down as Secondary) |
| 41 | Isolated non-master, arbitrated | rare | Connected over the cable | the reachable host | VMs restart on the master after one self-fence | kept |
| 42 | Reboot of the last UpToDate copy | less likely | peer Connected but Inconsistent | this host until it reboots | reboot held until the peer is UpToDate (capped) | restored before the reboot, or alerted |
| 43 | Sole survivor `Consistent`, peer Diskless | rare | survivor cannot promote | nothing until recovered | down until recovered | none until the peer's backing is rebuilt |
| 44 | Replication stuck mid-handshake (WFBitMap) | rare | one host stuck in WFBitMapS/T | both, replication one-way | no impact until cleared | reduced until cleared |
| 45 | One host's link drops, other keeps its gateway (fate-shared) | less likely | split | the host with the gateway | losing host's VMs restart on the survivor | restored after resync |

There is no case 40.

## Lifecycle and steady state

### 1. Steady state

- **Trigger:** normal operation, both hosts healthy.
- **DRBD:** both `Primary`/`UpToDate`/`Connected`. In arbitrated mode, two
  paths in active-backup: the direct link active (`repl path: direct link` in
  `twinstor status`), the switch path as hot standby.
- **Serves:** both `ready`; the SR is served by both LIO targets through
  multipath.
- **VMs/HA:** VMs run on either host; XHA armed.
- **Redundancy:** full. **Operator:** none.

### 2. twinstor daemon crash or restart

- **Trigger:** the daemon dies or is restarted. DRBD and iSCSI are kernel
  state and survive it.
- **Behaviour:** no I/O gap. systemd restarts the daemon
  (`Restart=on-failure`); it finds DRBD already Primary and iSCSI already up,
  and resumes Online.
- **VMs/HA:** no impact. **Redundancy:** full. **Operator:** none.
- **See:** FAQ.md, failure scenario "twinstor daemon crash".

### 3. Graceful reboot / Rolling Pool Update (RPU)

- **Trigger:** planned maintenance; VMs are evacuated from B first (standard
  RPU).
- **Behaviour:** the daemon stops B's target, B reboots, DRBD reconnects and
  resyncs only the blocks that changed while B was down.
- **Path selection:** when B reconnects, the daemon re-asserts A's own local
  multipath path on the reconnect, so guest I/O on A never lingers on B's
  not-yet-serving path (where a guest could take an I/O error and remount
  read-only).
- **Serves:** A throughout; B returns to `ready` after the small resync.
- **VMs/HA:** VMs migrate off B before the reboot and can return after.
- **Redundancy:** none while B is down, restored after the resync.
- **Operator:** standard RPU; see FAQ.md "How do I do planned maintenance?".

### 4. Cold start (both hosts boot together)

- **Trigger:** both hosts power on at once.
- **Behaviour:** each host honours a startup connection wait
  (`wfc-timeout`/`degr-wfc-timeout`), so the peers connect before either
  promotes and come up as a synced pair, not two lone primaries.
- **Serves:** both reach `ready` once connected and synced.
- **VMs/HA:** VMs start after the pair is up; XHA sees both hosts alive.
- **Redundancy:** full once synced. **Operator:** none.

### 5. Staggered double reboot

- **Trigger:** both hosts reboot a short interval apart (for example a botched
  update).
- **Behaviour:** the later host sees DRBD `Connecting` and defers its promote
  while the peer may still be booting (`drbdadm wait-connect`, bounded). It
  connects and resyncs before it writes, so no divergence forms.
- **Serves:** the first host serves; the later one joins cleanly.
- **VMs/HA:** VMs start once a host is `ready`.
- **Redundancy:** restored once both are up. **Operator:** none.
- **See:** `should_defer_promote` in `state_machine.rs`.

## Host failures

### 6. Single host death or power loss (HA armed)

- **Trigger:** B loses power or hangs.
- **Behaviour:** A continues alone, `Primary`/`UpToDate`, never interrupted.
  When B returns it resyncs the changed blocks.
- **Serves:** A, as `serving-without-redundancy`.
- **VMs/HA:** XHA detects B gone (heartbeat and statefile timeout), B's
  watchdog resets B, and XAPI restarts B's protected VMs on A. If B was the
  pool master, XHA elects A.
- **Redundancy:** none until B rejoins. **Operator:** none.
- **See:** FAQ.md, failure scenario "Node power loss".

### 7. Single host death (HA disarmed)

- **Trigger:** B dies while HA is not armed.
- **Behaviour:** A continues alone and keeps serving
  (`serving-without-redundancy`).
- **VMs/HA:** **B's VMs are not restarted**, and there is no fencing
  authority. This is the exposed state.
- **Redundancy:** none, and no protection against a partition.
- **Operator:** re-arm HA (`xe pool-ha-enable`). The daemon alerts after
  about 30 minutes disarmed (case 24).
- **See:** VM_PLACEMENT_SAFETY.md (XHA armed).

### 8. Simultaneous dual reboot or full power outage

- **Trigger:** both hosts go down and come back together (power strip, site
  outage).
- **Behaviour:** both boot, connect before promoting (case 4), and resync if
  needed. Storage returns once a host reaches `ready`.
- **VMs/HA:** XHA restarts protected VMs once the pool is back and within the
  HA failure budget (`ha-host-failures-to-tolerate`). An outage beyond the
  budget may need a manual VM start.
- **Redundancy:** restored after sync.
- **Operator:** check that protected VMs restarted.
- **Boot gate.** A host about to promote alone (connection wait expired, peer
  not connected) first looks for positive evidence that the peer is alive,
  from local and direct signals only:
  - the peer's iSCSI portal, probed on the configured peer address, the
    direct-link address, and the pool master this host follows in
    `pool.conf`. The peer's management address is also read from the local
    copy of the pool database, so a host fenced while it was pool master
    (whose `pool.conf` still says `master`) can still probe it;
  - the local xhad liveset;
  - the peer's twinstor beacon.

  What the gate does with that evidence:
  - **Peer serving or HA-live:** the promote is held with no time limit
    (case 19).
  - **Peer seen serving** (its portal answered): the host remembers it. Until
    DRBD is Connected again, or this host's own DRBD generation moves (a
    resync, an operator force), it never promotes alone, even if the evidence
    later disappears, because its copy may be behind the peer's (see
    [Known limitations](#known-limitations)). The memory is durable
    (`/var/lib/twinstor/boot-gate-peer-served`, keyed by the host's current
    generation) and survives daemon restarts and reboots.
  - **Only the peer's daemon answers** (both hosts bringing storage up while
    replication is down, peer never seen serving): only the keeper promotes.
    The other host waits about 11 minutes (10 minutes, then the 100 s clearing
    grace) before the normal lone-survivor rules apply, or not at all when the
    keeper's beacon attests that its replica is destroyed (a device that is
    merely absent may still be assembling and does not count).
  - **No evidence at all** (for example a lone host returning from a full
    power loss): the normal timers apply. Arbitrated: promote after 100 s.
    Fate-shared: promote after the 10-minute lone-survivor hold, provided the
    gateway or a ping target answers. An isolated fate-shared host keeps
    holding and alerting until its network is back. A site with no gateway or
    ping target configured holds for an operator, because the fence-peer
    handler then refuses a lone promote.
  - After evidence disappears, the gate keeps holding for 100 s, so a single
    missed probe cannot release a promote.

  Holds show in `twinstor status`. Reconnects are retried every 60 s without
  steering, so the arbitrated switch fallback path stays usable.

## Disk and resync

### 9. Local disk failure (one host)

- **Trigger:** B's backing disk starts erroring; DRBD detaches it
  (`on-io-error detach`).
- **Behaviour:** B is `Diskless` `Primary`. A diskless Primary forwards all
  I/O to A's disk.
- **Serves:** B keeps its target serving (`serving-without-redundancy`). No
  path change for VMs or the XHA statefile, only higher I/O latency on B. A
  priority-1 `twinstor_degraded` alert is raised.
- **VMs/HA:** no failover, no freeze, no fence; VMs are unaffected.
- **Redundancy:** none until the disk returns. If A is also unavailable
  (double failure), B's target stops so initiators fail fast.
- **Operator:** none for the automatic path; a physically replaced disk
  follows case 11.
- **See:** FAQ.md, failure scenario "Local disk failure".

### 10. Transient disk error

- **Trigger:** a controller blip or hotplug that resolves on its own.
- **Behaviour:** B goes `Diskless` and the daemon retries `drbdadm attach`
  (throttled). As soon as the device returns it re-attaches and resyncs the
  changed blocks. Meanwhile B serves through A (case 9).
- **VMs/HA:** no impact.
- **Redundancy:** restored on re-attach, with no intervention.
- **Operator:** none.

### 11. Disk replacement and resync

- **Trigger:** B's disk is physically replaced with a blank one and B boots.
- **Behaviour:** B comes up `Inconsistent` and a full resync starts from A
  (A = `SyncSource`/`UpToDate`, B = `SyncTarget`/`Inconsistent`).
- **Serves:** B is `not-ready` and does not advertise its target until it
  reaches `UpToDate`. A serves at full speed. B can still use the SR over A's
  path (its initiator logs into A's portal), so the SR is available on B with
  a single, remote path.
- **VMs/HA:** VMs can start on B during the resync; B runs the compute while
  all its storage I/O goes over the network to A's copy. Do not let HA treat B
  as a placement or failover target while it is `not-ready`.
- **Redundancy:** **none until B is UpToDate.** A is a single point of
  failure during the resync: if A or the inter-host link fails mid-sync, B's
  copy is incomplete and its only path was to A, so the SR goes down. The
  data is not corrupted; B simply cannot serve.
- **Operator:** prefer running VMs on A until B reaches `UpToDate`, then
  rebalance. Follow FAQ.md "How do I replace a failed disk?".
- **See:** VM_PLACEMENT_SAFETY.md (host storage readiness); `readiness.rs`.

### 12. What is usable during any resync

- **Trigger:** any resync in progress (cases 3, 6, 8, 10, 11, or a split-brain
  loser).
- **DRBD:** one host `UpToDate` (source), one `Inconsistent` (target),
  `Connected`.
- **Serves:** the source at full speed. Reads on the target are served
  transparently (DRBD fetches not-yet-synced blocks from the source).
  XAPI and pool membership are unaffected on both hosts; only the twinstor
  target is gated.
- **VMs/HA:** the source is a valid placement target. The `Inconsistent` host
  is `not-ready`, so placement avoids it; on a fresh boot its target stays
  gated until `UpToDate`.
- **Redundancy:** none until both are `UpToDate`; the source is a single point
  of failure for that window.
- **Operator:** let the resync finish before relying on the second copy. A
  split forced with `drbdadm new-current-uuid` resyncs the full device even
  when no data diverged.

### 42. Reboot while this host is the last UpToDate copy (reboot drain)

- **Trigger:** a clean reboot (manual `reboot`, a kernel-update or `yum`
  reboot, `xe host-reboot`, an XO rolling pool update) on a host that is
  `UpToDate` while the peer is Connected but still resyncing (`Inconsistent`).
  This host is then the only current copy and the only one carrying guest I/O
  (an `Inconsistent` SyncTarget cannot serve, case 12). Rebooting it drops all
  guest I/O and strands the peer's rebuild, with a risk of losing the tail if
  it does not come back. Such a resync often follows a disk replacement
  (case 11).
- **Mechanism:** `twinstor-reboot-drain.service` runs `twinstor reboot-drain`
  from a systemd `ExecStop` early in the shutdown, before the network goes
  down, so DRBD is still readable and the resync keeps going. It reads DRBD
  locally (never `xe`, which no longer answers this late in shutdown):
  - **A current copy survives: proceed at once.** The peer is Connected and
    `UpToDate`, or this host is not itself the current copy (`Inconsistent`
    SyncTarget, `Diskless`, `Outdated`). The returning SyncTarget always
    reboots freely; the drain only ever holds the sync source.
  - **Last UpToDate copy, peer catching up: hold, bounded.** The drain re-reads
    DRBD every 5 s until the peer is `UpToDate` (then proceeds), the operator
    overrides, or the cap expires. Cap: 600 s by default
    (`[reboot_drain] max_secs`), clamped to 615 s so the drain always proceeds
    cleanly before the unit's `TimeoutStopSec=660`; to hold longer, raise both.
    A peer that is Connected but not actively resyncing (a paused or stuck
    sync) cannot reach `UpToDate` by waiting, so the drain proceeds after
    about 15 s.
  - **Last UpToDate copy, peer not connected: proceed, with an alert.** Waiting
    cannot help, since the peer cannot resync from this host.
- **Fails open:** any doubt (DRBD unreadable, config missing, an error, a read
  that fails mid-hold) proceeds. Only a confirmed "last UpToDate copy, peer
  mid-resync" holds, and only for the bounded window. A wedged host is worse
  than the risk being mitigated.
- **Effect:** usually the hold defers the reboot by seconds to minutes until
  the peer is a full copy, so the reboot lands with redundancy intact. A
  healthy pool (peer already `UpToDate`) reboots after one local DRBD read
  and no delay.
- **Override:** `touch /run/twinstor/allow-unsafe-reboot` proceeds without
  waiting. It is re-checked on every 5 s poll, so a live hold can be released
  from another shell. It lives in `/run`, so it applies to the current
  shutdown only.
- **Next-boot alert:** when a reboot proceeds while this host was the last
  UpToDate copy (cap reached, peer unreachable, override, or a mid-hold read
  failure), the drain writes `/var/lib/twinstor/last-reboot-drain`. The daemon
  consumes it at the next start and raises `twinstor_reboot_unsafe`, saying
  where the peer's resync was. If the reboot was deliberate, nothing to do;
  otherwise confirm both hosts are back Connected and `UpToDate`, and do not
  reboot the UpToDate host again until the resync completes.
- **Why the cap is enough:** a rolling-reboot resync is a small delta and
  finishes well inside the cap. A full rebuild after a disk swap (case 11)
  exceeds it and becomes a bounded, alerted proceed rather than a hang.
- **Scope:** every clean reboot goes through the shutdown transaction, so one
  unit covers them all. `reboot -f`, a kernel panic, a power cut and an XHA
  hard fence bypass it. The orchestrator-side counterpart is the
  `twinstor-synced` advertisement (case 3), which paces XO RPU; the drain
  covers the clean reboots XO never sees.
- **Operator:** none in the common case. `[reboot_drain] enabled = false`
  turns the hold off (the unit becomes a no-op).
- **See:** FAQ.md "Can I reboot a node during a resync?"; cases 3, 11, 12;
  `reboot_drain.rs`, `config/twinstor-reboot-drain.service`.
- **Validated:** the decision core (`reboot_safety`) is unit-tested across the
  disk-state, connection and peer-state matrix. End to end on an arbitrated
  and a fate-shared pool against a real Connected+`Inconsistent` resync: a
  healthy pool proceeds transparently (under 20 ms); the SyncTarget reboots
  freely; the source holds, then proceeds at the cap (breadcrumb written);
  it releases as soon as the peer reaches `UpToDate` (no breadcrumb); it
  proceeds with an alert when the peer is disconnected; it fails open on an
  unreadable resource; and the `ExecStop` really blocks the stop for the
  hold.

### 43. Sole survivor stuck `Consistent` with a Diskless peer

- **Trigger:** a host serving as Primary crashes without its fence-peer
  having outdated the peer (for example it was network-isolated when it died)
  and reboots with its disk `Consistent`. The only peer connects `Diskless`
  because its backing device is gone (destroyed array, dead disk). DRBD
  refuses a plain promote of a `Consistent` disk with no peer to revalidate
  against ("Need access to UpToDate data"). Without escalation the pool's
  storage stays down indefinitely, and with the HA statefile on the SR, xapi
  can wedge pool-wide as well (one reported outage lasted over 40 minutes).
- **Backing attestation:** each host tells its peer what it can prove about
  its own backing device, on the peer-signal beacon (TCP 7900, no extra
  port), on every peer-facing address, in both modes. The querier checks
  the identity (host name and resource). States: *attached*; *destroyed*
  (device node present but no valid DRBD metadata, probed with
  `drbdadm get-gi`, published only after 3 identical readings in a row);
  *detached* (metadata present: a real replica); *absent* (device node
  missing); *unknown*.
- **Automatic recovery (narrow):** the survivor runs
  `drbdadm primary --force` only when all of these hold:
  - DRBD `Connected`; local `Consistent` + `Secondary`; peer `Diskless` +
    `Secondary`;
  - the peer has attested *destroyed* continuously for 5 minutes (persisted
    across daemon restarts, with a monotonic in-process floor against clock
    steps);
  - local metadata shows this host crashed as the last writer it saw;
  - the configured hostname matches the kernel hostname;
  - a fresh re-read of everything still agrees just before acting.

  It writes `/var/lib/twinstor/force-promote-breadcrumb` before the promote.
  The target and SR then come up on the normal path (this works with xapi
  down), and the pool runs without redundancy until the peer's backing is
  rebuilt (`twinstor replace-backing`).
- **Guided manual recovery (everything else):** if the peer's device is
  merely *absent* (an unassembled md array or an unenumerated disk may hold
  an intact, possibly fresher, replica), its attestation is unavailable
  (older daemon), its metadata is present, or the local disk is `Outdated`
  (a recorded rollback), the daemon does not act. It raises an alert naming
  the recovery: `drbdadm primary --force twinstor-<res>` on the survivor once
  the peer's data is confirmed lost, or restore or reassemble the peer's
  device and it rejoins automatically. No DRBD-visible signal can tell "no
  replica exists" from "replica temporarily invisible", and force-promoting
  over an intact, fresher copy would be an automated rollback.
- **Alerts with xapi down:** alerts that cannot be delivered are also written
  to the kernel log (`dmesg`/kern.log, throttled). The xapi alert lands as
  soon as the toolstack returns.
- **Operator:** none for the automatic path. Otherwise, first try to restore
  the peer's backing device; only if its data is confirmed lost, run the
  command from the alert on the survivor.
- **See:** cases 9-11; the both-Outdated tiebreak in CONVERGENCE.md (same
  deadlock family).

### 44. Replication stuck mid-handshake (WFBitMap) and the automatic clear

- **Trigger:** two promotions landing in the same instant (for example a
  manual force-promote racing the peer daemon's own promote) can leave one
  host in DRBD's bitmap-exchange step (`WFBitMapS`/`WFBitMapT`) indefinitely
  while the peer reports Established. The connection is up but replication
  is one-way: writes through the stuck host's path do not reach the peer, and
  the two hosts disagree about health (one "ready", one "peer disk
  Consistent"). A step that normally lasts seconds becoming permanent is the
  signature.
- **Behaviour:** when the state persists 3 minutes while Connected, the daemon
  bounces the replication connection, the same remedy that works by hand. The
  bounce is bracketed, because an unannounced disconnect under an armed
  fencing policy makes the peer fence and outdate the disconnecting host,
  which can create a split-brain from nothing (reproduced in testing):
  - the bouncing host advertises the bounce on the beacon (a trailing `B`
    token), and the peer's fence-peer handler stands down when it sees it;
  - local fencing is disarmed for the seconds of the bounce, guarded by a
    marker the daemon re-arms from at startup, with a per-tick reconciler if
    the re-arm fails;
  - the bounce is refused unless the peer provably runs the flag-aware fence
    script (an older peer daemon means alert only) and the local beacon is
    verifiably serving the flag.

  Rate limit: 10-minute cooldown, 3 attempts per episode (the count resets
  only after the link has been healthy for a full cooldown), then an alert.
- **Manual procedure** (after the attempts are used up, or when the
  automation stands down):
  1. On both hosts: `drbdadm net-options --fencing=dont-care <res>` (with both
     disarmed, no handler runs anywhere).
  2. On the stuck host: `drbdadm disconnect --force <res>` (a plain disconnect
     may be refused in this state); one plain `disconnect` if `connect`
     complains about a leftover net-config; then `drbdadm connect <res>`.
  3. Confirm both hosts show Connected and UpToDate/UpToDate.
  4. On both hosts, restore `--fencing=resource-only` (fate-shared) or
     `--fencing=resource-and-stonith` (arbitrated). This step is not
     optional.
- **Freeze visibility:** with `resource-and-stonith`, a peer standing down
  leaves its I/O briefly frozen until the reconnect (seconds; the XHA
  statefile budget has more than 15x margin). The daemon reports
  `suspended:fencing` and alerts if such a freeze lasts over 2 minutes with
  replication disconnected.
- **Operator:** none for the automatic path; the alert gives the manual
  procedure when the automation stands down.
- **See:** cases 9-12 for degraded serving; the mutual-SyncSource limbo heal
  in CONVERGENCE.md (same bounce, same bracket).

## Network and topology

These depend on the mode and on the network invariant. Full analysis:
[NETWORK_TOPOLOGY_SAFETY.md](NETWORK_TOPOLOGY_SAFETY.md).

### 13. Network flap shorter than the HA timeout

- **Trigger:** a brief interruption or packet loss while the link carrier
  stays up (STP reconvergence, congestion, an upstream blip).
- **DRBD:** stays `Connected`. Timers: `ping-int` 2 s with a 0.5 s
  `ping-timeout`, so an idle link drops within about 2.5 s of real packet
  loss; under active I/O, `ko-count 3` against the 5 s connection `timeout`,
  about 15 s. A congestion blip delays TCP rather than losing the ping ack,
  so short in-band flaps are ridden with no disconnect and no fence-peer.
- **VMs/HA:** XHA does not act if the heartbeat returns before its timeout
  (120 s fate-shared, 90 s arbitrated by default). No impact.
- **Redundancy:** full. **Operator:** none.
- **Carrier drop (validated on hardware):** a physical link flap (cable pull,
  NIC reset, switch port bounce) is not this case, however short. The TCP
  session dies with the carrier, DRBD disconnects at once, and fence-peer
  runs 2-3 s later.
  - Link back before those probes: both sides reach the gateway. The handler
    probes the peer's DRBD port before confirming the survive verdict and,
    finding it listening, waits a few seconds so DRBD reconnects first.
    Writes resume replicated: no divergence, no restarts, just a stall.
  - Reconnect misses that wait: a bounded split-brain, resolved by case 21
    (in testing, 84 s to converged, the loser's VMs crash-restarted).
  - Link back after the probes but within the XHA timeout: the isolated side
    froze and simply resumes. A stall, no restarts.

  For a link that keeps flapping, see FAQ.md "What if a link keeps
  flapping".

### 14. Inter-host link loss, invariant holds (fate-shared)

- **Trigger:** the single shared path drops, so the peer and the gateway are
  lost together.
- **Behaviour:** DRBD splits. Both fence-peer handlers find no ping target,
  freeze I/O and self-fence.
- **Serves:** nothing; a clean double outage, no writer continues.
- **VMs/HA:** both hosts self-fence; VMs restart on recovery.
- **Redundancy:** restored on reconnect, no corruption (the isolated hosts
  never wrote after the freeze).
- **Operator:** none. This is the safe outcome the invariant guarantees.

### 45. One host's link drops, the other keeps its gateway (fate-shared)

- **Trigger:** the cable, NIC or switch port of one host fails (or
  renegotiates) for longer than DRBD's ping timeout; the other host still
  reaches the gateway.
- **With freeze mode (the default):** DRBD freezes I/O on both hosts at the
  disconnect. The host that reaches its gateway resumes within seconds; the
  isolated host stays frozen, so it acknowledges nothing its peer lacks. See
  [FATE_SHARED_FREEZE_MODE.md](FATE_SHARED_FREEZE_MODE.md) for how the copy
  to keep is chosen. Freeze mode engages only when both hosts run 0.7.0 or
  later.
- **With freeze mode off:** both hosts lose the peer and run the fence-peer
  handler, which first logs the host out of the peer's iSCSI portal (its copy
  may now diverge, so its guests must not reach the peer's; it logs back in
  once the pair is Connected). Fate-shared mode fences `resource-only`, so I/O
  keeps flowing while the handler decides: the isolated host serves its VMs
  for those seconds (the ping timeouts, about 4 s observed), then finds no
  ping target, freezes and yields. The other host reaches its gateway and
  keeps serving. Both wrote, so on reconnect DRBD declares a split-brain (a
  true fork), whatever the lineage counts say. Resolution depends on whether
  VMs on the SR ran on the yielded host:
  - **No VM ran there** (its writes were in-flight blocks or the XHA
    statefile): it latches the loser verdict at the yield (durably, bound to
    its DRBD generation) and attests it on the peer beacon. The survivor reads
    it before its lineage verdict and takes the win, even if the yielded side
    shows a few more unreplicated blocks. The yielded host discards and
    resyncs; the survivor's VMs keep running.
  - **VMs ran there:** they may hold acknowledged writes the survivor lacks,
    so the yield does not decide. Both hosts follow the DRBD generation
    lineage and reach the same verdict. If the yielded host holds more, it
    keeps its copy, lifts its freeze, and its VMs continue; the survivor
    discards. Otherwise the yielded host stops its VMs (crash boundary),
    discards and resyncs, and its VMs restart on the survivor, **losing what
    they wrote in their last seconds before the freeze**.
- **Durability of the verdict:** a win is recorded durably and attested
  against the loser's generation, and a host latched as the loser never
  promotes alone, so a daemon restart or a reboot of either host does not
  change the outcome. A host that had already offered its own discard
  withdraws it before reconnecting.
- **VMs/HA:** only the losing host's VMs restart. With pool HA armed, a link
  loss of more than about 60 s can also end with XHA rebooting one host a few
  minutes later, possibly the one that kept serving; nothing acknowledged is
  lost (see [Known limitations](#known-limitations)).
- **Operator:** none. If both hosts still hold after 5 minutes (an older peer
  during an upgrade, both yielded, a survivor that is not UpToDate), the
  `twinstor_split_brain` "split-brain wedge" alert gives the manual step (FAQ
  recovery table).

### 15. Switch failure, fate-shared mode

- **Trigger:** the single switch dies; both hosts lose the peer and the
  gateway.
- **Behaviour:** DRBD splits; both hosts freeze and self-fence.
- **Serves:** nothing; full pool outage.
- **VMs/HA:** VMs restart on recovery.
- **Redundancy:** restored after; no corruption.
- **Operator:** none. The switch is the known single point of failure of
  this mode. See also case 37.

### 16. Switch failure with the gateway on the dead switch, arbitrated mode

- **Trigger:** the switch dies and the gateway was behind it; the direct
  cable survives.
- **DRBD:** replication already runs over the cable, so the switch death does
  not touch it: the data stays consistent, no split-brain, no reconnect.
- **Behaviour:** both hosts lose their gateway. Each reads the peer's gateway
  state over the direct-link beacon, finds that neither side can reach
  clients, and holds: it suppresses its own XHA statefile fence (local
  `ha-disable`) and keeps its VMs running. This is case 37 in arbitrated
  mode.
- **VMs/HA:** VMs keep running, but their guest network is on the dead switch,
  so clients cannot reach them until it returns. Nothing fences. When the
  switch returns, each host re-arms HA and the pool converges without an
  outage.
- **Redundancy:** data safe on both copies; compute kept, but unreachable.
- **Operator:** none. Reaching the VMs during a switch loss needs redundancy at
  the network layer (bonded uplinks, redundant switches), outside TwinStor.
- **See:** NETWORK_TOPOLOGY_SAFETY.md, "Arbitrated mode".

### 17. Switch failure with the gateway still reachable, arbitrated mode

- **Trigger:** the switch path used for storage fails, but the gateway is
  reachable by another route; the direct cable survives.
- **Behaviour:** replication already runs over the cable: no pause, no
  reconnect, no split-brain. Fence-peer never fires (the replication link
  never dropped).
- **VMs/HA:** no impact from replication; management-path effects depend on
  the network layout.
- **Redundancy:** kept. **Operator:** none.

### 18. Direct-link failure, arbitrated mode

- **Trigger:** the back-to-back cable (or its NIC) fails while the switch path
  is healthy.
- **DRBD:** the TCP session dies with the carrier, so DRBD disconnects, both
  hosts freeze their I/O (`resource-and-stonith`) and both run the fence-peer
  handler. Replication then reconnects over the switch path (the active-backup
  fallback).
  - **A carrier flap of a few seconds:** the handler sees the peer's DRBD port
    answer and waits up to about 4 s. If DRBD reconnects in that window,
    writes resume replicated: a stall, no divergence.
  - **A longer loss** (DRBD's own reconnect over the switch takes about
    10 s): the keeper's handler stops waiting and resumes serving (a new data
    generation); the non-keeper yields and stays frozen. DRBD refuses the
    reconnect while the non-keeper is still Primary ("I shall become
    SyncTarget, but I am primary"). Handler runs are serialized and later runs
    keep the yield, so the two never both write: no fork, no lost
    acknowledged write.
- **Serves:** the keeper, throughout. The non-keeper serves nobody. Once DRBD
  has refused a reconnect after its yield was recorded (its copy is only
  behind the keeper's, no fork) and the keeper's iSCSI portal answers over the
  switch path, the non-keeper steps down through the split-brain crash
  boundary: it stops its guests at the hypervisor, discards, resyncs from the
  keeper over the switch path, and its guests are restarted on the keeper (or
  on itself once it has rejoined, for a guest the keeper cannot take). No
  reboot. If the keeper's portal does not answer, the non-keeper holds frozen
  and alerts after 5 minutes; with pool HA on, XHA's statefile watchdog
  reboots it after about 105 s and it rejoins as a sync target.
- **VMs/HA:** VMs on the keeper stall for the handler's few seconds. VMs on
  the non-keeper are frozen until the step-down, then stopped and restarted
  on the keeper. In testing (arbitrated pair, HA on, cable down 60 s and
  180 s, on either end) they were frozen 70-77 s, within the XHA statefile
  watchdog, and every guest was running again 100-170 s after the cut, with
  no reboot and no acknowledged write lost. The step-down happens while the
  cable is still out.
- **Redundancy:** the keeper's copy only, until the non-keeper resyncs; one
  replication path (the switch) until the cable returns.
- **Operator:** `twinstor_replication_fallback` is raised after a 2-minute
  debounce (and again daily), and `twinstor status` shows
  `repl path: switch`. Replug the cable; the daemon steers replication back
  (steered reconnects and the guarded automatic failback), or run
  `twinstor failback-replication` to force it. For a planned cable swap, move
  the VMs off the non-keeper first. DRBD must run over both paths; a
  cable-only DRBD would split here. See
  [Known limitations](#known-limitations).

### 19. Both host-to-host paths down while the gateway stays up

- **Trigger:** the rare case that breaks the invariant: the peer is
  unreachable by every path, yet both hosts reach the gateway (for example two
  access switches with a dead east-west link, or a stack split).
- **Behaviour:** DRBD splits; both fence-peer handlers reach the gateway and
  keep serving. Both diverge, and the XHA statefile diverges and may fence
  unpredictably.
- **VMs/HA:** a bounded split-brain, resolved with an alert (case 21), with
  real downtime and loss of the loser's recent writes.
- **Redundancy:** restored after resolution; no silent corruption.
- **Fenced host returning (boot gate):** if XHA fences one side during the
  split and it reboots while replication is still down, it does not promote
  alone after its startup connection wait as long as it sees positive
  evidence that the peer is alive (the peer's iSCSI portal answers, or its
  local xhad liveset shows the peer live). It holds with no time limit, keeps
  a reconnect pending, alerts after 5 minutes, and rejoins as a sync target as
  soon as replication returns. Having seen the peer serving, it keeps holding
  even if the peer later disappears; the alert gives the command that makes it
  serve if the peer is lost for good (see
  [Known limitations](#known-limitations)). Without this gate, a returning
  host promoted alone against a peer that was serving every guest and became
  a second writer (observed in testing, HA armed, 5-minute replication cut).
  With no evidence of a live peer the normal timers apply (case 8).
- **Operator:** fix the topology so the peer and the fence target share a path
  (fate-shared), or add the direct cable so a single link cannot split DRBD
  (arbitrated). This case needs both links down, which is far rarer than a
  single switch failure.
- **See:** NETWORK_TOPOLOGY_SAFETY.md, "Why multi-path topologies reopen the
  window".

## Split-brain

### 20. StandAlone clean rejoin (no divergence)

- **Trigger:** B crashed, lost power or rebooted and served nobody while
  down; on return DRBD is `StandAlone`.
- **Behaviour:** no split-brain marker. After `standalone_grace_secs`
  (default 30 s) the daemon tries a non-destructive `connect`, which succeeds
  and resyncs B from A.
- **Serves:** A throughout; B returns to `ready`.
- **Redundancy:** restored, nothing discarded. **Operator:** none. This is the
  common path.
- **During bring-up:** the reconnect also runs while the host is still
  bringing storage up (Init), not only once Online. A host that boots
  `StandAlone` with an `Inconsistent` replica (fenced mid-resync) reconnects
  and, on a split-brain handshake, resolves as the loser from bring-up: its
  local copy is an interrupted resync, already invalid, so the discard loses
  nothing. Bring-up never resolves as the winner.

### 21. True split-brain, winner decidable

- **Trigger:** both hosts wrote independently while disconnected; DRBD
  disconnects and the handler drops the marker `/run/twinstor/split-brain`.
- **Choosing the winner:**
  - From the DRBD generation lineage first: the side that is ahead keeps. On a
    true fork, the side with more unreplicated writes keeps. A host that
    crashed while Primary is weighed without its activity-log marks (DRBD
    marks up to about 4.8 GiB out of sync at such a restart, written or not).
  - A lineage verdict is acted on at the first tick after DRBD declares the
    split-brain, with no reconnect grace.
  - Without lineage evidence, the winner is the pool master. The answer must
    stay stable across a 45 s sampling window, so an in-flight failover
    settles first.
  - The winner keeps its data and is the sync source.
- **Crash boundary on the loser.** When the lineage shows a true fork (or
  cannot exclude one) and the loser's target is exported, the loser stops its
  guests before it touches its target, its loopback session or DRBD. Without
  this, multipath would move still-running VMs onto the winner's disk history
  while their memory held the discarded one: silent cross-file corruption that
  fsck does not see. The steps:
  1. List the VMs on this host with a disk on the TwinStor SR, record them
     durably, and try to disable the host (best effort: with pool HA armed,
     XAPI refuses to disable a host of a two-host pool).
  2. Stop, at once and locally, every guest domain the local scan (`xl` plus
     the block backends in xenstore) finds using the SR, including one XAPI
     does not attribute to this host. First a hypervisor poweroff (dom0's
     `xen.lowlevel.xc`, reason poweroff): the guest stops at once, and
     xenopsd collects it as a guest shutdown with its own disk teardown and
     never rebuilds it in place. (A destroy outside xenopsd reads as a crash,
     which the VM's crash action answers with an in-place restart.) Then
     `xl destroy` for a domain still running, for a powered-off domain
     xenopsd has not collected after 8 s, or for every domain when the binding
     is missing.
  3. XAPI is consulted for at most 5 s, in parallel; the whole boundary plus
     the discard take seconds. Until the loser discards, the split goes on,
     and with pool HA armed the XHA statefile on the SR is split too (see
     [Known limitations](#known-limitations)). XAPI's list is cross-checked
     against the scan, so a scan that does not understand the host holds
     instead of reading as "no guests".
  4. Once the scan is empty, release the disk handles the destroyed guests
     may have left when xenopsd's own teardown did not run: their datapaths
     in XAPI's storage layer (`DP.destroy`: the deactivate and detach of a
     normal shutdown, tapdisk close included), with `tap-ctl destroy` only as
     a fallback for a tapdisk still open. The close must happen before the
     discard (it writes the cached VHD metadata), and a stale datapath would
     make a later start on this host reuse the old attachment.
  5. Only when the scan is empty and no tapdisk on the SR remains, re-read
     just before the first admission change, stop the iSCSI target and
     `connect --discard-my-data`. If that last read finds the SR in use again
     (a guest rebuilt in place after a destroy), the boundary is
     re-established at once rather than held.
  6. Right after the discard, read once more. A guest domain on the SR that is
     running and already existed when this copy was withdrawn (its domid is
     not above the highest one present right after the demote) may have run
     across the discard. It is stopped (through XAPI first, so pool HA leaves
     it down; at the hypervisor if XAPI does not answer, and the alert then
     says HA may restart it), dropped from the owed restarts, and alerted with
     its VDIs and the checks to run before an operator starts it. A domain
     created after the withdrawal reaches the SR only through the winner's
     portal and is left running. Neither check runs when the round ended with
     the peer refusing and this host claiming the win (nothing was discarded;
     the owed restarts then wait for the end of the episode, on this host).
- **No boundary needed:** when the lineage shows no divergence (this host is
  simply behind), or for a bring-up loser that never exported its target.
- **Yielded loser:** a loser whose fence-peer yielded runs the boundary when
  its target is exported. Its guests' writes froze in flight, and lifting the
  suspend without stopping them first would complete those writes and then
  discard them under the running guests. Its suspend is lifted only once its
  guests are stopped and no domain on this host runs or is being built with
  an SR disk (the local scan decides; an unreadable XAPI storage layer does
  not block it), and only to close what they left open. It is suspended again
  as soon as a guest is found being started here, and before each boundary
  attempt returns, whether it clears or holds; only the discard's own resume
  outlasts it. A yielded copy found running before an attempt (the daemon
  killed during a lift) is suspended again first, and leaked datapaths are
  not retried while the copy is suspended.
- **VM restarts:**
  - TwinStor starts the stopped VMs on the winner as soon as the discard is
    under way, one per tick, on the winner's XAPI host, addressed by identity
    (protected or not).
  - When XAPI took the background hard-stop, it recorded an intended stop and
    pool HA leaves the VM to TwinStor. When only the hypervisor poweroff
    stopped it, XAPI sees a guest shutdown and pool HA may restart a protected
    VM itself on an enabled host, possibly the loser (with pool HA armed the
    loser usually cannot be disabled; the checks just before and just after
    the discard cover that). TwinStor's own start then finds it running.
  - A start that keeps failing is alerted after 10 attempts and retried every
    15 minutes; it is never dropped.
  - A VM is started on the loser host itself only once that host has rejoined
    after its discard, only when the winner cannot attach its disk or keeps
    failing to start it (two attempts), and only once its disks have no stale
    datapath (released first) and no leftover tapdisk. A tapdisk left from
    before the discard is never closed (its close would write the old VHD
    metadata into the resynced disk), so that VM stays stopped and the alert
    gives the manual procedure.
  - The VMs reboot from the authoritative copy: a crash and reboot, not a live
    rollback. Their writes made during the split are lost.
  - A priority-1 `twinstor_split_brain` alert names them, and a second one
    reports the restarts. The host is re-enabled automatically once its
    storage is healthy.
- **Crash-boundary hold:** if any step cannot be completed or confirmed (the
  local scan is unreadable or disagrees with XAPI, a domain survives two
  destroy rounds, a tapdisk on the SR stays open, the record cannot be
  written), nothing is discarded. Both copies stay intact, the loser keeps
  serving its own copy (as in case 22), retries every minute and completes by
  itself once the cause clears. A priority-1 alert names the failed step and
  the manual fix. VMs already stopped are started on the winner during the
  hold when the lineage decided the loser; when divergence could not be read
  they stay stopped until the episode ends (the alert says which).
- **Manual resolution (`twinstor discard-and-resync`),** on the host whose copy
  is given up (this case's hold, or case 22):
  1. Stop its VMs on the TwinStor SR.
  2. `systemctl stop twinstor`.
  3. `twinstor discard-and-resync`. It refuses while the daemon runs, while
     any VM on the host still uses the SR, when DRBD is neither StandAlone nor
     marked split-brain, or when the generation lineage says this host holds
     the copy to keep (unless `--discard-kept-copy` is given). It asks you to
     type DISCARD unless `--yes` is given. Just before acting it re-checks
     every precondition (the prompt may have waited) and disables the host in
     XAPI when allowed. It records the request durably, withdraws the target,
     demotes, and reconnects discarding the local copy; the resync starts as
     soon as the peer accepts the connection.
  4. `systemctl start twinstor`.

  Until the resync has started, the daemon keeps this host down (Secondary, no
  target) and re-arms the discard offer, across restarts and reboots, so the
  discarded copy is never promoted or served again. If the kept host sits
  StandAlone (case 22), a plain `drbdadm connect twinstor-<resource>` there
  lets it accept the connection. To cancel a pending request, delete
  `/var/lib/twinstor/discard-requested`.
- **Winner-side check:** a VM on the winner could only have seen the losing
  history through the peer's portal. If the winner's local storage path was
  not confirmed active at some probe since the pair was last redundant, the
  winner raises an alert naming its VMs when the episode ends, however it
  ends (no automatic action).
- **Redundancy:** restored after the resync; the loser's diverged writes are
  lost, in exchange for a single agreed copy.
- **Operator:** check the VMs that ran on the loser during the partition (they
  were rebooted and lost their writes made during the split).
- **See:** FAQ.md "StandAlone recovery"; CONVERGENCE.md "The loser crash
  boundary"; [RECOVERY_CRASH_BOUNDARY.md](RECOVERY_CRASH_BOUNDARY.md).

### 22. True split-brain, winner undecidable

- **Trigger:** a split-brain where the daemon cannot read a stable pool master
  (XAPI unreachable, or the answer flips within the 45 s window).
- **Behaviour:** the daemon refuses to discard; both copies are kept intact,
  and each host keeps serving its own copy locally.
- **VMs/HA:** a `twinstor_split_brain` alert is raised for manual review.
  With HA armed, the XHA statefile split tends to fence one side, which bounds
  the divergence.
- **Redundancy:** none; the cluster waits rather than guessing.
- **Operator:** once XAPI is reachable, pick the authoritative host, then on
  the other host follow the manual resolution of case 21
  (`twinstor discard-and-resync`).
- **Mastership read:** live XAPI first, then the local `pool.conf` role, so a
  wedged slave-to-master XAPI connection does not block resolution by itself.
  This case is reserved for a master that is unreadable or unstable.
- **See:** `xapi.rs` (`is_pool_master_resilient`).

## Readiness, placement and HA arming

### 23. Host reports not-ready

- **Trigger:** the local replica is not safe to serve: not `Primary`, not
  `UpToDate` (resyncing), disk detached with no healthy peer, or toolstack not
  converged (`host.enabled=false`).
- **Behaviour:** `not-ready`. The daemon does not advertise the local target,
  so this host is not a usable path and placement avoids it. The other host
  serves.
- **VMs/HA:** do not place VMs here; HA should not fail VMs onto a
  `not-ready` host.
- **Redundancy:** depends on the cause.
- **Operator:** wait for it to converge (resync finishing, toolstack
  settling); investigate if it persists.
- **See:** VM_PLACEMENT_SAFETY.md (host storage readiness); `readiness.rs`.

### 24. HA disarmed past the grace period

- **Trigger:** HA left disabled (maintenance not re-armed) for longer than
  about 30 minutes.
- **Behaviour:** storage is unaffected; hosts keep serving.
- **VMs/HA:** **no fencing authority**: a partition could corrupt data, and a
  dead host's VMs would not restart.
- **Redundancy:** data redundancy may be intact, but the safety guarantee is
  not.
- **Operator:** re-arm HA (`xe pool-ha-enable`). The daemon raises
  `twinstor_ha_disarmed` after 30 minutes.
- **See:** VM_PLACEMENT_SAFETY.md (XHA armed).

### 33. XHA re-admission join deadlock after a fence storm

- **Trigger:** two staggered HA daemon restarts after fences. Each host's
  stale liveset claim blocks the other's start, and both cycle through 120 s
  join timeouts.
- **Symptom:** `xe` reports "The server could not join the liveset because the
  HA daemon failed to start", possibly pool-wide, while storage is healthy.
- **Behaviour:** the daemon detects the signature (host state stuck
  non-Online for 5 minutes, plus the join-timeout log pair). If the storm is
  over and storage is healthy, it runs the quiesce-and-sync-start recovery
  automatically (both HA daemons quiesced for over 90 s, then one
  synchronized re-arm that keeps the HA timeout), at most once an hour. While
  the storm persists it alerts "fix the link first". `[ha]
  readmission_assist = false` keeps detection and the alert only.
- **Operator:** normally none. The manual procedure (the join deadlock) is in
  FAQ.md.
- **See:** NETWORK_TOPOLOGY_SAFETY.md, "The join-deadlock variant".

### 34. Mutual-yield recovery (both fence-peers yielded)

- **Trigger:** a storm leaves both fence-peers without a gateway; both freeze
  and latch as the loser. This is a safe hold: nothing is discarded.
- **Recovery:** the operator clears the chosen winner's yield markers (the
  alert gives the command), or a verdict flip picks it. From there it is
  automatic: plain reconnects clear any stale kernel discard intent, the
  loser's parked offer is taken, and a winner that converged while demoted
  re-enters bring-up to re-promote and restart its target.
- **Operator:** one decision (pick the survivor); no `drbdadm` surgery.

### 35. Single-breaker rack: one host loses power, the switch with it

- **Trigger:** host A and the only switch share a breaker, and it trips. B
  survives but is fully isolated (peer and gateway gone at the same instant;
  in arbitrated mode the direct link is dead too, because the peer is off).
- **Behaviour:** B cannot tell this from its own isolation and does the safe
  thing: fence-peer yields (freeze), XHA self-fences it, and its reboot holds
  in bring-up (no gateway, no promote). Nothing serves during the outage
  (clients were behind the dead switch anyway), so the copies cannot diverge.
  When power returns, the hosts rejoin with nothing discarded.
- **Modes:** same outcome in both; the direct link only helps when the peer
  survives the switch.
- **Operator:** none for recovery. The real fix is rack wiring (separate
  circuits).
- **Validated:** on hardware, both modes (staged single-breaker trip).

### 36. XHA daemon dead after boot raced storage recovery

- **Trigger:** a fenced host reboots while the shared SR is still unreachable
  (the peer's target is down, mid-incident). xhad's statefile attach fails at
  start and XCP-ng never retries it, so the host runs without HA (no fencing
  protection, no liveset membership) while everything else looks healthy.
- **Behaviour:** the readmission watchdog notices xhad dead and silent (no log
  growth over a rolling 5-minute window) with storage healthy, alerts, then
  restarts it locally (`ha_readmission_revive`).
- **Operator:** none. Manual equivalent: `systemctl restart xhad` once storage
  is healthy.
- **Validated:** the revive logic is unit-tested and the restart itself was
  proven on hardware. The automatic trigger (a race of about 30 s) has not
  been reproduced in four staged attempts.

### 37. Total switch blackout: both hosts lose the switch network, power stays on

- **Trigger:** every switch dies at once (shared power, a switch without UPS
  during an outage) while the servers stay up.
- **Arbitrated:** both hosts stay up and hold. Each reads the peer's gateway
  state over the direct-link beacon, finds that neither side can reach
  clients, suppresses its XHA statefile fence (local `ha-disable`), and keeps
  its VMs running. The VMs' guest network is on the dead switch, so they are
  unreachable, but no VM goes down and nothing fences. When the switch
  returns, each host re-arms HA automatically and the pool converges without
  an outage.
- **Fate-shared:** there is no direct-link beacon (the same loss also cuts
  replication), so XHA liveness dies pool-wide and both hosts fence-reboot
  (between about 40 s and 2 minutes after the loss). Their bring-up holds (no
  gateway, no promote). When the switch returns, the pool reassembles unaided
  in about 5 minutes, including an empty-divergence loser discard. A host,
  typically the master, can boot into maintenance mode because of an xapi
  timing race; the host auto-enable watchdog clears it.
- **Operator:** none.
- **Validated:** the fate-shared fence path on hardware with boot-persistent
  staging, and reproduced independently on another pool with matching
  numbers. The arbitrated hold on hardware (both ports down: no fence, no
  stand-down, VMs untouched, clean convergence on re-enable).

### 38. Stale serving Primary (SyncTarget-while-Primary refusal)

- **Trigger:** a host promotes while its peer sits StandAlone between
  reconnect windows (a lone-survivor boot against a survivor that is not
  listening), writes nothing, then meets the peer, which has moved ahead.
  Every handshake agrees on the roles (no split-brain, no marker), but the
  kernel refuses to make a serving Primary the SyncTarget, so plain reconnects
  abort forever.
- **Behaviour:** after 3 aborted reconnects in a row, with the kernel's refusal
  line fresh in the episode and the peer's portal answering, the daemon
  demotes in place: target down, Secondary, reconnect, catch-up resync,
  re-promote, target back (about 82 s measured). The handshake's own verdict
  is the data-safety proof: a stale side with writes of its own would have
  produced a split-brain verdict instead.
- **Operator:** none.
- **Validated:** found by compound fault injection and validated on hardware;
  the side that was ahead stayed untouched throughout.

### 39. Isolated pool master: switch path lost, direct link intact (arbitrated)

- **Trigger:** the current pool master loses only its switch-side link (port,
  cable, SFP, NIC) while the back-to-back link keeps replication and both
  statefile copies healthy.
- **Why it needs handling:** otherwise the isolated master keeps mastership,
  and HA consolidates the VMs onto the host clients cannot reach.
- **Stand-down** (`[ha] self_standdown_isolated_master`, default on): once the
  isolation is confirmed (client network unreachable, peer alive and
  `UpToDate` on the replication link) for the stand-down window, the isolated
  master makes its own storage unavailable. Window: 10 minutes, with an alert
  at 5 minutes; 60 s for a master running no VMs. In order, it:
  1. leaves HA cleanly: hard-stops any local guests and disables its local
     xhad (`host-emergency-ha-disable --force`);
  2. stops its iSCSI target, logs out its initiator sessions, and demotes to
     `Secondary`.

  Its VMs then cannot run or write the shared LUN by any path (neither its own
  target nor the peer's target over the direct link through multipath), and
  XHA restarts them on the reachable peer. Disabling the local xhad first
  matters: logging the initiator out also cuts this host's only path to the
  HA statefile (an LV on the same SR), and an armed xhad's statefile watchdog
  would self-fence the host after about 105 s. Leaving HA first removes that
  watchdog, so the stand-down completes in one pass with no reboot. The peer
  removes the host from the liveset as a clean leave, not a fence, and
  restarts its protected VMs.
- **Interlock:** the handoff is interlocked by the HA statefile, not by
  timing. Cutting storage stops the VM writes and the statefile heartbeat
  together, and the peer restarts the VMs only once it sees that heartbeat
  stop, so the isolated host is out of the storage before the peer starts
  them. A stand-down only ever removes a writer, never adds one.
- **Rejoin:** a persistent marker keeps the host parked across reboots while
  it is still isolated, so it can never come back and re-run the VMs. It
  rejoins automatically, still `UpToDate` (it stayed Connected as Secondary),
  once its client network has been back for 45 s (so a blip cannot re-promote
  it) and the peer is reachable. It re-arms HA by restarting only its xhad
  (`ha_start_daemon`), never a full toolstack restart, which would replug the
  SR and briefly push guest I/O onto the remote multipath path.
- **Stranded former master:** after the survivor's emergency master
  transition, the returning host can still read itself as pool master: a
  dual-master with healthy DRBD, which the StandAlone-keyed dual-master heal
  does not catch. It would sit `enabled=false`, not live, with no VMs, HA
  unable to re-arm. The daemon detects this from local signals only (it reads
  as pool master; its peer is alive and `UpToDate` on the replication link
  yet reads not live in its own pool; it runs zero VMs, which rules out the
  survivor holding the VMs) and runs a bounded, guarded local
  `xe-toolstack-restart` so xapi demotes it to slave. This never asserts
  mastership, so it cannot create a new split. The forceful
  `pool-emergency-reset-master` escalation is described in CONVERGENCE.md
  ("Stranded former-master on rejoin").
- **Stuck xhad on re-arm** (`[ha] reconcile_stuck_xhad`, default on): after a
  fence or reboot, pool HA can read off (a fence emergency-disabled it) while
  the local xhad is still running and joined. Every `pool-ha-enable` then
  fails at once (its first step `ha_set_pool_state init` refuses while the
  daemon is present: `MTC_EXIT_DAEMON_IS_PRESENT`,
  `Xha_scripts.Xha_error(15)`), and the re-arm loops. On the pool master, when
  a re-arm is due (an obligation is pending, the peer is re-admitted, storage
  is healthy) and the mismatch has lasted past a settle window longer than a
  normal enable (120 s), the daemon stops the local xhad with
  `ha_stop_daemon` so the next enable starts cleanly. Control plane only; the
  automated form of "restart the toolstack on both hosts, then
  `pool-ha-enable`". It is most often seen after reconnecting a host's network
  while it is still rebooting on a marginal link.
- **With pool HA armed, XHA can act before the stand-down window ends.** The
  XHA heartbeat runs over the switch network, so it is lost at once. Past the
  XHA timeout (90 s by default in arbitrated mode), XHA's survival rule keeps
  one host and the other self-fences. XHA chooses which:
  - **XHA keeps the reachable host:** the isolated master self-fences and
    reboots, and its VMs restart on the peer. One host reboots.
  - **XHA keeps the isolated master:** the reachable host self-fences. The
    isolated master then loses its DRBD peer too, cannot reach its gateway,
    and yields (freezes its I/O). The HA statefile is on that frozen copy, so
    its statefile watchdog fences it too. **Both hosts reboot.** The VMs come
    back on the reachable host once it has rebooted, about 10 minutes after
    the cut in testing; the isolated master rejoins when its network
    returns.

  No acknowledged write is lost either way. In testing, the second outcome
  happened every time the keeper host was the isolated master.
- **Modes:** arbitrated only. The stand-down needs a replication path separate
  from the client network to tell "this master lost its switch" from a
  pool-wide outage. In fate-shared mode the same loss cuts replication, the
  isolated host yields and the survivor takes over (case 15), so the
  stand-down never triggers there; it stays detect-and-alert.
- **Operator:** usually nothing: reconnect the master's switch path when
  convenient and it rejoins. With the stand-down disabled, the manual fallback
  is an emergency master transition on the connected host and a power cycle
  of the isolated one.
- **Validated:** on hardware: the stand-down fires under a real switch loss
  with the production window, the peer stays the only writer, a rebooted
  isolated host comes back parked and never promotes, and the stand-down
  stays inert in fate-shared mode. On an arbitrated pair, a master with no VMs
  stood down in about 1 minute, before the statefile fence, with no reboot;
  the peer removed it from the liveset cleanly, and it rejoined `UpToDate`
  with the local multipath path preferred and HA re-armed, without a
  toolstack restart or a duplicate iSCSI session.

### 41. Isolated non-master host: switch path lost, direct link intact (arbitrated)

- **Trigger:** the host that is not the pool master loses only its
  switch-side link while the back-to-back link keeps replication and both
  statefile copies healthy (the mirror of case 39).
- **Why it needs handling:** the isolated host keeps the HA statefile alive
  over the direct link, so the master's XHA never drops it from the liveset,
  and its VMs would stay on the isolated host until the cable returned.
- **Behaviour:** once the isolation has held continuously for the reclaim
  window (about 2 minutes: client network unreachable, peer alive and
  `UpToDate` on the replication link), the isolated host stands its storage
  down as in case 39 (hard-stop local guests, target off, initiator logged
  out, `Secondary`), but without the local `ha-disable`. Its xhad stays armed
  and, losing its statefile heartbeat, self-fences once (the boot after the
  fence then disables HA locally to avoid a fence loop). The master's XHA
  restarts the host's protected VMs on the reachable host; the master stays
  master, and since the isolated host stopped its own VMs there is no
  double-run.
- **Rejoin:** unattended, once its client network is stably back: it
  re-registers the multipath WWID, reassembles the map, and re-forms the
  liveset.
- **Guards:** pool HA must be armed (with HA off nothing would restart the
  VMs, so stopping them would be worse), and the isolated host must be running
  VMs. The peer's gateway must be up (read over the direct-link beacon), so a
  whole-switch loss (case 37) stays a hold.
- **Modes:** arbitrated only, as for case 39.
- **Operator:** usually nothing: reconnect the switch path when convenient.
- **Validated:** on an arbitrated pair (non-keeper, non-master cut): the host
  stood down, the master's XHA restarted its VMs on the reachable host within
  the window, and it rejoined cleanly on replug. With both ports down
  (whole switch), no stand-down happened.

## Capacity, durability and data integrity

These failure modes build up slowly. They rarely matter in a staffed
datacenter where capacity is on a dashboard and disks are swapped on SMART
warnings, but they are the ones that hit an unattended edge box over several
years.

### 25. Backing store full (SR or backing LV out of space)

- **Trigger:** the SR fills up (thin-provisioned VDIs grow, snapshots
  accumulate, a runaway guest writes).
- **DRBD:** unchanged at the connection level; writes that cannot land return
  I/O errors up the stack.
- **Behaviour:** both copies fill at the same rate (synchronous replication),
  so there is no better host to fail over to. The pool master's daemon polls
  the SR's physical utilisation every 5 minutes and raises a
  `twinstor_degraded` alert at 80% (warning) and 95% (critical), re-raised every 24 hours while it
  lasts and cleared when usage drops below 80%. It does not gate serving on
  capacity.
- **VMs/HA:** VMs that hit `ENOSPC` may crash, pause or go read-only,
  depending on the guest. XHA does not restart them elsewhere: the storage is
  full pool-wide, no host is down.
- **Redundancy:** nominally full (both copies equally full), but the SR is
  unusable.
- **Operator:** act on the warning: reclaim space (delete snapshots or VDIs)
  or grow the backing device. Out-of-band monitoring (XO,
  `xe sr-list params=physical-utilisation,physical-size`) remains useful.

### 26. Abrupt power loss (write durability)

- **Trigger:** a host is fenced or loses power just after a guest write was
  acknowledged. On commodity SSDs the acknowledgement can come while the data
  is still in the drive's volatile cache.
- **DRBD:** replication is synchronous (protocol C): an acknowledgement means
  the write reached both hosts' DRBD layer. The remaining risk is the local
  drive cache below DRBD.
- **Behaviour:** at startup and about every 60 s, the daemon checks each
  backing drive for a volatile write cache and disables it where one is
  reported (re-applied after every boot, because NVMe caches revert on a
  controller reset). Enterprise drives with power-loss protection report no
  volatile cache and keep full performance.
- **VMs/HA:** an acknowledged (fsync'd) write is on persistent media before
  the acknowledgement, so a fence or power cut does not silently lose data the
  guest believed durable.
- **Redundancy:** restored after the normal post-outage resync (case 8).
- **Operator:** none for the automatic path. If disabling the cache fails on a
  drive (logged as a warning), accept the risk, disable it by hand, or use a
  power-loss-protected SSD.
- **See:** `write_cache.rs`. This underpins every power-loss case (8, 14, 15,
  16) on edge hardware.

### 28. Double disk failure (both backing disks dead)

- **Trigger:** both hosts' backing disks fail, or one host is already
  `Diskless` (case 9) and the other's disk then fails too.
- **DRBD:** both `Diskless`; there is no `UpToDate` copy anywhere.
- **Behaviour:** a diskless host only serves while it can forward to a healthy
  peer. With no healthy disk on either side, the daemon stops the iSCSI
  target so initiators fail fast rather than hang.
- **VMs/HA:** VMs lose storage: a full SR outage with nowhere to fail over.
- **Redundancy:** **total data loss** if both physical disks are really dead.
  TwinStor keeps two copies; both gone means gone (see the
  [README limits](../README.md#limits)).
- **Operator:** restore from backup onto rebuilt disks. There is no in-product
  recovery from losing both copies.
- **See:** case 9.

### 32. Silent corruption / bitrot (no scheduled scrub)

- **Trigger:** a block decays on media without the drive reporting an error,
  plausible on consumer flash left powered for years.
- **DRBD:** transfers are checksummed, but TwinStor does not schedule an
  online verify. The resource sets `verify-alg crc32c`, so
  `drbdadm verify` works when you run it; a resource file created by an older
  release lacks it, and the daemon then says to run
  `twinstor regenerate-config` on each host. No `csums-alg` or
  `data-integrity-alg` is set. Silent on-media divergence between the copies
  is not detected.
- **Behaviour:** states look healthy; both hosts keep serving. A corrupt block
  is served as is.
- **VMs/HA:** undetected at the storage layer; it shows up only as corruption
  inside a guest.
- **Redundancy:** two copies, unverified; they may silently disagree.
- **Operator:** to detect it, schedule `drbdadm verify <resource>` in a low-I/O
  window (for example a monthly cron job) and watch for out-of-sync blocks. A
  mismatch then needs manual reconciliation. A plain disconnect and reconnect
  discards the verify's findings; resync them from the host that ran the
  verify with `drbdsetup invalidate-remote <res> <peer> 0 --reset-bitmap=no`.
- **Status:** no automatic scrub (see
  [Known limitations](#known-limitations)).

## Lifecycle: boot, install, upgrade, long-term operation

### 27. Brownout / repeated boot interruption

- **Trigger:** unstable site power: a host starts to boot or resync, power
  drops again before it settles, repeatedly.
- **DRBD:** on every boot, a host honours the startup connection wait before
  promoting (`wfc-timeout` 90 s with a healthy pair, `degr-wfc-timeout` 10 s
  when it knows the peer was already down; the daemon's ceiling is 100 s). It
  defers its promote while DRBD is still `Connecting` (`drbdadm
  wait-connect`), so it never races the peer into two lone primaries. A
  resync is restartable: an interrupted one resumes where it stopped on the
  next boot.
- **Serves:** whichever host settles first serves once `ready`; the other
  joins when power holds long enough.
- **VMs/HA:** VMs start once a host is `ready`. Repeated interruptions delay
  readiness but do not corrupt data (the deferred promote and
  connect-before-write logic of cases 4 and 5 applies on each cycle).
- **Redundancy:** restored once power is stable long enough for a resync to
  finish.
- **Operator:** none required. A UPS that rides through brownouts removes the
  churn.
- **See:** cases 4, 5; `should_defer_promote` in `state_machine.rs`.

### 29. Long-term single-host operation

- **Trigger:** the peer is dead (case 6) and, with no staff on site, stays dead
  for days or weeks until a scheduled visit.
- **DRBD:** the survivor runs alone, `Primary`/`UpToDate`/`StandAlone`.
- **Behaviour:** `serving-without-redundancy`, indefinitely. Nothing degrades
  over time: the daemon does not demote, throttle or stop on its own. It
  re-raises the `twinstor_degraded` alert every 24 hours so the condition is
  not forgotten.
- **VMs/HA:** the pool keeps running on one host. HA has nowhere to fail over
  to, so a second fault is a full outage.
- **Redundancy:** none until the peer returns; the survivor is a single point
  of failure, and the only copy (case 26 matters here).
- **Operator:** treat the daily degraded alert as the trigger for a site
  visit; the longer the window, the longer the exposure. The returning peer
  follows case 20 (clean rejoin) or case 11 (disk replacement).
- **See:** case 6; `DEGRADED_REALERT_AFTER` in `state_machine.rs`.

### 30. Upgrade / version or kernel-module skew

- **Trigger:** the two hosts run different TwinStor versions or different
  DRBD kernel modules, for example during a rolling upgrade or after a
  half-applied update.
- **DRBD:** versions with compatible userspace replicate normally. If a DRBD
  protocol or module incompatibility prevents the connection, DRBD fails to
  connect.
- **Behaviour:** a host that cannot connect to its peer behaves like a host
  with a dead peer: the reachable host serves as
  `serving-without-redundancy`, the other is `not-ready`/`StandAlone`. Each
  daemon publishes its version in its XAPI host record
  (`other-config:twinstor-version`); the daemon alerts when the two differ
  for more than 1 hour (30 minutes when the peer runs 0.6.x, see the upgrade
  note in [Known limitations](#known-limitations)). A DRBD kernel-module skew
  is not detected as such; it shows up as a peer that does not reconnect.
- **VMs/HA:** the pool keeps serving on the reachable host. An incompatible
  peer cannot establish replication, so there is no version-specific
  corruption path.
- **Redundancy:** none while the versions are skewed and not connecting.
- **Operator:** finish the upgrade so versions and modules match; the peer
  then rejoins via case 20. Upgrade one host at a time and confirm the
  reconnect before touching the second. `twinstor upgrade-apply` enforces
  this: it refuses to restart while the peer is unhealthy or itself
  restarting, and blocks until this host reconverges, so a rolling upgrade
  cannot restart both daemons close together.
- **See:** FAQ.md "How do I upgrade twinstor?".

### 31. Partial setup failure (one host succeeds, the other fails)

- **Trigger:** `twinstor setup` completes on one host but fails part-way on
  the other (disk error, network cut, an LVM or SR step fails).
- **Behaviour:** `twinstor setup` can be re-run (it detects and reuses a
  resource that is already up) but does not roll back a failure: config files,
  DRBD metadata, LVM filter changes, firewall rules and the enabled service
  can be left in place from the aborted attempt.
- **Serves:** the cluster is not yet a healthy pair: one host may be `Primary`
  and waiting, the SR may not exist, and the second host's iSCSI target may be
  half-configured.
- **VMs/HA:** do not place workloads until both hosts report `ready` and the
  SR exists on both; do not rely on HA during setup.
- **Redundancy:** not yet established.
- **Operator:** re-run `twinstor setup` with the same parameters, or run
  `twinstor uninstall` to clear the partial state and start again. Check that
  `twinstor status` reaches `ready` on both hosts before going live.
- **See:** FAQ.md (setup, removal); `setup.rs`, `uninstall.rs`.

## Validation status

TwinStor is tested on two hardware pools, one fate-shared and one arbitrated,
with links cut at a managed switch and hosts powered off through smart plugs.
Guests write checksummed, sequence-numbered records during each fault, and a
verifier checks afterwards that every acknowledged write survived (see the
[README](../README.md#how-it-is-tested)).

**Validated on hardware** (as recorded in this document):

- Cases 1-24 broadly, through the fault campaigns, including:
  - a master power-pull failover and rejoin (cases 6, 20);
  - a simultaneous double power pull (case 8: all VMs restored in 2 min 28 s);
  - a 2 s carrier flap (case 13: a bounded split-brain auto-resolved in
    84 s);
  - direct-link cuts of 60 s and 180 s with HA armed (case 18);
  - the boot gate's trigger: a returning host becoming a second writer after a
    5-minute replication cut with HA armed (case 19);
  - the resilient master read against a wedged XAPI staged for the test (case 22);
  - three 8-minute master cable isolations exercising the full dual-master
    wedge ladder, including both survivor HA-reset branches; a slave cable
    isolation; a whole-switch loss with HA disabled (a freeze and resume with
    no split-brain marker).
- Case 26 (durability: a host fence under write load).
- Cases 35, 37, 38, 39, 41 and 42, as described in each case.
- Case 45 and the fate-shared HA timeout figures under Known limitations
  (single-host cuts of 45, 70 and 130 s).
- Case 36 in part: the xhad restart works; the automatic trigger has not been
  reproduced.
- Case 44 in part: the manual bounce it automates was validated; the
  split-brain an unbracketed bounce can cause was reproduced.

**Documented from the code and its unit tests** (no hardware run is recorded
here):

- Case 25: the capacity watchdog.
- Case 27: follows the validated cold-start path (cases 4, 5).
- Cases 29, 30 (version-skew alert), 31, 32.
- Cases 33, 34 and 43: the automatic paths.

## Known limitations

- **A rebooted host that saw its peer serving waits for an operator if the
  peer is then lost for good (a double fault).** A host that comes back while
  replication is still cut and sees its peer serving (case 19) does not serve
  its own copy alone until the two reconnect, across its own reboots too: its
  copy may be behind, and serving it could roll back the peer's writes. If the
  peer then dies for good while replication is still cut, nothing serves until
  an operator decides. The data is protected; availability is not. The alert
  names the command (`drbdadm primary --force twinstor-<resource>` on that
  host, after confirming the peer is gone for good); the daemon then brings
  storage up from that copy.

- **With pool HA armed, a split-brain that lasts past the XHA statefile
  timeout gets one host fenced.** The XHA statefile lives on the TwinStor SR,
  which is replicated. While replication is cut, and until the split-brain
  loser discards, each host sees only its own copy of the statefile and the
  other's slot goes stale. XHA's fault handling starts once the split has
  lasted the statefile timeout (`StateFileTimeout`, the pool's HA timeout; in
  testing with a 60 s timeout it started 60 s after the cut, to the second), and once
  started it self-fences one host about a minute later, either side,
  including the split-brain winner, whatever happens to the storage
  meanwhile. Every VM on that host crashes and is restarted by HA.

  So the whole episode has to end inside the timeout: the cut, DRBD's
  reconnect and split-brain detection after the link returns (about 15 s in
  testing), and the resolution. A split-brain that DRBD has declared and the
  lineage decides is resolved on the first tick, and the crash boundary and
  discard take seconds. In fate-shared mode this also covers a single host
  losing its link: that host keeps its guests (and its own XHA statefile I/O)
  off the peer's copy until the pair is Connected again, so the statefile
  copies stay apart from the cut until the resync (the cut plus 15-75 s in
  testing). This is why TwinStor sets a 120 s HA timeout on fate-shared pools
  (90 s on arbitrated pools). Pools set up with an older default keep their
  timeout; `twinstor doctor` recommends 120 s and prints the command.

  A side effect when the frozen host is the pool master: XAPI may not list its
  guests in time, so its crash boundary stops them from the local scan without
  scheduling their restart on the winner, and pool HA can restart one on the
  losing host. The check right after the discard stops it again and alerts;
  that VM waits for a manual start.

  In practice:
  - **Arbitrated mode:** a replication cut shorter than about the statefile
    timeout minus 15 s resolves before XHA acts. A longer one ends with XHA
    fencing one host (in one test with a 90 s timeout, at 197 s after the
    cut).
  - **Fate-shared mode, one host losing its link:** the margin is smaller,
    because the statefile copies stay apart for the cut plus the losing host's
    step-down and resync (15-75 s after the link returns). At the 120 s
    timeout, testing saw no reboot after 32 cuts of 45 s, a reboot after about
    half of the 70 s cuts, and after nearly all 130 s cuts. The reboot can come
    a few minutes after the pair looks healthy again, when XHA merges its two
    views of the pool, and it can hit the host that kept serving rather than
    the one that was cut. Nothing acknowledged is lost: the rebooted host's
    VMs crash and HA restarts them on the other host.
  - A longer HA timeout widens the window, at the cost of an equally slower VM
    restart when a host really dies. In a maintenance window, from the pool
    master: `xe pool-ha-disable`, then
    `xe pool-ha-enable heartbeat-sr-uuids=<TwinStor SR uuid> ha-config:timeout=<seconds>`.
    See [HA_TIMEOUT_TUNING.md](HA_TIMEOUT_TUNING.md).

- **A sustained replication-only cut with both gateways reachable still
  diverges.** Both hosts keep serving for as long as the cut lasts, or until
  XHA fences one of them (197 s observed with HA armed). On reconnect, the
  losing side's acknowledged writes for that period are discarded and its VMs
  are rebooted (case 21). Two hosts cannot tell an isolated but alive peer
  from a dead one without a third party; arbitrated mode's direct cable makes
  the cut much rarer, not impossible.

- **Fate-shared mode with freeze mode off: a host that loses its link serves
  for a few seconds before it yields, and those writes are lost.**
  `resource-only` fencing keeps I/O flowing while the fence-peer handler
  pings its targets; the host that finds none freezes only then, and the
  survivor wins the resulting fork (case 45). The yielded host's guests lose
  what they wrote in those seconds, as in a crash. Freeze mode, on by default
  (`/etc/twinstor/fate-shared-freeze-off` turns it off), freezes I/O at the
  disconnect and removes this window; it engages only when both hosts run
  0.7.0 or later. Arbitrated mode freezes at the disconnect and does not have
  this window.

- **Arbitrated mode: losing only the direct cable still interrupts the
  non-keeper's VMs.** DRBD drops the connection (the TCP session dies with the
  carrier) and both hosts freeze while the fence-peer handler decides. The
  keeper resumes within seconds; its new writes leave the non-keeper behind,
  so DRBD refuses to reconnect over the switch while the non-keeper is still
  Primary. The non-keeper keeps its VMs frozen rather than writing (no
  acknowledged write lost, no split-brain), then steps down through the crash
  boundary: its guests stop, it resyncs from the keeper over the switch path,
  and they restart on the keeper. In testing (cable down 60 s and 180 s, HA
  armed) the non-keeper's VMs were frozen 70-77 s and running again 100-170 s
  after the cut, with no reboot and no acknowledged write lost. The switch
  path does not carry the connection through without this reset. If the
  keeper's iSCSI portal does not answer over the switch path, the non-keeper
  holds frozen and XHA reboots it after about 105 s (case 18). A drop of only a
  few seconds is absorbed (a stall).

- **Arbitrated mode: a host that loses only its switch link is not failed
  over at once.** The direct cable keeps replication and the HA statefile
  healthy, so XHA does not see the host as dead. The daemon stands it down
  after a confirmation window (10 minutes for a pool master with VMs, 60 s
  for one without, about 2 minutes for the other host; cases 39 and 41), or
  XHA fences first.

- **Arbitrated mode: a switch isolation of the pool master that outlasts the
  XHA timeout can reboot both hosts.** When XHA keeps the isolated master, the
  reachable host is fenced; the isolated master then freezes its own copy (no
  gateway, no peer) and is fenced by its statefile watchdog. The VMs are down
  until the reachable host has rebooted and restarted them (about 10 minutes
  in testing). Nothing acknowledged is lost. See case 39.

- **Dom0 users of the SR are outside the split-brain crash boundary.** The
  loser stops guest domains before discarding. A backup, export or VDI copy
  attached in dom0 on the losing host while a split-brain is being resolved
  can read a disk history that changes underneath it, producing a bad backup.
  Avoid running such jobs while a split-brain alert is open.

- **Two copies only.** If both backing disks fail, the data is lost (case 28).

- **No background scrub.** Silent on-media corruption on one disk is not
  detected until it is read; `drbdadm verify` must be scheduled by the
  operator (case 32).

- **DRBD kernel-module skew is not detected as such.** It shows up as a peer
  that does not reconnect (case 30). TwinStor version skew is alerted.

- **Setup does not roll back.** A partial setup is recovered by re-running
  `twinstor setup` or by `twinstor uninstall` (case 31).

### Upgrade note: 0.6.x to 0.7.0 or later

Keep the mixed-version window short. 0.6.x and 0.7.0 weigh split-brain
evidence differently (0.7.0 discounts a crashed Primary's activity-log marks
and compares generations the way DRBD does), so any split-brain that forms
while one host runs each version, not only one present at upgrade time, may
resolve asymmetrically, including a host discarding the copy that kept the VMs
running. The health gate of `twinstor upgrade-apply` (Connected, both
UpToDate) only protects the moment of each restart.

- Upgrade the two hosts back to back with `twinstor upgrade-apply`, and do no
  network maintenance on the replication or management paths until both run
  0.7.0 or later. The 0.7.0 host alerts if the 0.6.x peer is still there after
  30 minutes.
- A 0.6.x host that loses such a split-brain has no crash boundary: it
  discards under its running VMs (the live history switch case 21 prevents).
- A 0.6.x host does not serve the split-brain yield claim (case 45). If the
  0.7.0 host keeps its gateway while the 0.6.x keeper yields, both can hold
  until the "split-brain wedge" alert's manual step.
- Freeze mode engages only once both hosts run 0.7.0 or later.
