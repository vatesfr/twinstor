# Changelog

All notable changes to TwinStor are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

TwinStor is in the 0.x tech-preview series: treat every release as
potentially behaviour-changing and read the notes before upgrading a live
pool.

## [0.7.6] - 2026-11-21

A returning host whose resync DRBD skipped is no longer served: TwinStor
detects the skip, rebuilds that copy with a full resync, and only then
serves it. A host that stood aside while its peer kept serving no longer
comes back and serves its older copy alone, and in arbitrated mode a
survivor frozen after XHA fenced its peer resumes sooner once the fenced
host is back. The DRBD configuration version is now 8; `upgrade-apply`
migrates it live (see Upgrading).

### Storage
- **A resync that DRBD skipped is detected and rebuilt.** With the DRBD
  version shipped in XCP-ng 8.3 (9.2.16), a connection drop while a
  returning host negotiates its resync can make DRBD skip that resync on
  the next reconnect and mark the host's stale copy UpToDate. Earlier
  TwinStor versions could then promote and serve that copy, and writes the
  peer had acknowledged were lost. TwinStor now reads this from the
  kernel log, records it in `/var/lib/twinstor/resync-skipped`, keeps the
  copy from being promoted or exported, rebuilds it with a full resync
  from the peer (`drbdadm invalidate`), and serves it only once that
  resync is under way and the copy is UpToDate again. The record survives
  a daemon restart and a reboot, and is cleared only by that full resync.
- **No promote while a resync from an UpToDate peer is pending.** A
  returning host is promoted only once its resync has started, no longer
  while it still waits for the bitmap exchange. Its iSCSI target still
  starts only once its copy is UpToDate.
- **Arbitrated pools: the skip is prevented in the usual case.** When the
  connection drops before the returning host's resync has started,
  TwinStor re-creates that host's DRBD connection before DRBD reconnects,
  so the next handshake runs the normal short resync instead of skipping
  it. The full rebuild above remains the fallback.
- **DRBD configuration version 8: `max-buffers 16384`.** DRBD's default
  (2048 pages) kept a full resync far below the link speed. The new value
  is applied live by `upgrade-apply`, with no reconnect, and each host
  uses its own value. Full resyncs are several times faster once both
  hosts are upgraded: on the test rig (NVMe, 40 GbE link) a 466 GiB full
  resync took 16 minutes, against 70 to 76 minutes before.
- **A host that stood aside no longer serves an old copy alone.** When a
  host loses replication and its fence-peer handler makes it stand aside
  (frozen, serving nothing) while the peer can keep serving, TwinStor now
  records that on disk (`/var/lib/twinstor/boot-gate-yielded`) once the
  stand-aside has lasted 20 s with the pair still disconnected; in tests the
  record was written about 28 s after the host stood aside. The record also
  survives the steps TwinStor itself takes later on a host that is still
  standing aside: in fate-shared mode the freeze budget resumes that host's
  I/O to stop its guests, which starts a new DRBD generation, and the record
  is marked on disk before that so it still holds at the next boot. If that
  host is then fenced or rebooted and comes back while the peer is down, it
  no longer promotes its copy alone, which silently dropped every write the
  peer had acknowledged in between. It holds its storage, raises an alert,
  and rejoins and serves by itself as soon as the peer is back and the
  resync completes. If the peer is lost for good, run
  `drbdadm primary --force twinstor-<resource>` on the held host; writes
  made only on the peer are then lost. The record is cleared once the pair
  is connected again, or once the host has served its own copy since. A host
  whose DRBD disk comes back UpToDate (it fenced the peer and kept writing)
  is not held. When both hosts lose power, recovery is unchanged if
  replication was still connected, or was lost less than about 20 s before
  the power went and TwinStor had not yet begun to step that host down.
- Arbitrated mode: when XHA fenced one host and the survivor froze its own
  I/O on the disconnect, the survivor now resumes sooner once the fenced
  host is back. Once DRBD has reconnected with the survivor as the resync
  source (or with nothing to resync), TwinStor re-checks the lift every
  second until the connection has held 5 s, instead of once per regular
  pass, which takes several seconds during a management cut. The
  conditions are unchanged (the pair connected, no split-brain, the
  survivor the resync source or nothing to resync, the connection held
  5 s) and are all re-read at every check; the second confirming read now
  comes one second after the first instead of one pass later. A survivor
  still frozen when the returning host starts a full rebuild is lifted the
  same way. On the test rig, I/O resumed 6 to 10 s after DRBD reconnected
  (20 to 23 s with 0.7.5).

### Alerts and status
- A skipped resync raises an alert. In the usual case the rebuild starts
  at once, and the alert says so: this host's copy is being rebuilt with a
  full resync from the peer, the pool runs on a single copy until it
  completes, no action is needed, and the peer must not be powered off
  meanwhile. While the rebuild waits (the peer not UpToDate, a split-brain
  or an operator discard pending), an alert says what it waits for, and a
  new one is sent when the reason changes (at most every 10 minutes). When
  the rebuild starts, the "being rebuilt" alert is sent unless the daemon
  itself delivered one of these waiting alerts for the skip; one delivered
  later from the held-back alerts can be followed by it. If the record
  cannot be written, the alert says the copy is held and names the file,
  so the filesystem holding it can be checked. A host just fenced and
  rebooted usually finds XAPI not answering yet when its rebuild starts:
  the "being rebuilt" alert is then raised again every 30 s, also after a
  daemon restart, for as long as the rebuild runs. If the rebuild
  completes and nothing about the skip had reached XAPI, a single alert
  says so instead, in the past tense: DRBD skipped the resync this host's
  copy needed, TwinStor rebuilt the copy with a full resync from the peer,
  the resync has completed, and no action is needed. That alert waits
  until the pair is connected with both copies UpToDate. A copy an
  operator forced out of its rebuild, in the boot the daemon finds it,
  gets no such alert. The alert is itself dropped if XAPI stays
  unreachable 15 more minutes.
- On a host that is already Primary when the daemon starts and whose
  kernel log shows a skipped resync (a release without this check promoted
  it), TwinStor changes nothing, because the copy may be serving, and
  raises an alert recommending `drbdadm verify twinstor-<resource>`, or a
  rebuild with `drbdadm invalidate` in a maintenance window.
- `twinstor status` shows the rebuild hold as the `resync_skip_repair`
  stage, the startup line logs `resync_skip=`, and
  `twinstor upgrade-apply --check` refuses while a record is open.
- The alert for a DRBD configuration older than the binary expects no
  longer names what the newer template adds; it asks to run
  `twinstor upgrade-apply` or `twinstor regenerate-config` on each host,
  one at a time.
- An alert XAPI could not take is now retried by the daemon itself once
  XAPI answers again, no longer only after another alert is sent: the first
  retry comes up to about two minutes after XAPI is back, then about every
  30 s. This applies to alerts not raised again for about a minute (one
  raised again is sent by its own next attempt). It is not attempted in the
  first two minutes of a split-brain or of a freeze, while this host's own
  I/O is frozen in freeze mode, or while the pool master does not answer.
  Undelivered alerts are still dropped after 15 minutes.
- While a host is held alone, `twinstor status` on that host shows the hold
  on its `auto-heal:` line and the force command on its `next:` line, and
  the daemon log records it. The status line asking not to run manual
  recovery commands now makes an exception for a command named on the
  `next:` line.
- The way out given by the alert for a host held as the loser of a
  split-brain now also removes `/var/lib/twinstor/boot-gate-yielded`, so a
  host that followed it after a reboot is not then held by the new record.
- The recovery steps in the alert for a split-brain where both hosts hold
  as the loser no longer ask to stop when the dry run
  (`drbdadm -d adjust`) prints a `drbdsetup net-options` line. That line
  is expected, on fate-shared pools in every case: TwinStor sets some
  connection options at run time, and the steps re-create the connection
  from the configuration file anyway.

### Known limitations
- While a copy is rebuilt after a skipped resync, the pool runs on the
  peer's copy alone. Do not power off or restart the peer until the
  resync completes (`twinstor status`).
- If the peer is lost for good during a rebuild, run
  `drbdadm primary --force twinstor-<resource>` on the held host, accepting
  that recent writes may be missing. Do it while the TwinStor daemon is
  running: it then serves that copy for the rest of the boot. A force made
  while the daemon is stopped is not seen if the host reboots first, and
  the copy is held again.
- Do not downgrade a host while `/var/lib/twinstor/resync-skipped`
  exists: an older release ignores it and can serve the stale copy. Wait
  until the rebuild has completed and the file is gone.
- During a rolling upgrade, a host still running 0.7.5 or earlier has no
  skipped-resync defence until it is upgraded.
- Fate-shared pools do not get the connection re-creation: a skip there is
  always repaired by the full rebuild.
- The connection drop that leads to a skip comes from DRBD itself. It can
  still cost a second reboot of the returning host, or a longer freeze of
  the survivor, before the pair heals.
- The alert for a skipped resync on a host that was already Primary is
  raised again after each daemon restart in the same boot. TwinStor never
  rebuilds that copy by itself.
- If XAPI does not answer from the skip until 15 minutes after the rebuild
  completed, no alert about the skip reaches XAPI; only the daemon log
  records it.
- If the kernel log cannot be read, TwinStor holds the promote for 60 s
  from the first failure in a row, then proceeds without the check, with
  a warning.
- During a rebuild the peer holds the only valid copy. If it loses power,
  writes it acknowledged just before can be lost when its drives do not
  keep completed writes. Drives with power-loss protection, or a UPS, are
  recommended.
- In rare cases a copy is rebuilt with a second full resync (the daemon
  stopped between the invalidate and its record, or a host rebooted while
  the record was still open). This costs time, not data.
- A host that dies abruptly while the pair is in sync (a crash, a power
  loss of that host alone, an HA fence while replication was still
  connected, a reboot) keeps no record of it. If its peer keeps serving and
  then goes down before that host is back and resynced, the host still
  promotes its own copy alone when it returns, and the writes the peer
  made in between are lost. Nothing on the returning host tells this apart
  from both hosts losing power together.
- If the network goes down more than about 20 s before both hosts lose
  power (for example hosts on a UPS and the switch not), both hosts record
  that they stood aside. If only one of them comes back, it holds its
  storage even though its copy is current, until the other host returns or
  an operator runs `drbdadm primary --force twinstor-<resource>` on it.
- After a whole-switch outage longer than about 20 s in which both hosts
  stood aside, if one host is then lost for good and the other reboots,
  the survivor holds its storage and needs
  `drbdadm primary --force twinstor-<resource>` to serve again.
- A host that stood aside and afterwards kept or was forced to serve its own
  copy, and is reset or powered off within seconds, before TwinStor has seen
  it serving, can hold its storage at a boot with the peer down. The same
  applies if the pair reconnected and resynced only while the TwinStor
  daemon was stopped. `drbdadm primary --force twinstor-<resource>` on that
  host is the way out.
- The hold's alert may not reach the pool's message list: XAPI is not
  available on a host held alone. `twinstor status` and the log on that
  host show the hold regardless.
- Arbitrated mode: the survivor's frozen I/O still waits for the fenced
  host to boot and reconnect, then for the connection to hold 5 s. After
  the lift, the guests' I/O returns once iSCSI has logged the survivor's
  path back in and multipath has reinstated it, on their own retry timers.
  The whole freeze must still end within the pool's HA timeout, or XHA
  resets the survivor and both hosts reboot; a fenced host that does not
  boot and reconnect well within that timeout still sees both hosts reboot.
- The other limitations of 0.7.5 are unchanged.

### Upgrading
- Upgrade the two hosts one after the other with `twinstor upgrade-apply`.
  No change to the TwinStor configuration file is needed, and it is not
  rewritten.
- The DRBD configuration version is now 8 (`max-buffers 16384`).
  `twinstor upgrade-apply` migrates the resource file live, as for 0.7.5;
  a pool upgraded from 0.7.4 or earlier also gets the 0.7.5 migration (see
  0.7.5, Upgrading).
- A 0.7.5 peer keeps its own behaviour until it is upgraded.

## [0.7.5] - 2026-11-19

TwinStor no longer changes pool HA across a management partition, its HA
repairs are re-checked when they fire, an arbitrated-mode storage freeze is
fixed, and fate-shared replication rides out heavier packet loss. Validated
on a fate-shared and on an arbitrated lab pool. Upgrade both hosts; the
DRBD resource file is migrated live (see Upgrading).

### During a partition
- **No HA change across a management partition.** When one host loses its
  client and management network but keeps replication, TwinStor no longer
  disables pool HA, disarms a host or restarts a toolstack across the cut.
  Every such action now checks, when it fires, that the peer's management
  address answers: a TCP connect to port 443 every 10 s, answered by at
  least 4 of the last 6 probes including the last one. A repair triggered by
  a missing answer (the peer's xapi silent, the local HA daemon not in the
  liveset) also waits 330 s after the network returns, the time XAPI took to
  reassemble the pool by itself in tests. A withheld action is logged
  (`ha_action_withheld`), shown by `twinstor status`, and retried later.
- A host that XAPI holds in its HA startup loop (locally armed, not in the
  liveset, the peer's management address silent) is never disarmed,
  transitioned to master or reset by TwinStor, whatever replication says.
  The whole-switch hold keeps its exemption.
- Pool HA is re-armed only when TwinStor recorded that it turned it off.
  Pool HA found off after a peer rejoin, with no such record, raises an
  alert instead of being re-enabled (it overrode an operator's disable in a
  test). Every local emergency disarm TwinStor makes is now recorded.
- A host held out of the pool in XAPI's HA startup loop alerted "stop xhad on
  both hosts for 90s, then re-arm" and could restart its HA daemon. Neither
  happens any more on such a host, nor on a pool whose HA reads off. An
  unknown pool HA state does not hold them: a host in a real join deadlock
  often cannot read it.
- A slave cut off from its pool master, on a pool whose HA an operator
  had just turned off, raised the join-deadlock runbook ("on the pool
  master run 'xe pool-ha-disable' ...") every few minutes: pool HA could
  not be read from there, and its HA daemon's exit had cleared the armed
  flag that held such a host back. The revive and the runbook alert now
  also wait while a slave is cut off from its master's management address
  with the peer alive on replication, and when the host's own HA daemon
  last exited with "Invalid pool state (1906)" (what it logs when pool HA
  is disabled) and nothing restarted it.
- While the peer's management address does not answer, the probes that
  could only time out are not sent, and a pool slave skips its XAPI
  housekeeping (every call would go to the unreachable master), which keeps
  the daemon's check loop at its normal pace during a cut.
- On a slave whose pool master's management address is silent, the
  once-a-minute check for an unplugged TwinStor PBD no longer asks the
  master (the call could only time out, holding the daemon's loop for
  15 seconds every minute).

### Repairs
- Every pool HA disable and enable the daemon issues on its own (the
  peer-emergency reset, the recorded-disarm re-arm, the re-admission
  recovery, the survivor's dual-master reset, the re-arm owed after
  TwinStor's own disable, the masterless recovery; not setup or the
  doctor's operator-run fix) runs outside the check loop and re-checks,
  right before each step, that the peer's management address answers (not
  on the masterless recovery's lone-survivor path, whose premise is a peer
  gone), that this host's pool role is unchanged, that XAPI does not hold it
  in its HA startup loop and that no host is in maintenance. Before a
  disable, pool HA must read on and no pool HA operation may be pending in
  XAPI; otherwise nothing is issued. A disable counts as TwinStor's only if
  it returned, or got no answer in time and pool HA then reads off; any
  error answer from XAPI with pool HA off afterwards is not taken as
  TwinStor's: nothing re-enables it, and an alert says that pool HA is off,
  that TwinStor's own disable was refused, and how to re-enable it by hand.
  A withheld enable is left to the re-arm obligation (on the lone-survivor
  path, to its own retry every 2 minutes: at most 5 enables issued and 30
  minutes in all, each only while this host's copy serves and the peer is
  still gone; once the peer is back, the normal re-arm takes over). The survivor's
  dual-master reset no longer writes the pool's failures-to-tolerate after
  its re-enable: pool HA disable and enable leave it unchanged, and writing
  back a value read before the disable could undo an operator's change made
  meanwhile.
- The peer's xapi role is read from its real answer: a slave wedged
  disarmed under an armed pool, or one just released from the HA startup
  loop, no longer reads as the pool master.
- While pool HA is on and the peer's management address answers, the pool
  master checks every minute whether the peer's XAPI answers as locally
  disarmed under the armed pool, so the reset that frees such a peer starts
  before XAPI's own HA restarts the peer's VMs elsewhere.
- The peer-emergency reset (a pool HA disable and re-enable) acts only when
  the peer's xapi answers that it is locally disarmed under the armed pool,
  on two probes 30 s to 5 minutes apart (the second taken 30 s after the
  first, not at the next minute's check), and the probe the reset fires on
  must itself give that answer; a single normal answer in between, as the
  disarmed slave's xapi gives while it restarts itself, does not reset the
  count, two in a row do. Its older signature (the peer's agent not
  answering) and the fast paths that fired during a normal rejoin are gone;
  `[ha] readmission_flap_debounce`, which tuned that signature, is still
  accepted but has no effect.
- The peer-emergency reset is limited by a budget instead of a one-hour
  cooldown: at most 3 resets in 6 hours, at least 10 minutes apart (an hour
  after a reset that did not free the peer), kept for the boot across daemon
  restarts, with each reset's pending verdict (a daemon restarted past the
  verdict's time counts that reset as not having freed the peer); a budget
  that cannot be written runs no reset. A reset whose disable was refused or
  never issued is given back;
  one that timed out may still take and counts. Whether a reset freed the peer is
  read from the peer's own xapi answer (two normal answers in a row after
  the reset, none locally disarmed), not from its agent, which answers
  throughout the wedge: the "freed, no action needed" alert is sent only
  then; a peer that answers locally disarmed again within 30 minutes of
  that verdict is the same wedge, and the reset counts as not having freed
  it. The verdict starts once pool HA reads on with no HA task pending
  (after the re-arm record's enable when the reset's own did not run), and
  whether a timed-out disable took is read from the re-arm record; every
  such wait ends 30 minutes after the reset's cycle, the reset then counting
  as not having freed the peer, with an alert saying only what was read. A
  verdict taken up again past that bound (a daemon restart, mastership
  back) is never reported as freed. Pool HA turned off by hand while the
  verdict probes the peer ends that verdict with no judgement: no hour's
  hold and no alert (the HA-off alert already says it). A reset that did
  not free the peer raises one alert saying so, until when TwinStor waits, and the manual
  pool HA cycle. A second wedge on the same master soon after a reset was
  left unrepaired for the rest of the hour, with
  nothing saying why, while XAPI showed the wedged host's running VMs as
  halted. While the budget holds a reset, TwinStor logs it as withheld with
  the rule that holds it, the PeerDisarmed status says until when, and for
  the long holds (the budget, the hour after a futile reset) one alert says
  that the peer's VMs are unprotected and may be restarted on another host,
  with the manual pool HA cycle.
- The re-arm record now says what TwinStor knows: its disable is pending
  (issued, not yet seen to take, for 7 minutes, and up to 4 minutes after
  TwinStor's own HA task was last seen pending; a cancelled task ends it once
  pool HA reads on),
  owed (pool HA read off after TwinStor's own
  disable) or enabled (TwinStor's enable call returned success, or pool HA
  has read on and stayed so for the settle period with no HA task pending).
  Only an owed record leads to an enable, and every enable reads the record
  again right before it runs; a pool HA-off seen after the record became
  enabled, or after a pending disable's window, is not TwinStor's and is
  never re-enabled (an alert says so). A record written by an earlier
  version reads as owed; the file stays readable by 0.7.4.
- A host that TwinStor itself disarmed (a whole-switch hold, a stand-down)
  and that is back in the HA liveset still locally disarmed under the armed
  pool, where HA protects nothing, gets one pool HA cycle (disable, then
  enable), which re-arms both hosts. It runs once per disarm, from the pool
  master when both hosts were disarmed, only with both hosts live in the
  liveset for 2 minutes, the peer's management address answering, a single
  pool master and no HA operation pending; `[ha]
  auto_resolve_peer_emergency = false` turns it into an alert. In a test both
  hosts stayed in that state for over 15 minutes after a whole-switch hold.
- A host whose xapi answers as pool master demotes itself to slave of the
  peer when its own HA liveset names the peer as master (two reads 30 s
  apart), or when XAPI released it from the HA startup loop because the
  pool's HA was disabled while it was cut off, and the peer is shown alive.
  It only points pool.conf at a peer management address that answers, and
  demotes at most 3 times per incident. In a test a host stayed a second
  master for 20 minutes after a partial heal; nothing else collapsed it.
- The automatic XHA re-admission recovery (`[ha] readmission_assist`, still
  on by default: a local HA disarm on both hosts, then one pool HA cycle) is
  gated where it acts. Before its local disarm each host checks only itself:
  the fire-time gate (the peer's management address answering for the
  settle period), pool HA on, XAPI's task list readable and showing no
  pending HA task (unreadable, the dance is cancelled rather than leave
  phase 1 less than its normal lead of the HA timeout plus a minute before
  the sync point), not held in XAPI's HA startup loop, and its own HA daemon out of
  the liveset (for the 5-minute suspicion window). Phase 2 runs through the
  gated pool HA cycle off the daemon's loop; if it does not start, one alert
  says so at once, and a host still locally disarmed 300 s after the sync
  point raises an alert about itself. Each alert names the
  host, says it stays locally disarmed, and gives the manual re-arm.
  TwinStor does not retry. A scheduled recovery is cancelled as soon as the
  host is back in the HA liveset.
- An HA re-arm that waits on a pending pool HA operation in XAPI no longer
  waits forever: TwinStor cancels its own operation once it has been pending
  for 6 minutes after its attempt (a failed enable could leave it pending and
  block every later re-arm). An operation TwinStor did not start is reported
  in `twinstor status` and by an alert, and never touched.
- A pool HA operation left pending in XAPI that holds TwinStor's HA repairs
  for more than 6 minutes is reported (or, if TwinStor started it,
  cancelled) even when no re-arm is owed; the alert names the task and how
  long it has been pending. An unreadable task list holds the re-arm rather
  than reading as "nothing pending".
- Hosts are identified by UUID, never by name-label. The peer's enabled
  flag (which gates the pool HA re-arm) and its host-metrics-live (which
  feeds the dual-master and stranded-master checks) were looked up by
  name-label using the peer's hostname. After a host was renamed in Xen
  Orchestra, both lookups matched nothing: the re-arm held forever, so pool
  HA stayed off after a TwinStor recovery disable, and the two master checks
  only worked when this host's own XAPI was hung. The peer's hostname is now
  resolved to its UUID once (again if that host was re-installed or
  re-joined), and every peer query goes by UUID.
- Pool HA is re-armed when the pool master has moved. The re-arm is owed on
  the host that disabled pool HA, and only the pool master attempted it:
  when the master later moved to the peer (an XHA election, or the operator
  designating it), pool HA stayed off for good. A holder that is no longer
  master now enables from the slave side (XAPI forwards the call to the
  master), with the same preconditions plus this host enabled, and only once
  its own last attempt has settled, so a master holding its own re-arm goes
  first and two enables do not race. From the slave side it runs the local
  self-heals (statefile refresh, PBD plug), not the orphan-xhad stop or the
  toolstack restart, which act on the master.
- The re-arm's toolstack escalation now actually runs. In 0.7.4 it called
  `xe-toolstack-restart` by its bare name, which is not on the daemon's
  PATH, so it never ran; it now uses the full path. It restarts the pool
  master's toolstack to clear a stuck "HA ... in progress" lock when a
  re-arm `pool-ha-enable` owed by TwinStor keeps failing: only on the
  master, only after the lighter remedies failed twice, only while the
  peer's management address answers, at least 10 minutes apart, and at
  most twice per incident (the count is kept on disk across daemon
  restarts, counted before the restart runs, and cleared when the re-arm
  is fulfilled).
- The protected-VM watchdog starts VMs only on the host that holds the HA
  master lock in its own liveset.
- The isolated-master stand-down (arbitrated) takes "I am the master" from
  the local xapi, not from pool.conf alone.

### Storage
- Fate-shared pools: DRBD now waits 3 s instead of 0.5 s for a ping reply
  (or, with both hosts Primary, a missing acknowledgement) before it drops
  the replication connection. At 0.5 s a link losing 20% of its packets was
  dropped and reconnected many times a minute, because TCP needs
  retransmits to get a lost packet through, and every drop started a
  fence-peer round. A dead peer is now noticed up to 2.5 s later. Arbitrated
  pools keep 0.5 s: there the same timer also decides how fast replication
  moves from a failed direct cable to the switched path. Existing pools get
  the new value when `twinstor upgrade-apply` migrates their resource file
  (see Upgrading).
- Arbitrated mode: when XHA fenced one host and the survivor had frozen its
  own I/O on the disconnect, the survivor's freeze could never lift once the
  fenced host came back behind: DRBD waits for the frozen writes before it
  starts the resync, and TwinStor waited for the resync. The HA statefile
  watchdog then reset the survivor too, and both hosts rebooted. The freeze
  now lifts once DRBD has made the survivor the resync source (no
  split-brain, the peer behind) and the connection has held 5 s; in tests
  the survivor resumed I/O about 20 s after the peer reconnected. The whole
  freeze must still end within the pool's HA timeout (see Known
  limitations).

### Alerts and status
- Alerts no longer advise `xe host-emergency-ha-disable --force` as a
  harmless first step. Run on one host that runs VMs while pool HA stays
  armed and the master's HA is live, it gets that host declared dead after
  about the HA timeout, and XAPI restarts its VMs on the other host while
  they still run (seen in testing about two minutes after the command). The
  alerts now give the pool-level command first (`xe pool-ha-disable` on the
  master), use the emergency disable only after that, or on both hosts
  within 30 seconds when the pool-level command fails, state the configured
  HA timeout, and no longer claim that VMs are unaffected. The emergency
  master transition's manual procedure applies only if no host answers as
  pool master. A host locally disarmed under the armed pool that runs VMs,
  not by TwinStor's own recorded disarm, now raises an alert saying so.
  The pool master raises one too, once per wedge (kept across a restart),
  on first seeing its peer answer that way, saying whether TwinStor's reset
  is expected, on hold until a given time, or turned off by configuration;
  not when TwinStor's own hold or reset explains the peer's state, nor
  within 30 minutes of a reset that freed it.
- A host held out of the pool after a fence (its HA daemon still starting)
  no longer alerts that the isolated pool master "is still in the HA
  liveset": it has no liveset of its own to tell. It now alerts that the
  master does not answer on the management network and that, if it still
  runs VMs, clients likely cannot reach them. A host whose liveset still
  shows the master does not claim so until XHA has had longer than it takes
  to decide (three heartbeat timeouts plus a minute) since both the
  isolation was first seen and the last change this host saw in what XHA
  decides on (the master's management address, the peer on replication, the
  DRBD connection, the peer's liveset entry, or XHA's own log of a
  membership change): the master may be about to be removed. It waits at
  most 15 minutes from the isolation; past that, with changes still seen,
  the alert goes out without the liveset claim. The alert
  also needs the master's management address silent, not merely this host's
  `xe` failing (a slave disarmed under the armed pool fails `xe` with
  nothing cut).
- When the peer rejoined with pool HA off and the master had not been master
  long enough to see HA while the peer was live, TwinStor logged that HA "was
  already off while the peer was alive" and raised no alert. It now raises
  the "pool HA is off and TwinStor did not turn it off" alert, saying it does
  not know whether HA was on before; HA is still not re-enabled.
- "Pool HA is off and TwinStor did not turn it off" depended on the pool
  master sampling the peer as not live (every 10 minutes): a peer fenced
  and back between two samples, with pool HA turned off by hand meanwhile,
  got no such alert. The master now also remembers a peer absence its
  partition view showed (peer gone, parked or cut off from its clients)
  and takes it as the peer's last state at the next sample. Still alert
  only; nothing re-enables pool HA. One pool-HA-off episode is reported
  once; and "HA was seen off while the peer was live" is said only when
  that sample was taken before the peer's absence began.
- "The twinstor daemon on <peer> has not reported liveness for ~23
  minutes" was raised a few minutes after a long management-network cut
  ended, while the peer's daemon had run throughout: its report travels
  over the management network and could not arrive. The time the peer's
  management address is silent is no longer counted, the staleness counts
  from its return, and the alert says that no report arrived rather than
  that the daemon is down.
- The no-tiebreaker alert said the gateway had been unreachable "for ~30
  minutes" after a 3-minute outage that followed an earlier one. The checks
  now also use the partition view's 10-second probe of the same targets: a
  gateway that answered most of those probes since the previous check starts
  the count of failed checks afresh, and the alert states the time since the
  last answer, or how many probes answered over the failing period.
- Alerts that advise `pool-ha-disable` or `host-emergency-ha-disable` say
  that the advice applies only when both hosts' management networks are up.
- Alerts whose remedy is `pool-ha-disable` or `host-emergency-ha-disable`
  are held back, not dropped, while a host is held out of the pool, and are
  delivered once none is.
- An alert held because XAPI did not answer is no longer delivered once
  untrue: "serving without redundancy" is withdrawn when redundancy
  returns, an isolated-master alert (either side's) when the isolation
  clears, a reset-hold alert when its hold ends, the master's alert about a
  disarmed peer when that wedge ends, and a disarmed host's own alert once
  it reads armed again; any other held alert is dropped once 15 minutes
  old unless raised again (checked every second, and again right before
  delivery). A one-off report (such as a cleared wedge) and a day-paced
  alert (such as a second pool master) count as sent only once delivered:
  they are not queued, and are raised again until delivered.
- `twinstor status` shows the partition view (for example ClientIsolated,
  Parked, PeerParked, Rejoining) with what to do, the peer's management
  reachability, the last withheld HA action, why an owed HA re-arm waits,
  and a warning when this host's local.db `ha_peers` lacks the peer.
  `twinstor doctor` reports `ha_peers` with its remedy.
- `twinstor status` no longer reads healthy on a second pool master or on a
  host left disarmed under the armed pool (views SecondMaster and
  DisarmedMember); in a test both hosts of a dual master read healthy.
- The partition view names the side that is cut off from its own client
  probe: when XHA keeps the cut-off host by host order, that host says its
  VMs are likely unreachable and its own network needs fixing, and the
  fenced host says its own network is fine and the peer is cut off. A host
  reads Parked only after a failed HA join, a host that stood its own storage
  down (and its peer) says so instead of naming an XHA restart, both hosts
  of a whole-switch hold show the hold, no view is shown while the
  management probes warm up after a daemon start, and a view must hold for
  about 30 s before it replaces another.
- While the peer's management address answers again but the probes have not
  confirmed it yet (after a cut is restored, or a peer reboots), and when a
  peer shown unreachable everywhere answers on replication again, the view
  reads Settling at once instead of naming a side cut off. A host armed for
  HA and out of the liveset reads Joining before its first failed HA join,
  and a confirmed park holds until the host joins, the peer's management
  address answers, or the host is released, instead of flapping with XAPI's
  join cycle. The Rejoining views no longer say the network was away.
- A host disarmed under the armed pool with its HA daemon stopped reads
  DisarmedMember (its own XAPI refuses logins with the armed-mismatch code;
  not asked on a pool whose HA was last read off), and the master reads
  PeerDisarmed, not healthy, while it sees the peer in that state.
- Advice to disable HA stays held while a host is held out of the pool even
  when the view reads Settling (a management link that answers now and
  then): the hold follows the park itself, not the text shown.
- A whole-switch hold keeps its status line for as long as it lasts.

### Configuration
- `[ha] rejoin_assist` (default on) governs the rejoin repairs (the
  stranded-master yield and reset, the returning-slave reconnect, the
  HA-deadlock exit, the masterless active rejoin, the dual-master yield, the
  self-unwedge, the HA-daemon revive and the second-master demote); setting
  `rejoin_assist = false` switches them off and keeps their detections and
  alerts.
- `[ha] readmission_assist` (default on) now governs only the re-admission
  recovery, whose two phases are also gated when they fire.
- `[ha] auto_resolve_peer_emergency` now also governs the re-arm of a host
  TwinStor itself disarmed (the recorded-disarm pool HA cycle): with it off,
  that host gets an alert asking for the cycle instead.

### Known limitations
- XHA still keeps the lowest-index host in a one-against-one tie, which can
  be the host that lost its network; that host's VMs then run where clients
  cannot reach them until the network returns.
- Arbitrated mode: the surviving host's frozen I/O is lifted only once the
  fenced host is back and replication has picked a direction, about 20 s
  after the fenced host reconnects. The whole freeze must end within the
  pool's HA timeout (90 s by default on arbitrated pools), or XHA still
  resets the survivor and both hosts reboot. The fix therefore only helps
  where the fenced host boots and reconnects in under about a minute. On
  the test hosts, back on the network about 50 to 55 s after the fence, the
  freeze lasted from 71 s to just over 90 s and the survivor was still
  reset in 4 runs of 15. A host that takes longer to boot, as most servers
  do, sees both hosts reboot as in 0.7.4. If the
  fenced host takes longer to come back than the HA statefile watchdog
  allows (about 105 s), the survivor can still be reset by that watchdog,
  and both hosts reboot.
- Arbitrated mode: the same freeze is not lifted when replication makes the
  frozen survivor the copy that receives the resync instead of the one that
  sends it; that case can still end the same way.
- Arbitrated mode: an isolated master that stands its storage down keeps
  answering as pool master until its network returns, so two masters answer
  during the hold; the second-master demote ends it after the release.
- Arbitrated mode: lifting the survivor's frozen I/O relies on DRBD
  refusing to make a Primary the resync target (its default `rr-conflict`
  policy); the generated resource file does not set that option itself.
- After a TwinStor re-enable whose call got no answer, pool HA reads on
  before it is certain; if an operator disables pool HA within the
  following settle period (5 minutes, longer while an HA task is pending
  or XAPI's task list cannot be read), TwinStor still takes the HA-off as
  its own and re-enables it.
- An operator's pool HA disable is read as TwinStor's, and TwinStor
  re-enables pool HA, when it lands between TwinStor's disable and its
  enable, during the 7 minutes TwinStor's own disable is pending, or up to 4
  minutes after TwinStor's own disable was last seen pending in XAPI.
- To keep pool HA off after TwinStor turned it off, delete
  /var/lib/twinstor/ha-rearm-obligation on the host that raised the alert:
  TwinStor then issues no further enable for it.
- The other limitations of 0.7.4 are unchanged. On fate-shared pools, the
  3 s ping timeout makes the heavy packet loss case rarer, not impossible;
  arbitrated pools are unchanged there.

### Upgrading
- Upgrade the two hosts back to back with `twinstor upgrade-apply`. No
  configuration change is needed: `[ha] rejoin_assist` is new and defaults
  on, `[ha] readmission_assist` keeps its default (on), and the
  configuration file is not rewritten.
- The DRBD resource file moves from configuration version 6 to 7. On each
  host, `twinstor upgrade-apply` regenerates it from the current template,
  keeps the old file beside it
  (`/etc/drbd.d/twinstor-<resource>.res.pre-regen`), and applies the new
  one live with `drbdadm adjust` before it restarts the daemon; replication
  stays connected. If the live apply is refused, the old file is put back
  and the upgrade goes on; `twinstor status` then shows a `config:` line,
  safe to leave until a maintenance window. The lines that can differ:
  - the configuration version line;
  - fate-shared pools: `ping-timeout 30;` (3 s) instead of
    `ping-timeout 5;` (arbitrated pools keep 5);
  - `c-fill-target` and `c-min-rate`, recomputed from the link speeds
    measured during the migration, as setup computes them: unchanged unless
    a speed reads differently from when the file was written. An arbitrated
    pool whose file was written before 0.7.4 and not regenerated since gets
    the sizing 0.7.4 introduced: the fill target from the direct cable, and
    the floor the lower of 80% of the cable and half the switched link
    (before, both came from the switched link);
  - in a file written before 0.7.4, comment lines (wording only).
- A 0.7.4 peer keeps its own behaviour until it is upgraded.

## [0.7.4] - 2026-10-29

A small fix release for 0.7.3. Upgrade both hosts.

### Fixed
- Arbitrated mode: setup sizes the resync from the direct cable, which
  carries it, instead of the switched network.
- `twinstor status` while a copy is being built shows the single iSCSI path
  as expected instead of degraded, and shows "no TwinStor SR yet" between
  the two hosts' setup steps instead of "XAPI did not answer". The
  install.sh warning about pending XCP-ng updates now says to apply them
  before running setup.
- The fence-peer handler no longer relies on a behaviour of `wait` that
  some bash 5.3 releases changed (XCP-ng's bash is not affected).

### Upgrading
- Upgrade the two hosts back to back with `twinstor upgrade-apply`. No
  configuration change. Existing arbitrated pools keep their resync
  settings until `twinstor regenerate-config`.

### Known limitations
- **Heavy packet loss between the hosts:** when the replication network
  keeps dropping a large share of its packets (20% or more, for minutes)
  without cutting the hosts apart, both hosts can still accept writes for a
  moment, and the last few writes of the host that loses are discarded; a
  VM that ran on it may need a filesystem check. An alert names the VMs
  concerned. In arbitrated mode this needs both the direct cable and the
  switched network to drop packets at once. (0.7.3 put the threshold at
  30%; further tests lost a few writes at 20%.)
- On a fate-shared pool, such a link can also leave both hosts stepped
  down, with nothing serving the SR until an operator picks the copy to
  keep; no acknowledged write is lost in that case. A link that keeps
  dropping packets is a hardware fault to fix (cable, optic, switch port).
- The other limitations of 0.7.3 are unchanged.

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
- `twinstor status` and `twinstor doctor` end quietly when the reader of
  their output exits early (`twinstor status | grep -q healthy`) instead of
  panicking on the broken pipe.

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
