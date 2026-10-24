# Changelog

All notable changes to TwinStor are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

TwinStor is in the 0.x tech-preview series: treat every release as
potentially behaviour-changing and read the notes before upgrading a live
pool.

## [Unreleased]

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
  XHA timeout can reboot both hosts (BEHAVIOR.md, case 39).
- Fate-shared mode: a cut longer than about 60 s can end with XHA rebooting
  either host (BEHAVIOR.md, Known limitations).
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
