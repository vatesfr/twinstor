# Security policy

TwinStor follows the Vates security process, described in
[Security at Vates](https://docs.vates.tech/security-advisories/security/):
how reports are handled, coordinated disclosure, and security advisories.

## Reporting a vulnerability

Please do not open a public issue. Report it privately to
**security@vates.tech**, with the TwinStor version, the XCP-ng version, and
steps to reproduce if you have them.

## Data-integrity bugs

A bug that can lose acknowledged writes, serve stale data, or let both hosts
write diverging copies is handled like a security issue even when no attacker
is involved. If you can reproduce one, report it privately as above and
attach `twinstor support-bundle` output from both hosts.

## Supported versions

TwinStor is a tech preview. Fixes go into the latest release only.
