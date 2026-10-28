# Changelog

All notable changes to TwinStor are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

TwinStor is in the 0.x tech-preview series: treat every release as
potentially behaviour-changing and read the notes before upgrading a live
pool.

## [0.7.3] - 2026-10-28

A fix release for 0.7.2, from fault campaigns on both test pools (heavy
packet loss, both hosts yielding at once, disk failures, router outages)
and a fresh-install run of the setup wizard. Upgrade both hosts: the new
handling of a split where both hosts yield acts only when both run 0.7.3.

### Fixed
- **Heavy packet loss no longer makes two writers in most cases.** A
  replication link that keeps dropping packets without staying down made
  both hosts read the other as gone and accept writes, and the resolution
  then discarded one side's acknowledged writes. The fence-peer handler now:
  watches the peer for a few seconds before it survives (arbitrated
  non-keeper: DRBD port and iSCSI portal for 10 s; fate-shared freeze mode:
  the DRBD port for 6 s); freezes I/O at entry when DRBD dropped its
  fencing suspend during a flap; keeps the freeze when a newer disconnect
  overtook its verdict; lifts the freeze itself, and reports no fencing,
  when the pair is connected again before it answers, and stays until the
  link has held for 20 s. Some loss remains possible, see Known
  limitations.
- **When both hosts yield, one of them takes over.** Arbitrated: when both
  hosts froze with guests and every reconnect aborts, the keeper resumes on
  its copy once the peer attests it is frozen at the same generation, takes
  the replication link down until its pending writes complete, and the
  other host stops its guests and resyncs. Fate-shared: a host whose copy
  holds everything the other's has takes the win, also after a daemon
  restart or a refused discard; when neither copy holds the other's, the
  keeper takes it after 90 s, in two published steps so that never both
  do; a host whose peer's copy is Inconsistent takes it at once. A host
  that took the win keeps serving through the next fence-peer run. The
  freeze budgets are staggered so the two hosts no longer step down
  together.
- **The losing host of a split-brain cuts its own iSCSI target before it
  lifts its frozen I/O,** in both modes, so a write still queued there is
  never acknowledged from the copy that is discarded. Its teardown now
  releases only the backstore behind that cut and rebuilds the LUN in place
  at the next start: deleting the portal could hang in the kernel after a
  timed-out login, and the host then rebooted itself.
- **An iSCSI portal is no longer unreachable for about 15 minutes after a
  frozen login.** The stock firewall answered the portal's retransmits with
  ICMP, so its socket lingered and LIO accepted no login meanwhile. The
  daemon now keeps a rule that answers such segments with a TCP reset.
- **A disk failure on the pool master no longer makes the slave take the
  mastership.** The slave read the master's Diskless state as stale and
  ran its emergency master transition, which could run a VM on both hosts
  at once. In fate-shared mode a Diskless host also keeps its path through
  the peer's portal during a link drop, and it stops its own target only
  after the peer has been gone for 10 s, logging its own session out
  first.
- **Arbitrated mode: releasing a whole-switch hold no longer reboots the
  host.** The release restarted XHA while DRBD I/O was suspended, and the
  XHA watchdog reset the host about 105 s later. A host with no guest now
  waits for I/O and for its HA statefile, repairing the statefile link if
  needed, and alerts after 5 minutes; a host running guests re-arms at once
  as before.
- **Arbitrated mode: losing only the gateway no longer stands a host down
  when the peer still answers on the gateway network.** The isolated-slave
  and isolated-master stand-downs first ping the peer's address when it is
  on the gateway network. A router outage seen by both hosts still holds
  as a whole-switch outage.
- **The stuck-Primary recovery never tears down storage or reboots a host
  that runs guests.** It held frozen copies on repeated aborted reconnects
  alone; it now also needs DRBD's own refusal and no guest, and otherwise
  holds and alerts.
- **Pool HA is armed again after a reboot that disabled it.** A master
  rebooted (or whose toolstack restarted) after an emergency HA disarm came
  back with pool HA off and left its stopped guests halted.
- **Write cache on hardware RAID volumes and SAS disks.** The cache check
  only spoke ATA, so these read as unknown and the pair as degraded. RAID
  volumes are now left to their controller, and other SCSI disks are read
  and set through the kernel.
- **`twinstor uninstall` leaves no state behind** after a wipe: it empties
  `/var/lib/twinstor` and `/run/twinstor`, and removes the backup copies
  setup and upgrades make of their files. Without a wipe the state stays.
- **Setup never wipes a live pair.** A re-run on a pair whose SR was
  renamed read as an unfinished install, and a rebuild could promote an
  empty disk; a live pair is now detected by the recorded SR uuid, the
  target IQN, the name or DRBD, and is never offered a re-run. The checks
  run before setup writes anything.
- **The setup wizard never offers an NVMe multipath path node or a disk
  reached over iSCSI** as the backing device: both looked like empty
  disks (the first was a boot disk on one server, the second the other
  host's TwinStor volume after an interrupted setup).
- Setup changes nothing on the host before its confirmation (it corrected
  an initiator name LIO refuses while still planning). A local SR left by
  the installer is named when it holds the disks. A failure after setup
  started says what to do next and never leaves the service stopped.

### Changed
- **`twinstor setup` with flags asks for a typed confirmation** before it
  erases the backing device, and without a terminal refuses unless `--yes`
  is given. Add `--yes` to scripted setups.
- `twinstor setup` with flags runs the wizard's requirement checks (a pool
  of exactly two hosts stops it; other findings are warnings) and refuses a
  backing device the wizard would not offer, with the reason;
  `--force-device` overrides that. The device a host already uses is
  exempt.
- On the second host, setup refuses a missing or wrong `--chap-password`
  and a topology that differs from the first host's, before changing
  anything.
- The first host waits up to 30 minutes for the second one (it was 5)
  before it promotes alone.
- `install.sh` refuses XCP-ng versions other than 8.3 (overridable with
  `TWINSTOR_SKIP_PREFLIGHT=1`), gives up the update check after a minute
  without network, and stops with the list of missing packages when yum did
  not install them all.
- `twinstor status` on a host receiving its first copy reads degraded with
  the copy's progress instead of critical, unless its own target is open
  meanwhile; other degraded reasons are listed first.

### Added
- **VMs marked auto_poweron start again after a reboot of both hosts.**
  XCP-ng tries them once per host boot, before the pool is whole, and HA
  refuses those starts while it cannot keep its failover plan, so they
  stayed halted. Once after it boots, the pool master now starts the VMs
  marked `other-config:auto_poweron=true` (with the pool's flag set, as
  XCP-ng requires) that have stayed halted since the boot, once both hosts
  are enabled, live and in the HA liveset, both copies are UpToDate and
  the SR is plugged on both. It never starts a VM HA still protects, one
  that ran since the boot or one whose disks are marked suspect, and it
  raises an alert listing what it started. `[ha] complete_autostart =
  false` turns it off.
- **The setup wizard checks the requirements first** (two hosts in one
  pool, same updates, every SR plugged) and, on a host alone in its pool,
  offers to join it to the other host's pool. As with any pool join, that
  host's root password becomes the other host's.
- In arbitrated mode the wizard lists the NICs with their speed, offers
  only those fit for a direct cable, configures both ends, and refuses a
  link with a switch behind it (read from the bridge's forwarding table).
  The second host follows the first host's topology.
- The wizard can destroy an empty local SR that holds the disks (typed
  confirmation; an SR with virtual disks is never touched), checks the
  pasted CHAP password before anything is erased, recognises a setup that
  stopped part-way and offers to run it again, and prints the equivalent
  `twinstor setup` command before its confirmation.

### Upgrading
- Upgrade the two hosts back to back with `twinstor upgrade-apply`. While
  one host still runs 0.7.2, a split where both hosts yield ends as on
  0.7.2. No configuration change; the install replaces the fence-peer
  handler, and the daemon adds its firewall rule at start.
- Scripts that run `twinstor setup` with flags need `--yes`.
- After a reboot of both hosts, the pool master starts the VMs marked
  auto_poweron that are still halted (see Added). Remove the mark from a VM
  that must stay off, or set `[ha] complete_autostart = false`.

### Known limitations
- **Heavy packet loss between the hosts:** when the replication network
  keeps dropping a large share of its packets (30% or more, for minutes)
  without cutting the hosts apart, both hosts can still accept writes for a
  moment, and the last few writes of the host that loses are discarded; a
  VM that ran on it may need a filesystem check. An alert names the VMs
  concerned. In arbitrated mode this needs both the direct cable and the
  switched network to drop packets at once. DRBD applies a fence-peer
  answer at its next state change, which a link that flaps every second
  can turn against the freeze.
- When such loss makes both hosts yield while each runs VMs, neither takes
  over by itself, because each copy may hold writes the other lacks: the
  pool stops serving until an operator picks the copy to keep. An alert on
  each host gives the steps.
- Arbitrated mode: if the direct link's address or NIC disappears (not a
  cable pull), DRBD does not use the switched path either; the host that
  does not keep serving stays frozen until the address is back.
- Arbitrated mode: a switch isolation of the pool master that outlasts the
  XHA timeout can reboot both hosts. Fate-shared mode: a cut longer than
  about 60 s can end with XHA rebooting either host.
- The wizard's switch-port check cannot tell a dedicated switch with nothing
  else on it from a cable, and needs the Open vSwitch backend (the
  default). During the wizard's pool join the master's password is visible
  in the joining host's process list (`xe pool-join` takes it only on its
  command line).

## [0.7.2] - 2026-10-25

Follow-up release to 0.7.1: the pool default HA timeout, quieter and safer
uninstall, and plain log messages.

### Changed
- **`twinstor setup` sets the pool's default HA timeout** (pool
  `other-config:default_ha_timeout`) to the mode's value: 90 s arbitrated,
  120 s fate-shared. A bare `xe pool-ha-enable` without `ha-config:timeout`
  otherwise arms XHA at 60 s, too short for the mode. Setup records the value
  it found, and `twinstor uninstall` puts it back if nobody has changed the
  key since. `twinstor upgrade-apply` does the same once on existing pools
  (see Upgrading). `twinstor doctor` reports the setting and how to fix it.
- Log, alert and CLI messages use plain descriptions instead of internal
  scenario tags. The `scenario` field takes the values `peer_emergency`,
  `local_emergency_wedge`, `xha_join_deadlock`, `masterless_pool` and
  `ha_deadlock_exit`; `heal_event` names are unchanged. Update any log filter
  that matched the old tags.
- The alert raised when pool HA is left disarmed says which timeout a bare
  `xe pool-ha-enable` uses (the pool default, or 60 s if unset) instead of
  30 s.
- `twinstor uninstall` reports commands that fail as expected (HA already
  off, no iSCSI session, setup key already gone) on one line instead of as
  warnings, and says whether the SR forget that follows a failed destroy
  worked.
- The bundle ships the README instead of `INSTALL.md`. The user
  documentation is at https://docs.vates.tech/tech-preview/twinstor/.

### Fixed
- **`twinstor uninstall` could check the wrong SR for running VMs.** The
  recorded SR uuid (`/var/lib/twinstor/sr-uuid`) survived uninstall and
  setup, so after a reinstall the safety check could count the VMs of a
  deleted SR, find none, and go ahead. Uninstall now removes the record,
  setup drops a leftover one, and the check prefers the SR it can find in
  XAPI over a recorded uuid.
- **The config file is never readable by other users while it is written.**
  It holds the CHAP secret. `twinstor setup` and `twinstor replace-backing`
  now write it to a temporary file that already has its final mode (0600
  for a new config, the existing mode otherwise), sync it before the rename
  and sync the directory after, so a crash cannot leave an empty config.

### Upgrading
- `twinstor upgrade-apply` sets the pool's default HA timeout on a pool that
  has none: to the pool's `ha-config:timeout`, or the mode's value when
  there is none. A value already there is left alone, as is a pool where
  the key was removed after TwinStor set it. The setting is recorded the
  same way as by setup, so uninstall puts it back.

## [0.7.1] - 2026-10-23

First public release. A fix release for 0.7.0, from the fault campaigns run
on both test pools after its release. Upgrade both hosts: the fate-shared
freeze-mode fixes act only when both run 0.7.1.

### Fixed
- **Fate-shared freeze mode: a short cut of both hosts no longer leaves both
  down.** A cut of a few seconds, with a brief reconnect in the middle, could
  release the fencing freeze the next fence decision depended on; both hosts
  then stepped down and one stayed down until a daemon restart. Lifting a
  leftover suspend now waits for the connection to hold and never runs under
  a fencing freeze, and a frozen copy that the last handshake shows as behind
  its peer steps down at once.
- **Fate-shared freeze mode: after a long cut only the losing host offers its
  discard.** The I/O lift at the freeze budget starts a new DRBD generation,
  which made the host drop its yield and claim guests it had already
  stopped. The yield record now follows the generation across every resume
  the daemon issues, durably.
- **The fence-peer handler records its yield at once** instead of about 11 s
  late: `drbdadm suspend-io` applies within seconds but did not return before
  its timeout.
- **Arbitrated mode: a yielded copy that is behind its peer steps down about
  10 s after the cut ends, not 44 s.** The decision uses DRBD's own refusal to
  reconnect, read from the kernel log, and waits up to 180 s for XAPI and the
  storage layer so the guests are stopped cleanly.
- **Arbitrated mode: a frozen host keeps XAPI calls out of its loop for the
  first two minutes,** so a slow call cannot delay its decision.
- **The stale-serving-Primary demote fires again on long-running hosts,**
  whose full kernel ring buffer hid new refusal lines from the count.
- **A host disabled by the crash boundary is enabled again when it ends up
  serving alone** (the winner died after the convergence).
- **Pool HA resets keep their settings and their re-arm obligation:** the
  survivor's reset restores `ha-host-failures-to-tolerate`, a failed or timed
  out disable keeps the obligation, the re-arm watchdog waits while XAPI has
  an HA task pending, and the peer-emergency cooldown applies only after a
  successful disable.
- **The halted-VM backstop starts at most three VMs per scan.**
- The crash-boundary restart alert names the host each guest was started on;
  `twinstor doctor` detects arbitrated mode the way the daemon does;
  `twinstor uninstall` says what to run by hand when XAPI does not answer.

### Added
- A guest that lands on a host whose copy is frozen (a live migration or HA
  start during the split) is logged once
  (`heal_event="guest_arrived_while_frozen"`).

### Upgrading
- Upgrade the two hosts back to back with `twinstor upgrade-apply`. While
  one host still runs 0.7.0, a cut ends as on 0.7.0. No configuration change;
  the install replaces the fence-peer handler.

### Known limitations
- Arbitrated mode: a switch isolation of the pool master that outlasts the
  XHA timeout can reboot both hosts.
- Fate-shared mode: a cut longer than about 60 s can end with XHA rebooting
  either host.
- A guest whose disk handle the crash boundary had to close itself cannot
  start on that host until its datapath is released; the alert names the
  cleanup.

## Before 0.7.1

TwinStor was developed in a private repository from 0.1.0 (June 2026) to
0.7.0 (October 2026) and tested by early users on their own pools. The main
steps:

- **0.7.0:** fate-shared freeze mode on by default (no acknowledged write is
  lost at a fate-shared split); the crash boundary (the losing side stops its
  guests before its copy is discarded); `twinstor discard-and-resync`; every
  decision path identifiable from the logs.
- **0.6.x:** stand-down of an isolated non-master host in arbitrated mode;
  resync rate scaled to the link; automatic recovery from a stuck DRBD
  handshake; bounded status probes.
- **0.5.x:** split-brain winner chosen from DRBD's generation lineage instead
  of the pool role; Xen Orchestra advertisement through XAPI `other_config`;
  health-gated `twinstor upgrade-apply`.
- **0.4.x:** stand-down of an isolated pool master in arbitrated mode; XHA
  re-admission and dual-master recovery; `twinstor replace-backing` for disk
  replacement.
- **0.1.x to 0.3.x:** dual-primary DRBD 9 with LIO iSCSI and multipath; the
  setup wizard; fate-shared and arbitrated modes; write-cache handling on
  consumer drives; XHA integration; split-brain detection and resolution.
