#!/usr/bin/env bash
# TWINSTOR test-bundle uninstaller.
#
# Tears down the cluster via `twinstor uninstall` (disables XHA, destroys
# the SR, stops DRBD/iSCSI, wipes the backing device and removes config, drops the
# firewall rules), then removes the pieces install.sh placed: the binary,
# the systemd unit, the logrotate config, and /etc/twinstor.
#
# Run this on BOTH hosts. Shut down or migrate every VM off the twinstor
# SR first, or `twinstor uninstall` will refuse to proceed.
#
# Flags:
#   --yes          non-interactive (skip the confirmation prompt)
#   --keep-data    preserve the backing device (do not wipe data)
#   --force        proceed even if the VM-safety check cannot confirm the SR
#                  is unused (e.g. a node with a degraded XAPI/HA view)
#   --purge-deps   also yum-remove the twinstor-specific dependency RPMs
#                  (DRBD + targetcli stack) for a fully clean host
set -uo pipefail

if [ "$(id -u)" -ne 0 ]; then
  echo "must run as root (try: sudo ./uninstall.sh)" >&2
  exit 1
fi

PURGE_DEPS=0
PASS_ARGS=()
for arg in "$@"; do
  case "$arg" in
    --purge-deps) PURGE_DEPS=1 ;;
    *) PASS_ARGS+=("$arg") ;;
  esac
done

if [ -x /usr/local/bin/twinstor ]; then
  echo "==> running twinstor uninstall (XHA, SR, DRBD, iSCSI, firewall teardown)"
  # If the cluster teardown does not complete, the SR/DRBD/HA are still up, so
  # we must NOT go on to delete the binary and config: doing so strands a live
  # cluster with no tool to manage or tear it down (and the safety guard's
  # refusal would be silently undone). Stop and let the operator fix the cause.
  if ! twinstor uninstall "${PASS_ARGS[@]}"; then
    echo >&2
    echo "twinstor uninstall did not complete, so the cluster is still up. NOT removing" >&2
    echo "the binary or config -- fix the cause and re-run. The usual cause is VMs still" >&2
    echo "on the twinstor SR (the safety guard refuses while they exist): shut them down" >&2
    echo "or migrate them off, then re-run. To tear down a node whose XAPI/HA view is" >&2
    echo "degraded and you are certain the SR is unused, re-run with --force." >&2
    exit 1
  fi
else
  echo "==> /usr/local/bin/twinstor not found; skipping cluster teardown"
  # Guard the same footgun from the other direction: a missing binary plus a
  # still-configured SR means this script cannot tear the cluster down, so
  # removing files would again strand it. Only clean up when no SR remains.
  if command -v xe >/dev/null 2>&1 \
     && [ -n "$(xe sr-list name-label=twinstor params=uuid --minimal 2>/dev/null)" ]; then
    echo >&2
    echo "A twinstor SR still exists but the binary is gone, so this script cannot tear" >&2
    echo "the cluster (SR/DRBD/HA) down. Reinstall the binary (./install.sh) and run" >&2
    echo "'twinstor uninstall' first; refusing to delete files and strand a live cluster." >&2
    exit 1
  fi
fi

echo "==> removing binary, systemd unit, and leftover config"
rm -f /usr/local/bin/twinstor
systemctl disable --now twinstor-reboot-drain.service 2>/dev/null || true
rm -f /usr/lib/systemd/system/twinstor.service
rm -f /usr/lib/systemd/system/twinstor-reboot-drain.service
rm -f /etc/logrotate.d/twinstor
rm -rf /etc/twinstor
systemctl daemon-reload 2>/dev/null || true

if [ "$PURGE_DEPS" -eq 1 ]; then
  echo "==> purging twinstor-specific dependency packages"
  # Only the clearly-twinstor packages. Shared libraries pulled in as
  # transitive deps (libnl, gobject-introspection, pyparsing, ...) are
  # left in place: removing them risks breaking unrelated system tools.
  yum remove -y \
    kmod-drbd drbd-utils targetcli \
    python-rtslib python-configshell python-urwid python-kmod \
    || echo "    some packages were not installed; ignoring"
fi

echo
echo "twinstor removed from this host. Run this on the peer node as well."
