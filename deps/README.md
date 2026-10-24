# Runtime dependencies

The bundle ships the packages TwinStor needs on an XCP-ng 8.3 host that the
base install does not provide. They are third-party packages under their own
licences and are not kept in this repository. `scripts/make-bundle.sh`
downloads any that are missing from the URLs in `SOURCES` (into this
directory, or into `$TWINSTOR_DEPS`) and refuses to build unless each one
matches `SHA256SUMS`.

| Package | Provides | Source |
|---|---|---|
| `kmod-drbd` 9.2.16 | DRBD 9 kernel module for the XCP-ng 8.3 kernel | Vates LINSTOR repository for XCP-ng 8.3 (`repo.vates.tech/xcp-ng/8/8.3/linstor`) |
| `drbd-utils` 9.33.0 | `drbdadm`, `drbdsetup`, `drbdmeta` | same |
| `pyparsing` | dependency of `targetcli` | XCP-ng 8.3 base repository (`updates.xcp-ng.org`) |
| `targetcli`, `python-rtslib`, `python-configshell` | LIO iSCSI target management | CentOS 7.9 (`vault.centos.org`) |
| `python-kmod`, `python-ethtool`, `python-urwid`, `python-gobject-base`, `gobject-introspection`, `libnl` | dependencies of the above | CentOS 7.9 (`vault.centos.org`) |

To move to a newer package, update both `SOURCES` and `SHA256SUMS`.
