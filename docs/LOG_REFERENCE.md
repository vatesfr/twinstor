# Reading TwinStor logs

How to tell, from the logs alone, which code path ran during an incident and
why. Written for support engineers working from a diagnostics bundle
(`twinstor support-bundle`) or the files on the hosts.

## Where things are

| What | Where | Notes |
|---|---|---|
| Daemon log | `/var/log/twinstor.log` | Daily rotation, 14 kept; the newest rotation (`.1`) stays uncompressed. |
| Fence-peer handler log | `/var/log/twinstor-fence-peer.log` | Written by DRBD's fence-peer and split-brain handlers, outside the daemon. Weekly rotation, 4 kept. Also sent to syslog (`twinstor` tag). |
| Kernel log | `kern.log`, `dmesg` | DRBD state changes and handshakes ("I shall become SyncTarget, but I am primary!", generation UUIDs), handler exit codes, and undeliverable alerts (`twinstor: UNDELIVERED ALERT`). |
| State files | `/var/lib/twinstor/` (durable), `/run/twinstor/` (until reboot) | Markers, latches and records the decision paths read. The bundle copies both directories (`state/var-lib/`, `state/run/`). |
| XAPI messages | `xe message-list` | Every alert TwinStor sends is also in the daemon log (see below). |

## What every daemon line carries

- **Source location**: `twinstor::state_machine: src/state_machine.rs:17689:`.
  Several paths share a message or an event name; the file and line say which
  one ran. Line numbers belong to a release: read them against the version in
  that daemon life's `startup_state` line.
- **`heal_event="..."`**: a stable name for the decision or step (the
  appendix lists them all), often with `step="..."` for the stage inside it.
- **Episode id**: `ep{id=splitbrain-1792571204}:` prefixes every line logged
  while a heal episode is open, the watchdog steps included. The id is the
  class and the Unix time it opened; each node mints its own.
- **Incident id**: `incident=freeze-1792571204` (or `arbyield-...`) on the
  fate-shared freeze and arbitrated step-down lines. These paths open no heal
  episode (episodes drive other decisions); the incident id only correlates
  lines. `incident_begin` / `incident_end` bracket it.

Debug lines are not written by default. To get them, add `RUST_LOG=twinstor=debug`
to the unit's environment (`systemctl edit twinstor`, then restart the daemon)
and remove it afterwards; `RUST_LOG` replaces the default filter as given.

## Lines to look at first

| Event | What it tells |
|---|---|
| `startup_state` | One line per daemon start: version, mode, and every durable state file a decision path reads (`-` = absent). Says which path a restart resumes. |
| `previous_forced_reboot` | The previous daemon life forced a reboot: time, version, episode and reason (from `/var/lib/twinstor/last-forced-reboot`, written and synced before the reboot). Paired with a `forced_reboot` line before it, when the log survived. |
| `fence_peer_verdict` | The fence-peer handler's verdict (id and exit code) with the I/O state it left, logged by the daemon. The handler's own lines carry the same run pid. |
| `resolver_decision` | Every split-brain verdict: entry (`init`/`online`), `verdict_source` (which signal decided), action (keep/discard/undecided). |
| `lineage_evidence` | The DRBD generation pair the resolver read, and its class (present, incomplete, absent, collect_failed). |
| `loser_discard` | This node discards its copy. `last_resolver_decision` names the decision that made it the loser (with its age), `evidence` the crash-boundary input. |
| `loser_crash_boundary` | The guests stopped before a discard, step by step (`start`, `stop`, `poweroff`, `datapaths`, `tapdisks`, `stopped`, `hold`, `restart`, ...). Guests are named with name and uuid. |
| `promote` | Every promote, with the state it was taken in (`alone=true` is a lone promote). |
| `heal_stage` | Every change of the stage `twinstor status` shows. |
| `XAPI alert sent` / `XAPI alert undeliverable` | Every alert, with its body (`alert=` name, `body=` text). Held-back alerts delivered later, dropped or withdrawn are logged too. |
| `drbd state change done` | Every DRBD command that changed state (connect, discard offer, promote, demote, suspend or resume I/O, new generation, peer object), whatever path issued it. Failed commands log `command failed` with the exit code. |

## The fence-peer handler

DRBD runs `/usr/lib/drbd/twinstor-fence-peer.sh` when a node loses its peer.
Each line is `<ISO time with offset> [<run pid>] <message>`; DRBD can start a
second run while one is still deciding, so lines interleave, and the pid
separates them. Each run logs `started:` (targets, mode), `context:` (role,
peer address, yield marker, freeze flag), what it probed, and ends with one
`verdict=<id> exit=<code>` line, also appended to `/run/twinstor/fence-verdict`,
whose every new line the daemon logs as `fence_peer_verdict`.

| Verdict id | Exit | Meaning |
|---|---|---|
| `survive` | 4 | A tiebreaker (gateway or ping-target) answered: this node serves, the peer is outdated. |
| `promote_outdate` | 4 | Same at promote time (this node was Secondary): the unreachable peer is marked outdated so this node may promote. |
| `flap_defused` | 4 | The link came back during the handler: writes resume replicated, nothing diverged. |
| `keeper` | 4 | Peer listening but not reconnected: the lower hostname keeps serving (not in freeze mode). |
| `gray_peer_survive` | 4 | Yield candidate, but the peer answers on no address (`probed:` lists them): survive. |
| `tiebreak_reconnected` | 4 | Reconnected while yielding: nothing written here, no divergence. |
| `tiebreak_yield` | 1 | Yields to the keeper (or, in freeze mode, to a live peer daemon): frozen, served nobody, yield recorded. |
| `briefly_cut_yield` | 1 | Freeze mode: this node's own network just came back: yields frozen. |
| `yield_to_peer` | 1 | Arbitrated: no tiebreaker, peer alive on the direct link: yields frozen. |
| `isolated_yield` | 1 | No tiebreaker answers: isolated, frozen (fate-shared: the peer shares that network and is not probed). |
| `keep_yield`, `keep_yield_io_unreadable`, `keep_yield_role_unreadable` | 1 | A yield of this episode is still in force (or its state cannot be read): kept. |
| `promote_refuse_entry`, `promote_refuse_decision` | 1 | Promote-time run while the peer's DRBD listens: refuses to outdate it. |
| `bounce_standdown` | 1 | The peer announced an administrative replication bounce: no verdict, no outdate. |
| `lock_timeout` | 1 | Another run held the lock past its bound: frozen, outdates nobody, no yield recorded. |

A `WARNING: drbdadm suspend-io failed` line means the yield did not freeze the
I/O; `firewall rule: INSERT-FAILED` means the fate-shared peer-portal fence was
not installed.

## Following one incident

1. Find the window: `grep -n 'heal_event="episode_open"\|heal_event="incident_begin"' twinstor.log`
   on both nodes (the bundle's `logs/episodes.txt` indexes them, and extracts
   the newest ones in full).
2. The handler's side: `grep fence_peer_verdict twinstor.log`, then the same
   run pid in `twinstor-fence-peer.log`.
3. The decision: `resolver_decision`, `freeze_*`, `arb_yield_*`, `beacon_claim`
   (what each node told the other: Y yielded, W won, G frozen with guests,
   R giving way).
4. The action: `loser_discard`, `loser_crash_boundary` steps,
   `drbd state change done` lines, `promote`.
5. The aftermath: `loser_crash_boundary step="restart"` (each owed guest
   restart), `protected_vm_*` (the halted-VM backstop), `heal_stage`,
   `ha_rearm_*` (pool HA re-arm and, in `ha_rearm_hold`, why it waits).

## Appendix: every heal_event

Generated from the sources of this release by `scripts/gen-log-events.py`
(re-run it for another release: line numbers move). One row per event and
`step` value; the message is that of the first site.

| heal_event (step) | level | where (file:line) | message (first occurrence) |
|---|---|---|---|
| `arb_yield_behind_hold` | WARN | state_machine.rs:19663 | this node yielded and its copy is behind the peer's, but the peer's iSCSI portal does not answer; holding f... |
| `arb_yield_not_decided` | INFO/WARN | state_machine.rs:19573, state_machine.rs:19592, state_machine.rs:19605, state_machine.rs:19637 | arbitrated yield: the kernel log no longer shows this resource's disconnect (or could not be read), so a re... |
| `arb_yield_step_down` | WARN | state_machine.rs:19697 | this node yielded (I/O frozen since the disconnect) and DRBD reads its copy as behind the peer's, with no f... |
| `arb_yield_xapi_wait_over` | WARN | state_machine.rs:19646 | arbitrated yield: this host's XAPI still does not answer; stepping down without it (the crash boundary stop... |
| `beacon_claim` | INFO | state_machine.rs:7120, state_machine.rs:7133 | split-brain claim served to the peer changed (Y yielded, W won, G frozen with guests, R giving way) |
| `boot_gate_peer_alive` | INFO/WARN | state_machine.rs:12735, state_machine.rs:12751, state_machine.rs:12849 | boot gate: the peer was seen serving; this node will not serve alone until it has reconnected (or an operat... |
| `both_outdated_tiebreak` | WARN | state_machine.rs:12124, state_machine.rs:12166, state_machine.rs:12180, state_machine.rs:12186, state_machine.rs:12193 | both replicas Outdated but node name == peer name: cannot elect a keeper; NOT running the tiebreak. Fix the... |
| `capacity` | WARN | state_machine.rs:9267 | SR physical utilisation at {p}% ({sev}); guests hit ENOSPC when it fills |
| `consistent_diskless_force` | INFO/WARN | state_machine.rs:11754, state_machine.rs:11779, state_machine.rs:11797, state_machine.rs:11821 | REFUSING the sole-survivor force-promote: configured node.name does not match this host's kernel hostname (... |
| `consistent_diskless_manual` | WARN | state_machine.rs:11875 | sole-survivor deadlock needs an operator decision (no safe automatic action); raising the guided-manual alert |
| `converged` | INFO | state_machine.rs:19964, state_machine.rs:20525, state_machine.rs:22423, state_machine.rs:23655 | split-brain: peer reconnected and is resyncing from us |
| `demoted_survivor_reassert` | WARN | state_machine.rs:9944 | re-asserting pool mastership: this host holds the only UpToDate copy but was demoted below a stale (Inconsi... |
| `discard_offer_withdraw` | WARN | state_machine.rs:10499 | this node may still carry a --discard-my-data offer from an earlier round but now keeps its copy; withdrawi... |
| `discard_offer_withdraw_failed` | ERROR/WARN | state_machine.rs:10473, state_machine.rs:10552 | could not withdraw a possible stale discard offer (its state is unreadable); connecting anyway |
| `disks_suspect_mark` | INFO | state_machine.rs:16618 | a VM marked disks-suspect was started again since the mark |
| `episode_close` | INFO | state_machine.rs:20855, state_machine.rs:20868 | heal episode closed (daemon died with it open; closing on restart so every open has a close) |
| `episode_open` | INFO | state_machine.rs:20830 |  |
| `failback_complete` | INFO | state_machine.rs:10414 | failback: replication re-established on the direct link |
| `failback_dance` | WARN | state_machine.rs:10329 | failback: moving replication to the direct link (brief local target stop; consumers ride multipath via the ... |
| `failback_dance` step=`restore_paths_failed` | WARN | state_machine.rs:10388 | failback: standby path restore failed; the steering restore retries it |
| `failback_latched` | WARN | state_machine.rs:10086 | direct link flapped back {FAILBACK_MAX_STRIKES} times; automatic failback is now LATCHED OFF until an opera... |
| `failback_missed` | WARN | state_machine.rs:10419 | failback: dance completed but the connect landed on the fallback path again; backing off |
| `failback_strike` | WARN | state_machine.rs:10079 | direct link flapped back to the fallback path shortly after a failback; escalating the stability hold |
| `fateshared_lone_survivor_hold` | WARN | state_machine.rs:12377 | fate-shared lone-survivor promote HELD to avoid a VM double-run: the peer is unreachable and, with no direc... |
| `fateshared_lone_survivor_isolated` | WARN | state_machine.rs:12330 | fate-shared lone-survivor hold expired but this node reaches neither its gateway nor any ping-target: it is... |
| `fateshared_no_tiebreaker` | WARN | state_machine.rs:12285 | fate-shared lone-survivor hold expired, but no gateway or ping-target is configured: this node cannot tell ... |
| `fence_peer_verdict` | INFO | state_machine.rs:6910 | fence-peer handler verdict (details in /var/log/twinstor-fence-peer.log, same run pid) |
| `forced_reboot` | ERROR | state_machine.rs:20320 | forcing a reboot now |
| `fork_claim_win` | WARN | state_machine.rs:13200, state_machine.rs:18412 | init: the peer yielded during the partition and never claims this fork (or this node already took it); keep... |
| `freeze_budget_keep` | WARN | state_machine.rs:18941 | freeze mode: the freeze budget ran out after the peer gave way; resuming this copy's I/O before the I/O sta... |
| `freeze_budget_keep` step=`resume_failed` | WARN | state_machine.rs:18952 | freeze mode: this node's guests reached the freeze budget without a decision; stopping them (nothing they w... |
| `freeze_budget_step_down` | WARN | state_machine.rs:18962 | freeze mode: this node's guests reached the freeze budget without a decision; stopping them (nothing they w... |
| `freeze_give_way` | WARN | state_machine.rs:19441 | freeze mode: the peer froze with guests and none ran here during the split; giving way (this copy's dom0-on... |
| `freeze_give_way_dropped` | WARN | state_machine.rs:19491 | freeze mode: the peer no longer holds a frozen copy; dropping the give-way and returning to bring-up |
| `freeze_give_way_expired` | WARN | state_machine.rs:19144 | freeze mode: the give-way never landed; dropping it and returning to bring-up |
| `freeze_give_way_holding` | INFO | state_machine.rs:21976 | freeze mode: giving way; Secondary with the target down, waiting for the frozen peer's new generation befor... |
| `freeze_give_way_landed` | INFO | state_machine.rs:19127 | freeze mode: the discard landed; resyncing from the peer and serving again |
| `freeze_give_way_reoffer` | INFO | state_machine.rs:19117, state_machine.rs:19472 | freeze mode: giving way; the peer took it, offering the discard |
| `freeze_give_way_taken` | INFO | state_machine.rs:19753 | freeze mode: the peer took this node's give-way (it may serve on the only full copy) |
| `freeze_give_way_taken_restored` | INFO | state_machine.rs:19104 | freeze mode: the peer had taken this node's give-way (recorded before a restart) |
| `freeze_keep` | WARN | state_machine.rs:19205 | freeze mode: the peer gave way to this node's frozen guests; new data generation here, resuming the guests ... |
| `freeze_keep` step=`new_generation_failed` | WARN | state_machine.rs:19214 | freeze mode: could not start a new data generation; retrying |
| `freeze_keep_resume` | INFO | state_machine.rs:19232 | freeze mode: resumed this copy's I/O; the peer discards its copy on the handshake |
| `freeze_keep_resume` step=`resume_failed` | WARN | state_machine.rs:19240 | freeze mode: the peer keeps its copy (guests ran there); stopping this node's frozen guests (nothing they w... |
| `freeze_step_down` | WARN | state_machine.rs:19178, state_machine.rs:19248, state_machine.rs:19312, state_machine.rs:19342 | freeze mode: this node yielded with no guest on the SR; stepping down (resync from the peer), no reboot |
| `freeze_survivor_keeps` | WARN | state_machine.rs:19432 | freeze mode: the peer froze with guests, but guests ran here during the split; keeping this copy, the peer ... |
| `freeze_wait` | INFO | state_machine.rs:19303, state_machine.rs:19383, state_machine.rs:19485 | freeze mode: this copy is behind the frozen peer's, but this host's XAPI does not answer yet; holding until... |
| `freeze_win_rebound` | INFO | state_machine.rs:19079 | freeze mode: the giving peer's generation moved; re-binding this node's win to it |
| `freshness_lineage` | INFO/WARN | state_machine.rs:13284, state_machine.rs:13311, state_machine.rs:18510 | init: split-brain resolved by DRBD generation lineage — this node is behind the peer, resolving as the lose... |
| `freshness_override` | WARN | state_machine.rs:13366, state_machine.rs:18574 | init: freshness gate overrode the data-blind verdict to protect data currency |
| `fs_freeze_connect_int` | INFO | state_machine.rs:11223 | fate-shared freeze mode: could not set the reconnect interval |
| `fs_freeze_fencing` | INFO | state_machine.rs:11161, state_machine.rs:13635 | fate-shared freeze mode: DRBD fencing policy set |
| `fs_freeze_peer` | INFO | state_machine.rs:11370 | engaged on the pair |
| `guest_arrived_while_frozen` | WARN | state_machine.rs:10830 | a guest domain using the SR appeared on this host while its copy is frozen by this node's fence-peer yield ... |
| `ha_armed_mismatch` | WARN | state_machine.rs:17129 | this host is LOCALLY HA-disarmed while pool HA is enabled; its next reboot springs an xapi trap |
| `ha_deadlock_exit` | WARN | state_machine.rs:14114 | isolated-master dual-master HA deadlock (data-safe): yielding as the returned/forked node — local toolstack... |
| `ha_readmission_dance_scheduled` | WARN | state_machine.rs:14391 | XHA join deadlock detected (both sides' stale liveset claims re-poisoning each other); scheduling the quies... |
| `ha_readmission_flap` | WARN | state_machine.rs:17181 | returning node rejoined the XHA liveset but has stayed locally HA-disarmed past the grace -- XAPI is stuck ... |
| `ha_readmission_quiesce` | WARN | state_machine.rs:14424 | re-admission dance phase 1: local emergency HA disable (quiescence) |
| `ha_readmission_recovered` | INFO | state_machine.rs:14216 | the peer rejoined the liveset after the synchronized HA restart — join-deadlock recovery confirmed |
| `ha_readmission_revive` | WARN | state_machine.rs:14375 | XHA daemon is dead with nothing retrying it (xha.log silent all window) while storage is healthy — the boot... |
| `ha_readmission_revive_failed` | WARN | state_machine.rs:14569 | XHA daemon restart did not reach Online; leaving the manual runbook alert armed (next automatic attempt in ... |
| `ha_readmission_revived` | INFO | state_machine.rs:14553 | XHA daemon restarted and rejoined the liveset; HA protection restored |
| `ha_readmission_storm` | WARN | state_machine.rs:14322 | XHA cannot rejoin and the storm is still active (statefile errors / storage not healthy); fix STORAGE first... |
| `ha_readmission_stuck` | WARN | state_machine.rs:14343 | XHA re-admission stuck; automated recovery unavailable (disabled, capped, or preconditions failed) — manual... |
| `ha_rearm_after_fence` | WARN | state_machine.rs:18253 | peer rejoined after a fence and pool HA is off; writing the re-arm obligation to restore HA |
| `ha_rearm_fulfilled` | INFO | state_machine.rs:17863 | pool HA is armed and settled; clearing the re-arm obligation |
| `ha_rearm_held_maintenance` | INFO | state_machine.rs:17816 | a re-arm obligation is pending but a host is in operator maintenance (MAINTENANCE_MODE) -- or the toolstack... |
| `ha_rearm_hold` | INFO | state_machine.rs:17737, state_machine.rs:17754, state_machine.rs:17853 | a pool-HA re-arm is owed, but the routine that disabled HA is still running; it re-arms HA itself |
| `ha_rearm_pbd_plug` | INFO | state_machine.rs:17705 | plugging the twinstor SR's PBD before pool-ha-enable (it was left unplugged by a reconnect/readmission; ena... |
| `ha_rearm_retry` | WARN | state_machine.rs:17878 | pool HA is still off from a daemon-initiated disable; retrying the re-arm with self-heal (persisted obligat... |
| `ha_rearm_skipped_operator_disable` | INFO | state_machine.rs:18227 | peer rejoined with pool HA off, but HA was already off while the peer was alive -- treating this as a delib... |
| `ha_rearm_toolstack_reset` | WARN | state_machine.rs:17942 | pool HA re-arm keeps failing after the light self-heal; restarting the local toolstack to clear a stuck 'HA... |
| `ha_reset_timeout` | WARN | state_machine.rs:23661 | survivor_ha_reset: peer did not demote within the window; HA re-armed, manual review |
| `heal_stage` | INFO | state_machine.rs:20757, state_machine.rs:20784 |  |
| `host_auto_enable` | WARN | state_machine.rs:16543 | this host is disabled (maintenance mode) with storage fully healthy and no operator marker: the boot-time e... |
| `host_auto_enable_failed` | WARN | state_machine.rs:16565 | host-enable did not stick; will retry after the cooldown (manual: xe host-enable) |
| `host_auto_enable_held_isolated` | INFO | state_machine.rs:16452 | not auto-enabling this disabled host: replication is healthy but the client network is unreachable -- it is... |
| `host_auto_enable_hold` | INFO | state_machine.rs:16505, state_machine.rs:16520 | this host is disabled; not enabling it yet |
| `host_auto_enabled` | INFO | state_machine.rs:16551 | host enabled; HA placement restored |
| `hostname_drift` | WARN | state_machine.rs:9092 | kernel hostname '{live}' no longer matches the configured node name '{configured}'; DRBD on-sections will n... |
| `incident_begin` | INFO | state_machine.rs:19743 | incident id for the log lines of this freeze or yield (no heal episode) |
| `incident_end` | INFO | state_machine.rs:19739, state_machine.rs:19914 | incident id closed (unused for 30 min) |
| `init_loser_inconsistent` | WARN | state_machine.rs:13132 | init: split-brain with a local Inconsistent replica (interrupted resync); resolving as the loser automatica... |
| `internal_shutdown_policy` step=`decided` | INFO | state_machine.rs:16968 | pool ha-reboot-vm-on-internal-shutdown: acting on freeze mode's state |
| `internal_shutdown_policy` step=`record_dropped` | INFO | state_machine.rs:17028 | freeze mode not engaged: twinstor's record dropped (the setting was already as found) |
| `internal_shutdown_policy` step=`record_off` | INFO | state_machine.rs:17012 | pool setting already off (not by twinstor this daemon life): recorded |
| `internal_shutdown_policy` step=`restore_failed` | WARN | state_machine.rs:17023 | XAPI refused or did not answer putting the pool setting back; retried |
| `internal_shutdown_policy` step=`restored` | WARN | state_machine.rs:17037 | freeze mode not engaged: pool ha-reboot-vm-on-internal-shutdown put back on (twinstor had turned it off) |
| `internal_shutdown_policy` step=`rollback_failed` | WARN | state_machine.rs:13596 | daemon stopping for a release without freeze mode: XAPI too slow to put the pool's ha-reboot-vm-on-internal... |
| `internal_shutdown_policy` step=`rollback_noop` | INFO | state_machine.rs:13591 | daemon stopping for a release without freeze mode: XAPI too slow to put the pool's ha-reboot-vm-on-internal... |
| `internal_shutdown_policy` step=`rollback_restored` | WARN | state_machine.rs:13584 | daemon stopping for a release without freeze mode: XAPI too slow to put the pool's ha-reboot-vm-on-internal... |
| `internal_shutdown_policy` step=`rollback_timeout` | WARN | state_machine.rs:13603 | daemon stopping for a release without freeze mode: XAPI too slow to put the pool's ha-reboot-vm-on-internal... |
| `internal_shutdown_policy` step=`turn_off_failed` | WARN | state_machine.rs:16978 | XAPI refused or did not answer turning the pool setting off; retried |
| `internal_shutdown_policy` step=`turned_off` | WARN | state_machine.rs:16985 | fate-shared freeze mode: pool ha-reboot-vm-on-internal-shutdown turned off, so a guest a losing pool master... |
| `internal_shutdown_policy` step=`unreadable` | INFO | state_machine.rs:16956 | pool ha-reboot-vm-on-internal-shutdown unreadable (XAPI did not answer, or a XAPI without that setting): no... |
| `isolated_master` | WARN | state_machine.rs:15720 | the pool master is unreachable on the client network but its daemon is alive on the replication link: it ha... |
| `isolated_master_clock_held` | INFO | state_machine.rs:15672 | xe answered again but this node still follows the peer; holding the isolated-master maturity clock across t... |
| `isolated_rejoin` | INFO | state_machine.rs:15012 | client network stably returned and the peer is reachable on replication; restoring storage bottom-up and re... |
| `isolated_slave_standdown` | WARN | state_machine.rs:15401 | this SLAVE is cut off from the client network while the master is reachable with its gateway UP over the di... |
| `isolated_standdown` | WARN | state_machine.rs:14780, state_machine.rs:14791 | isolated SLAVE past the stand-down window with the master reachable and UpToDate: leaving xhad ARMED and ma... |
| `isolated_standdown_hold` | WARN | state_machine.rs:14976, state_machine.rs:15005 | parked as a stood-down isolated master; storage held unavailable so VMs stay on the reachable peer. Reconne... |
| `keep_and_alert` | WARN | state_machine.rs:20141 | split-brain: peer reachable but will not resync; suspected DUAL-MASTER wedge. This node is the survivor or ... |
| `leftover_lift_wait` | INFO | state_machine.rs:8451, state_machine.rs:10715 | I/O suspended (suspended:user) while Connected and both UpToDate: waiting for the connection to hold before... |
| `lineage_evidence` | INFO/WARN | state_machine.rs:23229, state_machine.rs:23249, state_machine.rs:23259 | split-brain resolver: DRBD generation lineage evidence read |
| `loopback_restore_pending` | WARN | state_machine.rs:11507 | failback: loopback login still failing; the periodic path self-heal will keep retrying |
| `loopback_restored` | INFO | state_machine.rs:11504 | failback: local loopback session re-established and local-path priority re-pinned |
| `loser_crash_boundary` | WARN | loser_boundary.rs:1236, loser_boundary.rs:1729, state_machine.rs:21167, state_machine.rs:21520, state_machine.rs:21631, state_machine.rs:21816 (+1) | {who}: crash boundary NOT established; holding both copies intact |
| `loser_crash_boundary` step=`confirm` | WARN | state_machine.rs:21095 | split-brain loser: the last check before the discard found the SR in use again; re-establishing the crash b... |
| `loser_crash_boundary` step=`datapaths` | INFO/WARN | loser_boundary.rs:1983, loser_boundary.rs:2064, loser_boundary.rs:2123, loser_boundary.rs:2131, loser_boundary.rs:2219, loser_boundary.rs:2307 (+1) | cannot read XAPI's storage datapaths on this host; closing leftover tapdisks directly instead |
| `loser_crash_boundary` step=`enumerate_unavailable` | INFO | loser_boundary.rs:1293, loser_boundary.rs:1299 | {who}: XAPI cannot list this host's guests; the local scan decides alone |
| `loser_crash_boundary` step=`hold` | WARN | state_machine.rs:21511 | crash boundary holding: nothing discarded, both copies kept, retrying |
| `loser_crash_boundary` step=`host_disable_skipped` | INFO | loser_boundary.rs:1311, loser_boundary.rs:1316 | {who}: the host disable did not answer in time; relying on the local scan (it may still land: enabled again... |
| `loser_crash_boundary` step=`host_disable_unconfirmed` | INFO | loser_boundary.rs:1329 | {who}: the host disable did not answer in time; relying on the local scan (it may still land: enabled again... |
| `loser_crash_boundary` step=`host_enable` | INFO/WARN | loser_boundary.rs:2702, loser_boundary.rs:2712, loser_boundary.rs:2763 | split-brain loser: the pair has converged (or this copy, which took the winner's history, now serves alone)... |
| `loser_crash_boundary` step=`not_required` | INFO | state_machine.rs:21259 | split-brain loser: no guest can be reading a diverged history through this node; no crash boundary needed |
| `loser_crash_boundary` step=`poweroff` | INFO/WARN | loser_boundary.rs:903, loser_boundary.rs:913, loser_boundary.rs:918, loser_boundary.rs:939, loser_boundary.rs:995, loser_boundary.rs:1001 (+1) | guest domains using the SR powered off at the hypervisor; xenopsd tears them down as a guest shutdown (no r... |
| `loser_crash_boundary` step=`rebuilt_after_discard` | ERROR/WARN | loser_boundary.rs:1618, loser_boundary.rs:1663, loser_boundary.rs:1680, loser_boundary.rs:1688, state_machine.rs:21223 | boundary record unreadable: every guest domain on the SR here counts as suspect |
| `loser_crash_boundary` step=`reconcile_stale` | INFO | loser_boundary.rs:2779 | split-brain loser: stale live record of a guest stopped here |
| `loser_crash_boundary` step=`record` | INFO | loser_boundary.rs:1251 | boundary record from another episode: keeping only its restart obligation |
| `loser_crash_boundary` step=`record_unreadable` | ERROR | state_machine.rs:21724 | the crash boundary's record cannot be read: owed guest restarts are NOT run automatically (start the halted... |
| `loser_crash_boundary` step=`release_before_peer_start` | INFO | loser_boundary.rs:2743 | split-brain loser: could not release this host's hold on the guest's disks before starting it on the winner |
| `loser_crash_boundary` step=`restart` | INFO/WARN | loser_boundary.rs:2875, loser_boundary.rs:2888, state_machine.rs:21595 | split-brain loser: owed guest restart |
| `loser_crash_boundary` step=`restart_escalated` | WARN | state_machine.rs:21758 | an owed guest restart keeps failing: alerted, retried at a slower pace |
| `loser_crash_boundary` step=`restart_here` | INFO | loser_boundary.rs:2831 | split-brain loser: the winner cannot attach this guest's disk while this host holds it; this node has rejoi... |
| `loser_crash_boundary` step=`restart_no_target` | WARN | state_machine.rs:21712 | owed guest restarts paused: XAPI cannot name this host or the peer |
| `loser_crash_boundary` step=`restart_xapi` | WARN | loser_boundary.rs:3656 | split-brain loser: restarting this host's XAPI agent (xapi only) so it drops the stale running record of a ... |
| `loser_crash_boundary` step=`resume_io` | INFO/WARN | loser_boundary.rs:1174, loser_boundary.rs:1185, loser_boundary.rs:1201 | this copy's I/O stays suspended: a domain on this host runs or is being built with an SR disk (or the local... |
| `loser_crash_boundary` step=`start` | INFO | state_machine.rs:21281 | split-brain loser: stopping the guests on this host that use the SR before discarding, so none of them is m... |
| `loser_crash_boundary` step=`started_after_withdrawal` | INFO | loser_boundary.rs:1643 | guest domains using the SR started on this host after this copy was withdrawn; they reach the SR through th... |
| `loser_crash_boundary` step=`stop` | INFO | loser_boundary.rs:1425 | {who}: stopping the guest domains that use the SR, all at once, before any withdrawal |
| `loser_crash_boundary` step=`stopped` | INFO | loser_boundary.rs:1507 | {who}: crash boundary established; the discard may proceed |
| `loser_crash_boundary` step=`suspend_io` | ERROR/INFO/WARN | loser_boundary.rs:1073, loser_boundary.rs:1078, loser_boundary.rs:1215, loser_boundary.rs:1220 | this yielded copy's I/O is running and could not be suspended again: it accepts writes that the discard wil... |
| `loser_crash_boundary` step=`tap_list_settled` | INFO | loser_boundary.rs:2429 | tap-ctl list settled after a tapdisk being torn down |
| `loser_crash_boundary` step=`tap_orphan_minors` | INFO | loser_boundary.rs:2412 | tap-ctl lists minors with no tapdisk process (left by a tapdisk that died); they hold no image |
| `loser_crash_boundary` step=`tapdisks` | INFO/WARN | loser_boundary.rs:1477, loser_boundary.rs:2028, loser_boundary.rs:2073, loser_boundary.rs:2080 | {who}: a guest is being started on this host during the boundary; stopping it too before releasing the disk... |
| `loser_crash_boundary` step=`unlisted_domain` | WARN | loser_boundary.rs:1535 | {who}: a guest domain using the SR runs here although XAPI does not list it on this host; stopping it too |
| `loser_discard` | WARN | state_machine.rs:21056 | split-brain: resolving as the loser, discarding local diverged writes and resyncing from the winner |
| `loser_holding` | WARN | state_machine.rs:22186 | split-brain: discard offered but the peer's handshake has not completed yet; holding down (Secondary, targe... |
| `loser_refused` | WARN | state_machine.rs:22136 | split-brain: the peer refused the discard handshake (it also claims loser); this node is entitled to the wi... |
| `loser_teardown_incomplete` | ERROR | state_machine.rs:21907, state_machine.rs:21932 | split-brain: the iSCSI backstore is still exported; NOT discarding this round (retrying the teardown on the... |
| `master_isolated` | WARN | state_machine.rs:15569 | this MASTER is cut off from the client network while the peer is alive on the replication link: any VMs her... |
| `masterless_defer_active_rejoin` | WARN | state_machine.rs:16132 | slave-of-self persisted past the grace window without re-admission; repointing pool.conf at the peer master... |
| `masterless_defer_peer_returned` | INFO | state_machine.rs:16259 | masterless recovery: the peer no longer reads confirmed-dead in the local liveset at the moment of action; ... |
| `masterless_defer_stuck` | WARN | state_machine.rs:16157 | slave-of-self did not rejoin after repeated repoints; operator intervention needed |
| `masterless_defer_to_master` | INFO/WARN | state_machine.rs:16093, state_machine.rs:16238 | pool.conf says slave-of-self, but the peer is a LIVE master: deferring (no transition) and rejoining as its... |
| `masterless_recover` | WARN | state_machine.rs:15910, state_machine.rs:16268 | starting the emergency master transition to break the mutual-slave deadlock (lower hostname acts) |
| `masterless_recover_failed` | WARN | state_machine.rs:16288 | pool.conf did not flip to master within the window; manual recovery needed (see the masterless alert) |
| `masterless_recover_unconfirmed` | WARN | state_machine.rs:16358 | emergency master transition done but the peer still reads as pool MASTER -- the slave-recovery demotion did... |
| `masterless_recovered` | INFO | state_machine.rs:16374 | emergency master transition complete; this node is master again and the peer is demoted / gone |
| `mastership_flap` | WARN | state_machine.rs:7871 | pool.conf role flapping while DRBD is Connected: suspected XHA re-promotion fight (control plane unstable, ... |
| `multipath_queue_restore` | WARN | multipath.rs:238 | multipath map queues fewer retries than configured; reconfiguring with both paths up |
| `mutual_slave_suspect` | WARN | state_machine.rs:15848 | pool.conf follows the peer but xe is not answering; watching |
| `operator_discard_done` | INFO | state_machine.rs:11978 | operator discard: resyncing from the peer; this node may serve again |
| `pbd_replug` | WARN | state_machine.rs:9860 | a twinstor PBD is unplugged while Online and healthy; re-running plug_and_multipath |
| `peer_emergency_reset_issued` | INFO | state_machine.rs:17509 | pool-level HA reset issued; confirming the peer rejoins the liveset before declaring it resolved |
| `peer_emergency_resolve` | WARN | state_machine.rs:17438 | resolving the peer's emergency-mode wedge: pool-level HA reset (disable dissolves the disagreement; the re-... |
| `peer_emergency_resolve` step=`disable_failed` | WARN | state_machine.rs:17466 | peer emergency resolve: pool-ha-disable failed or timed out; no reset issued (the re-arm obligation stays; ... |
| `peer_emergency_resolve` step=`enable_failed` | WARN | state_machine.rs:17494 | peer emergency resolve: pool-ha-enable failed; the re-arm obligation stays and the HA watchdog retries it |
| `peer_emergency_resolve` step=`ha_task_pending` | WARN | state_machine.rs:17417 | peer emergency resolve: an earlier pool HA operation is still pending in XAPI; not issuing another reset (r... |
| `peer_emergency_resolve` step=`sr_unresolved` | WARN | state_machine.rs:17405 | peer emergency resolve: cannot resolve the SR uuid; holding, retried next pass |
| `peer_emergency_resolve_failed` | WARN | state_machine.rs:17290 | the automatic pool-level HA reset did NOT bring the peer back — its xhad could not rejoin the liveset. It n... |
| `peer_emergency_resolved` | INFO | state_machine.rs:17270 | peer is live again after the pool-level HA reset — the emergency-mode wedge is cleared |
| `peer_emergency_suspect` | WARN | state_machine.rs:17325 | peer xapi reports not-live while its daemon answers on the replication network and pool HA is armed: suspec... |
| `peer_emergency_wedge_confirmed` | INFO | state_machine.rs:17367 | peer rejoined this host's xhad liveset while the HA-armed-mismatch signature persists (peer xapi raw-not-li... |
| `peer_fork_claim` | INFO | state_machine.rs:18821 | split-brain: the peer's beacon claim for this fork |
| `peer_object_restore` | WARN | state_machine.rs:11403 | the DRBD peer object declared in the resource file is missing from the kernel; re-creating it |
| `peer_portal_fence_failed` | WARN | state_machine.rs:10979 | could not install the peer-portal fence rule; retrying |
| `peer_portal_fenced` | WARN | state_machine.rs:10996 | fate-shared: this Primary is disconnected from the peer, so its guests must not reach the peer's copy; fenc... |
| `peer_portal_restored` | INFO | state_machine.rs:11082 | the path through the peer's iSCSI portal fenced at the split is back |
| `peer_won_attested` | WARN | state_machine.rs:13178, state_machine.rs:18396 | init: the peer attests it took the win for this split-brain fork; resolving as the loser from bring-up |
| `peer_yield_not_current` | WARN | state_machine.rs:18864 | split-brain: the peer attests a fence-peer yield, but this copy is not UpToDate; not claiming the win on it |
| `pool_masterless` | WARN | state_machine.rs:16058 | POOL IS MASTERLESS: this node's pool.conf points at itself as the master it should follow; xe is wedged poo... |
| `pool_masterless_suspect` | WARN | state_machine.rs:15940 | pool.conf says this node is a slave of ITSELF; watching (xapi may still be transitioning) |
| `pool_mutual_slave` | WARN | state_machine.rs:15901 | POOL IS MASTERLESS (mutual-slave): this node follows the peer and the peer PROVABLY follows this node; xe i... |
| `previous_forced_reboot` | WARN | state_machine.rs:6930 | the previous daemon life forced a reboot (unix time, version, episode, reason) |
| `promote` | INFO | state_machine.rs:12441 | try_go_online: promoting to Primary |
| `promote_after_reconnect_deferred` | WARN | state_machine.rs:22360 | reconnected but could not take the Primary role back yet (no forced promote); the bring-up path retries |
| `protected_vm_first_seen` | INFO | state_machine.rs:16696 | halted VM with a restart priority first seen by this master |
| `protected_vm_halted` | INFO/WARN | state_machine.rs:16779, state_machine.rs:16822 | halted VM with a restart priority, shut down on purpose and not traced to an incident: not alerted, left alone |
| `protected_vm_restart` | WARN | state_machine.rs:16858 | starting incident-orphaned protected VM(s) (restart_halted_protected_vms) |
| `protected_vm_restart` step=`deferred` | INFO | state_machine.rs:16851 | more halted protected VMs to start than one scan does; the rest next scan |
| `protected_vm_restarted` | INFO | state_machine.rs:16902 | protected VM starts issued |
| `protected_vm_start_failed` | WARN | state_machine.rs:16886 | could not start a halted protected VM; retrying after a back-off |
| `protected_vm_started` | INFO | state_machine.rs:16867 | halted protected VM started (restart_halted_protected_vms) |
| `protected_vm_watch` step=`selector_unreadable` | WARN | state_machine.rs:16731 | halted protected VMs: XAPI did not answer whether they are still protected or marked suspect; alerting with... |
| `readmission_rearm_issued` | WARN | state_machine.rs:14505 | synchronized HA restart done and pool HA re-armed; confirming the peer rejoined the liveset before declarin... |
| `readmission_recover_unconfirmed` | WARN | state_machine.rs:14230 | the synchronized HA restart re-armed pool HA but the peer did not rejoin the liveset within the confirmatio... |
| `refused_flip` | WARN | state_machine.rs:18436 | the peer also claims split-brain loser (it refused our discard); this node did not yield and kept serving d... |
| `rejoin_selfheal_deferred` | INFO | state_machine.rs:9412, state_machine.rs:9705 | stranded-master toolstack yield DEFERRED: node participates in XHA (liveset member / mid-readmission); coll... |
| `replication_path_restored` | INFO | state_machine.rs:10047 | replication re-established on the direct link |
| `resolver_decision` | WARN | state_machine.rs:23293 |  |
| `resume_io_raced_drop` | WARN | state_machine.rs:10927 | the connection dropped while this copy's I/O was being lifted; the lift may have released the freeze a new ... |
| `resume_io_skipped` | INFO | state_machine.rs:10902 | not lifting this copy's I/O suspend: the connection dropped again or a fence-peer decision holds the I/O |
| `resync_stall` | WARN | state_machine.rs:9166 | DRBD resync stalled at {pct:.2}% for >= {mins} min; redundancy is not returning |
| `sb_flip_held` | WARN | state_machine.rs:22080 | split-brain: the peer also refused the discard (symmetric mutual-loser). Not flipping to a second serving m... |
| `self_unwedge_clear` | WARN | state_machine.rs:17619 | breaking my own HA emergency wedge: the peer's pool-level reset cannot recruit a cycling xhad, so clearing ... |
| `self_unwedge_signature_miss` | WARN | state_machine.rs:17581 | the core wedge holds (disarmed, xhad not joined, storage healthy, peer is master) but no invalid-state sign... |
| `self_unwedge_suspect` | WARN | state_machine.rs:17595 | local HA is disarmed while pool HA is armed and my xhad is stuck in the invalid-state cycle with the peer s... |
| `slave_rejoin_reconnect` | WARN | state_machine.rs:9771 | returning slave: restarting the local toolstack to re-establish the slave->master XAPI link (non-destructiv... |
| `split_brain_diverged` | WARN | state_machine.rs:13261, state_machine.rs:18484 | init: split-brain is a TRUE fork; this node holds fewer un-replicated writes, resolving as the loser and ro... |
| `split_brain_fast_resolve` | INFO | state_machine.rs:12951 | DRBD declared a split-brain and the generation lineage decides it: resolving now instead of waiting out the... |
| `split_guests` | INFO | state_machine.rs:10966, state_machine.rs:19425 | freeze mode: a guest on the SR runs here during the split |
| `splitbrain_thrash` | WARN | state_machine.rs:18285 | split-brain resolution thrashing: {cycles} cycles within {}s; pausing auto-resolve and holding (no discard,... |
| `stale_primary_demote` | WARN | state_machine.rs:20413 | serving Primary is the handshake-agreed stale side (kernel keeps refusing SyncTarget-while-Primary); demoti... |
| `standdown_guest_stop_blocked` | WARN | state_machine.rs:14753 | isolated stand-down HELD: could not confirm the local guest domains are stopped, so refusing to tear storag... |
| `standdown_suppressed_ha_off` | WARN | state_machine.rs:15374, state_machine.rs:15541 | isolated SLAVE past the stand-down window with VMs here, but pool HA is NOT enabled: suppressing the fence-... |
| `standing_offer_landed_unarmed` | WARN | state_machine.rs:22258 | this loser's discard offer landed while it was not armed for it (Connected, Secondary, resynced from the pe... |
| `startup_state` | INFO | state_machine.rs:6937 | durable state found at startup ("-" = absent) |
| `startup_state_files` | INFO | state_machine.rs:6977 | every state file at startup (path=first line) |
| `stranded_master_force_reset` | WARN | state_machine.rs:9542 | stranded former-master: toolstack-restart yields did not demote (persistent forked dual-master); the peer i... |
| `stranded_master_force_reset_deferred` | WARN | state_machine.rs:9527 | stranded former-master: ready to force the demote, but the real master's management IP is unreachable (this... |
| `stranded_master_reclaim` | WARN | state_machine.rs:9589 | dual-master with a NON-serving peer (it is DRBD Secondary, e.g. a stood-down isolated master): holding mast... |
| `stranded_master_yield` | WARN | state_machine.rs:9484 | stranded former-master: restarting the local toolstack so xapi demotes this node to slave of the real maste... |
| `stuck_primary_demote` | WARN | state_machine.rs:20437 | suspended non-serving Primary stuck StandAlone (peer keeps refusing the reconnect); demoting in place so th... |
| `stuck_primary_fence` | ERROR | state_machine.rs:20577 | stuck-Primary demote could not complete; self-fencing (forced reboot) to recover |
| `survivor_ha_reset` | WARN | state_machine.rs:23472 | DUAL-MASTER wedge: surviving master briefly disabling pool HA so the wedged peer can demote (its emergency_... |
| `survivor_ha_reset` step=`disable_failed` | WARN | state_machine.rs:23563 | survivor_ha_reset: pool-ha-disable failed or timed out; aborting the reset, the re-arm obligation stays (HA... |
| `survivor_ha_reset` step=`ha_task_pending` | WARN | state_machine.rs:23466 | survivor_ha_reset: a pool HA operation is still pending in XAPI; not disabling HA on top of it (the heal re... |
| `symmetric_tiebreak` | WARN | state_machine.rs:13386, state_machine.rs:13404, state_machine.rs:18554 | init: symmetric split-brain wedge; this node is the non-keeper, resolving as the loser and resyncing from t... |
| `undecidable` | ERROR | state_machine.rs:20245 | split-brain: cannot determine pool master; refusing to auto-discard, alerting and retrying |
| `vdi_lock_sweep` | WARN | state_machine.rs:17075 | clearing stale VDI activating lock(s): half-acquired leftovers of activations killed by a host reset (upstr... |
| `vdi_lock_swept` | INFO | state_machine.rs:17088 | stale activating locks cleared |
| `version_skew` | WARN | state_machine.rs:13823 | peer daemon version '{peer}' != local '{local}' beyond the upgrade grace window |
| `version_skew` step=`detected` | INFO | state_machine.rs:13794 | the two nodes run different twinstor versions (alerted if it lasts past the grace) |
| `wfbitmap_autoclear` | WARN | state_machine.rs:8547 | replication stuck in bitmap exchange (WFBitMap) with the connection up -- the promote-race wedge; bouncing ... |
| `whole_switch_guard_disarm` | WARN | state_machine.rs:7277 | fast fence-guard confirmed a whole-switch outage (my client gateway down + the peer's gateway also down ove... |
| `whole_switch_guard_release` | INFO | state_machine.rs:7294 | fast fence-guard: whole-switch over (client gateway back, or the peer now serves its gateway UP); cleared t... |
| `whole_switch_hold` | WARN | state_machine.rs:15232 | both hosts are isolated from the client network (whole-switch outage): the peer's gateway is ALSO down over... |
| `whole_switch_release` | INFO | state_machine.rs:15260 | client network restored after a whole-switch hold; re-arming local HA |
| `winner_crash_boundary` | WARN | state_machine.rs:21443, state_machine.rs:21460 | split-brain winner (bring-up): cannot read this host's guest domains; holding |
| `winner_path_exposure` | WARN | state_machine.rs:21853 | split-brain winner: the local storage path was not confirmed active during the episode; guests here may hav... |
| `winner_resume_io` | WARN | state_machine.rs:10727 | split-brain: the peer accepted the discard; resuming this kept copy's frozen I/O |
| `xha_orphan_reconcile` | WARN | state_machine.rs:18095 | pool HA reads off but the local xhad is still present; pool-ha-enable would fail with DAEMON_IS_PRESENT (Xh... |
| `xha_orphan_reconcile_fast` | WARN | state_machine.rs:17918 | pool-ha-enable was refused DAEMON_IS_PRESENT (Xha_error 15); stopping the local xhad and re-enabling immedi... |
| `xha_orphan_stop_failed` | WARN | state_machine.rs:17993 | ha_stop_daemon did NOT reap the local xhad; NOT force-killing it (a SIGKILL skips the watchdog disarm and w... |
| `yield` | WARN | state_machine.rs:20677 | DUAL-MASTER auto-heal: this node is the freshly-rebooted, VM-less spurious master; restarting the local too... |
| `yield_deferred_storage_attach` | INFO | state_machine.rs:9454, state_machine.rs:9741 | holding the toolstack yield: an iSCSI/PBD attach is in flight, a restart would wipe it (collapse timer pres... |
| `yield_fence` | WARN | state_machine.rs:20119 | DUAL-MASTER wedge: restart budget exhausted and the survivor's escalation window has passed; self-fencing t... |
| `yield_hold` | WARN | state_machine.rs:15795 | yielded and frozen while the gateway is healthy and the peer stays unreachable: THIS NEEDS AN OPERATOR DECI... |
| `yield_latch_promote_hold` | WARN | state_machine.rs:18721 | not promoting: this node is latched as the loser of the current split-brain fork (fence-peer yield); waitin... |
| `yield_latched` | INFO | state_machine.rs:571, state_machine.rs:18663 | fence-peer yield with no guest on the SR here; latching the loser verdict for this fork and announcing it o... |
| `yield_latched` step=`pending_relatch_dropped` | INFO | state_machine.rs:21412 | a pending yield re-latch no longer applies to this copy (not the frozen Primary it was recorded on); dropped |
| `yield_latched` step=`relatch_skipped` | INFO | state_machine.rs:21385 | a guest on the SR runs here (or none could be confirmed) after the resume; the yield latch is not moved ont... |
| `yield_latched` step=`relatched_after_lift` | INFO | state_machine.rs:21338 | the crash boundary's I/O lift moved this copy's generation; the yield latch follows it, so the beacon keeps... |
| `yield_loser_holding` | INFO/WARN | state_machine.rs:22021, state_machine.rs:22287 | split-brain: nobody took the discard yet; this node yielded, staying down (Secondary, target off) with the ... |
| `yield_not_frozen` | WARN | state_machine.rs:10876 | guests here may be writing on a copy the split can discard |
| `yield_with_guests` | INFO | state_machine.rs:503, state_machine.rs:18651 | this yield had guests (or live SR disks, or an unreadable state) on this host: it does not decide the split... |
| `yielded_keep` | WARN | state_machine.rs:18685 | this node yielded with guests on the SR and its copy holds more by the lineage: keeping it; its frozen I/O ... |
| `yielded_loser` | WARN | state_machine.rs:13156, state_machine.rs:18368 | init: this node's fence-peer yielded and it has not served since; resolving as the split-brain loser from b... |
