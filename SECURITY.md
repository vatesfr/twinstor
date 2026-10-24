# Security policy

TwinStor runs as root in XCP-ng dom0 and controls the storage your VMs run
on, so we treat security and data-integrity bugs with priority.

## Reporting a vulnerability

Please do not open a public issue. Use GitHub's private vulnerability
reporting instead: open the **Security** tab of this repository and choose
**Report a vulnerability**. Include the TwinStor version, the XCP-ng version,
and steps to reproduce if you have them.

We will acknowledge the report, keep you informed while we work on a fix, and
credit you in the release notes unless you prefer otherwise.

## Supported versions

TwinStor is a tech preview. Fixes go into the latest release only.

## Data-integrity bugs

A bug that can lose acknowledged writes, serve stale data, or let both hosts
write diverging copies is handled like a security issue even when no attacker
is involved. If you can reproduce one, report it privately as above and
attach `twinstor support-bundle` output from both hosts.
