# TwinStor architecture

This document is for contributors: how TwinStor is put together, what each
source file does, and why the main design choices were made. What happens in
each failure case, from an administrator's point of view, is in
[BEHAVIOR.md](BEHAVIOR.md).

## Problem

A two-host XCP-ng pool with only local disks cannot live-migrate VMs, run
pool HA (XHA), or use any feature that needs shared storage. The usual
answers (a SAN, NFS, XOSTOR/LINSTOR) need external infrastructure or a third
host. TwinStor provides shared block storage from the two hosts and their
local disks alone.

## Scope

- Two hosts, never more.
- Runs in dom0: the storage layer must be up before any VM can start.
- DRBD 9 dual-primary, exported over iSCSI (LIO) and consumed through
  dm-multipath.
- XHA is the fencing authority and restarts VMs after a host failure.
  TwinStor manages the storage layer and drives recovery around it.
- No patches to SM or XAPI. TwinStor presents a standard LVM-over-iSCSI
  (`lvmoiscsi`) SR that the XCP-ng stack consumes unmodified.

## High-level architecture

```
┌──────────────── Host A (dom0) ─────────────────┐
│                                                │
│  ┌──────────┐    ┌─────────┐    ┌──────────┐   │
│  │ XCP-ng   │    │ iSCSI   │    │ twinstor │   │
│  │ XAPI     │    │ target  │◄───│ daemon   │   │
│  │          │    │ (LIO)   │    │          │   │
│  └────┬─────┘    └────┬────┘    └────┬─────┘   │
│       │               │              │         │
│  ┌────┴──────────────────┐     ┌────┴─────┐    │
│  │ LVM on DRBD device    │     │ drbdadm  │    │
│  │ /dev/drbd0            │     │          │    │
│  └────┬──────────────────┘     └──────────┘    │
│       │                                        │
│  ┌────┴────┐                                   │
│  │ DRBD 9  │  dual-primary                     │
│  │ kernel  │  protocol C (synchronous)         │
│  └────┬────┘                                   │
│       │                                        │
└───────┼────────────────────────────────────────┘
        │  replication network (fate-shared)
        │  or direct cable (arbitrated)
┌───────┼────────────────────────────────────────┐
│  ┌────┴────┐                                   │
│  │ DRBD 9  │  Host B: identical layout         │
│  │ kernel  │  also Primary                     │
│  └─────────┘                                   │
└────────────────────────────────────────────────┘
```

## Data path

```
VM I/O (on each host)
  -> iSCSI initiator (dm-multipath, prefers the local target)
    -> local iSCSI target (LIO, same IQN and LUN serial on both hosts)
      -> LVM PV on /dev/drbdX
        -> DRBD 9 dual-primary
          -> synchronous replication over the shared network (fate-shared)
             or the direct cable (arbitrated; the switched network is the
             alerted fallback path)
            -> local backing device (whole disk, partition or md-RAID array,
               referenced by its stable /dev/disk/by-id path)
```

Each host's VMs read from the local disk, with no network hop. Writes are
acknowledged only once they are on both disks (DRBD protocol C).
dm-multipath knows both portal addresses and prefers the local one
(priority 50 against 10).

Both iSCSI targets use the same IQN and LUN serial, so multipath sees them as
one device with two paths.

## Performance, layer by layer

Order-of-magnitude figures from test hardware (Gen4 NVMe backing, 10 GbE
replication link, encrypted stack), measured with fio. Absolute numbers
depend on hardware and tuning; what matters is the relative cost of each
layer. The dominant cost, synchronous replication, applies to any replicated
storage.

**Per-I/O latency** is the clean, reproducible signal. A 4k random read at
queue depth 1, measured at three points in the stack (3 runs each, within
about 1%):

| Access point | Layers added | 4k QD1 read latency |
|--------------|--------------|---------------------|
| raw `/dev/drbd0` (device level) | DRBD + dm-crypt + NVMe | ~75 us |
| iSCSI LUN (dom0) | + LIO/iSCSI + multipath | ~115 us |
| inside a VM (`/dev/xvdX`) | + tapdisk + Xen PV ring | ~150 us |

Each layer adds roughly 40 us per I/O, reproducibly. A VM read costs about
twice the device latency; the device-level row is not what a guest sees.

**Throughput** is plentiful but varies a lot from run to run (NVMe SLC
cache, queue depth, the replication link), so treat these as ranges:
device-level sequential reads measured roughly 1.3 to 5 GB/s; through the
iSCSI hop, around 1.3 GB/s consistently; inside a VM, a few hundred MiB/s.
Random-read IOPS land in the tens of thousands per host.

**Writes are bounded by synchronous replication.** A VM's durable sequential
write runs at a few hundred MiB/s (measured 0.2 to 0.4 GB/s, about 10k IOPS
at 4k, about 0.9 ms for a single QD1 write). The bound is DRBD protocol C
(local commit, network round trip, remote commit), not the iSCSI or tapdisk
layers. tapdisk write-back can make a guest's writes appear faster; the
durable figure is the one above.

In short:

- The per-layer latency cost is small and reproducible (about 40 us per
  layer).
- Reads are served locally, with no replication penalty: a few hundred MiB/s
  and about 150 us in a VM, up to several GB/s at the device.
- Durable writes are bounded by synchronous replication.
- On slower media (SATA SSD) the fixed per-layer latency matters less
  relative to the media, and dm-crypt's sequential cost approaches zero.

## Who does what

| twinstor | XHA | XAPI |
|---|---|---|
| DRBD bring-up, promote, demote | heartbeat and liveset | VM lifecycle |
| iSCSI target start and stop | statefile | LV activation per host |
| readiness gate before serving | survival rule | restart of protected VMs |
| multipath local-path preference and self-heal | watchdog fence | SR and PBD management |
| fence-peer and split-brain handlers | | |
| split-brain resolution and crash boundary | | |
| disk failure handling, write-cache control | | |
| PBD re-plug, HA re-arm, control-plane self-heal | | |
| alerts | | |

The boundaries:

- twinstor implements no heartbeat and no VM placement. XHA decides which
  host is alive and fences; XAPI places and restarts VMs.
- twinstor does act on VMs and on the HA control plane in specific recovery
  paths: the split-brain crash boundary stops the losing host's guests and
  restarts them on the winner, a halted HA-protected VM left behind by an
  incident is started again, pool HA is re-armed after a daemon-initiated
  disable or a fence, and several XHA and XAPI wedges are healed (see
  [CONVERGENCE.md](CONVERGENCE.md)). In a few wedges that have no other way
  out, the daemon forces a reboot of its own host.
- XHA does not manage DRBD or iSCSI. XAPI does not know about DRBD: it sees a
  standard `lvmoiscsi` SR.

## Source files

About 69,000 lines of Rust in 25 files under `src/`, roughly 40% of them in
unit-test modules (about 720 tests). Most decisions are pure functions of
observed state, tested without real storage. `state_machine.rs` holds more
than half of the code.

| File | Role |
|------|------|
| `main.rs` | Entry point: logging setup (the daemon writes `/var/log/twinstor.log` with file and line on every line; the CLI logs to stderr), runtime start, dispatch to `cli.rs`. |
| `cli.rs` | Command-line interface: every subcommand, `status` rendering (text, `--plain`, `--json`), the `upgrade-apply` health gate, `discard-and-resync`, `replace-backing`, `regenerate-config`. |
| `cmd.rs` | Bounded execution of external commands: every `drbdadm`, `targetcli`, `xe` and similar call goes through it with a timeout, and state-changing DRBD commands are logged. |
| `config.rs` | Loading and validating `/etc/twinstor/twinstor.toml`. |
| `setup.rs` | `twinstor setup`: detection, DRBD resource file, handlers, LVM filter, firewall, metadata, SR creation, XHA enable. Also renders the `.res` file for `regenerate-config` and reconciles the installed handler scripts at daemon start. |
| `wizard.rs` | The interactive setup wizard run by `twinstor setup` on a terminal with no arguments. |
| `uninstall.rs` | `twinstor uninstall`: guided teardown of XHA, SR, iSCSI, DRBD and configuration. |
| `drbd.rs` | DRBD control behind the `DrbdControl` trait: status parsing, `drbdadm`/`drbdsetup` commands, replication paths, the generation UUIDs of the last handshake (read from the kernel log). |
| `iscsi.rs` | The LIO target via `targetcli` (CHAP ACLs, discovery authentication), initiator sessions via `iscsiadm`, and the firewall rule that fences the peer's portal. |
| `multipath.rs` | Initiator-side health: local-path priority config, path-health probe and self-heal, iSCSI session `recovery_tmo` enforcement. Reported and self-healed, never used to gate the target. |
| `readiness.rs` | The pure storage-readiness predicate that gates the iSCSI target (see [VM_PLACEMENT_SAFETY.md](VM_PLACEMENT_SAFETY.md)). |
| `state_machine.rs` | The daemon: the tick loop and nearly all decision logic (bring-up, health checks, split-brain resolution, fate-shared freeze mode, arbitrated stand-downs, HA and XAPI self-heal, alerts). |
| `loser_boundary.rs` | The split-brain crash boundary: stops and confirms gone every guest on the losing host that uses the SR before its copy is discarded, then owes and runs their restarts (see [RECOVERY_CRASH_BOUNDARY.md](RECOVERY_CRASH_BOUNDARY.md)). |
| `xapi_storage.rs` | Calls into XAPI's local storage layer (`message-cli`, `DP.destroy`) to release the datapaths of guest domains destroyed outside xenopsd. |
| `peer_signal.rs` | The peer beacon on TCP port 7900: one line per connection carrying this host's gateway state, backing-device attestation, split-brain claim and bounce flag. |
| `xapi.rs` | XAPI queries (pool master, host enabled, HA state, VMs, PBDs), VM and HA operations, and alert delivery (with a kernel-log fallback and retry when XAPI does not answer). The local XHA liveset is read in `state_machine.rs` through xhad's `calldaemon`. |
| `advertise.rs` | The `twinstor-*` keys in host and SR `other_config` that Xen Orchestra reads (health, sync state, sync ETA). |
| `write_cache.rs` | Detects and disables the backing drive's volatile write cache on drives without power-loss protection (NVMe admin command, SATA `hdparm`). |
| `reboot_drain.rs` | `twinstor reboot-drain`: the shutdown-time hold that keeps a clean reboot from taking down the last up-to-date copy while the peer resyncs. |
| `doctor.rs` | `twinstor doctor`: diagnoses (and with `--fix` clears) an HA-state disagreement and stale VDI `activating` locks. |
| `diagnostics.rs` | `twinstor support-bundle`: logs, redacted configuration and state snapshots in one archive. |
| `heal_state.rs` | Exports the open heal episode (stage, next step) to `/run/twinstor` for `status` and the support bundle. |
| `failback_state.rs` | Exports the arbitrated failback state (hold, back-off, latch) and carries the `failback-replication` request to the daemon. |
| `render.rs` | Terminal output: colours, markers, TTY and `NO_COLOR` handling. |
| `sd_notify.rs` | systemd `READY` and watchdog notifications, without a dependency. |

Files under `config/`:

| File | Role |
|------|------|
| `twinstor-fence-peer.sh` | DRBD fence-peer handler: the gateway tiebreaker and the yield logic. |
| `twinstor-split-brain.sh` | DRBD split-brain handler: writes the marker the daemon acts on and sends an immediate alert. |
| `twinstor.service` | systemd unit for the daemon. |
| `twinstor-reboot-drain.service` | systemd unit whose `ExecStop` runs `twinstor reboot-drain` during shutdown. |
| `twinstor.logrotate` | Rotation for `twinstor.log` and `twinstor-fence-peer.log`, installed as `/etc/logrotate.d/twinstor`. |
| `twinstor.toml.example` | Annotated example configuration. |

The two handler scripts and the logrotate file are embedded in the binary;
the daemon rewrites the installed copies at every start if they differ, so a
binary upgrade refreshes them. Under `scripts/`: `install.sh` and
`uninstall.sh` (shipped in the release bundle), `make-bundle.sh` (builds the
bundle) and `gen-log-events.py` (generates the event appendix of
[LOG_REFERENCE.md](LOG_REFERENCE.md)).

## Code walkthrough

### main.rs

Sets up tracing with the `RUST_LOG` filter (default `twinstor=info`; a
`RUST_LOG` value replaces the default), in the host's local time. The daemon
logs to `/var/log/twinstor.log` without dropping lines under load, and every
line carries its source file and line number. Rotation is left to
logrotate. The CLI logs to stderr so that `status --json` stays parseable.

### cli.rs

Subcommands:

| Command | Purpose |
|---|---|
| `setup` | Configure this host. Interactive wizard with no arguments on a terminal; scripted with `--peer-ip`, `--backing-device`, `--direct-ip` (arbitrated mode), `--ping-target` (repeatable), `--chap-password`, `--full-sync`, `--allow-no-tiebreaker`. |
| `status [--json \| --plain]` | One-line verdict, then DRBD, readiness, iSCSI, multipath, tiebreaker and heal-episode state. |
| `doctor [--fix]` | Diagnose the toolstack knots that block VM starts on healthy storage. |
| `support-bundle [-o path]` | Collect the diagnostics archive. Run on both hosts. |
| `replace-backing <device>` | Point twinstor at a replaced backing disk and start the full resync. |
| `discard-and-resync` | Resolve a held split-brain by hand on the host whose copy is given up. Refuses while the daemon runs, while a VM on this host uses the SR, or when the generation lineage says this copy is the one to keep (unless `--discard-kept-copy`). |
| `failback-replication` | Arbitrated mode: move replication back to the direct link now. |
| `regenerate-config [--yes] [--persist-only]` | Refresh the DRBD resource file from the current template (guarded, verified, rolled back on failure). |
| `upgrade-apply [--check]` | Restart the local daemon for an upgrade only when it is safe (Connected, both UpToDate, no heal episode, peer daemon alive), then wait for reconvergence. |
| `sr record \| set \| show` | Inspect or repair the recorded SR uuid. |
| `uninstall [--yes] [--keep-data] [--force]` | Guided teardown. |
| `daemon` | Run the state machine (started by systemd). |
| `reboot-drain` | Shutdown-time hold (started by systemd, not by hand). |

`run_daemon` records the SR uuid if it is missing, delivers a pending
reboot-drain alert in the background, installs SIGINT and SIGTERM handlers
on a broadcast channel, and runs the state machine until shutdown.

### config.rs

Loads and validates `/etc/twinstor/twinstor.toml`. The file is written by
`twinstor setup` and should not need manual editing.

```toml
[node]
name = "node-a"        # local hostname
peer = "node-b"        # peer hostname

[network]
peer_address = "10.0.0.2"
drbd_port = 7789
# allow_no_tiebreaker = false   # set by setup --allow-no-tiebreaker

[storage]
drbd_resource = "data" # DRBD resource suffix (full name: twinstor-data)
backing_device = "/dev/disk/by-id/nvme-MODEL_SERIAL"
drbd_device = "/dev/drbd0"

[iscsi]
target_iqn = "iqn.2025-01.com.xcp-ng:twinstor-data"
lun_serial = "twinstor-data"   # must match on both hosts
portal_port = 3260             # default
chap_user = "twinstor"         # CHAP authentication (generated by setup)
chap_password = "abc123..."    # same on both hosts, passed to xe sr-create
initiator_iqns = [             # both hosts' initiator IQNs (from XAPI)
  "iqn.1993-08.org.debian:01:node-a",
  "iqn.1993-08.org.debian:01:node-b",
]

[timers]
poll_interval_ms = 1000        # tick interval, default 1 s
standalone_grace_secs = 30     # wait before acting on StandAlone, default 30 s

[failback]                     # arbitrated mode only
auto = true                    # daemon-initiated failback to the direct link

[ha]
readmission_assist = true            # automatic XHA re-admission recovery
restart_halted_protected_vms = true  # start protected VMs halted by an incident
auto_resolve_peer_emergency = true   # pool HA reset for a peer stuck in emergency mode
self_standdown_isolated_master = true  # arbitrated isolated-host stand-down
reconcile_stuck_xhad = true          # stop an orphan xhad so pool-ha-enable can succeed
readmission_flap_debounce = true     # brief flaps do not restart the peer-emergency clock

[reboot_drain]
enabled = true
max_secs = 600                 # upper bound on the shutdown-time hold
```

Every key in `[timers]`, `[failback]`, `[ha]` and `[reboot_drain]` is
optional; the values shown are the defaults. Validation rejects
`node.name == node.peer` and a partial CHAP configuration (`chap_user`,
`chap_password` and `initiator_iqns` must be set together or not at all,
since a partial set would start an unauthenticated target).

The topology is not a config key: arbitrated mode is recorded by
`/etc/twinstor/direct-peer` (the peer's direct-link address) and the
two-path connection in the DRBD resource file.

### drbd.rs

Wraps DRBD behind the `DrbdControl` trait so that the whole decision logic
runs against a mock in tests (`MockDrbd`, in the state machine's test
module, with configurable status, call recording and failure injection).

**Status parsing** (`parse_drbdadm_text`) reads `drbdadm status` into a
`DrbdStatus`: local role and disk, connection state, peer role and disk,
whether I/O is suspended by a user request (`suspended:user`, the fence-peer
handler's `suspend-io`) or by DRBD fencing (`suspended:fencing`), the resync
direction and its progress. Both DRBD 9 (`role:Primary`, `disk:UpToDate`,
`connection:Connected`) and DRBD 8.4 (`ro:`, `ds:`, `cs:`) formats are
accepted. When peer data is present without a `connection:` field (8.4),
the parser infers `Connected`. `SyncSource` and `SyncTarget` map to
`Connected`; `WFConnection` and `WFReportParams` map to `Connecting`.

**The trait** covers status, up and attach, primary, forced primary and
secondary, connect, connect with `--discard-my-data`, disconnect, suspend
and resume I/O, a new current generation, the live fencing policy and
connect interval, replication paths (steering and restore), the peer
object, and withdrawal of a pending discard offer. It also reads the
generation UUIDs of the last DRBD handshake from the kernel log (`dmesg`,
bounded), the evidence the split-brain resolver weighs.

### iscsi.rs

Manages the LIO target through `targetcli`.

**Start** (`IscsiController::start`):

1. If the target is already configured with a LUN mapped (and CHAP as
   expected), adopt it and do nothing else. This keeps a daemon restart from
   touching a target that serves running VMs.
2. `clearconfig confirm=True` (clean slate).
3. Wait until the DRBD device presents its real size. A device caught
   mid-promote briefly reports a phantom size of about 64 sectors, and LIO
   records the size at backstore creation; building on it would publish a
   malformed LUN. If the size stays wrong, the start fails and is retried.
4. Create the block backstore on the DRBD device and write the shared
   `vpd_unit_serial` to sysfs (both hosts use the same serial so multipath
   sees one device).
5. Create the target and its LUN.
6. With CHAP configured (`chap_user`, `chap_password` and `initiator_iqns`
   all present): `authentication=1`, `generate_node_acls=0`, one ACL per
   initiator IQN with the CHAP user and password, and discovery
   authentication with the same credentials (XCP-ng's SM runs
   CHAP-authenticated discovery). Without CHAP: `authentication=0`,
   `generate_node_acls=1`.
7. Create the portal on `0.0.0.0:<port>` last, so an initiator can only ever
   reach a fully built LUN.
8. `saveconfig`.

**Stop**: close the listening portal first (retrying initiators then get
connection refused at once instead of parking login threads in LIO), delete
the backstore (which releases the DRBD device), delete the target, then
`saveconfig`. A stop that could not delete the backstore is reported as a
failure.

**Restart safety**: `is_configured` returns "configured" on any error
(targetcli crash, timeout, unreadable output), so an error can never lead to
a destructive `clearconfig` under a target serving live VMs. In the other
direction, the first stop after a daemon restart checks whether a target
from the earlier run is still exported and tears it down, so a teardown path
reached before any start (a split-brain loser, a stand-down) does not fail
its demote on a device LIO still holds open.

### state_machine.rs

The daemon. A tick loop reads DRBD and the local environment and drives
state transitions and recovery.

#### States

```
Init ──► Online ──► Degraded
  ▲        │           │
  └────────┴───────────┘
```

| State | DRBD | iSCSI target | When |
|-------|------|--------------|------|
| Init | coming up | off | Bring-up: DRBD up, connect wait, promote, readiness gate, target start |
| Online | Primary | on | Serving storage, monitoring health |
| Degraded | Primary, Diskless | on while the peer is healthy, off otherwise | Local disk failed; DRBD serves through the peer's disk |

Both hosts run the same state machine independently and are both Online in
steady state. A host drops back to Init when it loses the Primary role (a
reconnect it could not promote after, a split-brain loser, a stand-down
rejoin).

#### Main loop

```text
loop {
    select! { shutdown => { enter_shutdown(); return }, tick => {} }  // poll_interval_ms
    feed the systemd watchdog
    stamp liveness and advertise storage state in XAPI
    if parked by an isolated stand-down: run only the rejoin gate; continue
    pool, HA, network and disk watchdogs        // each one a bounded "step"
    fence-verdict logging, peer-portal fence, freeze-mode steps
    match state {
        Init     => try_go_online()
        Online   => check_health(); check_paths()
        Degraded => check_degraded()
    }
}
```

Details that matter when changing the loop:

- Missed ticks are not replayed in a burst (a slow XAPI call can stretch one
  tick to seconds).
- A stop request is visible between steps, not only between ticks: once
  SIGTERM arrives, no new step starts, but the step in flight completes.
- Each step that runs longer than a threshold logs `tick step ran long` with
  its name.
- While a split-brain or a freeze is fresh (its first two minutes), and
  while this copy's I/O is frozen by a yield, the steps that call XAPI are
  skipped. During a cut the pool master's XAPI often does not answer, and
  each timeout would lengthen the frozen guests' pause or delay the
  resolution. Their alerts are sent afterwards.
- A host that stood its storage down (arbitrated isolation) runs only its
  rejoin gate and none of the pool or HA steps, so a host that has just left
  the pool never manages the pool's HA or re-promotes.
- While a heal episode is open, every log line carries its id
  (`ep{id=...}`).

The watchdog steps include: pool HA armed check and re-arm, flap-storm
detection, write-cache enforcement, peer-daemon liveness, tiebreaker health,
SR capacity, resync progress, hostname drift, XHA re-admission, masterless
and mutual-slave pool detection, the yield hold, the whole-switch hold, the
isolated-master and isolated-slave stand-downs, host auto-enable, halted
protected VMs, the pool's `ha-reboot-vm-on-internal-shutdown` policy, owed
crash-boundary restarts, stale VDI locks, the HA-armed mismatch, the peer's
emergency-mode wedge, and this host's own HA wedge.

#### Init: bring-up (`try_go_online`)

1. On the first attempt and every 30th: make sure the DRBD and iSCSI
   firewall ports are open (`iptables -C`, insert if missing) and activate
   the backing LV if it is one.
2. Read DRBD status; run `drbdadm up` if the resource is not up. Re-apply
   the split-brain safety policies to the live resource. A Diskless disk is
   re-attached.
3. **StandAlone at bring-up**: a host that boots StandAlone with an
   `Inconsistent` disk (a resync interrupted by a fence) can never be
   promoted, so bring-up runs the same paced, non-destructive reconnect as
   the Online path, plus a loser-only split-brain resolution: an
   `Inconsistent` replica discards (it was already invalid); an `UpToDate`
   one resolves only through the yield latch, the peer's beacon claim or the
   lineage. Bring-up never resolves as the winner on a guess. Healthy boots
   pass straight through. See [CONVERGENCE.md](CONVERGENCE.md).
4. A host that an operator discarded by hand (`discard-and-resync`) is not
   promoted or exported until the resync from the peer has started. A host
   latched as the loser of the current fork is not promoted.
5. **Promote, but not alone too early.** While DRBD is still connecting,
   the promote is deferred and `drbdadm wait-connect` honours the resource's
   startup window (`wfc-timeout 90` after a clean shutdown of a healthy
   pair, `degr-wfc-timeout 10` when the peer was already gone), so a
   staggered double reboot does not leave both hosts Primary alone. Further
   gates apply before a promote without a connected peer:
   - the **boot gate**: evidence that the peer is alive or serving (its
     iSCSI portal, its beacon, the XHA liveset), or the durable memory of
     having seen it serve since the last connection, forbids a lone promote
     with no time limit;
   - **fate-shared mode**: a lone promote is held for 10 minutes, because a
     peer that cannot be reached may be isolated rather than dead; after
     that, it promotes only if the gateway or a ping target answers;
   - both copies `Outdated`: the keeper (lower hostname) force-promotes, the
     other waits;
   - a `Consistent` copy against a Diskless peer (a plain promote fails in
     that shape): forced promote only after the peer's beacon has attested
     for a grace period that its backing device is gone, otherwise a
     guided-manual alert.
6. **Readiness gate** (`readiness::assess`, see
   [VM_PLACEMENT_SAFETY.md](VM_PLACEMENT_SAFETY.md)): re-read DRBD and refuse
   to start the target unless the local copy is Primary and UpToDate (or
   Diskless with a healthy peer), the peer is Connected and UpToDate or this
   host is a lone survivor, and XAPI reports the host `enabled=true`. A
   host that is not ready stays in Init and retries each tick; until then it
   is not a usable multipath path.
7. Start the iSCSI target (bounded at 60 s; a timed-out start is not
   retried for 5 minutes).
8. Transition to Online and spawn `plug_and_multipath` in the background.

#### Online: health monitoring (`check_health`, `check_paths`)

Runs every tick.

**Connection edges.** On a connect or disconnect the daemon re-asserts the
local-path preference at once (a reconnect makes XAPI replug the peer's
session, and multipath can briefly route guest I/O onto it). On reconnect it
also re-runs multipath discovery so the second path is picked up.

**Redundancy watchdog.** A peer that is down, disconnected or not UpToDate
leaves this host serving alone but still Online. After the StandAlone grace
the daemon raises `twinstor_degraded` and re-raises it every 24 hours until
the peer is a healthy copy again.

**StandAlone: clean rejoin or split-brain.** When DRBD is StandAlone the
daemon waits `standalone_grace_secs` (30 s) before the first reconnect,
then retries promptly. The wait is skipped when the answer is already known:
a fresh fence-peer yield, a split-brain the lineage decides
(`split_brain_fast_resolve`), and the freeze-mode cases. Two situations are
kept apart:

- **No split-brain marker** (a host that was merely behind: crash, power
  loss, reboot). The daemon issues a plain, non-destructive `connect`. If it
  succeeds, the behind host resyncs from the other and nothing is lost. If
  DRBD finds divergent generations, it disconnects again and runs the
  split-brain handler, which writes `/run/twinstor/split-brain`.
- **Marker present** (both hosts wrote while disconnected): the resolver
  runs, below.

A marker still present once the pair is Connected with both disks UpToDate
is stale and is removed, so it cannot make the next ordinary disconnect look
like a split-brain. Repeated aborted reconnects on a Primary that is
suspended and not serving (the "stuck Primary") lead to an in-place demote
so the resync DRBD is asking for can run; if that demote cannot complete,
the host forces a reboot.

**Split-brain resolution** (`resolve_split_brain`). The first signal that
applies decides:

1. **Yield latch.** This host's fence-peer handler yielded (froze its I/O)
   and the I/O stayed frozen, with no guest on the SR running here: it
   served nobody during the partition and is the loser. The verdict is
   latched on disk for the episode.
2. **Beacon claims.** The peer attests on the beacon that it took the win
   for this fork: loser. The peer yielded and never claims the fork: winner.
3. **Refused discard.** A host that did not yield, whose discard offer the
   peer refused (both offered), becomes the winner on the next round.
4. **DRBD generation lineage** from the last handshake: the copy that
   descends from the other is ahead and keeps; the other discards. In a true
   fork (both advanced), the copy with more un-replicated writes keeps, and
   `twinstor_split_brain_diverged` names the host whose writes are rolled
   back. A tie keeps on both sides, which holds both copies.
5. **Pool role**, when the generations are equal or unreadable: the pool
   master keeps, read through `is_pool_master_resilient` (live XAPI, else the
   local `pool.conf`) and accepted only if 10 reads 5 s apart (45 s) agree.
   When both hosts fall back to `pool.conf` and both read "not master", the
   lower hostname keeps. A freshness gate on the disk states then overrides
   this verdict: a stale copy never keeps, an UpToDate copy is never
   discarded on a guessed verdict.
6. **Undecidable**: nothing is discarded. The host keeps serving, raises a
   manual-review alert and retries on later ticks.

Every verdict is logged as `resolver_decision` with its source. Repeated
resolution cycles in a short window pause automatic resolution and hold
(`splitbrain_thrash`).

The **winner** keeps its data and serving, and reconnects with a plain
`connect` (never `--discard-my-data`). The **loser** first establishes the
crash boundary (`loser_boundary.rs`): every guest on this host that uses the
SR is stopped and confirmed gone, or nothing is discarded at all. It then
logs out its own loopback session, resumes its I/O, stops its target
(bounded at 30 s; a teardown that wedges forces a reboot), demotes, and
reconnects with `--discard-my-data`. Once the resync has started, the
stopped guests are restarted on the winner. If nobody takes the discard
offer, a yielded loser stays down (Secondary, target off) with the offer
parked; the parked offer is recorded on disk so a daemon restart re-adopts
it instead of promoting the discarded copy. The full sequence is in
[RECOVERY_CRASH_BOUNDARY.md](RECOVERY_CRASH_BOUNDARY.md).

Alert latching: each resolution alert latches only once XAPI has accepted
it (XAPI may itself be restarting mid-incident) and the latches reset when
the episode ends.

**Fate-shared freeze mode.** On a fate-shared pair where both hosts run it,
DRBD freezes I/O on both hosts at a disconnect, the host that keeps the
gateway resumes, and the frozen host either gives way or keeps its copy
depending on where guests ran; no reboot is involved. See
[FATE_SHARED_FREEZE_MODE.md](FATE_SHARED_FREEZE_MODE.md). On a fate-shared
pool, a disconnected Primary also fences the path to the peer's iSCSI portal
(firewall rule and offline SCSI devices) until the pair is Connected again,
so its guests cannot be carried onto the peer's copy.

**Disk failure.** If the local disk is Diskless (I/O errors made DRBD
detach) and the peer is Connected and UpToDate, the target stays up: a
Diskless Primary forwards all I/O to the peer's disk, so VMs and the XHA
statefile see no path change. Tearing the target down would force a
multipath failover that can stall in-flight I/O long enough for XHA to
fence the host. The host moves to Degraded and raises `twinstor_degraded`.
Without a healthy peer to serve through, the target stops. `Inconsistent`
(a resync in progress) does not trigger Degraded. A backing device whose
`/dev/disk/by-id` path no longer resolves raises `twinstor_backing_missing`
(the recovery is `twinstor replace-backing`).

**Pool HA watch.** Every 10 minutes the daemon reads `pool.ha-enabled`. If
HA has been off for 30 minutes, the pool master raises
`twinstor_ha_disarmed` with the re-arm command, re-raised every 24 hours
while HA stays off. The daemon re-arms pool HA itself only when it disabled
HA (a persisted re-arm obligation) or after the peer rejoins from a fence;
it never re-enables HA an operator turned off.

**Arbitrated isolation.** In arbitrated mode a host can lose its switch
link while replication and the HA statefile stay healthy over the direct
cable, so XHA does not see it as dead. The peer beacon tells a single-host
isolation (the peer's gateway is up) from a whole-switch outage (both
gateways down):

- an isolated **pool master** with the peer UpToDate stands its storage
  down after 10 minutes: it stops its target, logs out its initiator
  sessions and demotes, so its VMs cannot write through any path (including
  the peer's target over the direct link) and XHA restarts them on the peer.
  A durable marker keeps it parked across reboots while still isolated; it
  rejoins once its client network is stably back. Controlled by
  `[ha] self_standdown_isolated_master` (default on);
- an isolated **slave** with VMs stands down after 2 minutes, with pool HA
  enabled, for the same reason;
- in a **whole-switch outage** both hosts hold and keep running.

See [NETWORK_TOPOLOGY_SAFETY.md](NETWORK_TOPOLOGY_SAFETY.md).

**Background enforcement.** Alongside the above: the path watchdog
(multipath health probed about once a minute, self-healed, iSCSI
`recovery_tmo` re-pinned), write-cache re-enforcement (a drive that
re-enables its volatile cache after a controller reset is disabled again),
flap-storm detection (`twinstor_flapping_link` after 3 split-brain
episodes or 5 disconnects within 15 minutes), PBD re-plug when this host's
PBD is unplugged while healthy, heal-episode export, and the dual-master
detection behind `twinstor_ha_wedge`.

#### Degraded -> Online

Each tick checks whether the disk is still Diskless. The daemon re-attaches
every 30 ticks; once the disk is back it calls `try_go_online`. If the peer
stops being a healthy copy while this host is Diskless, the target stops.

#### Shutdown

On SIGTERM or SIGINT the daemon starts no new storage work, lets the step in
flight finish, clears its `twinstor-alive` stamp (so `upgrade-apply` on the
peer reads it as down until it restarts) and exits. It does not stop the
iSCSI target: the target and DRBD are kernel state and keep serving across a
daemon restart. When it stops because a release without freeze mode was
installed (a rollback), it restores the configured DRBD fencing policy on a
healthy pair and the pool's `ha-reboot-vm-on-internal-shutdown` setting.

#### Helpers

**`is_pool_master_resilient`** (in `xapi.rs`): asks live XAPI first and
falls back to the local `pool.conf` role when XAPI cannot answer. Returns
master, not master, or undeterminable, and whether the answer came from
live XAPI. Used by the split-brain resolver's role step and to scope
pool-level alerts to the master. Recovery decisions otherwise avoid calls
that XAPI forwards to the pool master, which may be the host that just
failed; they use local signals such as DRBD state, the beacon and the local
XHA liveset.

**`plug_and_multipath`**: runs after going Online. Waits up to about a
minute for XAPI to answer, discovers and logs in to both targets (local and
peer, up to 3 attempts), reconfigures multipath with the local-path
preference unless this copy's I/O is frozen, enforces `recovery_tmo` on the
sessions, and plugs this host's PBD for the TwinStor SR if it is detached
(up to 6 attempts, 10 s apart, since XHA may still be settling after a
failover). Single-flighted: a trigger that arrives during a run is
deferred until it finishes.

**`configure_local_prio`** (in `multipath.rs`): maps the SCSI host numbers
under `/sys/class/iscsi_host/` to the local and remote portals, writes
`/etc/multipath/conf.d/twinstor.conf` for the TwinStor WWID with
`prio weightedpath` (50 local, 10 remote), `path_grouping_policy
group_by_prio`, `failback immediate` and `no_path_retry 30` (with every
path gone, I/O is queued for 30 checker intervals, about 150 s, then
failed), and reconfigures multipathd.

#### Alerts

All alerts are XAPI messages with priority 1, visible in Xen Orchestra:

| Alert | Raised when |
|---|---|
| `twinstor_degraded` | Serving without redundancy (local disk failed, or the peer is not a healthy copy); re-raised every 24 h. Also a stalled resync. |
| `twinstor_split_brain` | A split-brain was detected, resolved, or is held (undecidable winner, crash-boundary hold, guests marked disks-suspect). |
| `twinstor_split_brain_diverged` | A true fork: the named host's un-replicated writes are rolled back. |
| `twinstor_ha_disarmed` | Pool HA off for 30 minutes; re-raised every 24 h. |
| `twinstor_ha_wedge` | Dual-master or HA wedge the daemon cannot resolve on its own. |
| `twinstor_flapping_link` | 3 split-brain episodes or 5 disconnects within 15 minutes. |
| `twinstor_replication_fallback` | Arbitrated: replication on the switch path for more than 2 minutes. |
| `twinstor_no_tiebreaker` | The fence-peer handler has nothing to probe, or every target has been unreachable for about 30 minutes. |
| `twinstor_peer_daemon_down` | The peer host is up but its daemon has not stamped itself alive for 15 minutes. |
| `twinstor_host_held_isolated` | A disabled host is kept disabled because its client network is unreachable. |
| `twinstor_cannot_go_online` | Bring-up keeps failing with errors for 5 minutes. |
| `twinstor_backing_missing` | The configured backing device path no longer resolves. |
| `twinstor_reboot_unsafe` | A reboot went ahead while this host held the only current copy (sent after the reboot). |

An alert latches only once XAPI accepts it. Until then it is retried, and it
is also written to the kernel log (`twinstor: UNDELIVERED ALERT`), so an
incident that coincides with a XAPI restart is not silently dropped.

### setup.rs

`twinstor setup` (interactive wizard on a terminal, or scripted with
`--peer-ip` and `--backing-device`).

**Detection**:

- Local hostname: `hostname::get()`.
- Peer hostname: from XAPI by the peer's address, falling back to
  `getent hosts`.
- Local IP: the source address of `ip route get <peer-ip>`.
- Backing device: no VG scan and no LV creation. The operator chooses a raw
  device (whole disk, partition or md-RAID array) with `--backing-device` or
  in the wizard, which lists candidates. The choice is resolved to a stable
  `/dev/disk/by-id` alias; for md arrays the `md-uuid-` alias is preferred
  over the name alias (stable across renames). Setup refuses a device with no
  stable alias.

**Sequence**:

1. Detect or validate every parameter.
2. Generate CHAP credentials: user `twinstor`, random 16-character
   alphanumeric password from `/dev/urandom`, unless `--chap-password` is
   given (second host).
3. Read both hosts' initiator IQNs from XAPI (`other-config:iscsi_iqn`),
   falling back to `/etc/iscsi/initiatorname.iscsi`.
4. Write `/etc/twinstor/twinstor.toml`.
5. Write `/etc/drbd.d/twinstor-data.res`, with explicit node-ids by sorted
   hostname.
6. Install the two handler scripts (to `/usr/lib/drbd/`) and the logrotate
   file.
7. Write the fence-peer inputs: `/etc/twinstor/ping-targets` and, in
   arbitrated mode, `/etc/twinstor/direct-peer`.
8. Add the backing device to the LVM `global_filter` in both
   `/etc/lvm/lvm.conf` and `/etc/lvm/master/lvm.conf` (SM runs LVM with
   the latter), so dom0 LVM never sees the VG through the raw device.
9. Disable the backing drive's volatile write cache if it lacks power-loss
   protection (see `write_cache.rs`).
10. Open the firewall ports (7789/tcp for DRBD, 3260/tcp for iSCSI) in
    `/etc/sysconfig/iptables` and in the live rules.
11. If the resource is already up from an aborted run, take it down. Refuse
    to wipe when a TwinStor SR already exists, unless `--full-sync` is
    given.
12. `wipefs -a` on the backing device, `drbdadm create-md --force`,
    `drbdadm up`.
13. Probe the peer's DRBD port for up to 15 s.

**First host** (peer not up yet):

- Wait up to 5 minutes for the peer to run setup and connect.
- Once connected: `drbdadm new-current-uuid --clear-bitmap` (both devices
  are freshly created and empty, so the initial sync is skipped), then
  `drbdadm primary --force`.
- Print the command to run on the second host (with `--chap-password`).
- With `--full-sync`: promote at once; the full sync starts when the peer
  joins.

**Second host** (peer up):

- Connect to the peer, skipping the initial sync (or with `--full-sync`,
  discard locally and sync from the peer). If the peer turns out to be
  syncing real data to this host, stay Secondary and let the daemon promote
  once UpToDate.
- Promote (dual-primary).
- Discover both targets, probe the SCSI id with the CHAP credentials, and
  `xe sr-create type=lvmoiscsi shared=true` with a `multihomelist` naming
  both portals and `chapuser`/`chappassword` (XAPI keeps the password in its
  secrets table for later PBD plugs).
- Plug the PBDs on both hosts.
- Enable XHA with `host-failures-to-tolerate=1` and an HA timeout of 120 s
  (fate-shared) or 90 s (arbitrated). See
  [HA_TIMEOUT_TUNING.md](HA_TIMEOUT_TUNING.md).

**Firewall handling** (`open_firewall_ports`): reads
`/etc/sysconfig/iptables`, detects the chain (`RH-Firewall-1-INPUT` or
`INPUT`), inserts the rules before the REJECT line, marked with a
`# TWINSTOR` comment. Idempotent. Also adds live rules.

### config/twinstor-fence-peer.sh

DRBD runs this handler when a host loses its peer. It is the tiebreaker that
lets two hosts decide without a third. Each run logs to
`/var/log/twinstor-fence-peer.log` and syslog and ends with one
`verdict=<id> exit=<code>` line; [LOG_REFERENCE.md](LOG_REFERENCE.md) lists
every verdict.

- It probes the default gateway and the addresses in
  `/etc/twinstor/ping-targets`, "any of N": if one answers, this host can
  serve clients. Each target gets one echo, then two more if the first is
  lost (1 s deadline each), so one dropped packet does not decide.
- **A target answers**: exit 4. DRBD marks the peer outdated and this host
  keeps serving. Before that, the handler checks whether the link came back
  during the run (exit 4, nothing diverged) and whether the peer's DRBD is
  listening but has not reconnected, in which case the lower hostname keeps
  serving and the other yields (freeze mode: it yields to a live peer
  daemon instead).
- **Promote time**: if this host is Secondary and the peer's DRBD port is
  listening, the peer is alive; the handler refuses (exit 1) and lets DRBD
  connect instead of outdating a live peer.
- **No target answers**: `drbdadm suspend-io`, record the yield, exit 1.
  The host stays frozen and serves nobody. In arbitrated mode, the handler
  also probes the direct link and records whether the peer is alive there
  (`yield_to_peer`) or the host is isolated.
- A yield already in force for the episode is kept. A run that waits too
  long behind another run stays frozen and outdates nobody.

With pool HA armed, a frozen host cannot write the XHA statefile, so XHA's
watchdog fences it if the freeze outlasts the HA timeout.

In arbitrated mode the resource uses `fencing resource-and-stonith`, so DRBD
suspends I/O the moment the peer is lost, before the handler runs: no write
lands while the outcome is undecided. Fate-shared freeze mode sets the same
policy live.

### config/twinstor.service

- `After=network-online.target`, `Before=xapi.service`: DRBD and iSCSI must
  be up before XAPI plugs PBDs.
- `Restart=on-failure`, `RestartSec=5`.
- `WatchdogSec=900`, `NotifyAccess=main`: the tick loop pings the systemd
  watchdog, so a hung daemon (not only a crashed one) is restarted. The
  window exceeds the longest bounded operation that runs inside one tick.
- `KillMode=process`: only the daemon process is killed. DRBD and LIO are
  kernel state and must persist across daemon restarts.
- `TimeoutStopSec=90`: on a stop the daemon starts no new storage work but
  lets the step in flight finish (a split-brain resolution that has just
  begun takes up to about a minute).

`twinstor-reboot-drain.service` is a oneshot whose `ExecStop` runs
`twinstor reboot-drain` during the shutdown transaction, ordered after
`xapi.service` so the hold runs while XAPI, XHA and the guests are still up.
Its `TimeoutStopSec=660` caps the hold.

## DRBD configuration

Generated by `twinstor setup` at `/etc/drbd.d/twinstor-data.res`:

```
# twinstor-config-version: 6
resource twinstor-data {
    protocol C;
    options {
        on-no-data-accessible suspend-io;
    }
    handlers {
        fence-peer "/usr/lib/drbd/twinstor-fence-peer.sh";
        split-brain "/usr/lib/drbd/twinstor-split-brain.sh";
    }
    startup {
        wfc-timeout 90;
        degr-wfc-timeout 10;
        outdated-wfc-timeout 10;
    }
    disk {
        on-io-error detach;
        fencing resource-only;      # resource-and-stonith in arbitrated mode
        c-fill-target ...;          # sized from the replication link
        c-max-rate 800M;
        c-min-rate ...;             # sized from the replication link
    }
    net {
        allow-two-primaries;
        timeout 50;
        ping-int 2;
        ping-timeout 5;
        ko-count 3;
        after-sb-0pri disconnect;
        after-sb-1pri disconnect;
        after-sb-2pri disconnect;
        verify-alg crc32c;
    }
    ...                             # on-host sections or the two-path connection
}
```

Key settings:

- **protocol C**: a write is acknowledged only once both hosts have it.
- **allow-two-primaries**: both hosts are Primary; there is no
  promote/demote coordination in steady state.
- **twinstor-config-version**: lets the daemon detect a resource file
  generated by an older release; `upgrade-apply` migrates it live when it
  can, and `regenerate-config` does it on demand.
- **on-no-data-accessible suspend-io**: a Primary with no up-to-date data
  reachable (local copy mid-resync, peer momentarily gone) suspends I/O
  instead of returning errors, which would reach the guest through the
  still-live path. I/O resumes as soon as data is accessible again.
- **startup timeouts**: a host that went down as half of a healthy pair
  waits up to 90 s at boot for the peer to connect, so a staggered double
  reboot does not leave both hosts promoting alone; a host that already knew
  the peer was gone waits 10 s and serves quickly.
- **on-io-error detach**: a failing disk is detached (the host goes
  Diskless) instead of passing errors up. The daemon moves to Degraded.
- **c-fill-target, c-max-rate, c-min-rate**: the resync controller, sized
  from the replication link so a resync makes real progress under load
  without starving live replication.
- **fencing**: `resource-and-stonith` in arbitrated mode (DRBD suspends I/O
  before the handler runs), `resource-only` in fate-shared mode (freeze mode
  sets `resource-and-stonith` live). Both call the fence-peer handler on
  peer loss.
- **timeout 50, ping-int 2, ping-timeout 5, ko-count 3**: `timeout` and
  `ping-timeout` are in tenths of a second. An idle connection is pinged
  every 2 s and dropped if a ping is not answered within 0.5 s, so real
  packet loss is detected within about 2.5 s. Under active I/O, a peer that
  stops completing writes is dropped after `ko-count` x `timeout`, about
  15 s. A congestion blip delays TCP rather than losing the ping answer, so
  short flaps are ridden without a disconnect. See
  [BEHAVIOR.md](BEHAVIOR.md).
- **after-sb-0pri/1pri/2pri disconnect**: DRBD never discards a diverged
  copy on its own, including when both hosts are Secondary at reconnect. On
  a split-brain it disconnects and runs the split-brain handler; the daemon
  then resolves as described above, loudly, or holds.
- **verify-alg crc32c**: makes `drbdadm verify` (online verification)
  available.

### Arbitrated mode: the two-path connection

In arbitrated mode the resource ends with one `connection` holding two
`path` blocks, the direct link first and the switched network second,
instead of per-host addresses:

```
    connection {
        path {   # the direct cable: the default replication path
            host node1 address 10.0.1.1:7789;
            host node2 address 10.0.1.2:7789;
        }
        path {   # the switched network: automatic fallback
            host node1 address 192.0.2.11:7789;
            host node2 address 192.0.2.12:7789;
        }
    }
```

The paths run in DRBD's default active-backup mode, never
`load-balance-paths`: that throughput option resets the whole connection on
any path change, a split-brain window on a dual-Primary pair (found in
physical path testing). DRBD has no path priority (only `new-path` and
`del-path`, and declaration order proved nondeterministic on DRBD 9.2.16),
so the daemon keeps replication on the cable in three ways:

- every daemon-initiated reconnect is steered: the fallback path is removed
  while the connection is down, so the connect can only land on the cable,
  then restored;
- replication running on the fallback for more than 2 minutes raises
  `twinstor_replication_fallback` and shows `repl path: switch` in
  `twinstor status`;
- failback is daemon-owned: a guarded automatic move (the side with fewer
  resident VMs briefly demotes and reconnects on the cable, only with both
  disks UpToDate and after a stability hold of 150 s, 10 minutes or 30
  minutes, escalating each time the link flaps back; after 3 such strikes it
  is latched to manual), disabled with `[failback] auto = false`, plus the
  manual `twinstor failback-replication`.

While replication runs on the fallback, the topology is temporarily
fate-shared, which is safer against a VM running on both hosts, not less
safe. See [NETWORK_TOPOLOGY_SAFETY.md](NETWORK_TOPOLOGY_SAFETY.md).

## Key design decisions

### Why iSCSI and not direct access to the DRBD device?

The iSCSI layer makes DRBD appear as a standard SCSI LUN, so XCP-ng's
existing `lvmoiscsi` SR driver, multipath stack and XAPI work unmodified.
Without it, TwinStor would need a custom SR driver, its own path management
and XAPI changes.

### Why dual-primary and not primary/secondary?

Both hosts serve storage locally: reads have no network hop and writes
replicate synchronously. When one host dies there is no promote delay, since
the survivor was already serving.

XAPI and SM already activate each LV on one host at a time, so dual-primary
is safe without a distributed lock manager.

### Why multipath over real addresses and not a floating IP?

Each host keeps its own address; dm-multipath knows both and prefers the
local one. There is no floating address to move, no gratuitous ARP and no
conflict with XAPI's network management, only the standard iSCSI multipath
XCP-ng already supports.

### Why delegate fencing to XHA?

XHA already detects host failure, fences through the Xen watchdog and
restarts VMs through XAPI. A second fencing mechanism in TwinStor would
conflict with it (double fencing, timing interference). TwinStor keeps the
storage consistent and heals the HA control plane around incidents; XHA
remains the authority on which host is alive.

### Why the gateway as tiebreaker and not a third host?

A host that cannot reach the network cannot serve clients. TwinStor uses
that as the tiebreaker: the host that reaches the gateway keeps serving, the
isolated host freezes. This is the same idea as the isolation response of
VMware vSphere HA. The gateway arbitrates without being a cluster member.

This relies on **fate-sharing**: in fate-shared mode, replication, the XHA
heartbeat and client traffic ride the same physical path, so a host is
either fully connected or fully isolated, and there is no steady state where
replication is down while the host still reaches the gateway. A dedicated
replication link brings that state back; arbitrated mode (setup with
`--direct-ip`) handles it with `resource-and-stonith` fencing, the
direct-link probe and the peer beacon. See
[NETWORK_TOPOLOGY_SAFETY.md](NETWORK_TOPOLOGY_SAFETY.md) and the FAQ's
explanation of fate-sharing in [FAQ.md](FAQ.md).

### Why skip the initial sync by default?

Both backing devices have just been wiped. `drbdadm new-current-uuid
--clear-bitmap` marks both copies UpToDate at once, avoiding hours of
syncing zeros on large disks. `--full-sync` is there for devices with
existing data.

### Why KillMode=process?

DRBD and the LIO target live in kernel modules and persist across process
restarts. If systemd killed the whole cgroup on a daemon restart, it could
take the iSCSI target down under running VMs. `KillMode=process` kills only
the daemon.

### Why does `is_configured` answer "yes" on error?

The alternative, answering "no" and running `clearconfig`, would destroy a
target serving live VMs whenever `targetcli` crashed or timed out. Assuming
the target is up and skipping reconfiguration is the safe default.

## Failure scenarios

A summary; [BEHAVIOR.md](BEHAVIOR.md) has every case with what serves, what
happens to VMs and what to do.

| Scenario | What happens |
|----------|-------------|
| Host dies | The survivor was already serving. XHA restarts the protected VMs on it. The dead host reboots, twinstor starts, DRBD reconnects and resyncs only the changed blocks. |
| Network partition (fate-shared) | DRBD runs the fence-peer handler on both hosts. The host that reaches the gateway keeps serving; the isolated host freezes and serves nothing. In freeze mode, I/O is frozen on both hosts until the handlers decide. |
| Direct link lost (arbitrated) | Replication fails over to the switched path; `twinstor_replication_fallback` after 2 minutes; failback to the cable when it is stable again. |
| twinstor crash or restart | systemd restarts it. DRBD and iSCSI are kernel state and keep serving. No I/O impact. |
| Local disk fails | With a healthy peer: the target stays up and serves through the peer's disk (no path change, no fence), the host enters Degraded and alerts; the disk is re-attached and resynced when it returns. Without a healthy peer: the target stops. |
| Short network flap | DRBD stays Connected; no fence-peer run, no XHA action. |
| Graceful reboot | The daemon stops, the peer keeps serving, and the reboot-drain hold delays the reboot only if this host has the only current copy while the peer resyncs. |
| Both hosts boot together | Each waits up to 90 s for the other to connect before promoting, so they come back as a synchronised pair. |
| Split-brain | Resolved from the yield, the beacon claims or the DRBD generation lineage, falling back to the pool role. The loser stops its guests on the SR, discards and resyncs, and the guests restart on the winner. Undecidable: nothing is discarded, alert, retry. |

## What twinstor does not do

- Heartbeat or host failure detection (XHA)
- VM placement (XAPI; twinstor only makes a host that is not ready an
  unusable path)
- Clustered file system or distributed locks (XAPI and SM activate each LV
  on one host)
- More than two hosts
- NVMe-oF (iSCSI today; NVMe-oF could replace it later)
