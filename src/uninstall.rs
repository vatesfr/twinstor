use std::io::Write;

use anyhow::{Context, Result, bail};

use crate::cmd::{cmd_output, run_cmd, run_cmd_quiet};

const CONFIG_PATH: &str = "/etc/twinstor/twinstor.toml";
const DRBD_CONF_DIR: &str = "/etc/drbd.d";

pub async fn run_uninstall(skip_confirm: bool, keep_data: bool, force: bool) -> Result<()> {
    println!("TwinStor uninstall\n");
    println!("this will:");
    println!("  1. disable XHA");
    println!("  2. unplug PBDs and destroy the twinstor SR");
    println!("  3. stop the twinstor daemon");
    println!("  4. stop iSCSI target and DRBD resource");
    if !keep_data {
        println!("  5. wipe the backing device (all data will be lost)");
    }
    println!("  6. remove config files\n");

    // Load config up front: it names the resource, backing device, and iSCSI
    // target IQN, and the IQN feeds the by-IQN SR-uuid fallback used below (a
    // renamed SR has no name-label match). Config files are removed in step 6.
    let config = crate::config::Config::load(std::path::Path::new(CONFIG_PATH)).ok();

    // Resolve the twinstor SR uuid once (recorded file, then name-label, then
    // by-IQN) and use it for both the VM-safety gate and the destroy-SR step.
    // Resolving by uuid rather than name-label survives an SR rename, which
    // would otherwise skip the destroy and make the safety gate read 0 VMs.
    // Empty means unresolved, which the gate treats as fail-closed.
    let sr_uuid = resolve_sr_uuid(config.as_ref()).await.unwrap_or_default();

    // Check for running VMs on the twinstor SR before doing anything. This
    // guard fails closed: uninstall destroys the SR and wipes this node's
    // backing, so an inability to verify the SR is unused must stop us, not be
    // read as "0 VMs, safe to proceed". --force overrides for the real
    // "I know it's unused" case (e.g. a node whose XAPI/HA view is degraded).
    match count_vms_on_twinstor_sr(&sr_uuid).await {
        Ok(0) => {}
        Ok(n) => {
            println!("WARNING: {n} VM(s) are currently using the twinstor SR.");
            println!("         shut down or migrate all VMs off the SR before uninstalling.\n");
            if !force {
                bail!("{n} VM(s) still on twinstor SR, aborting (use --force to override)");
            }
            println!("--force given: proceeding despite {n} VM(s) on the SR.\n");
        }
        Err(e) => {
            println!("WARNING: could not verify the twinstor SR is unused: {e}");
            if !force {
                bail!(
                    "refusing to uninstall: the VM-safety check could not confirm the SR is \
                     unused. Fix XAPI/HA on this node, or re-run with --force if you are sure \
                     no VMs depend on this node's storage."
                );
            }
            println!("--force given: proceeding without a verified VM count.\n");
        }
    }

    if !skip_confirm {
        print!("proceed? [y/N] ");
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("aborted.");
            return Ok(());
        }
        println!();
    }

    let res = config
        .as_ref()
        .map(|c| format!("twinstor-{}", c.storage.drbd_resource))
        .unwrap_or_else(|| "twinstor-data".to_string());

    // The backing device, used both for LV removal (step 5) and to reverse the
    // LVM filter setup installed. Captured up front because the config files are
    // removed in step 6, after which it is no longer available.
    let backing_dev = config.as_ref().map(|c| c.storage.backing_device.clone());

    // Step 1: disable XHA
    print_step(1, "disabling XHA");
    match run_cmd("xe", &["pool-ha-disable"]).await {
        Ok(()) => println!("  XHA disabled"),
        Err(e) => println!("  XHA not enabled or already disabled ({e})"),
    }
    // Put back the pool's ha-reboot-vm-on-internal-shutdown if fate-shared
    // freeze mode turned it off (it records the value it found).
    match crate::xapi::pool_internal_shutdown_policy().await {
        Some((false, Some(true))) => {
            if crate::xapi::restore_pool_internal_shutdown(Some(true)).await {
                println!(
                    "  pool ha-reboot-vm-on-internal-shutdown put back on (freeze mode had turned it off)"
                );
            } else {
                println!(
                    "  could not put pool ha-reboot-vm-on-internal-shutdown back on; \
                          run: xe pool-param-set uuid=<pool> ha-reboot-vm-on-internal-shutdown=true"
                );
            }
        }
        Some((_, Some(_))) => {
            if !crate::xapi::restore_pool_internal_shutdown(None).await {
                println!(
                    "  could not drop twinstor's record of pool ha-reboot-vm-on-internal-shutdown; \
                          run: xe pool-param-remove uuid=<pool> param-name=other-config \
                          param-key={}",
                    crate::xapi::INTERNAL_SHUTDOWN_PRIOR_KEY
                );
            }
        }
        Some((_, None)) => {}
        None => {
            // XAPI did not answer (or predates the setting): freeze mode may
            // have turned the setting off.
            println!("{}", internal_shutdown_unread_note());
        }
    }

    // Step 2: unplug PBDs and destroy the SR (using the uuid resolved up front,
    // which survives an SR rename that a name-label lookup would miss).
    print_step(2, "destroying twinstor SR");
    let sr_uuid = sr_uuid.trim();

    if sr_uuid.is_empty() {
        println!("  no twinstor SR found, skipping");
    } else {
        // Unplug all PBDs for this SR
        let pbds = cmd_output(
            "xe",
            &["pbd-list", &format!("sr-uuid={sr_uuid}"), "--minimal"],
        )
        .await
        .unwrap_or_default();
        for pbd in pbds.split(',') {
            let pbd = pbd.trim();
            if pbd.is_empty() {
                continue;
            }
            print!("  unplugging PBD {pbd}... ");
            match run_cmd("xe", &["pbd-unplug", &format!("uuid={pbd}")]).await {
                Ok(()) => println!("ok"),
                Err(_) => println!("already unplugged"),
            }
        }

        // Forget (not destroy) the SR: the LV removal handles the data
        print!("  destroying SR {sr_uuid}... ");
        match run_cmd("xe", &["sr-destroy", &format!("uuid={sr_uuid}")]).await {
            Ok(()) => println!("ok"),
            Err(e) => {
                println!("failed ({e}), trying sr-forget");
                let _ = run_cmd("xe", &["sr-forget", &format!("uuid={sr_uuid}")]).await;
            }
        }
    }

    // Drop the pool-wide shared-setup handoff key if it lingered (e.g. the
    // second node never ran, so it was never cleared). Idempotent and harmless
    // when absent.
    crate::setup::clear_shared_setup().await;

    // Step 3: stop twinstor daemon
    print_step(3, "stopping twinstor daemon");
    match run_cmd("systemctl", &["stop", "twinstor"]).await {
        Ok(()) => println!("  daemon stopped"),
        Err(_) => println!("  daemon not running"),
    }
    let _ = run_cmd("systemctl", &["disable", "twinstor"]).await;

    // Step 4: stop iSCSI target and DRBD
    print_step(4, "stopping iSCSI target");
    let _ = run_cmd("targetcli", &["clearconfig confirm=True"]).await;
    let _ = run_cmd("targetcli", &["saveconfig"]).await;
    println!("  iSCSI cleared");

    println!("  stopping DRBD resource {res}");
    let _ = run_cmd("drbdadm", &["secondary", &res]).await;
    let _ = run_cmd("drbdadm", &["down", &res]).await;
    println!("  DRBD stopped");

    // Log out of iSCSI sessions to the twinstor target
    let iqn = config.as_ref().map(|c| c.iscsi.target_iqn.clone());
    if let Some(iqn) = &iqn {
        println!("  logging out of iSCSI sessions for {iqn}");
        let _ = run_cmd("iscsiadm", &["-m", "node", "-T", iqn, "--logout"]).await;
        let _ = run_cmd("iscsiadm", &["-m", "node", "-T", iqn, "-o", "delete"]).await;
    }

    // Step 5: wipe the backing device
    if !keep_data {
        let backing = backing_dev.as_ref().map(|d| d.display().to_string());
        if let Some(backing) = backing {
            print_step(5, &format!("wiping backing device {backing}"));
            clear_backing_device(&backing).await;
        } else {
            print_step(5, "wiping backing device");
            println!("  no config found, skipping wipe");
        }
    }

    // Step 6: remove config files
    let step = if keep_data { 5 } else { 6 };
    print_step(step, "removing config files");
    for path in [
        CONFIG_PATH,
        "/etc/twinstor/ping-targets",
        "/etc/twinstor/direct-peer",
        &format!("{DRBD_CONF_DIR}/{res}.res"),
        "/usr/lib/drbd/twinstor-fence-peer.sh",
        "/usr/lib/drbd/twinstor-split-brain.sh",
        "/etc/logrotate.d/twinstor",
        "/run/twinstor/split-brain",
        "/etc/multipath/conf.d/twinstor.conf",
        // Restart obligation of a split-brain crash boundary: a reinstall
        // must not start VMs an operator may have left halted on purpose.
        "/var/lib/twinstor/loser-boundary",
        "/var/lib/twinstor/loser-boundary.tmp",
        "/var/lib/twinstor/discard-requested.tmp",
        "/var/lib/twinstor/boot-gate-peer-served.tmp",
        "/run/twinstor/discard-offer-parked",
        "/run/twinstor/steer-pending",
        // Split-brain fork records: they describe an episode of this install.
        "/var/lib/twinstor/yield-loser-latched",
        "/var/lib/twinstor/yield-loser-latched.resuming",
        "/var/lib/twinstor/split-brain-win-claim",
    ] {
        if std::path::Path::new(path).exists() {
            match std::fs::remove_file(path) {
                Ok(()) => println!("  removed {path}"),
                Err(e) => println!("  failed to remove {path}: {e}"),
            }
        }
    }
    // The two markers that keep this copy from serving (a pending manual
    // discard; the memory of a peer seen serving) describe the data, which
    // --keep-data preserves: a reinstall over the same copy must honour them.
    for path in [
        "/var/lib/twinstor/discard-requested",
        "/var/lib/twinstor/boot-gate-peer-served",
    ] {
        if std::path::Path::new(path).exists() {
            if keep_data {
                println!("  kept {path} (it protects the preserved copy)");
            } else {
                match std::fs::remove_file(path) {
                    Ok(()) => println!("  removed {path}"),
                    Err(e) => println!("  failed to remove {path}: {e}"),
                }
            }
        }
    }
    // Remove config dir if empty
    let _ = std::fs::remove_dir("/etc/twinstor");

    // Reverse the LVM filter setup installed to hide the raw backing device.
    if let Some(backing) = &backing_dev {
        revert_lvm_filter(backing).await;
    }

    // Remove firewall rules
    remove_firewall_rules().await;

    // Reconfigure multipath to drop twinstor paths
    let _ = run_cmd("multipathd", &["reconfigure"]).await;

    println!("\ntwinstor uninstalled.");
    if keep_data {
        println!("the backing device was preserved (--keep-data).");
    }
    println!("run this command on the peer node as well.");

    Ok(())
}

fn print_step(n: u32, msg: &str) {
    println!("step {n}: {msg}");
}

/// What to run by hand when the pool's ha-reboot-vm-on-internal-shutdown
/// could not be read during uninstall.
fn internal_shutdown_unread_note() -> String {
    let key = crate::xapi::INTERNAL_SHUTDOWN_PRIOR_KEY;
    format!(
        "  could not read pool ha-reboot-vm-on-internal-shutdown (XAPI did not answer, or has no such \
         setting). If twinstor's record is there (xe pool-param-get uuid=<pool> param-name=other-config \
         param-key={key}): when it reads true (the value twinstor found), run xe pool-param-set \
         uuid=<pool> ha-reboot-vm-on-internal-shutdown=true; whatever it reads, then drop it: \
         xe pool-param-remove uuid=<pool> param-name=other-config param-key={key}"
    )
}

/// MiB offset at which to start zeroing the trailing 8 MiB of a device that is
/// `sectors` 512-byte sectors long, or None when the device is too small to
/// have a tail distinct from its head. DRBD `meta-disk internal` keeps its
/// metadata at the very end of the backing device, so a head-only wipe leaves
/// it behind.
fn tail_zero_seek_mib(sectors: u64) -> Option<u64> {
    let total_mib = sectors / 2048; // 2048 * 512B = 1 MiB
    (total_mib > 8).then(|| total_mib - 8)
}

/// Whether `dev` resolves to an LVM logical volume. setup puts DRBD directly on
/// the raw by-id device (disk or partition), so this is false for everything it
/// creates; it is true only for older installs that carved an LV, which are
/// still removed with lvremove.
async fn is_lvm_lv(dev: &str) -> bool {
    // run_cmd_quiet: a raw disk/partition path always makes lvs fail, so log
    // that expected miss at debug rather than warn.
    run_cmd_quiet("lvs", &["--noheadings", dev]).await.is_ok()
}

/// Return the backing device to a clean, re-installable state. For the raw
/// disk/partition that setup creates, `lvremove` does not apply (it errors
/// "Invalid path for Logical Volume"); instead drop every on-disk signature
/// with `wipefs -a` and zero the head and tail, the tail being where DRBD's
/// internal metadata lives. Each sub-step reports its own failure but never
/// aborts the uninstall.
async fn clear_backing_device(backing: &str) {
    if is_lvm_lv(backing).await {
        match run_cmd("lvremove", &["-f", backing]).await {
            Ok(()) => println!("  LV removed"),
            Err(e) => println!("  LV removal failed ({e})"),
        }
        return;
    }
    if let Err(e) = run_cmd("wipefs", &["-a", backing]).await {
        println!("  wipefs failed ({e})");
    }
    // Head: LVM PV label, filesystem superblocks.
    let _ = run_cmd(
        "dd",
        &[
            "if=/dev/zero",
            &format!("of={backing}"),
            "bs=1M",
            "count=8",
            "conv=fsync",
            "status=none",
        ],
    )
    .await;
    // Tail: DRBD internal metadata.
    if let Ok(out) = cmd_output("blockdev", &["--getsz", backing]).await
        && let Ok(sectors) = out.trim().parse::<u64>()
        && let Some(seek) = tail_zero_seek_mib(sectors)
    {
        let _ = run_cmd(
            "dd",
            &[
                "if=/dev/zero",
                &format!("of={backing}"),
                "bs=1M",
                &format!("seek={seek}"),
                "count=8",
                "conv=fsync",
                "status=none",
            ],
        )
        .await;
    }
    println!("  backing device wiped");
}

/// Resolve the twinstor SR uuid for uninstall: the recorded-file / name-label
/// resolver first, then (if config is present) the by-IQN discovery that finds a
/// renamed SR with no recorded file. `None` = could not resolve, which the
/// safety gate treats as fail-closed.
async fn resolve_sr_uuid(config: Option<&crate::config::Config>) -> Option<String> {
    if let Some(u) = crate::xapi::twinstor_sr_uuid().await {
        return Some(u);
    }
    match config {
        Some(c) => crate::xapi::discover_sr_uuid_by_iqn(&c.iscsi.target_iqn).await,
        None => None,
    }
}

/// Number of VMs with a disk on the twinstor SR (pool-wide), for the resolved
/// `sr_uuid`. Returns Err when the uuid is unresolved (empty) or when XAPI
/// cannot be queried; the caller treats that Err as "cannot confirm, do not
/// proceed". This must not default to 0 on an unresolved uuid: a renamed SR
/// would then look unused and uninstall would wipe a replica still in use.
/// Ok(0) is returned only when the SR exists but has no attached VDIs, a
/// positively confirmed "nothing depends on it".
async fn count_vms_on_twinstor_sr(sr_uuid: &str) -> Result<u64> {
    let sr_uuid = sr_uuid.trim();
    if sr_uuid.is_empty() {
        // Unresolved: fail closed, like an XAPI-error path. Do not read it
        // as "0 VMs, safe to wipe".
        bail!("the twinstor SR uuid could not be resolved, so its VM usage cannot be confirmed");
    }

    // VDIs that live on the SR, and the attached disk VBDs in the pool. Both
    // are pool-global xe queries, so the answer does not depend on which node
    // runs uninstall. A failure of either is a hard error, not an implied zero.
    let sr_vdis = cmd_output(
        "xe",
        &[
            "vdi-list",
            &format!("sr-uuid={sr_uuid}"),
            "params=uuid",
            "--minimal",
        ],
    )
    .await
    .context("listing VDIs on the twinstor SR")?;
    let sr_vdis: std::collections::HashSet<String> = sr_vdis
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if sr_vdis.is_empty() {
        return Ok(0);
    }

    let vbds = cmd_output(
        "xe",
        &[
            "vbd-list",
            "type=Disk",
            "currently-attached=true",
            "params=vdi-uuid",
            "--minimal",
        ],
    )
    .await
    .context("listing attached disk VBDs")?;
    let mut count = 0u64;
    for vdi in vbds.split(',') {
        let vdi = vdi.trim();
        if !vdi.is_empty() && sr_vdis.contains(vdi) {
            count += 1;
        }
    }
    Ok(count)
}

/// Reverse `setup::install_lvm_filter`: drop the reject pattern for the backing
/// device's by-id name from the active `global_filter` in both the system and
/// SM-private lvm.conf. Leaving it behind is harmless (the device is gone) but
/// a host reused for something else would carry the stale entry indefinitely.
/// Only raw-disk by-id backings ever had a filter installed.
async fn revert_lvm_filter(backing: &std::path::Path) {
    if !backing.starts_with("/dev/disk/by-id") {
        return;
    }
    let name = backing
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if name.is_empty() {
        return;
    }

    let mut reverted = false;
    for conf in ["/etc/lvm/lvm.conf", "/etc/lvm/master/lvm.conf"] {
        let content = match tokio::fs::read_to_string(conf).await {
            Ok(c) => c,
            Err(_) => continue,
        };
        // None: pattern not present (already clean), so this is idempotent.
        if let Some(patched) = unpatch_lvm_global_filter(&content, &name) {
            if tokio::fs::write(conf, patched).await.is_ok() {
                println!("  removed LVM filter for {name} in {conf}");
                reverted = true;
            }
        }
    }
    if reverted {
        let _ = run_cmd("pvscan", &["--cache"]).await;
    }
}

/// Remove the `"r|name|"` reject entry that `setup::patch_lvm_global_filter`
/// added to a `global_filter`. Returns `None` when it is not present. If the
/// reject was the only entry on the line (the line setup created from scratch),
/// the whole line is dropped rather than left as an empty `global_filter`.
fn unpatch_lvm_global_filter(content: &str, name: &str) -> Option<String> {
    let reject = format!("\"r|{name}|\"");
    if !content.contains(&reject) {
        return None;
    }

    let mut out: Vec<String> = Vec::new();
    for line in content.lines() {
        if !line.contains(&reject) {
            out.push(line.to_string());
            continue;
        }
        // Setup inserts the entry as `[ "r|name|", ` ahead of any existing
        // patterns; strip that form first, then the trailing/standalone forms.
        let cleaned = line
            .replace(&format!("{reject}, "), "")
            .replace(&format!(", {reject}"), "")
            .replace(&reject, "");
        // Drop the line entirely if nothing remains between the brackets (it
        // was a filter line setup created with our entry as its sole content).
        let empty_filter = match (cleaned.find('['), cleaned.rfind(']')) {
            (Some(a), Some(b)) if b > a => cleaned[a + 1..b].trim().is_empty(),
            _ => false,
        };
        if !empty_filter {
            out.push(cleaned);
        }
    }
    Some(out.join("\n") + "\n")
}

async fn remove_firewall_rules() {
    let iptables_file = "/etc/sysconfig/iptables";
    let content = match tokio::fs::read_to_string(iptables_file).await {
        Ok(c) => c,
        Err(_) => return,
    };

    if !content.contains("# TWINSTOR") {
        return;
    }

    let new_content: String = content
        .lines()
        .filter(|line| {
            !line.contains("# TWINSTOR")
                && !line.contains("--dport 7789")
                && !line.contains("--dport 3260")
        })
        .collect::<Vec<_>>()
        .join("\n");

    if let Ok(()) = tokio::fs::write(iptables_file, format!("{new_content}\n")).await {
        let _ = run_cmd("systemctl", &["restart", "iptables"]).await;
        println!("  removed firewall rules");
    }
}

#[cfg(test)]
mod tests {
    use super::{tail_zero_seek_mib, unpatch_lvm_global_filter};

    #[test]
    fn tail_seek_targets_last_8_mib() {
        // 200 MiB device (2048 * 200 sectors): tail starts at 192 MiB.
        assert_eq!(tail_zero_seek_mib(2048 * 200), Some(192));
        // 100 GiB device: tail starts at (102400 - 8) MiB.
        assert_eq!(tail_zero_seek_mib(2048 * 102_400), Some(102_392));
    }

    #[test]
    fn tail_seek_none_when_no_distinct_tail() {
        assert_eq!(tail_zero_seek_mib(0), None);
        assert_eq!(tail_zero_seek_mib(2048 * 8), None); // exactly 8 MiB
    }

    #[test]
    fn internal_shutdown_unread_note_says_both_steps() {
        let n = super::internal_shutdown_unread_note();
        assert!(
            n.contains("ha-reboot-vm-on-internal-shutdown=true")
                && n.contains("pool-param-remove")
                && n.contains("twinstor-internal-shutdown-prior")
                && n.contains("whatever it reads")
                && !n.contains('\n'),
            "{n}"
        );
    }

    #[test]
    fn unpatch_restores_existing_filter() {
        // The shape setup::patch_lvm_global_filter produces when an active
        // global_filter already existed: our entry is prepended.
        let patched = "devices {\n\tglobal_filter = [ \"r|nvme-MODEL_SERIAL123|\", \"r|^/dev/drbd.*|\" ]\n}\n";
        let out = unpatch_lvm_global_filter(patched, "nvme-MODEL_SERIAL123").unwrap();
        assert!(out.contains("global_filter = [ \"r|^/dev/drbd.*|\" ]"));
        assert!(!out.contains("nvme-MODEL_SERIAL123"));
    }

    #[test]
    fn unpatch_drops_filter_line_setup_created() {
        // The shape setup produces when it created the filter line from scratch:
        // our entry is the only content, so the whole line should go.
        let patched =
            "# comment\ndevices {\n\tdir = \"/dev\"\n\tglobal_filter = [ \"r|nvme-X_1|\" ]\n}\n";
        let out = unpatch_lvm_global_filter(patched, "nvme-X_1").unwrap();
        assert!(!out.contains("global_filter"));
        assert!(!out.contains("nvme-X_1"));
        // unrelated lines are preserved
        assert!(out.contains("dir = \"/dev\""));
    }

    #[test]
    fn unpatch_is_idempotent_when_absent() {
        let conf = "devices {\n\tglobal_filter = [ \"r|^/dev/drbd.*|\" ]\n}\n";
        assert!(unpatch_lvm_global_filter(conf, "nvme-MODEL_SERIAL123").is_none());
    }
}
