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
| `arb_yield_behind_hold` | WARN | state_machine.rs:19668 | this node yielded and its copy is behind the peer's, but the peer's iSCSI portal does not answer; holding f... |
| `arb_yield_not_decided` | INFO/WARN | state_machine.rs:19578, state_machine.rs:19597, state_machine.rs:19610, state_machine.rs:19642 | arbitrated yield: the kernel log no longer shows this resource's disconnect (or could not be read), so a re... |
| `arb_yield_step_down` | WARN | state_machine.rs:19702 | this node yielded (I/O frozen since the disconnect) and DRBD reads its copy as behind the peer's, with no f... |
| `arb_yield_xapi_wait_over` | WARN | state_machine.rs:19651 | arbitrated yield: this host's XAPI still does not answer; stepping down without it (the crash boundary stop... |
| `beacon_claim` | INFO | state_machine.rs:7129, state_machine.rs:7142 | split-brain claim served to the peer changed (Y yielded, W won, G frozen with guests, R giving way) |
| `boot_gate_peer_alive` | INFO/WARN | state_machine.rs:12744, state_machine.rs:12760, state_machine.rs:12858 | boot gate: the peer was seen serving; this node will not serve alone until it has reconnected (or an operat... |
| `both_outdated_tiebreak` | WARN | state_machine.rs:12133, state_machine.rs:12175, state_machine.rs:12189, state_machine.rs:12195, state_machine.rs:12202 | both replicas Outdated but node name == peer name: cannot elect a keeper; not running the tiebreak. Fix the... |
| `capacity` | WARN | state_machine.rs:9276 | SR physical utilisation at {p}% ({sev}); guests hit ENOSPC when it fills |
| `consistent_diskless_force` | INFO/WARN | state_machine.rs:11763, state_machine.rs:11788, state_machine.rs:11806, state_machine.rs:11830 | refusing the sole-survivor force-promote: configured node.name does not match this host's kernel hostname (... |
| `consistent_diskless_manual` | WARN | state_machine.rs:11884 | sole-survivor deadlock needs an operator decision (no safe automatic action); raising the guided-manual alert |
| `converged` | INFO | state_machine.rs:19969, state_machine.rs:20530, state_machine.rs:22428, state_machine.rs:23660 | split-brain: peer reconnected and is resyncing from us |
| `demoted_survivor_reassert` | WARN | state_machine.rs:9953 | re-asserting pool mastership: this host holds the only UpToDate copy but was demoted below a stale (Inconsi... |
| `discard_offer_withdraw` | WARN | state_machine.rs:10508 | this node may still carry a --discard-my-data offer from an earlier round but now keeps its copy; withdrawi... |
| `discard_offer_withdraw_failed` | ERROR/WARN | state_machine.rs:10482, state_machine.rs:10561 | could not withdraw a possible stale discard offer (its state is unreadable); connecting anyway |
| `disks_suspect_mark` | INFO | state_machine.rs:16627 | a VM marked disks-suspect was started again since the mark |
| `episode_close` | INFO | state_machine.rs:20860, state_machine.rs:20873 | heal episode closed (daemon died with it open; closing on restart so every open has a close) |
| `episode_open` | INFO | state_machine.rs:20835 |  |
| `failback_complete` | INFO | state_machine.rs:10423 | failback: replication re-established on the direct link |
| `failback_dance` | WARN | state_machine.rs:10338 | failback: moving replication to the direct link (brief local target stop; consumers ride multipath via the ... |
| `failback_dance` step=`restore_paths_failed` | WARN | state_machine.rs:10397 | failback: standby path restore failed; the steering restore retries it |
| `failback_latched` | WARN | state_machine.rs:10095 | direct link flapped back {FAILBACK_MAX_STRIKES} times; automatic failback is now latched off until an opera... |
| `failback_missed` | WARN | state_machine.rs:10428 | failback: dance completed but the connect landed on the fallback path again; backing off |
| `failback_strike` | WARN | state_machine.rs:10088 | direct link flapped back to the fallback path shortly after a failback; escalating the stability hold |
| `fateshared_lone_survivor_hold` | WARN | state_machine.rs:12386 | fate-shared lone-survivor promote held to avoid a VM double-run: the peer is unreachable and, with no direc... |
| `fateshared_lone_survivor_isolated` | WARN | state_machine.rs:12339 | fate-shared lone-survivor hold expired but this node reaches neither its gateway nor any ping-target: it is... |
| `fateshared_no_tiebreaker` | WARN | state_machine.rs:12294 | fate-shared lone-survivor hold expired, but no gateway or ping-target is configured: this node cannot tell ... |
| `fence_peer_verdict` | INFO | state_machine.rs:6919 | fence-peer handler verdict (details in /var/log/twinstor-fence-peer.log, same run pid) |
| `forced_reboot` | ERROR | state_machine.rs:20325 | forcing a reboot now |
| `fork_claim_win` | WARN | state_machine.rs:13209, state_machine.rs:18421 | init: the peer yielded during the partition and never claims this fork (or this node already took it); keep... |
| `freeze_budget_keep` | WARN | state_machine.rs:18950 | freeze mode: the freeze budget ran out after the peer gave way; resuming this copy's I/O before the I/O sta... |
| `freeze_budget_keep` step=`resume_failed` | WARN | state_machine.rs:18961 | freeze mode: this node's guests reached the freeze budget without a decision; stopping them (nothing they w... |
| `freeze_budget_step_down` | WARN | state_machine.rs:18971 | freeze mode: this node's guests reached the freeze budget without a decision; stopping them (nothing they w... |
| `freeze_give_way` | WARN | state_machine.rs:19446 | freeze mode: the peer froze with guests and none ran here during the split; giving way (this copy's dom0-on... |
| `freeze_give_way_dropped` | WARN | state_machine.rs:19496 | freeze mode: the peer no longer holds a frozen copy; dropping the give-way and returning to bring-up |
| `freeze_give_way_expired` | WARN | state_machine.rs:19148 | freeze mode: the give-way never landed; dropping it and returning to bring-up |
| `freeze_give_way_holding` | INFO | state_machine.rs:21981 | freeze mode: giving way; Secondary with the target down, waiting for the frozen peer's new generation befor... |
| `freeze_give_way_landed` | INFO | state_machine.rs:19131 | freeze mode: the discard landed; resyncing from the peer and serving again |
| `freeze_give_way_reoffer` | INFO | state_machine.rs:19122, state_machine.rs:19477 | freeze mode: giving way; the peer took it, offering the discard |
| `freeze_give_way_taken` | INFO | state_machine.rs:19758 | freeze mode: the peer took this node's give-way (it may serve on the only full copy) |
| `freeze_give_way_taken_restored` | INFO | state_machine.rs:19107 | freeze mode: the peer had taken this node's give-way (recorded before a restart) |
| `freeze_keep` | WARN | state_machine.rs:19210 | freeze mode: the peer gave way to this node's frozen guests; new data generation here, resuming the guests ... |
| `freeze_keep` step=`new_generation_failed` | WARN | state_machine.rs:19219 | freeze mode: could not start a new data generation; retrying |
| `freeze_keep_resume` | INFO | state_machine.rs:19237 | freeze mode: resumed this copy's I/O; the peer discards its copy on the handshake |
| `freeze_keep_resume` step=`resume_failed` | WARN | state_machine.rs:19245 | freeze mode: the peer keeps its copy (guests ran there); stopping this node's frozen guests (nothing they w... |
| `freeze_step_down` | WARN | state_machine.rs:19183, state_machine.rs:19253, state_machine.rs:19317, state_machine.rs:19347 | freeze mode: this node yielded with no guest on the SR; stepping down (resync from the peer), no reboot |
| `freeze_survivor_keeps` | WARN | state_machine.rs:19437 | freeze mode: the peer froze with guests, but guests ran here during the split; keeping this copy, the peer ... |
| `freeze_wait` | INFO | state_machine.rs:19308, state_machine.rs:19388, state_machine.rs:19490 | freeze mode: this copy is behind the frozen peer's, but this host's XAPI does not answer yet; holding until... |
| `freeze_win_rebound` | INFO | state_machine.rs:19085 | freeze mode: the giving peer's generation moved; re-binding this node's win to it |
| `freshness_lineage` | INFO/WARN | state_machine.rs:13293, state_machine.rs:13320, state_machine.rs:18519 | init: split-brain resolved by DRBD generation lineage: this node is behind the peer, resolving as the loser... |
| `freshness_override` | WARN | state_machine.rs:13375, state_machine.rs:18583 | init: freshness gate overrode the data-blind verdict to protect data currency |
| `fs_freeze_connect_int` | INFO | state_machine.rs:11232 | fate-shared freeze mode: could not set the reconnect interval |
| `fs_freeze_fencing` | INFO | state_machine.rs:11170, state_machine.rs:13644 | fate-shared freeze mode: DRBD fencing policy set |
| `fs_freeze_peer` | INFO | state_machine.rs:11379 | engaged on the pair |
| `guest_arrived_while_frozen` | WARN | state_machine.rs:10839 | a guest domain using the SR appeared on this host while its copy is frozen by this node's fence-peer yield ... |
| `ha_armed_mismatch` | WARN | state_machine.rs:17138 | this host is locally HA-disarmed while pool HA is enabled; its next reboot springs an xapi trap |
| `ha_deadlock_exit` | WARN | state_machine.rs:14123 | isolated-master dual-master HA deadlock (data-safe): yielding as the returned/forked node: local toolstack ... |
| `ha_readmission_dance_scheduled` | WARN | state_machine.rs:14400 | XHA join deadlock detected (both sides' stale liveset claims re-poisoning each other); scheduling the quies... |
| `ha_readmission_flap` | WARN | state_machine.rs:17190 | returning node rejoined the XHA liveset but has stayed locally HA-disarmed past the grace: XAPI is stuck re... |
| `ha_readmission_quiesce` | WARN | state_machine.rs:14433 | re-admission dance phase 1: local emergency HA disable (quiescence) |
| `ha_readmission_recovered` | INFO | state_machine.rs:14225 | the peer rejoined the liveset after the synchronized HA restart; join-deadlock recovery confirmed |
| `ha_readmission_revive` | WARN | state_machine.rs:14384 | XHA daemon is dead with nothing retrying it (xha.log silent all window) while storage is healthy (the boot-... |
| `ha_readmission_revive_failed` | WARN | state_machine.rs:14578 | XHA daemon restart did not reach Online; leaving the manual runbook alert armed (next automatic attempt in ... |
| `ha_readmission_revived` | INFO | state_machine.rs:14562 | XHA daemon restarted and rejoined the liveset; HA protection restored |
| `ha_readmission_storm` | WARN | state_machine.rs:14331 | XHA cannot rejoin and the storm is still active (statefile errors / storage not healthy); fix storage first... |
| `ha_readmission_stuck` | WARN | state_machine.rs:14352 | XHA re-admission stuck; automated recovery unavailable (disabled, capped, or preconditions failed); manual ... |
| `ha_rearm_after_fence` | WARN | state_machine.rs:18262 | peer rejoined after a fence and pool HA is off; writing the re-arm obligation to restore HA |
| `ha_rearm_fulfilled` | INFO | state_machine.rs:17872 | pool HA is armed and settled; clearing the re-arm obligation |
| `ha_rearm_held_maintenance` | INFO | state_machine.rs:17825 | a re-arm obligation is pending but a host is in operator maintenance (MAINTENANCE_MODE), or the toolstack d... |
| `ha_rearm_hold` | INFO | state_machine.rs:17746, state_machine.rs:17763, state_machine.rs:17862 | a pool-HA re-arm is owed, but the routine that disabled HA is still running; it re-arms HA itself |
| `ha_rearm_pbd_plug` | INFO | state_machine.rs:17714 | plugging the twinstor SR's PBD before pool-ha-enable (it was left unplugged by a reconnect/readmission; ena... |
| `ha_rearm_retry` | WARN | state_machine.rs:17887 | pool HA is still off from a daemon-initiated disable; retrying the re-arm with self-heal (persisted obligat... |
| `ha_rearm_skipped_operator_disable` | INFO | state_machine.rs:18236 | peer rejoined with pool HA off, but HA was already off while the peer was alive, so treating this as a deli... |
| `ha_rearm_toolstack_reset` | WARN | state_machine.rs:17951 | pool HA re-arm keeps failing after the light self-heal; restarting the local toolstack to clear a stuck 'HA... |
| `ha_reset_timeout` | WARN | state_machine.rs:23666 | survivor_ha_reset: peer did not demote within the window; HA re-armed, manual review |
| `heal_stage` | INFO | state_machine.rs:20762, state_machine.rs:20789 |  |
| `host_auto_enable` | WARN | state_machine.rs:16552 | this host is disabled (maintenance mode) with storage fully healthy and no operator marker: the boot-time e... |
| `host_auto_enable_failed` | WARN | state_machine.rs:16574 | host-enable did not stick; will retry after the cooldown (manual: xe host-enable) |
| `host_auto_enable_held_isolated` | INFO | state_machine.rs:16461 | not auto-enabling this disabled host: replication is healthy but the client network is unreachable, so it i... |
| `host_auto_enable_hold` | INFO | state_machine.rs:16514, state_machine.rs:16529 | this host is disabled; not enabling it yet |
| `host_auto_enabled` | INFO | state_machine.rs:16560 | host enabled; HA placement restored |
| `hostname_drift` | WARN | state_machine.rs:9101 | kernel hostname '{live}' no longer matches the configured node name '{configured}'; DRBD on-sections will n... |
| `incident_begin` | INFO | state_machine.rs:19748 | incident id for the log lines of this freeze or yield (no heal episode) |
| `incident_end` | INFO | state_machine.rs:19744, state_machine.rs:19919 | incident id closed (unused for 30 min) |
| `init_loser_inconsistent` | WARN | state_machine.rs:13141 | init: split-brain with a local Inconsistent replica (interrupted resync); resolving as the loser automatica... |
| `internal_shutdown_policy` step=`decided` | INFO | state_machine.rs:16977 | pool ha-reboot-vm-on-internal-shutdown: acting on freeze mode's state |
| `internal_shutdown_policy` step=`record_dropped` | INFO | state_machine.rs:17037 | freeze mode not engaged: twinstor's record dropped (the setting was already as found) |
| `internal_shutdown_policy` step=`record_off` | INFO | state_machine.rs:17021 | pool setting already off (not by twinstor this daemon life): recorded |
| `internal_shutdown_policy` step=`restore_failed` | WARN | state_machine.rs:17032 | XAPI refused or did not answer putting the pool setting back; retried |
| `internal_shutdown_policy` step=`restored` | WARN | state_machine.rs:17046 | freeze mode not engaged: pool ha-reboot-vm-on-internal-shutdown put back on (twinstor had turned it off) |
| `internal_shutdown_policy` step=`rollback_failed` | WARN | state_machine.rs:13605 | daemon stopping for a release without freeze mode: XAPI too slow to put the pool's ha-reboot-vm-on-internal... |
| `internal_shutdown_policy` step=`rollback_noop` | INFO | state_machine.rs:13600 | daemon stopping for a release without freeze mode: XAPI too slow to put the pool's ha-reboot-vm-on-internal... |
| `internal_shutdown_policy` step=`rollback_restored` | WARN | state_machine.rs:13593 | daemon stopping for a release without freeze mode: XAPI too slow to put the pool's ha-reboot-vm-on-internal... |
| `internal_shutdown_policy` step=`rollback_timeout` | WARN | state_machine.rs:13612 | daemon stopping for a release without freeze mode: XAPI too slow to put the pool's ha-reboot-vm-on-internal... |
| `internal_shutdown_policy` step=`turn_off_failed` | WARN | state_machine.rs:16987 | XAPI refused or did not answer turning the pool setting off; retried |
| `internal_shutdown_policy` step=`turned_off` | WARN | state_machine.rs:16994 | fate-shared freeze mode: pool ha-reboot-vm-on-internal-shutdown turned off, so a guest a losing pool master... |
| `internal_shutdown_policy` step=`unreadable` | INFO | state_machine.rs:16965 | pool ha-reboot-vm-on-internal-shutdown unreadable (XAPI did not answer, or a XAPI without that setting): no... |
| `isolated_master` | WARN | state_machine.rs:15729 | the pool master is unreachable on the client network but its daemon is alive on the replication link: it ha... |
| `isolated_master_clock_held` | INFO | state_machine.rs:15681 | xe answered again but this node still follows the peer; holding the isolated-master maturity clock across t... |
| `isolated_rejoin` | INFO | state_machine.rs:15021 | client network stably returned and the peer is reachable on replication; restoring storage bottom-up and re... |
| `isolated_slave_standdown` | WARN | state_machine.rs:15410 | this slave is cut off from the client network while the master is reachable with its gateway up over the di... |
| `isolated_standdown` | WARN | state_machine.rs:14789, state_machine.rs:14800 | isolated slave past the stand-down window with the master reachable and UpToDate: leaving xhad armed and ma... |
| `isolated_standdown_hold` | WARN | state_machine.rs:14985, state_machine.rs:15014 | parked as a stood-down isolated master; storage held unavailable so VMs stay on the reachable peer. Reconne... |
| `keep_and_alert` | WARN | state_machine.rs:20146 | split-brain: peer reachable but will not resync; suspected dual-master wedge. This node is the survivor or ... |
| `leftover_lift_wait` | INFO | state_machine.rs:8460, state_machine.rs:10724 | I/O suspended (suspended:user) while Connected and both UpToDate: waiting for the connection to hold before... |
| `lineage_evidence` | INFO/WARN | state_machine.rs:23234, state_machine.rs:23254, state_machine.rs:23264 | split-brain resolver: DRBD generation lineage evidence read |
| `loopback_restore_pending` | WARN | state_machine.rs:11516 | failback: loopback login still failing; the periodic path self-heal will keep retrying |
| `loopback_restored` | INFO | state_machine.rs:11513 | failback: local loopback session re-established and local-path priority re-pinned |
| `loser_crash_boundary` | WARN | loser_boundary.rs:1236, loser_boundary.rs:1729, state_machine.rs:21172, state_machine.rs:21525, state_machine.rs:21636, state_machine.rs:21821 (+1) | {who}: crash boundary not established; holding both copies intact |
| `loser_crash_boundary` step=`confirm` | WARN | state_machine.rs:21100 | split-brain loser: the last check before the discard found the SR in use again; re-establishing the crash b... |
| `loser_crash_boundary` step=`datapaths` | INFO/WARN | loser_boundary.rs:1983, loser_boundary.rs:2064, loser_boundary.rs:2123, loser_boundary.rs:2131, loser_boundary.rs:2219, loser_boundary.rs:2307 (+1) | cannot read XAPI's storage datapaths on this host; closing leftover tapdisks directly instead |
| `loser_crash_boundary` step=`enumerate_unavailable` | INFO | loser_boundary.rs:1293, loser_boundary.rs:1299 | {who}: XAPI cannot list this host's guests; the local scan decides alone |
| `loser_crash_boundary` step=`hold` | WARN | state_machine.rs:21516 | crash boundary holding: nothing discarded, both copies kept, retrying |
| `loser_crash_boundary` step=`host_disable_skipped` | INFO | loser_boundary.rs:1311, loser_boundary.rs:1316 | {who}: the host disable did not answer in time; relying on the local scan (it may still land: enabled again... |
| `loser_crash_boundary` step=`host_disable_unconfirmed` | INFO | loser_boundary.rs:1329 | {who}: the host disable did not answer in time; relying on the local scan (it may still land: enabled again... |
| `loser_crash_boundary` step=`host_enable` | INFO/WARN | loser_boundary.rs:2703, loser_boundary.rs:2713, loser_boundary.rs:2764 | split-brain loser: the pair has converged (or this copy, which took the winner's history, now serves alone)... |
| `loser_crash_boundary` step=`not_required` | INFO | state_machine.rs:21264 | split-brain loser: no guest can be reading a diverged history through this node; no crash boundary needed |
| `loser_crash_boundary` step=`poweroff` | INFO/WARN | loser_boundary.rs:903, loser_boundary.rs:913, loser_boundary.rs:918, loser_boundary.rs:939, loser_boundary.rs:995, loser_boundary.rs:1001 (+1) | guest domains using the SR powered off at the hypervisor; xenopsd tears them down as a guest shutdown (no r... |
| `loser_crash_boundary` step=`rebuilt_after_discard` | ERROR/WARN | loser_boundary.rs:1618, loser_boundary.rs:1663, loser_boundary.rs:1680, loser_boundary.rs:1688, state_machine.rs:21228 | boundary record unreadable: every guest domain on the SR here counts as suspect |
| `loser_crash_boundary` step=`reconcile_stale` | INFO | loser_boundary.rs:2780 | split-brain loser: stale live record of a guest stopped here |
| `loser_crash_boundary` step=`record` | INFO | loser_boundary.rs:1251 | boundary record from another episode: keeping only its restart obligation |
| `loser_crash_boundary` step=`record_unreadable` | ERROR | state_machine.rs:21729 | the crash boundary's record cannot be read: owed guest restarts are not run automatically (start the halted... |
| `loser_crash_boundary` step=`release_before_peer_start` | INFO | loser_boundary.rs:2744 | split-brain loser: could not release this host's hold on the guest's disks before starting it on the winner |
| `loser_crash_boundary` step=`restart` | INFO/WARN | loser_boundary.rs:2876, loser_boundary.rs:2889, state_machine.rs:21600 | split-brain loser: owed guest restart |
| `loser_crash_boundary` step=`restart_escalated` | WARN | state_machine.rs:21763 | an owed guest restart keeps failing: alerted, retried at a slower pace |
| `loser_crash_boundary` step=`restart_here` | INFO | loser_boundary.rs:2832 | split-brain loser: the winner cannot attach this guest's disk while this host holds it; this node has rejoi... |
| `loser_crash_boundary` step=`restart_no_target` | WARN | state_machine.rs:21717 | owed guest restarts paused: XAPI cannot name this host or the peer |
| `loser_crash_boundary` step=`restart_xapi` | WARN | loser_boundary.rs:3657 | split-brain loser: restarting this host's XAPI agent (xapi only) so it drops the stale running record of a ... |
| `loser_crash_boundary` step=`resume_io` | INFO/WARN | loser_boundary.rs:1174, loser_boundary.rs:1185, loser_boundary.rs:1201 | this copy's I/O stays suspended: a domain on this host runs or is being built with an SR disk (or the local... |
| `loser_crash_boundary` step=`start` | INFO | state_machine.rs:21286 | split-brain loser: stopping the guests on this host that use the SR before discarding, so none of them is m... |
| `loser_crash_boundary` step=`started_after_withdrawal` | INFO | loser_boundary.rs:1643 | guest domains using the SR started on this host after this copy was withdrawn; they reach the SR through th... |
| `loser_crash_boundary` step=`stop` | INFO | loser_boundary.rs:1425 | {who}: stopping the guest domains that use the SR, all at once, before any withdrawal |
| `loser_crash_boundary` step=`stopped` | INFO | loser_boundary.rs:1507 | {who}: crash boundary established; the discard may proceed |
| `loser_crash_boundary` step=`suspend_io` | ERROR/INFO/WARN | loser_boundary.rs:1073, loser_boundary.rs:1078, loser_boundary.rs:1215, loser_boundary.rs:1220 | this yielded copy's I/O is running and could not be suspended again: it accepts writes that the discard wil... |
| `loser_crash_boundary` step=`tap_list_settled` | INFO | loser_boundary.rs:2429 | tap-ctl list settled after a tapdisk being torn down |
| `loser_crash_boundary` step=`tap_orphan_minors` | INFO | loser_boundary.rs:2412 | tap-ctl lists minors with no tapdisk process (left by a tapdisk that died); they hold no image |
| `loser_crash_boundary` step=`tapdisks` | INFO/WARN | loser_boundary.rs:1477, loser_boundary.rs:2028, loser_boundary.rs:2073, loser_boundary.rs:2080 | {who}: a guest is being started on this host during the boundary; stopping it too before releasing the disk... |
| `loser_crash_boundary` step=`unlisted_domain` | WARN | loser_boundary.rs:1535 | {who}: a guest domain using the SR runs here although XAPI does not list it on this host; stopping it too |
| `loser_discard` | WARN | state_machine.rs:21061 | split-brain: resolving as the loser, discarding local diverged writes and resyncing from the winner |
| `loser_holding` | WARN | state_machine.rs:22191 | split-brain: discard offered but the peer's handshake has not completed yet; holding down (Secondary, targe... |
| `loser_refused` | WARN | state_machine.rs:22141 | split-brain: the peer refused the discard handshake (it also claims loser); this node is entitled to the wi... |
| `loser_teardown_incomplete` | ERROR | state_machine.rs:21912, state_machine.rs:21937 | split-brain: the iSCSI backstore is still exported; not discarding this round (retrying the teardown on the... |
| `master_isolated` | WARN | state_machine.rs:15578 | this master is cut off from the client network while the peer is alive on the replication link: any VMs her... |
| `masterless_defer_active_rejoin` | WARN | state_machine.rs:16141 | slave-of-self persisted past the grace window without re-admission; repointing pool.conf at the peer master... |
| `masterless_defer_peer_returned` | INFO | state_machine.rs:16268 | masterless recovery: the peer no longer reads confirmed-dead in the local liveset at the moment of action; ... |
| `masterless_defer_stuck` | WARN | state_machine.rs:16166 | slave-of-self did not rejoin after repeated repoints; operator intervention needed |
| `masterless_defer_to_master` | INFO/WARN | state_machine.rs:16102, state_machine.rs:16247 | pool.conf says slave-of-self, but the peer is a live master: deferring (no transition) and rejoining as its... |
| `masterless_recover` | WARN | state_machine.rs:15919, state_machine.rs:16277 | starting the emergency master transition to break the mutual-slave deadlock (lower hostname acts) |
| `masterless_recover_failed` | WARN | state_machine.rs:16297 | pool.conf did not flip to master within the window; manual recovery needed (see the masterless alert) |
| `masterless_recover_unconfirmed` | WARN | state_machine.rs:16367 | emergency master transition done but the peer still reads as pool master; the slave-recovery demotion did n... |
| `masterless_recovered` | INFO | state_machine.rs:16383 | emergency master transition complete; this node is master again and the peer is demoted / gone |
| `mastership_flap` | WARN | state_machine.rs:7883 | pool.conf role flapping while DRBD is Connected: suspected XHA re-promotion fight (control plane unstable, ... |
| `multipath_queue_restore` | WARN | multipath.rs:238 | multipath map queues fewer retries than configured; reconfiguring with both paths up |
| `mutual_slave_suspect` | WARN | state_machine.rs:15857 | pool.conf follows the peer but xe is not answering; watching |
| `operator_discard_done` | INFO | state_machine.rs:11987 | operator discard: resyncing from the peer; this node may serve again |
| `pbd_replug` | WARN | state_machine.rs:9869 | a twinstor PBD is unplugged while Online and healthy; re-running plug_and_multipath |
| `peer_emergency_reset_issued` | INFO | state_machine.rs:17518 | pool-level HA reset issued; confirming the peer rejoins the liveset before declaring it resolved |
| `peer_emergency_resolve` | WARN | state_machine.rs:17447 | resolving the peer's emergency-mode wedge: pool-level HA reset (disable dissolves the disagreement; the re-... |
| `peer_emergency_resolve` step=`disable_failed` | WARN | state_machine.rs:17475 | peer emergency resolve: pool-ha-disable failed or timed out; no reset issued (the re-arm obligation stays; ... |
| `peer_emergency_resolve` step=`enable_failed` | WARN | state_machine.rs:17503 | peer emergency resolve: pool-ha-enable failed; the re-arm obligation stays and the HA watchdog retries it |
| `peer_emergency_resolve` step=`ha_task_pending` | WARN | state_machine.rs:17426 | peer emergency resolve: an earlier pool HA operation is still pending in XAPI; not issuing another reset (r... |
| `peer_emergency_resolve` step=`sr_unresolved` | WARN | state_machine.rs:17414 | peer emergency resolve: cannot resolve the SR uuid; holding, retried next pass |
| `peer_emergency_resolve_failed` | WARN | state_machine.rs:17299 | the automatic pool-level HA reset did not bring the peer back: its xhad could not rejoin the liveset. It ne... |
| `peer_emergency_resolved` | INFO | state_machine.rs:17279 | peer is live again after the pool-level HA reset; the emergency-mode wedge is cleared |
| `peer_emergency_suspect` | WARN | state_machine.rs:17334 | peer xapi reports not-live while its daemon answers on the replication network and pool HA is armed: suspec... |
| `peer_emergency_wedge_confirmed` | INFO | state_machine.rs:17376 | peer rejoined this host's xhad liveset while the HA-armed-mismatch signature persists (peer xapi raw-not-li... |
| `peer_fork_claim` | INFO | state_machine.rs:18830 | split-brain: the peer's beacon claim for this fork |
| `peer_object_restore` | WARN | state_machine.rs:11412 | the DRBD peer object declared in the resource file is missing from the kernel; re-creating it |
| `peer_portal_fence_failed` | WARN | state_machine.rs:10988 | could not install the peer-portal fence rule; retrying |
| `peer_portal_fenced` | WARN | state_machine.rs:11005 | fate-shared: this Primary is disconnected from the peer, so its guests must not reach the peer's copy; fenc... |
| `peer_portal_restored` | INFO | state_machine.rs:11091 | the path through the peer's iSCSI portal fenced at the split is back |
| `peer_won_attested` | WARN | state_machine.rs:13187, state_machine.rs:18405 | init: the peer attests it took the win for this split-brain fork; resolving as the loser from bring-up |
| `peer_yield_not_current` | WARN | state_machine.rs:18873 | split-brain: the peer attests a fence-peer yield, but this copy is not UpToDate; not claiming the win on it |
| `pool_masterless` | WARN | state_machine.rs:16067 | pool is masterless: this node's pool.conf points at itself as the master it should follow; xe is wedged poo... |
| `pool_masterless_suspect` | WARN | state_machine.rs:15949 | pool.conf says this node is a slave of itself; watching (xapi may still be transitioning) |
| `pool_mutual_slave` | WARN | state_machine.rs:15910 | pool is masterless (mutual-slave): this node follows the peer and the peer provably follows this node; xe i... |
| `previous_forced_reboot` | WARN | state_machine.rs:6939 | the previous daemon life forced a reboot (unix time, version, episode, reason) |
| `promote` | INFO | state_machine.rs:12450 | try_go_online: promoting to Primary |
| `promote_after_reconnect_deferred` | WARN | state_machine.rs:22365 | reconnected but could not take the Primary role back yet (no forced promote); the bring-up path retries |
| `protected_vm_first_seen` | INFO | state_machine.rs:16705 | halted VM with a restart priority first seen by this master |
| `protected_vm_halted` | INFO/WARN | state_machine.rs:16788, state_machine.rs:16831 | halted VM with a restart priority, shut down on purpose and not traced to an incident: not alerted, left alone |
| `protected_vm_restart` | WARN | state_machine.rs:16867 | starting incident-orphaned protected VM(s) (restart_halted_protected_vms) |
| `protected_vm_restart` step=`deferred` | INFO | state_machine.rs:16860 | more halted protected VMs to start than one scan does; the rest next scan |
| `protected_vm_restarted` | INFO | state_machine.rs:16911 | protected VM starts issued |
| `protected_vm_start_failed` | WARN | state_machine.rs:16895 | could not start a halted protected VM; retrying after a back-off |
| `protected_vm_started` | INFO | state_machine.rs:16876 | halted protected VM started (restart_halted_protected_vms) |
| `protected_vm_watch` step=`selector_unreadable` | WARN | state_machine.rs:16740 | halted protected VMs: XAPI did not answer whether they are still protected or marked suspect; alerting with... |
| `readmission_rearm_issued` | WARN | state_machine.rs:14514 | synchronized HA restart done and pool HA re-armed; confirming the peer rejoined the liveset before declarin... |
| `readmission_recover_unconfirmed` | WARN | state_machine.rs:14239 | the synchronized HA restart re-armed pool HA but the peer did not rejoin the liveset within the confirmatio... |
| `refused_flip` | WARN | state_machine.rs:18445 | the peer also claims split-brain loser (it refused our discard); this node did not yield and kept serving d... |
| `rejoin_selfheal_deferred` | INFO | state_machine.rs:9421, state_machine.rs:9714 | stranded-master toolstack yield deferred: node participates in XHA (liveset member / mid-readmission); coll... |
| `replication_path_restored` | INFO | state_machine.rs:10056 | replication re-established on the direct link |
| `resolver_decision` | WARN | state_machine.rs:23298 |  |
| `resume_io_raced_drop` | WARN | state_machine.rs:10936 | the connection dropped while this copy's I/O was being lifted; the lift may have released the freeze a new ... |
| `resume_io_skipped` | INFO | state_machine.rs:10911 | not lifting this copy's I/O suspend: the connection dropped again or a fence-peer decision holds the I/O |
| `resync_stall` | WARN | state_machine.rs:9175 | DRBD resync stalled at {pct:.2}% for >= {mins} min; redundancy is not returning |
| `sb_flip_held` | WARN | state_machine.rs:22085 | split-brain: the peer also refused the discard (symmetric mutual-loser). Not flipping to a second serving m... |
| `self_unwedge_clear` | WARN | state_machine.rs:17628 | breaking my own HA emergency wedge: the peer's pool-level reset cannot recruit a cycling xhad, so clearing ... |
| `self_unwedge_signature_miss` | WARN | state_machine.rs:17590 | the core wedge holds (disarmed, xhad not joined, storage healthy, peer is master) but no invalid-state sign... |
| `self_unwedge_suspect` | WARN | state_machine.rs:17604 | local HA is disarmed while pool HA is armed and my xhad is stuck in the invalid-state cycle with the peer s... |
| `slave_rejoin_reconnect` | WARN | state_machine.rs:9780 | returning slave: restarting the local toolstack to re-establish the slave->master XAPI link (non-destructiv... |
| `split_brain_diverged` | WARN | state_machine.rs:13270, state_machine.rs:18493 | init: split-brain is a true fork; this node holds fewer un-replicated writes, resolving as the loser and ro... |
| `split_brain_fast_resolve` | INFO | state_machine.rs:12960 | DRBD declared a split-brain and the generation lineage decides it: resolving now instead of waiting out the... |
| `split_guests` | INFO | state_machine.rs:10975, state_machine.rs:19430 | freeze mode: a guest on the SR runs here during the split |
| `splitbrain_thrash` | WARN | state_machine.rs:18294 | split-brain resolution thrashing: {cycles} cycles within {}s; pausing auto-resolve and holding (no discard,... |
| `stale_primary_demote` | WARN | state_machine.rs:20418 | serving Primary is the handshake-agreed stale side (kernel keeps refusing SyncTarget-while-Primary); demoti... |
| `standdown_guest_stop_blocked` | WARN | state_machine.rs:14762 | isolated stand-down held: could not confirm the local guest domains are stopped, so refusing to tear storag... |
| `standdown_suppressed_ha_off` | WARN | state_machine.rs:15383, state_machine.rs:15550 | isolated slave past the stand-down window with VMs here, but pool HA is not enabled: suppressing the fence-... |
| `standing_offer_landed_unarmed` | WARN | state_machine.rs:22263 | this loser's discard offer landed while it was not armed for it (Connected, Secondary, resynced from the pe... |
| `startup_state` | INFO | state_machine.rs:6946 | durable state found at startup ("-" = absent) |
| `startup_state_files` | INFO | state_machine.rs:6986 | every state file at startup (path=first line) |
| `stranded_master_force_reset` | WARN | state_machine.rs:9551 | stranded former-master: toolstack-restart yields did not demote (persistent forked dual-master); the peer i... |
| `stranded_master_force_reset_deferred` | WARN | state_machine.rs:9536 | stranded former-master: ready to force the demote, but the real master's management IP is unreachable (this... |
| `stranded_master_reclaim` | WARN | state_machine.rs:9598 | dual-master with a non-serving peer (it is DRBD Secondary, e.g. a stood-down isolated master): holding mast... |
| `stranded_master_yield` | WARN | state_machine.rs:9493 | stranded former-master: restarting the local toolstack so xapi demotes this node to slave of the real maste... |
| `stuck_primary_demote` | WARN | state_machine.rs:20442 | suspended non-serving Primary stuck StandAlone (peer keeps refusing the reconnect); demoting in place so th... |
| `stuck_primary_fence` | ERROR | state_machine.rs:20582 | stuck-Primary demote could not complete; self-fencing (forced reboot) to recover |
| `survivor_ha_reset` | WARN | state_machine.rs:23477 | dual-master wedge: surviving master briefly disabling pool HA so the wedged peer can demote (its emergency_... |
| `survivor_ha_reset` step=`disable_failed` | WARN | state_machine.rs:23568 | survivor_ha_reset: pool-ha-disable failed or timed out; aborting the reset, the re-arm obligation stays (HA... |
| `survivor_ha_reset` step=`ha_task_pending` | WARN | state_machine.rs:23471 | survivor_ha_reset: a pool HA operation is still pending in XAPI; not disabling HA on top of it (the heal re... |
| `symmetric_tiebreak` | WARN | state_machine.rs:13395, state_machine.rs:13413, state_machine.rs:18563 | init: symmetric split-brain wedge; this node is the non-keeper, resolving as the loser and resyncing from t... |
| `undecidable` | ERROR | state_machine.rs:20250 | split-brain: cannot determine pool master; refusing to auto-discard, alerting and retrying |
| `vdi_lock_sweep` | WARN | state_machine.rs:17084 | clearing stale VDI activating lock(s): half-acquired leftovers of activations killed by a host reset (upstr... |
| `vdi_lock_swept` | INFO | state_machine.rs:17097 | stale activating locks cleared |
| `version_skew` | WARN | state_machine.rs:13832 | peer daemon version '{peer}' != local '{local}' beyond the upgrade grace window |
| `version_skew` step=`detected` | INFO | state_machine.rs:13803 | the two nodes run different twinstor versions (alerted if it lasts past the grace) |
| `wfbitmap_autoclear` | WARN | state_machine.rs:8556 | replication stuck in bitmap exchange (WFBitMap) with the connection up (the promote-race wedge); bouncing t... |
| `whole_switch_guard_disarm` | WARN | state_machine.rs:7287 | fast fence-guard confirmed a whole-switch outage (my client gateway down + the peer's gateway also down ove... |
| `whole_switch_guard_release` | INFO | state_machine.rs:7304 | fast fence-guard: whole-switch over (client gateway back, or the peer now serves its gateway up); cleared t... |
| `whole_switch_hold` | WARN | state_machine.rs:15241 | both hosts are isolated from the client network (whole-switch outage): the peer's gateway is also down over... |
| `whole_switch_release` | INFO | state_machine.rs:15269 | client network restored after a whole-switch hold; re-arming local HA |
| `winner_crash_boundary` | WARN | state_machine.rs:21448, state_machine.rs:21465 | split-brain winner (bring-up): cannot read this host's guest domains; holding |
| `winner_path_exposure` | WARN | state_machine.rs:21858 | split-brain winner: the local storage path was not confirmed active during the episode; guests here may hav... |
| `winner_resume_io` | WARN | state_machine.rs:10736 | split-brain: the peer accepted the discard; resuming this kept copy's frozen I/O |
| `xha_orphan_reconcile` | WARN | state_machine.rs:18104 | pool HA reads off but the local xhad is still present; pool-ha-enable would fail with DAEMON_IS_PRESENT (Xh... |
| `xha_orphan_reconcile_fast` | WARN | state_machine.rs:17927 | pool-ha-enable was refused DAEMON_IS_PRESENT (Xha_error 15); stopping the local xhad and re-enabling immedi... |
| `xha_orphan_stop_failed` | WARN | state_machine.rs:18002 | ha_stop_daemon did not reap the local xhad; not force-killing it (a SIGKILL skips the watchdog disarm and w... |
| `yield` | WARN | state_machine.rs:20682 | dual-master auto-heal: this node is the freshly-rebooted, VM-less spurious master; restarting the local too... |
| `yield_deferred_storage_attach` | INFO | state_machine.rs:9463, state_machine.rs:9750 | holding the toolstack yield: an iSCSI/PBD attach is in flight, a restart would wipe it (collapse timer pres... |
| `yield_fence` | WARN | state_machine.rs:20124 | dual-master wedge: restart budget exhausted and the survivor's escalation window has passed; self-fencing t... |
| `yield_hold` | WARN | state_machine.rs:15804 | yielded and frozen while the gateway is healthy and the peer stays unreachable: this needs an operator deci... |
| `yield_latch_promote_hold` | WARN | state_machine.rs:18730 | not promoting: this node is latched as the loser of the current split-brain fork (fence-peer yield); waitin... |
| `yield_latched` | INFO | state_machine.rs:575, state_machine.rs:18672 | fence-peer yield with no guest on the SR here; latching the loser verdict for this fork and announcing it o... |
| `yield_latched` step=`pending_relatch_dropped` | INFO | state_machine.rs:21417 | a pending yield re-latch no longer applies to this copy (not the frozen Primary it was recorded on); dropped |
| `yield_latched` step=`relatch_skipped` | INFO | state_machine.rs:21390 | a guest on the SR runs here (or none could be confirmed) after the resume; the yield latch is not moved ont... |
| `yield_latched` step=`relatched_after_lift` | INFO | state_machine.rs:21343 | the crash boundary's I/O lift moved this copy's generation; the yield latch follows it, so the beacon keeps... |
| `yield_loser_holding` | INFO/WARN | state_machine.rs:22026, state_machine.rs:22292 | split-brain: nobody took the discard yet; this node yielded, staying down (Secondary, target off) with the ... |
| `yield_not_frozen` | WARN | state_machine.rs:10885 | guests here may be writing on a copy the split can discard |
| `yield_with_guests` | INFO | state_machine.rs:503, state_machine.rs:18660 | this yield had guests (or live SR disks, or an unreadable state) on this host: it does not decide the split... |
| `yielded_keep` | WARN | state_machine.rs:18694 | this node yielded with guests on the SR and its copy holds more by the lineage: keeping it; its frozen I/O ... |
| `yielded_loser` | WARN | state_machine.rs:13165, state_machine.rs:18377 | init: this node's fence-peer yielded and it has not served since; resolving as the split-brain loser from b... |
