# Installing TwinStor

TwinStor is distributed as a self-contained tarball: the static binary, the
systemd units, the runtime dependency packages, and the install and uninstall
scripts. There is no RPM repository, no Rust toolchain and no build step on
the hosts.

This file ships in the bundle as `INSTALL.md`. The other documents it
mentions are in the `docs/` folder of the TwinStor repository on GitHub.

## Requirements

* Two XCP-ng 8.3 hosts in the same pool.
* A spare local disk, partition or md-RAID array on each host for the
  storage. **This device is wiped**, so do not point it at anything you want
  to keep.
* **Single-disk hosts** work too: install XCP-ng without local storage so the
  system disk keeps a large unallocated tail, then let the setup wizard carve
  a backing partition from that free space (see
  [Interactive setup](#interactive-setup)). If the free space is already held
  by a local SR you no longer want, the wizard detects it and prints the
  `xe sr-forget` / `xe sr-destroy` commands to free it. It never destroys an
  SR itself.
* Both hosts patched and rebooted before installing (the bundled DRBD module
  is built for the current XCP-ng 8.3 kernel).
* A network layout that matches one of the two modes, fate-shared (default)
  or arbitrated. Read `docs/NETWORK_TOPOLOGY_SAFETY.md` before choosing.

## Install

On each host:

```bash
curl -L <bundle-link> -o twinstor-test.tar.gz
tar xzf twinstor-test.tar.gz
cd twinstor-test-*
sudo ./install.sh
```

`install.sh`:

1. Runs preflight checks: x86_64 and `xe` present (both required), XCP-ng
   8.3 and no pending updates (warnings only). Set
   `TWINSTOR_SKIP_PREFLIGHT=1` to continue past a failed check.
2. Installs the bundled dependencies (DRBD module, drbd-utils, targetcli and
   its libraries).
3. Installs the binary at `/usr/local/bin/twinstor` and the systemd units:
   `twinstor.service`, and `twinstor-reboot-drain.service`, which holds a
   reboot while this host has the only up-to-date copy and the peer is
   resyncing.
4. Checks that the DRBD module loads for the running kernel.

It does not configure anything yet.

If `install.sh` reports that the DRBD module failed to load, the host is
almost certainly updated but not rebooted (the module is built for a specific
kernel). Run `yum update && reboot`, then run `install.sh` again.

## Configure

The two hosts run setup one after the other and coordinate over the network,
so the order and a few values matter:

* **Order.** Run node 1 first. It initialises DRBD and waits up to 5 minutes
  for the peer (if the peer does not connect in time, it promotes anyway and
  a full sync runs when the peer joins). Then run node 2, which connects,
  creates the shared SR and enables pool HA. Node 1 finishes once node 2 has
  joined.
* **Same CHAP password on both hosts.** Pick one and pass it to both with
  `--chap-password`. If you omit it, each host generates its own and the two
  iSCSI targets will not authenticate each other. Node 1 prints the password
  it used.
* **Each host's own backing device.** Pass `--backing-device` with a stable
  `/dev/disk/by-id/...` path for the disk on that host. Do not use a kernel
  name such as `/dev/nvme0n1`: kernel names can swap across reboots, while the
  by-id path always follows the same physical disk. The disk is wiped.
* **Network mode.** Without `--direct-ip`, setup configures fate-shared mode
  (one switched path). For arbitrated mode, add `--direct-ip <peer-direct-IP>`
  on each host, giving the peer's address on the back-to-back link; the
  direct-link interface must already have a static IP. Choose the mode now:
  the two-path configuration cannot be added to a running pair.

To list a host's candidate disks by their stable id:

```bash
ls -l /dev/disk/by-id/ | grep -v part   # pick the data disk, not the system disk
```

### Node 1 (run first, it waits for the peer)

```bash
twinstor setup \
  --peer-ip <node2-IP> \
  --backing-device /dev/disk/by-id/<node1-disk> \
  --chap-password <pick-a-password>
```

### Node 2 (run second, it creates the SR)

Use the same `--chap-password` and node 2's own `--backing-device`:

```bash
twinstor setup \
  --peer-ip <node1-IP> \
  --backing-device /dev/disk/by-id/<node2-disk> \
  --chap-password <same-password>
```

Setup writes the DRBD and iSCSI configuration, adds the firewall rules, and
enables and starts the daemon. On node 2 it also creates the SR and enables
pool HA with the mode's timeout (120 s fate-shared, 90 s arbitrated; see
`docs/HA_TIMEOUT_TUNING.md`).

Node 1 prints the command to run on the peer with a
`--backing-device <disk-on-peer>` placeholder: fill in node 2's own disk.
Simpler still, run plain `twinstor setup` on node 2: it reuses node 1's
settings and only asks for node 2's disk and the CHAP password.

### Interactive setup

Run `twinstor setup` with no arguments on a terminal for the wizard. It:

* detects the peer;
* asks for the network mode (fate-shared by default, arbitrated for a direct
  link between the hosts);
* lists the candidate disks and asks before wiping anything;
* on a single-disk host with no eligible spare device, offers to carve a
  backing partition from the system disk's unallocated space. This is a last
  resort behind a stricter typed `I confirm`; the whole system disk is never
  offered;
* on node 1, generates the CHAP password; on node 2, reuses node 1's settings
  and asks you to paste node 1's CHAP password (the password is a secret and
  is never stored in the pool database).

Run it on node 1 first, then on node 2.

### Check the result

Confirm which build you installed (include this in any report):

```bash
twinstor --version
```

Check the state at any time:

```bash
twinstor status
```

Right after node 2 joins, one host may briefly report `degraded-paths` (only
one iSCSI session). The path watchdog establishes the second session within a
minute and readiness settles to `ready`.

## Remove

To return both hosts to a clean state, first shut down or migrate every VM off
the TwinStor SR (the teardown refuses while VMs still use it), then on each
host:

```bash
cd twinstor-test-*
sudo ./uninstall.sh
```

This runs `twinstor uninstall` (disables XHA, destroys the SR, stops DRBD and
iSCSI, wipes the backing device, removes the configuration, drops the firewall
rules), then removes the binary, the systemd units and the remaining
configuration files. If `twinstor uninstall` does not complete, the script
stops without removing anything, so the cluster is never left without the
tool that manages it.

Options:

* `--yes` skips the confirmation prompt.
* `--keep-data` leaves the backing device as it is (no wipe).
* `--force` proceeds when the VM-safety check cannot confirm the SR is
  unused (for example on a host whose XAPI or HA view is degraded). Use it
  only when you know no VM depends on this host's storage.
* `--purge-deps` also removes the DRBD and targetcli packages, for a fully
  clean host.

## Reporting a problem

Run the support bundle on both hosts and attach both files to your report (a
GitHub issue; for anything that could lose or corrupt data, or a security
issue, follow `SECURITY.md` in the repository instead):

```bash
twinstor support-bundle
```

Each run writes a `twinstor-bundle-<host>-<timestamp>.tar.gz` containing the
logs (with rotated history), the configuration (CHAP password and DRBD secret
redacted), and a snapshot of DRBD, iSCSI, multipath, XAPI and host state.
TwinStor is a two-host system, and most issues (split-brain, fencing,
failover) can only be understood from both sides.

If you cannot run the bundle, include `twinstor --version` and
`twinstor status` output, `/var/log/twinstor.log`, and the XCP-ng version
from both hosts.
