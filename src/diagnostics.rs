//! Support-bundle collector.
//!
//! Gathers logs, configuration, and a point-in-time snapshot of runtime state
//! into a single redacted `.tar.gz`. The binary only *collects* raw data: it
//! never interprets it. All diagnosis (readiness verdicts, timeline
//! reconstruction, failure-signature matching) is done by the private analyzer
//! against these raw artifacts, so diagnosis can improve without shipping a
//! new binary to every node.
//!
//! TwinStor is a two-node product, so a single bundle only tells half the
//! story: run `support-bundle` on both nodes and send both files. The analyzer
//! stitches the two halves into one cross-node view.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use flate2::Compression;
use flate2::write::GzEncoder;

/// Layout/format version of the bundle. The analyzer reads this from the
/// manifest first and adapts. Bump on any change to file names or layout.
const SCHEMA_VERSION: u32 = 1;

/// Per-command wall-clock budget. A diagnostics tool runs on sick nodes, where
/// `xe` (wedged toolstack), `drbdadm`, or `targetcli` routinely hang forever.
/// Without a bound the bundle would never finish, which defeats its purpose, so
/// a hung command is recorded as timed-out and collection moves on.
const CMD_TIMEOUT: Duration = Duration::from_secs(20);

/// Run a command and capture its invocation, exit status, stdout, and stderr
/// into a single text blob. Best-effort: a command that is missing, fails, or
/// hangs produces an explanatory blob rather than aborting the bundle, because
/// a broken node is when we most need whatever else we can collect.
async fn capture_cmd(cmd: &str, args: &[&str]) -> Vec<u8> {
    let line = format!("$ {cmd} {}", args.join(" "));
    let mut buf = format!("{line}\n");
    // kill_on_drop so a command we abandon on timeout is reaped, not leaked.
    let fut = tokio::process::Command::new(cmd)
        .args(args)
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(CMD_TIMEOUT, fut).await {
        Ok(Ok(o)) => {
            match o.status.code() {
                Some(c) => buf.push_str(&format!("# exit: {c}\n")),
                None => buf.push_str("# exit: terminated by signal\n"),
            }
            if !o.stdout.is_empty() {
                buf.push_str("--- stdout ---\n");
                buf.push_str(&String::from_utf8_lossy(&o.stdout));
                if !buf.ends_with('\n') {
                    buf.push('\n');
                }
            }
            if !o.stderr.is_empty() {
                buf.push_str("--- stderr ---\n");
                buf.push_str(&String::from_utf8_lossy(&o.stderr));
                if !buf.ends_with('\n') {
                    buf.push('\n');
                }
            }
        }
        Ok(Err(e)) => buf.push_str(&format!("# failed to run: {e}\n")),
        Err(_) => buf.push_str(&format!("# timed out after {}s\n", CMD_TIMEOUT.as_secs())),
    }
    buf.into_bytes()
}

/// Run a batch of `(in-tar path, program, args)` command captures concurrently.
/// Each is best-effort and independent, so on a slow node the batch costs about
/// one slow command instead of the sum of all of them. Results come back in
/// completion order; tar/manifest ordering is handled by the caller.
async fn capture_all(specs: Vec<(String, String, Vec<String>)>) -> Vec<(String, Vec<u8>)> {
    let mut set = tokio::task::JoinSet::new();
    for (name, prog, args) in specs {
        set.spawn(async move {
            let argref: Vec<&str> = args.iter().map(String::as_str).collect();
            (name, capture_cmd(&prog, &argref).await)
        });
    }
    let mut out = Vec::new();
    while let Some(res) = set.join_next().await {
        if let Ok(pair) = res {
            out.push(pair);
        }
    }
    out
}

/// Mask the value of any `chap_password` key in the TOML config, preserving
/// indentation and the rest of the file verbatim. The CHAP password is the
/// only secret in the config; the username and IQNs are kept because they are
/// needed to read iSCSI/multipath state.
fn redact_chap_password(toml_text: &str) -> String {
    let mut out: String = toml_text
        .lines()
        .map(|line| {
            // Match the exact key (`chap_password = ...`), not a prefix, so a
            // hypothetical `chap_password_backup` field isn't mangled.
            let is_key = line
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == "chap_password");
            if is_key {
                let indent = &line[..line.len() - line.trim_start().len()];
                format!("{indent}chap_password = \"***REDACTED***\"")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    out.push('\n');
    out
}

/// Mask a DRBD `shared-secret` directive in a generated `.res` file. DRBD's
/// replication auth secret would otherwise be captured verbatim.
fn redact_drbd_secrets(res_text: &str) -> String {
    let mut out: String = res_text
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            // A directive whose first token is `shared-secret`; skip comments
            // (a `#`-prefixed line) so we don't uncomment and reshape them.
            if trimmed.starts_with("shared-secret") {
                let indent = &line[..line.len() - trimmed.len()];
                format!("{indent}shared-secret \"***REDACTED***\";")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    out.push('\n');
    out
}

fn json_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

/// Build the manifest the analyzer parses first to learn the bundle's shape.
fn build_manifest(
    hostname: &str,
    node: &str,
    peer: &str,
    timestamp: &str,
    contents: &[String],
    redactions: &[&str],
) -> String {
    let list = |items: &[String]| -> String {
        items
            .iter()
            .map(|s| format!("    \"{}\"", json_escape(s)))
            .collect::<Vec<_>>()
            .join(",\n")
    };
    let redaction_list = redactions
        .iter()
        .map(|s| format!("    \"{}\"", json_escape(s)))
        .collect::<Vec<_>>()
        .join(",\n");
    format!(
        "{{\n  \"schema_version\": {},\n  \"twinstor_version\": \"{}\",\n  \"hostname\": \"{}\",\n  \"node\": \"{}\",\n  \"peer\": \"{}\",\n  \"collected_at\": \"{}\",\n  \"redactions\": [\n{}\n  ],\n  \"contents\": [\n{}\n  ]\n}}\n",
        SCHEMA_VERSION,
        json_escape(env!("CARGO_PKG_VERSION")),
        json_escape(hostname),
        json_escape(node),
        json_escape(peer),
        json_escape(timestamp),
        redaction_list,
        list(contents),
    )
}

fn readme(redactions: &[&str]) -> Vec<u8> {
    let red = redactions
        .iter()
        .map(|s| format!("  - {s}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "TWINSTOR support bundle\n\
         =======================\n\n\
         This archive contains logs, configuration, and a point-in-time\n\
         snapshot of this node's storage state, collected by\n\
         `twinstor support-bundle`. It is meant to be attached to a problem\n\
         report so the issue can be diagnosed offline.\n\n\
         TWINSTOR runs on two nodes. Please also run `twinstor support-bundle`\n\
         on the PEER node and attach that file too: most issues (split-brain,\n\
         fencing, failover) can only be understood by looking at both sides.\n\n\
         Layout:\n\
         \x20 manifest.json   bundle metadata (version, node identity, contents)\n\
         \x20 config/         twinstor + DRBD configuration (secrets redacted)\n\
         \x20 logs/           daemon log (with rotated history) + fence + journal\n\
         \x20                 + xensource/xha/SM tails (election, fencing, SR ops)\n\
         \x20 state/          DRBD / iSCSI / multipath / XAPI snapshots\n\
         \x20 system/         host info (kernel, network, LVM, systemd)\n\n\
         The following secrets have been redacted before packaging:\n\
         {red}\n\n\
         No other data is scrubbed. IP addresses, hostnames, and IQNs are kept\n\
         because they are needed to interpret replication and pathing state.\n",
        red = red
    )
    .into_bytes()
}

/// Format a UTC epoch as `(compact, iso)`, e.g. `("20260617T120000Z",
/// "2026-06-17T12:00:00Z")`. Done in-process (Hinnant's civil-from-days) so the
/// bundle has no dependency on the host `date` binary and the manifest's ISO
/// timestamp is always well-formed even on a stripped-down or broken node.
fn format_utc(epoch_secs: u64) -> (String, String) {
    let (h, m, s) = (
        (epoch_secs / 3600) % 24,
        (epoch_secs / 60) % 60,
        epoch_secs % 60,
    );
    let z = (epoch_secs / 86400) as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    (
        format!("{year:04}{month:02}{d:02}T{h:02}{m:02}{s:02}Z"),
        format!("{year:04}-{month:02}-{d:02}T{h:02}:{m:02}:{s:02}Z"),
    )
}

/// Read a file into memory, returning at most `max` bytes from the end (the
/// most recent log lines matter most).
///
/// The file is snapshotted into a Vec rather than streamed straight into the
/// tar: a tar entry's header declares the byte count up front, so a
/// file that grows mid-read (the daemon log is appended live) or whose stat
/// size is a lie (`/proc/drbd` reports 0) would desync the header from the
/// payload and corrupt the archive. A single read gives one exact length.
///
/// Returns the io::Error on failure so the caller can distinguish a file that
/// is simply absent (skip it) from one that exists but couldn't be read (worth
/// recording).
fn read_file_capped(path: &Path, max: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    // proc files report len 0; treat that as "unknown" and just read to EOF.
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut buf = Vec::new();
    if len > max {
        let _ = f.seek(SeekFrom::Start(len - max));
        buf.extend_from_slice(
            format!("[... truncated: showing last {max} bytes of {len} ...]\n").as_bytes(),
        );
    }
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Append in-memory artifacts into a gzip-compressed tar. Every entry's size
/// is the exact length of its buffer, so headers always match payloads.
/// Returns the final archive size in bytes.
fn write_tarball(
    out_path: &Path,
    root: &str,
    generated: &[(String, Vec<u8>)],
    mtime: u64,
) -> Result<u64> {
    let file = std::fs::File::create(out_path)
        .with_context(|| format!("creating {}", out_path.display()))?;
    let enc = GzEncoder::new(file, Compression::default());
    let mut tar = tar::Builder::new(enc);

    for (name, bytes) in generated {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(mtime);
        header.set_cksum();
        tar.append_data(&mut header, format!("{root}/{name}"), bytes.as_slice())
            .with_context(|| format!("adding {name} to bundle"))?;
    }

    let enc = tar.into_inner().context("finalizing tar")?;
    enc.finish().context("finishing gzip")?;
    let size = std::fs::metadata(out_path).map(|m| m.len()).unwrap_or(0);
    Ok(size)
}

pub async fn run(
    config: &crate::config::Config,
    config_path: &Path,
    output: Option<PathBuf>,
) -> Result<()> {
    let hostname = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string());

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (ts_compact, ts_iso) = format_utc(now_secs);

    let resource = format!("twinstor-{}", config.storage.drbd_resource);
    let root = format!("twinstor-bundle-{hostname}-{ts_compact}");
    let out_path = output.unwrap_or_else(|| PathBuf::from(format!("{root}.tar.gz")));

    // In-memory artifacts: command captures + redacted configs + metadata.
    let mut generated: Vec<(String, Vec<u8>)> = Vec::new();

    // Runtime-state and host-info snapshots, all raw command output. params=all
    // on the `xe` calls because the default `*-list` output omits the fields
    // that matter most (pool ha-enabled / ha-host-failures-to-tolerate, host
    // enabled, PBD currently-attached). Order doesn't matter; they run
    // concurrently and the manifest sorts the final contents list.
    let spec = |name: &str, prog: &str, args: &[&str]| {
        (
            name.to_string(),
            prog.to_string(),
            args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
    };
    let specs = vec![
        spec("state/drbd-status.txt", "drbdadm", &["status", &resource]),
        spec("state/drbd-dump.txt", "drbdadm", &["dump", &resource]),
        spec("state/drbd-version.txt", "drbdadm", &["--version"]),
        spec("state/multipath.txt", "multipath", &["-ll"]),
        spec("state/targetcli.txt", "targetcli", &["ls"]),
        spec("state/iscsi-sessions.txt", "iscsiadm", &["-m", "session"]),
        spec("state/xapi-pool.txt", "xe", &["pool-list", "params=all"]),
        spec("state/xapi-hosts.txt", "xe", &["host-list", "params=all"]),
        spec("state/xapi-sr.txt", "xe", &["sr-list", "params=all"]),
        spec("state/xapi-pbd.txt", "xe", &["pbd-list", "params=all"]),
        spec("system/uname.txt", "uname", &["-a"]),
        spec("system/ip-addr.txt", "ip", &["addr"]),
        spec("system/ip-route.txt", "ip", &["route"]),
        spec(
            "system/lvs.txt",
            "lvs",
            &["-a", "-o", "+lv_uuid,lv_time_removed"],
        ),
        spec("system/vgs.txt", "vgs", &[]),
        spec(
            "system/systemctl-status.txt",
            "systemctl",
            &["status", "twinstor", "--no-pager"],
        ),
        spec("system/dmesg.txt", "dmesg", &["-T"]),
        spec(
            "logs/journal-twinstor.txt",
            "journalctl",
            &["-u", "twinstor", "--no-pager", "-n", "20000"],
        ),
        // Dom0 control-plane logs: XHA election/fencing and the XAPI master
        // transition do not appear in the daemon log, but a failover cannot be
        // interpreted without them (for example, when the survivor became
        // master). Tail-bounded; a missing file shows as a nonzero exit in the
        // capture rather than being dropped silently.
        spec(
            "logs/xensource.txt",
            "tail",
            &["-n", "5000", "/var/log/xensource.log"],
        ),
        spec("logs/xha.txt", "tail", &["-n", "5000", "/var/log/xha.log"]),
        spec("logs/smlog.txt", "tail", &["-n", "5000", "/var/log/SMlog"]),
        // Rotated logs: a fence storm can rotate the logs mid-incident, leaving
        // the relevant history in the .1 files and the current tails empty.
        spec(
            "logs/xha.1.txt",
            "tail",
            &["-n", "5000", "/var/log/xha.log.1"],
        ),
        spec(
            "logs/kern.txt",
            "tail",
            &["-n", "3000", "/var/log/kern.log"],
        ),
        spec(
            "logs/kern.1.txt",
            "tail",
            &["-n", "3000", "/var/log/kern.log.1"],
        ),
        spec(
            "logs/daemon.1.txt",
            "tail",
            &["-n", "3000", "/var/log/daemon.log.1"],
        ),
        // Boot/fence chronology: `last -x` shows a repeated fence-reboot loop
        // that log tails can miss.
        spec("system/last-boots.txt", "last", &["-x", "-n", "40"]),
        // DRBD deep state: generation identifiers show which side diverged
        // from which; statistics carry the out-of-sync counts; events2 shows
        // per-path establishment (arbitrated).
        spec("state/drbd-show-gi.txt", "drbdadm", &["show-gi", &resource]),
        spec(
            "state/drbd-statistics.txt",
            "drbdsetup",
            &["status", "--verbose", "--statistics"],
        ),
        spec(
            "state/drbd-events2.txt",
            "drbdsetup",
            &["events2", "--now", &resource],
        ),
        // Network-stack snapshot (for ping working while TCP is dead): conntrack
        // counters, firewall counters, socket summary, OVS datapath flows.
        spec("system/conntrack.txt", "conntrack", &["-S"]),
        spec("system/iptables-input.txt", "iptables", &["-vnL", "INPUT"]),
        spec(
            "system/iptables-output.txt",
            "iptables",
            &["-vnL", "OUTPUT"],
        ),
        spec("system/ss-summary.txt", "ss", &["-s"]),
        spec("system/ss-listeners.txt", "ss", &["-ltn"]),
        spec("system/ovs-flows.txt", "ovs-appctl", &["dpctl/dump-flows"]),
    ];

    // Run the command batch and the backing-disk write-cache probe (its own
    // nvme/hdparm subprocesses) concurrently. Write-cache must be disabled for
    // durability; reuse the same probe the daemon uses.
    let (cmd_results, cache) = tokio::join!(
        capture_all(specs),
        crate::write_cache::status(&config.storage.backing_device),
    );
    generated.extend(cmd_results);

    let mut cache_txt = String::from("backing-disk write cache state\n");
    for (disk, state) in &cache {
        cache_txt.push_str(&format!("{disk}: {} ({})\n", state.word(), state.label()));
    }
    generated.push(("state/write-cache.txt".into(), cache_txt.into_bytes()));
    // Heal-episode index: every episode_open/close bookend plus a full
    // per-id extraction of the most recent episodes, so support starts from
    // an incident list instead of raw logs.
    {
        let mut logs = String::new();
        for p in ["/var/log/twinstor.log.1", "/var/log/twinstor.log"] {
            if let Ok(s) = std::fs::read_to_string(p) {
                logs.push_str(&s);
            }
        }
        let (index, per_episode) = episode_index_from(&logs);
        generated.push(("logs/episodes.txt".into(), index.into_bytes()));
        for (name, body) in per_episode {
            generated.push((name, body.into_bytes()));
        }
    }

    // --- redacted configuration ---
    let redactions = ["chap_password (twinstor.toml)", "shared-secret (drbd .res)"];
    // The main config was already read by Config::load before dispatch, so a
    // failure here is unexpected; record it rather than silently dropping it.
    match std::fs::read_to_string(config_path) {
        Ok(text) => generated.push((
            "config/twinstor.toml".into(),
            redact_chap_password(&text).into_bytes(),
        )),
        Err(e) => generated.push((
            "config/twinstor.toml.read-error".into(),
            format!("could not read {}: {e}\n", config_path.display()).into_bytes(),
        )),
    }
    let res_path = PathBuf::from(format!("/etc/drbd.d/{resource}.res"));
    match std::fs::read_to_string(&res_path) {
        Ok(text) => generated.push((
            format!("config/{resource}.res"),
            redact_drbd_secrets(&text).into_bytes(),
        )),
        // Absent is normal in some topologies; only a real read error is noted.
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => generated.push((
            format!("config/{resource}.res.read-error"),
            format!("could not read {}: {e}\n", res_path.display()).into_bytes(),
        )),
        Err(_) => {}
    }

    // --- on-disk files copied verbatim (read into memory, see read_file_capped) ---
    const FILE_CAP: u64 = 25 * 1024 * 1024;
    let mut files: Vec<(String, PathBuf)> = vec![
        (
            "config/ping-targets".into(),
            PathBuf::from("/etc/twinstor/ping-targets"),
        ),
        (
            "config/direct-peer".into(),
            PathBuf::from("/etc/twinstor/direct-peer"),
        ),
        (
            "config/multipath-twinstor.conf".into(),
            PathBuf::from("/etc/multipath/conf.d/twinstor.conf"),
        ),
        ("state/proc-drbd.txt".into(), PathBuf::from("/proc/drbd")),
        // dom0 memory and slab: catches elevated usage or DRBD-kmod slab growth at
        // bundle time (an OOM would already be in dmesg/kern.log, but a sub-OOM
        // leak or resync-time pressure otherwise leaves no trace).
        ("system/meminfo.txt".into(), PathBuf::from("/proc/meminfo")),
        (
            "system/slabinfo.txt".into(),
            PathBuf::from("/proc/slabinfo"),
        ),
        (
            "state/split-brain-marker".into(),
            PathBuf::from("/run/twinstor/split-brain"),
        ),
        (
            "state/heal-state.json".into(),
            PathBuf::from(crate::heal_state::HEAL_STATE_PATH),
        ),
    ];

    // The durable and runtime state files the decision paths read (markers,
    // latches, records): small, and they say which path a restart resumes.
    for (dir, tag) in [("/var/lib/twinstor", "var-lib"), ("/run/twinstor", "run")] {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_file()) {
                    let name = entry.file_name().to_string_lossy().to_string();
                    files.push((format!("state/{tag}/{name}"), entry.path()));
                }
            }
        }
    }

    // Daemon log plus any rotated history (twinstor.log, twinstor.log.1, ...).
    match std::fs::read_dir("/var/log") {
        Ok(entries) => {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                // The fence-peer handler's log too, rotations included: its
                // verdicts are half of every split episode.
                if name.starts_with("twinstor.log") || name.starts_with("twinstor-fence-peer.log") {
                    files.push((format!("logs/{name}"), entry.path()));
                }
            }
        }
        Err(e) => generated.push((
            "logs/var-log.read-error".into(),
            format!("could not list /var/log: {e}\n").into_bytes(),
        )),
    }

    // A file that is simply absent is skipped silently (some are topology- or
    // state-dependent, e.g. the split-brain marker). A file that exists but
    // can't be read leaves a `.read-error` stub so the gap is visible.
    for (name, path) in &files {
        match read_file_capped(path, FILE_CAP) {
            Ok(bytes) => generated.push((name.clone(), bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => generated.push((
                format!("{name}.read-error"),
                format!("could not read {}: {e}\n", path.display()).into_bytes(),
            )),
        }
    }

    // Manifest last, listing everything actually included.
    let mut contents: Vec<String> = generated.iter().map(|(n, _)| n.clone()).collect();
    contents.sort();
    let manifest = build_manifest(
        &hostname,
        &config.node.name,
        &config.node.peer,
        &ts_iso,
        &contents,
        &redactions,
    );
    generated.push(("manifest.json".into(), manifest.into_bytes()));
    generated.push(("README.txt".into(), readme(&redactions)));

    let size = write_tarball(&out_path, &root, &generated, now_secs)?;

    println!(
        "support bundle written: {} ({} bytes)",
        out_path.display(),
        size
    );
    println!("run `twinstor support-bundle` on the peer node too, and send both files.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn redacts_chap_password_keeps_rest() {
        let toml =
            "[iscsi]\nchap_user = \"alice\"\nchap_password = \"hunter2\"\nlun_serial = \"x\"\n";
        let red = redact_chap_password(toml);
        assert!(!red.contains("hunter2"));
        assert!(red.contains("***REDACTED***"));
        // Non-secret fields preserved.
        assert!(red.contains("chap_user = \"alice\""));
        assert!(red.contains("lun_serial = \"x\""));
    }

    #[test]
    fn redacts_chap_password_with_indentation() {
        let toml = "    chap_password = \"secret\"\n";
        let red = redact_chap_password(toml);
        assert!(red.starts_with("    chap_password = \"***REDACTED***\""));
        assert!(!red.contains("secret"));
    }

    #[test]
    fn redacts_drbd_shared_secret() {
        let res = "resource x {\n  net {\n    shared-secret \"topsecret\";\n  }\n}\n";
        let red = redact_drbd_secrets(res);
        assert!(!red.contains("topsecret"));
        assert!(red.contains("***REDACTED***"));
        assert!(red.contains("resource x"));
    }

    #[test]
    fn redact_only_matches_exact_chap_password_key() {
        // A different key that merely starts with the same prefix must be left
        // intact, not rewritten to chap_password.
        let toml = "chap_password_backup = \"keep-me\"\n";
        let red = redact_chap_password(toml);
        assert!(red.contains("chap_password_backup = \"keep-me\""));
    }

    #[test]
    fn redact_skips_commented_shared_secret() {
        // A comment must not be uncommented/reshaped into an active directive.
        let res = "  # shared-secret was here\n  shared-secret \"live\";\n";
        let red = redact_drbd_secrets(res);
        assert!(red.contains("# shared-secret was here"));
        assert!(!red.contains("live"));
        assert!(red.contains("shared-secret \"***REDACTED***\";"));
    }

    #[test]
    fn format_utc_matches_known_epochs() {
        assert_eq!(
            format_utc(0),
            (
                "19700101T000000Z".to_string(),
                "1970-01-01T00:00:00Z".to_string()
            )
        );
        // Cross-checked against `date -u -d @1781438596`.
        assert_eq!(
            format_utc(1_781_438_596),
            (
                "20260614T120316Z".to_string(),
                "2026-06-14T12:03:16Z".to_string()
            )
        );
    }

    #[test]
    fn manifest_is_valid_shape_and_escaped() {
        let m = build_manifest(
            "host\"weird",
            "node-a",
            "node-b",
            "2026-06-17T00:00:00Z",
            &["state/drbd-status.txt".to_string()],
            &["chap_password"],
        );
        assert!(m.contains("\"schema_version\": 1"));
        assert!(m.contains("\"node\": \"node-a\""));
        // Quote in hostname must be escaped, not raw.
        assert!(m.contains("host\\\"weird"));
        assert!(m.contains("state/drbd-status.txt"));
    }

    #[test]
    fn tarball_roundtrips_generated_entries() {
        let dir = std::env::temp_dir();
        let out = dir.join("twinstor-test-bundle.tar.gz");
        let _ = std::fs::remove_file(&out);

        let generated = vec![
            ("manifest.json".to_string(), b"{\"k\":1}".to_vec()),
            ("state/x.txt".to_string(), b"hello".to_vec()),
        ];
        let size = write_tarball(&out, "root", &generated, 0).unwrap();
        assert!(size > 0);

        // Read it back and confirm both entries land under the root dir.
        let f = std::fs::File::open(&out).unwrap();
        let dec = flate2::read::GzDecoder::new(f);
        let mut ar = tar::Archive::new(dec);
        let mut found = std::collections::HashMap::new();
        for entry in ar.entries().unwrap() {
            let mut e = entry.unwrap();
            let path = e.path().unwrap().to_string_lossy().to_string();
            let mut buf = Vec::new();
            e.read_to_end(&mut buf).unwrap();
            found.insert(path, buf);
        }
        assert_eq!(
            found.get("root/manifest.json").map(|v| v.as_slice()),
            Some(&b"{\"k\":1}"[..])
        );
        assert_eq!(
            found.get("root/state/x.txt").map(|v| v.as_slice()),
            Some(&b"hello"[..])
        );

        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn proc_file_with_lying_stat_size_roundtrips() {
        // /proc files stat as size 0 but have content. Reading into memory must
        // capture an exact length so the tar header matches the payload; a
        // mismatch corrupts the archive. The roundtrip below would error on a
        // header/payload desync.
        let content = read_file_capped(Path::new("/proc/self/status"), 25 * 1024 * 1024)
            .expect("read /proc/self/status");
        assert!(!content.is_empty(), "proc file should have content");

        let dir = std::env::temp_dir();
        let out = dir.join("twinstor-test-proc.tar.gz");
        let _ = std::fs::remove_file(&out);
        let generated = vec![("state/proc.txt".to_string(), content.clone())];
        write_tarball(&out, "root", &generated, 0).unwrap();

        let f = std::fs::File::open(&out).unwrap();
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(f));
        let mut got = None;
        for entry in ar.entries().unwrap() {
            let mut e = entry.unwrap();
            if e.path().unwrap().to_string_lossy() == "root/state/proc.txt" {
                let mut buf = Vec::new();
                e.read_to_end(&mut buf).unwrap();
                got = Some(buf);
            }
        }
        assert_eq!(got.as_deref(), Some(content.as_slice()));
        let _ = std::fs::remove_file(&out);
    }
}

/// Build the episode index from raw daemon-log text: the open/close bookend
/// lines, plus a full line-extraction for each of the most recent episodes
/// (newest 5). Free of I/O for testability.
fn episode_index_from(logs: &str) -> (String, Vec<(String, String)>) {
    let mut index = String::new();
    let mut ids: Vec<String> = Vec::new();
    for line in logs.lines() {
        if line.contains("heal_event=\"episode_open\"")
            || line.contains("heal_event=\"episode_close\"")
            || line.contains("heal_event=\"incident_begin\"")
            || line.contains("heal_event=\"incident_end\"")
        {
            index.push_str(line);
            index.push('\n');
            let key = if line.contains("episode_open") {
                Some("episode=")
            } else if line.contains("incident_begin") {
                Some("incident=")
            } else {
                None
            };
            if let Some(id) = key
                .and_then(|k| line.split(k).nth(1))
                .and_then(|r| r.split_whitespace().next())
            {
                ids.push(id.to_string());
            }
        }
    }
    if index.is_empty() {
        index.push_str("no heal episodes recorded\n");
    }
    let mut per = Vec::new();
    for id in ids.iter().rev().take(5) {
        let body: String = logs
            .lines()
            .filter(|l| l.contains(id.as_str()))
            .map(|l| format!("{l}\n"))
            .collect();
        per.push((format!("logs/episode-{id}.txt"), body));
    }
    (index, per)
}

#[cfg(test)]
mod episode_index_tests {
    use super::episode_index_from;

    #[test]
    fn indexes_and_extracts_recent_episodes() {
        let logs = "\
a INFO heal episode opened episode=splitbrain-11 class=\"splitbrain\" heal_event=\"episode_open\"\n\
b INFO ep{id=splitbrain-11}: doing things heal_event=\"loser_discard\"\n\
c INFO ep{id=splitbrain-11}: heal episode closed episode=splitbrain-11 heal_event=\"episode_close\"\n\
d INFO heal episode opened episode=limbo-22 class=\"limbo\" heal_event=\"episode_open\"\n";
        let (index, per) = episode_index_from(logs);
        assert_eq!(index.lines().count(), 3, "two opens + one close");
        assert_eq!(per.len(), 2);
        assert_eq!(per[0].0, "logs/episode-limbo-22.txt");
        let sb = per
            .iter()
            .find(|(n, _)| n.contains("splitbrain-11"))
            .unwrap();
        assert_eq!(sb.1.lines().count(), 3, "all lines mentioning the id");
    }

    #[test]
    fn empty_logs_yield_placeholder() {
        let (index, per) = episode_index_from("");
        assert!(index.contains("no heal episodes"));
        assert!(per.is_empty());
    }
}
