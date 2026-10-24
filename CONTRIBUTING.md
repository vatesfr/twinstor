# Contributing

Thanks for your interest in TwinStor. Bug reports, test results from real
hardware, documentation fixes and code are all welcome.

## Reporting bugs

Open an issue with the TwinStor and XCP-ng versions, the pool mode
(fate-shared or arbitrated), what happened, and what you expected. Run
`twinstor support-bundle` on **both** hosts and attach the archives: most
failover questions cannot be answered from one side. Check them for anything
you do not want to share first; they contain host names, IP addresses and
logs.

For anything that could lose or corrupt data, see [SECURITY.md](SECURITY.md).

## Building

TwinStor is a single Rust binary. XCP-ng dom0 ships an old glibc, so the
binary deployed to hosts is a static musl build:

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

`scripts/make-bundle.sh` builds the installable tarball. It downloads the
dependency RPMs it bundles and checks their checksums; see
[deps/README.md](deps/README.md).

## Before sending a pull request

```bash
cargo fmt
cargo clippy --all-targets
cargo test
```

The unit tests cover the decision logic (split-brain choice, fencing, HA
coordination, recovery ladders) without touching real storage, and they run
in about a minute.

Changes to failover, fencing, promotion or split-brain handling also need a
run on a real two-host pool. Describe what you tested in the pull request:
which faults (cable cuts of the master or the slave, power loss, both hosts
isolated), how long, HA on or off, and whether a write workload was checked
for lost or rolled-back data. If you do not have a test pool, say so; we will
run it on ours before merging.

## Code style

- Keep decision logic in pure functions that take observed state and return
  an action, so it can be unit-tested. Side effects stay in thin wrappers.
- Every external command goes through `src/cmd.rs`, which bounds it in time.
- Recovery decisions must rely on local observations (DRBD state, the local
  XHA liveset, the direct link), not on calls forwarded to a pool master that
  may be the host that just failed.
- Comments explain why, not what. Keep them short.

## Licence

By contributing you agree that your contribution is licensed under the
GNU General Public License, version 2 or any later version (see
[LICENSE](LICENSE)).
