//! Backing-device volatile write cache control.
//!
//! The dom0 storage stack (tapdisk in particular) treats a completed
//! O_DIRECT write as durable: guest flush barriers are acknowledged but not
//! propagated down to the device as flush commands. On a drive with a
//! volatile write cache that assumption is false: acknowledged writes sit
//! in cache DRAM and a hard host reset (watchdog fence, power event) can
//! drop them. While DRBD is connected the peer's epoch flushes keep the
//! replicated copy durable, but a node writing alone (disconnected peer,
//! split-brain) holds the only copy of recent writes in its cache: a fence
//! then loses fsync'd guest data. Disabling the cache makes write
//! completion mean durable-on-media, restoring fsync semantics end-to-end
//! regardless of what the layers above do with flushes.
//!
//! Drives without a volatile write cache (enterprise SSDs with power-loss
//! protection) are already durable on completion and are left alone.
//!
//! The NVMe volatile-write-cache feature is not persistent: any controller
//! reset (every reboot) re-enables it. Like the iSCSI `recovery_tmo`
//! enforcement, the daemon re-asserts it periodically rather than trusting
//! a one-shot setting.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;

use tracing::{info, warn};

/// Volatile write cache state of one physical disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheState {
    /// The drive reports no volatile write cache: durable on completion.
    NoVolatileCache,
    Enabled,
    Disabled,
    /// A volume of a hardware RAID controller (the named driver): the
    /// controller owns the cache. Durable when its cache is battery- or
    /// flash-backed and the member disks' own caches are off, which only the
    /// controller's tools can tell.
    ControllerManaged(String),
    /// State could not be determined (unsupported bus, query failure).
    Unknown(String),
}

impl CacheState {
    pub fn label(&self) -> &str {
        match self {
            CacheState::NoVolatileCache => "none (durable on completion)",
            CacheState::Enabled => "enabled (not durable on host reset)",
            CacheState::Disabled => "disabled (durable)",
            CacheState::ControllerManaged(_) => {
                "managed by the RAID controller (keep its cache battery- or flash-backed \
                 and the disks' own cache off)"
            }
            CacheState::Unknown(_) => "unknown",
        }
    }

    pub fn word(&self) -> &str {
        match self {
            CacheState::NoVolatileCache => "none",
            CacheState::Enabled => "enabled",
            CacheState::Disabled => "disabled",
            CacheState::ControllerManaged(_) => "controller",
            CacheState::Unknown(_) => "unknown",
        }
    }
}

/// Outcome of one enforcement attempt on one physical disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnforceOutcome {
    NoVolatileCache,
    AlreadyDisabled,
    /// The cache was enabled and has been disabled now.
    Disabled,
    /// ZFS zvol backing: there is no device-level volatile-cache toggle;
    /// flush durability is governed by ZFS itself (ZIL / the dataset's
    /// `sync` property). Nothing to enforce here; reported separately so the
    /// operator-facing lines describe it correctly instead of showing a
    /// misleading hdparm failure.
    ZvolDelegated,
    /// Hardware RAID volume: the controller owns the cache (driver named).
    ControllerDelegated(String),
    Failed(String),
}

/// Query the volatile write cache state of every physical disk under the
/// backing device (an LV or partition resolves to its underlying disks).
pub async fn status(backing: &Path) -> Vec<(String, CacheState)> {
    let mut v = Vec::new();
    for disk in physical_disks(backing) {
        let state = query_disk(&disk).await;
        v.push((disk, state));
    }
    v
}

/// Disable the volatile write cache on every physical disk under the
/// backing device. Silent when there is nothing to do, so it is safe to
/// call on a periodic cadence.
///
/// The disable is issued unconditionally whenever the drive reports a
/// volatile cache (the Identify presence bit, which is reliable), not gated
/// on a read of the current setting: not every drive's firmware populates
/// the GET FEATURES completion value (two units of the same model can
/// differ, one returning the value and one returning zero), and gating on a
/// false "already disabled" read would leave the cache on. The set is
/// idempotent, so re-issuing it every cadence is harmless. A read is still
/// taken first, but only to decide whether this is a state change worth
/// logging.
pub async fn enforce(backing: &Path) -> Vec<(String, EnforceOutcome)> {
    let mut v = Vec::new();
    for disk in physical_disks(backing) {
        if disk.starts_with("zd") {
            v.push((disk, EnforceOutcome::ZvolDelegated));
            continue;
        }
        let was = query_disk(&disk).await;
        let outcome = match was {
            CacheState::NoVolatileCache => EnforceOutcome::NoVolatileCache,
            CacheState::ControllerManaged(driver) => EnforceOutcome::ControllerDelegated(driver),
            // Enabled, Disabled (possibly a stale read), or Unknown: a
            // volatile cache exists, so disable it regardless of the read.
            _ => match disable_disk(&disk).await {
                Ok(()) => {
                    // Log only on a believed state change; a drive that
                    // reads back Disabled every cadence stays quiet.
                    if was != CacheState::Disabled {
                        info!(disk = %disk, prior = %was.word(),
                            "disabled volatile write cache on backing disk");
                        EnforceOutcome::Disabled
                    } else {
                        EnforceOutcome::AlreadyDisabled
                    }
                }
                Err(e) => {
                    warn!(disk = %disk, error = %e,
                        "failed to disable volatile write cache: fsync'd guest data \
                         can be lost if this host is fenced while its peer is disconnected");
                    EnforceOutcome::Failed(e)
                }
            },
        };
        v.push((disk, outcome));
    }
    v
}

async fn query_disk(disk: &str) -> CacheState {
    if disk.starts_with("nvme") {
        match nvme_vwc_state(disk) {
            Ok(s) => s,
            Err(e) => CacheState::Unknown(e),
        }
    } else if disk.starts_with("sd") {
        match sd_kind(disk) {
            SdKind::Ata => sata_vwc_state(disk).await,
            SdKind::Raid(driver) => CacheState::ControllerManaged(driver),
            SdKind::Scsi => scsi_cache_state(disk),
        }
    } else {
        CacheState::Unknown(format!("unsupported device type: {disk}"))
    }
}

async fn disable_disk(disk: &str) -> Result<(), String> {
    if disk.starts_with("nvme") {
        nvme_set_vwc(disk, false)
    } else if disk.starts_with("sd") && sd_kind(disk) == SdKind::Scsi {
        scsi_set_write_through(disk)
    } else {
        // SET FEATURES via hdparm; ships in dom0. Bounded: hdparm can block on
        // a wedged disk, and this runs on the tick (enforce_write_cache).
        crate::cmd::run_cmd_for("hdparm", &["-W0", &format!("/dev/{disk}")], 15)
            .await
            .map_err(|e| format!("hdparm -W0: {e}"))
    }
}

/// How an `sd` disk's cache is reached.
#[derive(Debug, PartialEq, Eq)]
enum SdKind {
    /// An ATA disk (libata, or SATA behind a pass-through HBA): hdparm.
    Ata,
    /// A volume of a hardware RAID controller (its driver): the controller's.
    Raid(String),
    /// Any other SCSI disk (SAS, for one): the kernel's caching mode page.
    Scsi,
}

/// SCSI host drivers of hardware RAID controllers, whose `sd` devices are
/// controller volumes (Dell PERC and Broadcom MegaRAID, Microchip/Adaptec
/// SmartRAID, HPE Smart Array, Areca).
const RAID_DRIVERS: [&str; 6] = [
    "megaraid_sas",
    "smartpqi",
    "hpsa",
    "aacraid",
    "arcmsr",
    "megaraid",
];

fn classify_sd(vendor: &str, host_driver: &str) -> SdKind {
    if vendor.trim() == "ATA" {
        SdKind::Ata
    } else if RAID_DRIVERS.contains(&host_driver.trim()) {
        SdKind::Raid(host_driver.trim().to_string())
    } else {
        SdKind::Scsi
    }
}

fn sd_kind(disk: &str) -> SdKind {
    let dev = Path::new("/sys/block").join(disk).join("device");
    let vendor = std::fs::read_to_string(dev.join("vendor")).unwrap_or_default();
    // The device path runs through its SCSI host (`.../host2/target2:2:0/...`).
    let host_driver = std::fs::canonicalize(&dev)
        .ok()
        .and_then(|p| {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .find(|c| c.starts_with("host") && c[4..].chars().all(|ch| ch.is_ascii_digit()))
        })
        .and_then(|h| std::fs::read_to_string(format!("/sys/class/scsi_host/{h}/proc_name")).ok())
        .unwrap_or_default();
    classify_sd(&vendor, &host_driver)
}

/// The kernel's `cache_type` for a SCSI disk (its caching mode page).
fn parse_cache_type(text: &str) -> CacheState {
    let t = text.trim();
    if t.starts_with("write back") {
        CacheState::Enabled
    } else if t.starts_with("write through") {
        CacheState::Disabled
    } else if t.starts_with("none") {
        CacheState::NoVolatileCache
    } else {
        CacheState::Unknown(format!("unparsable cache_type: {t}"))
    }
}

/// The sysfs `cache_type` file of an `sd` disk.
fn scsi_cache_type_path(disk: &str) -> Option<std::path::PathBuf> {
    let dir = Path::new("/sys/block")
        .join(disk)
        .join("device")
        .join("scsi_disk");
    let entry = std::fs::read_dir(dir).ok()?.flatten().next()?;
    Some(entry.path().join("cache_type"))
}

fn scsi_cache_state(disk: &str) -> CacheState {
    match scsi_cache_type_path(disk).map(std::fs::read_to_string) {
        Some(Ok(t)) => parse_cache_type(&t),
        Some(Err(e)) => CacheState::Unknown(format!("cache_type: {e}")),
        None => CacheState::Unknown("no scsi_disk cache_type".into()),
    }
}

/// Turn a SCSI disk's write cache off: the kernel sends the MODE SELECT
/// (WCE=0) when `write through` is written to its `cache_type`.
fn scsi_set_write_through(disk: &str) -> Result<(), String> {
    let path = scsi_cache_type_path(disk).ok_or("no scsi_disk cache_type")?;
    std::fs::write(&path, "write through").map_err(|e| format!("cache_type: {e}"))?;
    match scsi_cache_state(disk) {
        CacheState::Disabled | CacheState::NoVolatileCache => Ok(()),
        other => Err(format!("cache_type reads {} after the write", other.word())),
    }
}

async fn sata_vwc_state(disk: &str) -> CacheState {
    // Bounded (see disable_disk): a wedged hdparm must not hang the tick.
    let text =
        match crate::cmd::cmd_output_for("hdparm", &["-W", &format!("/dev/{disk}")], 15).await {
            Ok(t) => t,
            Err(e) => return CacheState::Unknown(format!("hdparm: {e}")),
        };
    // "write-caching =  1 (on)" / "= 0 (off)"
    if let Some(line) = text.lines().find(|l| l.contains("write-caching")) {
        if line.contains("(on)") {
            CacheState::Enabled
        } else if line.contains("(off)") {
            CacheState::Disabled
        } else {
            CacheState::Unknown(format!("unparsable hdparm output: {}", line.trim()))
        }
    } else {
        CacheState::Unknown("no write-caching line in hdparm output".into())
    }
}

// ---------------------------------------------------------------------------
// NVMe admin commands via ioctl. dom0 does not ship nvme-cli, and the
// volatile-write-cache feature has no sysfs knob (`queue/write_cache` only
// changes whether the kernel sends flushes, not the device cache itself),
// so the feature is driven directly through the admin-command ioctl.

const NVME_ADMIN_IDENTIFY: u8 = 0x06;
const NVME_ADMIN_SET_FEATURES: u8 = 0x09;
const NVME_ADMIN_GET_FEATURES: u8 = 0x0a;
const NVME_FEAT_VOLATILE_WC: u32 = 0x06;
// _IOWR('N', 0x41, struct nvme_admin_cmd /* 72 bytes */)
// u32 + cast at the call site: the ioctl request parameter is c_ulong on
// glibc but c_int on musl.
const NVME_IOCTL_ADMIN_CMD: u32 = 0xc048_4e41;
// Identify Controller: byte 525 bit 0 = volatile write cache present.
const ID_CTRL_VWC_OFFSET: usize = 525;

#[repr(C)]
#[derive(Default)]
struct NvmeAdminCmd {
    opcode: u8,
    flags: u8,
    rsvd1: u16,
    nsid: u32,
    cdw2: u32,
    cdw3: u32,
    metadata: u64,
    addr: u64,
    metadata_len: u32,
    data_len: u32,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
    timeout_ms: u32,
    result: u32,
}

fn nvme_admin(file: &File, cmd: &mut NvmeAdminCmd) -> Result<u32, String> {
    let rc = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            NVME_IOCTL_ADMIN_CMD as _,
            cmd as *mut NvmeAdminCmd,
        )
    };
    if rc < 0 {
        Err(format!("ioctl: {}", std::io::Error::last_os_error()))
    } else if rc > 0 {
        // Positive return is the NVMe status code.
        Err(format!("NVMe status 0x{rc:x}"))
    } else {
        Ok(cmd.result)
    }
}

/// Controller character device for a namespace block device:
/// `nvme0n1` gives `nvme0`. Admin commands (Identify, Get/Set Features) must
/// go to the controller node, not the namespace node: passthru of bufferless
/// controller-scoped feature commands through `/dev/nvmeXnY` is unreliable
/// (on some drives Get/Set Features VWC returns success but reads or writes
/// nothing), while `/dev/nvmeX` works consistently. nvme-cli does the same.
fn nvme_controller(disk: &str) -> String {
    // Strip a trailing namespace suffix "n<digits>" (e.g. nvme0n1 to nvme0).
    // The suffix's 'n' is the last 'n' that is both preceded by a digit and
    // followed only by digits, which distinguishes it from the 'n' in
    // "nvme".
    let bytes = disk.as_bytes();
    if let Some(idx) = disk.rfind('n') {
        let after = &disk[idx + 1..];
        let preceded_by_digit = idx > 0 && bytes[idx - 1].is_ascii_digit();
        if preceded_by_digit && !after.is_empty() && after.bytes().all(|b| b.is_ascii_digit()) {
            return disk[..idx].to_string();
        }
    }
    disk.to_string()
}

fn nvme_vwc_state(disk: &str) -> Result<CacheState, String> {
    let ctrl = nvme_controller(disk);
    let file = File::open(format!("/dev/{ctrl}")).map_err(|e| e.to_string())?;

    let mut id = vec![0u8; 4096];
    let mut cmd = NvmeAdminCmd {
        opcode: NVME_ADMIN_IDENTIFY,
        addr: id.as_mut_ptr() as u64,
        data_len: id.len() as u32,
        cdw10: 1, // CNS 1: Identify Controller
        ..Default::default()
    };
    nvme_admin(&file, &mut cmd)?;
    if id[ID_CTRL_VWC_OFFSET] & 1 == 0 {
        return Ok(CacheState::NoVolatileCache);
    }

    let mut cmd = NvmeAdminCmd {
        opcode: NVME_ADMIN_GET_FEATURES,
        cdw10: NVME_FEAT_VOLATILE_WC, // SEL 0: current value
        ..Default::default()
    };
    let current = nvme_admin(&file, &mut cmd)?;
    Ok(if current & 1 == 1 {
        CacheState::Enabled
    } else {
        CacheState::Disabled
    })
}

fn nvme_set_vwc(disk: &str, enabled: bool) -> Result<(), String> {
    let ctrl = nvme_controller(disk);
    let file = File::open(format!("/dev/{ctrl}")).map_err(|e| e.to_string())?;
    let mut cmd = NvmeAdminCmd {
        opcode: NVME_ADMIN_SET_FEATURES,
        cdw10: NVME_FEAT_VOLATILE_WC,
        cdw11: enabled as u32,
        ..Default::default()
    };
    nvme_admin(&file, &mut cmd).map(|_| ())
}

// ---------------------------------------------------------------------------
// Device resolution: the configured backing device (a stable by-id symlink,
// an LV, or a partition) maps to one or more physical disks.

/// Physical disk names (e.g. `nvme1n1`, `sdb`) underlying a block device.
fn physical_disks(dev: &Path) -> Vec<String> {
    let Ok(real) = std::fs::canonicalize(dev) else {
        return Vec::new();
    };
    let Some(name) = real.file_name().map(|n| n.to_string_lossy().to_string()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    resolve_disks(Path::new("/sys/class/block"), &name, &mut out, 0);
    out.sort();
    out.dedup();
    out
}

/// Walk device-mapper slaves and partition parents down to whole disks.
fn resolve_disks(sys_block: &Path, name: &str, out: &mut Vec<String>, depth: u32) {
    // LVM-on-LVM or dm stacking is bounded in practice; the cap only guards
    // against a cyclic sysfs layout.
    if depth > 8 {
        return;
    }
    // dm device: recurse into each slave.
    if let Ok(rd) = std::fs::read_dir(sys_block.join(name).join("slaves")) {
        let slaves: Vec<_> = rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        if !slaves.is_empty() {
            for s in slaves {
                resolve_disks(sys_block, &s, out, depth + 1);
            }
            return;
        }
    }
    // Partition: the parent disk's sysfs dir contains the partition's dir.
    if sys_block.join(name).join("partition").exists()
        && let Ok(rd) = std::fs::read_dir(sys_block)
    {
        for e in rd.flatten() {
            let parent = e.file_name().to_string_lossy().to_string();
            if parent != name && sys_block.join(&parent).join(name).exists() {
                resolve_disks(sys_block, &parent, out, depth + 1);
                return;
            }
        }
    }
    out.push(name.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mkdirs(root: &Path, paths: &[&str]) {
        for p in paths {
            std::fs::create_dir_all(root.join(p)).unwrap();
        }
    }

    fn touch(root: &Path, path: &str) {
        std::fs::write(root.join(path), "").unwrap();
    }

    #[test]
    fn sd_disks_are_classified_by_vendor_then_host_driver() {
        assert_eq!(classify_sd("ATA     ", "ahci"), SdKind::Ata);
        assert_eq!(
            classify_sd("ATA     ", "megaraid_sas"),
            SdKind::Ata,
            "SATA pass-through"
        );
        assert_eq!(
            classify_sd("DELL    ", "megaraid_sas\n"),
            SdKind::Raid("megaraid_sas".into())
        );
        assert_eq!(
            classify_sd("HPE     ", "smartpqi"),
            SdKind::Raid("smartpqi".into())
        );
        assert_eq!(classify_sd("SEAGATE ", "mpt3sas"), SdKind::Scsi);
        assert_eq!(classify_sd("", ""), SdKind::Scsi);
    }

    #[test]
    fn scsi_cache_type_is_parsed() {
        assert_eq!(parse_cache_type("write back\n"), CacheState::Enabled);
        assert_eq!(
            parse_cache_type("write back, no read (daft)"),
            CacheState::Enabled
        );
        assert_eq!(parse_cache_type("write through\n"), CacheState::Disabled);
        assert_eq!(parse_cache_type("none"), CacheState::NoVolatileCache);
        assert!(matches!(parse_cache_type("bogus"), CacheState::Unknown(_)));
    }

    #[test]
    fn whole_disk_resolves_to_itself() {
        let tmp = std::env::temp_dir().join("twinstor-test-wc-disk");
        let _ = std::fs::remove_dir_all(&tmp);
        mkdirs(&tmp, &["nvme0n1"]);
        let mut out = Vec::new();
        resolve_disks(&tmp, "nvme0n1", &mut out, 0);
        assert_eq!(out, vec!["nvme0n1"]);
    }

    #[test]
    fn partition_resolves_to_parent_disk() {
        let tmp = std::env::temp_dir().join("twinstor-test-wc-part");
        let _ = std::fs::remove_dir_all(&tmp);
        mkdirs(&tmp, &["nvme0n1/nvme0n1p3", "nvme0n1p3"]);
        touch(&tmp, "nvme0n1p3/partition");
        let mut out = Vec::new();
        resolve_disks(&tmp, "nvme0n1p3", &mut out, 0);
        assert_eq!(out, vec!["nvme0n1"]);
    }

    #[test]
    fn nvme_controller_strips_namespace() {
        assert_eq!(nvme_controller("nvme0n1"), "nvme0");
        assert_eq!(nvme_controller("nvme11n2"), "nvme11");
        // Already a controller name, or no namespace suffix: unchanged.
        assert_eq!(nvme_controller("nvme0"), "nvme0");
        // SATA names are passed straight through (never routed here, but be safe).
        assert_eq!(nvme_controller("sdb"), "sdb");
    }

    #[test]
    fn lv_resolves_through_slaves_to_disks() {
        // dm-0 (LV) -> sda3 (PV partition) -> sda, plus a second PV disk.
        let tmp = std::env::temp_dir().join("twinstor-test-wc-lv");
        let _ = std::fs::remove_dir_all(&tmp);
        mkdirs(
            &tmp,
            &[
                "dm-0/slaves/sda3",
                "sda3",
                "sda/sda3",
                "dm-0/slaves/sdb",
                "sdb",
            ],
        );
        touch(&tmp, "sda3/partition");
        let mut out = Vec::new();
        resolve_disks(&tmp, "dm-0", &mut out, 0);
        out.sort();
        assert_eq!(out, vec!["sda", "sdb"]);
    }
}
