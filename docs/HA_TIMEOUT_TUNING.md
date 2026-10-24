# Tuning the XHA timeout

This page covers what the pool's `ha-config:timeout` setting controls, why
TwinStor sets it to 120 s on fate-shared pools and 90 s on arbitrated pools,
how far it can be lowered on good hardware, and the short "ghost VM" window
that follows a fast host reboot.

The recovery times below were measured on an arbitrated pool with the HA
statefile on NVMe and 10/25 GbE networking.

## Summary

- **The HA timeout `T` is one trade-off.** A high `T` means fewer false
  fences and slower failover; a low `T` means faster failover and fences on
  shorter interruptions.
- **Recovery after a fast host reboot takes about `T` + 40 s.** The 40 s is a
  floor on the XAPI side (`ha_monitor_interval` of 20 s, the failover plan,
  and the VM boot) that no timeout value removes. Tuning `T` changes only the
  first term.
- **TwinStor sets `T` = 120 s on fate-shared pools and 90 s on arbitrated
  pools** (`ha_timeout_secs` in `src/setup.rs`). These are safe defaults for
  hardware TwinStor knows nothing about.
- **On known-good hardware** (fast statefile storage, redundant or bonded
  NICs, a lightly loaded dom0), `T` = 30 s is a reasonable choice on an
  arbitrated pool: about 70 s recovery instead of about 120 s at 90 s, with
  no early self-fence in testing. Read the fate-shared caveat under
  [Recommendation](#recommendation) first.
- The ghost window that `T` shortens is an availability gap (a VM briefly
  down), not a data-safety risk. See
  [The ghost is not a double-run](#the-ghost-is-not-a-double-run).

## What `T` controls

XAPI derives every `xhad` timer from `T` (`Timeouts.derive` in
`xapi_ha.ml`):

| Timer | Formula | At T=10 | At T=60 |
|---|---|---|---|
| `heartbeat_interval` | `min((T+10)/10, 3)` | 2 s | 3 s |
| `heartbeat_timeout` | `T` | 10 s | 60 s |
| `statefile_timeout` | `T` | 10 s | 60 s |
| `heartbeat_watchdog` (self-fence) | `T` | 10 s | 60 s |
| `statefile_watchdog` (self-fence) | `T + 15` | 25 s | 75 s |
| `boot/enable_join` | `T + 60` | 70 s | 120 s |

The design comment in XAPI reads: heartbeats are cheap but unreliable (UDP),
so have many; the statefile is slow but reliable, so have 20 when possible.
At `T` = 60 the interval is 3 s, which gives 60 / 3 = 20 confirmations per
window; that is where XAPI's own 60 s default comes from
(`ha_default_timeout_base` in `xapi_globs.ml`).

XAPI enforces `T >= 10` (`constraint violation: timeout >= 10`). At 10 there
are about 5 confirmations and a 10 s self-fence watchdog: the smallest window
in which XHA can still tell a real host death from a transient loss or a
scheduling stall.

## The fast-reboot ghost VM

When a host reboots faster than `T`, its VMs pass through a transient state:

1. The reboot kills the VM's domain; nothing runs on the host.
2. XHA has not yet evicted the host (the heartbeat gap is shorter than `T`),
   so XAPI's failover has not started, and XAPI still reports the VM as
   `running` on that host, with a stale domain id and no domain behind it.
   This is the ghost.
3. The heartbeat gap keeps growing (a rebooted host's `xhad` does not rejoin
   before the timeout), so at `T` XHA evicts the host and XAPI restarts the VM
   on the other host.

The ghost window is therefore about `T` long and always ends at the eviction.
No VM is stranded permanently.

### The ghost is not a double-run

During the window the VM runs nowhere: `xl list` on both hosts shows no
domain for it. XAPI's view is only stale. The dangerous HA failure is one VM
writing from two hosts; this is the opposite case, and the DRBD storage is
correct throughout. Lowering `T` shortens an availability gap and trades
nothing on data safety.

Because the ghost reports `running`, monitoring based on power state does not
see it: XHA, TwinStor's `restart_halted_protected_vms`, `auto_poweron` and
simple probes all read "running". The only reliable sign is a domain id that
has no live domain on its resident host.

## Measured recovery against `T`

Member host rebooted with `sysrq b`, HA-protected agile VMs on it, no
`auto_poweron`:

| `T` | Recovery after a fast reboot | Notes |
|---|---|---|
| 10 s (minimum) | about 55 s | Too sensitive: no self-fence margin |
| 30 s | about 70 s | Clean on NVMe and 10 GbE |
| 90 s (arbitrated default) | about 120 s | Safe, conservative |
| 300 s | about 330 s | Long ghost window, no benefit |

The results fit recovery = `T` + about 40 s. The eviction fires at about `T`
(the heartbeat gap crosses `T`, plus a few seconds of processing). Everything
after it (the HA monitor's 20 s re-plan cadence, the restart plan, the guest
boot) is the fixed floor.

## Why not `T` = 10 everywhere

Bonded or redundant networking removes the main cause of false fences (lost
UDP heartbeats), but `T` also bounds two things a bond does not help with:

- **Host responsiveness.** `heartbeat_watchdog` = `T`. A dom0 too busy to
  run `xhad` for `T` seconds (CPU starvation, a kernel stall, a hung XAPI, a
  large LVM coalesce) self-fences. At `T` = 10, a 10 s dom0 stall reboots a
  healthy host.
- **Statefile latency.** The HA statefile lives on the TwinStor SR itself
  (iSCSI over DRBD), so `T` ties fencing to storage latency, which rises
  during the events HA exists for: resync, path failover, DRBD reconnect. On
  commodity hardware (one slow disk, 1 GbE), 30 s can stall the first
  `pool-ha-enable` or trip an early self-fence.

The storage latency floor depends on the hardware. On NVMe and 10/25 GbE it
is far below 30 s; on a single slow disk it is not. Size `T` for the
worst-case statefile write latency and dom0 stall you expect, plus margin,
not for the network alone.

There is a third reason, specific to TwinStor: **a replication split also
splits the statefile.** Each host writes its own copy of the statefile while
replication is down, and a split that lasts longer than `T` makes XHA reboot
one host of its own choosing (in testing, the host running the VMs). On a
fate-shared pool the copies stay apart for the whole split: the cut itself,
then 15 to 75 s of resolution. At 120 s, a split of roughly 45 to 100 s heals
without a reboot; at 60 s, none longer than about 45 s did. See
`BEHAVIOR.md`, [Known limitations](BEHAVIOR.md#known-limitations).

## Recommendation

- **Unknown or mixed hardware:** keep the TwinStor defaults (120 s
  fate-shared, 90 s arbitrated).
- **Known-good hardware** (fast statefile storage, redundant NICs, headroom
  in dom0), arbitrated pool: `T` = 30 s gives noticeably faster failover and
  stays clear of the self-fence margin. Going below 30 s gains little (the
  40 s floor dominates) and erodes the margin.
- **Fate-shared pools:** lowering `T` below 120 s shortens the replication
  split that heals without XHA rebooting a host, and, when freeze mode is
  engaged, shortens the freeze budget too (it ends 25 s before the statefile
  watchdog; see [FATE_SHARED_FREEZE_MODE.md](FATE_SHARED_FREEZE_MODE.md)).
  Weigh that against the faster failover before changing it.
- `twinstor doctor` reports the pool's HA timeout and warns when it is below
  the mode's default.
- Set the value explicitly on every `pool-ha-enable`: it is not kept across
  a disable and enable, and a bare enable (by hand or from Xen Orchestra)
  falls back to XAPI's 60 s.

  ```bash
  xe pool-ha-enable heartbeat-sr-uuids=<twinstor SR uuid> ha-config:timeout=30
  ```

  To make every later enable use your value without passing it each time,
  set XAPI's persistent pool default, which a bare enable reads before
  falling back to 60 s:

  ```bash
  xe pool-param-set uuid=<pool uuid> other-config:default_ha_timeout=30
  ```

See also [VM_PLACEMENT_SAFETY.md](VM_PLACEMENT_SAFETY.md) (the guard that
requires XHA to be armed, and the statefile on the SR) and `ha_timeout_secs`
in `src/setup.rs`.
