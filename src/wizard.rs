//! Interactive setup wizard, run when `twinstor setup` is invoked with no
//! arguments on a terminal. Every prompt has an Enter-through default so the
//! fast path is a handful of keystrokes, and the destructive step (wiping the
//! backing device) always requires an explicit confirmation that shows the
//! device's model, serial and size.

use std::io::{BufRead, IsTerminal, Write};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::cmd::{cmd_output, run_cmd};
use crate::setup::{ARBITRATED_MODE_NOTE, SetupParams};

pub fn stdin_is_tty() -> bool {
    std::io::stdin().is_terminal()
}

/// Print the TwinStor wordmark shown at the top of the interactive wizard
/// (figlet "Standard" font). Raw string so the backslashes stay literal.
fn print_banner() {
    println!(
        r#"
 _____          _       ____  _
|_   _|_      _(_)_ __ / ___|| |_ ___  _ __
  | | \ \ /\ / / | '_ \\___ \| __/ _ \| '__|
  | |  \ V  V /| | | | |___) | || (_) | |
  |_|   \_/\_/ |_|_| |_|____/ \__\___/|_|
"#
    );
}

/// Ask a question; empty input returns the default (when there is one).
pub(crate) fn ask(question: &str, default: Option<&str>) -> Result<String> {
    match default {
        Some(d) if !d.is_empty() => print!("{question} [{d}]: "),
        _ => print!("{question}: "),
    }
    std::io::stdout().flush()?;
    let mut line = String::new();
    let read = std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("reading input")?;
    // End of input (Ctrl-D, a closed pipe): stop instead of looping on a
    // question that has no default.
    if read == 0 {
        println!();
        bail!("input closed; nothing more was changed");
    }
    let line = line.trim().to_string();
    if line.is_empty() {
        Ok(default.unwrap_or("").to_string())
    } else {
        Ok(line)
    }
}

pub fn ask_yes_no(question: &str, default_yes: bool) -> Result<bool> {
    let hint = if default_yes { "Y/n" } else { "y/N" };
    let answer = ask(&format!("{question} ({hint})"), Some(""))?;
    Ok(match answer.to_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        _ => false,
    })
}

/// Prompt until a valid IP address is entered (no default).
fn ask_ip(question: &str) -> Result<IpAddr> {
    loop {
        match ask(question, None)?.parse() {
            Ok(ip) => return Ok(ip),
            Err(_) => println!("  not a valid IP address, try again"),
        }
    }
}

/// A selectable backing target: a whole raw disk, or a free partition that no
/// SR, mount, swap, or LVM volume group is using. On the system disk only a
/// partition explicitly labeled for TwinStor is eligible (`on_system`).
struct CandidateDisk {
    kernel: String,
    by_id: Option<PathBuf>,
    model: String,
    size_gb: u64,
    is_partition: bool,
    fstype: String,
    on_system: bool,
}

/// The disk hosting the root filesystem (its whole parent device).
fn system_disk() -> Option<String> {
    let mounts = std::fs::read_to_string("/proc/mounts").ok()?;
    let root_dev = mounts.lines().find_map(|l| {
        let mut f = l.split_whitespace();
        let dev = f.next()?;
        (f.next()? == "/").then(|| dev.trim_start_matches("/dev/").to_string())
    })?;
    // Map the partition to its parent disk (nvme0n1p3 to nvme0n1, sda2 to sda).
    for disk in std::fs::read_dir("/sys/block").ok()?.flatten() {
        let name = disk.file_name().to_string_lossy().to_string();
        if root_dev == name || (root_dev.starts_with(&name) && root_dev.len() > name.len()) {
            return Some(name);
        }
    }
    None
}

/// Best /dev/disk/by-id alias per canonical device (model+serial names first).
fn by_id_map() -> std::collections::HashMap<PathBuf, PathBuf> {
    let mut best: std::collections::HashMap<PathBuf, (u8, PathBuf)> = Default::default();
    if let Ok(entries) = std::fs::read_dir("/dev/disk/by-id") {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(target) = std::fs::canonicalize(e.path()) else {
                continue;
            };
            let score = if name.starts_with("nvme-eui.") || name.starts_with("wwn-") {
                2
            } else if name.starts_with("nvme-")
                || name.starts_with("ata-")
                || name.starts_with("scsi-")
            {
                3
            } else {
                1
            };
            let entry = best.entry(target).or_insert((0, e.path()));
            if score > entry.0 {
                *entry = (score, e.path());
            }
        }
    }
    best.into_iter().map(|(k, (_, v))| (k, v)).collect()
}

/// Canonical device paths currently mounted or used as swap.
fn busy_sources() -> std::collections::HashSet<PathBuf> {
    let mut set = std::collections::HashSet::new();
    for (file, skip) in [("/proc/mounts", 0usize), ("/proc/swaps", 1)] {
        if let Ok(text) = std::fs::read_to_string(file) {
            for line in text.lines().skip(skip) {
                if let Some(dev) = line.split_whitespace().next()
                    && dev.starts_with("/dev/")
                    && let Ok(c) = std::fs::canonicalize(dev)
                {
                    set.insert(c);
                }
            }
        }
    }
    set
}

/// Canonical device paths that are LVM physical volumes (backing an SR or a
/// system volume group), which must never be offered.
fn lvm_pv_paths() -> std::collections::HashSet<PathBuf> {
    let mut set = std::collections::HashSet::new();
    if let Ok(out) = std::process::Command::new("pvs")
        .args(["--noheadings", "-o", "pv_name"])
        .output()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let dev = line.trim();
            if !dev.is_empty()
                && let Ok(c) = std::fs::canonicalize(dev)
            {
                set.insert(c);
            }
        }
    }
    set
}

/// Filesystem type per kernel device name (empty string when none), from one
/// lsblk call.
fn fstype_map() -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    if let Ok(out) = std::process::Command::new("lsblk")
        .args(["-rno", "NAME,FSTYPE"])
        .output()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let mut f = line.split_whitespace();
            if let Some(name) = f.next() {
                map.insert(name.to_string(), f.next().unwrap_or("").to_string());
            }
        }
    }
    map
}

/// Canonical device paths the operator has explicitly earmarked for TwinStor
/// via a GPT partition label starting with "twinstor". Only these are eligible
/// on the system disk, so its free space is never offered unless labeled
/// (e.g. `parted /dev/<systemdisk> mkpart twinstor-data ...`).
fn twinstor_labeled_parts() -> std::collections::HashSet<PathBuf> {
    let mut set = std::collections::HashSet::new();
    if let Ok(entries) = std::fs::read_dir("/dev/disk/by-partlabel") {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with("twinstor")
                && let Ok(c) = std::fs::canonicalize(e.path())
            {
                set.insert(c);
            }
        }
    }
    set
}

/// Kernel names of disks carrying an XCP-ng/XenServer install, detected by its
/// filesystem-label convention: a `root-<suffix>` partition alongside a
/// `boot-`/`swap-`/`logs-` sibling. Such a disk also carries the reserved
/// backup-root partition (an empty ~18 GB partition, same GPT type and no
/// filesystem, kept for upgrade/restore), which looks like free space, so its
/// partitions are treated like the running system disk's and never offered, leaving the backup partition untouched. Takes `lsblk -P`
/// pairs output (`NAME PKNAME LABEL`) so it is testable off-host.
fn parse_xcp_os_disks(pairs_output: &str) -> std::collections::HashSet<String> {
    let field = |line: &str, key: &str| -> Option<String> {
        let pat = format!("{key}=\"");
        let start = line.find(&pat)? + pat.len();
        let rest = &line[start..];
        rest.find('"').map(|end| rest[..end].to_string())
    };
    let mut by_disk: std::collections::HashMap<String, Vec<String>> = Default::default();
    for line in pairs_output.lines() {
        let Some(pk) = field(line, "PKNAME").filter(|s| !s.is_empty()) else {
            continue;
        };
        by_disk
            .entry(pk)
            .or_default()
            .push(field(line, "LABEL").unwrap_or_default().to_lowercase());
    }
    by_disk
        .into_iter()
        .filter_map(|(disk, labels)| {
            let has_root = labels.iter().any(|l| l.starts_with("root-"));
            let has_sibling = labels.iter().any(|l| {
                l.starts_with("boot-") || l.starts_with("swap-") || l.starts_with("logs-")
            });
            (has_root && has_sibling).then_some(disk)
        })
        .collect()
}

fn xcp_os_disks() -> std::collections::HashSet<String> {
    std::process::Command::new("lsblk")
        .args(["-P", "-n", "-o", "NAME,PKNAME,LABEL"])
        .output()
        .map(|o| parse_xcp_os_disks(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// A local SR occupying a block device, so its disk cannot be offered until the
/// operator frees it. `dev` is the device the SR sits on, `uuid` the SR uuid.
struct SrBlocker {
    dev: String,
    uuid: String,
    name: Option<String>,
}

/// Devices held by a local SR, as (device, sr-uuid): an EXT/file SR is mounted
/// at `/run/sr-mount/<uuid>`, an LVM SR's PV lives in a `VG_XenStorage-<uuid>`
/// volume group. Parses `/proc/mounts` and `pvs -o pv_name,vg_name` text so
/// it is testable off-host.
fn parse_sr_blockers(mounts: &str, pvs: &str) -> Vec<(String, String)> {
    // A device-mapper / multipath node, i.e. an iSCSI/FC-transported SR (the
    // TwinStor SR itself presents this way). Only SRs on a local disk block a
    // local disk, so these are not blockers.
    let is_dm = |dev: &str| {
        dev.starts_with("/dev/mapper/")
            || dev.starts_with("/dev/dm-")
            || dev.contains("/dm-name-")
            || dev.contains("/dm-uuid-")
    };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in mounts.lines() {
        let mut f = line.split_whitespace();
        let (Some(dev), Some(mp)) = (f.next(), f.next()) else {
            continue;
        };
        if let Some(rest) = mp.strip_prefix("/run/sr-mount/")
            && dev.starts_with("/dev/")
            && !is_dm(dev)
            && seen.insert(dev.to_string())
        {
            let uuid = rest.split('/').next().unwrap_or(rest).to_string();
            out.push((dev.to_string(), uuid));
        }
    }
    for line in pvs.lines() {
        let mut f = line.split_whitespace();
        let (Some(pv), Some(vg)) = (f.next(), f.next()) else {
            continue;
        };
        // LVM SRs live in VG_XenStorage-<uuid>; the EXT local SR (the XCP-ng
        // installer's default) in XSLocalEXT-<uuid>, whose filesystem is
        // mounted from a device-mapper node and so never shows above. Missing
        // it hid a tester's three data disks with no word on why.
        if let Some(uuid) = vg
            .strip_prefix("VG_XenStorage-")
            .or_else(|| vg.strip_prefix("XSLocalEXT-"))
            && !is_dm(pv)
            && seen.insert(pv.to_string())
        {
            out.push((pv.to_string(), uuid.to_string()));
        }
    }
    out
}

fn local_sr_blockers() -> Vec<SrBlocker> {
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let pvs = std::process::Command::new("pvs")
        .args(["--noheadings", "-o", "pv_name,vg_name"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    parse_sr_blockers(&mounts, &pvs)
        .into_iter()
        .map(|(dev, uuid)| {
            let name = std::process::Command::new("xe")
                .args([
                    "sr-param-get",
                    &format!("uuid={uuid}"),
                    "param-name=name-label",
                ])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty());
            SrBlocker { dev, uuid, name }
        })
        .collect()
}

/// The local SRs holding disks, grouped: (uuid, name, devices), in first-seen
/// order.
fn group_blockers(blockers: &[SrBlocker]) -> Vec<(String, Option<String>, Vec<String>)> {
    let mut out: Vec<(String, Option<String>, Vec<String>)> = Vec::new();
    for b in blockers {
        match out.iter_mut().find(|(u, _, _)| *u == b.uuid) {
            Some((_, _, devs)) => devs.push(b.dev.clone()),
            None => out.push((b.uuid.clone(), b.name.clone(), vec![b.dev.clone()])),
        }
    }
    out
}

/// What the wizard knows about a local SR before offering to destroy it.
#[derive(Debug, Default)]
struct SrFacts {
    /// XAPI knows this SR (an old volume group from before a reinstall is
    /// not one).
    known: bool,
    shared: bool,
    sr_type: String,
    /// Hosts with a PBD for this SR.
    pbd_hosts: Vec<String>,
    /// None when the list could not be read.
    vdis: Option<usize>,
    /// Files in an EXT SR's mount that are not virtual disks (ISOs,
    /// backups); None when the mount could not be read.
    other_files: Option<Vec<String>>,
    /// A partition of the system disk would have to be labelled to be
    /// offered, and that disk has no GPT label.
    os_disk_not_gpt: bool,
}

/// Why the wizard must not destroy this SR, or None when it may offer to:
/// a local SR (ext or lvm, not shared, attached to this host only) that
/// XAPI knows, holding no virtual disk and, for ext, no other file.
fn destroy_refusal(f: &SrFacts, this_host: &str) -> Option<String> {
    if !f.known {
        return Some("XAPI does not know this SR".into());
    }
    if f.shared || !matches!(f.sr_type.as_str(), "ext" | "lvm") {
        return Some(format!(
            "it is not a local disk SR (type {}, shared {})",
            f.sr_type, f.shared
        ));
    }
    if f.pbd_hosts.iter().any(|h| h != this_host) {
        return Some("another host has it attached".into());
    }
    match f.vdis {
        None => return Some("its virtual disks could not be listed".into()),
        Some(n) if n > 0 => return Some(format!("it holds {n} virtual disk(s)")),
        Some(_) => {}
    }
    if f.sr_type == "ext" {
        match &f.other_files {
            None => return Some("its content could not be read (is it plugged here?)".into()),
            Some(files) if !files.is_empty() => {
                return Some(format!("it holds other files: {}", files.join(", ")));
            }
            Some(_) => {}
        }
    }
    if f.os_disk_not_gpt {
        return Some(
            "it sits on the system disk, which has no GPT label, so the freed partition \
             could not be labelled for TwinStor"
                .into(),
        );
    }
    None
}

async fn xe_param(class: &str, uuid: &str, param: &str) -> Option<String> {
    cmd_output(
        "xe",
        &[
            &format!("{class}-param-get"),
            &format!("uuid={uuid}"),
            &format!("param-name={param}"),
        ],
    )
    .await
    .ok()
    .map(|s| s.trim().to_string())
}

async fn sr_vdi_count(uuid: &str) -> Option<usize> {
    let v = cmd_output("xe", &["vdi-list", &format!("sr-uuid={uuid}"), "--minimal"])
        .await
        .ok()?;
    Some(v.split(',').filter(|s| !s.trim().is_empty()).count())
}

/// The facts behind `destroy_refusal` for one SR occupying `devs`.
async fn sr_facts(uuid: &str, devs: &[String]) -> SrFacts {
    let Some(sr_type) = xe_param("sr", uuid, "type").await else {
        return SrFacts::default();
    };
    let pbd_hosts = cmd_output(
        "xe",
        &[
            "pbd-list",
            &format!("sr-uuid={uuid}"),
            "params=host-uuid",
            "--minimal",
        ],
    )
    .await
    .unwrap_or_default()
    .split(',')
    .map(|h| h.trim().to_string())
    .filter(|h| !h.is_empty())
    .collect();
    let other_files = std::fs::read_dir(format!("/run/sr-mount/{uuid}"))
        .ok()
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| !n.ends_with(".vhd") && n != "lost+found")
                .collect()
        });
    let os = system_disk();
    let xcp = xcp_os_disks();
    let mut os_disk_not_gpt = false;
    for dev in devs {
        let Ok(canon) = std::fs::canonicalize(dev) else {
            continue;
        };
        let kname = canon
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if let Some((disk, _)) = partition_of(&kname)
            && (os.as_deref() == Some(disk.as_str()) || xcp.contains(&disk))
        {
            let pt = cmd_output("lsblk", &["-dno", "PTTYPE", &format!("/dev/{disk}")])
                .await
                .unwrap_or_default();
            os_disk_not_gpt |= pt.trim() != "gpt";
        }
    }
    SrFacts {
        known: true,
        shared: xe_param("sr", uuid, "shared").await.as_deref() == Some("true"),
        sr_type,
        pbd_hosts,
        vdis: sr_vdi_count(uuid).await,
        other_files,
        os_disk_not_gpt,
    }
}

/// Offer to destroy each local SR that holds disks but no virtual disk (the
/// installer's default local SR on the data disks, never used), after a typed
/// confirmation. An SR with virtual disks is never touched: the note that
/// follows says how to free it by hand. `true` when one was destroyed (the
/// caller scans the disks again).
async fn offer_to_free_empty_local_srs(blockers: &[SrBlocker]) -> Result<bool> {
    let Some((this_host, _)) = two_host_pool().await else {
        return Ok(false);
    };
    let mut freed = false;
    for (uuid, name, devs) in group_blockers(blockers) {
        let label = name.map(|n| format!("\"{n}\" ")).unwrap_or_default();
        if let Some(why) = destroy_refusal(&sr_facts(&uuid, &devs).await, &this_host) {
            // An SR holding virtual disks is the note's business; say why
            // any other one is not offered.
            if !why.starts_with("it holds") || why.contains("other files") {
                println!("\nThe SR {label}({uuid}) is not offered for destruction: {why}.");
            }
            continue;
        }
        println!(
            "\nThe local SR {label}({uuid}) holds no virtual disk and occupies {}.",
            devs.iter()
                .map(|d| kernel_dev(d))
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!(
            "It holds nothing else and only this host uses it: destroying it frees these \
             disks for TwinStor."
        );
        let confirm = ask(
            "Type 'destroy' to destroy this SR now (anything else keeps it)",
            None,
        )?;
        if !confirm.eq_ignore_ascii_case("destroy") {
            println!("Kept.");
            continue;
        }
        // The prompt may have waited a long time: a VM may have been created
        // on it since (the local SR is often the pool's default SR).
        if sr_vdi_count(&uuid).await != Some(0) {
            println!("  it holds a virtual disk now; kept.");
            continue;
        }
        let pbd = cmd_output(
            "xe",
            &[
                "pbd-list",
                &format!("sr-uuid={uuid}"),
                &format!("host-uuid={this_host}"),
                "--minimal",
            ],
        )
        .await
        .unwrap_or_default();
        let pbd = pbd.trim();
        if !pbd.is_empty()
            && let Err(e) = run_cmd("xe", &["pbd-unplug", &format!("uuid={pbd}")]).await
        {
            println!("  could not unplug it ({e}); kept. Free it by hand as described below.");
            continue;
        }
        match run_cmd("xe", &["sr-destroy", &format!("uuid={uuid}")]).await {
            Ok(()) => {
                println!("  destroyed.");
                freed = true;
                for dev in &devs {
                    label_freed_os_partition(dev).await;
                }
            }
            Err(e) => {
                // Plug it back so the host is left as it was.
                let replugged = pbd.is_empty()
                    || run_cmd("xe", &["pbd-plug", &format!("uuid={pbd}")])
                        .await
                        .is_ok();
                if replugged {
                    println!(
                        "  could not destroy it ({e}); left as it was. Free it by hand as described below."
                    );
                } else {
                    println!(
                        "  could not destroy it ({e}), and plugging it back failed too: run \
                         xe pbd-plug uuid={pbd}, then free it by hand as described below."
                    );
                }
            }
        }
    }
    if freed {
        let _ = run_cmd("udevadm", &["settle"]).await;
    }
    Ok(freed)
}

/// `/dev/sda4` for a `/dev/disk/by-id/...` alias: the name operators know.
fn kernel_dev(dev: &str) -> String {
    std::fs::canonicalize(dev)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| dev.to_string())
}

/// The parent disk and partition number of a partition, from sysfs
/// (`sda4` -> `("sda", 4)`). None for a whole disk.
fn partition_of(kname: &str) -> Option<(String, u32)> {
    let sys = std::fs::canonicalize(format!("/sys/class/block/{kname}")).ok()?;
    let num = std::fs::read_to_string(sys.join("partition"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let parent = sys.parent()?.file_name()?.to_string_lossy().to_string();
    Some((parent, num))
}

/// A partition of the system disk (or of another disk holding an XCP-ng
/// install) is offered only when labelled for TwinStor. When the wizard has
/// just destroyed the local SR on such a partition, that partition is known to
/// be the SR's and nothing else's, so label it: otherwise the destroy frees
/// nothing the wizard can use (the installer's default layout on a
/// single-disk host).
async fn label_freed_os_partition(dev: &str) {
    let Ok(canon) = std::fs::canonicalize(dev) else {
        return;
    };
    let kname = canon
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let Some((disk, num)) = partition_of(&kname) else {
        return;
    };
    if system_disk().as_deref() != Some(disk.as_str()) && !xcp_os_disks().contains(&disk) {
        return;
    }
    let disk_dev = format!("/dev/{disk}");
    match run_cmd(
        "sgdisk",
        &["-c", &format!("{num}:twinstor-data"), &disk_dev],
    )
    .await
    {
        Ok(()) => {
            let _ = run_cmd("partprobe", &[&disk_dev]).await;
            // partprobe may refuse a disk with mounted partitions: ask udev to
            // re-read this partition so its by-partlabel link appears.
            let _ = run_cmd(
                "udevadm",
                &["trigger", "--action=change", &format!("/dev/{kname}")],
            )
            .await;
            let _ = run_cmd("udevadm", &["settle"]).await;
            println!("  labelled /dev/{kname} (on the system disk) for TwinStor.");
        }
        Err(e) => println!(
            "  /dev/{kname} is on the system disk, so it is offered only once labelled \
             for TwinStor, and labelling failed ({e}). Run\n    \
             sgdisk -c {num}:twinstor-data {disk_dev}\n  then run setup again."
        ),
    }
}

/// Operator guidance explaining why an SR-backed disk is not offered and how to
/// free it, or None when nothing is blocked. Destroy frees the space (on the
/// system disk it must then be labelled); forget alone leaves LVM/filesystem
/// signatures that still block it.
fn sr_blocker_note(blockers: &[SrBlocker]) -> Option<String> {
    if blockers.is_empty() {
        return None;
    }
    let mut s = String::from("Some devices are not offered because a local SR is using them:\n");
    for b in blockers {
        let label = b
            .name
            .as_deref()
            .map(|n| format!(" \"{n}\""))
            .unwrap_or_default();
        s.push_str(&format!(
            "  - {} is used by local SR{} (uuid {})\n",
            kernel_dev(&b.dev),
            label,
            b.uuid
        ));
    }
    s.push_str(
        "To reuse such a disk for TwinStor, free it on this node first:\n\
        \x20 - destroy the SR and its data:  xe sr-destroy uuid=<uuid>\n\
        \x20   (detach it first if needed:   xe pbd-unplug uuid=<pbd>)\n\
        \x20 - or keep the data: xe sr-forget uuid=<uuid>, then wipe the leftover\n\
        \x20   LVM/filesystem signatures (wipefs -a / pvremove) before re-running.\n\
        Destroying frees the space; forgetting alone leaves signatures that still\n\
        block it. A freed partition of the system disk is offered only once\n\
        labelled: sgdisk -c <partition number>:twinstor-data /dev/<disk>",
    );
    Some(s)
}

/// Kernel block-device names that are virtual or frontend layers, never a raw
/// backing target: loop/ram/zram/nbd, the dm/drbd/sr device-mapper stacks, td*
/// XCP-ng tapdisk frontends (live VM disks) and xvd* guest block frontends.
/// Software RAID (md*) is absent on purpose: an idle array is a valid backing
/// device, and its member disks are excluded by the holders check instead.
fn is_virtual_device(name: &str) -> bool {
    const VIRTUAL: [&str; 9] = [
        "loop", "ram", "dm-", "drbd", "sr", "zram", "nbd", "td", "xvd",
    ];
    VIRTUAL.iter().any(|p| name.starts_with(p))
}

/// An NVMe controller path node (`nvme<subsys>c<ctrl>n<ns>`): with native
/// NVMe multipath the kernel lists one per path in `/sys/block`, hidden,
/// beside the namespace head (`nvme0n1`) that carries the partitions and the
/// filesystems. The path node shows no partition of its own and a name that
/// is not the system disk's, so it read as an empty whole disk: a tester's
/// wizard offered the XCP-ng boot disk (a Dell BOSS-N1) this way.
fn is_nvme_path_node(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("nvme") else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let Some((subsys, rest)) = rest.split_once('c') else {
        return false;
    };
    let Some((ctrl, ns)) = rest.split_once('n') else {
        return false;
    };
    digits(subsys) && digits(ctrl) && digits(ns)
}

/// A `/sys/block` entry the kernel hides (`hidden` = 1: an NVMe multipath
/// path node, see `is_nvme_path_node`, which also covers kernels without the
/// attribute). Never a backing target: the device it is a path to is listed
/// under its own name.
/// A disk reached over the network: an iSCSI LUN sits under a `session<N>`
/// node of its sysfs path. TwinStor's own LUN is one, and so is any array's;
/// none is a local backing device. Takes the resolved `/sys/block/<name>`.
fn is_network_disk_path(resolved: &str) -> bool {
    resolved.split('/').any(|c| {
        c.strip_prefix("session")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

fn is_network_disk(sys_path: &std::path::Path) -> bool {
    std::fs::canonicalize(sys_path)
        .map(|p| is_network_disk_path(&p.to_string_lossy()))
        .unwrap_or(false)
}

fn is_hidden_block(name: &str, sys_path: &std::path::Path) -> bool {
    is_nvme_path_node(name)
        || std::fs::read_to_string(sys_path.join("hidden")).is_ok_and(|h| h.trim() == "1")
}

/// Candidate backing targets: whole free disks and free partitions on data
/// disks. Partitions of an OS disk (the running system disk, or a dormant
/// XCP-ng install disk, see `parse_xcp_os_disks`) are excluded, because the
/// backup-root partition looks the same as a free one (same GPT type, no
/// filesystem). The exception is a partition the operator labeled for
/// TwinStor, which is the safe way to use OS-disk free space. Anything
/// mounted, used as swap, an LVM PV (SR or system VG), or with a holder
/// stacked on it is skipped as in use.
fn candidate_disks() -> Vec<CandidateDisk> {
    let system = system_disk();
    let os_disks = xcp_os_disks();
    let ids = by_id_map();
    let busy = busy_sources();
    let pvs = lvm_pv_paths();
    let fstypes = fstype_map();
    let labeled = twinstor_labeled_parts();
    let mut out = Vec::new();

    // Minimum size for a partition to be worth offering (whole disks are always
    // listed). Filters tiny leftover partitions out of the list.
    const MIN_PART_GB: u64 = 1;

    let consider = |name: &str,
                    sys_path: &std::path::Path,
                    model: &str,
                    is_partition: bool,
                    on_system: bool,
                    out: &mut Vec<CandidateDisk>| {
        let dev = PathBuf::from(format!("/dev/{name}"));
        let canon = std::fs::canonicalize(&dev).unwrap_or_else(|_| dev.clone());
        if busy.contains(&canon) || pvs.contains(&canon) {
            return; // mounted, swap, or an LVM PV (SR / system VG)
        }
        let has_holders = std::fs::read_dir(sys_path.join("holders"))
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
        if has_holders {
            return; // DRBD, multipath, dm-crypt, md, ... stacked on it
        }
        let sectors: u64 = std::fs::read_to_string(sys_path.join("size"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        if sectors == 0 {
            return;
        }
        let size_gb = sectors * 512 / 1_000_000_000;
        if is_partition && size_gb < MIN_PART_GB {
            return;
        }
        out.push(CandidateDisk {
            kernel: name.to_string(),
            by_id: ids.get(&canon).cloned(),
            model: model.to_string(),
            size_gb,
            is_partition,
            fstype: fstypes.get(name).cloned().unwrap_or_default(),
            on_system,
        });
    };

    let Ok(entries) = std::fs::read_dir("/sys/block") else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if is_virtual_device(&name)
            || is_hidden_block(&name, &e.path())
            || is_network_disk(&e.path())
        {
            continue;
        }
        // The running system disk or a dormant XCP-ng install disk: its
        // partitions (including the reserved backup-root) are never auto-offered.
        let is_system = Some(&name) == system.as_ref() || os_disks.contains(&name);
        let model = std::fs::read_to_string(e.path().join("device/model"))
            .map(|m| m.trim().to_string())
            .unwrap_or_default();
        // Partitions of this disk are subdirs that carry a `partition` file.
        let parts: Vec<(String, PathBuf)> = std::fs::read_dir(e.path())
            .map(|d| {
                d.flatten()
                    .filter(|c| c.path().join("partition").exists())
                    .map(|c| (c.file_name().to_string_lossy().to_string(), c.path()))
                    .collect()
            })
            .unwrap_or_default();
        if parts.is_empty() {
            // Whole raw disk with no partition table. Never an OS disk (which
            // always has partitions, so this branch is non-system).
            if !is_system {
                consider(&name, &e.path(), &model, false, false, &mut out);
            }
        } else {
            // Offer the free partitions, not the disk as a whole. On the system
            // disk, only a partition the operator explicitly labeled for
            // TwinStor is eligible.
            for (pname, ppath) in parts {
                if is_system {
                    let canon = std::fs::canonicalize(format!("/dev/{pname}")).ok();
                    if !canon.map(|c| labeled.contains(&c)).unwrap_or(false) {
                        continue;
                    }
                }
                consider(&pname, &ppath, &model, true, is_system, &mut out);
            }
        }
    }
    out.sort_by_key(|d| std::cmp::Reverse(d.size_gb));
    out
}

/// What setup knows about a device given with `--backing-device`, to tell
/// why the wizard would not offer it.
#[derive(Debug, Default)]
struct DeviceFacts {
    /// The wizard's list offers it.
    offered: bool,
    /// Mounted, or used as swap.
    busy: bool,
    /// A local SR sits on it (its name, or its uuid).
    sr: Option<String>,
    lvm_pv: bool,
    /// Something is stacked on it (DRBD, multipath, md, device-mapper).
    holders: bool,
    /// A partition of the system disk or of a dormant XCP-ng install disk
    /// that is not labelled for TwinStor.
    on_os_disk: bool,
    /// A whole disk that carries a partition table.
    has_partitions: bool,
}

/// Why setup refuses a device the wizard would not offer, or None when the
/// wizard would offer it. Free of I/O so it can be unit-tested.
fn device_refusal(f: &DeviceFacts) -> Option<String> {
    if f.offered {
        return None;
    }
    Some(if f.busy {
        "it is mounted or used as swap".to_string()
    } else if let Some(sr) = &f.sr {
        format!("it holds the local SR \"{sr}\"")
    } else if f.lvm_pv {
        "it is an LVM physical volume (an SR or a volume group uses it)".to_string()
    } else if f.holders {
        "something is stacked on it (DRBD, multipath, md RAID or LVM)".to_string()
    } else if f.on_os_disk {
        "it is on the system disk (there, only a partition labelled for TwinStor is used)"
            .to_string()
    } else if f.has_partitions {
        "it has partitions: give one of them, or clear the partition table first (wipefs -a)"
            .to_string()
    } else {
        "it is not a free local disk or partition (too small, or not a local block device)"
            .to_string()
    })
}

/// The `/sys/class/block` directory of a canonical `/dev/<name>` path.
fn sys_block(canon: &Path) -> Option<PathBuf> {
    let name = canon.file_name()?.to_string_lossy().to_string();
    Some(PathBuf::from(format!("/sys/class/block/{name}")))
}

/// Why `dev` is not a device the wizard would offer as the backing device,
/// or None when it is. A device that cannot be resolved is left to setup's
/// own existence check.
pub(crate) fn backing_device_refusal(dev: &Path) -> Option<String> {
    let canon = std::fs::canonicalize(dev).ok()?;
    let offered = candidate_disks()
        .iter()
        .any(|d| std::fs::canonicalize(format!("/dev/{}", d.kernel)).is_ok_and(|c| c == canon));
    let sys = sys_block(&canon)?;
    let is_partition = sys.join("partition").exists();
    let has_children = |p: &Path| {
        std::fs::read_dir(p)
            .map(|d| d.flatten().any(|c| c.path().join("partition").exists()))
            .unwrap_or(false)
    };
    // The whole disk a partition belongs to: its parent directory in sysfs.
    let parent = if is_partition {
        std::fs::canonicalize(&sys).ok().and_then(|p| {
            p.parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string())
        })
    } else {
        None
    };
    let os_disks = xcp_os_disks();
    let system = system_disk();
    let facts = DeviceFacts {
        offered,
        busy: busy_sources().contains(&canon),
        sr: local_sr_blockers()
            .into_iter()
            .find(|b| std::fs::canonicalize(&b.dev).is_ok_and(|c| c == canon))
            .map(|b| b.name.unwrap_or(b.uuid)),
        lvm_pv: lvm_pv_paths().contains(&canon),
        holders: std::fs::read_dir(sys.join("holders"))
            .map(|mut d| d.next().is_some())
            .unwrap_or(false),
        on_os_disk: parent
            .as_ref()
            .is_some_and(|p| os_disks.contains(p) || system.as_ref() == Some(p)),
        has_partitions: !is_partition && has_children(&sys),
    };
    device_refusal(&facts)
}

/// "/dev/sdb, 1920 GB, PERC H755N" for the confirmation prompt.
pub(crate) fn describe_device(dev: &Path) -> String {
    let Ok(canon) = std::fs::canonicalize(dev) else {
        return dev.display().to_string();
    };
    let mut parts = vec![canon.display().to_string()];
    if let Some(sys) = sys_block(&canon) {
        if let Some(gb) = std::fs::read_to_string(sys.join("size"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|sectors| sectors * 512 / 1_000_000_000)
        {
            parts.push(format!("{gb} GB"));
        }
        let model_dir = if sys.join("partition").exists() {
            sys.join("../device")
        } else {
            sys.join("device")
        };
        if let Ok(m) = std::fs::read_to_string(model_dir.join("model"))
            && !m.trim().is_empty()
        {
            parts.push(m.trim().to_string());
        }
    }
    parts.join(", ")
}

/// Parse `parted -m <dev> unit B print free` machine output into the
/// unallocated ("free") regions as (start, end, size) byte triples, largest
/// first. Each machine line is `num:start:end:size:fs:name:flags;`; a free
/// region carries `free` in the filesystem field. Partition and device-header
/// lines never match, so they are dropped.
fn parse_free_regions(machine_output: &str) -> Vec<(u64, u64, u64)> {
    let mut regions: Vec<(u64, u64, u64)> = machine_output
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.trim_end_matches(';').split(':').collect();
            if f.len() >= 5 && f[4].starts_with("free") {
                let p = |s: &str| s.trim_end_matches('B').parse::<u64>().ok();
                if let (Some(s), Some(e), Some(sz)) = (p(f[1]), p(f[2]), p(f[3])) {
                    return Some((s, e, sz));
                }
            }
            None
        })
        .collect();
    regions.sort_by_key(|r| std::cmp::Reverse(r.2));
    regions
}

/// The largest unallocated region on the system disk, as
/// (disk, start_byte, end_byte, size_byte), or None when there is no system
/// disk, parted is unavailable, or the biggest gap is too small to bother with.
///
/// This is the single-disk edge install where local storage was skipped at
/// install time: a large unpartitioned tail sits on the system disk but
/// `candidate_disks()` offers nothing (the system disk is never auto-eligible).
/// The wizard offers to carve a backing partition from this region.
fn system_disk_free_region() -> Option<(String, u64, u64, u64)> {
    // Below this a "free" gap is GPT alignment slack, not usable storage.
    const MIN_FREE_GB: u64 = 10;
    let disk = system_disk()?;
    let out = std::process::Command::new("parted")
        .args([
            "-s",
            "-m",
            &format!("/dev/{disk}"),
            "unit",
            "B",
            "print",
            "free",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let (start, end, size) = parse_free_regions(&String::from_utf8_lossy(&out.stdout))
        .into_iter()
        .next()?;
    if size / 1_000_000_000 < MIN_FREE_GB {
        return None;
    }
    Some((disk, start, end, size))
}

/// A network both hosts share, as a candidate for DRBD replication + iSCSI.
struct PeerNetwork {
    name: String,
    vlan: i32, // -1 = untagged
    local_ip: IpAddr,
    peer_ip: IpAddr,
    is_management: bool,
    /// A /30 or /31 netmask: a strong hint this is a point-to-point link (a
    /// likely back-to-back direct link) rather than a switched subnet.
    point_to_point: bool,
}

fn vlan_label(vlan: i32) -> String {
    if vlan < 0 {
        "untagged".to_string()
    } else {
        format!("VLAN {vlan}")
    }
}

/// This host's uuid plus the peer's uuid, only for a pool of exactly two hosts
/// (the supported TwinStor topology).
async fn two_host_pool() -> Option<(String, String)> {
    let my_hostname = hostname::get().ok()?.to_string_lossy().to_string();
    let uuids = cmd_output("xe", &["host-list", "--minimal"]).await.ok()?;
    let mut this_uuid = None;
    let mut peer_uuid = None;
    let mut others = 0;
    for uuid in uuids.split(',').map(str::trim).filter(|u| !u.is_empty()) {
        let name = cmd_output(
            "xe",
            &[
                "host-param-get",
                &format!("uuid={uuid}"),
                "param-name=hostname",
            ],
        )
        .await
        .ok()?;
        if name.trim() == my_hostname {
            this_uuid = Some(uuid.to_string());
        } else {
            others += 1;
            peer_uuid = Some(uuid.to_string());
        }
    }
    match (this_uuid, peer_uuid) {
        (Some(t), Some(p)) if others == 1 => Some((t, p)),
        _ => None,
    }
}

/// The other host's hostname, for messages ("paste the password <name>
/// printed"); None when the pool cannot be read.
async fn peer_host_name() -> Option<String> {
    let (_, peer) = two_host_pool().await?;
    cmd_output(
        "xe",
        &[
            "host-param-get",
            &format!("uuid={peer}"),
            "param-name=hostname",
        ],
    )
    .await
    .ok()
    .map(|n| n.trim().to_string())
    .filter(|n| !n.is_empty())
}

/// Ask for the CHAP password on the second node until the paste is plausible:
/// never empty (Enter would generate a new password that the first node's
/// target refuses at the very end), and equal to the first node's when it
/// published a check value.
fn ask_second_node_chap(peer: &str, check: Option<&str>) -> Result<String> {
    use crate::setup::{ChapVerdict, chap_verdict, normalize_chap_paste};
    loop {
        let raw = ask(
            &format!("CHAP password (paste the one {peer} printed)"),
            None,
        )?;
        let pw = normalize_chap_paste(&raw);
        if pw.is_empty() {
            println!(
                "  Paste the CHAP password that {peer} printed (it is required on this host)."
            );
            continue;
        }
        if chap_verdict(&pw, check) == ChapVerdict::Differs {
            println!(
                "  This is not the password {peer} printed. Paste it again (it is also in\n\
                 \x20 /etc/twinstor/twinstor.toml on {peer}, key chap_password)."
            );
            continue;
        }
        return Ok(pw);
    }
}

/// Whether the peer's DRBD port is already accepting connections, i.e. it ran
/// setup first and is waiting for this node. One short TCP probe on the default
/// DRBD port; any failure (port closed, peer unreachable, custom port) returns
/// false so the wizard proceeds on the normal first-node path. Used only to
/// reassure the operator that this run is the second node.
async fn peer_drbd_listening(peer_ip: IpAddr) -> bool {
    matches!(
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            tokio::net::TcpStream::connect(format!("{peer_ip}:7789")),
        )
        .await,
        Ok(Ok(_))
    )
}

/// Whether `detect_peer_networks` finds the other host on some network.
pub(crate) async fn shares_a_network_with_peer() -> bool {
    !detect_peer_networks().await.is_empty()
}

/// Networks where both hosts have an IPv4 PIF: the candidates that can carry
/// replication and iSCSI. Includes VLAN networks (a dedicated storage VLAN over
/// the same NIC), so the operator can keep DRBD off the management network.
async fn detect_peer_networks() -> Vec<PeerNetwork> {
    let Some((this_uuid, peer_uuid)) = two_host_pool().await else {
        return vec![];
    };
    let mut out = Vec::new();
    let pifs = cmd_output(
        "xe",
        &["pif-list", &format!("host-uuid={this_uuid}"), "--minimal"],
    )
    .await
    .unwrap_or_default();
    for pif in pifs.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        // One param-list call instead of a query per field.
        let p = pif_params(pif).await;
        let Some(local_ip) = p.get("IP").and_then(|s| s.parse::<IpAddr>().ok()) else {
            continue;
        };
        let Some(net) = p.get("network-uuid").filter(|s| !s.is_empty()) else {
            continue;
        };
        // The peer's IP on the same network (skip if it has none configured).
        let peer_pifs = cmd_output(
            "xe",
            &[
                "pif-list",
                &format!("host-uuid={peer_uuid}"),
                &format!("network-uuid={net}"),
                "--minimal",
            ],
        )
        .await
        .unwrap_or_default();
        let mut peer_ip = None;
        for pp in peer_pifs
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            if let Ok(ip) = pif_param(pp, "IP").await.parse::<IpAddr>() {
                peer_ip = Some(ip);
                break;
            }
        }
        let Some(peer_ip) = peer_ip else { continue };
        let name = cmd_output(
            "xe",
            &[
                "network-param-get",
                &format!("uuid={net}"),
                "param-name=name-label",
            ],
        )
        .await
        .unwrap_or_default()
        .trim()
        .to_string();
        out.push(PeerNetwork {
            name,
            vlan: p.get("VLAN").and_then(|s| s.parse().ok()).unwrap_or(-1),
            local_ip,
            peer_ip,
            is_management: p.get("management").map(|s| s == "true").unwrap_or(false),
            point_to_point: p
                .get("netmask")
                .map(|s| is_point_to_point(s))
                .unwrap_or(false),
        });
    }
    // Management first (the safe default), then by name.
    out.sort_by(|a, b| {
        b.is_management
            .cmp(&a.is_management)
            .then(a.name.cmp(&b.name))
    });
    out
}

/// Read one PIF parameter, empty string on any failure. For a single field;
/// use `pif_params` when several fields of the same PIF are needed.
async fn pif_param(pif: &str, key: &str) -> String {
    cmd_output(
        "xe",
        &[
            "pif-param-get",
            &format!("uuid={pif}"),
            &format!("param-name={key}"),
        ],
    )
    .await
    .unwrap_or_default()
    .trim()
    .to_string()
}

/// All parameters of a PIF in one `xe pif-param-list` call.
async fn pif_params(pif: &str) -> std::collections::HashMap<String, String> {
    let out = cmd_output("xe", &["pif-param-list", &format!("uuid={pif}")])
        .await
        .unwrap_or_default();
    parse_pif_param_list(&out)
}

/// Parse `xe *-param-list` output (lines like `    device ( RO): eth0`) into a
/// key->value map. The `): ` after the access marker is the field separator,
/// so values containing `:` (e.g. IPv6) survive intact.
fn parse_pif_param_list(out: &str) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    for line in out.lines() {
        if let Some((left, value)) = line.split_once("): ")
            && let Some((key, _marker)) = left.rsplit_once(" (")
        {
            m.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    m
}

/// A /30 or /31 netmask: effectively a point-to-point link.
fn is_point_to_point(netmask: &str) -> bool {
    matches!(netmask, "255.255.255.252" | "255.255.255.254")
}

/// A NIC of this host as a candidate end of a new direct link.
#[derive(Debug, Clone, PartialEq)]
struct NicChoice {
    device: String,
    speed_mbps: Option<u32>,
    /// Why it cannot be the direct link; None when it can.
    exclusion: Option<String>,
}

/// Why a NIC cannot be offered as an end of a new direct link, from its
/// `xe pif-param-list` fields and the number of VM interfaces on its pool
/// network. On XCP-ng a NIC that carries only VM traffic also has a link and
/// no IP, so "link up, no IP" alone is no evidence of a cable to the peer.
fn direct_link_exclusion(
    p: &std::collections::HashMap<String, String>,
    vifs: usize,
) -> Option<String> {
    let set = |k: &str| {
        p.get(k)
            .is_some_and(|v| !v.is_empty() && v != "<not in database>")
    };
    if let Some(why) = pif_unusable(p) {
        Some(why.to_string())
    } else if set("IP") {
        Some(format!("it already has the address {}", p["IP"]))
    } else if p.get("carrier").is_none_or(|c| c != "true") {
        Some("no link (no cable, or the port at the other end is down)".into())
    } else if set("VLAN-slave-of") {
        Some("it carries VLAN networks".into())
    } else if vifs > 0 {
        Some(format!(
            "its network carries VM traffic ({vifs} virtual interface(s))"
        ))
    } else {
        None
    }
}

/// The link speed the kernel reports for `device`, in Mb/s.
fn nic_speed_mbps(device: &str) -> Option<u32> {
    std::fs::read_to_string(format!("/sys/class/net/{device}/speed"))
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|s| *s > 0)
        .and_then(|s| u32::try_from(s).ok())
}

/// This host's physical NICs other than the management one, each with the
/// reason it cannot be a new direct link, if any. Sorted by device.
async fn direct_link_nics() -> Vec<NicChoice> {
    let Some((this_uuid, _peer)) = two_host_pool().await else {
        return vec![];
    };
    let pifs = cmd_output(
        "xe",
        &["pif-list", &format!("host-uuid={this_uuid}"), "--minimal"],
    )
    .await
    .unwrap_or_default();
    let mut out = Vec::new();
    for pif in pifs.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let p = pif_params(pif).await;
        let device = p.get("device").cloned().unwrap_or_default();
        if device.is_empty()
            || p.get("management").is_some_and(|m| m == "true")
            || p.get("physical").is_none_or(|m| m != "true")
        {
            continue;
        }
        let vifs = match p.get("network-uuid").filter(|n| !n.is_empty()) {
            Some(net) => cmd_output(
                "xe",
                &["vif-list", &format!("network-uuid={net}"), "--minimal"],
            )
            .await
            .unwrap_or_default()
            .split(',')
            .filter(|v| !v.trim().is_empty())
            .count(),
            None => 0,
        };
        out.push(NicChoice {
            speed_mbps: nic_speed_mbps(&device),
            exclusion: direct_link_exclusion(&p, vifs),
            device,
        });
    }
    out.sort_by(|a, b| a.device.cmp(&b.device));
    out.dedup_by(|a, b| a.device == b.device);
    out
}

/// One line of the NIC list: device, speed, and why it is not offered.
fn nic_choice_line(n: &NicChoice) -> String {
    let speed = n
        .speed_mbps
        .map(|s| format!("{s} Mb/s"))
        .unwrap_or_else(|| "speed unknown".into());
    match &n.exclusion {
        None => format!("{:<8} {speed}", n.device),
        Some(why) => format!("{:<8} {speed}, not offered: {why}", n.device),
    }
}

/// What resolving a direct link for arbitrated mode produced.
#[derive(Debug, PartialEq)]
enum DirectLinkOutcome {
    /// A second shared network is already usable; carries the peer's IP on it.
    Ready(IpAddr),
    /// Unconfigured NICs that could be the direct link; the operator picks.
    NeedsConfig { devices: Vec<String> },
    /// Nothing suitable found.
    None,
}

/// A ready peer IP wins; otherwise the NICs that could take a new link (never
/// a guess among them); otherwise nothing. Free of I/O so it can be
/// unit-tested.
fn decide_direct_link(ready_peer_ips: &[IpAddr], nics: &[NicChoice]) -> DirectLinkOutcome {
    let devices: Vec<String> = nics
        .iter()
        .filter(|n| n.exclusion.is_none())
        .map(|n| n.device.clone())
        .collect();
    if let Some(ip) = ready_peer_ips.first() {
        DirectLinkOutcome::Ready(*ip)
    } else if !devices.is_empty() {
        DirectLinkOutcome::NeedsConfig { devices }
    } else {
        DirectLinkOutcome::None
    }
}

const DIRECT_LINK_NETMASK: &str = "255.255.255.252"; // /30 point-to-point

/// Operator instructions to bring up an unconfigured direct link, using the
/// persistent method (`xe pif-reconfigure-ip`, not raw `ip addr`,
/// which XAPI reaps on network events).
fn direct_link_instructions(device: &str) -> String {
    // pif-reconfigure-ip takes uuid= (not device=), so resolve the PIF uuid in
    // the command. Use 10.99.99.1 on the first node and .2 on the second; the
    // peer's NIC name may differ, so each host resolves its own.
    format!(
        "\"{device}\" has a cable/carrier but no IP. Give it a static point-to-point\n\
         address on both hosts (XAPI-managed so it survives reboots), then re-run\n\
         setup and choose arbitrated mode. Use 10.99.99.1 on the first node and\n\
         10.99.99.2 on the second; pick another /30 if that range is already in use.\n\n\
         \x20 # on each host, for its interface on the cable (the device name may\n\
         \x20 # differ between hosts; do not reconfigure the management NIC):\n\
         \x20 PIF=$(xe pif-list device={device} host-uuid=$(xe host-list hostname=$(hostname) --minimal) --minimal)\n\
         \x20 xe pif-reconfigure-ip uuid=$PIF mode=static IP=10.99.99.1 netmask={DIRECT_LINK_NETMASK}\n"
    )
}

/// The other usable host address in the /30 containing `local` (the two usable
/// addresses in each 4-address block are net+1 and net+2). None if `local` is
/// the network or broadcast address of its /30.
fn p2p_peer(local: Ipv4Addr) -> Option<Ipv4Addr> {
    let o = u32::from(local);
    let net = o & !0b11;
    let (h1, h2) = (net + 1, net + 2);
    if o == h1 {
        Some(Ipv4Addr::from(h2))
    } else if o == h2 {
        Some(Ipv4Addr::from(h1))
    } else {
        None
    }
}

/// An existing local address sharing the /30 of `local`: a collision that would
/// make the point-to-point link ambiguous.
fn p2p_collision(existing: &[Ipv4Addr], local: Ipv4Addr) -> Option<Ipv4Addr> {
    let net = u32::from(local) & !0b11;
    existing
        .iter()
        .copied()
        .find(|a| (u32::from(*a) & !0b11) == net)
}

/// This host's configured IPv4 addresses, parsed from `ip -4 -o addr show`.
async fn host_ipv4_addrs() -> Vec<Ipv4Addr> {
    let out = cmd_output("ip", &["-4", "-o", "addr", "show"])
        .await
        .unwrap_or_default();
    let mut v = Vec::new();
    for line in out.lines() {
        for tok in line.split_whitespace() {
            if let Some((ip, _mask)) = tok.split_once('/')
                && let Ok(a) = ip.parse::<Ipv4Addr>()
            {
                v.push(a);
            }
        }
    }
    v
}

/// The PIF uuid for `device` on this host (device names aren't unique across a
/// pool, so the host filter is required).
async fn pif_uuid_for_device(device: &str) -> Option<String> {
    let (this_uuid, _) = two_host_pool().await?;
    let u = cmd_output(
        "xe",
        &[
            "pif-list",
            &format!("host-uuid={this_uuid}"),
            &format!("device={device}"),
            "--minimal",
        ],
    )
    .await
    .ok()?;
    let u = u.trim();
    (!u.is_empty()).then(|| u.to_string())
}

/// `other-config` key the wizard sets on each PIF it configures as an end of
/// the direct link: a link it finds half configured is resumed only when the
/// address is its own, never an operator's routed /30.
const DIRECT_LINK_TAG: &str = "twinstor-direct-link";

/// Why a PIF cannot be an end of the direct link, from its `xe pif-param-list`
/// fields: only an untagged physical NIC outside any bond, and never the
/// management one.
fn pif_unusable(p: &std::collections::HashMap<String, String>) -> Option<&'static str> {
    let is = |k: &str, v: &str| p.get(k).is_some_and(|s| s == v);
    if is("management", "true") {
        Some("it is the management interface")
    } else if !is("physical", "true") {
        Some("it is not a physical NIC (a bond or a VLAN)")
    } else if p.get("VLAN").is_some_and(|v| v != "-1") {
        Some("it is a VLAN")
    } else if p
        .get("bond-slave-of")
        .is_some_and(|b| !b.is_empty() && b != "<not in database>")
    {
        Some("it is part of a bond")
    } else {
        None
    }
}

/// Whether the wizard configured this PIF as a direct-link end.
fn pif_tagged(p: &std::collections::HashMap<String, String>) -> bool {
    p.get("other-config")
        .is_some_and(|o| o.contains(&format!("{DIRECT_LINK_TAG}: true")))
}

/// What to do with the peer's PIF on the direct-link network, given its
/// current state.
#[derive(Debug, PartialEq)]
enum PeerEnd {
    /// It already holds the expected address.
    Done,
    /// No address and a link: configure it.
    Configure,
    /// Something else is there; the operator decides.
    Refuse(String),
}

fn peer_end_action(ip: &str, carrier: bool, unusable: Option<&str>, expected: Ipv4Addr) -> PeerEnd {
    if let Some(why) = unusable {
        PeerEnd::Refuse(why.to_string())
    } else if ip == expected.to_string() {
        PeerEnd::Done
    } else if !ip.is_empty() {
        PeerEnd::Refuse(format!("it already has the address {ip}"))
    } else if !carrier {
        PeerEnd::Refuse("it shows no link (is the cable on another port of the peer?)".into())
    } else {
        PeerEnd::Configure
    }
}

/// The peer's PIF on the same network as this host's `device`, if exactly one.
async fn peer_pif_on_same_network(device: &str) -> Option<String> {
    let (_, peer_host) = two_host_pool().await?;
    let local = pif_uuid_for_device(device).await?;
    let network = pif_params(&local).await.get("network-uuid").cloned()?;
    let pif = cmd_output(
        "xe",
        &[
            "pif-list",
            &format!("host-uuid={peer_host}"),
            &format!("network-uuid={network}"),
            "--minimal",
        ],
    )
    .await
    .ok()?;
    let pif = pif.trim();
    (!pif.is_empty() && !pif.contains(',')).then(|| pif.to_string())
}

/// The manual way to configure the peer's end, for the messages below.
fn peer_end_manual(peer_dl: Ipv4Addr) -> String {
    format!(
        "configure the peer's end of the direct link by hand, on the peer:\n\
         \x20 PIF=$(xe pif-list device=<its direct-link NIC> host-uuid=$(xe host-list hostname=$(hostname) --minimal) --minimal)\n\
         \x20 xe pif-reconfigure-ip uuid=$PIF mode=static IP={peer_dl} netmask={DIRECT_LINK_NETMASK}\n\
         then run setup here again"
    )
}

/// XAPI refuses to change a PIF's address while pool HA is enabled.
async fn refuse_if_pool_ha() -> Result<()> {
    let pool = cmd_output("xe", &["pool-list", "--minimal"])
        .await
        .unwrap_or_default();
    let ha = cmd_output(
        "xe",
        &[
            "pool-param-get",
            &format!("uuid={}", pool.trim()),
            "param-name=ha-enabled",
        ],
    )
    .await
    .unwrap_or_default();
    if ha.trim() == "true" {
        bail!(
            "pool HA is enabled, and XAPI refuses to change network addresses while it is: \
             disable it (xe pool-ha-disable), then run setup again (setup enables it at the end)"
        );
    }
    Ok(())
}

/// Give `pif` the address `ip`/30 and tag it as a direct-link end.
async fn set_link_end(pif: &str, ip: Ipv4Addr) -> Result<()> {
    run_cmd(
        "xe",
        &[
            "pif-reconfigure-ip",
            &format!("uuid={pif}"),
            "mode=static",
            &format!("IP={ip}"),
            &format!("netmask={DIRECT_LINK_NETMASK}"),
        ],
    )
    .await?;
    let _ = run_cmd(
        "xe",
        &[
            "pif-param-set",
            &format!("uuid={pif}"),
            &format!("other-config:{DIRECT_LINK_TAG}=true"),
        ],
    )
    .await;
    Ok(())
}

/// Undo `set_link_end`: no address, no tag.
async fn clear_link_end(pif: &str) -> Result<()> {
    let _ = run_cmd(
        "xe",
        &[
            "pif-param-remove",
            &format!("uuid={pif}"),
            "param-name=other-config",
            &format!("param-key={DIRECT_LINK_TAG}"),
        ],
    )
    .await;
    run_cmd(
        "xe",
        &["pif-reconfigure-ip", &format!("uuid={pif}"), "mode=None"],
    )
    .await
}

async fn ping_once(ip: &str) -> bool {
    tokio::process::Command::new("ping")
        .args(["-c", "1", "-W", "1", ip])
        .output()
        .await
        .is_ok_and(|o| o.status.success())
}

/// Bring up the direct link on `device`: this host's end `local_dl` (set
/// unless `local_is_set`) and the peer's end on its PIF in the same pool
/// network. Both hosts are in one pool, so XAPI reaches the peer's PIF from
/// here. The peer's PIF is checked before anything changes; when the peer
/// does not answer afterwards, every address this run set is removed again.
async fn bring_up_direct_link(device: &str, local_dl: Ipv4Addr, local_is_set: bool) -> Result<()> {
    let peer_dl = p2p_peer(local_dl).expect("a validated /30 address");
    let manual = peer_end_manual(peer_dl);
    refuse_if_pool_ha().await?;
    let Some(peer_pif) = peer_pif_on_same_network(device).await else {
        bail!("the peer has no single NIC on the same pool network as {device}; {manual}");
    };
    let p = pif_params(&peer_pif).await;
    let peer_dev = p.get("device").cloned().unwrap_or_default();
    let action = peer_end_action(
        p.get("IP").map(String::as_str).unwrap_or(""),
        p.get("carrier").is_some_and(|s| s == "true"),
        pif_unusable(&p),
        peer_dl,
    );
    if let PeerEnd::Refuse(why) = &action {
        bail!("the peer's {peer_dev} cannot take {peer_dl}: {why}; {manual}");
    }
    if action == PeerEnd::Configure
        && !ask_yes_no(
            &format!("Configure the peer's {peer_dev} with {peer_dl}/30 too?"),
            true,
        )?
    {
        bail!("{manual}");
    }
    let local_pif = pif_uuid_for_device(device)
        .await
        .ok_or_else(|| anyhow::anyhow!("could not resolve a PIF uuid for {device} on this host"))?;
    if !local_is_set {
        set_link_end(&local_pif, local_dl)
            .await
            .with_context(|| format!("reconfiguring {device} with {local_dl}/30"))?;
        println!("  configured {device} = {local_dl}/30.");
    }
    if action == PeerEnd::Configure {
        if let Err(e) = set_link_end(&peer_pif, peer_dl).await {
            if !local_is_set {
                let _ = clear_link_end(&local_pif).await;
            }
            return Err(e)
                .with_context(|| format!("reconfiguring the peer's {peer_dev} with {peer_dl}/30"));
        }
        println!("  configured the peer's {peer_dev} = {peer_dl}/30.");
    } else {
        println!("  the peer's {peer_dev} already has {peer_dl}.");
    }
    // Both ends apply their address asynchronously.
    let mut answered = false;
    for _ in 0..20 {
        if host_ipv4_addrs().await.contains(&local_dl) && ping_once(&peer_dl.to_string()).await {
            answered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // A ping also answers through a switch. The bridge's forwarding table
    // tells a cable (only the peer behind the port) from a switch port
    // (other devices too); when it cannot be read, the link is taken as is.
    let foreign = if answered {
        foreign_macs_on_link(device, &peer_pif)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    if answered && foreign.is_empty() {
        return Ok(());
    }
    // A wrong pairing (pool networks pair NICs by device name, so the cable
    // may sit on another NIC of the peer), or a switch port: undo what this
    // run set, so a later run does not take it for a working link.
    let mut undone = Vec::new();
    if action == PeerEnd::Configure && clear_link_end(&peer_pif).await.is_ok() {
        undone.push(format!("the peer's {peer_dev}"));
    }
    if !local_is_set && clear_link_end(&local_pif).await.is_ok() {
        undone.push(device.to_string());
    }
    let undone = if undone.is_empty() {
        String::new()
    } else {
        format!(
            " The addresses set on {} were removed again.",
            undone.join(" and ")
        )
    };
    if !foreign.is_empty() {
        bail!(
            "{device} sees {} other device(s) besides the peer: this looks like a switch port, \
             not a cable between the two hosts.{undone} Pick the NIC that is cabled straight to \
             the other host. If {device} really is, configure the link by hand:\n{}",
            foreign.len(),
            direct_link_instructions(device)
        );
    }
    bail!(
        "{peer_dl} does not answer over {device}: the cable may sit on another NIC of the \
         peer than {peer_dev}.{undone} Check the cabling, or {manual}"
    )
}

/// The device of this host's PIF that holds `ip`, if any.
async fn device_with_ip(ip: IpAddr) -> Option<String> {
    let (this_uuid, _) = two_host_pool().await?;
    let pifs = cmd_output(
        "xe",
        &["pif-list", &format!("host-uuid={this_uuid}"), "--minimal"],
    )
    .await
    .ok()?;
    for pif in pifs.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let p = pif_params(pif).await;
        if p.get("IP").is_some_and(|i| *i == ip.to_string()) {
            return p.get("device").cloned().filter(|d| !d.is_empty());
        }
    }
    None
}

/// MACs the bridge learned on `port` other than `allowed`, from
/// `ovs-appctl fdb/show <bridge>` (`port VLAN MAC age` rows).
fn foreign_macs(fdb: &str, port: &str, allowed: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in fdb.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() >= 3
            && f[0] == port
            && !allowed.iter().any(|a| a.eq_ignore_ascii_case(f[2]))
            && !out.iter().any(|m| m.eq_ignore_ascii_case(f[2]))
        {
            out.push(f[2].to_string());
        }
    }
    out
}

/// The devices other than the peer that this host's bridge has seen behind
/// `device`; None when the bridge cannot be read (a Linux-bridge network
/// backend, or the tools fail), so no check is possible.
async fn foreign_macs_on_link(device: &str, peer_pif: &str) -> Option<Vec<String>> {
    let local_pif = pif_uuid_for_device(device).await?;
    let net = pif_params(&local_pif).await.get("network-uuid").cloned()?;
    let bridge = cmd_output(
        "xe",
        &[
            "network-param-get",
            &format!("uuid={net}"),
            "param-name=bridge",
        ],
    )
    .await
    .ok()?;
    let port = cmd_output("ovs-vsctl", &["get", "Interface", device, "ofport"])
        .await
        .ok()?;
    let fdb = cmd_output("ovs-appctl", &["fdb/show", bridge.trim()])
        .await
        .ok()?;
    let peer_mac = pif_params(peer_pif)
        .await
        .get("MAC")
        .cloned()
        .unwrap_or_default();
    Some(foreign_macs(&fdb, port.trim(), &[peer_mac]))
}

/// A direct link this wizard configured on this host (tagged
/// `DIRECT_LINK_TAG`) whose peer end on the same pool network has no address:
/// an earlier run stopped before configuring the peer. Returns the device and
/// this host's address on it.
async fn half_configured_link() -> Option<(String, Ipv4Addr)> {
    let (this_host, _) = two_host_pool().await?;
    let pifs = cmd_output(
        "xe",
        &["pif-list", &format!("host-uuid={this_host}"), "--minimal"],
    )
    .await
    .ok()?;
    for pif in pifs.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let p = pif_params(pif).await;
        let (Some(dev), Some(ip)) = (
            p.get("device"),
            p.get("IP").and_then(|s| s.parse::<Ipv4Addr>().ok()),
        ) else {
            continue;
        };
        if !pif_tagged(&p)
            || pif_unusable(&p).is_some()
            || p.get("netmask").map(String::as_str) != Some(DIRECT_LINK_NETMASK)
            || p2p_peer(ip).is_none()
        {
            continue;
        }
        let Some(peer_pif) = peer_pif_on_same_network(dev).await else {
            continue;
        };
        if pif_params(&peer_pif)
            .await
            .get("IP")
            .is_none_or(|s| s.is_empty())
        {
            return Some((dev.clone(), ip));
        }
    }
    None
}

/// The address this host should take on the direct link: the other half of
/// the /30 the peer's end already holds, if it holds one, else the default.
fn default_local_dl(peer_ip: Option<Ipv4Addr>, peer_mask: Option<&str>) -> Ipv4Addr {
    match (peer_ip, peer_mask) {
        (Some(ip), Some(DIRECT_LINK_NETMASK)) => {
            p2p_peer(ip).unwrap_or(Ipv4Addr::new(10, 99, 99, 1))
        }
        _ => Ipv4Addr::new(10, 99, 99, 1),
    }
}

/// Resolve the direct link for arbitrated mode interactively. Returns the
/// peer's direct-link IP to use, or `None` to fall back to fate-shared, which
/// is offered only when `allow_fate_shared` (the first host choosing its
/// topology; the second host must follow the first).
async fn resolve_direct_link(
    nets: &[PeerNetwork],
    replication_peer_ip: IpAddr,
    peer: &str,
    allow_fate_shared: bool,
) -> Result<Option<IpAddr>> {
    // A shared network whose peer endpoint is not the replication peer is a
    // candidate direct link. Matching on the peer IP (not the network name) is
    // robust when replication was entered as a manual IP, or when two networks
    // share a name-label. Point-to-point links sort first as the best guess.
    let mut ready: Vec<&PeerNetwork> = nets
        .iter()
        .filter(|n| n.peer_ip != replication_peer_ip)
        .collect();
    ready.sort_by_key(|n| std::cmp::Reverse(n.point_to_point));
    let ready_ips: Vec<IpAddr> = ready.iter().map(|n| n.peer_ip).collect();
    if ready.is_empty()
        && let Some((device, local_dl)) = half_configured_link().await
    {
        let peer_dl = p2p_peer(local_dl).expect("checked by half_configured_link");
        println!(
            "\n{device} has {local_dl}/30 here (set by an earlier setup), but {peer}'s end\n\
             of that link has no address yet."
        );
        bring_up_direct_link(&device, local_dl, true).await?;
        return Ok(Some(IpAddr::V4(peer_dl)));
    }
    // Only probe NICs if there's no ready network to offer.
    let nics = if ready.is_empty() {
        direct_link_nics().await
    } else {
        vec![]
    };
    let none_choice = if allow_fate_shared {
        "none of these: use fate-shared (one network) instead"
    } else {
        "none of these: stop setup here"
    };
    let decline = || -> Result<Option<IpAddr>> {
        if allow_fate_shared {
            println!("  using fate-shared mode.");
            Ok(None)
        } else {
            bail!(
                "{peer} runs in arbitrated mode, so this host needs its end of the direct link: \
                 check the cable between the two hosts, then run setup again"
            )
        }
    };

    match decide_direct_link(&ready_ips, &nics) {
        DirectLinkOutcome::Ready(_) => {
            // The operator must confirm which network is the direct link: a
            // switched second path is not a valid direct link (it shares fate
            // with the first), so we can't auto-pick even when there's one.
            println!("Which network is the direct link (a cable between the two hosts, with no");
            println!("switch on it)?");
            for (i, n) in ready.iter().enumerate() {
                let hint = if n.point_to_point {
                    "  (looks point-to-point)"
                } else {
                    ""
                };
                println!(
                    "  {}) {:<20} local {:<15} peer {}{}",
                    i + 1,
                    n.name,
                    n.local_ip,
                    n.peer_ip,
                    hint
                );
            }
            println!("  0) {none_choice}");
            // A default only when the answer is not a guess: one network,
            // addressed as a point-to-point link.
            let default = (ready.len() == 1 && ready[0].point_to_point).then_some("1");
            let chosen = loop {
                match ask("Choice", default)?.parse::<usize>() {
                    Ok(0) => return decline(),
                    Ok(k) if k >= 1 && k <= ready.len() => break ready[k - 1],
                    _ => println!("  pick a number from the list"),
                }
            };
            // The same check as for a link the wizard sets up: only the peer
            // may sit behind this port. The addresses are the operator's, so
            // nothing is undone.
            if let Some(device) = device_with_ip(chosen.local_ip).await
                && let Some(peer_pif) = peer_pif_on_same_network(&device).await
                && let Some(foreign) = foreign_macs_on_link(&device, &peer_pif).await
                && !foreign.is_empty()
            {
                bail!(
                    "{device} sees {} other device(s) besides {peer}: \"{}\" looks like a \
                     switched network, not a cable between the two hosts. Pick the network of \
                     the direct cable, or connect one, then run setup again",
                    foreign.len(),
                    chosen.name
                );
            }
            Ok(Some(chosen.peer_ip))
        }
        DirectLinkOutcome::NeedsConfig { devices } => {
            println!("\nWhich NIC is cabled directly to {peer} (a cable between the two hosts,");
            println!("no switch)? NICs with VM traffic, an address or no link are not offered.");
            for (i, d) in devices.iter().enumerate() {
                let n = nics.iter().find(|n| &n.device == d).expect("listed above");
                println!("  {}) {}", i + 1, nic_choice_line(n));
            }
            println!("  0) {none_choice}");
            for n in nics.iter().filter(|n| n.exclusion.is_some()) {
                println!("     {}", nic_choice_line(n));
            }
            let device = loop {
                match ask("Choice", None)?.parse::<usize>() {
                    Ok(0) => return decline(),
                    Ok(k) if k >= 1 && k <= devices.len() => break devices[k - 1].clone(),
                    _ => println!("  pick a number from the list"),
                }
            };
            println!(
                "This host takes one end of a point-to-point /30 and {peer} the other;\n\
                 setup configures both. Pick another /30 if 10.99.99.0/30 is in use."
            );
            // The peer's end may already hold its half of a /30 (the
            // manual command, an earlier run): offer the other half.
            let peer_now = match peer_pif_on_same_network(&device).await {
                Some(pp) => pif_params(&pp).await,
                None => Default::default(),
            };
            let default_dl = default_local_dl(
                peer_now.get("IP").and_then(|s| s.parse().ok()),
                peer_now.get("netmask").map(String::as_str),
            )
            .to_string();
            let local_dl = loop {
                let Ok(ip) =
                    ask("This host's direct-link IP", Some(&default_dl))?.parse::<Ipv4Addr>()
                else {
                    println!("  not a valid IPv4 address");
                    continue;
                };
                if p2p_peer(ip).is_none() {
                    println!("  {ip} is not a usable /30 host address (use e.g. .1 or .2)");
                    continue;
                }
                if let Some(c) = p2p_collision(&host_ipv4_addrs().await, ip) {
                    println!(
                        "  the /30 around {ip} is already in use on this host ({c}); pick another /30."
                    );
                    continue;
                }
                break ip;
            };
            let peer_dl = p2p_peer(local_dl).expect("validated in the loop above");
            bring_up_direct_link(&device, local_dl, false).await?;
            Ok(Some(IpAddr::V4(peer_dl)))
        }
        DirectLinkOutcome::None => {
            println!("\nNo NIC of this host can take a direct link.");
            if !nics.is_empty() {
                println!("This host's other NICs:");
                for n in &nics {
                    println!("  {}", nic_choice_line(n));
                }
            }
            println!(
                "A direct-link NIC is cabled straight to {peer} (link up on both ends), has no\n\
                 IP yet, and carries no VM network."
            );
            if allow_fate_shared
                && ask_yes_no("Use fate-shared mode (one network) instead?", false)?
            {
                Ok(None)
            } else {
                bail!(
                    "connect the direct link (a cable between the two hosts), then run setup \
                     again and choose arbitrated mode"
                )
            }
        }
    }
}

/// How the second node takes up the first node's direct link.
#[derive(Debug, PartialEq, Eq)]
enum InheritedLink {
    /// The first node's end answers: use it.
    Use(IpAddr),
    /// This host has no address on that link yet (a first node that only
    /// configured its own end): configure this end, never fate-shared.
    ConfigureHere,
    /// This host has its end, and the first node's does not answer.
    NoAnswer(IpAddr),
}

fn inherited_link_action(
    first_end: Option<IpAddr>,
    answers: bool,
    has_local_end: bool,
) -> InheritedLink {
    match first_end {
        Some(ip) if answers => InheritedLink::Use(ip),
        Some(ip) if has_local_end => InheritedLink::NoAnswer(ip),
        _ => InheritedLink::ConfigureHere,
    }
}

/// The second node in arbitrated mode: follow the first node's direct link
/// (it configured both ends) instead of choosing again, and never fall back
/// to fate-shared, which would leave the two hosts on different topologies.
async fn follow_inherited_direct_link(
    nets: &[PeerNetwork],
    peer_ip: IpAddr,
    peer: &str,
    first_end: Option<IpAddr>,
) -> Result<IpAddr> {
    let mut answers = false;
    let mut has_local_end = false;
    if let Some(ip) = first_end {
        println!("Direct link: {peer}'s end is {ip} (set up by {peer}); checking it answers...");
        for _ in 0..5 {
            if ping_once(&ip.to_string()).await {
                answers = true;
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        has_local_end = match ip {
            IpAddr::V4(v4) => p2p_collision(&host_ipv4_addrs().await, v4).is_some(),
            IpAddr::V6(_) => true,
        };
    }
    match inherited_link_action(first_end, answers, has_local_end) {
        InheritedLink::Use(ip) => {
            println!("  it answers.");
            Ok(ip)
        }
        InheritedLink::NoAnswer(ip) => bail!(
            "the direct link to {ip} ({peer}'s end) does not answer from this host: check the \
             cable between the two hosts (both ports show a link), then run setup again"
        ),
        InheritedLink::ConfigureHere => {
            println!("This host has no address on the direct link yet; set up its end now.");
            match resolve_direct_link(nets, peer_ip, peer, false).await? {
                Some(ip) if first_end.is_none_or(|f| f == ip) => Ok(ip),
                Some(ip) => bail!(
                    "the link set up here reaches {ip}, but {peer}'s end of the direct link is \
                     {}: pick the NIC cabled to that end, then run setup again",
                    first_end.map(|f| f.to_string()).unwrap_or_default()
                ),
                None => bail!("{peer} runs in arbitrated mode; this host needs the direct link"),
            }
        }
    }
}

/// What an existing configuration on this host stands for.
#[derive(Debug, PartialEq, Eq)]
enum InstallState {
    /// A live pair: the pool has the TwinStor SR (found by any signal), or
    /// DRBD here holds or replicates a current copy. Setup never wipes it.
    Live,
    /// This host finished its part and waits for the other host: it
    /// published the handoff, and its service runs.
    WaitingForPeer,
    /// The second host's setup stopped after DRBD connected but before the
    /// SR was created: the first host's handoff is still in the pool DB.
    SecondUnfinished,
    /// A setup that stopped part-way, with no sign of a live pair.
    Unfinished,
}

/// Classify an existing configuration. Fails closed: any live signal reads
/// as Live, except the two states where the handoff proves the SR was never
/// created.
fn install_state(
    pair: &crate::setup::PairState,
    own_handoff: bool,
    other_handoff: bool,
    daemon_active: bool,
) -> InstallState {
    if pair.sr_exists {
        InstallState::Live
    } else if own_handoff && daemon_active {
        InstallState::WaitingForPeer
    } else if pair.drbd_live() && other_handoff {
        InstallState::SecondUnfinished
    } else if pair.drbd_live() {
        InstallState::Live
    } else {
        InstallState::Unfinished
    }
}

/// The menu for an existing configuration: its lines and the default
/// choice. Resuming is the default only where nothing live can be lost; a
/// live pair is never offered a re-run.
fn install_menu(state: &InstallState, peer: &str) -> (Vec<String>, &'static str) {
    match state {
        InstallState::Live => (
            vec![
                "  q) quit: leave everything as it is (check the pair with `twinstor status`)"
                    .into(),
                "  u) uninstall: remove TwinStor from this host".into(),
                "  To give this host a new disk, run: twinstor replace-backing <new-device>".into(),
            ],
            "q",
        ),
        InstallState::WaitingForPeer => (
            vec![
                format!("  q) quit: leave this host waiting, and run `twinstor setup` on {peer}"),
                "  u) uninstall: remove what setup did on this host".into(),
            ],
            "q",
        ),
        InstallState::SecondUnfinished => (
            vec![
                "  q) quit: leave everything as it is".into(),
                "  r) run setup again on this host (erases this host's TwinStor disk; no SR \
                 exists yet)"
                    .into(),
                "  u) uninstall: remove what setup did on this host".into(),
            ],
            "q",
        ),
        InstallState::Unfinished => (
            vec![
                "  r) run setup again from the start (recommended; erases this host's TwinStor disk)"
                    .into(),
                "  u) remove what the previous setup did on this host (uninstall)".into(),
                "  q) quit: leave everything as it is".into(),
            ],
            "r",
        ),
    }
}

/// Whether a menu offers the choice `key` ("r", "u").
fn menu_offers(lines: &[String], key: &str) -> bool {
    lines
        .iter()
        .any(|l| l.trim_start().starts_with(&format!("{key})")))
}

/// Show the current installation: persisted settings plus a quick live view,
/// and what it means.
async fn show_current_install(state: &InstallState, peer: &str) {
    match state {
        InstallState::Live => println!("TwinStor is installed on this host.\n"),
        InstallState::WaitingForPeer => {
            println!("TwinStor is set up on this host and waits for {peer}.\n")
        }
        InstallState::SecondUnfinished => println!(
            "The setup of this host stopped before the shared SR was created ({peer}'s\n\
             setup is still waiting for it).\n"
        ),
        InstallState::Unfinished => println!(
            "A previous setup on this host did not finish (there is no TwinStor SR yet).\n"
        ),
    }
    let cfg = crate::config::Config::load(Path::new("/etc/twinstor/twinstor.toml")).ok();
    if let Some(cfg) = &cfg {
        println!(
            "  node:    {} (peer: {} @ {})",
            cfg.node.name, cfg.node.peer, cfg.network.peer_address
        );
        println!("  device:  {}", cfg.storage.backing_device.display());
        println!(
            "  DRBD:    {} port {}",
            cfg.storage.drbd_device.display(),
            cfg.network.drbd_port
        );
        println!("  iSCSI:   {}", cfg.iscsi.target_iqn);
        println!(
            "  CHAP:    {} / ********",
            cfg.iscsi.chap_user.as_deref().unwrap_or("(none)")
        );
    }
    let daemon = cmd_output("systemctl", &["is-active", "twinstor"])
        .await
        .unwrap_or_else(|_| "unknown".into());
    println!("  daemon:  {}", daemon.trim());
    if let Ok(drbd) = cmd_output("drbdadm", &["status", "twinstor-data"]).await {
        let line: String = drbd
            .split_whitespace()
            .take(4)
            .collect::<Vec<_>>()
            .join(" ");
        println!("  status:  {line}");
    }
    // By uuid, then IQN, then name: an SR renamed in Xen Orchestra is still
    // this installation's.
    if let Some(sr) = crate::uninstall::resolve_sr_uuid(cfg.as_ref()).await
        && crate::xapi::sr_uuid_exists(&sr).await
    {
        let name = cmd_output(
            "xe",
            &[
                "sr-param-get",
                &format!("uuid={sr}"),
                "param-name=name-label",
            ],
        )
        .await
        .unwrap_or_default();
        println!("  SR:      {sr} (\"{}\")", name.trim());
    }
    println!();
    if *state == InstallState::WaitingForPeer
        && let Some(pw) = cfg.and_then(|c| c.iscsi.chap_password)
    {
        println!("On {peer}, run `twinstor setup` and paste this CHAP password when asked:");
        println!("\n      CHAP password:  {pw}\n");
    }
}

/// The tiebreaker line of the summary, from what was actually chosen.
fn ping_summary(targets: &[IpAddr], no_tiebreaker: bool) -> String {
    let extra = targets
        .iter()
        .map(|t| t.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    match (no_tiebreaker, extra.is_empty()) {
        (true, true) => "none (no tiebreaker accepted)".to_string(),
        (_, true) => "the default gateway".to_string(),
        (_, false) => format!("the default gateway, and {extra}"),
    }
}

/// The topology question, in the docs' terms: a stacked or MLAG switch pair
/// is one network (fate-shared); arbitrated is for replication on its own
/// network, and needs a direct cable.
const TOPOLOGY_QUESTION: &str = "\
How are the two hosts and the gateway wired?
  1) fate-shared: one network carries everything (one switch, or a stacked or
     MLAG switch pair). A host that loses the other also loses the gateway,
     so it knows it is the one cut off.                          [most setups]
  2) arbitrated: replication has its own network, separate from the gateway
     (storage switches or a storage VLAN). This needs a cable plugged
     directly between the two hosts.
If unsure, choose 1. Details:
https://docs.vates.tech/tech-preview/twinstor/installation/#pick-your-topology
";

/// Run the interactive wizard. Returns `Some(params)` when a (re)install
/// should proceed, `None` when the session was handled in place (status
/// shown, uninstall performed, or the user quit).
pub async fn run() -> Result<Option<SetupParams>> {
    if std::path::Path::new("/etc/twinstor/twinstor.toml").exists() {
        let pair = crate::setup::live_pair_state().await;
        let daemon_active = cmd_output("systemctl", &["is-active", "twinstor"])
            .await
            .is_ok_and(|s| s.trim() == "active");
        let state = install_state(
            &pair,
            crate::setup::own_shared_setup_published().await,
            crate::setup::inherited_shared_setup().await.is_some(),
            daemon_active,
        );
        let peer = peer_host_name()
            .await
            .unwrap_or_else(|| "the other host".to_string());
        show_current_install(&state, &peer).await;
        let (lines, default) = install_menu(&state, &peer);
        for l in &lines {
            println!("{l}");
        }
        let choice = ask("Choice", Some(default))?;
        match choice.to_lowercase().as_str() {
            "u" if menu_offers(&lines, "u") => {
                let keep = ask_yes_no("Keep the backing data (skip deleting the volume)?", false)?;
                // uninstall has its own confirmation prompt. The interactive
                // path never forces past the VM-safety check.
                crate::uninstall::run_uninstall(false, keep, false).await?;
                return Ok(None);
            }
            "r" if menu_offers(&lines, "r") => {
                println!();
                // The menu read "no SR" from lookups that also read a failed
                // or slow XAPI as "none". Stopping this host's target and DRBD
                // is only safe when XAPI positively says no SR exists.
                if !crate::xapi::twinstor_sr_absent_confirmed(crate::setup::TARGET_IQN).await {
                    return Err(crate::render::CliError::new(
                        "XAPI did not confirm that the pool has no TwinStor SR, so setup did \
                         not stop anything on this host; nothing was changed"
                            .to_string(),
                        "check that `xe sr-list` answers on this host, then run setup again",
                    )
                    .into());
                }
                crate::uninstall::release_for_rerun().await;
            }
            _ => {
                println!("nothing changed.");
                return Ok(None);
            }
        }
    }

    print_banner();
    println!("TwinStor interactive setup. Press Enter to accept defaults.\n");
    crate::preflight::run(true).await?;
    let peer_label = peer_host_name()
        .await
        .unwrap_or_else(|| "the other host".to_string());
    println!(
        "This wizard runs on both hosts: here, then on {peer_label}. It erases one disk on\n\
         each host and creates a shared SR \"twinstor\", whose data DRBD (the Linux disk\n\
         replication) keeps identical on both hosts. Nothing is changed until you\n\
         confirm; Ctrl-C quits.\n"
    );

    // 1. The other node and the network that carries replication + iSCSI.
    // TwinStor mirrors storage between the two hosts, so it needs the address of
    // the *other* one. Auto-detect the networks both hosts share, default to
    // management, and let the operator pick a dedicated (e.g. VLAN) network.
    println!("This host pairs with the other XCP-ng host in the pool.");
    let nets = detect_peer_networks().await;
    // If the other node already ran setup it published its cluster-shared
    // choices to the pool DB. When present, this is the second node: inherit
    // those choices (topology, network, ping targets, sync) instead of re-asking
    // them, so the operator only picks this host's own backing device (+ pastes
    // the CHAP password). None = first node / no handoff: ask everything.
    let inherited = crate::setup::inherited_shared_setup().await;

    let (peer_ip, local_ip): (IpAddr, Option<IpAddr>) = if let Some(inh) = &inherited {
        // Match the same replication network the first node chose, keyed by the
        // IP it published, so both ends agree without another prompt.
        if let Some(n) = nets.iter().find(|n| n.peer_ip == inh.peer_ip) {
            println!(
                "The other node is already set up; reaching it at {} on \"{}\".",
                n.peer_ip, n.name
            );
            (n.peer_ip, Some(n.local_ip))
        } else {
            // Published IP matches no detected shared network (topology changed,
            // or toolstack view differs): use it directly, detect our local IP.
            println!(
                "The other node is already set up at {} (network not auto-matched).",
                inh.peer_ip
            );
            (inh.peer_ip, None)
        }
    } else if nets.len() >= 2 {
        println!("Which network should carry the replication (every write is copied to the other");
        println!("host over it) and the storage traffic (iSCSI)?");
        for (i, n) in nets.iter().enumerate() {
            println!(
                "  {}) {:<20} {:<9}  local {:<15} peer {}{}",
                i + 1,
                n.name,
                vlan_label(n.vlan),
                n.local_ip,
                n.peer_ip,
                if n.is_management {
                    "  [management]"
                } else {
                    ""
                }
            );
        }
        let chosen = loop {
            let a = ask("Choice", Some("1"))?; // management sorts first
            match a.parse::<usize>() {
                Ok(k) if k >= 1 && k <= nets.len() => break &nets[k - 1],
                _ => println!("  pick a number from the list"),
            }
        };
        if !chosen.is_management {
            println!("  note: replication will run on a non-management network. When asked");
            println!("        for ping targets below, add one on this network (its gateway or");
            println!("        a switch IP) so a partition of just this network is detected.");
        }
        (chosen.peer_ip, Some(chosen.local_ip))
    } else if let Some(n) = nets.first() {
        println!(
            "Found the other host at {} on \"{}\" ({}).",
            n.peer_ip,
            n.name,
            vlan_label(n.vlan)
        );
        if ask_yes_no("Use this network to reach the other node?", true)? {
            (n.peer_ip, Some(n.local_ip))
        } else {
            (ask_ip("Other node's IP address")?, None)
        }
    } else {
        // No XAPI data (not a 2-host pool, or toolstack down): ask directly.
        println!("Could not detect the other host automatically.");
        (ask_ip("Other node's IP address")?, None)
    };
    println!();

    // Reassure the operator this is the second node: it joins the node that is
    // already up and creates the shared SR, and just needs node A's CHAP
    // password. An inherited handoff is the strong signal; fall back to a live
    // DRBD-port probe when there is none (e.g. the key was already cleared).
    let peer_already_up = inherited.is_some() || peer_drbd_listening(peer_ip).await;
    let peer_name = peer_label;
    if peer_already_up {
        println!("{peer_name} already runs TwinStor and is waiting for this host.");
        println!("This host joins it as the second host and creates the shared SR.");
        if inherited.is_some() {
            println!("Shared settings are inherited from it; you only choose this host's disk");
            println!("and paste the CHAP password {peer_name} printed.");
        } else {
            println!("Have the CHAP password {peer_name} printed ready to paste when asked below.");
        }
        println!();
    }

    // 1b. Topology mode. Inherited from the first node when this is the second
    // node (it must match), otherwise asked: fate-shared (one network, which
    // includes a stacked or MLAG switch pair) is the default; arbitrated is for
    // replication on a network separate from the gateway, plus a direct cable.
    let direct_ip: Option<IpAddr> = if let Some(inh) = &inherited {
        if inh.arbitrated {
            println!(
                "Topology: arbitrated, chosen on {peer_name}: replication uses a direct cable\n\
                 between the two hosts, with the switched network as a fallback."
            );
            Some(follow_inherited_direct_link(&nets, peer_ip, &peer_name, inh.direct_ip).await?)
        } else {
            println!(
                "Topology: fate-shared, chosen on {peer_name}: one network carries everything."
            );
            None
        }
    } else {
        print!("{TOPOLOGY_QUESTION}");
        let mode = ask("Choice", Some("1"))?.trim().to_lowercase();
        if matches!(mode.as_str(), "2" | "a" | "arbitrated") {
            println!("\n{ARBITRATED_MODE_NOTE}\n");
            resolve_direct_link(&nets, peer_ip, &peer_name, true).await?
        } else {
            None
        }
    };
    println!();

    // 2. Backing device: a free whole disk or a free partition (on the system
    // disk, only a TwinStor-labeled one or the carve below). Partition contents
    // are shown so a partition holding data is obvious before it is wiped.
    // There is no "carve from a VG" fallback: it would consume an existing
    // SR's free space.
    let mut disks = candidate_disks();
    // Disks consumed by a local SR are silently excluded above; surface them so
    // "why isn't my big disk offered?" has an answer and a remedy. An empty one
    // (the installer's default local SR, never used) can be destroyed here.
    let mut blockers = local_sr_blockers();
    if offer_to_free_empty_local_srs(&blockers).await? {
        disks = candidate_disks();
        blockers = local_sr_blockers();
    }
    let sr_note = sr_blocker_note(&blockers);
    if disks.is_empty() {
        // No spare disk and no free partition, but the system disk may carry a
        // large unallocated tail (the usual single-disk install where local
        // storage was skipped). Offer to carve the backing partition from it
        // rather than giving up. This is the only place the wizard touches the
        // system disk, so it takes a stricter, typed confirmation than the wipe
        // step, and only ever adds a partition in verified-free space (it never
        // moves or resizes an existing one).
        if let Some((disk, start, end, size)) = system_disk_free_region() {
            let size_gb = size / 1_000_000_000;
            println!("\nNo spare disk or free partition was found, but the system disk");
            println!("/dev/{disk} has {size_gb} GB of unallocated free space, the usual");
            println!("layout when local storage is skipped at install time.");
            println!("\nTwinStor can carve a {size_gb} GB backing partition from it. This puts");
            println!("the replicated storage on the same disk as dom0, which is fine for a");
            println!("single-box edge or demo deployment but has trade-offs to know:");
            println!("  - dom0 and the storage share this disk's I/O and bandwidth");
            println!("  - TwinStor disables the disk's volatile write cache for durability,");
            println!("    which then also applies to dom0's own writes on this disk");
            println!("  - a failure of this disk takes out the OS and the local replica at");
            println!("    once (the peer node still holds a full copy)");
            println!("\nThe partition is created in free space only; existing partitions are");
            println!("not touched. It is wiped on setup like any backing device.");
            let confirm = ask(
                &format!(
                    "\nType 'I confirm' to create a {size_gb} GB partition on /dev/{disk} (anything else skips)"
                ),
                None,
            )?;
            if confirm.eq_ignore_ascii_case("i confirm") {
                println!("\nCreating backing partition on /dev/{disk} ...");
                run_cmd(
                    "parted",
                    &[
                        "-s",
                        "-a",
                        "optimal",
                        &format!("/dev/{disk}"),
                        "mkpart",
                        "twinstor-data",
                        &format!("{start}B"),
                        &format!("{end}B"),
                    ],
                )
                .await
                .context("carving backing partition on the system disk")?;
                // Re-read the table and let udev publish the by-id / by-partlabel
                // aliases before candidate_disks() looks for them.
                let _ = run_cmd("partprobe", &[&format!("/dev/{disk}")]).await;
                let _ = run_cmd("udevadm", &["settle"]).await;
                disks = candidate_disks();
            } else {
                println!("Skipped: no partition created.");
            }
        }
    }
    if disks.is_empty() {
        let hint = sr_note
            .as_deref()
            .map(|n| format!("\n\n{n}"))
            .unwrap_or_default();
        bail!(
            "no free disk or partition found for the backing device. Either:\n\
             - dedicate a spare disk, or create a free partition on a data disk, or\n\
             - to use free space on the system disk, create a partition there and\n\
               label it for TwinStor, e.g.:\n\
               parted /dev/<systemdisk> mkpart twinstor-data <start> <end>\n\
             then re-run setup.{hint}"
        );
    }
    if let Some(note) = &sr_note {
        println!("\n{note}");
    }
    println!("\nBacking device (all data on the chosen target will be erased):");
    for (i, d) in disks.iter().enumerate() {
        let id = d
            .by_id
            .as_ref()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "no stable id, not recommended".into());
        let kind = if d.is_partition { "partition" } else { "disk" };
        let contents = if d.fstype.is_empty() {
            "empty".to_string()
        } else {
            format!("has {}", d.fstype)
        };
        let marker = if d.on_system {
            "  (OS disk, labeled)"
        } else {
            ""
        };
        println!(
            "  {}) /dev/{:<12} {:>6} GB  {:<9} {:<8} {:<20} [{}]{}",
            i + 1,
            d.kernel,
            d.size_gb,
            kind,
            contents,
            d.model,
            id,
            marker
        );
    }
    let default_choice = if disks.len() == 1 {
        Some("1".to_string())
    } else {
        None
    };
    let backing_device: PathBuf = loop {
        let answer = ask("Choice", default_choice.as_deref())?;
        if let Ok(n) = answer.parse::<usize>()
            && n >= 1
            && n <= disks.len()
        {
            let d = &disks[n - 1];
            break d
                .by_id
                .clone()
                .unwrap_or_else(|| PathBuf::from(format!("/dev/{}", d.kernel)));
        }
        println!("  pick a number from the list");
    };

    // 3. Options. Full sync, ping targets and the DRBD port are cluster-shared,
    // so the second node inherits them from the first instead of re-asking; only
    // the CHAP password is always prompted (it is a secret, never published).
    let full_sync = match &inherited {
        Some(inh) => inh.full_sync,
        None => ask_yes_no(
            "\nCopy this host's whole disk to the other host, instead of starting from two\n\
             empty disks? Only when the disks hold data to keep; for fresh disks answer no",
            false,
        )?,
    };
    // When the peer is already up this is the second node: the first node's
    // password is required, and checked against the value it published.
    let chap_password = if peer_already_up {
        Some(ask_second_node_chap(
            &peer_name,
            inherited.as_ref().and_then(|i| i.chap_check.as_deref()),
        )?)
    } else {
        let chap = ask(
            &format!("CHAP password (Enter to generate one; you will paste it on {peer_name})"),
            Some(""),
        )?;
        let chap = crate::setup::normalize_chap_paste(&chap);
        if chap.is_empty() { None } else { Some(chap) }
    };
    let mut allow_no_tiebreaker = false;
    let ping_targets: Vec<IpAddr> = if let Some(inh) = &inherited {
        // Cluster-shared choice made on the first node. If it accepted a
        // no-tiebreaker deployment (no targets and no gateway on this network
        // either), follow it here with a warning instead of failing the
        // second node's setup on a decision already taken.
        if inh.ping_targets.is_empty() && crate::setup::detect_default_gateway().await.is_none() {
            println!(
                "\nWARNING: inheriting a deployment with no partition tiebreaker (no ping\n\
                 targets and no default gateway). Any replication interruption freezes\n\
                 storage until both nodes are back."
            );
            allow_no_tiebreaker = true;
        }
        inh.ping_targets.clone()
    } else {
        let gateway = crate::setup::detect_default_gateway().await;
        match &gateway {
            Some(gw) => println!(
                "\nIf the two hosts lose sight of each other, each pings the gateway ({gw}) to\n\
                 decide which one keeps running (the tiebreaker). You can add other always-on\n\
                 addresses on this network (a switch, a second router). Do not enter the\n\
                 other host's addresses."
            ),
            None => println!(
                "\nWARNING: no default gateway detected. If the two hosts lose sight of each\n\
                 other, each pings an always-on address to decide which one keeps running\n\
                 (the tiebreaker). With none, this host freezes its storage on any\n\
                 replication interruption, including the other host simply dying (a\n\
                 1-host failure becomes a full outage). Do not enter the other host's\n\
                 addresses."
            ),
        }
        // Addresses that prove nothing about this host's reach: the other
        // host's (it may be the one cut off), the direct link's, and this
        // host's own.
        let mut not_targets = host_ipv4_addrs()
            .await
            .into_iter()
            .map(IpAddr::V4)
            .collect::<Vec<_>>();
        not_targets.push(peer_ip);
        not_targets.extend(direct_ip);
        not_targets.extend(nets.iter().map(|n| n.peer_ip));
        let mut targets = Vec::new();
        loop {
            let prompt = if gateway.is_some() {
                "Other always-on addresses, separated by spaces (Enter to skip)"
            } else {
                "Always-on local IPs to probe, space-separated (required here; or type \
                 'no-tiebreaker' to accept freeze-on-disconnect)"
            };
            let ping_raw = ask(prompt, Some(""))?;
            if gateway.is_none() && ping_raw.trim() == "no-tiebreaker" {
                println!(
                    "  Accepted: with no tiebreaker, any replication interruption freezes\n\
                     \x20 this node's storage until the peer returns; HA will not restart\n\
                     \x20 VMs anywhere."
                );
                allow_no_tiebreaker = true;
                break;
            }
            for t in ping_raw.split_whitespace() {
                match t.parse::<IpAddr>() {
                    Ok(ip) if not_targets.contains(&ip) => println!(
                        "  ignoring {ip}: it belongs to one of the two hosts or the direct link, \
                         so it cannot tell which host is cut off"
                    ),
                    Ok(ip) => targets.push(ip),
                    Err(_) => println!("  ignoring invalid IP: {t}"),
                }
            }
            if gateway.is_some() || !targets.is_empty() {
                break;
            }
            println!("  A tiebreaker is required on a network with no default gateway.");
        }
        targets
    };
    // The DRBD replication port is an advanced setting kept out of the interactive
    // flow (override with --drbd-port); the second node inherits the first's.
    let drbd_port: Option<u16> = inherited.as_ref().map(|i| i.drbd_port);

    // 4. Summary + explicit destructive confirmation
    println!("\nSummary:");
    println!("  peer:    {peer_ip}");
    match direct_ip {
        Some(d) => println!("  mode:    arbitrated (direct link to {d})"),
        None => println!("  mode:    fate-shared (one network)"),
    }
    println!("  device:  {} (will be wiped)", backing_device.display());
    println!(
        "  sync:    {}",
        if full_sync {
            "full initial sync"
        } else {
            "skip (fresh devices)"
        }
    );
    println!(
        "  CHAP:    {}",
        if chap_password.is_some() {
            "provided"
        } else {
            "auto-generated"
        }
    );
    println!(
        "  ping:    {}",
        ping_summary(&ping_targets, allow_no_tiebreaker)
    );
    let params = SetupParams {
        peer_ip,
        backing_device: Some(backing_device),
        local_ip,
        drbd_port,
        full_sync,
        chap_password,
        ping_targets,
        direct_ip,
        allow_no_tiebreaker,
        from_wizard: true,
        yes: true,
        force_device: false,
    };
    println!("\nSame setup without questions (for scripts, or to run it yourself):");
    println!("  {}", crate::setup::equivalent_command(&params));
    let confirm = ask("\nType 'yes' to proceed (anything else aborts)", None)?;
    if confirm != "yes" {
        bail!("aborted by user");
    }
    println!();

    Ok(Some(params))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn an_existing_config_is_read_fail_closed() {
        use crate::setup::PairState;
        let sr = PairState {
            sr_exists: true,
            ..Default::default()
        };
        let uptodate = PairState {
            local_uptodate: true,
            ..Default::default()
        };
        let connected = PairState {
            connected: true,
            ..Default::default()
        };
        let nothing = PairState::default();
        // A renamed SR still found by its uuid or IQN reads Live, whatever
        // else is true.
        assert_eq!(install_state(&sr, true, true, true), InstallState::Live);
        assert_eq!(install_state(&sr, false, false, false), InstallState::Live);
        // The first host after its part: handoff published, service up.
        assert_eq!(
            install_state(&uptodate, true, false, true),
            InstallState::WaitingForPeer
        );
        // No SR found, but DRBD holds a current copy here: Live, never
        // Unfinished.
        assert_eq!(
            install_state(&uptodate, false, false, true),
            InstallState::Live
        );
        assert_eq!(
            install_state(&connected, false, false, false),
            InstallState::Live
        );
        assert_eq!(
            install_state(&uptodate, true, false, false),
            InstallState::Live
        );
        // The second host after a failed SR creation: the first host's
        // handoff is still there.
        assert_eq!(
            install_state(&connected, false, true, true),
            InstallState::SecondUnfinished
        );
        // Nothing live: the first host whose window was closed during the
        // wait, or any setup that stopped early.
        assert_eq!(
            install_state(&nothing, true, false, false),
            InstallState::Unfinished
        );
        assert_eq!(
            install_state(&nothing, false, false, true),
            InstallState::Unfinished
        );
    }

    #[test]
    fn a_live_pair_is_never_offered_a_rerun() {
        let (lines, d) = install_menu(&InstallState::Live, "host-a");
        assert_eq!(d, "q");
        assert!(!menu_offers(&lines, "r"), "{lines:?}");
        assert!(menu_offers(&lines, "u"));
        assert!(lines.iter().any(|l| l.contains("replace-backing")));
        let (lines, d) = install_menu(&InstallState::WaitingForPeer, "host-a");
        assert_eq!(d, "q");
        assert!(!menu_offers(&lines, "r"), "{lines:?}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("`twinstor setup` on host-a"))
        );
        let (lines, d) = install_menu(&InstallState::SecondUnfinished, "host-a");
        assert_eq!(d, "q", "a live DRBD never defaults to a re-run");
        assert!(menu_offers(&lines, "r"));
        let (lines, d) = install_menu(&InstallState::Unfinished, "host-a");
        assert_eq!(d, "r");
        assert!(lines[0].contains("run setup again"), "{lines:?}");
        assert!(menu_offers(&lines, "u") && menu_offers(&lines, "q"));
    }

    #[test]
    fn the_second_host_follows_the_first_hosts_direct_link() {
        let first = Some(ip("10.99.99.1"));
        assert_eq!(
            inherited_link_action(first, true, true),
            InheritedLink::Use(ip("10.99.99.1"))
        );
        // The first host configured both ends, but the cable is wrong.
        assert_eq!(
            inherited_link_action(first, false, true),
            InheritedLink::NoAnswer(ip("10.99.99.1"))
        );
        // A first host that configured only its own end.
        assert_eq!(
            inherited_link_action(first, false, false),
            InheritedLink::ConfigureHere
        );
        // A first host that published no address: set up this end, never
        // fall back to fate-shared.
        assert_eq!(
            inherited_link_action(None, false, false),
            InheritedLink::ConfigureHere
        );
    }

    #[test]
    fn the_summary_shows_the_tiebreakers_actually_chosen() {
        assert_eq!(ping_summary(&[], false), "the default gateway");
        assert_eq!(ping_summary(&[], true), "none (no tiebreaker accepted)");
        assert_eq!(
            ping_summary(&[ip("192.0.2.250"), ip("192.0.2.251")], false),
            "the default gateway, and 192.0.2.250, 192.0.2.251"
        );
    }

    #[test]
    fn the_topology_question_matches_the_docs() {
        // A stacked or MLAG pair is one network: fate-shared, the default.
        assert!(TOPOLOGY_QUESTION.contains("MLAG"));
        assert!(!TOPOLOGY_QUESTION.contains("redundant networks"));
        assert!(TOPOLOGY_QUESTION.contains("#pick-your-topology"));
        assert!(TOPOLOGY_QUESTION.lines().all(|l| l.len() <= 80));
    }

    #[test]
    fn a_device_the_wizard_would_not_offer_is_refused_with_the_reason() {
        let offered = DeviceFacts {
            offered: true,
            ..Default::default()
        };
        assert_eq!(device_refusal(&offered), None);
        let why = |f: DeviceFacts| device_refusal(&f).expect("refused");
        assert!(
            why(DeviceFacts {
                busy: true,
                ..Default::default()
            })
            .contains("mounted")
        );
        assert!(
            why(DeviceFacts {
                sr: Some("Local storage".into()),
                lvm_pv: true,
                ..Default::default()
            })
            .contains("\"Local storage\"")
        );
        assert!(
            why(DeviceFacts {
                lvm_pv: true,
                ..Default::default()
            })
            .contains("LVM")
        );
        assert!(
            why(DeviceFacts {
                holders: true,
                ..Default::default()
            })
            .contains("stacked")
        );
        assert!(
            why(DeviceFacts {
                on_os_disk: true,
                ..Default::default()
            })
            .contains("system disk")
        );
        assert!(
            why(DeviceFacts {
                has_partitions: true,
                ..Default::default()
            })
            .contains("wipefs -a")
        );
        assert!(why(DeviceFacts::default()).contains("not a free local disk"));
    }

    #[test]
    fn blockers_group_by_sr() {
        let b = |dev: &str, uuid: &str| SrBlocker {
            dev: dev.into(),
            uuid: uuid.into(),
            name: Some("Local storage".into()),
        };
        let g = group_blockers(&[
            b("/dev/sda", "u1"),
            b("/dev/nvme0n1p3", "u2"),
            b("/dev/sdb", "u1"),
        ]);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].0, "u1");
        assert_eq!(g[0].2, vec!["/dev/sda", "/dev/sdb"]);
        assert_eq!(g[1].2, vec!["/dev/nvme0n1p3"]);
    }

    fn pif(kv: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        kv.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn only_a_free_cabled_nic_can_take_a_new_direct_link() {
        let free = [
            ("device", "eth4"),
            ("physical", "true"),
            ("VLAN", "-1"),
            ("carrier", "true"),
            ("IP", ""),
            ("VLAN-slave-of", ""),
            ("bond-slave-of", "<not in database>"),
        ];
        assert_eq!(direct_link_exclusion(&pif(&free), 0), None);
        let with = |k: &'static str, v: &'static str| {
            let mut p = pif(&free);
            p.insert(k.to_string(), v.to_string());
            p
        };
        // A NIC that only carries VM traffic: link up and no IP as well.
        assert!(
            direct_link_exclusion(&pif(&free), 2)
                .unwrap()
                .contains("VM traffic")
        );
        assert!(
            direct_link_exclusion(&with("carrier", "false"), 0)
                .unwrap()
                .contains("no link")
        );
        assert!(
            direct_link_exclusion(&with("IP", "192.0.2.5"), 0)
                .unwrap()
                .contains("192.0.2.5")
        );
        assert!(
            direct_link_exclusion(&with("VLAN-slave-of", "OpaqueRef:1"), 0)
                .unwrap()
                .contains("VLAN")
        );
        assert!(
            direct_link_exclusion(&with("bond-slave-of", "OpaqueRef:2"), 0)
                .unwrap()
                .contains("bond")
        );
        assert!(direct_link_exclusion(&with("management", "true"), 0).is_some());
    }

    #[test]
    fn nic_lines_show_speed_and_reason() {
        let n = NicChoice {
            device: "eth4".into(),
            speed_mbps: Some(25000),
            exclusion: None,
        };
        assert_eq!(nic_choice_line(&n).trim_end(), "eth4     25000 Mb/s");
        let n = NicChoice {
            device: "eth1".into(),
            speed_mbps: None,
            exclusion: Some("no link".into()),
        };
        assert_eq!(
            nic_choice_line(&n),
            "eth1     speed unknown, not offered: no link"
        );
    }

    #[test]
    fn only_the_peer_may_sit_behind_a_direct_link_port() {
        let fdb = " port  VLAN  MAC                Age\n\
                   \x20   1     0  aa:bb:cc:00:00:01    3\n\
                   \x20   2     0  aa:bb:cc:00:00:02    1\n\
                   \x20   2     0  aa:bb:cc:00:00:03   12\n\
                   LOCAL     0  aa:bb:cc:00:00:09    0\n";
        // Port 1 sees only the peer (case does not matter).
        assert!(foreign_macs(fdb, "1", &["AA:BB:CC:00:00:01".into()]).is_empty());
        // Port 2 sees two devices besides the peer: a switch port.
        assert_eq!(
            foreign_macs(fdb, "2", &["aa:bb:cc:00:00:01".into()]),
            vec!["aa:bb:cc:00:00:02", "aa:bb:cc:00:00:03"]
        );
        assert!(foreign_macs("", "1", &[]).is_empty());
    }

    #[test]
    fn nvme_path_nodes_are_never_candidates() {
        for name in ["nvme0c0n1", "nvme1c2n3", "nvme10c11n12"] {
            assert!(is_nvme_path_node(name), "{name}");
        }
        // Namespace heads and their partitions stay eligible.
        for name in [
            "nvme0n1",
            "nvme0n1p4",
            "nvme12n3",
            "sda",
            "md0",
            "nvme",
            "nvmec0n1",
            "nvme0cn1",
            "nvme0c0n",
        ] {
            assert!(!is_nvme_path_node(name), "{name}");
        }
        // The kernel's hidden flag excludes too, whatever the name.
        let dir = std::env::temp_dir().join(format!("twinstor-hidden-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!is_hidden_block("sdz", &dir), "no attribute: not hidden");
        std::fs::write(dir.join("hidden"), "0\n").unwrap();
        assert!(!is_hidden_block("sdz", &dir));
        std::fs::write(dir.join("hidden"), "1\n").unwrap();
        assert!(is_hidden_block("sdz", &dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn iscsi_luns_are_not_local_disks() {
        // The paths seen on a host during setup: its own SATA disk, and the
        // other host's TwinStor LUN after the iSCSI login.
        assert!(!is_network_disk_path(
            "/sys/devices/pci0000:00/0000:00:17.0/ata2/host1/target1:0:0/1:0:0:0/block/sda"
        ));
        assert!(is_network_disk_path(
            "/sys/devices/platform/host2/session4/target2:0:0/2:0:0:0/block/sdb"
        ));
        assert!(!is_network_disk_path(
            "/sys/devices/virtual/block/sessions/x"
        ));
        assert!(!is_network_disk_path("/sys/devices/session/block/sdc"));
    }

    #[test]
    fn md_arrays_are_not_treated_as_virtual() {
        // Software-RAID arrays must reach the candidate list; their member
        // disks are filtered out separately by the holders check.
        assert!(!is_virtual_device("md0"));
        assert!(!is_virtual_device("md127"));
        // Real backing disks stay eligible.
        assert!(!is_virtual_device("sda"));
        assert!(!is_virtual_device("nvme0n1"));
        // Virtual/frontend layers stay excluded.
        for name in [
            "loop0", "ram0", "dm-3", "drbd0", "sr0", "zram0", "nbd0", "td2", "xvda",
        ] {
            assert!(is_virtual_device(name), "{name} should be virtual");
        }
    }

    #[test]
    fn free_regions_parsed_largest_first() {
        // `parted -m unit B print free` style output: header, partitions, and
        // two free gaps (a tiny pre-first-partition gap and the big tail).
        let out = "BYT;\n\
            /dev/sda:256060514304B:scsi:512:512:gpt:ATA WUXIN G5 256GB:;\n\
            1:17408B:1048575B:1031168B:free;\n\
            1:1048576B:4296015871B:4294967296B:ext3::;\n\
            6:43504369664B:44601933823B:1097564160B:linux-swap(v1)::;\n\
            1:44601933824B:256060514304B:211458580480B:free;\n";
        let regions = parse_free_regions(out);
        // Only the two free gaps, largest first; partitions/header dropped.
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0], (44601933824, 256060514304, 211458580480));
        assert_eq!(regions[1], (17408, 1048575, 1031168));
    }

    #[test]
    fn free_regions_empty_when_none() {
        let out = "BYT;\n\
            /dev/sda:256060514304B:scsi:512:512:gpt:ATA WUXIN G5 256GB:;\n\
            1:1048576B:256060514304B:256059465728B:ext3::;\n";
        assert!(parse_free_regions(out).is_empty());
    }

    fn nic(device: &str, exclusion: Option<&str>) -> NicChoice {
        NicChoice {
            device: device.into(),
            speed_mbps: None,
            exclusion: exclusion.map(String::from),
        }
    }

    #[test]
    fn ready_network_wins_over_unconfigured_nic() {
        let out = decide_direct_link(&[ip("198.51.100.2")], &[nic("eth0", None)]);
        assert_eq!(out, DirectLinkOutcome::Ready(ip("198.51.100.2")));
    }

    #[test]
    fn every_free_nic_is_offered_and_none_is_picked() {
        let out = decide_direct_link(
            &[],
            &[
                nic("eth1", Some("its network carries VM traffic")),
                nic("eth3", None),
                nic("eth4", None),
            ],
        );
        assert_eq!(
            out,
            DirectLinkOutcome::NeedsConfig {
                devices: vec!["eth3".to_string(), "eth4".to_string()]
            }
        );
    }

    #[test]
    fn nothing_found_yields_none() {
        assert_eq!(decide_direct_link(&[], &[]), DirectLinkOutcome::None);
        // Only excluded NICs: nothing to offer either.
        assert_eq!(
            decide_direct_link(&[], &[nic("eth1", Some("no link"))]),
            DirectLinkOutcome::None
        );
    }

    #[test]
    fn peer_end_configured_only_when_blank_and_cabled() {
        let want: Ipv4Addr = "10.99.99.2".parse().unwrap();
        assert_eq!(peer_end_action("", true, None, want), PeerEnd::Configure);
        assert_eq!(
            peer_end_action("10.99.99.2", true, None, want),
            PeerEnd::Done
        );
        assert!(matches!(
            peer_end_action("192.0.2.7", true, None, want),
            PeerEnd::Refuse(_)
        ));
        assert!(matches!(
            peer_end_action("", false, None, want),
            PeerEnd::Refuse(_)
        ));
        // An unusable PIF (management, bond member, VLAN) is never touched,
        // even blank and cabled.
        assert_eq!(
            peer_end_action("", true, Some("it is part of a bond"), want),
            PeerEnd::Refuse("it is part of a bond".into())
        );
    }

    fn pif_map(kv: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        kv.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn only_an_empty_local_sr_of_this_host_may_be_destroyed() {
        let ok = SrFacts {
            known: true,
            shared: false,
            sr_type: "ext".into(),
            pbd_hosts: vec!["me".into()],
            vdis: Some(0),
            other_files: Some(vec![]),
            os_disk_not_gpt: false,
        };
        assert_eq!(destroy_refusal(&ok, "me"), None);
        let lvm = SrFacts {
            sr_type: "lvm".into(),
            other_files: None,
            ..ok
        };
        assert_eq!(
            destroy_refusal(&lvm, "me"),
            None,
            "lvm has no mount to read"
        );
        let ok = SrFacts {
            sr_type: "ext".into(),
            other_files: Some(vec![]),
            ..lvm
        };
        let refused = |f: SrFacts| destroy_refusal(&f, "me").is_some();
        assert!(refused(SrFacts {
            known: false,
            ..Default::default()
        }));
        assert!(refused(SrFacts {
            shared: true,
            ..ok_clone(&ok)
        }));
        assert!(refused(SrFacts {
            sr_type: "lvmoiscsi".into(),
            ..ok_clone(&ok)
        }));
        assert!(refused(SrFacts {
            pbd_hosts: vec!["me".into(), "peer".into()],
            ..ok_clone(&ok)
        }));
        assert!(refused(SrFacts {
            vdis: Some(1),
            ..ok_clone(&ok)
        }));
        assert!(refused(SrFacts {
            vdis: None,
            ..ok_clone(&ok)
        }));
        assert!(refused(SrFacts {
            other_files: Some(vec!["debian.iso".into()]),
            ..ok_clone(&ok)
        }));
        assert!(refused(SrFacts {
            other_files: None,
            ..ok_clone(&ok)
        }));
        assert!(refused(SrFacts {
            os_disk_not_gpt: true,
            ..ok_clone(&ok)
        }));
    }

    fn ok_clone(f: &SrFacts) -> SrFacts {
        SrFacts {
            known: f.known,
            shared: f.shared,
            sr_type: f.sr_type.clone(),
            pbd_hosts: f.pbd_hosts.clone(),
            vdis: f.vdis,
            other_files: f.other_files.clone(),
            os_disk_not_gpt: f.os_disk_not_gpt,
        }
    }

    #[test]
    fn only_a_plain_physical_nic_can_be_a_link_end() {
        let plain = [
            ("physical", "true"),
            ("VLAN", "-1"),
            ("bond-slave-of", "<not in database>"),
            ("management", "false"),
        ];
        assert_eq!(pif_unusable(&pif_map(&plain)), None);
        let with = |k: &str, v: &str| {
            let mut m = pif_map(&plain);
            m.insert(k.into(), v.into());
            pif_unusable(&m)
        };
        assert!(with("management", "true").is_some());
        assert!(
            with("physical", "false").is_some(),
            "a bond master or VLAN PIF"
        );
        assert!(with("VLAN", "20").is_some());
        assert!(with("bond-slave-of", "OpaqueRef:1234").is_some());
        assert!(
            pif_unusable(&pif_map(&[])).is_some(),
            "unreadable: unusable"
        );
    }

    #[test]
    fn half_configured_link_needs_the_wizards_own_tag() {
        assert!(pif_tagged(&pif_map(&[(
            "other-config",
            "twinstor-direct-link: true"
        )])));
        assert!(pif_tagged(&pif_map(&[(
            "other-config",
            "foo: bar; twinstor-direct-link: true"
        )])));
        assert!(!pif_tagged(&pif_map(&[("other-config", "")])));
        assert!(!pif_tagged(&pif_map(&[])));
    }

    #[test]
    fn local_default_completes_the_peers_30() {
        let ip = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        assert_eq!(default_local_dl(None, None), ip("10.99.99.1"));
        assert_eq!(
            default_local_dl(Some(ip("10.99.99.1")), Some("255.255.255.252")),
            ip("10.99.99.2")
        );
        assert_eq!(
            default_local_dl(Some(ip("10.20.0.6")), Some("255.255.255.252")),
            ip("10.20.0.5")
        );
        // Not a /30: the default, and the operator decides.
        assert_eq!(
            default_local_dl(Some(ip("10.20.0.6")), Some("255.255.255.0")),
            ip("10.99.99.1")
        );
    }

    #[test]
    fn instructions_use_uuid_not_device_and_warn() {
        let s = direct_link_instructions("eth0");
        // pif-reconfigure-ip requires uuid=; device= is an error.
        assert!(s.contains("pif-reconfigure-ip uuid=$PIF"));
        assert!(!s.contains("pif-reconfigure-ip device="));
        assert!(s.contains("management NIC")); // warns not to reconfigure it
        assert!(s.contains("10.99.99.1"));
        assert!(s.contains("10.99.99.2"));
        assert!(s.contains(DIRECT_LINK_NETMASK));
    }

    #[test]
    fn p2p_peer_flips_within_the_30() {
        assert_eq!(
            p2p_peer("198.51.100.1".parse().unwrap()),
            Some("198.51.100.2".parse().unwrap())
        );
        assert_eq!(
            p2p_peer("198.51.100.2".parse().unwrap()),
            Some("198.51.100.1".parse().unwrap())
        );
        // A different /30 block (.4-.7): usable hosts are .5 and .6.
        assert_eq!(
            p2p_peer("198.51.100.5".parse().unwrap()),
            Some("198.51.100.6".parse().unwrap())
        );
        // Network (.0) and broadcast (.3) addresses are not usable hosts.
        assert_eq!(p2p_peer("198.51.100.0".parse().unwrap()), None);
        assert_eq!(p2p_peer("198.51.100.3".parse().unwrap()), None);
    }

    #[test]
    fn p2p_collision_detects_same_30_only() {
        let existing: Vec<Ipv4Addr> = ["192.0.2.102", "198.51.100.2"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        // .1 shares the .0/30 with the existing .2, so it collides.
        assert_eq!(
            p2p_collision(&existing, "198.51.100.1".parse().unwrap()),
            Some("198.51.100.2".parse().unwrap())
        );
        // A different /30 (.4-.7) does not collide.
        assert_eq!(
            p2p_collision(&existing, "198.51.100.5".parse().unwrap()),
            None
        );
    }

    #[test]
    fn parses_pif_param_list_output() {
        let out = "\
uuid ( RO)                  : abc-123
                device ( RO): eth0
                    IP ( RO): 198.51.100.1
               netmask ( RO): 255.255.255.252
            management ( RW): false
               carrier ( RO): true";
        let m = parse_pif_param_list(out);
        assert_eq!(m.get("device").map(String::as_str), Some("eth0"));
        assert_eq!(m.get("IP").map(String::as_str), Some("198.51.100.1"));
        assert_eq!(m.get("management").map(String::as_str), Some("false"));
        assert_eq!(m.get("carrier").map(String::as_str), Some("true"));
        assert!(is_point_to_point(m.get("netmask").unwrap()));
    }

    #[test]
    fn point_to_point_only_for_30_and_31() {
        assert!(is_point_to_point("255.255.255.252"));
        assert!(is_point_to_point("255.255.255.254"));
        assert!(!is_point_to_point("255.255.255.0"));
    }

    #[test]
    fn xcp_install_disk_detected_by_label_convention() {
        // Real XCP-ng 8.3 layout: root-/logs-/swap-/BOOT- labels share a suffix;
        // the backup-root partition (sda2) is empty (no label). sdb is a plain
        // data disk that must not be mistaken for an OS disk.
        let out = "\
NAME=\"sda\" PKNAME=\"\" LABEL=\"\"\n\
NAME=\"sda1\" PKNAME=\"sda\" LABEL=\"root-efqgbm\"\n\
NAME=\"sda2\" PKNAME=\"sda\" LABEL=\"\"\n\
NAME=\"sda3\" PKNAME=\"sda\" LABEL=\"BOOT-EFQGBM\"\n\
NAME=\"sda5\" PKNAME=\"sda\" LABEL=\"logs-efqgbm\"\n\
NAME=\"sda6\" PKNAME=\"sda\" LABEL=\"swap-efqgbm\"\n\
NAME=\"sdb\" PKNAME=\"\" LABEL=\"\"\n\
NAME=\"sdb1\" PKNAME=\"sdb\" LABEL=\"my-data\"\n";
        let disks = parse_xcp_os_disks(out);
        assert!(disks.contains("sda"), "sda is an XCP-ng install disk");
        assert!(!disks.contains("sdb"), "sdb is a plain data disk");
    }

    #[test]
    fn root_label_alone_is_not_an_os_disk() {
        // A lone root- label (no boot/swap/logs sibling) is too weak a signal;
        // require the sibling to avoid excluding a user's data partition.
        let out = "\
NAME=\"sdc1\" PKNAME=\"sdc\" LABEL=\"root-of-my-stuff\"\n";
        assert!(parse_xcp_os_disks(out).is_empty());
    }

    #[test]
    fn sr_blockers_from_ext_mount_and_lvm_vg() {
        // EXT/file SR mounted under /run/sr-mount/<uuid>; LVM SR PV in a
        // VG_XenStorage-<uuid> volume group. Non-SR mounts/VGs are ignored.
        let mounts = "\
/dev/sda1 / ext3 rw 0 0\n\
/dev/nvme0n1p3 /run/sr-mount/1a2b3c4d-0000-1111-2222-333344445555 ext4 rw 0 0\n\
tmpfs /dev/shm tmpfs rw 0 0\n";
        let pvs = "\
  /dev/nvme1n1  VG_XenStorage-9f9f9f9f-aaaa-bbbb-cccc-ddddeeeeffff\n\
  /dev/sda4     twinstor-vg\n";
        let got = parse_sr_blockers(mounts, pvs);
        assert_eq!(got.len(), 2);
        // The installer's EXT local SR over several disks: its mount is a
        // device-mapper node (skipped), its PVs carry the SR uuid.
        let ext_mounts = "/dev/mapper/XSLocalEXT--b7a2eef2--fdef--c3d5--10ba--dcf0e4c440e4-b7a2eef2--fdef--c3d5--10ba--dcf0e4c440e4 /run/sr-mount/b7a2eef2-fdef-c3d5-10ba-dcf0e4c440e4 ext3 rw 0 0\n";
        let ext_pvs = "\
  /dev/sda  XSLocalEXT-b7a2eef2-fdef-c3d5-10ba-dcf0e4c440e4\n\
  /dev/sdb  XSLocalEXT-b7a2eef2-fdef-c3d5-10ba-dcf0e4c440e4\n\
  /dev/sdc  XSLocalEXT-b7a2eef2-fdef-c3d5-10ba-dcf0e4c440e4\n";
        let ext = parse_sr_blockers(ext_mounts, ext_pvs);
        assert_eq!(
            ext.iter().map(|(d, _)| d.as_str()).collect::<Vec<_>>(),
            ["/dev/sda", "/dev/sdb", "/dev/sdc"]
        );
        assert!(
            ext.iter()
                .all(|(_, u)| u == "b7a2eef2-fdef-c3d5-10ba-dcf0e4c440e4")
        );
        assert_eq!(
            got[0],
            (
                "/dev/nvme0n1p3".into(),
                "1a2b3c4d-0000-1111-2222-333344445555".into()
            )
        );
        assert_eq!(
            got[1],
            (
                "/dev/nvme1n1".into(),
                "9f9f9f9f-aaaa-bbbb-cccc-ddddeeeeffff".into()
            )
        );
    }

    #[test]
    fn iscsi_multipath_sr_is_not_a_local_blocker() {
        // The TwinStor SR (lvmoiscsi) reports its PV as a dm-multipath node;
        // it must not be flagged as a local SR to destroy.
        let pvs = "  /dev/disk/by-id/dm-name-36001405daa0000000000000000000000  VG_XenStorage-5e5e0005-0000-4000-8000-000000000005\n";
        assert!(parse_sr_blockers("", pvs).is_empty());
    }

    #[test]
    fn sr_blocker_note_answers_destroy_vs_forget() {
        let blockers = vec![SrBlocker {
            dev: "/dev/nvme0n1".into(),
            uuid: "abc".into(),
            name: Some("Local storage".into()),
        }];
        let note = sr_blocker_note(&blockers).unwrap();
        assert!(note.contains("/dev/nvme0n1"));
        assert!(note.contains("Local storage"));
        assert!(note.contains("sr-destroy"));
        assert!(note.contains("sr-forget"));
        assert!(sr_blocker_note(&[]).is_none());
    }
}
