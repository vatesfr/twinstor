#!/bin/bash
#
# DRBD split-brain handler for TWINSTOR.
#
# Invoked by DRBD when it detects a true two-primary split-brain: both
# nodes wrote independently while disconnected, so neither copy is a
# superset of the other. This is the dangerous case. We must NOT silently
# discard either side's writes.
#
# This handler does not resolve anything itself. It drops a marker that the
# twinstor daemon polls, and the daemon runs the deterministic, loud
# auto-resolution (winner keeps serving, loser's VMs restart on the winner's
# data, with an explicit alert). Keeping the decision in the daemon means it
# can use pool-master state the handler does not have.

export PATH="/usr/bin:/usr/sbin:/bin:/sbin:$PATH"
RESOURCE="${DRBD_RESOURCE:-twinstor-data}"
LOG="/var/log/twinstor-fence-peer.log"
MARKER_DIR="/run/twinstor"
MARKER="$MARKER_DIR/split-brain"

log() {
    echo "$(date '+%Y-%m-%d %H:%M:%S') split-brain: $1" >> "$LOG"
    logger -t twinstor "split-brain: $1"
}

mkdir -p "$MARKER_DIR" 2>/dev/null
echo "$(date '+%Y-%m-%d %H:%M:%S') resource=$RESOURCE" > "$MARKER"
log "DETECTED on $RESOURCE - marker written, daemon will auto-resolve"

# Best-effort immediate notification. The daemon sends a richer alert once it
# resolves; this one fires the moment DRBD detects the split-brain. Wrap xe in
# a timeout: handlers run with no controlling terminal and XAPI may be slow.
HOST_UUID=$(timeout 5 xe host-list hostname="$(hostname)" params=uuid --minimal 2>/dev/null)
if [ -n "$HOST_UUID" ]; then
    timeout 5 xe message-create host-uuid="$HOST_UUID" \
        name=twinstor_split_brain priority=1 \
        body="TWINSTOR detected DRBD split-brain on $RESOURCE. Both nodes wrote independently while disconnected. Auto-resolving now: the losing node's diverged writes will be discarded and its VMs restarted on the winner. Verify any VMs that were running on the losing node." \
        >/dev/null 2>&1
fi

exit 0
