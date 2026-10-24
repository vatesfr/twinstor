# TwinStor

Shared storage for two-host XCP-ng pools, built from the local disks of the two hosts.

TwinStor turns a local disk on each host into one shared SR. Live migration, HA, snapshots, backups and Rolling Pool Updates work as they do with a SAN, without a SAN, a NAS or a third host.

**Status: tech preview.** Use it on lab pools and keep backing up your VMs. Feedback is welcome on the [XCP-ng forum](https://xcp-ng.org/forum/topic/12323/twinstor-next-gen-2-nodes-hci).

## What you get

- **A standard shared SR.** VMs, disks, snapshots, backups and Xen Orchestra work as usual. Nothing in XCP-ng is patched.
- **Every write on both hosts.** A write is confirmed to the VM only once both hosts have it, so losing a host never loses a confirmed write.
- **Local reads.** Each VM reads from the disk of the host it runs on.
- **No witness.** The network gateway you already have is the tie-breaker.
- **Self-healing.** Failed hosts, disks and links are handled automatically, and the pool returns to full redundancy on its own.
- **Simple to run.** One command installs, one sets up, one removes. `twinstor status` tells you where things stand, and every alert says what to do.

## What happens when something fails

| Failure | What happens |
|---|---|
| A host dies | The other host keeps serving. HA restarts the lost host's VMs on it. When the host returns, it catches up and redundancy is back. |
| A disk dies | VMs keep running from the other host's copy. Replace the disk when convenient. |
| The network between the hosts is cut | The host that lost the network freezes and steps aside; the other keeps serving. No confirmed write is lost, and the pair resyncs when the link is back. |
| Power cut on both hosts | Both hosts boot, storage comes back and HA restarts the VMs, in about 3 minutes. |
| TwinStor itself restarts or is upgraded | Storage keeps serving and the VMs keep running. |

When TwinStor cannot be sure what is safe, it does not guess: it keeps both copies and raises an alert telling you what to decide. Waiting is always safe.

## Two network modes

TwinStor asks how your hosts are connected:

- **Fate-shared** (default): the hosts and the gateway share the same network, one switch or a switch pair run as one. A host that loses its peer also loses the gateway, so it knows it is the one to step aside.
- **Arbitrated**: replication runs on a separate network. A direct cable between the two hosts carries it, with the switched network as backup.

The [installation guide](https://docs.vates.tech/tech-preview/twinstor/installation/#pick-your-topology) helps you choose.

## Limits

- **Exactly two hosts and two copies.** While a host or a disk is down, the pool runs on one copy until the resync finishes.
- **Long network cuts with HA on can reboot a host.** After a cut of more than about a minute, XCP-ng HA may reboot one host once the link is back. No confirmed write is lost, and HA restarts that host's VMs.
- **A double network failure can cost a few seconds of writes.** If the two hosts lose each other completely while both still reach the gateway (in arbitrated mode, the direct cable and the switched path failing together), both keep serving. When they reconnect, the writes made on one side during the cut are discarded, and an alert names the VMs concerned.
- **XCP-ng 8.3, x86_64.**

## How it is tested

Every release goes through real faults on two hardware pools, one per network mode: cables cut, power removed, hosts rebooted, with HA on and the guests writing checksummed data that is verified afterwards. The 0.7.1 release candidates went through about 340 such cases, and none lost a confirmed write. More than 700 unit tests cover the decision logic.

## Getting started

You need two XCP-ng 8.3 hosts in the same pool, each with a spare disk, partition or md-RAID device that TwinStor may erase.

1. Get the bundle from the [TwinStor deploy page](https://cdn.vates.tech/twinstor/deploy/) and run `./install.sh` on each host.
2. Run `twinstor setup` on the first host, then on the second, and answer the questions.
3. Check with `twinstor status` on both hosts.

The full procedure is in the [installation guide](https://docs.vates.tech/tech-preview/twinstor/installation/).

## Documentation

The user documentation is on [docs.vates.tech](https://docs.vates.tech/tech-preview/twinstor/): [installation](https://docs.vates.tech/tech-preview/twinstor/installation/), [administration](https://docs.vates.tech/tech-preview/twinstor/administration/), [failure behavior](https://docs.vates.tech/tech-preview/twinstor/failure-behavior/), [updates](https://docs.vates.tech/tech-preview/twinstor/updates/) and [troubleshooting](https://docs.vates.tech/tech-preview/twinstor/troubleshooting/).

## Building

TwinStor is a single static Rust binary running in dom0.

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
cargo test
```

`scripts/make-bundle.sh` builds the installable bundle. See [CONTRIBUTING.md](CONTRIBUTING.md).

## Reporting problems

Open an issue and attach the output of `twinstor support-bundle` from **both** hosts. For anything that could lose or corrupt data, or a security issue, follow [SECURITY.md](SECURITY.md).

## Licence

TwinStor is developed by [Vates](https://vates.tech), the company behind XCP-ng and Xen Orchestra. It is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License, version 2 or (at your option) any later version; see [LICENSE](LICENSE).
