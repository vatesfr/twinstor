#!/usr/bin/env bash
# TWINSTOR test-bundle installer.
#
# Runs preflight checks, installs the runtime dependency RPMs bundled in
# deps/, places the static binary and the systemd unit, then verifies the
# DRBD kernel module loads for the running kernel. It does NOT configure
# anything: once this finishes, run `twinstor setup`.
#
# Run this on BOTH hosts.
#
# Environment:
#   TWINSTOR_SKIP_PREFLIGHT=1   skip the preflight gate (not recommended)
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

ok()   { printf '  [ ok ] %s\n' "$1"; }
warn() { printf '  [warn] %s\n' "$1" >&2; }
fail() { printf '  [FAIL] %s\n' "$1" >&2; }

if [ "$(id -u)" -ne 0 ]; then
  echo "must run as root (try: sudo ./install.sh)" >&2
  exit 1
fi

# Detect an update (binary already present) BEFORE we overwrite it, so the
# closing message can tell the operator to restart the daemon rather than
# print the fresh-install "run setup" guidance. install.sh stages the new
# binary but deliberately does not restart a running daemon.
was_installed=0
if [ -x /usr/local/bin/twinstor ]; then
  was_installed=1
fi

# ---------------------------------------------------------------------------
# Preflight: catch the environment problems that otherwise surface much later
# as a confusing `twinstor setup` failure.
# ---------------------------------------------------------------------------
echo "==> preflight checks"
errors=0

# Architecture: the bundled binary and RPMs are x86_64 only.
if [ "$(uname -m)" = "x86_64" ]; then
  ok "architecture x86_64"
else
  fail "architecture is $(uname -m), this bundle is x86_64 only"
  errors=$((errors + 1))
fi

# XCP-ng dom0: twinstor drives XAPI, so `xe` must be present.
if command -v xe >/dev/null 2>&1; then
  ok "XAPI present (xe)"
else
  fail "xe not found: twinstor must run on an XCP-ng host (dom0)"
  errors=$((errors + 1))
fi

# Version: the bundled dependencies target XCP-ng 8.3 (el7). Other versions
# are a warning, not a hard stop, in case you know what you are doing.
xcp_ver="$( ( . /etc/os-release 2>/dev/null && echo "${VERSION_ID:-}" ) || true )"
case "$xcp_ver" in
  8.3*) ok "XCP-ng $xcp_ver" ;;
  "")   warn "could not detect the XCP-ng version (expected 8.3)" ;;
  *)    warn "detected version $xcp_ver; bundled deps target XCP-ng 8.3 (el7)" ;;
esac

# Up to date: a pending kernel/xen/xcp-ng update means the host should be
# patched and rebooted first, otherwise the DRBD module built for the
# updated kernel will not match the running one.
if command -v yum >/dev/null 2>&1; then
  rc=0
  updates="$(yum check-update -q 2>/dev/null)" || rc=$?
  if [ "$rc" -eq 100 ]; then
    if printf '%s\n' "$updates" | grep -qiE '^(kernel|kmod|xen|xcp-ng|microcode)'; then
      warn "core updates pending (kernel/xen/xcp-ng): patch and reboot before installing"
    else
      warn "package updates are available; consider 'yum update' then reboot"
    fi
  elif [ "$rc" -eq 0 ]; then
    ok "host is up to date"
  else
    warn "could not check for updates (no network or repo issue); skipping"
  fi
else
  warn "yum not found; skipping the up-to-date check"
fi

if [ "$errors" -gt 0 ]; then
  if [ "${TWINSTOR_SKIP_PREFLIGHT:-0}" = "1" ]; then
    warn "preflight found $errors problem(s); continuing because TWINSTOR_SKIP_PREFLIGHT=1"
  else
    echo
    echo "preflight failed with $errors problem(s). Fix them, or re-run with" >&2
    echo "TWINSTOR_SKIP_PREFLIGHT=1 to override." >&2
    exit 1
  fi
fi

# ---------------------------------------------------------------------------
# Install
# ---------------------------------------------------------------------------
echo "==> installing runtime dependencies (DRBD module, drbd-utils, targetcli, ...)"
if ls "$DIR"/deps/*.rpm >/dev/null 2>&1; then
  # The kmod is xcp-ng-signed and the el7 packages carry their own keys;
  # fall back to --nogpgcheck so a missing key on a fresh host is not a
  # blocker for early testing.
  #
  # yum exits non-zero with "Nothing to do" when every bundled RPM is already
  # installed, which is exactly the re-run / in-place-update case. Tolerate
  # that here (otherwise `set -e` aborts the update before the binary is
  # replaced): a genuinely missing or broken dependency is still caught by the
  # verify step below (modprobe drbd + command checks).
  dep_out="$(yum install -y "$DIR"/deps/*.rpm 2>&1 \
    || yum install -y --nogpgcheck "$DIR"/deps/*.rpm 2>&1)" || true
  printf '%s\n' "$dep_out"
  if printf '%s' "$dep_out" | grep -q 'Nothing to do'; then
    ok "runtime dependencies already present"
  fi
else
  echo "    no bundled deps/ found, assuming dependencies are already installed"
fi

echo "==> installing binary to /usr/local/bin/twinstor"
install -m 0755 "$DIR/twinstor" /usr/local/bin/twinstor

echo "==> installing systemd unit"
install -m 0644 "$DIR/systemd/twinstor.service" /usr/lib/systemd/system/twinstor.service
# A unit file in /etc/systemd/system takes precedence over the one installed
# here, so a copy left there by hand silently keeps old settings (stop timeout,
# dependencies) across upgrades. Point it out; never delete it.
for u in twinstor.service twinstor-reboot-drain.service; do
  if [ -f "/etc/systemd/system/$u" ]; then
    warn "/etc/systemd/system/$u overrides the unit this release installs; if you did not customize it on purpose, remove it and run: systemctl daemon-reload"
  fi
done
# Reboot-safety backstop: a oneshot whose ExecStop, during shutdown, holds this
# host's reboot while it is the last UpToDate replica and the peer is resyncing.
# It must be ENABLED and STARTED (unlike the daemon, which this script deliberately
# does not touch) because RemainAfterExit only arms the ExecStop while the unit is
# active. Idempotent, so re-running install.sh on an upgrade picks it up.
install -m 0644 "$DIR/systemd/twinstor-reboot-drain.service" /usr/lib/systemd/system/twinstor-reboot-drain.service
systemctl daemon-reload
# `enable` + `start` as two steps: `enable --now` is a systemd v220 flag and the
# dom0 baseline is 219 (the RHEL/CentOS-7 backport usually has it, but do not
# depend on it). The unit must be BOTH enabled (armed on future boots) and started
# now (RemainAfterExit arms the ExecStop only while active).
if systemctl enable twinstor-reboot-drain.service 2>/dev/null \
   && systemctl start twinstor-reboot-drain.service 2>/dev/null; then
  ok "reboot-safety drain armed (twinstor-reboot-drain.service)"
else
  echo "    WARNING: could not enable/start twinstor-reboot-drain.service; reboots" >&2
  echo "    will NOT be held during a resync until it is armed manually:" >&2
  echo "        systemctl enable twinstor-reboot-drain.service" >&2
  echo "        systemctl start  twinstor-reboot-drain.service" >&2
fi

# ---------------------------------------------------------------------------
# Verify: the most common real failure is a DRBD module that does not match
# the running kernel (host updated but not rebooted). Catch it now, not at
# `twinstor setup` time.
# ---------------------------------------------------------------------------
echo "==> verifying installation"
verify_failed=0

if modprobe drbd 2>/dev/null; then
  ok "DRBD module loaded (kernel $(uname -r))"
else
  fail "DRBD module failed to load for kernel $(uname -r)"
  echo "    The kmod-drbd build does not match the running kernel. The host is" >&2
  echo "    most likely updated but not rebooted (or not fully patched). Run:" >&2
  echo "        yum update && reboot" >&2
  echo "    then re-run ./install.sh." >&2
  verify_failed=1
fi

for c in drbdadm targetcli multipath iscsiadm lvcreate; do
  if command -v "$c" >/dev/null 2>&1; then
    ok "$c present"
  else
    fail "$c not found after install"
    verify_failed=1
  fi
done

if [ "$verify_failed" -ne 0 ]; then
  echo
  echo "installation verification failed; resolve the items above before setup." >&2
  exit 1
fi

echo
echo "installed version: $(/usr/local/bin/twinstor --version 2>/dev/null || echo 'unknown (pre-0.1.6 build, no --version flag)')"
echo
if [ "$was_installed" -eq 1 ]; then
  echo "twinstor updated and verified on this host."
  echo
  echo "The new binary is in place but the running daemon is still the old one."
  echo "Activate it with the health-gated rolling restart (do ONE host at a time;"
  echo "it refuses unless replication is healthy and blocks until this node"
  echo "reconverges, so it is safe to run then move to the peer):"
  echo
  echo "    twinstor upgrade-apply"
  echo
  echo "On restart the daemon applies this version's DRBD split-brain safety"
  echo "policies LIVE automatically, regardless of the on-disk .res version -- so"
  echo "if 'twinstor status' later shows the config as outdated, that is EXPECTED"
  echo "and safe, not something to rush."
  echo
  echo "Refreshing the on-disk DRBD template to this version is OPTIONAL (it only"
  echo "clears that status line and closes a small boot-window gap). On a connected,"
  echo "serving pool 'twinstor regenerate-config' cannot reset the peer connection to"
  echo "apply it live and will defer safely. Adopt it in a maintenance window and"
  echo "FOLLOW WHAT 'regenerate-config' PRINTS -- it detects which case you are in:"
  echo
  echo "    twinstor regenerate-config --persist-only --yes   # stage the new template"
  echo
  echo "  * Normal refresh: reboot THIS node, wait for 'twinstor status' Connected +"
  echo "    both UpToDate, THEN do the other node -- one node at a time."
  echo "  * Connection-topology change (rare, only very old pools): the reconfigured"
  echo "    node will NOT reconnect to an un-reconfigured peer, so one-at-a-time"
  echo "    SPLITS the pool. Stage --persist-only on BOTH nodes first, then reboot"
  echo "    both together in the same window. 'regenerate-config' detects this and"
  echo "    refuses the one-node path, printing the coordinated procedure -- follow"
  echo "    it; do not improvise a rolling reboot."
else
  echo "twinstor installed and verified on this host."
  echo
  echo "Next, set up the cluster with the interactive wizard. Run it on the"
  echo "FIRST host, then the SECOND; it detects the peer, picks this host's"
  echo "backing disk, and starts the daemon for you:"
  echo
  echo "    twinstor setup"
  echo
  echo "The wizard is the recommended path. For non-interactive/advanced use"
  echo "(flags such as --peer-ip, --backing-device, --chap-password), see:"
  echo
  echo "    twinstor setup --help"
fi
