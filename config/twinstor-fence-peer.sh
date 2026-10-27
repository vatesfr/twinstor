#!/bin/bash
#
# DRBD fence-peer handler for TWINSTOR.
#
# Runs on the node that just lost its DRBD peer. In arbitrated mode
# (`fencing resource-and-stonith`) DRBD has already SUSPENDED I/O before
# invoking this handler, so nothing diverges while we decide. In fate-shared
# mode (`fencing resource-only`) I/O keeps flowing while the handler runs: a
# node that ends up yielding has served writes for those seconds (the ping
# timeouts), and they are discarded when it resyncs from the peer. The
# decision is one question: can I still serve clients, i.e. reach the network?
#
#   Any ping target reachable:   exit 4 (peer assumed outdated; DRBD resumes
#                                me as the sole Primary, I serve clients).
#   No target reachable:         exit 1 (I cannot serve clients; DRBD keeps my
#                                I/O frozen, I yield). If a peer is alive on the
#                                direct link it becomes the sole server and XHA
#                                then fences me so my VMs restart on it. If the
#                                peer is gone too I am genuinely isolated and
#                                stay frozen: a clean outage, never corruption.
#
# Ping targets (in order):
#   1. Default gateway (auto-detected from the routing table)
#   2. /etc/twinstor/ping-targets (managed-switch IP, second router, ...)
#
# Arbitrated mode only: the peer's direct-link IP in /etc/twinstor/direct-peer
# is probed to LABEL the outcome (yield-to-live-peer vs isolated). It does not
# change the survive/yield decision, which is gateway reachability, because
# that is what actually proves we can still serve clients. A node that kept the
# direct link but lost its switch still cannot serve clients and must yield.
#
# "Any of N" reachability is the same model vSphere HA uses: one reachable
# target means we are not isolated.

# The TWINSTOR_* overrides exist for the handler's tests; DRBD never sets them.
export PATH="${TWINSTOR_SHIM_PATH:+$TWINSTOR_SHIM_PATH:}/usr/bin:/usr/sbin:/bin:/sbin:$PATH"
RESOURCE="${DRBD_RESOURCE:-twinstor-data}"
ETC="${TWINSTOR_ETC_DIR:-/etc/twinstor}"
RUN="${TWINSTOR_RUN_DIR:-/run/twinstor}"
LOG="${TWINSTOR_FENCE_LOG:-/var/log/twinstor-fence-peer.log}"
YIELD_MARKER="$RUN/fence-yielded"
FS_FREEZE_FLAG="$RUN/fs-freeze"

# Every line names its run ([pid]): DRBD can start a second run while one is
# still deciding, and their lines interleave. Timestamps carry the offset, to
# line up with the daemon's log.
log() {
    echo "$(date '+%Y-%m-%dT%H:%M:%S%z') [$$] $1" >> "$LOG"
    logger -t twinstor "fence-peer[$$]: $1"
}

# Every exit goes through here: one line naming the verdict (a stable id),
# and the same appended to $RUN/fence-verdict, whose new lines the daemon
# logs (cleared with /run at reboot). The line before it says why.
# A survive verdict lifts an entry freeze (below), from the newest run only:
# DRBD ignores an older run's exit code, and the newer run decides for it.
entry_freeze_release() {
    [ -f "$ENTRY_FREEZE" ] || return 0
    if [ "$(cat "$RUN/fence-peer.newest" 2>/dev/null)" != "$$" ]; then
        log "a newer fence-peer run decides this disconnect: the entry freeze stays for it"
        return 0
    fi
    local pid
    timeout 10 drbdadm resume-io "$RESOURCE" >/dev/null 2>&1 9>&- &
    pid=$!
    for _ in $(seq 1 20); do
        sleep 0.5
        user_suspended || break
        kill -0 "$pid" 2>/dev/null || { wait "$pid"; user_suspended || break; }
    done
    if user_suspended; then
        log "WARNING: could not lift the entry freeze (drbdadm resume-io): this node stays frozen until the daemon lifts it"
    else
        rm -f "$ENTRY_FREEZE" 2>/dev/null
        log "entry freeze lifted for the survive verdict"
    fi
}

verdict() {
    [ "$2" = 4 ] && entry_freeze_release
    log "verdict=$1 exit=$2 role=${LOCAL_ROLE:-unread} freeze=$FREEZE elapsed=$(( $(date +%s) - ${START_EPOCH:-$(date +%s)} ))s"
    mkdir -p "$RUN" 2>/dev/null
    echo "$(date +%s) $$ $1 $2" >> "$RUN/fence-verdict" 2>/dev/null
    exit "$2"
}

# Collect ping targets: default gateway first, then operator-supplied.
TARGETS=""
GATEWAY=$(ip route show default 2>/dev/null | awk '/default/{print $3; exit}')
[ -n "$GATEWAY" ] && TARGETS="$GATEWAY"

if [ -f "$ETC/ping-targets" ]; then
    while IFS= read -r ip; do
        ip=$(echo "$ip" | sed 's/#.*//' | tr -d ' ')
        [ -n "$ip" ] && TARGETS="$TARGETS $ip"
    done < "$ETC/ping-targets"
fi

# Arbitrated mode: peer's direct-link IP (for labelling the outcome only).
DIRECT_PEER=""
if [ -f "$ETC/direct-peer" ]; then
    DIRECT_PEER=$(sed 's/#.*//' "$ETC/direct-peer" | tr -d ' ' | grep -m1 .)
fi

# Fate-shared freeze mode, read once: the daemon keeps the flag (its PID)
# exactly while this node's fencing freezes I/O at a disconnect, never on
# arbitrated. Only a live daemon counts: a freeze-only yield must have a
# daemon here to resolve it (a stopped daemon leaves the policy armed).
FREEZE=0
BRIEFLY_CUT=0
if [ -z "$DIRECT_PEER" ] && [ -e "$FS_FREEZE_FLAG" ]; then
    FLAG_PID=$(tr -dc '0-9' < "$FS_FREEZE_FLAG" 2>/dev/null)
    # The daemon itself, not a reused PID: the installed binary running
    # `daemon` (the CLI runs the same binary). An upgrade replaces the file
    # before it restarts the daemon, and the kernel then names the running
    # one "<path> (deleted)": still the daemon, still in freeze mode (without
    # it, the handler took the keeper path between install and restart).
    FLAG_EXE=$(readlink "/proc/${FLAG_PID:-0}/exe" 2>/dev/null)
    if [ -n "$FLAG_EXE" ] && [ "${FLAG_EXE% (deleted)}" = "${TWINSTOR_DAEMON_EXE:-/usr/local/bin/twinstor}" ] \
        && tr '\0' '\n' < "/proc/$FLAG_PID/cmdline" 2>/dev/null | grep -qx daemon; then
        FREEZE=1
    fi
fi

# In freeze mode the yield marker is dated from here: the daemon counts the
# freeze budget from it, and the I/O froze when this run started, not when
# its probes and waits ended.
START_EPOCH=$(date +%s)
log "started: targets=[$TARGETS] direct_peer=[${DIRECT_PEER:-none}] resource=[$RESOURCE] freeze=[$FREEZE]"

# Promote-time guard: when this node is still Secondary, this handler runs
# because a promote wants the unreachable peer marked outdated. If the
# peer's DRBD listener is in fact accepting on a replication address, the
# partition has already healed under us (a link flap, or our own link coming
# back mid-attempt): outdating it now would promote a stale copy alongside a
# live Primary and manufacture a dual-master split-brain the auto-heal
# ladder then has to unwind. Refuse instead (exit 1): the promote fails
# cleanly, DRBD connects within a couple of connect-int rounds, and the node
# rejoins as a plain sync target. The probe is the DRBD PORT, not ICMP, on
# purpose: a host that pings but whose storage stack is down (dead daemon,
# missing kmod) must not block a lone survivor forever; its closed port lets
# the normal decision below proceed. Serve-time fencing (role Primary, I/O
# suspended) keeps the gateway-only DECISION (refusing there would freeze a
# healthy survivor against a gray-failure peer); when the peer's port
# answers it only DELAYS the verdict briefly so a healed flap reconnects
# instead of diverging (see the flap-defusal block below).
# Every drbdadm/drbdsetup call is bounded: DRBD's netlink can wedge, and an
# unbounded call would hold the handler's lock (and the node's frozen I/O)
# past every other bound here.
LOCAL_ROLE=$(timeout 10 drbdadm role "$RESOURCE" 2>/dev/null | head -1)
PEER_ADDR=$(sed -n 's/^peer_address *= *"\(.*\)".*/\1/p' "$ETC/twinstor.toml" 2>/dev/null | head -1)
if [ -n "${FLAG_PID:-}" ]; then
    FLAG_STATE="pid=$FLAG_PID exe=${FLAG_EXE:-gone} live=$FREEZE"
elif [ -z "$DIRECT_PEER" ] && [ -e "$FS_FREEZE_FLAG" ]; then
    FLAG_STATE="present-without-pid"
elif [ -n "$DIRECT_PEER" ]; then
    FLAG_STATE="arbitrated"
else
    FLAG_STATE="absent"
fi
log "context: role=[${LOCAL_ROLE:-unreadable}] peer_address=[${PEER_ADDR:-none}] yield_marker=[$( [ -f "$YIELD_MARKER" ] && cat "$YIELD_MARKER" 2>/dev/null || echo absent)] freeze_flag=[$FLAG_STATE] drbd_peer_node=[${DRBD_PEER_NODE_ID:-?}]"
DRBD_PORT=$(sed -n 's/^drbd_port *= *\([0-9]*\).*/\1/p' "$ETC/twinstor.toml" 2>/dev/null | head -1)
[ -n "$DRBD_PORT" ] || DRBD_PORT=7789

user_suspended() {
    local tok
    tok=$(timeout 3 drbdadm status "$RESOURCE" 2>/dev/null | head -1 | grep -o 'suspended:[^ ]*' | head -1)
    case ",${tok#suspended:}," in *,user,*) return 0;; esac
    return 1
}
suspend_io_live() {
    local pid i
    # fd 9 (the handler lock) closed for the child: it can outlive this run
    # by up to its timeout, and must not hold the next run off meanwhile.
    timeout 10 drbdadm suspend-io "$RESOURCE" >/dev/null 2>&1 9>&- &
    pid=$!
    for i in $(seq 1 20); do
        sleep 0.5
        user_suspended && return 0
        if ! kill -0 "$pid" 2>/dev/null; then
            wait "$pid"
            local rc=$?
            [ "$rc" = 0 ] && return 0
            user_suspended && return 0
            return "$rc"
        fi
    done
    wait "$pid"
    user_suspended && return 0
    return 124
}
# DRBD's own freeze can be missing. When a connection that came back while a
# run was still deciding drops again, DRBD lifts the fencing suspend
# ("susp-io( fencing -> no ) [after-fencing]"), ignores the pending run's exit
# code and starts the next run with this node's I/O flowing. Seen on a
# fate-shared test pool under 40% packet loss: both nodes served unfrozen, one
# for 16 s, and the loser's acknowledged writes were discarded. Where this
# handler relies on that freeze (arbitrated, fate-shared freeze mode), a
# Primary holding a copy that finds its I/O not suspended while another run
# is deciding, or started within the flap window, freezes it here, before
# waiting or probing.
# A survive verdict of the newest run lifts it (`verdict`); every other exit
# leaves it in place, like DRBD's own freeze (the daemon lifts a leftover
# suspend once the pair is Connected again).
ENTRY_FREEZE="$RUN/fence-entry-freeze"
ENTRY_FROZE=0
mkdir -p "$RUN" 2>/dev/null
# The previous run started within the flap window: it may still be deciding,
# or have exited a moment before this run's check below.
PREV_RUN_AT=$(stat -c %Y "$RUN/fence-peer.newest" 2>/dev/null || echo 0)
RECENT_RUN=0
[ $(( START_EPOCH - PREV_RUN_AT )) -lt "${TWINSTOR_FLAP_WINDOW:-120}" ] && RECENT_RUN=1
echo "$$" > "$RUN/fence-peer.newest" 2>/dev/null
# 0 suspended, 1 not suspended, 2 unreadable.
io_suspended() {
    local out tok
    out=$(timeout 3 drbdadm status "$RESOURCE" 2>/dev/null | head -1)
    [ -n "$out" ] || return 2
    tok=$(echo "$out" | grep -o 'suspended:[^ ]*' | head -1)
    case "${tok#suspended:}" in ""|no) return 1 ;; esac
    return 0
}
# An entry freeze is in force only while a user suspend is: otherwise the
# record is left over from an earlier episode.
[ -f "$ENTRY_FREEZE" ] && ! user_suspended && rm -f "$ENTRY_FREEZE" 2>/dev/null
other_run_deciding() {
    command -v flock >/dev/null 2>&1 || return 1
    (exec 8>>"$RUN/fence-peer.lock" && ! flock -n 8) 2>/dev/null
}
if { [ "$FREEZE" = 1 ] || [ -n "$DIRECT_PEER" ]; } && [ "$LOCAL_ROLE" = "Primary" ] \
    && { [ "$RECENT_RUN" = 1 ] || other_run_deciding; }; then
    io_suspended
    if [ $? = 1 ] && [ "$(timeout 10 drbdadm dstate "$RESOURCE" 2>/dev/null | head -1 | cut -d/ -f1)" != "Diskless" ]; then
        if suspend_io_live; then
            ENTRY_FROZE=1
            echo "$$" > "$ENTRY_FREEZE" 2>/dev/null
            log "this node's I/O was not frozen at this disconnect (DRBD lifted its fencing suspend while another run was deciding): frozen now, before deciding"
        else
            log "WARNING: this node's I/O was not frozen at this disconnect and suspend-io failed: it keeps flowing while this run decides"
        fi
    fi
fi

# Fate-shared mode (no direct link): this node's copy keeps taking writes
# while this handler decides (resource-only fencing), so from here on it may
# hold acknowledged writes the peer's copy lacks. Its guests must never reach
# the peer's copy through the peer's iSCSI portal: once the local path stalls
# (a yield freezes the local I/O) multipath fails over to it the moment the
# link returns, and writes made after these would land on the peer's copy
# without them, which corrupts whichever copy wins (seen in testing:
# ext4 metadata from two points in time). Fence that path now: a firewall
# rule rejects this node's traffic to the peer's portal, so the iSCSI
# session cannot recover when the link returns, and its disks are set
# offline so multipath fails the path at once. The daemon removes the rule
# once the pair is Connected (or this node is Secondary): the SAME session
# then recovers and multipath reinstates the same path, with no new login
# and no map reload. Arbitrated mode freezes I/O at the disconnect, so a
# failover there carries no such gap.
# Not on a Diskless node (its local disk failed): it holds no copy, its
# guests' writes can only land on the peer's, and the peer's portal is the one
# path they have left once this node's target stops.
PORTAL_PORT=$(sed -n 's/^portal_port *= *\([0-9]*\).*/\1/p' "$ETC/twinstor.toml" 2>/dev/null | head -1)
[ -n "$PORTAL_PORT" ] || PORTAL_PORT=3260
LOCAL_DISK=$(timeout 10 drbdadm dstate "$RESOURCE" 2>/dev/null | head -1 | cut -d/ -f1)
if [ -z "$DIRECT_PEER" ] && [ "$LOCAL_ROLE" = "Primary" ] && [ "$LOCAL_DISK" = "Diskless" ]; then
    log "fate-shared: this node is Diskless (its local disk failed): no copy here to diverge, the peer's iSCSI portal stays its guests' path (no fence)"
elif [ -z "$DIRECT_PEER" ] && [ "$LOCAL_ROLE" = "Primary" ] && [ -n "$PEER_ADDR" ]; then
    FENCE_RULE="-d $PEER_ADDR/32 -p tcp -m tcp --dport $PORTAL_PORT -m comment --comment twinstor-peer-portal-fence -j REJECT --reject-with tcp-reset"
    if timeout 5 iptables -w 3 -C OUTPUT $FENCE_RULE 2>/dev/null; then
        RULE_STATE="present"
    elif timeout 5 iptables -w 3 -I OUTPUT 1 $FENCE_RULE 2>/dev/null; then
        RULE_STATE="inserted"
    else
        RULE_STATE="INSERT-FAILED rc=$?"
    fi
    PEER_DISKS=$(timeout 5 iscsiadm -m session -P 3 2>/dev/null | awk -v p="$PEER_ADDR:$PORTAL_PORT" '
        /Current Portal:/ { cur = ($3 ~ "^" p) }
        cur && /Attached scsi disk/ { print $4 }')
    OFFLINE_FAILED=""
    for d in $PEER_DISKS; do
        echo offline > "/sys/block/$d/device/state" 2>/dev/null || OFFLINE_FAILED="$OFFLINE_FAILED $d"
    done
    log "fate-shared: path through the peer's iSCSI portal $PEER_ADDR:$PORTAL_PORT fenced (firewall rule: $RULE_STATE; disks offline: ${PEER_DISKS:-none}; offline failed:${OFFLINE_FAILED:- none}); this copy may now diverge from the peer's"
    # The daemon lifts the fence (and removes this) once this node may use
    # the peer's copy again: Secondary, or Connected.
    mkdir -p "$RUN" 2>/dev/null; : > "$RUN/peer-portal-fenced"
elif [ -z "$DIRECT_PEER" ] && [ "$LOCAL_ROLE" = "Primary" ]; then
    log "fate-shared: peer-portal fence NOT applied: no peer_address in $ETC/twinstor.toml"
fi

# Yield: freeze this node's I/O explicitly (also correct under resource-only)
# and record the yield for the daemon. Both results are checked: a yield whose
# suspend failed does NOT freeze the I/O, which the log must say.
# The live state decides, not the exit code: from inside this handler the
# command applies the suspend within seconds but may not return before its
# timeout (and a second call then fails on the suspend already in place), so
# the yield was logged as failed and recorded ~10 s late.
suspend_and_mark() {
    suspend_io_live
    SUSP_RC=$?
    if [ "$SUSP_RC" != 0 ]; then
        sleep 1
        suspend_io_live
        SUSP_RC=$?
    fi
    mkdir -p "$RUN" 2>/dev/null
    date '+%Y-%m-%d %H:%M:%S' > "$YIELD_MARKER" 2>/dev/null
    [ "$FREEZE" = 1 ] && touch -d "@$START_EPOCH" "$YIELD_MARKER" 2>/dev/null
    [ "$SUSP_RC" = 0 ] || log "WARNING: drbdadm suspend-io failed (rc=$SUSP_RC, retried once): this yield's own suspend did not take (under resource-only the I/O keeps flowing; under resource-and-stonith only the fencing suspend holds it)"
    [ -f "$YIELD_MARKER" ] || log "WARNING: the yield marker could not be written ($YIELD_MARKER): the daemon cannot see this yield"
}

peer_drbd_listening() {
    for p in $PEER_ADDR $DIRECT_PEER; do
        if timeout 2 bash -c "exec 3<>/dev/tcp/$p/$DRBD_PORT" 2>/dev/null; then
            return 0
        fi
    done
    return 1
}

# One verdict at a time. DRBD can start a second fence-peer for the resource
# while the first is still deciding: a direct-link loss reconnects over the
# switched path, the handshake is refused (this Primary would have to become
# the sync target), the connection closes again and the handler runs again.
# The second run then finds the peer's listener closed (the peer went
# StandAlone too), reaches the gateway and resumes I/O with exit 4 while the
# first run is about to yield: both nodes write, a true fork (seen on the
# an arbitrated test pool with a 60 s direct-link drop). So runs queue on a lock,
# and a run that finds a yield still in force keeps it instead of deciding
# again (below). The kernel drops a flock when its holder exits or dies, so a
# crashed run leaves no stale lock, and the lock file lives under /run. The
# wait is bounded: a run that cannot get the lock in time stays frozen
# without outdating anyone (exit 1, no yield marker), never a second writer.
# The bound covers a normal run (yield path ~50 s with the lossy-link
# windows, plus up to 6 s per ping target, each drbdadm call bounded at
# 10 s); a run delayed past it by wedged DRBD calls makes a waiting run
# freeze, which is the safe side.
LOCK_WAIT="${TWINSTOR_FENCE_LOCK_WAIT:-90}"
mkdir -p "$RUN" 2>/dev/null
if command -v flock >/dev/null 2>&1 && exec 9>>"$RUN/fence-peer.lock"; then
    if ! flock -n 9; then
        log "another fence-peer run is still deciding; waiting for it (up to ${LOCK_WAIT}s)"
        if ! flock -w "$LOCK_WAIT" 9; then
            log "the other fence-peer run still holds the lock after ${LOCK_WAIT}s -> stay frozen, outdate nobody (exit 1, no yield marker)"
            verdict lock_timeout 1
        fi
    fi
else
    log "fence-peer lock unavailable ($RUN/fence-peer.lock); deciding unserialized"
fi

# An arbitrated mutual-freeze keep (the daemon's decision: both nodes froze
# with guests, the link cannot settle, and this node, the keeper, takes the
# split) stands for the rest of the episode, before any yield rule: a run
# that waited on the lock while the daemon resumed must not freeze this node
# again (its guests would fail over to the peer's portal). The intent holds
# the generation both copies shared at the decision; it counts only while that
# is still this node's current or bitmap generation, so one left by an earlier
# episode never matches once the pair resynced. The daemon removes it at every
# episode end (tmpfs: gone at reboot).
ARB_KEEP="${TWINSTOR_ARB_KEEP_MARKER:-$RUN/arb-mutual-keep}"
is_gen() { case "$1" in [0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f]) return 0 ;; esac; return 1; }
keep_bound_here() {
    local u gi cur bm
    u=$(head -1 "$ARB_KEEP" 2>/dev/null | tr -d ' ')
    is_gen "$u" || return 1
    gi=$(timeout 10 drbdadm get-gi "$RESOURCE" 2>/dev/null | head -1)
    cur=$(echo "$gi" | cut -d: -f1)
    bm=$(echo "$gi" | cut -d: -f2)
    # Bit 0 is DRBD's primary-at-the-time flag, not part of the generation.
    { is_gen "$cur" && [ $(( 0x$cur & ~1 )) -ne 0 ] && [ $(( 0x$cur & ~1 )) -eq $(( 0x$u & ~1 )) ]; } \
        || { is_gen "$bm" && [ $(( 0x$bm & ~1 )) -ne 0 ] && [ $(( 0x$bm & ~1 )) -eq $(( 0x$u & ~1 )) ]; }
}
if [ -n "$DIRECT_PEER" ] && [ -f "$ARB_KEEP" ] && [ "$LOCAL_ROLE" = "Primary" ] && keep_bound_here; then
    log "arbitrated mutual-freeze keep in force ($(head -1 "$ARB_KEEP" 2>/dev/null)): this node takes the split -> keep serving (exit 4)"
    verdict keep_win 4
fi

# A yield of this episode is in force when its marker exists AND the I/O
# suspend it set is still in place (the daemon only ever lifts that suspend
# once replication is healthy again, and never sets one itself). A later run
# must keep it: the yielding node served nobody, its peer is the one serving,
# and a fresh decision taken now cannot see that peer (it may be StandAlone
# with its listener closed) and would resume this node's I/O beside it. The
# daemon owns every exit from a yield. Not on a promote-time run (Secondary),
# which keeps its own guard below. Fails closed: with a marker present, an
# unreadable role or I/O state keeps the yield.
io_suspend_state() {
    local out tok
    out=$(timeout 10 drbdadm status "$RESOURCE" 2>/dev/null | head -1)
    if [ -z "$out" ]; then
        echo unknown
        return
    fi
    tok=$(echo "$out" | grep -o 'suspended:[^ ]*' | head -1)
    case ",${tok#suspended:}," in
        *,user,*) echo user ;;
        *) echo none ;;
    esac
}
# A run that froze this node itself at entry found no suspend in place, so any
# marker is from an earlier episode.
if [ -f "$YIELD_MARKER" ] && [ "$LOCAL_ROLE" != "Secondary" ] && [ "$ENTRY_FROZE" = 1 ]; then
    log "a yield marker is present ($(cat "$YIELD_MARKER" 2>/dev/null)) but this node's I/O was not suspended when this run began -> stale, deciding afresh"
elif [ -f "$YIELD_MARKER" ] && [ "$LOCAL_ROLE" != "Secondary" ]; then
    case "$(io_suspend_state)" in
        user)
            log "a yield of this episode is still in force (marker present, I/O suspended:user) -> keep it: stay frozen (exit 1)"
            verdict keep_yield 1
            ;;
        unknown)
            log "a yield marker is present and this node's I/O state cannot be read -> keep the yield (fail closed): stay frozen (exit 1)"
            verdict keep_yield_io_unreadable 1
            ;;
    esac
    if [ -z "$LOCAL_ROLE" ]; then
        log "a yield marker is present and this node's role cannot be read -> keep the yield (fail closed): stay frozen (exit 1)"
        verdict keep_yield_role_unreadable 1
    fi
    log "a yield marker is present ($(cat "$YIELD_MARKER" 2>/dev/null)) but this node's I/O is not suspended:user -> stale, deciding afresh"
fi


# Administrative-bounce guard. A deliberate replication bounce (the
# daemon's stuck-WFBitMap auto-clear, or a runbook maintenance bounce)
# advertises a trailing "B" token on the bouncing node's beacon line
# (port 7900). Fencing must stand down for it: outdating the bouncing peer
# turns a seconds-long administrative reconnect into a fencing incident
# (lab-proven: an unannounced admin disconnect under armed fencing
# made BOTH sides fence and manufactured a zero-data split-brain). Exit 1
# WITHOUT the yield marker: an indeterminate verdict, no outdate;
# resource-and-stonith briefly holds I/O until the reconnect lands
# (~seconds), resource-only continues untouched. An old peer daemon that
# serves no flag simply never matches — today's behavior.
BEACON_PORT=7900
RES_SHORT="${RESOURCE#twinstor-}"
peer_bounce_flagged() {
    # timeout 1, not 3: the only peer that can serve a B flag is a LIVE daemon
    # on a LAN (answers in microseconds); a dead host's blackholed SYN must not
    # add seconds of frozen I/O to every REAL fence. A successfully parsed
    # identity line WITHOUT the flag is authoritative (both addresses serve
    # the same beacon), so stop probing on the first parsed line either way.
    for p in $DIRECT_PEER $PEER_ADDR; do
        line=$(timeout 1 bash -c "exec 3<>/dev/tcp/$p/$BEACON_PORT && head -c 192 <&3" 2>/dev/null)
        case "$line" in
            *" twinstor1 "*" $RES_SHORT "*" B"*) return 0 ;;
            *" twinstor1 "*" $RES_SHORT "*)      return 1 ;;
        esac
    done
    return 1
}

# Freeze mode: a peer whose daemon answers (its beacon) while its DRBD port is
# closed is alive, not gone: after a cut that ended within moments both copies
# can sit StandAlone (an aborted handshake), and its daemon reconnects within
# a pass or two. Surviving on that closed port while the peer's run does the
# same would make two writers. Wait for its listener instead; a dead or
# stopping peer (its beacon silent) costs at most the one-second probe.
freeze_peer_reconnecting() {
    [ "$FREEZE" = 1 ] && [ -n "$PEER_ADDR" ] || return 1
    timeout 1 bash -c "exec 3<>/dev/tcp/$PEER_ADDR/$BEACON_PORT" 2>/dev/null || return 1
    log "freeze mode: the peer's daemon answers but its DRBD port is closed; waiting up to 6s for it to listen"
    for _ in 1 2 3 4 5 6; do
        sleep 1
        peer_drbd_listening && return 0
    done
    return 1
}

# Lossy links. Under heavy packet loss (not a clean cut) DRBD drops and
# re-forms the connection again and again, and one probe of the peer proves
# nothing: both nodes' runs lost their single probe, read the peer as gone and
# survived on a gateway that still answered a retried echo, and the copies
# forked (20 to 40% loss on both hosts, both topologies, test pools). The
# keeper (the lower name, the tiebreak below) still decides on one probe, as
# before. On arbitrated, where the I/O is frozen while this decides, the
# other node reads the peer as gone only after the peer stayed silent for a
# short window; an answer sends it into the tiebreak, where it yields only to
# a peer that demonstrably still serves. Silent means the peer's DRBD port
# and its iSCSI portal: after a lossy disconnect the keeper's DRBD can take
# half a minute to tear the old connection down and listen again while it
# already serves (a test pool forked on that). A peer whose target is down
# serves nobody and still reads as gone. A dead keeper costs the survivor
# this window. Not in fate-shared freeze mode, which has no keeper at the
# tiebreak: there the window made both nodes yield frozen under sustained
# loss, both reached the freeze budget, both stepped down, and nobody served
# (test pool). Under resource-only fencing this node's copy takes writes
# while it decides, so nothing waits there either.
# Read where the decision needs it (a promote-time or isolated run never does).
read_keeper() {
    MY_NAME=$(hostname)
    PEER_NAME=$(timeout 10 drbdsetup events2 "$RESOURCE" --now 2>/dev/null \
        | grep -o 'conn-name:[^ ]*' | head -1 | cut -d: -f2)
    # No name from DRBD (events2 failed): the configured peer name, the one
    # the daemon's own keeper rule reads, rather than none (which makes this
    # node the keeper, and the peer's run may read it the same way).
    [ -n "$PEER_NAME" ] || PEER_NAME=$(sed -n 's/^peer *= *"\(.*\)".*/\1/p' "$ETC/twinstor.toml" 2>/dev/null | head -1)
    IS_KEEPER=0
    { [ -z "$PEER_NAME" ] || [ "$MY_NAME" \< "$PEER_NAME" ]; } && IS_KEEPER=1
}
FROZEN_IO=0
[ -n "$DIRECT_PEER" ] && FROZEN_IO=1
PEER_SILENT_SECS="${TWINSTOR_PEER_SILENT_SECS:-10}"
# One round of the window: the peer's DRBD port and iSCSI portal on both
# addresses, all at once with 1 s each, so a dead peer costs one second a
# round (one probe after another at 2 s each made a 10 s window last 17 s,
# the survivor's I/O frozen all along) and a lossy link gets four chances.
# Prints which ports answered.
peer_probe_round() {
    local p port
    for p in $PEER_ADDR $DIRECT_PEER; do
        for port in $DRBD_PORT $PORTAL_PORT; do
            ( timeout 1 bash -c "exec 3<>/dev/tcp/$p/$port" 2>/dev/null && echo "$port" ) &
        done
    done
    wait
}
peer_listening_within_window() {
    local end answered
    end=$(( $(date +%s) + PEER_SILENT_SECS ))
    log "the peer's DRBD port did not answer one probe; not the keeper, I/O frozen: probing it and its iSCSI portal for up to ${PEER_SILENT_SECS}s before reading the peer as gone (a lossy link drops single probes)"
    while [ "$(date +%s)" -lt "$end" ]; do
        answered=$(peer_probe_round)
        case " $(echo $answered) " in
            *" $DRBD_PORT "*)
                log "the peer's DRBD port answered within the window: the peer is alive -> tiebreak"
                return 0
                ;;
            *" $PORTAL_PORT "*)
                log "the peer's iSCSI portal answered within the window: the peer serves -> tiebreak"
                return 0
                ;;
        esac
        sleep 0.2
    done
    log "the peer stayed silent for ${PEER_SILENT_SECS}s: the peer is gone"
    return 1
}

if peer_bounce_flagged; then
    log "peer advertises an administrative bounce (beacon B flag) -> standing down: exit 1, no outdate, no yield marker"
    verdict bounce_standdown 1
fi

if [ "$LOCAL_ROLE" = "Secondary" ] && peer_drbd_listening; then
    log "promote-time: peer DRBD listening on a replication address -> refusing to outdate it, let DRBD connect (exit 1)"
    verdict promote_refuse_entry 1
fi

EARLIER_DOWN=""
# Can I still serve clients? Loss-tolerant probe (3 packets) so a single
# dropped packet never triggers a needless self-fence; with
# resource-and-stonith there is no divergence to race while we check.
for target in $TARGETS; do
    # One echo answers in milliseconds when the gateway is up (every second
    # spent here is a second of frozen I/O under resource-and-stonith); two
    # more only if the first is lost.
    FIRST_OK=1
    # With the I/O frozen anyway, a few more before reading the gateway as
    # down: a keeper on a lossy link that yields here leaves both nodes
    # frozen (its peer yields to it in the tiebreak below).
    if ping -c 1 -W 1 "$target" >/dev/null 2>&1 || { FIRST_OK=0; ping -c 2 -W 1 "$target" >/dev/null 2>&1; } \
        || { [ "$FROZEN_IO" = 1 ] && ping -c 4 -i 0.5 -W 1 "$target" >/dev/null 2>&1; }; then
        # An earlier target that did not answer and answers now: the network
        # came back during this run, as when the first echo here was lost.
        if [ "$FREEZE" = 1 ] && [ "$FIRST_OK" = 1 ] && [ -n "$EARLIER_DOWN" ]; then
            for t in $EARLIER_DOWN; do
                ping -c 1 -W 1 "$t" >/dev/null 2>&1 && { FIRST_OK=0; break; }
            done
        fi
        # The network can return DURING the probes above (a replug landing
        # mid-attempt): re-ask the promote-time question at the decision
        # instant, not just at handler entry. A live peer's listener always
        # answers here; only a genuinely dead storage stack lets a Secondary
        # promote alone.
        if [ "$LOCAL_ROLE" = "Secondary" ] && peer_drbd_listening; then
            log "promote-time: $target reachable but peer DRBD is ALSO listening -> refusing to outdate it, let DRBD connect (exit 1)"
            verdict promote_refuse_decision 1
        fi
        # Freeze mode: the gateway answered only after the first probe was
        # lost, so this node's own network has just come back and it is the
        # side that was cut off. Surviving here, after the peer's own run
        # survived on its gateway, made two writers; the lineage then rolled
        # back the side with fewer guests (a 5 s cut, test pool). Yield
        # frozen instead: where the guests ran decides, and nothing
        # acknowledged here can be lost. A healthy node that merely lost one
        # echo yields for nothing worse than that decision.
        # Only on a live peer daemon (its beacon answers): a dead or still cut
        # off peer cannot decide, and yielding to it would leave no one
        # serving.
        if [ "$FREEZE" = 1 ] && [ "$FIRST_OK" = 0 ] && [ "$LOCAL_ROLE" != "Secondary" ] \
            && timeout 2 bash -c "exec 3<>/dev/tcp/$PEER_ADDR/7900" 2>/dev/null; then
            log "freeze mode: $target answered only after the first probe was lost (this node's network just came back) -> yield frozen"
            BRIEFLY_CUT=1
            break
        fi
        # Serve-time flap defusal. A sub-probe-window carrier flap kills the
        # DRBD TCP instantly, but by the time this handler runs the link is
        # often already back: both sides then reach their gateway, both
        # return exit 4, both bump their data generation at the next write,
        # and a guaranteed (if bounded) split-brain plus a VM-restart round
        # follows on EVERY such flap. The VERDICT stays exit 4 either way
        # (a gray peer, listening but unable to reconnect, must never freeze
        # a healthy survivor); but when the peer's DRBD port answers, the
        # partition has likely healed under us, so give DRBD a few seconds
        # to reconnect FIRST. If it does, the blocked writes resume through
        # replication with no solo-write generation bump on either side:
        # nothing diverges, nothing is discarded, no VMs restart. The
        # true-partition path (peer not listening) is unchanged and instant.
        [ "$LOCAL_ROLE" != "Secondary" ] && read_keeper
        if [ "$LOCAL_ROLE" != "Secondary" ] && { peer_drbd_listening || freeze_peer_reconnecting \
            || { [ "$FROZEN_IO" = 1 ] && [ "$IS_KEEPER" = 0 ] && peer_listening_within_window; }; }; then
            for _ in 1 2 3 4; do
                sleep 1
                CSTATE=$(timeout 5 drbdadm cstate "$RESOURCE" 2>/dev/null | head -1)
                case "$CSTATE" in
                    Connected|Sync*)
                        log "flap defused: peer reconnected during the fence ($CSTATE) -> exit 4 (writes resume replicated, no divergence)"
                        verdict flap_defused 4
                        ;;
                esac
            done
            # Not reconnected in 4s. Both sides exiting 4 here is the
            # both-survive race: two writers, guaranteed split-brain on
            # every direct-link loss (latent since the first cable-active
            # release; confirmed in testing). Deterministic tiebreak
            # instead: the lexicographically LOWER hostname keeps serving
            # (same keeper convention as the failback dance); the higher
            # one yields IF AND ONLY IF consumers demonstrably still have
            # the keeper's storage (its iSCSI portal answers) — a gray
            # keeper (listening but unable to serve) must never freeze a
            # healthy survivor, so on a dead portal we keep the old
            # both-survive behavior and let the resolver clean up.
            # Fate-shared freeze mode (the daemon keeps $FS_FREEZE_FLAG while
            # it runs the pair in it): no hostname keeper. A cut shorter than
            # this handler ends with the link back here, after the peer's own
            # run already survived on its gateway; the keeper surviving too
            # made two writers and the lineage rolled back the side with fewer
            # guests (15 s of acknowledged writes, test pool). Yielding frozen
            # (on a live peer portal, below) hands the decision to where the
            # guests ran, and nothing acknowledged here can be lost.
            if [ "$FREEZE" = 0 ] && [ "$IS_KEEPER" = 1 ]; then
                log "peer listening, not reconnected after 4s; tiebreak KEEPER ($MY_NAME < ${PEER_NAME:-unknown}) -> exit 4 (survive, serve clients)"
                verdict keeper 4
            fi
            # Yield candidate: consumers must have the keeper's portal. In
            # freeze mode the peer's daemon must answer instead (its beacon,
            # 7900): the give-way that ends this yield is its decision, and
            # on a fate-shared pool the portal is the very path this handler
            # just fenced, so it never answers here (a 5 s cut, test pool).
            PROBE_PORT=3260
            [ "$FREEZE" = 1 ] && PROBE_PORT=7900
            PORTAL_OK=0
            PROBE_ADDRS=$(timeout 10 drbdsetup events2 "$RESOURCE" --now 2>/dev/null \
                | grep -o 'peer:ipv4:[0-9.]*' | cut -d: -f3 | sort -u)
            # No address from DRBD (events2 failed): the configured peer address,
            # rather than reading "answers nowhere" and surviving beside it.
            [ -n "$PROBE_ADDRS" ] || PROBE_ADDRS="$PEER_ADDR"
            # One pass when this copy takes writes meanwhile (resource-only);
            # with the I/O frozen, retried over the window, so a lossy link
            # does not read a live peer as gray and make a second writer.
            PROBE_SECS=0
            [ "$FROZEN_IO" = 1 ] && PROBE_SECS=$PEER_SILENT_SECS
            PROBE_END=$(( $(date +%s) + PROBE_SECS ))
            while :; do
                for addr in $PROBE_ADDRS; do
                    if timeout 2 bash -c "exec 3<>/dev/tcp/$addr/$PROBE_PORT" 2>/dev/null; then
                        exec 3<&- 3>&- 2>/dev/null
                        PORTAL_OK=1; break
                    fi
                done
                [ "$PORTAL_OK" = 1 ] || [ "$(date +%s)" -ge "$PROBE_END" ] && break
                sleep 0.5
            done
            if [ "$PORTAL_OK" = 0 ]; then
                log "tiebreak yield candidate but the peer answers NOWHERE on port $PROBE_PORT (probed for ${PROBE_SECS}s: ${PROBE_ADDRS:-none}; gray peer) -> exit 4 (survive; legacy behavior)"
                verdict gray_peer_survive 4
            fi
            # Extended defusal while frozen: with the keeper's callout done
            # (it exits within its own 4s), the reconnect over the fallback
            # path can now land — observed live to complete during a
            # callout ("flap defused" fires in production logs).
            for _ in 1 2 3 4 5 6 7 8 9 10 11 12; do
                sleep 1
                CSTATE=$(timeout 5 drbdadm cstate "$RESOURCE" 2>/dev/null | head -1)
                case "$CSTATE" in
                    Connected|Sync*)
                        log "tiebreak: reconnected while yielding ($CSTATE) -> exit 4 (no divergence, nothing was written here)"
                        verdict tiebreak_reconnected 4
                        ;;
                esac
            done
            # Keeper serves, consumers have its portal, we never wrote:
            # yield frozen. The daemon owns every exit from here (leftover
            # -suspend resume on reconnect; stuck-suspended-Primary demote
            # if the freeze persists).
            suspend_and_mark
            log "tiebreak YIELD ($MY_NAME vs $PEER_NAME$([ "$FREEZE" = 1 ] && echo ', freeze mode'), peer portal alive): staying frozen, served nobody (exit 1, yield recorded)"
            verdict tiebreak_yield 1
        fi
        # An earlier yield marker is deliberately NOT removed here. The
        # marker means "this node froze and served nobody during the current
        # episode", and that stays true even if the network returns before
        # the split-brain gets resolved: the reconnect itself re-runs this
        # handler, and removing the marker on the exit-4 path would erase
        # the evidence seconds before the resolver reads it. Staleness is
        # guarded by the daemon instead: the marker only ever acts while the
        # yield's I/O suspend is still in place, and it is cleared once the
        # node is healthy again (or by reboot; it lives under /run).
        if [ "$LOCAL_ROLE" = "Secondary" ]; then
            log "$target reachable -> exit 4 (promote-time: the unreachable peer is marked outdated, this node may promote)"
            verdict promote_outdate 4
        fi
        log "$target reachable -> exit 4 (survive, serve clients)"
        verdict survive 4
    fi
    log "$target unreachable"
    EARLIER_DOWN="$EARLIER_DOWN $target"
done

# No target reachable: I cannot serve clients, so I must yield. Keep I/O
# suspended explicitly too, so this is also correct under `resource-only`.
# Record the yield where the daemon can read it: a node frozen since its own
# yield served nobody, so if a split-brain must later be resolved, this node
# is the loser regardless of what its (possibly stale) pool role claims. The
# marker lives under /run, so a reboot (which makes the node a clean rejoiner
# again) clears it automatically.
suspend_and_mark
if [ "$BRIEFLY_CUT" = 1 ]; then
    log "freeze mode: cut off until moments ago -> stay frozen, served nobody (exit 1, yield recorded)"
    verdict briefly_cut_yield 1
elif [ -n "$DIRECT_PEER" ] && ping -c 2 -W 1 "$DIRECT_PEER" >/dev/null 2>&1; then
    log "gateway unreachable, peer ALIVE on direct link ($DIRECT_PEER) -> yield to peer, stay frozen (exit 1, yield recorded)"
    verdict yield_to_peer 1
elif [ -z "$TARGETS" ]; then
    log "no gateway and no ping-target configured: nothing proves this node can serve -> stay frozen (exit 1, yield recorded)"
    verdict isolated_yield 1
elif [ -z "$DIRECT_PEER" ]; then
    log "gateway unreachable (fate-shared: the peer shares that network, not probed) -> isolated, stay frozen (exit 1, yield recorded)"
    verdict isolated_yield 1
else
    log "gateway unreachable, peer not reachable -> isolated, stay frozen (exit 1, yield recorded)"
    verdict isolated_yield 1
fi
