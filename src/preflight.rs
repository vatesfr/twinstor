//! Setup preflight: the installation requirements checked before the wizard
//! asks anything, each with the fix spelled out. Testers kept skipping the
//! requirements page (a host alone in its pool, the installer's local SR on
//! the data disks), and the wizard then failed later with a less useful
//! message ("could not detect the other host", "no free disk"). A missing
//! pool stops here; the rest warns and the operator decides.

use std::collections::HashMap;

use anyhow::{Result, bail};

use crate::cmd::cmd_output;

/// What the pool's host count means for setup.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PoolVerdict {
    /// Exactly two hosts: the supported topology.
    Pair,
    /// This host is alone in its pool: the other one has to join it first.
    Alone,
    /// More hosts than TwinStor pairs.
    TooMany(usize),
}

pub(crate) fn pool_verdict(hosts: usize) -> PoolVerdict {
    match hosts {
        2 => PoolVerdict::Pair,
        0 | 1 => PoolVerdict::Alone,
        n => PoolVerdict::TooMany(n),
    }
}

/// The `software-version` keys that must match on both hosts: the release,
/// its build, and the toolstack and hypervisor the updates bring.
const VERSION_KEYS: [&str; 4] = ["product_version", "build_number", "xapi", "xen"];

/// Parse `xe host-param-get param-name=software-version` (`key: value; ...`).
pub(crate) fn parse_software_version(raw: &str) -> HashMap<String, String> {
    raw.split(';')
        .filter_map(|kv| {
            let (k, v) = kv.split_once(':')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

/// The version keys on which two hosts differ, with both values. A key one
/// host does not report counts as a difference only when the other does.
pub(crate) fn version_differences(
    a: &HashMap<String, String>,
    b: &HashMap<String, String>,
) -> Vec<(&'static str, String, String)> {
    VERSION_KEYS
        .iter()
        .filter_map(|&k| {
            let (va, vb) = (a.get(k), b.get(k));
            (va != vb).then(|| {
                (
                    k,
                    va.cloned().unwrap_or_else(|| "?".into()),
                    vb.cloned().unwrap_or_else(|| "?".into()),
                )
            })
        })
        .collect()
}

/// Records of `xe pbd-list currently-attached=false params=sr-uuid,host-uuid`:
/// (sr uuid, host uuid) per unplugged PBD. xe prints the fields in record
/// order whatever `params=` asks (host-uuid first), so each blank-line
/// separated record is read whole.
pub(crate) fn parse_unplugged_pbds(out: &str) -> Vec<(String, String)> {
    let mut v = Vec::new();
    let (mut sr, mut host) = (None, None);
    let mut flush = |sr: &mut Option<String>, host: &mut Option<String>| {
        if let (Some(s), Some(h)) = (sr.take(), host.take()) {
            v.push((s, h));
        }
        *sr = None;
        *host = None;
    };
    for line in out.lines() {
        if line.trim().is_empty() {
            flush(&mut sr, &mut host);
            continue;
        }
        // `sr-uuid ( RO)    : <uuid>`: xe pads the column before the colon.
        let Some((left, value)) = line
            .split_once(')')
            .and_then(|(k, rest)| Some((k, rest.split_once(':')?.1)))
        else {
            continue;
        };
        let value = value.trim().to_string();
        match left.trim_start() {
            k if k.starts_with("sr-uuid") => sr = Some(value),
            k if k.starts_with("host-uuid") => host = Some(value),
            _ => {}
        }
    }
    flush(&mut sr, &mut host);
    v
}

async fn xe(args: &[&str]) -> Option<String> {
    cmd_output("xe", args)
        .await
        .ok()
        .map(|s| s.trim().to_string())
}

async fn host_name(uuid: &str) -> String {
    xe(&[
        "host-param-get",
        &format!("uuid={uuid}"),
        "param-name=name-label",
    ])
    .await
    .unwrap_or_else(|| uuid.to_string())
}

/// Run the checks and print the checklist. Errors only on what makes setup
/// impossible (no pool of two); warnings ask before going on when
/// `interactive`, and are only printed on the non-interactive path (the
/// operator who scripted the command decides from the output).
pub(crate) async fn run(interactive: bool) -> Result<()> {
    println!(
        "Checking the requirements (https://docs.vates.tech/tech-preview/twinstor/installation/#requirements):"
    );

    // 1. A pool of exactly two hosts. When this host is alone, the wizard
    // offers to run the join itself, then checks again.
    let hosts = loop {
        let Some(list) = xe(&["host-list", "--minimal"]).await else {
            bail!(
                "XAPI does not answer on this host, so the pool cannot be read. Restart the \
                 toolstack (xe-toolstack-restart), wait a minute, and run setup again."
            );
        };
        let hosts: Vec<String> = list
            .split(',')
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .map(String::from)
            .collect();
        if interactive && pool_verdict(hosts.len()) == PoolVerdict::Alone {
            println!(
                "  [FAIL] this host is alone in its pool; TwinStor pairs two hosts of one pool."
            );
            if crate::pool_join::offer().await? {
                continue;
            }
        }
        break hosts;
    };
    match pool_verdict(hosts.len()) {
        PoolVerdict::Pair => {
            let (a, b) = (host_name(&hosts[0]).await, host_name(&hosts[1]).await);
            println!("  [ok]   pool of two hosts: {a} and {b}");
        }
        PoolVerdict::Alone => {
            let me = xe(&["pif-list", "management=true", "params=IP", "--minimal"])
                .await
                .unwrap_or_default();
            let me = me.split(',').next().unwrap_or("").trim();
            let me = if me.is_empty() {
                "<this host's IP>"
            } else {
                me
            };
            bail!(
                "this host is alone in its pool. TwinStor pairs two hosts of the same pool.\n\
                 Join the other host to this pool first, then run setup again:\n\
                 \x20 - on the OTHER host: xe pool-join master-address={me} master-username=root master-password=<root password>\n\
                 \x20 - or in Xen Orchestra: open this pool and use \"Add host\".\n\
                 The joining host must have no running VM. If both hosts were reinstalled,\n\
                 also check that both are on the same XCP-ng updates (yum update, then reboot)."
            );
        }
        PoolVerdict::TooMany(n) => bail!(
            "this pool has {n} hosts. TwinStor pairs exactly two hosts: use a pool of two \
             (move the other hosts to another pool) and run setup again."
        ),
    }

    let mut warned = false;

    // 2. The same patch level on both hosts.
    let mut versions = Vec::new();
    for h in &hosts {
        let raw = xe(&[
            "host-param-get",
            &format!("uuid={h}"),
            "param-name=software-version",
        ])
        .await
        .unwrap_or_default();
        versions.push(parse_software_version(&raw));
    }
    let diffs = version_differences(&versions[0], &versions[1]);
    if diffs.is_empty() {
        println!("  [ok]   both hosts on the same XCP-ng build and updates");
    } else {
        warned = true;
        let (a, b) = (host_name(&hosts[0]).await, host_name(&hosts[1]).await);
        println!("  [warn] the hosts are not on the same updates ({a} / {b}):");
        for (k, va, vb) in &diffs {
            println!("           {k}: {va} / {vb}");
        }
        println!("         Update both (yum update, then reboot) to the same level before setup.");
    }

    // 3. No SR that fails to plug on a host.
    let unplugged = parse_unplugged_pbds(
        &xe(&[
            "pbd-list",
            "currently-attached=false",
            "params=sr-uuid,host-uuid",
        ])
        .await
        .unwrap_or_default(),
    );
    if unplugged.is_empty() {
        println!("  [ok]   every SR is plugged on every host");
    } else {
        warned = true;
        println!("  [warn] SRs not plugged on a host (a host whose SR fails to plug at boot stays");
        println!("         disabled, and HA cannot restart VMs on it):");
        for (sr, host) in &unplugged {
            let name = xe(&[
                "sr-param-get",
                &format!("uuid={sr}"),
                "param-name=name-label",
            ])
            .await
            .unwrap_or_default();
            println!("           \"{name}\" ({sr}) on {}", host_name(host).await);
        }
        println!("         Plug them (xe pbd-plug), or forget the SRs no longer in use.");
    }

    // 4. What software cannot check.
    println!("  [info] set \"Restore on AC power loss\" to Power On in both hosts' BIOS, so the");
    println!("         pool comes back by itself after a power cut (not checkable from here).");

    if warned && !interactive {
        println!("  (non-interactive: going on despite the warnings above)");
    } else if warned && !crate::wizard::ask_yes_no("\nContinue the setup anyway?", false)? {
        bail!("setup stopped at the requirements check; fix the warnings above and run it again");
    }
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_sizes() {
        assert_eq!(pool_verdict(2), PoolVerdict::Pair);
        assert_eq!(pool_verdict(1), PoolVerdict::Alone);
        assert_eq!(pool_verdict(0), PoolVerdict::Alone);
        assert_eq!(pool_verdict(3), PoolVerdict::TooMany(3));
    }

    #[test]
    fn versions_compare_on_the_update_keys_only() {
        let a = parse_software_version(
            "product_version: 8.3.0; build_number: cloud; xapi: 25.6; xen: 4.17.5-13; hostname: a; date: 2026-01-01",
        );
        let mut b = a.clone();
        b.insert("hostname".into(), "b".into());
        b.insert("date".into(), "2026-02-02".into());
        assert!(
            version_differences(&a, &b).is_empty(),
            "host-specific keys ignored"
        );
        b.insert("xen".into(), "4.17.5-15".into());
        assert_eq!(
            version_differences(&a, &b),
            vec![("xen", "4.17.5-13".to_string(), "4.17.5-15".to_string())]
        );
        b.remove("xapi");
        assert_eq!(
            version_differences(&a, &b).len(),
            2,
            "a key missing on one host differs"
        );
    }

    #[test]
    fn unplugged_pbds_pair_sr_and_host() {
        // As xe prints it: host-uuid first, whatever `params=` says.
        let out = "\
host-uuid ( RO)    : 22222222-bbbb\n\
      sr-uuid ( RO): 11111111-aaaa\n\
\n\
\n\
host-uuid ( RO)    : 44444444-dddd\n\
      sr-uuid ( RO): 33333333-cccc\n";
        let want = vec![
            ("11111111-aaaa".to_string(), "22222222-bbbb".to_string()),
            ("33333333-cccc".to_string(), "44444444-dddd".to_string()),
        ];
        assert_eq!(parse_unplugged_pbds(out), want);
        // The other order pairs the same way.
        let swapped = "\
sr-uuid ( RO)    : 11111111-aaaa\n\
    host-uuid ( RO): 22222222-bbbb\n\
\n\
sr-uuid ( RO)    : 33333333-cccc\n\
    host-uuid ( RO): 44444444-dddd\n";
        assert_eq!(parse_unplugged_pbds(swapped), want);
        // A single record (the case the first parser reported as all plugged).
        assert_eq!(
            parse_unplugged_pbds("host-uuid ( RO)    : h\n      sr-uuid ( RO): s\n"),
            vec![("s".to_string(), "h".to_string())]
        );
        assert!(parse_unplugged_pbds("").is_empty());
    }
}
