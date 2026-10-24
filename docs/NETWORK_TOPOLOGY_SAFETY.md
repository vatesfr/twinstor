# Network topology safety

TwinStor runs on two hosts and has no third vote. When the hosts lose each
other, each one decides alone whether to keep serving, using the network's
gateway as the tie-breaker. This document states the network property that
decision depends on, shows the topologies that satisfy it and the ones that
quietly break it, and describes the two supported deployment modes,
fate-shared and arbitrated. It also covers what remains possible in each
mode, why TwinStor does not add a witness, and how XHA re-admission behaves
after a fence.

See also [VM_PLACEMENT_SAFETY.md](VM_PLACEMENT_SAFETY.md),
[FATE_SHARED_FREEZE_MODE.md](FATE_SHARED_FREEZE_MODE.md) and
[CONVERGENCE.md](CONVERGENCE.md).

## The invariant

When a host loses its DRBD peer, TwinStor's fence-peer handler keeps it
serving as long as it can still reach a ping target (the default gateway, or
a target listed in `/etc/twinstor/ping-targets`), and freezes it when no
target answers. That is the rule for a host that is already Primary; a
promote-time guard adds a third branch (see
[How the fence-peer handler decides](#how-the-fence-peer-handler-decides)).
Keeping a host serving on a reachable gateway is safe **if and only if**:

> A host that can still reach its fence target can also still reach its
> peer.

In other words, the peer and the fence target must share fate: reachable over
the same physical path, so that the peer cannot be lost while the gateway is
kept. When that holds, "I lost the peer but the gateway answers" cannot
happen, so the handler keeps a host serving only when the peer is really dead
or the host is really alone. When it does not hold, a host can lose its peer,
keep reaching the gateway, keep serving, and diverge from a peer that is also
still serving.

A single flat L2 segment with the gateway on it satisfies the invariant. Any
topology that gives the peer a different physical path from the fence target
breaks it.

## The keeper: a tiebreak without a third vote

Every safety mechanism below eventually meets the same question: when the two
hosts disagree and neither can prove which one is right, who acts? A
three-node cluster answers with a majority vote. TwinStor has no third node
and no external arbiter, by design: a site is expected to have only what an
ordinary two-host install already has, a gateway it can ping. There is no
witness to power, patch or keep online (see
[Why there is no witness](#why-there-is-no-witness)).

The missing vote comes from identity instead, which needs no communication
and no extra hardware. The two hosts are ordered by hostname, and the
lexicographically lower one is the **keeper** (on a pool created by
`twinstor setup`, it is also DRBD node-id 0). It is a static, local fact:
each host computes it from its own configuration, offline, and both reach the
same answer without exchanging a packet. That is the property a
partition takes away and a majority vote would otherwise restore.

### What the keeper decides

The keeper is not a primary. In normal operation either host can be the pool
master, either can serve, and DRBD is dual-primary; the keeper has no special
role and no say while the pair is healthy. It is a tiebreak of last resort,
consulted only after every evidence-based rule has come up undecided:

- **Split-brain resolution.** When a split-brain has formed, the fence-peer
  yield record and the DRBD generation lineage have not decided it, and
  neither side can read a live pool master (both fall back to `pool.conf`
  and both read "not master"), the keeper keeps its copy as the single sync
  source and the other host discards and resyncs. One pass, no handshake;
  see [CONVERGENCE.md](CONVERGENCE.md).
- **Dual-master stagger.** In a symmetric partition with no reboot, the
  non-keeper acts first, so the two sides never demote at once and leave the
  pool with no master.
- **Masterless recovery.** In a mutual-slave deadlock (each host's
  `pool.conf` follows the other, and `xe` is wedged pool-wide), only the
  keeper runs the emergency master transition.
- **Short replication cuts without freeze mode.** When the fence-peer handler
  sees the peer's DRBD port answering but the link has not reconnected after
  4 s, the keeper keeps serving and the other host yields (see below).

### Why it does not weaken data safety

The keeper only ever changes the keeper's own action, and only toward keeping
data. It is never the rule that decides to discard.

- It fires only on a **stable** "neither is master" reading. An unreadable or
  unstable verdict holds and alerts; it never discards on uncertainty. The
  worst a keeper misfire can produce is a "both keep" hold (data-safe and
  visible to the operator), never a "both discard".
- It adds no discard. In the symmetric case the non-keeper would discard
  either way; the keeper rule only changes the keeper from discard to keep.
  The data at risk is the same bounded set the loser alert already names.
- Real evidence wins over identity. If either host can read a live pool
  master, that answer overrides the keeper entirely, and a genuine loser
  still discards and converges on the real master.
- It is a local static rule, with nothing to power, patch, monitor or lose.
  It resolves a wedge that has already formed; it does not stand between a
  host and serving its own data.

Each host's static identity is the one thing a partition cannot corrupt, and
each side can trust the other to have computed the same answer. That lets a
two-host cluster converge deterministically without a third vote and without
enlarging the set of data it is willing to discard.

## What triggers a fence, and what does not

These points are confirmed in the code:

- **The fence-peer handler runs when DRBD loses the peer, and on a promote
  that wants an unreachable peer marked outdated.** It is a DRBD handler
  (`config/twinstor-fence-peer.sh`). If the link between the hosts is up,
  DRBD stays Connected and the handler never runs. It pings the gateway only
  after DRBD has already lost the peer.
- **Gateway loss alone never fences.** The daemon does ping the gateway: to
  alert when the tie-breaker is missing or unreachable, to detect an
  isolated host in arbitrated mode (which also requires the peer alive and
  UpToDate on replication), for the arbitrated whole-switch hold, and before
  a fate-shared lone survivor promotes. None of these fences a host because
  the gateway went away. The fence decision on peer loss is made in the
  handler only.
- **The XHA heartbeat runs on the management interface**, not the storage
  network and not the gateway (`xen-api`, `xapi_ha.ml`, `write_config_file`:
  the heartbeat interface is `_management_interface`).
- **The XHA statefile is on the TwinStor SR**, so it travels iSCSI, then
  LIO, then DRBD on the storage path (`src/setup.rs`,
  `pool-ha-enable heartbeat-sr-uuids=<TwinStor SR>`).
- **VDI activation is plain local LVM.** SM activates with `lvchange -ay` and
  there is no cluster lock manager (no sanlock, lvmlockd or dlm). Exclusive
  use of a VDI is enforced only by the XAPI pool master over the management
  network, which a partition severs. Nothing at the block layer prevents the
  same LV being activated read-write on both hosts at once.

Consequence of the last three points: in any split-network deployment, the
heartbeat (management) and the statefile (storage, DRBD) are separate failure
domains, and the statefile is itself replicated by DRBD. When the storage
path splits, the statefile splits with it: each host keeps a writable local
copy and reads itself as a valid survivor. XHA's usual guarantee, one shared
statefile that picks one winner, does not apply during the split. The
remaining tie-breaker is the fence-peer handler's gateway ping, which keeps a
host serving when the gateway answers.

## What happens, per fault

Assuming pool HA is armed and both hosts were healthy:

| Fault | DRBD | Heartbeat (management) | Fence-peer ping | Outcome |
|---|---|---|---|---|
| Gateway or router down, link between hosts up | Connected | up | not run | **No effect.** No fence. |
| Link between hosts down, gateway reachable only through it | split | down | fails (no target) | **Safe.** Both hosts freeze I/O and are fenced: a clean outage of the pool, no corruption. |
| Brief carrier flap (under about 3 s) | splits, reconnects | rides it out | succeeds on both | **No divergence in the normal case.** Each handler finds the peer's DRBD port listening (the flap has already healed) and holds its verdict up to 4 s so DRBD reconnects first; writes resume replicated. If the reconnect misses the hold, the result is a bounded split-brain the daemon resolves on its own (84 s in testing). Repeated flaps raise `twinstor_flapping_link` (3 episodes, or 5 disconnects, within 15 minutes). See "Network flap" in [FAQ.md](FAQ.md). |
| Link between hosts down, gateway reachable by another path, heartbeat on the dead path | split | down | succeeds | **Silent corruption.** The survivor's HA restarts the peer's VMs while the peer keeps writing: the same VDI with two writers. The resolver then propagates a corrupted winner. |
| Storage link down, heartbeat on a separate live path, gateway reachable | split | up | succeeds | **Split-brain and unstable XHA fencing.** Both hosts keep serving and DRBD diverges; XHA fences unpredictably (extra reboots); the resolver keeps one copy and the loser's diverged writes are discarded. Bounded and announced, no silent corruption, but real downtime and loss of the loser's recent writes. |
| HA disarmed | any | n/a | n/a | **Exposed.** No fencing authority at all; the daemon alerts after 30 minutes disarmed. See [VM_PLACEMENT_SAFETY.md](VM_PLACEMENT_SAFETY.md). |

Only the first two rows are fully safe, and both require the invariant.

## Why multi-path topologies reopen the window

The corruption case needs all traffic between the hosts to die while the
gateway stays reachable. A fault that kills only one kind of traffic is
caught by the other channel:

- IGMP snooping pruning a multicast heartbeat: DRBD (unicast) survives, and
  the statefile keeps replicating and contradicts the dropped heartbeat.
- MTU faults dropping large frames: the small heartbeat survives, so the
  survivor does not declare the peer dead.

So the danger requires the heartbeat and DRBD to lose host-to-host
connectivity together while the gateway survives. That is a property of the
topology, not of a single flaky link. Three ways to build it:

### 1. Two access switches with a dead east-west path

Host A is on switch 1, host B on switch 2, both uplinked to the core and the
gateway. East-west traffic crosses the inter-switch link; gateway traffic
goes north. The inter-switch link fails (or STP blocks it) while both uplinks
stay up. A and B lose each other (DRBD splits, the statefile splits, the
heartbeat drops) but both still reach the gateway northbound. Both fence-peer
handlers exit 4 and keep serving. On one flat switch this cannot happen:
breaking A to B also breaks A to the gateway.

### 2. Stacked switches, stack split

A common "redundant" design presents two switches as one logical stack. A
stack-link failure, or a member reboot during a firmware update, can split
the stack: each member keeps forwarding north to the core but loses the
east-west stitching. A and B land on different members, lose each other and
keep their uplinks: the same outcome as case 1. The redundancy added a new
failure mode (the stack split) that separates east-west from north-south.

### 3. Heartbeat and fence target on different paths

Splitting traffic across VLANs is usually safe by itself: if only the storage
VLAN fails, DRBD splits but the heartbeat (management) stays up, so the
survivor still sees the peer alive and does not restart its VMs. The result
is a split-brain the resolver handles, not two writers on one VDI. The
dangerous variant puts the heartbeat and the fence target on different
physical paths, for example the heartbeat on a low-latency storage network
while the fence-peer handler pings the management gateway. Then "storage path
down" means heartbeat down and DRBD down while the management gateway is
still reachable: the handler keeps serving, the survivor restarts the VMs,
and the VDI has two writers.

The rule for operators: **VLANs that share physical links are not
independent failure domains, and putting the heartbeat with DRBD while the
fence target lives elsewhere separates "heartbeat lost" from "gateway
lost."**

## How the fence-peer handler decides

The handler (`config/twinstor-fence-peer.sh`, log in
`/var/log/twinstor-fence-peer.log`) asks one question: can this host still
serve clients, meaning can it reach the network? Its ping targets are the
default gateway from the routing table and any addresses in
`/etc/twinstor/ping-targets` (set with `twinstor setup --ping-target`). One
answering target is enough. Each target gets one echo, and two more only if
the first is lost, with a 1 s timeout each.

- **Promote-time guard.** When this host is Secondary and the peer's DRBD
  listener answers a TCP connect on the replication port (the configured peer
  address, plus the direct link in arbitrated mode), the handler refuses to
  mark the peer outdated and exits 1, even though the gateway answers. The
  question is asked again after each successful gateway ping. The probe is
  the DRBD TCP port, not ICMP, so a host that answers pings with a dead
  storage stack never blocks a lone survivor.
- **A target answers: survive (exit 4).** DRBD resumes I/O and this host
  keeps serving. Before answering, two refinements apply:
  - If the peer's DRBD port answers, the link has probably healed under the
    handler, so it waits up to 4 s for DRBD to reconnect. If it does, writes
    resume replicated and nothing diverges.
  - If the link has not reconnected after 4 s, two survivors would make two
    writers. Without freeze mode, the keeper (lower hostname) survives. The
    other host checks that the keeper's iSCSI portal answers, waits up to
    12 s more for a reconnect, and then yields frozen. If the peer answers
    nowhere, it survives too, and the resolver cleans up afterwards. In fate-shared
    freeze mode there is no hostname keeper: the host yields frozen to a peer
    whose daemon answers, and the give-way decision follows where the guests
    ran ([FATE_SHARED_FREEZE_MODE.md](FATE_SHARED_FREEZE_MODE.md)).
- **No target answers: yield (exit 1).** The host cannot serve clients, so
  it keeps its I/O suspended rather than risk becoming a second writer. In
  arbitrated mode the peer's direct-link address is pinged only to label the
  log line (`yield to peer` against `isolated`); it never turns a yield into
  a survive. A host that kept the direct link but lost its switch still cannot
  serve clients, and a symmetric "survive if the peer answers on the cable"
  rule lets both sides write during a reconnect gap (in testing, that rule
  produced a split-brain).

A yield is recorded in `/run/twinstor/fence-yielded`, and the daemon's
split-brain resolver honours it:

- A host frozen since its own yield served nobody, so it resolves as the
  loser even if its local `pool.conf` still says it is the pool master.
  Without this, losing the pool master's client-facing path deadlocks
  recovery: XHA elects the peer, but the frozen former master's stale
  "master" role makes both sides keep their data as putative winner, and the
  real winner waits forever for a discard the frozen host never runs.
- The marker is first checked against the yield's I/O suspend, which is
  still in place (the proof the host served nobody). The verdict is then
  latched for the rest of the episode: a discard round can find nobody
  listening when the peer's resolver runs on its own timer, and without the
  latch the retry would fall back to the pool-role rule and invert the
  winner.
- While latched, the host holds itself down between rounds instead of
  serving stale data again. The surviving peer, whose discard handshake is
  refused (a discard offer can only be refused when both sides offered one),
  flips to winner because it did not yield and kept serving.
- The yield, the latch and the flip are cleared once the host serves with
  redundancy again, after a resolution converges, and by a reboot.

## The two deployment modes

The deciding factor is not the number of NICs on the host. It is whether the
replication path and the gateway (the fence target) sit in the **same
failure domain** (they fail together) or in **separate** ones (they can fail
apart):

- **A single switch or a single L2 segment** (edge, small office), or a
  redundant switch pair run as one L2 (an MLAG pair or a stack carrying
  everything on one segment): use **fate-shared**. Replication and the
  gateway fail together, so the invariant holds by topology. The MLAG case
  also removes the switch as a single point of failure, which often makes it
  the least surprising choice for a redundant site that does not need a
  separate storage network.
- **A storage network separate from the client and gateway network** (a
  storage plane on its own switches or fabric, spine/leaf, a separate
  25/40/100G storage VLAN): use **arbitrated**. Storage and gateway can fail
  independently, so the topology no longer guarantees the invariant and the
  direct link is required.

Spare NICs are not the trigger; a network that can cut storage off from the
gateway is. Fate-shared mode on such a network is the misconfiguration that
reopens the corruption window (see
[Why multi-path topologies reopen the window](#why-multi-path-topologies-reopen-the-window)).

### Fate-shared mode (default)

One L2 segment with the gateway on it; DRBD, the XHA heartbeat and the fence
target all on the same physical path. The invariant holds by topology, so the
corruption window cannot occur. This is the small office, retail or edge
setup on a single switch, and equally a site that runs a redundant MLAG pair
or stack as one segment with everything on it.

DRBD fencing: the generated resource file sets `fencing resource-only`, so
I/O continues while the handler checks the gateway. Freeze mode, on by
default when both hosts support it (0.7.0 or later on both), sets
`resource-and-stonith` live so I/O freezes at the disconnect on both hosts
until each handler decides; the side that cannot reach the gateway then has
nothing acknowledged that its peer lacks. See
[FATE_SHARED_FREEZE_MODE.md](FATE_SHARED_FREEZE_MODE.md).

On a genuinely single switch, that switch is a single point of failure. If it
dies, both hosts lose the peer and the gateway, both freeze and are fenced: a
clean outage of the whole pool, with VMs restarted on recovery and no
corruption. Run as an MLAG pair or stack, losing one switch keeps the segment
up, and only losing the whole L2 stops the pool, cleanly.

**A control-plane exposure, and how it is reduced.** The invariant protects
storage: the two hosts never write divergent data. It does not by itself
prevent a VM from running twice. If one host is down and the survivor then
loses the shared segment (isolated but still running), a returning host
cannot tell the survivor from a dead one: with one path there is no
independent signal, and the shared HA statefile goes stale. It would promote,
serve, and let HA restart VMs the survivor is still running, so the same VM
writes its disk from two hosts, corrupting the guest file system while DRBD
stays block-consistent.

The daemon reduces this: a fate-shared host that would promote alone (peer
unreachable, nothing to resolve) holds its storage Secondary and alerts for
10 minutes, so a peer that returns within that window reconciles with no
double run. After the window it serves alone only if it can reach its own
gateway or a ping target; a host that reaches none is the isolated side and
keeps holding. A peer that returns after the window can still collide.
Arbitrated mode avoids this case because the direct link keeps the peer's
liveness visible. Prefer arbitrated for the strongest protection against
this failure class.

### Arbitrated mode

Add a back-to-back link between the hosts and run DRBD over both paths in
active-backup mode (one path active, the other a hot standby; DRBD's default
for several paths). `twinstor setup --direct-ip <peer-direct-ip>` generates
it. The point of the link is not bandwidth: it gives DRBD a replication path
that does not share fate with the switch, so a switch failure neither splits
the data nor fences the pool.

The two-path configuration cannot be added to a running dual-Primary
resource (`drbdadm adjust` refuses it), so arbitrated mode must be chosen at
setup.

#### Replication prefers the direct link

The direct path is declared first in the generated resource, and the daemon
keeps replication on it:

- Synchronous writes and resyncs stay off the switch, which keeps carrying
  management, the XHA heartbeat, iSCSI and guest traffic.
- Switch faults (flaps, STP events, firmware updates, the common failures)
  never touch replication. In testing, with replication on the cable,
  repeated switch-path flaps produced no DRBD disconnect; with replication on
  the switch, the same flaps produced disconnects and split-brain episodes
  every cycle.
- **No dark standby.** The fallback (the switch path) is exercised all the
  time by every other kind of traffic, so its health is visible. The reverse
  arrangement leaves the cable unused until the moment it is needed.
- A cable failure fails over to the switch path automatically. While
  replication runs there the topology is temporarily fate-shared: the
  invariant holds, so the degraded mode is the safer mode, at the price of
  the switch being a single point of failure until the cable is fixed. After
  2 minutes on the fallback the daemon raises `twinstor_replication_fallback`
  (repeated daily), and `twinstor status` shows the path in use on its
  `repl path:` line.
- **Failback is the daemon's job.** DRBD has no path preference of any kind
  (no priority setting, and declaration order did not decide the path
  reliably on DRBD 9.2.16), so the daemon brings replication back itself:
  - Every reconnect the daemon starts is steered: the fallback path is
    removed while the connection is down and restored right after, so the
    reconnect lands on the cable.
  - When nothing reconnects on its own, the daemon runs a guarded failback:
    the side with fewer VMs briefly stops its target (its consumers continue
    over multipath), demotes, disconnects, reconnects on the cable, and
    serves again.
  - It does this only after a stability hold that grows with each flap-back:
    2.5 minutes on a first clean return, then 10 and 30 minutes. A cable
    that flaps back to the fallback three times latches failback to manual.
  - `twinstor status` and the alert say what the daemon is waiting for, and
    `twinstor failback-replication` runs the failback immediately (it skips
    the hold and clears the latch; the safety checks still apply).
  - **Before switch maintenance, check that `twinstor status` shows
    `repl path: direct link`.** If not, fix the cable, run
    `twinstor failback-replication`, check again, then proceed.
- Setup checks the cable first: a ping is required, and a full-MTU
  don't-fragment probe and the link speed give warnings, because a degraded
  but working cable would slow every synchronous write.

#### Active-backup, not `load-balance-paths`

TwinStor does not set `load-balance-paths`. Per the DRBD 9.2
`drbdsetup(8)` man page, the TCP transport by default "establishes only one
configured path at a time. It switches to another path only in case the
established one fails", whereas `load-balance-paths yes` "establishes all
paths in parallel." The latter is a throughput feature, not a failover one:
any path state change, down or up, even on the idle path, resets the whole
connection (`NetworkFailure`, then `StandAlone` and a resync), which on a
dual-Primary pair opens a split-brain window. In testing, pulling the direct
link with `load-balance-paths` reset the whole connection, split-brained,
discarded the loser's copy and restarted its VMs.

With active-backup:

- **Losing the standby path changes nothing.** In testing, pulling the
  standby path left the connection `Connected` throughout: no
  `NetworkFailure`, no `StandAlone`, no resync, no split-brain.
- **Losing the active path** (the direct link, since replication prefers it)
  makes DRBD switch to the standby, as a reconnect with a short gap rather
  than a seamless switch. Both gateways stay reachable, so both handlers run.
  Under active dual-primary writes the reconnect always finds unacknowledged
  in-flight writes on both sides, and the handler's tiebreak settles it: the
  keeper serves throughout, the other host yields frozen and resyncs, and
  only never-acknowledged in-flight writes are discarded.

Avoiding `load-balance-paths` also avoids an lb-tcp chunk-reassembly
corruption bug that LINBIT fixed in DRBD 9.2.17 (TwinStor ships DRBD
9.2.16).

#### Freeze I/O before anyone can diverge

Arbitrated mode sets `fencing resource-and-stonith` in the resource file
(`src/setup.rs`). DRBD then suspends I/O the moment it loses the peer, before
the fence-peer handler even runs. While I/O is frozen neither side advances,
so the copies cannot diverge even though both are nominally Primary during
the gap. This also freezes the XHA statefile (it lives on the DRBD-backed
SR), which is always being written while HA is armed. In testing, without
this freeze, an HA-armed switch death split-brained (the statefile diverged
during the reconnect gap) and XHA fenced repeatedly; with it, the same fault
gave one clean outage and no split-brain.

The handler then decides as described in
[How the fence-peer handler decides](#how-the-fence-peer-handler-decides).

What remains possible:

- **Both host-to-host paths die while the gateway stays reachable to both.**
  Both handlers keep their host serving, and the copies can diverge. This
  needs both links to fail at once, which is far rarer than a single switch
  or cable failure.
- **The switch dies and the gateway was on it.** Data stays consistent; how
  compute behaves is described next.

#### The XHA survival rules: why a switch loss fences both nodes unless they hold

With replication on the cable, a switch death does not touch DRBD. The fence
that can follow is XHA's, not DRBD's. Its logic is in the `xha` source,
`daemon/sm.c` (`test_Survival_Rule` and `am_I_in_largest_partition`). After a
heartbeat partition, a host survives if one of three rules lets it, checked in
this order:

1. **Subset rule:** every host it sees in the statefile it also hears on the
   heartbeat. It then knows its whole partition and survives.
2. **Largest partition:** each host's partition is scored from the statefile
   (the hosts it both hears and sees there, plus their configured weight),
   and the highest score wins. On a tie the host with the lowest index in the
   HA configuration wins. In a two-host pool split one against one, with
   equal weights, this is always the same host, which is why the same host
   was the one fenced in every isolation test on a given pool.
3. **Survival rule 2:** every host it cannot hear on the heartbeat has been
   excluded (declared dead) in the statefile.

A host that passes none of them self-fences.

In a switch loss both hosts hear only themselves on the heartbeat (UDP on
the management network, which was on the dead switch) while both still see
each other in the statefile, which DRBD keeps replicated over the cable. The
subset rule fails on both sides, and the largest-partition rule should let
the lower-index host survive. In testing, before the whole-switch hold
existed, a physical switch power-cycle of about 2 minutes nevertheless made
both hosts log `Survival rule failed (1905) ... Self-Fence` about 60 s after
the cut and reboot, while DRBD stayed consistent with no split-brain. Why
the partition rule did not save one host there was not established; the
hold below avoids depending on it.

**The whole-switch hold.** To keep the VMs running through a switch death,
each arbitrated host serves its own gateway reachability to the peer over the
direct link (a one-byte beacon on port 7900). When a host loses its gateway
and reads over the direct link that the peer has lost its gateway too, both
conditions stable for 15 s, it disarms its local XHA fencing
(`host-emergency-ha-disable`) and keeps its VMs running. This check runs on
its own 2 s task, so a daemon tick stalled on XAPI cannot make it miss XHA's
deadline. The hold is released, and HA re-armed, when the host's gateway
returns, when the peer reports its gateway up again (the case becomes a
single-host isolation and the stand-down below takes over), or when the peer's
"gateway down" can no longer be confirmed within 30 s.

During the hold the VMs keep running but cannot be reached by clients, since
their guest network was on the dead switch. Reaching them during a switch
outage needs network-layer redundancy (bonded uplinks, redundant switches),
which is outside TwinStor. The hold does not exist in fate-shared mode, where
there is no direct link to learn the peer's state from.

#### The HA timeout in arbitrated mode

Arbitrated mode sets the XHA failover timeout to 90 s (fate-shared uses 120 s,
for the replication-split reason in
[BEHAVIOR.md, Known limitations](BEHAVIOR.md#known-limitations); see
`ha_timeout_secs` in `src/setup.rs`). If the heartbeat returns before the
timeout expires, XHA's fault handler never runs, so a short interruption (STP
reconvergence, a fast switch reboot) passes with no fence at all. The XHA
statefile lives on the DRBD-backed SR, so commodity hardware also needs this
headroom to arm HA and write the statefile without an early self-fence. The
cost of a longer timeout is a slower failover on a real host death. See
[HA_TIMEOUT_TUNING.md](HA_TIMEOUT_TUNING.md).

#### The isolated-master trap, and the storage stand-down

Arbitrated mode separates replication onto its own path, and that opens a
case fate-sharing cannot produce: the pool master loses only its client
(switch) path while the direct link keeps replication alive. The master is
now an island: unreachable from outside but alive and `UpToDate` on
replication, so XHA's statefile keeps it live and leaves its VMs running
where clients cannot reach them. Storage is fine throughout; the harm is
availability.

A reboot-based self-fence is the wrong fix: the island comes back still
isolated and still believing it is master, XHA cannot reach it to demote it,
and when its management network briefly returns it can restart the VMs, which
then run on both hosts and write the same LUN.

TwinStor uses a **durable storage stand-down** instead
(`[ha] self_standdown_isolated_master`, on by default). After the isolation
has been confirmed for 10 minutes (an alert at 5 minutes; 1 minute when the
isolated master runs no VMs), with the peer alive and `UpToDate` on the
replication link, the isolated master:

1. hard-stops its local guests, and goes no further until they are confirmed
   gone;
2. disables its local `xhad`, so losing the statefile path in the next step
   cannot make XHA self-fence the host;
3. stops its iSCSI target, logs out its initiator sessions, and demotes to
   `Secondary`.

Demoting local DRBD alone is not enough: the direct link still gives
multipath a route to the peer's target, which the VMs would fail over to and
keep writing through. All client access has to be removed. XHA then restarts
the VMs on the reachable peer. Three properties make this safe:

- **It only removes a writer, never adds one**, so it cannot create two
  writers. It is the opposite of a self-fence.
- **The handoff is interlocked by the statefile, not by timing.** On each
  host, SR access, the ability to write the VMs' disks and the ability to
  write its statefile heartbeat are one and the same path (iSCSI, LIO,
  DRBD). Removing storage stops the writes and the heartbeat together, and
  the peer's XHA restarts the VMs only after it sees that heartbeat stop. The
  island is out of the storage before the peer starts: a gap, never an
  overlap.
- **It is durable.** A persistent marker
  (`/var/lib/twinstor/isolated-standdown`, kept across reboots) keeps the
  host parked (Secondary, no promote, no serving) on every boot while it is
  still isolated. A rebooted island can never come back and run the VMs
  again.

The host rejoins on its own when its client network has been back for 45 s
(so a gateway blip cannot re-promote it) and the peer is reachable on
replication (it never re-promotes blind). Because it stayed `Connected` as
Secondary throughout, it is still `UpToDate`: a clean re-promote with no
divergence.

**Scope and guards.** Arbitrated only; in fate-shared mode a switch loss
takes the peer too, so the condition cannot arise. It requires the peer to be
`UpToDate` (never hand over to a peer without a good copy) and falls back to
detect-and-alert whenever it cannot confirm a peer that can serve. It is not
a fence on gateway loss alone: it acts only on a master isolated past the
confirmed window with a peer that can serve, and it never discards data. It
also differs from the whole-switch case above: there both hosts lose the
client network and there is no reachable peer to hand the VMs to.

**With pool HA armed, XHA usually acts first.** The heartbeat runs over the
switch network and is lost at once, so after the HA timeout XHA keeps one
host and the other self-fences, which may happen before the stand-down
window ends. If XHA keeps the reachable host, the isolated master reboots and
its VMs restart on the peer. If XHA keeps the isolated master, both hosts can
reboot and the VMs are down for about 10 minutes. No acknowledged write is
lost either way. An isolated pool member (not the master) stands down in a
similar way after about 2 minutes. Both cases are described in
[BEHAVIOR.md](BEHAVIOR.md).

#### Measured results

- On hardware: switch-path blocks at the DRBD level, and an HA-armed switch
  death with a running VM; active-backup behaviour was also checked
  physically.
- An overnight soak on an arbitrated pool passed 43 of 43 runs across
  carrier loss on either path, clean reboot, sysrq power cut, daemon crash,
  iSCSI bounce, single, dual and staggered reboots, and isolation. Every run
  healed back to a healthy dual-primary pair with no split-brain.
- The dual-master wedge that `survivor_ha_reset` heals could not be staged in
  that soak by any of four methods, because `resource-and-stonith` fencing
  prevents it from forming. The path was later exercised by three 8-minute
  master cable isolations, which ran the full wedge ladder including both
  branches of the survivor HA reset.

#### A direct link alone is the worst option

If DRBD runs on the cable only, the cable can fail on its own, DRBD splits
while the gateway is fine, and both handlers keep serving. The feature is
"DRBD over both paths, so no single link can split it", not "add a cable".

### What each mode survives

| Event | Fate-shared | Arbitrated (two DRBD paths, active-backup) |
|---|---|---|
| Host death or power loss | VMs restart on the survivor, no corruption | Same |
| Switch failure, gateway still reachable (for example on a separate VLAN) | Full pool outage | No effect on replication: it already runs on the direct link, and the failed switch path was only the standby |
| Switch failure, gateway on the dead switch | Full pool outage (both hosts freeze and are fenced, cleanly) | Data consistent, no split-brain. Both hosts hold: after 15 s they disarm local XHA fencing and keep their VMs running, unreachable to clients until the switch returns, then re-arm HA. A blip shorter than the XHA timeout passes with no action. Reaching the VMs during the outage needs network-layer redundancy |
| Direct link failure | n/a | Replication moves to the switch path without a connection reset (temporarily fate-shared; `twinstor_replication_fallback` alert and `repl path:` in `twinstor status`); the daemon moves it back to the cable once the cable returns |
| Both host-to-host paths down, gateway up | Cannot happen (single segment) | Remaining corruption window, which needs both links down at once; see [Why there is no witness](#why-there-is-no-witness) |
| HA disarmed | Exposed | Exposed |

## The divergence budget

The only event that can cost acknowledged writes is a **symmetric dual run**:
both hosts keep serving while replication is cut, both accumulate their own
writes, and block replication cannot merge them, so whichever side loses the
resolution has its diverged writes rolled back. TwinStor's position on this
event: it favours convergence and self-healing, and accepts a bounded,
announced rollback when the event does occur, because layered prevention
makes it rare and the fencing design keeps it short. The rollback alert gives
the approximate number of discarded blocks; there is no copy of the discarded
data, so it cannot be recovered.

**Layer 1: most partitions never diverge.** The gateway tie-breaker makes an
isolated host yield and freeze its I/O rather than serve (it served nobody,
so its later discard loses nothing), and the yield latch makes that verdict
stick for the whole episode whatever its stale pool role says. When one side
provably has all the data, that side wins.

**Layer 2: races cannot promote into a dual run.** The startup connect-wait,
the re-armed connect-wait when the peer becomes reachable again during a
rejoin (30 s), and the handler's promote-time refusal while the peer's DRBD
port answers all stop a rebooting or rejoining host from promoting alone into
a partition that is already healing.

**Layer 3: transient faults are defused, not resolved.** A carrier flap
under about 3 s kills the DRBD TCP connection at once, but the handler probes
the peer's DRBD port and holds up to 4 s when it answers, so the link
reconnects with no divergence. Repeated episodes raise a
`twinstor_flapping_link` alert naming the suspect hardware.

**Layer 4: arbitrated mode removes the common trigger.** With replication on
the direct link, switch faults, by far the most common failures, never cut
it, and a cut cable falls back to the switch without a connection reset.

**Layer 5: when a dual run does form, XHA eventually ends it.** The XHA
statefile lives on the replicated SR, so a cut of replication also splits the
statefile, and XHA fences one side, though not within the HA timeout: in
testing (HA armed, 90 s HA timeout, 5-minute replication-only cut) the fence
came 197 s after the cut. **The divergence lasts as long as the cut, or until
that fence**, and the rollback is the loser's acknowledged writes for that
whole period. A flap shorter than the handler's probe (a few seconds) is
defused with no divergence.

**Layer 6: the resolution is deterministic, crash-consistent and loud.** The
order of evidence:

1. A recorded fence-peer yield: the host that yielded served nobody and
   loses.
2. The DRBD generation lineage: a copy that descends from the other holds
   every write the other has, and is kept. In a true fork (neither descends
   from the other), the copy with more un-replicated writes is kept, and a
   priority-1 `twinstor_split_brain_diverged` alert names the rolled-back
   host. A tie keeps both copies and holds.
3. The pool master role, read from live XAPI.
4. When neither side can read a live pool master (both fall back to
   `pool.conf` and both read "not master"), the keeper.

An unstable or unreadable verdict never discards; it keeps both copies and
alerts. The keeper step only makes the loser deterministic instead of the
result of a discard-refusal race, and never enlarges the discard set. It is
not the witness rejected below: it is a local static rule with no external
component, and it resolves a wedge that has already formed rather than
deciding who may serve.

The loser's VMs are stopped before its copy is discarded and restarted on the
winner's copy, with power-loss semantics (a clean crash, not corruption); see
[RECOVERY_CRASH_BOUNDARY.md](RECOVERY_CRASH_BOUNDARY.md). The alert names the
VMs whose writes were rolled back.

TwinStor does not hold the pool down to avoid the rollback. A "never
auto-discard under running VMs" policy would trade the rollback of the
loser's writes for an outage lasting until an operator acts; it does not
exist today.

### The conjunction: how many independent failures a loss requires

The layers above are not a sequence; they are independent conditions, and a
split that loses acknowledged writes has to defeat all of them at once:

    acknowledged-write loss requires
        replication really cut              (past the 4 s flap-defusal hold)
      AND not absorbed by a second path     (arbitrated: the cable AND the
                                             switch path both down;
                                             fate-shared has no second path)
      AND the fence invariant broken        (both hosts keep the gateway
                                             while losing the peer)
      AND both hosts already serving        (neither is booting or
                                             rejoining)
      AND before XHA fences one side        (the divergence lasts until the
                                             cut ends or that fence)
      AND a guest writing on the losing side (writes that landed only on the
                                             winner cost nothing)

Each condition is uncommon on its own, and the combination is what makes the
event rare. The last two are not prevention but bounds: the XHA fence ends
the window, and Layer 6 (the evidence order, never discarding on doubt, the
priority-1 alert) makes whatever gets through deterministic, minimal and
loud.

The two topologies narrow this differently. On fate-shared, the second and
third conditions leave one residual case: a grey transport fault where
DRBD's TCP dies while the ICMP gateway probe and the UDP heartbeat both
survive, which a single flat L2 segment makes unlikely. On arbitrated, they
require a real double link failure (the cable and the switched east-west
path) at the same time, while both hosts still reach the gateway.

### Measured in soak testing

Night soaks drove a fate-shared pool (single path) and an arbitrated pool
(cable and switch, active-backup) through compound faults: isolation, daemon
kills, carrier drops, and second-strike combinations aimed at the replication
path. An in-guest checksummed-write verifier and a byte comparison of the two
replicas checked for silent loss. The fate-shared pool ran `resource-only`
(freeze mode did not exist yet), so its figures do not reflect a fate-shared
pool with freeze mode engaged.

- **Layer 4 holds in practice.** On the arbitrated pool the direct cable
  absorbed the switch-path faults: most fault cycles never cut replication,
  so most never forked. On the fate-shared pool the single path was cut by
  almost every replication-targeting fault, producing far more split-brain
  episodes.
- **The window was short, and shorter on arbitrated.** When a true fork did
  form in those soaks, the rollback was the loser's last seconds of writes
  acknowledged on one copy only: about 3.5 s on the arbitrated pool against
  about 73 s on the fate-shared pool. `resource-and-stonith` freezes I/O when
  the last path is lost, so an arbitrated host serves on one copy for far
  less time before it reconnects or is fenced. The exact seconds come from a
  handful of fork events; the ratio reflects the mechanism. A sustained
  replication-only cut is longer: see Layer 5.
- **No silent loss, no physical divergence.** Across 21 cycles of compound
  faults on the arbitrated pool, no acknowledged write was lost. Every fork
  resolved to a byte-identical pair (out-of-sync = 0 on verify) and raised
  the priority-1 `twinstor_split_brain_diverged` alert. The only losses seen
  in the whole campaign were the expected, alerted, bounded fork rollbacks
  on the fate-shared pool.

### Deployment recommendation

Prefer arbitrated mode for any pool carrying real workloads where a second
NIC and a back-to-back cable are feasible: it removes the common
(switch-side) split-brain trigger, shrinks the remaining window, and needs no
extra host. Keep fate-shared, the default, for genuine single-switch sites,
where the invariant holds by topology and an outage on switch death is
acceptable. Neither mode makes a total switch death invisible to clients,
because the client network still runs through the switch; that is a
network-layer redundancy decision, independent of the mode.

## Why there is no witness

The one remaining data-loss window in both modes is the symmetric partition
in the last row of the table above: both host-to-host paths down while the
gateway stays reachable to both, so both handlers keep their host serving and
the copies diverge. A quorum or tiebreak witness would target this
window, and the idea comes up regularly. The answer is settled here.

**TwinStor does not add a witness.** The reasons, most important first:

1. **Arbitrated mode already covers the realistic route.** Without it, the
   window opens on a single replication-link failure. Arbitrated mode closes
   that with a second replication path in active-backup (losing one path
   does not partition replication) and `resource-and-stonith` (I/O freezes
   the moment the peer is lost, before anyone can diverge during a reconnect
   gap). Reaching the symmetric state then requires losing both replication
   paths at once while the gateway stays up, a double fault far rarer than
   the single switch or cable failure the mode was built for. On fate-shared
   the only route left is a grey transport fault (DRBD's TCP dies while the
   ICMP gateway probe and the UDP heartbeat both survive), which a single
   flat L2 segment makes unlikely. A witness would only cover this rare
   residual.
2. **A gating witness breaks self-healing.** DRBD strict quorum freezes on
   loss (`on-no-quorum suspend-io`): a host that loses quorum stops serving
   until quorum returns, and if the lost element is the witness, recovery
   needs a person. That is the opposite of TwinStor's self-healing behaviour,
   so a quorum-gating witness is rejected.
3. **A tiebreak-only witness still costs the two-host design.** A witness
   that only settles the symmetric tie (never blocking a healthy lone
   survivor) would not hurt self-healing, but it is still a third thing to
   power, patch and monitor, which can itself fail, across many unattended
   sites with no local IT. It cannot become the baseline without giving up
   "no third host". At most it could be an option, and an option that
   protects only a rare residual case is weak justification for the added
   surface.
4. **The residual is a containment problem, not a prevention one.** What a
   witness would prevent, TwinStor contains when it happens: detect the
   divergence, keep the copy that loses the fewest writes, and alert. Keeping
   the loser's diverged data before the discard is not implemented yet.
   Containment covers both topologies and needs no new component.

**About `drbdadm disconnect` tests.** A test that runs `drbdadm disconnect`
severs replication administratively while both hosts stay fully reachable.
That produces the symmetric state directly, bypassing both fate-sharing
(which makes it physically unreachable on a single link) and active-backup
(which rides out the loss of one path). Such a test shows that the window is
severe when reached, not that real faults reach it often. What would justify
revisiting the witness is evidence that the physical routes (a
transport-only drop on fate-shared, a loss of both paths on arbitrated) reach
the symmetric state often enough to be worth a new component.

## Recovery after a fence: the XHA re-admission wedge

Everything above concerns the fence decision (who keeps serving when a
partition happens). This section covers what comes after: a host that was
fenced, then returns and does not cleanly rejoin the pool.

### Symptom

After a host is isolated (cables pulled, or any fault that takes its
heartbeat and storage path together) and the survivor's HA restarts its VMs,
the returning host can come back **stuck in maintenance or disabled** and
never re-enable on its own. XAPI operations that reference it fail, typically
with an `INVALID_VALUE` or "invalid host UUID" style error for a host UUID
that is present in the pool's host list. Clearing it requires restarting HA
on the **pool master** (disable HA, reboot the master or restart its `xhad`,
re-enable HA), not acting on the returning host.

The daemon attempts recovery from this state automatically: first a gentle
toolstack restart on the returning host, then an HA reset orchestrated by the
survivor (HA is kept off for at most 480 s, then re-armed regardless).
Progress is shown in `twinstor status`, and the manual procedure below
remains the fallback when the automatic steps give up. The rejoin race that
creates these wedges is closed separately by the connect-wait that is
re-armed when the peer becomes reachable again (held for 30 s of wall-clock
time unless DRBD connects first) and by the handler's promote-time guard.

In testing (fate-shared pool, member host isolated by pulling its cable), the
member was fenced and rebooted cleanly and DRBD resynced to `UpToDate` with
no split-brain, but the host stayed in maintenance with an invalid-host-UUID
error until the master was rebooted with HA disabled.

### Why it happens

This follows from the same fact that drives the survival rule: **the XHA
statefile is on the TwinStor SR**, so it is DRBD-replicated local storage,
not a single shared object.

XHA assumes the statefile is one physical object on storage that is
independent of the hosts' failure domains and never forks: every host reads
the same bytes, and the master's `xhad` liveset (who is admitted, who is
excluded) stays consistent with it. Re-admitting a returning host is reliable
under that assumption.

TwinStor breaks the assumption. During a full isolation the statefile forks:
each host keeps writing its own local DRBD copy and reads itself as a valid
survivor (see [The invariant](#the-invariant)). On reconnect, DRBD's
split-brain resolution keeps one copy and replaces the other underneath
`xhad`, which has no way to know its statefile's contents just changed. The
master's `xhad` still holds the returning host as excluded in its in-memory
liveset, the statefile on disk no longer agrees, and XHA has no protocol for
"my shared object was reconciled by an external replicator". The returning
host cannot be re-admitted to the liveset, stays disabled, and pool
operations that look it up in the master's stale view fail.

Restarting the master's `xhad` rebuilds its liveset from the now-consistent
statefile, which is why the fix is on the master. `xhad` behaves correctly
for an invariant this topology does not provide; the wedge is a cost of
building shared storage from replicated local storage, the same trade-off
that makes the statefile forkable in the first place.

This is not specific to one mode: any fault that isolates a host long enough
to fork the statefile can trigger it. It is more likely in fate-shared mode
only because there a single cable pull takes the heartbeat and storage
together.

### The join-deadlock variant

A second re-admission failure exists besides the stale-liveset wedge. After a
fence storm, two staggered `xhad` restarts can deadlock on each other's stale
liveset claims, which stay fresh for about one HA timeout. Each host waits
for a join acknowledgement only the other, as a member, could give; both
time out (`xe` reports "HA daemon failed to start", possibly on both hosts);
and each retry poisons the other's window again. Nothing blocking is
persistent.

The daemon detects the signature (host state stuck in Starting plus the
join-timeout pair in `xha.log`) and, unless `[ha] readmission_assist = false`,
recovers automatically, at most once an hour and only when local storage is
healthy and the peer is reachable. Both daemons independently compute the
same wall-clock sync point, quiesce their HA daemons past the
claim-freshness window, and the pool-master side re-arms HA with the pool's
existing timeout, which starts both HA daemons together into a joint
bootstrap from an empty liveset. If the storm is still cutting the
statefile, the daemon alerts "fix the link first" instead. The manual
fallback is in [FAQ.md](FAQ.md), "A fenced host came back stuck in
maintenance".

### Manual recovery

On the **pool master**:

1. `xe pool-ha-disable` (clears HA pool-wide and makes the liveset
   consistent again). If the master's `xhad` is wedged enough that this
   hangs, use `xe host-emergency-ha-disable --force` on the master, or reboot
   the master; either restarts its `xhad`.
2. Check that the returning host is `enabled=true` and out of maintenance
   (`xe host-list`), and that its PBD is plugged.
3. Re-arm with the mode's timeout:
   `xe pool-ha-enable heartbeat-sr-uuids=<TwinStor SR UUID> ha-config:timeout=<120 fate-shared, 90 arbitrated>`.

Do not start by reintroducing or rebooting the returning host: the stale
state is on the master, so acting on the host alone does not clear it.

### What is confirmed and what is inferred

The class of failure (an HA re-admission wedge cleared by restarting HA on
the master) is well supported: it has been observed, and the forkable
statefile is confirmed in the code (`src/setup.rs`,
`pool-ha-enable heartbeat-sr-uuids=...`; the statefile is on the
DRBD-backed SR). The exact XAPI code path that produces the invalid-host-UUID
error is inferred, not pinned down: the support bundle captures XAPI state
snapshots but not `xensource.log`, so the precise error class
(`HOST_OFFLINE`, a slave-without-master proxy failure, or a stale host
record) is not confirmed. If you hit this, attach `xensource.log` and
`xha.log` from both hosts to the report. Whether `xhad` could be hardened to
recover from a statefile whose contents change under it is a question for
upstream `xha`.

### The dual-master auto-heal: which wedges each topology can reach

The daemon's dual-master auto-heal (the freshly rebooted, VM-less non-keeper
restarts its own toolstack so XAPI demotes it to a slave of the survivor; see
[FAQ.md](FAQ.md), "A fenced host came back stuck in maintenance") decides
from local signals only. Its coverage is easiest to understand from which
wedges can physically occur in each mode.

**Fate-shared: the isolated host is always empty when the wedge forms.** One
path carries replication, heartbeat and gateway, so an isolated host loses all
three at once: the handler cannot reach the gateway and freezes I/O, and the
XHA statefile watchdog fences the host. It reboots and joins, still
partitioned, as a second master with no VMs. A loser with VMs cannot occur,
and the decision reduces to two reachable cases, both handled by the
auto-heal and both checked in testing:

| Isolated / survivor VMs | Reachable in fate-shared? | Outcome |
|---|---|---|
| `0 / 0` | Yes (the loser is always empty) | The non-keeper yields; the keeper wins (deterministic) |
| `0 / >0`, the empty host is the non-keeper | Yes | The empty host yields; the survivor (with VMs) wins |
| `>0 / 0`, `>0 / >0` (dual run) | No | Requires the loser to keep serving, which the fate-shared fence prevents |

The only way to produce a loser with VMs in fate-shared mode is to block just
the DRBD port (for example an iptables rule on tcp/7789) while leaving the
gateway reachable. That is an artificial injection, not a real partition: a
real link cut also drops the gateway, so one side yields; a port-only block
leaves both sides reaching the gateway, so both survive and diverge. It is a
test artifact, not a product failure mode; do not use it to test a dual run
and conclude the product is broken.

**Fate-shared limitation: a keeper-isolated `0 / >0` does not auto-heal.**
If the host that is isolated and reboots is the keeper, it returns empty but
does not yield. The keeper guard in `decide_wedge_action` is needed for the
`0 / 0` tie (without it, two empty hosts would both yield and leave no
master), and locally a host cannot tell `0 / 0` from `0 / >0` without asking
the peer for its VM count, which the auto-heal avoids. So this case falls to
the fail-safe (keep and alert, never discard): safe, but the auto-heal fires
only for partitions where the non-keeper was cut. Relaxing it would bring
back the peer-query coordination the auto-heal was designed to avoid.

**Arbitrated: the `>0` isolated cases become real.** With a dedicated DRBD
link, a host can lose the switched (gateway and management) path while the
direct link survives, so it is not fenced and keeps running its VMs: a real
dual run (`>0 / >0`). The auto-heal is fail-safe here (the zero-VM check stops
a host with VMs from ever yielding). The `peer_reachable` check probes the
peer's management address through XAPI, the path the demotion depends on,
and falls back to the replication address only when XAPI cannot answer. In
testing, arbitrated dual-master convergence has been exercised at the DRBD
and fence level (loser discard), but the toolstack-restart auto-heal itself
has not yet been seen firing in an arbitrated dual run.

## Checking your topology

These checks cannot all be enforced from dom0, but they catch the common
misconfigurations. Run the failure tests before putting VMs on the SR: on an
exposed topology the test itself produces a split-brain.

Fate-shared mode:

- The peer address and the default gateway are in the same subnet as the
  local replication interface, so the peer and the fence target share the
  path.
- There is a single route to both the peer and the gateway, with no separate
  management and storage split that would decouple them.
- Break the connection between the hosts at the point your topology could
  fail (an inter-switch link, a stack member), with both hosts healthy, and
  read `/var/log/twinstor-fence-peer.log` on both. Safe: at least one host
  yields (a line ending `-> isolated, stay frozen (exit 1, yield recorded)`
  or another `exit 1` verdict). Exposed: both hosts log
  `reachable -> exit 4 (survive, serve clients)`.

Arbitrated mode:

- Both paths are configured: `drbdsetup events2 --now twinstor-data` lists
  two `path` lines, and `twinstor status` shows `repl path: direct link`.
- Taking the main switch path down leaves DRBD Connected over the direct
  link (no split, no fence-peer run).
- DRBD does not run on the direct link alone (see
  [A direct link alone is the worst option](#a-direct-link-alone-is-the-worst-option)).

Both modes:

- Pool HA armed (`pool.ha-enabled = true`); the daemon alerts after 30
  minutes disarmed.
- The HA timeout matches the mode; `twinstor doctor` warns when it is lower.
- No host left `enabled=false`, and no permanently unplugged PBDs.

## Appendix: code references

- Fence-peer decision and gateway ping: `config/twinstor-fence-peer.sh`;
  DRBD `fencing resource-only` (fate-shared, raised live to
  `resource-and-stonith` in freeze mode) or `resource-and-stonith`
  (arbitrated) in the generated `.res` (`src/setup.rs`).
- Gateway probes in the daemon (alerting, arbitrated isolation and
  whole-switch detection, fate-shared lone-survivor decision):
  `src/state_machine.rs`, `ping_default_gateway`, `check_tiebreaker_health`,
  `whole_switch_guard_decision`, `lone_survivor_decision`.
- HA enable, statefile on the TwinStor SR, timeouts: `src/setup.rs`
  (`pool-ha-enable heartbeat-sr-uuids=...`, `ha_timeout_secs`).
- XHA heartbeat on the management interface: `xen-api`,
  `ocaml/xapi/xapi_ha.ml`, `write_config_file` (`_management_interface`).
- XHA survival rules (subset test over the heartbeat and statefile sets, then
  the weighted largest partition with the lowest host index breaking ties,
  then survival rule 2 over excluded hosts): `xha`, `daemon/sm.c`,
  `test_Survival_Rule` and `am_I_in_largest_partition`.
- XAPI's HA timeout: `ha-config:timeout` if given at enable, else pool
  `other-config:default_ha_timeout`, else 60 s (`xen-api`,
  `ocaml/xapi/xapi_ha.ml`, `Timeouts.get_base_t`; xhad's own built-in default
  of 30 s is not used by XAPI).
- VDI activation is plain `lvchange -ay` with no cluster lock manager: SM
  `lvutil.py`, `_activate`; locks are local fcntl locks under
  `/var/lock/sm`, reference counts under `/run/sm/refcount`.
