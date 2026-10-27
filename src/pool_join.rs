//! Join this host to the other host's pool from the setup wizard, when the
//! preflight finds it alone. The requirement used to be reported with the
//! `xe pool-join` command to type on the other host; testers who skipped the
//! requirements page then had to leave the wizard, find the command, and
//! come back. Here the wizard asks for the other host's address and root
//! password once, makes the two hosts' names distinct (TwinStor keys its
//! DRBD configuration on them), shows what it will do, and runs the join
//! after a typed confirmation.
//!
//! This host joins the other one's pool, so only the other host's password
//! is needed. The password never appears on a command line that is logged:
//! the remote reads use a password file, and the join itself is run without
//! the logging command runners.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::cmd::cmd_output;

/// What the wizard learned about both hosts before a join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JoinFacts {
    pub local_hostname: String,
    pub local_label: String,
    pub remote_hostname: String,
    pub remote_label: String,
    /// Hosts in the other host's pool.
    pub remote_hosts: usize,
    /// VMs running on this host (a joining host must have none).
    pub local_running_vms: usize,
}

/// Whether the join can go ahead, and whether this host needs a new name
/// first.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum JoinPlan {
    Refuse(String),
    /// Join; rename this host first when its hostname or name-label is the
    /// other host's.
    Join {
        rename: bool,
    },
}

pub(crate) fn join_plan(f: &JoinFacts) -> JoinPlan {
    if f.remote_hosts != 1 {
        return JoinPlan::Refuse(format!(
            "the other host's pool already has {} hosts; TwinStor pairs exactly two hosts",
            f.remote_hosts
        ));
    }
    if f.local_running_vms > 0 {
        return JoinPlan::Refuse(format!(
            "{} VM(s) run on this host, and a host joins a pool only with none running: shut \
             them down, or make the other host join this one instead (on the other host: xe \
             pool-join master-address=<this host's IP> master-username=root \
             master-password=<this host's root password>)",
            f.local_running_vms
        ));
    }
    JoinPlan::Join {
        rename: f.local_hostname == f.remote_hostname || f.local_label == f.remote_label,
    }
}

/// A hostname XAPI and DRBD both accept: letters, digits and dashes, 1 to 63
/// characters, not starting or ending with a dash.
pub(crate) fn valid_hostname(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// A name for this host that differs from the other host's.
pub(crate) fn suggest_name(local: &str, remote: &str) -> String {
    for suffix in ["2", "b", "3"] {
        let candidate = match local.strip_suffix(|c: char| c.is_ascii_digit()) {
            Some(stem) if stem.ends_with('-') => format!("{stem}{suffix}"),
            _ => format!("{local}-{suffix}"),
        };
        if candidate != remote && valid_hostname(&candidate) {
            return candidate;
        }
    }
    format!("{local}-twin")
}

/// Read a secret from the terminal without echoing it.
fn ask_secret(prompt: &str) -> Result<String> {
    print!("{prompt}: ");
    std::io::stdout().flush()?;
    let tty = || std::fs::File::open("/dev/tty");
    let echo_off = tty()
        .ok()
        .and_then(|t| {
            std::process::Command::new("stty")
                .arg("-echo")
                .stdin(t)
                .status()
                .ok()
        })
        .is_some_and(|s| s.success());
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line);
    if echo_off && let Ok(t) = tty() {
        let _ = std::process::Command::new("stty")
            .arg("echo")
            .stdin(t)
            .status();
    }
    println!();
    if read.context("reading input")? == 0 {
        bail!("input closed; nothing was changed");
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// A root-only password file for `xe -pwf`, removed when dropped.
struct PasswordFile(std::path::PathBuf);

impl PasswordFile {
    fn new(password: &str) -> Result<Self> {
        let path =
            std::path::PathBuf::from(format!("/run/twinstor-join-{}.pw", std::process::id()));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        f.write_all(password.as_bytes())?;
        Ok(PasswordFile(path))
    }
}

impl Drop for PasswordFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `xe` against the other host, authenticated with the password file.
async fn remote_xe(addr: &str, pw: &PasswordFile, args: &[&str]) -> Result<String> {
    let path = pw.0.display().to_string();
    let mut all = vec!["-s", addr, "-u", "root", "-pwf", path.as_str()];
    all.extend_from_slice(args);
    cmd_output("xe", &all).await
}

async fn remote_host_param(addr: &str, pw: &PasswordFile, uuid: &str, param: &str) -> String {
    remote_xe(
        addr,
        pw,
        &[
            "host-param-get",
            &format!("uuid={uuid}"),
            &format!("param-name={param}"),
        ],
    )
    .await
    .map(|s| s.trim().to_string())
    .unwrap_or_default()
}

async fn local_xe(args: &[&str]) -> Option<String> {
    cmd_output("xe", args)
        .await
        .ok()
        .map(|s| s.trim().to_string())
}

/// Offer to join this host to the other host's pool. Returns true when the
/// join was done (the caller checks the pool again), false when the operator
/// declined.
pub(crate) async fn offer() -> Result<bool> {
    println!();
    if !crate::wizard::ask_yes_no(
        "Join this host to the other host's pool now? The wizard does it for you",
        false,
    )? {
        return Ok(false);
    }
    let addr = loop {
        let a = crate::wizard::ask("The other host's IP address (its management address)", None)?;
        if a.parse::<std::net::IpAddr>().is_ok() {
            break a;
        }
        println!("  not a valid IP address, try again");
    };
    let password = ask_secret(&format!("Root password of {addr}"))?;
    let pw = PasswordFile::new(&password)?;

    println!("Reading the other host...");
    let remote_hosts = remote_xe(&addr, &pw, &["host-list", "--minimal"])
        .await
        .with_context(|| {
            format!(
                "could not log in to {addr} as root (wrong password, wrong address, or its \
                 toolstack is down); nothing was changed"
            )
        })?;
    let remote_uuids: Vec<&str> = remote_hosts
        .split(',')
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .collect();
    let remote_uuid = remote_uuids.first().copied().unwrap_or_default();
    let remote_hostname = remote_host_param(&addr, &pw, remote_uuid, "hostname").await;
    let remote_label = remote_host_param(&addr, &pw, remote_uuid, "name-label").await;

    let local_uuid = crate::setup::local_host_uuid()
        .await
        .context("reading this host's uuid")?;
    let local_hostname = local_xe(&[
        "host-param-get",
        &format!("uuid={local_uuid}"),
        "param-name=hostname",
    ])
    .await
    .unwrap_or_default();
    let local_label = local_xe(&[
        "host-param-get",
        &format!("uuid={local_uuid}"),
        "param-name=name-label",
    ])
    .await
    .unwrap_or_default();
    let local_running_vms = local_xe(&[
        "vm-list",
        &format!("resident-on={local_uuid}"),
        "power-state=running",
        "is-control-domain=false",
        "--minimal",
    ])
    .await
    .unwrap_or_default()
    .split(',')
    .filter(|v| !v.trim().is_empty())
    .count();

    let facts = JoinFacts {
        local_hostname: local_hostname.clone(),
        local_label,
        remote_hostname: remote_hostname.clone(),
        remote_label,
        remote_hosts: remote_uuids.len(),
        local_running_vms,
    };
    let rename = match join_plan(&facts) {
        JoinPlan::Refuse(why) => bail!("{why}; nothing was changed"),
        JoinPlan::Join { rename } => rename,
    };
    let new_name = if rename {
        println!(
            "Both hosts are named \"{local_hostname}\". TwinStor needs distinct names, so this \
             host gets a new one first."
        );
        let default = suggest_name(&local_hostname, &remote_hostname);
        loop {
            let n = crate::wizard::ask("New name for this host", Some(&default))?;
            if !valid_hostname(&n) {
                println!("  use letters, digits and dashes only (up to 63 characters)");
            } else if n == remote_hostname {
                println!("  that is the other host's name");
            } else {
                break Some(n);
            }
        }
    } else {
        None
    };

    println!("\nThe wizard will:");
    if let Some(n) = &new_name {
        println!("  - rename this host to \"{n}\" (hostname and name in XCP-ng)");
    }
    println!(
        "  - join this host to the pool of {addr} (\"{remote_hostname}\"): xe pool-join \
         master-address={addr} master-username=root master-password=********"
    );
    println!("This host's toolstack restarts during the join; it takes a minute or two.");
    let confirm = crate::wizard::ask("Type 'join' to proceed (anything else aborts)", None)?;
    if confirm != "join" {
        bail!("aborted by user; nothing was changed");
    }

    if let Some(n) = &new_name {
        crate::cmd::run_cmd(
            "xe",
            &[
                "host-set-hostname-live",
                &format!("host-uuid={local_uuid}"),
                &format!("host-name={n}"),
            ],
        )
        .await
        .context("renaming this host")?;
        crate::cmd::run_cmd(
            "xe",
            &[
                "host-param-set",
                &format!("uuid={local_uuid}"),
                &format!("name-label={n}"),
            ],
        )
        .await
        .context("setting this host's name in XCP-ng")?;
        println!("  this host is now \"{n}\".");
    }

    println!("  joining the pool of {addr}...");
    // Not through crate::cmd: its runners put the whole command line, with
    // the password, in the log and in the error.
    let out = tokio::time::timeout(
        Duration::from_secs(300),
        tokio::process::Command::new("xe")
            .args([
                "pool-join",
                &format!("master-address={addr}"),
                "master-username=root",
                &format!("master-password={password}"),
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await;
    match out {
        Ok(Ok(o)) if o.status.success() => {}
        Ok(Ok(o)) => bail!(
            "xe pool-join failed: {}",
            String::from_utf8_lossy(&o.stderr)
                .trim()
                .replace(&password, "********")
        ),
        Ok(Err(e)) => bail!("could not run xe pool-join: {e}"),
        Err(_) => {
            bail!("xe pool-join did not finish within 5 minutes; check the pool in Xen Orchestra")
        }
    }

    // The toolstack restarts as a pool member: wait until it lists both hosts.
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let n = local_xe(&["host-list", "--minimal"])
            .await
            .map(|l| l.split(',').filter(|u| !u.trim().is_empty()).count())
            .unwrap_or(0);
        if n == 2 {
            println!("  done: this host is in the pool of {addr}.\n");
            return Ok(true);
        }
    }
    bail!(
        "this host joined the pool of {addr}, but its toolstack does not list both hosts yet; \
         wait a minute, then run `twinstor setup` again"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> JoinFacts {
        JoinFacts {
            local_hostname: "xcp-ng-a".into(),
            local_label: "xcp-ng-a".into(),
            remote_hostname: "xcp-ng-b".into(),
            remote_label: "xcp-ng-b".into(),
            remote_hosts: 1,
            local_running_vms: 0,
        }
    }

    #[test]
    fn a_free_host_joins_and_keeps_a_distinct_name() {
        assert_eq!(join_plan(&facts()), JoinPlan::Join { rename: false });
    }

    #[test]
    fn a_name_shared_with_the_other_host_is_changed_first() {
        let mut f = facts();
        f.local_hostname = "xcp-ng".into();
        f.remote_hostname = "xcp-ng".into();
        assert_eq!(join_plan(&f), JoinPlan::Join { rename: true });
        // The XCP-ng installer's default label can collide on its own.
        let mut f = facts();
        f.local_label = "XCP-ng".into();
        f.remote_label = "XCP-ng".into();
        assert_eq!(join_plan(&f), JoinPlan::Join { rename: true });
    }

    #[test]
    fn a_join_that_would_not_give_a_pair_is_refused() {
        let mut f = facts();
        f.remote_hosts = 2;
        assert!(matches!(join_plan(&f), JoinPlan::Refuse(w) if w.contains("exactly two")));
        let mut f = facts();
        f.local_running_vms = 3;
        assert!(matches!(join_plan(&f), JoinPlan::Refuse(w) if w.contains("3 VM(s)")));
    }

    #[test]
    fn hostnames_are_checked_and_suggested() {
        assert!(valid_hostname("twinstor-2"));
        assert!(!valid_hostname(""));
        assert!(!valid_hostname("-a"));
        assert!(!valid_hostname("a-"));
        assert!(!valid_hostname("a b"));
        assert!(!valid_hostname("a.example.com"));
        assert!(!valid_hostname(&"a".repeat(64)));
        assert_eq!(suggest_name("xcp-ng", "xcp-ng"), "xcp-ng-2");
        assert_eq!(suggest_name("host-1", "host-1"), "host-2");
        // The first suggestion is the other host's name: take the next.
        assert_eq!(suggest_name("host-1", "host-2"), "host-b");
    }
}
