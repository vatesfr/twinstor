# VM placement safety on a TwinStor SR

A TwinStor SR should carry running VMs only when the storage stack is
healthy on the host that runs them and XHA is armed on the pool. This
document explains the two hazards behind that rule, how TwinStor enforces
what it can without hooks into XAPI or SM, and what remains the
administrator's job.

In short:

- **Host storage readiness** (per host, changes minute to minute) is
  enforced: a host that is not ready does not export its iSCSI target, so it
  is not a usable storage path.
- **XHA armed** (per pool) is advisory: `twinstor status` shows it, and the
  daemon raises an alert when pool HA stays off.
- The same lever, withdrawing a host's storage, is used to move VMs off an
  isolated host in arbitrated mode.
- PCI passthrough VMs must use auto-start, not HA protection.

## Background

In testing, a running VM whose disk lives on the TwinStor SR was
live-migrated onto a host that had just rebooted. The VM crashed on the
destination even though `drbdadm status` reported `Primary / UpToDate` on
both hosts. The cause was not DRBD replication: the destination was still
inside its post-reboot convergence window. The VM recovered on the healthy
host.

## The two hazards

### 1. Dual-primary DRBD without fencing

TwinStor runs DRBD dual-primary so that either host can write the shared
LUN. Dual-primary is safe only if a partitioned or degraded host is fenced
before the survivor keeps writing alone. Fencing is XHA's job: the
fence-peer handler lets a host keep serving whenever it can still reach a
ping target, so it does not stop both hosts from writing when only
replication is cut.

If VMs run on a TwinStor SR while XHA is not armed, a cut of replication
alone (or a host that wrongly believes it is healthy) lets both sides write
their own copy until the link returns. TwinStor then resolves the
split-brain from DRBD's record of which copy holds which writes, and the
losing side's writes from that period are discarded, with an alert naming
the affected VMs (see [BEHAVIOR.md](BEHAVIOR.md)). With XHA armed the
window is bounded: the XHA statefile lives on the SR, so a cut that
outlasts the HA timeout makes XHA fence one host.

So pool HA should be armed on any pool running VMs on a TwinStor SR.

### 2. The post-reboot convergence window

Even with DRBD `UpToDate`, a freshly booted host is not yet ready to serve
the LUN. Several things must converge first:

- iSCSI sessions log in to both portals;
- multipath probes the paths and marks one active;
- DRBD reconnects and both copies reach `Connected` and `UpToDate`;
- the twinstor daemon finishes bring-up;
- the XAPI and xenopsd toolstack finishes initialising (host
  `enabled=true`).

Starting or migrating a VM into this window places it on storage that is
not yet reliably serving. That is what crashed the VM above.

## Two guards, kept separate

The two hazards have different enforcement paths and are not combined in
one check:

- **Host storage readiness** is a per-host runtime fact the daemon observes
  locally, and it changes during a reboot or a resync.
- **XHA armed** is a pool-level precondition, true or false for the whole
  pool regardless of any one host's storage.

### Guard A: host storage readiness (runtime, per host)

A host's copy is serving reliably only when all of these hold:

1. the local DRBD resource is `role:Primary`, `disk:UpToDate`
   (**enforced**);
2. the peer is `Connected` with `peer-disk:UpToDate`, or the peer is down
   and this host is the lone survivor (see the caveat below) (**enforced**);
3. multipath shows at least one active path to the LUN;
4. iSCSI sessions exist to both portals (or to the only reachable portal
   for a lone survivor);
5. XAPI reports the host `enabled=true`, meaning the toolstack has
   converged (**enforced**).

Items 3 and 4 are reported as a separate `degraded-paths` readiness verdict
and self-healed by the path watchdog, but never gate serving. The target
must stay up regardless: it is the peer's second path, and at boot the
multipath map cannot exist before the target it rides on, so gating on it
would deadlock bring-up.

### Guard B: XHA armed (static, per pool)

6. XHA enabled and armed on the pool, so that a partitioned or degraded
   host can be fenced before the survivor keeps writing.

This is a deployment requirement, checked in an advisory way, never in the
storage-serving path:

- **Bootstrap deadlock.** XHA's statefile lives on the TwinStor SR itself.
  In a two-host pool with no other shared storage, the TwinStor SR is the HA
  heartbeat SR. If TwinStor refused to serve until XHA was armed, XHA could
  never arm: it needs the SR to write its statefile.
- **Failing closed here would be a pool-wide outage.** Guard A trips on
  local, fast-changing facts and only affects starts and migrations onto one
  host. Guard B is a pool-wide flag that flips during ordinary operations
  (HA disabled for maintenance, a master election, a toolstack restart).
  Gating serving on it would take storage away from every running VM at
  once the moment HA blips.
- **It protects against a future event, not the present write.** XHA being
  disarmed does not corrupt the write happening now; it removes the fencing
  safety net for a future partition. Refusing present I/O to hedge it is
  itself the outage.
- **The flag does not prove the fence works.** `ha-enabled=true` says HA is
  configured, not that the fence path works. The divergence case above
  (replication cut while both hosts reach the gateway, so the fence-peer
  handler lets both serve) happens with HA enabled too; HA bounds it, it
  does not prevent it. An enforcing check would add the deadlock risk
  without removing that case.

## How readiness is enforced

TwinStor has no SM or XAPI hook, so a readiness flag the daemon merely
advertised would have nothing to consume it: the XAPI scheduler would still
place a VM on a host that is not ready. A flag is useful in
`twinstor status`, but it is advisory.

The enforcing mechanism is to **gate the iSCSI target on readiness**: a
host does not export its LIO target until Guard A holds. A host that is not
ready is then not a usable multipath path, and the existing multipath logic
routes around it with no new policy code and nothing required on the XAPI
side.

**The lone-survivor exception.** Gating the target on the peer being
`Connected` would strand a host whose peer is really dead, exactly when it
is needed. The gate therefore keys on local serving ability (`Primary`,
`UpToDate`) and accepts a peer that is gone. A lone survivor is only safe
if the peer was actually fenced, which is XHA's job (Guard B), not merely
unreachable. Whether a host may promote alone in the first place is decided
earlier in bring-up (the boot gate and, on a fate-shared pool, a 10-minute
lone-survivor hold; see [ARCHITECTURE.md](ARCHITECTURE.md#init-bring-up-try_go_online)).

**Fail closed for starts and migrations.** When readiness cannot be
confirmed, the host does not offer the path, so placement avoids it.

## What the code does

- A pure readiness predicate (`src/readiness.rs`, `assess()`) evaluates the
  local DRBD state (items 1 and 2) and toolstack convergence (item 5). It
  returns one of four verdicts: `ready`, `degraded-paths`,
  `serving-without-redundancy`, or `not-ready`.
- DRBD data safety dominates. A Diskless Primary whose peer is Connected
  and UpToDate is `serving-without-redundancy` (DRBD forwards its I/O to the
  peer's disk). On a copy that is safe to serve, host `enabled=false`
  downgrades the verdict to `not-ready`.
- `host_enabled` is read from XAPI with a bounded call
  (`xapi::is_host_enabled`). When XAPI cannot answer, the DRBD verdict
  stands, so an `xe` that is briefly unavailable at boot cannot strand
  storage.
- `try_go_online` (`src/state_machine.rs`) re-reads DRBD after promoting,
  calls `assess()`, and does not start the LIO target unless the verdict
  can serve. A host that is not ready stays in `Init` and retries each tick.
  A lone survivor (`Primary` and `UpToDate`, peer gone) goes through as
  `serving-without-redundancy`, with a warning.
- `twinstor status` reports the same verdict, and shows when pool HA is
  disabled.
- For Guard B, the daemon checks `ha-enabled` every 10 minutes. Once HA has
  been off for 30 minutes, the pool master raises `twinstor_ha_disarmed`
  with the re-arm command, re-raised every 24 hours while HA stays off.

## Known gaps

- **Multipath and session health (items 3 and 4)** are reported and
  self-healed, never part of the serving gate.
- **The gate is on the providing side.** It binds this host's target to this
  host's readiness. The original incident was a migration onto a
  destination; with no SM or XAPI hook, TwinStor cannot veto XAPI's
  placement decision. It can only make sure a destination that is not ready
  presents no usable local storage path during its window.
- **Guard B has no setup-time preflight.** The HA-disarmed alert fires
  whether or not any VM uses the SR.

## The same lever, used to evict: isolated-host stand-down

Making a host's storage unavailable is not only a readiness gate (refuse to
serve when not ready). It is also how TwinStor moves VMs off a host that
must not run them.

In arbitrated mode, a host that loses its switch link keeps replication and
the HA statefile alive over the direct cable, so XHA does not see it as
dead and does not fail its VMs over. When the peer is reachable on the
direct link, its gateway is up and its copy is `UpToDate`:

- an isolated **pool master** stands its storage down after a confirmed
  10-minute window;
- an isolated **slave** with VMs stands down after 2 minutes, when pool HA
  is enabled.

Standing down means stopping the host's target, logging out its initiator
sessions and demoting to `Secondary`, so its VMs (already unreachable to
clients) lose their disks and XHA restarts them on the reachable peer. It
has to sever all of the host's storage access, not only demote its local
DRBD: the direct link keeps a multipath path to the peer's target, which
the VMs would otherwise keep writing through.

This is the closest thing TwinStor has to block-layer mutual exclusion
(nothing at the block layer prevents the same LV from being activated on
both hosts; a host with no SR access cannot do it). The stand-down only
ever removes a writer, so it cannot lose data. It is gated on the peer
being `UpToDate`, persists across reboots (a durable marker keeps the host
parked while it is still isolated), and is interlocked by the HA
statefile: severing storage stops the writes and the statefile heartbeat
together, so the peer takes over only once the isolated host is out of the
storage. It is controlled by `[ha] self_standdown_isolated_master` (default
on) and inert on fate-shared pools. In a whole-switch outage (the peer's
gateway is down too) neither host stands down. See
[NETWORK_TOPOLOGY_SAFETY.md](NETWORK_TOPOLOGY_SAFETY.md).

## Passthrough and host-pinned VMs: auto-start, never HA-protected

A VM with a **PCI passthrough device** (GPU, NIC, HBA, an NVMe drive:
anything assigned through `other-config:pci`) is bound to the host that
physically holds the device. It cannot live-migrate and cannot be started
anywhere else. Configure such a VM with:

- `other-config:auto_poweron=true` **on the VM** and
  `other-config:auto_poweron=true` **on the pool** (without the pool-level
  flag the VM flag is ignored); and
- an empty `ha-restart-priority`, so it is **not HA-protected**.

**HA protection is harmful here, not merely useless.** XHA computes VM
agility from storage and memory only; it does not account for PCI
passthrough. A passthrough VM with `ha-restart-priority=restart` is
therefore considered placeable on either host and admitted into the
failover plan (`ha-overcommitted` stays `false`). When the device's host
fails, XHA tries to restart the VM on the survivor, the start fails because
the device is not there, and the VM stays halted, resident nowhere, for the
whole outage. It recovers only when its own host returns. This is an XHA
limitation, not a TwinStor bug.

This was reproduced on real hardware: an HA-protected GPU VM was stranded
through a host crash while the agile VMs on the same host failed over
cleanly. The strand is confined to the passthrough VM and does not affect
the other VMs' failover, but that VM is down for the whole outage even
though XHA counted it as covered.

**Auto-start is the right recovery, and it is sufficient.** Since the VM
can only run on its device's host, restarting it on that host when the host
boots is the only possible recovery, and it is what `auto_poweron` does. In
the same test, after its host was crashed and woken again, the VM started
on that host with no HA involvement.

Rule of thumb: **agile VM, HA-protect it; passthrough or host-pinned VM,
auto-start only, never HA.**

```
xe pool-param-set uuid=<pool> other-config:auto_poweron=true
xe vm-param-set    uuid=<vm>   other-config:auto_poweron=true
xe vm-param-set    uuid=<vm>   ha-restart-priority=            # empty = not HA-protected
```
