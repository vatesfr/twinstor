//! Interactive setup wizard, run when `twinstor setup` is invoked with no
//! arguments on a terminal. Every prompt has an Enter-through default so the
//! fast path is a handful of keystrokes, and the destructive step (wiping the
//! backing device) always requires an explicit confirmation that shows the
//! device's model, serial and size.

use std::io::{BufRead, IsTerminal, Write};
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
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
fn ask(question: &str, default: Option<&str>) -> Result<String> {
    match default {
        Some(d) if !d.is_empty() => print!("{question} [{d}]: "),
        _ => print!("{question}: "),
    }
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("reading input")?;
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
        let Ok(vdis) =
            cmd_output("xe", &["vdi-list", &format!("sr-uuid={uuid}"), "--minimal"]).await
        else {
            continue;
        };
        if !vdis.trim().is_empty() {
            continue;
        }
        let label = name.map(|n| format!("\"{n}\" ")).unwrap_or_default();
        println!(
            "\nThe local SR {label}({uuid}) holds no virtual disk and occupies {}.",
            devs.join(", ")
        );
        println!("Destroying it frees these disks for TwinStor. Nothing on it is in use.");
        let confirm = ask(
            "Type 'destroy' to destroy this SR now (anything else keeps it)",
            None,
        )?;
        if !confirm.eq_ignore_ascii_case("destroy") {
            println!("Kept.");
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
            }
            Err(e) => {
                // Plug it back so the host is left as it was.
                if !pbd.is_empty() {
                    let _ = run_cmd("xe", &["pbd-plug", &format!("uuid={pbd}")]).await;
                }
                println!(
                    "  could not destroy it ({e}); left as it was. Free it by hand as described below."
                );
            }
        }
    }
    if freed {
        let _ = run_cmd("udevadm", &["settle"]).await;
    }
    Ok(freed)
}

/// Operator guidance explaining why an SR-backed disk is not offered and how to
/// free it, or None when nothing is blocked. Destroy frees the space (on the
/// system disk it then becomes carveable); forget alone leaves LVM/filesystem
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
            b.dev, label, b.uuid
        ));
    }
    s.push_str(
        "To reuse such a disk for TwinStor, free it on this node first:\n\
        \x20 - destroy the SR and its data:  xe sr-destroy uuid=<uuid>\n\
        \x20   (detach it first if needed:   xe pbd-unplug uuid=<pbd>)\n\
        \x20 - or keep the data: xe sr-forget uuid=<uuid>, then wipe the leftover\n\
        \x20   LVM/filesystem signatures (wipefs -a / pvremove) before re-running.\n\
        Destroying frees the space; forgetting alone leaves signatures that still\n\
        block it.",
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
        if is_virtual_device(&name) || is_hidden_block(&name, &e.path()) {
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

/// One line per non-management NIC of this host: device, link, IP. Shown
/// when no direct link is found, so a back-to-back cable without a link (a
/// DAC whose port is down, a cable in the wrong port) is visible at once.
async fn nic_link_lines() -> Vec<String> {
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
        if let Some(l) = nic_link_line(&p) {
            out.push(l);
        }
    }
    out.sort();
    out
}

fn nic_link_line(p: &std::collections::HashMap<String, String>) -> Option<String> {
    let device = p.get("device").filter(|d| !d.is_empty())?;
    if p.get("management").is_some_and(|m| m == "true")
        || p.get("physical").is_some_and(|m| m == "false")
    {
        return None;
    }
    let link = if p.get("carrier").is_some_and(|c| c == "true") {
        "link up"
    } else {
        "no link"
    };
    let ip = p
        .get("IP")
        .filter(|i| !i.is_empty())
        .map(|i| format!(", IP {i}"))
        .unwrap_or_else(|| ", no IP".into());
    Some(format!("{device}: {link}{ip}"))
}

/// This host's NICs that have a cable/carrier but no IP and aren't the
/// management interface: candidates for an as-yet-unconfigured direct link.
async fn carrier_nics_without_ip() -> Vec<String> {
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
        let has_ip = p.get("IP").map(|s| !s.is_empty()).unwrap_or(false);
        let carrier = p.get("carrier").map(|s| s == "true").unwrap_or(false);
        let mgmt = p.get("management").map(|s| s == "true").unwrap_or(false);
        if device.is_empty() || has_ip || !carrier || mgmt {
            continue;
        }
        out.push(device);
    }
    out.sort();
    out.dedup();
    out
}

/// What resolving a direct link for arbitrated mode produced.
#[derive(Debug, PartialEq)]
enum DirectLinkOutcome {
    /// A second shared network is already usable; carries the peer's IP on it.
    Ready(IpAddr),
    /// A cabled-but-unconfigured NIC looks like the intended direct link.
    NeedsConfig { device: String },
    /// Nothing suitable found.
    None,
}

/// A ready peer IP wins; otherwise a cabled NIC to configure; otherwise
/// nothing. Free of I/O so it can be unit-tested.
fn decide_direct_link(ready_peer_ips: &[IpAddr], carrier_no_ip: &[String]) -> DirectLinkOutcome {
    if let Some(ip) = ready_peer_ips.first() {
        DirectLinkOutcome::Ready(*ip)
    } else if let Some(dev) = carrier_no_ip.first() {
        DirectLinkOutcome::NeedsConfig {
            device: dev.clone(),
        }
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

fn peer_end_action(ip: &str, carrier: bool, management: bool, expected: Ipv4Addr) -> PeerEnd {
    if management {
        PeerEnd::Refuse("it is the peer's management interface".into())
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

/// Give the peer's end of the direct link `peer_dl`/30, on the peer's PIF in
/// the same network as `device` here. Both hosts are in one pool, so XAPI
/// reaches the peer's PIF from here. Without this, the first node's setup
/// pinged an address nobody had configured yet and always stopped.
async fn configure_peer_end(device: &str, peer_dl: Ipv4Addr) -> Result<()> {
    let manual = format!(
        "configure the peer's end of the direct link by hand, on the peer:\n\
         \x20 PIF=$(xe pif-list device=<its direct-link NIC> host-uuid=$(xe host-list hostname=$(hostname) --minimal) --minimal)\n\
         \x20 xe pif-reconfigure-ip uuid=$PIF mode=static IP={peer_dl} netmask={DIRECT_LINK_NETMASK}\n\
         then run setup here again"
    );
    let Some(peer_pif) = peer_pif_on_same_network(device).await else {
        bail!("the peer has no single NIC on the same network as {device}; {manual}");
    };
    let p = pif_params(&peer_pif).await;
    let peer_dev = p.get("device").cloned().unwrap_or_default();
    let action = peer_end_action(
        p.get("IP").map(String::as_str).unwrap_or(""),
        p.get("carrier").is_some_and(|s| s == "true"),
        p.get("management").is_some_and(|s| s == "true"),
        peer_dl,
    );
    match action {
        PeerEnd::Done => println!("  the peer's {peer_dev} already has {peer_dl}."),
        PeerEnd::Refuse(why) => {
            bail!("the peer's {peer_dev} cannot take {peer_dl}: {why}; {manual}")
        }
        PeerEnd::Configure => {
            if !ask_yes_no(
                &format!("Configure the peer's {peer_dev} with {peer_dl}/30 too?"),
                true,
            )? {
                bail!("{manual}");
            }
            run_cmd(
                "xe",
                &[
                    "pif-reconfigure-ip",
                    &format!("uuid={peer_pif}"),
                    "mode=static",
                    &format!("IP={peer_dl}"),
                    &format!("netmask={DIRECT_LINK_NETMASK}"),
                ],
            )
            .await
            .with_context(|| format!("reconfiguring the peer's {peer_dev} with {peer_dl}/30"))?;
            println!("  configured the peer's {peer_dev} = {peer_dl}/30.");
        }
    }
    // The peer applies the address on its side; give it a moment before the
    // setup's own reachability check.
    for _ in 0..20 {
        if ping_once(&peer_dl.to_string()).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!(
        "{peer_dl} does not answer over {device}: the cable may sit on another NIC of the \
         peer than {peer_dev}. Check the cabling, or {manual}"
    )
}

async fn ping_once(ip: &str) -> bool {
    tokio::process::Command::new("ping")
        .args(["-c", "1", "-W", "1", ip])
        .output()
        .await
        .is_ok_and(|o| o.status.success())
}

/// A non-management PIF of this host with a /30 address whose peer PIF on the
/// same network has none: the direct link of an earlier setup run that
/// stopped before the peer's end was configured. Returns the device and this
/// host's address on it.
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
        if p.get("management").is_some_and(|s| s == "true")
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

/// Configure this host's end of the direct link with `local_dl`/30 via the
/// persistent XAPI method, then confirm the address comes up. The peer's end
/// is `configure_peer_end`.
async fn configure_direct_link(device: &str, local_dl: Ipv4Addr) -> Result<()> {
    let uuid = pif_uuid_for_device(device)
        .await
        .ok_or_else(|| anyhow::anyhow!("could not resolve a PIF uuid for {device} on this host"))?;
    run_cmd(
        "xe",
        &[
            "pif-reconfigure-ip",
            &format!("uuid={uuid}"),
            "mode=static",
            &format!("IP={local_dl}"),
            &format!("netmask={DIRECT_LINK_NETMASK}"),
        ],
    )
    .await
    .with_context(|| format!("reconfiguring {device} with {local_dl}/30"))?;
    // XAPI applies this synchronously, but poll briefly to be sure the address
    // is up before run_setup's route check relies on it.
    for _ in 0..10 {
        if host_ipv4_addrs().await.contains(&local_dl) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("{device} did not come up with {local_dl}/30 after reconfigure");
}

/// Resolve the direct link for arbitrated mode interactively. Returns the
/// peer's direct-link IP to use, or `None` to fall back to fate-shared.
async fn resolve_direct_link(
    nets: &[PeerNetwork],
    replication_peer_ip: IpAddr,
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
            "\n{device} has {local_dl}/30 here, but the peer's end of that link has no address\n\
             yet (an earlier setup stopped before configuring it)."
        );
        configure_peer_end(&device, peer_dl).await?;
        return Ok(Some(IpAddr::V4(peer_dl)));
    }
    // Only probe NICs if there's no ready network to offer.
    let carrier = if ready.is_empty() {
        carrier_nics_without_ip().await
    } else {
        vec![]
    };

    match decide_direct_link(&ready_ips, &carrier) {
        DirectLinkOutcome::Ready(_) => {
            // The operator must confirm which network is the direct link: a
            // switched second path is not a valid direct link (it shares fate
            // with the first), so we can't auto-pick even when there's one.
            println!("Which network is the back-to-back direct link (a host-to-host");
            println!("cable, not a switched network)?");
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
            println!("  0) none of these / use fate-shared instead");
            let chosen = loop {
                match ask("Choice", Some("1"))?.parse::<usize>() {
                    Ok(0) => {
                        println!("  using fate-shared mode.");
                        return Ok(None);
                    }
                    Ok(k) if k >= 1 && k <= ready.len() => break ready[k - 1],
                    _ => println!("  pick a number from the list"),
                }
            };
            Ok(Some(chosen.peer_ip))
        }
        DirectLinkOutcome::NeedsConfig { device } => {
            println!("\nFound a cabled NIC with no IP: {device} (a likely direct link).");
            if ask_yes_no(&format!("Configure {device} as the direct link now?"), true)? {
                println!(
                    "This host takes one end of a point-to-point /30 and the peer the other;\n\
                     setup configures both. Pick another /30 if 10.99.99.0/30 is in use."
                );
                let local_dl = loop {
                    let Ok(ip) =
                        ask("This host's direct-link IP", Some("10.99.99.1"))?.parse::<Ipv4Addr>()
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
                configure_direct_link(&device, local_dl).await?;
                println!("  configured {device} = {local_dl}/30; the peer's end is {peer_dl}.");
                configure_peer_end(&device, peer_dl).await?;
                Ok(Some(IpAddr::V4(peer_dl)))
            } else {
                println!("\n{}", direct_link_instructions(&device));
                if ask_yes_no("Continue in fate-shared mode for now instead?", true)? {
                    Ok(None)
                } else {
                    bail!(
                        "configure the direct link, then re-run setup and choose arbitrated mode"
                    );
                }
            }
        }
        DirectLinkOutcome::None => {
            println!("\nNo second network or spare cabled NIC found for a direct link.");
            let nics = nic_link_lines().await;
            if !nics.is_empty() {
                println!("This host's other NICs:");
                for l in &nics {
                    println!("  {l}");
                }
                println!(
                    "A direct-link NIC must show a link (cable plugged in, the other host powered\n\
                     on, the port up on both ends) and have no IP yet."
                );
            }
            if ask_yes_no(
                "Arbitrated mode needs a dedicated back-to-back link. Continue in fate-shared mode instead?",
                true,
            )? {
                Ok(None)
            } else {
                bail!("connect the direct link, then re-run setup and choose arbitrated mode")
            }
        }
    }
}

/// Show the current installation: persisted settings plus a quick live view.
async fn show_current_install() {
    println!("TwinStor is already installed on this node.\n");
    if let Ok(cfg) =
        crate::config::Config::load(std::path::Path::new("/etc/twinstor/twinstor.toml"))
    {
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
    if let Ok(sr) = cmd_output("xe", &["sr-list", "name-label=twinstor", "--minimal"]).await
        && !sr.trim().is_empty()
    {
        println!("  SR:      {}", sr.trim());
    }
    println!();
}

/// Run the interactive wizard. Returns `Some(params)` when a (re)install
/// should proceed, `None` when the session was handled in place (status
/// shown, uninstall performed, or the user quit).
pub async fn run() -> Result<Option<SetupParams>> {
    if std::path::Path::new("/etc/twinstor/twinstor.toml").exists() {
        show_current_install().await;
        println!("  q) quit: leave everything as it is");
        println!("  u) uninstall: remove TwinStor from this node");
        println!(
            "  r) reconfigure: uninstall is not run; setup re-runs and wipes the backing device"
        );
        let choice = ask("Choice", Some("q"))?;
        match choice.to_lowercase().as_str() {
            "u" => {
                let keep = ask_yes_no("Keep the backing data (skip deleting the volume)?", false)?;
                // uninstall has its own confirmation prompt. The interactive
                // path never forces past the VM-safety check.
                crate::uninstall::run_uninstall(false, keep, false).await?;
                return Ok(None);
            }
            "r" => {
                println!();
                // fall through into the normal wizard below
            }
            _ => {
                println!("nothing changed.");
                return Ok(None);
            }
        }
    }

    print_banner();
    println!("TwinStor interactive setup. Press Enter to accept defaults.\n");
    crate::preflight::run().await?;

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
        println!("Which network should carry DRBD replication and iSCSI?");
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
    if peer_already_up {
        println!("The other node is already running TwinStor and waiting for this one.");
        println!("This node joins it as the second node and creates the shared SR.");
        if inherited.is_some() {
            println!("Shared settings are inherited from it; you only choose this host's disk");
            println!("and paste node A's CHAP password below.");
        } else {
            println!("Have node A's CHAP password ready to paste when asked below.");
        }
        println!();
    }

    // 1b. Topology mode. Inherited from the first node when this is the second
    // node (it must match), otherwise asked: fate-shared (single path) is the
    // default; arbitrated (direct link) suits a redundant/multi-switch network.
    let direct_ip: Option<IpAddr> = if let Some(inh) = &inherited {
        if inh.arbitrated {
            println!("Inherited topology: arbitrated (direct link); configure this node's end.");
            println!("\n{ARBITRATED_MODE_NOTE}\n");
            resolve_direct_link(&nets, peer_ip).await?
        } else {
            println!("Inherited topology: fate-shared (single path).");
            None
        }
    } else {
        println!("Topology mode:");
        println!("  1) fate-shared   single replication path over the network above (default)");
        println!("  2) arbitrated    add a back-to-back direct link (for redundant networks)");
        let mode = ask("Choice", Some("1"))?.trim().to_lowercase();
        if matches!(mode.as_str(), "2" | "a" | "arbitrated") {
            println!("\n{ARBITRATED_MODE_NOTE}\n");
            resolve_direct_link(&nets, peer_ip).await?
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
            "\nForce a full initial sync (only needed if the devices hold pre-existing data)",
            false,
        )?,
    };
    // When the peer is already up this is the second node: pasting node A's
    // password is required, not optional, so point straight at that.
    let chap_prompt = if peer_already_up {
        "CHAP password (paste the one shown on node A)"
    } else {
        "CHAP password (Enter to auto-generate on the first node; paste node A's on the second)"
    };
    let chap = ask(chap_prompt, Some(""))?;
    let chap_password = if chap.is_empty() { None } else { Some(chap) };
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
                "\nThe fence-peer tiebreaker always pings this node's default gateway ({gw})."
            ),
            None => println!(
                "\nWARNING: no default gateway detected. The fence-peer handler will have\n\
                 nothing to probe unless you add a local target below, and would then\n\
                 freeze this node's storage on any replication interruption, including\n\
                 the peer host simply dying (a 1-node failure becomes a full outage)."
            ),
        }
        let mut targets = Vec::new();
        loop {
            let prompt = if gateway.is_some() {
                "Extra always-on local IPs to also probe, space-separated (optional)"
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
                match t.parse() {
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
        Some(d) => println!("  mode:    arbitrated (direct link via peer {d})"),
        None => println!("  mode:    fate-shared (single path)"),
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
        "  ping:    default gateway always{}",
        if ping_targets.is_empty() {
            String::new()
        } else {
            format!(" + {ping_targets:?}")
        }
    );
    let confirm = ask("\nType 'yes' to proceed (anything else aborts)", None)?;
    if confirm != "yes" {
        bail!("aborted by user");
    }
    println!();

    Ok(Some(SetupParams {
        peer_ip,
        backing_device: Some(backing_device),
        local_ip,
        drbd_port,
        full_sync,
        chap_password,
        ping_targets,
        direct_ip,
        allow_no_tiebreaker,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
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

    #[test]
    fn nic_link_lines_show_link_and_ip() {
        let m = |kv: &[(&str, &str)]| -> std::collections::HashMap<String, String> {
            kv.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        assert_eq!(
            nic_link_line(&m(&[
                ("device", "eth3"),
                ("carrier", "false"),
                ("IP", ""),
                ("physical", "true")
            ]))
            .as_deref(),
            Some("eth3: no link, no IP")
        );
        assert_eq!(
            nic_link_line(&m(&[
                ("device", "eth1"),
                ("carrier", "true"),
                ("IP", "10.0.0.2")
            ]))
            .as_deref(),
            Some("eth1: link up, IP 10.0.0.2")
        );
        assert!(nic_link_line(&m(&[("device", "eth0"), ("management", "true")])).is_none());
        assert!(nic_link_line(&m(&[("device", "bond0"), ("physical", "false")])).is_none());
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

    #[test]
    fn ready_network_wins_over_unconfigured_nic() {
        let out = decide_direct_link(&[ip("198.51.100.2")], &["eth0".to_string()]);
        assert_eq!(out, DirectLinkOutcome::Ready(ip("198.51.100.2")));
    }

    #[test]
    fn cabled_nic_suggested_when_no_ready_network() {
        let out = decide_direct_link(&[], &["eth0".to_string(), "eth3".to_string()]);
        assert_eq!(
            out,
            DirectLinkOutcome::NeedsConfig {
                device: "eth0".to_string()
            }
        );
    }

    #[test]
    fn nothing_found_yields_none() {
        assert_eq!(decide_direct_link(&[], &[]), DirectLinkOutcome::None);
    }

    #[test]
    fn peer_end_configured_only_when_blank_and_cabled() {
        let want: Ipv4Addr = "10.99.99.2".parse().unwrap();
        assert_eq!(peer_end_action("", true, false, want), PeerEnd::Configure);
        assert_eq!(
            peer_end_action("10.99.99.2", true, false, want),
            PeerEnd::Done
        );
        assert!(matches!(
            peer_end_action("192.0.2.7", true, false, want),
            PeerEnd::Refuse(_)
        ));
        assert!(matches!(
            peer_end_action("", false, false, want),
            PeerEnd::Refuse(_)
        ));
        // Never touch the peer's management NIC, even blank and cabled.
        assert!(matches!(
            peer_end_action("", true, true, want),
            PeerEnd::Refuse(_)
        ));
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
