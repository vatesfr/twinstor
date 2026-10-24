use tracing::{debug, info, warn};

/// Run an `xe ... --minimal` query with a 5 s bound, so a hung toolstack cannot
/// stall a daemon tick. `None` on timeout, spawn failure or non-zero exit;
/// otherwise the trimmed stdout, which may be empty (callers decide what empty
/// means).
async fn xe_minimal(args: &[&str]) -> Option<String> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new("xe")
            .args(args)
            .arg("--minimal")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run an `xe ...` query without `--minimal`, bounded like [`xe_minimal`].
/// This keeps the multi-line `param: value` record format, which SR IQN
/// discovery needs to read the `device-config` map of each PBD record.
/// `None` on timeout, spawn failure or non-zero exit.
async fn xe_raw(args: &[&str]) -> Option<String> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new("xe")
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// First comma-separated token of an `xe ... --minimal` result, or `None` if
/// empty. `--minimal` comma-joins the values when a query matches more than one
/// object; callers that expect one match use this so a multi-match degrades to
/// the first value instead of passing a comma-joined string on.
fn first_token(s: &str) -> Option<String> {
    let t = s.split(',').next().unwrap_or("").trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Whether this host is the pool master, via live XAPI. On a slave this
/// `xe pool-list` is forwarded to the master over the slave-to-master XAPI
/// connection, so it returns `None` whenever that path is down. For decisions
/// that must survive a partition (split-brain resolution) use
/// [`is_pool_master_resilient`] instead.
pub async fn is_pool_master() -> Option<bool> {
    let pool_master = xe_minimal(&["pool-list", "params=master"]).await?;
    let my_hostname = hostname::get().ok()?.to_string_lossy().to_string();
    let my_uuid = xe_minimal(&[
        "host-list",
        &format!("hostname={my_hostname}"),
        "params=uuid",
    ])
    .await?;
    if pool_master.is_empty() || my_uuid.is_empty() {
        return None;
    }
    Some(pool_master == my_uuid)
}

/// The pool master's hostname (master UUID from `pool-list`, then its
/// `hostname`), shown in `twinstor status`. `None` if XAPI cannot answer, for
/// example on a slave whose link to the master is wedged; the caller renders
/// "unknown".
pub async fn pool_master_hostname() -> Option<String> {
    let master_uuid = xe_minimal(&["pool-list", "params=master"]).await?;
    if master_uuid.is_empty() {
        return None;
    }
    let host = xe_minimal(&[
        "host-list",
        &format!("uuid={master_uuid}"),
        "params=hostname",
    ])
    .await?;
    first_token(&host).filter(|s| !s.is_empty())
}

/// The peer's management address as XAPI records it (`xe host-list
/// hostname=<peer> params=address`). XAPI uses this path for inter-host RPC,
/// including the pool-master demotion that the dual-master auto-heal triggers
/// with a toolstack restart; in an arbitrated topology it differs from the
/// DRBD replication link. The address is cached in the local DB, so it is
/// answerable even while the pool is wedged. `None` if XAPI cannot answer or
/// returns empty.
pub async fn peer_management_address(peer_hostname: &str) -> Option<String> {
    let addr = xe_minimal(&[
        "host-list",
        &format!("hostname={peer_hostname}"),
        "params=address",
    ])
    .await?;
    // A duplicate hostname would comma-join two addresses; take the first so
    // the ping gate gets a usable address.
    first_token(&addr)
}

/// This host's UUID from the XCP-ng inventory (`INSTALLATION_UUID`). A local
/// file read, so it works when the slave-to-master link is down, unlike a
/// hostname lookup that a slave forwards to the master.
pub(crate) async fn local_host_uuid() -> Option<String> {
    let inv = tokio::fs::read_to_string("/etc/xensource-inventory")
        .await
        .ok()?;
    inv.lines().find_map(|l| {
        l.strip_prefix("INSTALLATION_UUID=")
            .map(|v| v.trim().trim_matches('\'').trim_matches('"').to_string())
    })
}

/// Names of the VMs XAPI believes are resident on this host, excluding the
/// control domain. Used at split-brain resolution to tell the operator whose
/// recent writes on the loser were rolled back. Best-effort: one bounded query,
/// so a slow or unreachable toolstack yields `None` and never delays the
/// discard. The value is `xe`'s comma-joined name list, empty when no VMs are
/// resident.
pub async fn resident_vm_names() -> Option<String> {
    let uuid = local_host_uuid().await?;
    xe_minimal(&[
        "vm-list",
        &format!("resident-on={uuid}"),
        "is-control-domain=false",
        "params=name-label",
    ])
    .await
}

/// Path to the recorded SR UUID state file, under `/var/lib/twinstor` unless
/// overridden by `TWINSTOR_SR_UUID_FILE`. Mirrors `ha_rearm_obligation_path()`.
pub fn sr_uuid_path() -> String {
    std::env::var("TWINSTOR_SR_UUID_FILE")
        .unwrap_or_else(|_| "/var/lib/twinstor/sr-uuid".to_string())
}

/// The recorded TwinStor SR UUID, or `None` if the state file is absent or
/// empty. An SR UUID survives a `name-label` rename, so reads need no
/// re-verification.
pub fn read_recorded_sr_uuid() -> Option<String> {
    let s = std::fs::read_to_string(sr_uuid_path()).ok()?;
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Write the TwinStor SR UUID to the state file, creating the parent directory.
/// Called by setup, by the daemon on first run, and by `twinstor sr record/set`.
pub fn write_recorded_sr_uuid(uuid: &str) -> std::io::Result<()> {
    if let Some(dir) = std::path::Path::new(&sr_uuid_path()).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(sr_uuid_path(), format!("{}\n", uuid.trim()))
}

/// UUID of the TwinStor SR, which is also the HA heartbeat SR. Used to re-arm
/// pool HA after dual-master recovery, and for PBD re-plug, capacity and
/// `doctor`. `None` if it cannot be resolved.
pub async fn twinstor_sr_uuid() -> Option<String> {
    // Prefer the recorded UUID, which survives an SR rename. `record_sr_uuid`
    // writes it at setup, on first run and from `twinstor sr record`.
    if let Some(u) = read_recorded_sr_uuid() {
        return Some(u);
    }
    // Fallback for installs that predate the recording: setup names the SR
    // "twinstor". If two SRs carry that name, take the first UUID rather than
    // pass a comma-joined pair to `pool-ha-enable heartbeat-sr-uuids=`.
    let u = xe_minimal(&["sr-list", "name-label=twinstor", "params=uuid"]).await?;
    first_token(&u)
}

/// Extract the `targetIQN` value from an lvmoiscsi PBD `device-config` line,
/// ending at the `;` field separator, so callers can compare it exactly. A
/// substring match would also accept a foreign SR whose IQN has ours as a
/// prefix (such as `...:twinstor-data-old`).
fn device_config_target_iqn(line: &str) -> Option<&str> {
    let after = line.split("targetIQN:").nth(1)?;
    let v = after.split(';').next()?.trim();
    if v.is_empty() { None } else { Some(v) }
}

/// Find the SR UUID whose PBD `device-config` names `iqn`, from `xe pbd-list
/// params=sr-uuid,device-config` output.
///
/// The output is a sequence of records separated by blank lines. Each record
/// has an `sr-uuid ( RO): <uuid>` line and a `device-config (MRO):
/// ...targetIQN: <iqn>...` line, in either order, so a match seen before the
/// `sr-uuid` line is remembered until that line arrives.
pub(crate) fn parse_sr_uuid_for_iqn(pbd_list: &str, iqn: &str) -> Option<String> {
    if iqn.is_empty() {
        return None;
    }
    let mut cur_uuid: Option<String> = None;
    let mut cur_matches = false;
    for line in pbd_list.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            // Record boundary: reset.
            cur_uuid = None;
            cur_matches = false;
            continue;
        }
        // `sr-uuid ( RO)       : <uuid>`: the key is the text before the first
        // ':'.
        if let Some((key, val)) = trimmed.split_once(':')
            && key.trim_start().starts_with("sr-uuid")
        {
            let uuid = val.trim().to_string();
            if cur_matches && !uuid.is_empty() {
                return Some(uuid);
            }
            cur_uuid = if uuid.is_empty() { None } else { Some(uuid) };
            continue;
        }
        // Exact match on the `targetIQN` field, not a substring of the line, so
        // a foreign SR whose IQN has ours as a prefix does not match.
        if device_config_target_iqn(trimmed) == Some(iqn) {
            if let Some(u) = &cur_uuid {
                return Some(u.clone());
            }
            cur_matches = true;
        }
    }
    None
}

/// Discover the TwinStor SR UUID by matching a PBD `device-config` target IQN.
/// Does not write the state file (used by `record_sr_uuid`, and by `sr set` to
/// warn about a wrong-SR override). `None` if xapi cannot answer or no PBD
/// matches the IQN.
pub async fn discover_sr_uuid_by_iqn(target_iqn: &str) -> Option<String> {
    let out = xe_raw(&["pbd-list", "params=sr-uuid,device-config"]).await?;
    parse_sr_uuid_for_iqn(&out, target_iqn)
}

/// Discover the TwinStor SR by its target IQN and record it, falling back to
/// the name-label lookup. Returns the UUID, and writes the state file, on
/// success.
pub async fn record_sr_uuid(target_iqn: &str) -> Option<String> {
    // 1. IQN discovery (survives an SR rename).
    if let Some(u) = discover_sr_uuid_by_iqn(target_iqn).await {
        let _ = write_recorded_sr_uuid(&u);
        return Some(u);
    }
    // 2. Name-label fallback.
    if let Some(u) = xe_minimal(&["sr-list", "name-label=twinstor", "params=uuid"])
        .await
        .and_then(|s| first_token(&s))
    {
        let _ = write_recorded_sr_uuid(&u);
        return Some(u);
    }
    None
}

/// Whether an SR with this UUID exists. `twinstor sr set` uses it to validate an
/// operator-supplied UUID before recording it.
pub async fn sr_uuid_exists(uuid: &str) -> bool {
    xe_minimal(&["sr-list", &format!("uuid={uuid}"), "params=uuid"])
        .await
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// The TwinStor SR's physical utilisation as a whole percent (0-100), or `None`
/// if XAPI cannot answer, the SR is unreadable, or its size is zero. A filling
/// SR first shows up as guest ENOSPC, so the capacity watchdog and `twinstor
/// status` read it here.
pub async fn twinstor_sr_utilisation_pct() -> Option<u8> {
    let sr = twinstor_sr_uuid().await?;
    let used: u64 = xe_minimal(&[
        "sr-param-get",
        &format!("uuid={sr}"),
        "param-name=physical-utilisation",
    ])
    .await?
    .parse()
    .ok()?;
    let size: u64 = xe_minimal(&[
        "sr-param-get",
        &format!("uuid={sr}"),
        "param-name=physical-size",
    ])
    .await?
    .parse()
    .ok()?;
    if size == 0 {
        return None;
    }
    Some((used.saturating_mul(100) / size).min(100) as u8)
}

/// The pool's configured HA failover timeout in seconds, from the live
/// `ha-configuration` map. Read before a survivor HA reset disables HA, so the
/// re-arm restores the configured value (arbitrated pools run a longer timeout
/// than fate-shared ones) instead of a fixed default. `None` if HA is off, the
/// key is absent, or XAPI cannot answer; callers then use the fate-shared
/// default.
pub async fn pool_ha_timeout() -> Option<u32> {
    let pool = xe_minimal(&["pool-list", "params=uuid"]).await?;
    if pool.is_empty() {
        return None;
    }
    xe_minimal(&[
        "pool-param-get",
        &format!("uuid={pool}"),
        "param-name=ha-configuration",
        "param-key=timeout",
    ])
    .await?
    .parse()
    .ok()
}

/// XAPI's pool bootstrap file. It records this host's own role: the literal
/// `master`, or `slave:<master-ip>`. XAPI writes and reads it locally to decide
/// its role at startup, so it answers "am I the master?" without the network
/// or a slave-to-master round-trip.
const POOL_CONF_PATH: &str = "/etc/xensource/pool.conf";

/// This host's pool role parsed from a pool.conf body: `Some(true)` master,
/// `Some(false)` slave, `None` if unrecognized. Only the role matters, not the
/// recorded master address, so a slave whose master IP is stale or even points
/// at itself still resolves unambiguously to `Some(false)`.
fn parse_pool_role(contents: &str) -> Option<bool> {
    match contents.trim() {
        "master" => Some(true),
        s if s.starts_with("slave:") => Some(false),
        _ => None,
    }
}

/// Local-only mastership read from [`POOL_CONF_PATH`]. `None` if the file is
/// missing or unparseable.
fn pool_master_from_conf() -> Option<bool> {
    parse_pool_role(&std::fs::read_to_string(POOL_CONF_PATH).ok()?)
}

/// This host's pool role from the local `pool.conf` (`Some(true)` master,
/// `Some(false)` slave, `None` unreadable). A cheap local read used by the
/// mastership-flap detector to spot the control plane oscillating while storage
/// is healthy. See CONVERGENCE.md.
pub fn local_pool_role() -> Option<bool> {
    pool_master_from_conf()
}

/// This host's XAPI enabled state and operator-maintenance marker, for the
/// boot-race auto-enable watchdog: `(enabled, operator_maintenance)`.
/// `None` when xe cannot answer (never guess about enabling a host then).
/// The marker is XO's `other-config:MAINTENANCE_MODE=true`, which operator
/// flows set and the boot-time enable race does not.
pub async fn local_host_enabled_state() -> Option<(bool, bool)> {
    let uuid = local_host_uuid().await?;
    let enabled = crate::cmd::cmd_output_for(
        "xe",
        &[
            "host-param-get",
            &format!("uuid={uuid}"),
            "param-name=enabled",
        ],
        10,
    )
    .await
    .ok()?;
    let enabled = match enabled.trim() {
        "true" => true,
        "false" => false,
        _ => return None,
    };
    // Unreadable is unknown, not "no marker": a failed read must never let
    // the watchdog enable a host an operator put in maintenance.
    let oc = crate::cmd::cmd_output_for(
        "xe",
        &[
            "host-param-get",
            &format!("uuid={uuid}"),
            "param-name=other-config",
        ],
        10,
    )
    .await
    .ok()?;
    let maintenance = oc.contains("MAINTENANCE_MODE: true") || oc.contains("MAINTENANCE_MODE:true");
    Some((enabled, maintenance))
}

/// Parse the maintenance state from `xe host-list params=other-config` output.
/// `None` input (the query timed out or the toolstack did not answer) maps to
/// `None`, unknown, not to "no maintenance": the caller must fail closed,
/// because an unresponsive toolstack is typical of a rolling pool update or
/// maintenance, which the HA re-arm (including its toolstack restart) must not
/// fight.
fn parse_maintenance(out: Option<&str>) -> Option<bool> {
    let s = out?;
    Some(s.contains("MAINTENANCE_MODE: true") || s.contains("MAINTENANCE_MODE:true"))
}

/// Whether any pool host carries the operator maintenance marker
/// (`other-config:MAINTENANCE_MODE=true`, set by XO's maintenance and rolling
/// pool update flows). An operator in maintenance wants HA off, so the HA
/// re-arm defers to them. `Some(true)`: a host is in maintenance;
/// `Some(false)`: confirmed none is; `None`: the toolstack could not be
/// queried, which the aggressive re-arm treats like maintenance.
pub async fn any_host_in_maintenance() -> Option<bool> {
    let out =
        crate::cmd::cmd_output_for("xe", &["host-list", "params=other-config", "--minimal"], 10)
            .await;
    parse_maintenance(out.ok().as_deref())
}

/// UUIDs of VMs with an HA restart priority that are currently halted: the
/// watchdog's alert set (see `xapi::halted_always_run_vms` for the ones it
/// may start). Server-side filtered. Snapshots and templates keep the restart
/// priority of the VM they were taken from but can never run: they are not
/// halted guests.
pub async fn halted_protected_vms() -> Option<Vec<String>> {
    halted_vm_list(&[]).await
}

/// The subset of [`halted_protected_vms`] XAPI itself still counts as
/// protected (`Helpers.is_xha_protected`: restart priority and
/// `ha-always-run`): the watchdog's start set. XAPI clears `ha-always-run` on
/// every halt meant to last (a shutdown or hard shutdown through XAPI, and a
/// halt xenopsd reports when the pool's `ha-reboot-vm-on-internal-shutdown`
/// is off) and never on an HA restart it gave up on or a dead host's reset;
/// `VM.start` sets it again. The priority alone also selects a VM an operator
/// shut down months ago.
pub async fn halted_always_run_vms() -> Option<Vec<String>> {
    halted_vm_list(&["ha-always-run=true"]).await
}

/// Halted VMs carrying [`DISKS_SUSPECT_KEY`]: never started by the watchdog.
pub async fn halted_disks_suspect_vms() -> Option<Vec<String>> {
    xe_vm_uuids(&[
        "vm-list",
        "power-state=halted",
        &format!("other-config:{DISKS_SUSPECT_KEY}=true"),
    ])
    .await
}

/// Running VMs still carrying [`DISKS_SUSPECT_KEY`]: candidates for
/// [`disks_suspect_mark_spent`].
pub async fn running_disks_suspect_vms() -> Option<Vec<String>> {
    xe_vm_uuids(&[
        "vm-list",
        "power-state=running",
        &format!("other-config:{DISKS_SUSPECT_KEY}=true"),
    ])
    .await
}

/// VM `other-config` key marking a guest the crash boundary caught running
/// across a discard (its disks are suspect): the protected-VM watchdog never
/// starts it. Removed once the VM has been started after the mark (an
/// operator's start after the checks the alert names).
pub const DISKS_SUSPECT_KEY: &str = "twinstor-disks-suspect";
/// When the mark was set, in XAPI's own UTC date format (as `start-time`).
pub const DISKS_SUSPECT_AT_KEY: &str = "twinstor-disks-suspect-at";

/// Now in XAPI's date format (`20261020T06:21:11Z`, UTC): fixed width, so two
/// such dates compare as strings.
pub fn xapi_utc_now() -> String {
    let t = time::OffsetDateTime::now_utc();
    format!(
        "{:04}{:02}{:02}T{:02}:{:02}:{:02}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// Whether a running VM's disks-suspect mark is spent: the VM was
/// started after the mark was set (both in XAPI's date format). A mark
/// without a readable date counts as spent once the VM runs; an unreadable
/// start time never does.
pub fn disks_suspect_mark_spent(start_time: &str, mark_at: Option<&str>) -> bool {
    let xapi_date = |d: &str| d.len() == 18 && d.as_bytes()[8] == b'T' && d.ends_with('Z');
    if !xapi_date(start_time) {
        return false;
    }
    match mark_at {
        Some(m) if xapi_date(m) => start_time > m,
        _ => true,
    }
}

/// Set (`true`, with the date) or remove [`DISKS_SUSPECT_KEY`] and its date on
/// a VM. `true` when XAPI accepted it.
pub async fn mark_disks_suspect(uuid: &str, on: bool) -> bool {
    if on {
        xe_minimal(&[
            "vm-param-set",
            &format!("uuid={uuid}"),
            &format!("other-config:{DISKS_SUSPECT_KEY}=true"),
            &format!("other-config:{DISKS_SUSPECT_AT_KEY}={}", xapi_utc_now()),
        ])
        .await
        .is_some()
    } else {
        let _ = xe_minimal(&[
            "vm-param-remove",
            &format!("uuid={uuid}"),
            "param-name=other-config",
            &format!("param-key={DISKS_SUSPECT_AT_KEY}"),
        ])
        .await;
        xe_minimal(&[
            "vm-param-remove",
            &format!("uuid={uuid}"),
            "param-name=other-config",
            &format!("param-key={DISKS_SUSPECT_KEY}"),
        ])
        .await
        .is_some()
    }
}

/// One VM parameter (`xe vm-param-get`), or one key of a map parameter.
/// `None` when XAPI does not answer or the key is absent.
pub async fn vm_param(uuid: &str, name: &str, key: Option<&str>) -> Option<String> {
    let mut args = vec![
        "vm-param-get".to_string(),
        format!("uuid={uuid}"),
        format!("param-name={name}"),
    ];
    if let Some(k) = key {
        args.push(format!("param-key={k}"));
    }
    let argrefs: Vec<&str> = args.iter().map(String::as_str).collect();
    xe_minimal(&argrefs).await
}

/// The date a VM's disks-suspect mark carries, read with the whole
/// `other-config` map in one call: `None` when XAPI did not answer, `Some(None)`
/// when the map has no date (an undated mark).
pub async fn disks_suspect_mark_date(uuid: &str) -> Option<Option<String>> {
    let map = xe_minimal(&[
        "vm-param-get",
        &format!("uuid={uuid}"),
        "param-name=other-config",
    ])
    .await?;
    Some(map.split(';').find_map(|kv| {
        kv.trim()
            .strip_prefix(DISKS_SUSPECT_AT_KEY)
            .and_then(|r| r.strip_prefix(": "))
            .map(|v| v.trim().to_string())
    }))
}

/// A VM's name-label, for alerts. `None` when XAPI does not answer.
pub async fn vm_name_label(uuid: &str) -> Option<String> {
    xe_minimal(&[
        "vm-param-get",
        &format!("uuid={uuid}"),
        "param-name=name-label",
    ])
    .await
}

async fn halted_vm_list(extra: &[&str]) -> Option<Vec<String>> {
    let mut args = vec![
        "vm-list",
        "is-control-domain=false",
        "is-a-snapshot=false",
        "is-a-template=false",
        "power-state=halted",
        "ha-restart-priority=restart",
    ];
    args.extend_from_slice(extra);
    xe_vm_uuids(&args).await
}

async fn xe_vm_uuids(args: &[&str]) -> Option<Vec<String>> {
    let mut a: Vec<&str> = args.to_vec();
    a.extend_from_slice(&["params=uuid", "--minimal"]);
    let out = crate::cmd::cmd_output_for("xe", &a, 15).await.ok()?;
    Some(
        out.trim()
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// Start a VM by UUID. True on success.
pub async fn start_vm(uuid: &str) -> bool {
    crate::cmd::run_cmd_for("xe", &["vm-start", &format!("uuid={uuid}")], 120)
        .await
        .is_ok()
}

/// Start a VM by UUID on the named host. `Err` carries the first line of
/// XAPI's error text.
#[cfg_attr(test, allow(dead_code))]
pub async fn start_vm_on(uuid: &str, host: &str) -> Result<(), String> {
    // Bounded well under a typical guest start's worst case: a start that
    // hangs (the toolstack still settling right after a discard) is retried
    // soon instead of holding the guest down for minutes. A start that was
    // only slow is found running on the retry and counted as such.
    match crate::cmd::cmd_status_for(
        "xe",
        &["vm-start", &format!("uuid={uuid}"), &format!("on={host}")],
        45,
    )
    .await
    {
        Ok((true, _, _)) => Ok(()),
        Ok((false, out, err)) => Err(first_error_line(&err)
            .or_else(|| first_error_line(&out))
            .unwrap_or_else(|| "no error text".to_string())),
        Err(e) => Err(format!("{e:#}")),
    }
}

/// The first non-empty line of an `xe` error output, for logs and alerts.
fn first_error_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// Hard-stop a VM through XAPI (`vm-shutdown --force`): the domain is
/// destroyed without a guest shutdown, and XAPI records it halted, so HA does
/// not treat it as a failure to restart. True on success.
#[cfg_attr(test, allow(dead_code))]
pub async fn hard_shutdown_vm(uuid: &str) -> bool {
    crate::cmd::run_cmd_for(
        "xe",
        &["vm-shutdown", "--force", &format!("uuid={uuid}")],
        60,
    )
    .await
    .is_ok()
}

/// Clear a stale `running` record (`vm-reset-powerstate --force`). XAPI only
/// accepts it on the pool master for a VM resident there (it checks xenopsd
/// first), or when the resident host is dead. True on success.
#[cfg_attr(test, allow(dead_code))]
pub async fn reset_vm_powerstate(uuid: &str) -> bool {
    crate::cmd::run_cmd_for(
        "xe",
        &["vm-reset-powerstate", &format!("uuid={uuid}"), "--force"],
        60,
    )
    .await
    .is_ok()
}

/// Restart only this host's XAPI agent (`systemctl restart xapi`, not the full
/// toolstack restart, which also restarts the storage stack) and wait, bounded,
/// for `xe` to answer again. On start XAPI resyncs its VM records against
/// xenopsd, which clears a `running` record whose domain no longer exists; this
/// is the remedy XAPI itself prescribes for a live resident host. True once
/// `xe` answers again.
#[cfg_attr(test, allow(dead_code))]
pub async fn restart_local_agent_and_wait(max_wait: std::time::Duration) -> bool {
    if crate::cmd::run_cmd_for("systemctl", &["restart", "xapi"], 120)
        .await
        .is_err()
    {
        return false;
    }
    let deadline = std::time::Instant::now() + max_wait;
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        if xe_minimal(&["host-list", "params=uuid"])
            .await
            .is_some_and(|s| !s.is_empty())
        {
            return true;
        }
    }
    false
}

/// A VM's power state as XAPI reports it (`running`, `halted`, ...).
#[cfg_attr(test, allow(dead_code))]
pub async fn vm_power_state(uuid: &str) -> Option<String> {
    xe_minimal(&[
        "vm-param-get",
        &format!("uuid={uuid}"),
        "param-name=power-state",
    ])
    .await
    .filter(|s| !s.is_empty())
}

/// The host a VM is resident on (XAPI host UUID), if any.
#[cfg_attr(test, allow(dead_code))]
pub async fn vm_resident_on(uuid: &str) -> Option<String> {
    xe_minimal(&[
        "vm-param-get",
        &format!("uuid={uuid}"),
        "param-name=resident-on",
    ])
    .await
    .filter(|s| !s.is_empty() && !s.starts_with("<not in database>"))
}

/// The other host of this two-host pool (XAPI host UUID), by identity rather
/// than by name: the TwinStor node name and the XAPI name-label can differ.
#[cfg_attr(test, allow(dead_code))]
pub async fn peer_host_uuid(this_host: &str) -> Option<String> {
    let all = xe_minimal(&["host-list", "params=uuid"]).await?;
    let others: Vec<&str> = all
        .split(',')
        .map(str::trim)
        .filter(|u| !u.is_empty() && *u != this_host)
        .collect();
    match others.as_slice() {
        [one] => Some((*one).to_string()),
        _ => None,
    }
}

/// Disable this host (no new VM placement). True on success.
#[cfg_attr(test, allow(dead_code))]
pub async fn disable_local_host() -> bool {
    let Some(uuid) = local_host_uuid().await else {
        return false;
    };
    crate::cmd::run_cmd_for("xe", &["host-disable", &format!("uuid={uuid}")], 30)
        .await
        .is_ok()
}

/// Guests resident on this host with an attached disk on the SR `sr`. Three
/// bounded queries (resident VMs, the SR's VDIs, attached VBDs) joined by a
/// pure selector; `None` when any of them does not answer, so the caller can
/// never mistake an unreachable toolstack for "no guests".
#[cfg_attr(test, allow(dead_code))]
pub async fn local_guests_on_sr(sr: &str) -> Option<Vec<crate::loser_boundary::GuestRef>> {
    let host = local_host_uuid().await?;
    let vms = xe_raw(&[
        "vm-list",
        &format!("resident-on={host}"),
        "is-control-domain=false",
        "params=uuid,name-label",
    ])
    .await?;
    let vdis = xe_minimal(&["vdi-list", &format!("sr-uuid={sr}"), "params=uuid"]).await?;
    let vbds = xe_raw(&[
        "vbd-list",
        "currently-attached=true",
        "params=vm-uuid,vdi-uuid",
    ])
    .await?;
    Some(crate::loser_boundary::select_sr_guests(&vms, &vdis, &vbds))
}

/// The VDIs on SR `sr` that guest `vm`'s disks use. `None` when XAPI does not
/// answer.
pub async fn guest_vdis_on_sr(vm: &str, sr: &str) -> Option<Vec<String>> {
    let vbds = xe_minimal(&[
        "vbd-list",
        &format!("vm-uuid={vm}"),
        "type=Disk",
        "params=vdi-uuid",
    ])
    .await?;
    let vdis = xe_minimal(&["vdi-list", &format!("sr-uuid={sr}"), "params=uuid"]).await?;
    Some(crate::loser_boundary::guest_vdis_from_minimal(&vbds, &vdis))
}

/// Whether the named peer host's agent answers a slave-routed operation.
/// `host-metrics-live` is not enough: a slave in emergency mode keeps
/// reporting it true while refusing pool participation. `host-dmesg` must be
/// served by the slave's own agent, so it fails whenever the slave cannot
/// participate, emergency mode included.
pub async fn peer_agent_responsive(peer_name: &str) -> Option<bool> {
    match tokio::time::timeout(
        std::time::Duration::from_secs(15),
        crate::cmd::cmd_output("xe", &["host-dmesg", &format!("host={peer_name}")]),
    )
    .await
    {
        Ok(Ok(_)) => Some(true),
        Ok(Err(_)) => Some(false),
        Err(_) => Some(false), // hang counts as unresponsive
    }
}

/// Enable this host (undo the boot-race disabled state). True on success.
pub async fn enable_local_host() -> bool {
    let Some(uuid) = local_host_uuid().await else {
        return false;
    };
    crate::cmd::run_cmd_for("xe", &["host-enable", &format!("uuid={uuid}")], 60)
        .await
        .is_ok()
}

/// The host-local HA armed flag, read from xapi's local database file rather
/// than through `xe`: the mismatch it detects (locally disarmed while pool HA
/// is enabled) is the state in which the slave's `xe` stops answering (xapi
/// emergency mode).
pub fn local_ha_armed() -> Option<bool> {
    let text = std::fs::read_to_string("/var/lib/xcp/local.db").ok()?;
    let idx = text.find("\"ha.armed\"")?;
    let rest = &text[idx..];
    let vidx = rest.find("value=\"")? + 7;
    rest[vidx..].split('\"').next().map(|v| v == "true")
}

/// VDIs on the given SR whose sm_config carries a bare 'activating' lock with
/// no `host_` key: the half-acquired state a crashed activation leaves. A
/// completed acquisition always carries `host_<ref>`.
pub async fn bare_activating_vdis(sr: &str) -> Option<Vec<String>> {
    let out = crate::cmd::cmd_output_for(
        "xe",
        &[
            "vdi-list",
            &format!("sr-uuid={sr}"),
            "params=uuid,sm-config",
        ],
        15,
    )
    .await
    .ok()?;
    let mut stale = Vec::new();
    let mut uuid = String::new();
    for line in out.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("uuid ( RO)") {
            uuid = rest.trim_start_matches([':', ' ']).trim().to_string();
        } else if line.starts_with("sm-config")
            && line.contains("activating")
            && !line.contains("host_")
            && !uuid.is_empty()
        {
            stale.push(uuid.clone());
        }
    }
    Some(stale)
}

/// Clear a stale 'activating' lock. `xe` cannot (the map is read-only there),
/// so this uses the XenAPI Python bindings in dom0.
pub async fn clear_activating_lock(vdi_uuid: &str) -> bool {
    let script = format!(
        "import XenAPI\ns=XenAPI.xapi_local()\ns.xenapi.login_with_password('root','')\n\
         try:\n r=s.xenapi.VDI.get_by_uuid('{vdi_uuid}')\n \
         s.xenapi.VDI.remove_from_sm_config(r,'activating')\nfinally:\n \
         s.xenapi.session.logout()\n"
    );
    crate::cmd::run_cmd_for("python3", &["-c", &script], 30)
        .await
        .is_ok()
}

/// The master address recorded in pool.conf when this host is a slave (`None`
/// when master or unreadable). For the masterless-pool watchdog:
/// `slave:<one of our own addresses>` is an invalid state in a two-node pool
/// (xapi can demote a rejoining ex-master to slave of itself when XHA's
/// `propose_master` returns false), and it wedges `xe` pool-wide, so detection
/// must be this local file read, never an `xe` call.
pub fn local_slave_master_addr() -> Option<String> {
    let c = std::fs::read_to_string(POOL_CONF_PATH).ok()?;
    c.trim()
        .strip_prefix("slave:")
        .map(|s| s.trim().to_string())
}

/// Rewrite this host's pool.conf to follow `master_addr` as a slave. The active
/// rejoin uses this to unstick a node stranded slave-of-self: the master's
/// recover-slaves would normally point it at the master, but a self-pointing
/// node is not a registered slave, so that never arrives. Non-destructive (a
/// master pointer, no data); the caller restarts the toolstack so xapi re-reads
/// it, after which the node rejoins as the master's slave.
pub fn repoint_pool_conf_to_slave(master_addr: &str) -> std::io::Result<()> {
    std::fs::write(POOL_CONF_PATH, format!("slave:{master_addr}\n"))
}

/// Mastership for split-brain resolution, resilient to a wedged control plane.
///
/// Prefers the live XAPI answer, and falls back to the local pool.conf role
/// when XAPI cannot answer. A split-brain can wedge the loser's slave-to-master
/// XAPI connection (a partition, or a corrupted master address in pool.conf),
/// and resolution must still proceed then; the local file read is not affected
/// and still reports the master/slave role used to pick the winner.
pub async fn is_pool_master_resilient() -> Option<bool> {
    is_pool_master_resilient_sourced().await.0
}

/// [`is_pool_master_resilient`] plus where the answer came from: the `bool` is
/// `true` when live XAPI answered, `false` when it fell back to the local
/// `pool.conf` because the control plane could not be reached.
///
/// The split-brain tiebreak depends on this. A live "not master" answer means
/// slave-to-master forwarding works, so a reachable master exists elsewhere and
/// the loser must discard and converge on it. The pool.conf fallback is the
/// signature of a wedged control plane: when both nodes fall back and both read
/// "not master", neither is the sync source and a role-only resolver would
/// discard on both sides forever. A fallback answer therefore calls for the
/// deterministic, identity-based tiebreak.
pub async fn is_pool_master_resilient_sourced() -> (Option<bool>, bool) {
    match is_pool_master().await {
        Some(v) => (Some(v), true),
        None => (pool_master_from_conf(), false),
    }
}

/// Stamp this daemon's liveness (unix seconds) into the local host's XAPI
/// record. A cheap XAPI DB write that works even while the storage path is
/// down. The peer's daemon reads it to notice that this daemon is absent while
/// the host still looks healthy: systemd only covers crashes, not a clean exit
/// or a start-limit stop, and the data plane keeps serving without the daemon,
/// so nothing else would notice.
pub async fn stamp_daemon_alive(now_unix: u64, fs_freeze: bool) -> bool {
    let Some(uuid) = local_host_uuid_by_name().await else {
        return false;
    };
    // The same param-set advertises this daemon's version, so the peer can
    // detect a mixed-version pair during a rolling upgrade, and whether it runs
    // fate-shared freeze mode (engaged only when both nodes do).
    xe_minimal(&[
        "host-param-set",
        &format!("uuid={uuid}"),
        &format!("other-config:twinstor-alive={now_unix}"),
        &format!(
            "other-config:twinstor-version={}",
            env!("CARGO_PKG_VERSION")
        ),
        &format!(
            "other-config:twinstor-fs-freeze={}",
            if fs_freeze { "on" } else { "off" }
        ),
    ])
    .await
    .is_some()
}

/// Clear this node's `twinstor-alive` stamp so the peer reads it as not alive
/// at once rather than after the staleness threshold. Called on graceful
/// shutdown: during a rolling restart this node is then visibly down, which
/// lets the peer-alive interlock in `upgrade-apply` refuse a concurrent restart
/// on the other node.
pub async fn clear_daemon_alive() -> bool {
    let Some(uuid) = local_host_uuid_by_name().await else {
        return false;
    };
    xe_minimal(&[
        "host-param-remove",
        &format!("uuid={uuid}"),
        "param-name=other-config",
        "param-key=twinstor-alive",
    ])
    .await
    .is_some()
}

/// This node's own `twinstor-alive` stamp (unix seconds), or `None` if unset
/// or unreadable. `upgrade-apply` uses it to confirm the daemon re-initialised
/// after a restart (it re-stamps on its first tick), not merely that systemctl
/// started a process.
pub async fn local_daemon_alive_stamp() -> Option<u64> {
    let uuid = local_host_uuid_by_name().await?;
    xe_minimal(&[
        "host-param-get",
        &format!("uuid={uuid}"),
        "param-name=other-config",
        "param-key=twinstor-alive",
    ])
    .await
    .and_then(|s| s.trim().parse::<u64>().ok())
}

/// Set several `twinstor-*` keys in this host's `other_config` in one call (the
/// storage advertisement). Each pair becomes an `other-config:KEY=VALUE`
/// param. Returns false without writing when the local host UUID cannot be
/// read; the caller keeps its unpublished cache and retries next round.
pub async fn set_host_other_config(pairs: &[(&str, String)]) -> bool {
    if pairs.is_empty() {
        return true;
    }
    let Some(uuid) = local_host_uuid_by_name().await else {
        return false;
    };
    let mut args: Vec<String> = vec!["host-param-set".to_string(), format!("uuid={uuid}")];
    for (k, v) in pairs {
        args.push(format!("other-config:{k}={v}"));
    }
    let argrefs: Vec<&str> = args.iter().map(String::as_str).collect();
    xe_minimal(&argrefs).await.is_some()
}

/// Set several `twinstor-*` keys in the TwinStor SR's `other_config` in one
/// call (the SR-level advertisement). Only the pool master should call this:
/// the SR object is pool-wide, and a single writer avoids two daemons racing.
pub async fn set_sr_other_config(sr_uuid: &str, pairs: &[(&str, String)]) -> bool {
    if pairs.is_empty() {
        return true;
    }
    let mut args: Vec<String> = vec!["sr-param-set".to_string(), format!("uuid={sr_uuid}")];
    for (k, v) in pairs {
        args.push(format!("other-config:{k}={v}"));
    }
    let argrefs: Vec<&str> = args.iter().map(String::as_str).collect();
    xe_minimal(&argrefs).await.is_some()
}

/// The peer daemon's last alive stamp, whether XAPI still considers the peer
/// host live, and the TwinStor version the peer advertises:
/// `(stamp_unix, host_metrics_live, peer_version)`. Any field is `None` when
/// XAPI cannot answer or the peer never stamped (an older version without the
/// stamp). Callers must not treat a missing stamp as down, and must treat a
/// `None` version as unknown, never as a skew.
pub async fn peer_daemon_alive(peer_hostname: &str) -> (Option<u64>, Option<bool>, Option<String>) {
    let Some(uuid) = xe_minimal(&[
        "host-list",
        &format!("hostname={peer_hostname}"),
        "params=uuid",
    ])
    .await
    .and_then(|s| first_token(&s)) else {
        return (None, None, None);
    };
    let stamp = xe_minimal(&[
        "host-param-get",
        &format!("uuid={uuid}"),
        "param-name=other-config",
        "param-key=twinstor-alive",
    ])
    .await
    .and_then(|s| s.trim().parse::<u64>().ok());
    let version = xe_minimal(&[
        "host-param-get",
        &format!("uuid={uuid}"),
        "param-name=other-config",
        "param-key=twinstor-version",
    ])
    .await
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty());
    let live = match xe_minimal(&[
        "host-list",
        &format!("hostname={peer_hostname}"),
        "params=host-metrics-live",
    ])
    .await
    .as_deref()
    {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    };
    (stamp, live, version)
}

/// Whether the peer daemon advertises fate-shared freeze mode on
/// (`twinstor-fs-freeze` in its host `other_config`) from a release that has
/// it (`twinstor-version`, read in the same call). `None` when XAPI cannot
/// answer; a missing key (a release without freeze mode) reads as off.
pub async fn peer_fs_freeze(peer_hostname: &str) -> Option<bool> {
    let uuid = xe_minimal(&[
        "host-list",
        &format!("hostname={peer_hostname}"),
        "params=uuid",
    ])
    .await
    .and_then(|s| first_token(&s))?;
    let config = xe_minimal(&[
        "host-param-get",
        &format!("uuid={uuid}"),
        "param-name=other-config",
    ])
    .await?;
    Some(fs_freeze_in_other_config(&config))
}

/// `twinstor-fs-freeze: on` from a `twinstor-version` that carries freeze mode
/// (0.7.0 or later), among the `key: value; ...` pairs xe prints for an
/// `other_config` map. A downgraded peer leaves its stale `on` behind, but its
/// older daemon rewrites the version.
fn fs_freeze_in_other_config(config: &str) -> bool {
    let value = |key: &str| {
        config
            .split(';')
            .find_map(|kv| kv.trim().strip_prefix(key)?.strip_prefix(": "))
    };
    value("twinstor-fs-freeze") == Some("on")
        && value("twinstor-version").is_some_and(version_has_freeze)
}

/// When the peer advertises a release without freeze mode (0.6.x) while this
/// node runs one with it: the operator note for a mixed pair, else `None`.
/// The two weigh split-brain evidence differently and only the newer one stops
/// its guests before discarding.
pub fn mixed_release_note(local: &str, peer: &str) -> Option<String> {
    let older = {
        let mut it = peer.split(['.', '-']).map(str::parse::<u32>);
        matches!((it.next(), it.next()), (Some(Ok(major)), Some(Ok(minor))) if (major, minor) < (0, 7))
    };
    (version_has_freeze(local) && older).then(|| {
        "a release without 0.7.0's split-brain handling: until both nodes run 0.7.0 or later, the \
         two weigh a split-brain's evidence differently, and a 0.6.x node that loses one discards \
         its copy under its running VMs. Upgrade it now and do no network maintenance until both \
         are upgraded"
            .to_string()
    })
}

/// Whether a daemon advertising `version` carries freeze mode (0.7.0 and
/// later). An unparsable version reads as no.
pub fn version_has_freeze(version: &str) -> bool {
    let mut it = version.split(['.', '-']).map(str::parse::<u32>);
    match (it.next(), it.next()) {
        (Some(Ok(major)), Some(Ok(minor))) => (major, minor) >= (0, 7),
        _ => false,
    }
}

/// This host's XAPI UUID, resolved by hostname (same convention as
/// `send_alert`).
async fn local_host_uuid_by_name() -> Option<String> {
    let name = hostname::get().ok()?.to_string_lossy().to_string();
    xe_minimal(&["host-list", &format!("hostname={name}"), "params=uuid"])
        .await
        .and_then(|s| first_token(&s))
}

/// Raise the no-tiebreaker alert: the fence-peer's effective probe set (the
/// default gateway plus the ping-targets file) is empty or has been entirely
/// unreachable for a sustained period. In that state any replication
/// interruption, including a plain peer host death, freezes this node (nothing
/// to probe means yield), so a one-node failure becomes a full-pool outage.
/// Advisory only: the monitor behind it must never feed fencing decisions.
/// Returns whether it was delivered.
pub async fn send_no_tiebreaker_alert(body: &str) -> bool {
    send_alert("twinstor_no_tiebreaker", body).await
}

/// Raise the peer-daemon-down alert: the peer host is alive per XAPI but its
/// TwinStor daemon has not stamped its liveness for a sustained period, so
/// the peer's whole safety layer (readiness gating, split-brain resolution,
/// wedge healing, alerting) is absent while its data plane keeps serving.
/// Returns whether it was delivered (retry on a later tick if not).
pub async fn send_peer_daemon_down_alert(peer: &str, minutes: u64) -> bool {
    let body = format!(
        "TwinStor: the twinstor daemon on {peer} has not reported liveness for \
         ~{minutes} minutes although the host itself is up. Storage keeps being \
         served by the kernel, but that node's safety layer (failure handling, \
         split-brain resolution, alerting) is not running: the next incident will \
         hit it unprotected. On {peer} run: systemctl status twinstor; \
         journalctl -u twinstor -n 100; then systemctl restart twinstor."
    );
    // One condition, re-sent with its current figures: one held message.
    send_alert_keyed(
        "twinstor_peer_daemon_down",
        "twinstor_peer_daemon_down",
        &body,
    )
    .await
}

/// Count of guest VMs resident on the host named `hostname`. `None` when
/// XAPI cannot answer (callers must treat that as "cannot decide safely",
/// not as zero). Used by the replication-failback dance to pick the cheaper side.
pub async fn resident_vm_count(hostname: &str) -> Option<usize> {
    let host_uuid =
        xe_minimal(&["host-list", &format!("hostname={hostname}"), "params=uuid"]).await?;
    if host_uuid.is_empty() {
        return None;
    }
    let vms = xe_minimal(&[
        "vm-list",
        &format!("resident-on={host_uuid}"),
        "is-control-domain=false",
        "params=uuid",
    ])
    .await?;
    Some(vms.split(',').filter(|s| !s.trim().is_empty()).count())
}

/// Pool `other-config` key recording that twinstor turned the pool's
/// `ha-reboot-vm-on-internal-shutdown` off, and the value it found: `true` or
/// `false` (already off: twinstor changed nothing). Read back to restore it.
pub const INTERNAL_SHUTDOWN_PRIOR_KEY: &str = "twinstor-internal-shutdown-prior";

/// The pool's `ha-reboot-vm-on-internal-shutdown` and TwinStor's record of the
/// value it found (`INTERNAL_SHUTDOWN_PRIOR_KEY`). `None` when XAPI cannot
/// answer or has no such field (older than xapi 25.16).
pub async fn pool_internal_shutdown_policy() -> Option<(bool, Option<bool>)> {
    let pool = xe_minimal(&["pool-list", "params=uuid"]).await?;
    if pool.is_empty() {
        return None;
    }
    let cur = match xe_minimal(&[
        "pool-param-get",
        &format!("uuid={pool}"),
        "param-name=ha-reboot-vm-on-internal-shutdown",
    ])
    .await?
    .as_str()
    {
        "true" => true,
        "false" => false,
        _ => return None,
    };
    // The map read proves XAPI answers; the exact-key read then fails only
    // when the key is absent.
    xe_minimal(&[
        "pool-param-get",
        &format!("uuid={pool}"),
        "param-name=other-config",
    ])
    .await?;
    let prior = xe_minimal(&[
        "pool-param-get",
        &format!("uuid={pool}"),
        "param-name=other-config",
        &format!("param-key={INTERNAL_SHUTDOWN_PRIOR_KEY}"),
    ])
    .await
    .and_then(|v| match v.as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    });
    Some((cur, prior))
}

/// Turn the pool's `ha-reboot-vm-on-internal-shutdown` off and record the
/// value found (`prior`), in one call. `true` when XAPI accepted it.
pub async fn set_pool_internal_shutdown_off(prior: bool) -> bool {
    let Some(pool) = xe_minimal(&["pool-list", "params=uuid"])
        .await
        .filter(|p| !p.is_empty())
    else {
        return false;
    };
    xe_minimal(&[
        "pool-param-set",
        &format!("uuid={pool}"),
        "ha-reboot-vm-on-internal-shutdown=false",
        &format!("other-config:{INTERNAL_SHUTDOWN_PRIOR_KEY}={prior}"),
    ])
    .await
    .is_some()
}

/// Drop TwinStor's record, first putting `ha-reboot-vm-on-internal-shutdown`
/// back to `restore` when given. `true` when XAPI accepted both.
pub async fn restore_pool_internal_shutdown(restore: Option<bool>) -> bool {
    let Some(pool) = xe_minimal(&["pool-list", "params=uuid"])
        .await
        .filter(|p| !p.is_empty())
    else {
        return false;
    };
    // The setting and the record change in one `xe` invocation. XAPI still
    // applies them as successive calls, but the gap is milliseconds rather than
    // a whole call. A restore interrupted in between would leave the setting
    // on under a record saying TwinStor turned it off, and freeze mode would
    // never turn it off again. `restored` reads as no record.
    if let Some(v) = restore
        && xe_minimal(&[
            "pool-param-set",
            &format!("uuid={pool}"),
            &format!("ha-reboot-vm-on-internal-shutdown={v}"),
            &format!("other-config:{INTERNAL_SHUTDOWN_PRIOR_KEY}=restored"),
        ])
        .await
        .is_none()
    {
        return false;
    }
    let removed = xe_minimal(&[
        "pool-param-remove",
        &format!("uuid={pool}"),
        "param-name=other-config",
        &format!("param-key={INTERNAL_SHUTDOWN_PRIOR_KEY}"),
    ])
    .await
    .is_some();
    removed || restore.is_some()
}

/// Whether HA is enabled on the pool. `None` if XAPI is unreachable or the
/// value can't be parsed.
pub async fn is_ha_enabled() -> Option<bool> {
    match xe_minimal(&["pool-list", "params=ha-enabled"])
        .await?
        .as_str()
    {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// The name of a pool HA enable or disable task XAPI still holds pending
/// (one that never returned keeps the HA operations blocked), if any. None
/// also when XAPI does not answer: the caller's own command then fails.
pub async fn pending_pool_ha_task() -> Option<String> {
    let out = xe_minimal(&["task-list", "status=pending", "params=name-label"]).await?;
    pending_ha_task_in(&out)
}

/// `pending_pool_ha_task` over `xe task-list --minimal` output.
fn pending_ha_task_in(names: &str) -> Option<String> {
    names
        .split(',')
        .map(str::trim)
        .find(|n| n.ends_with("pool.disable_ha") || n.ends_with("pool.enable_ha"))
        .map(str::to_string)
}

/// Whether this host reports `enabled=true` in XAPI (toolstack converged).
///
/// `Some(true)`/`Some(false)` reflect XAPI's `host.enabled`; `None` means it
/// could not be determined (XAPI unreachable, timeout, or unexpected output).
/// Callers treat `None` as "cannot confirm" and must not block on it.
pub async fn is_host_enabled() -> Option<bool> {
    let my_hostname = hostname::get().ok()?.to_string_lossy().to_string();
    let out = xe_minimal(&[
        "host-list",
        &format!("hostname={my_hostname}"),
        "params=enabled",
    ])
    .await?;
    match out.as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Raise a degraded-storage alert (local disk failed, running without redundancy).
///
/// Returns whether the alert was delivered to XAPI. When the peer dies the
/// local toolstack may be unable to answer (if the dead node was the pool
/// master, `xe` fails until XHA elects this node and XAPI restarts), so callers
/// treat a failed send as "not alerted yet" and retry on a later tick.
pub async fn send_xapi_alert(body: &str) -> bool {
    send_alert("twinstor_degraded", body).await
}

/// [`send_xapi_alert`] for a message re-sent with changing content; `key`
/// names the message (see [`send_alert_keyed`]).
pub async fn send_xapi_alert_keyed(key: &str, body: &str) -> bool {
    send_alert_keyed("twinstor_degraded", key, body).await
}

/// Raise a split-brain alert. A separate message name so operators can filter
/// or alarm on it: it means writes may have been discarded.
///
/// Returns whether the alert was delivered to XAPI. XAPI may be down or
/// restarting during split-brain resolution, so callers treat a failed send as
/// "not alerted yet" and retry on a later tick.
pub async fn send_split_brain_alert(body: &str) -> bool {
    send_alert("twinstor_split_brain", body).await
}

/// [`send_split_brain_alert`] for a message re-sent with changing content;
/// `key` names the message (see [`send_alert_keyed`]).
pub async fn send_split_brain_alert_keyed(key: &str, body: &str) -> bool {
    send_alert_keyed("twinstor_split_brain", key, body).await
}

/// Raise an alert that HA has been disabled long enough to be a concern. The
/// pool is running DRBD dual-primary without fencing, so a partition risks
/// split-brain. The body carries the one-line remediation so the operator can
/// re-arm without looking it up.
///
/// Returns whether it was delivered (retry on a later tick if not).
pub async fn send_ha_disarmed_alert(minutes: u64) -> bool {
    let body = format!(
        "TwinStor: pool HA has been disabled for ~{minutes} min. Two consequences: \
         (1) your HA-protected VMs will not be restarted automatically after a host \
         failure until HA is re-armed; (2) TwinStor runs DRBD dual-primary, and \
         without HA armed there is no fencing, so a network partition can cause \
         split-brain and data corruption. If maintenance (e.g. a rolling pool \
         update) is complete, re-arm HA with the explicit form (a bare pool-ha-enable \
         uses the pool default_ha_timeout, or 60 s if unset): xe pool-ha-enable \
         heartbeat-sr-uuids=<twinstor SR uuid> ha-config:timeout=<120 fate-shared | 90 arbitrated>"
    );
    // One condition, re-sent with its current figures: one held message.
    send_alert_keyed("twinstor_ha_disarmed", "twinstor_ha_disarmed", &body).await
}

/// Raise the flapping-link alert: several split-brain episodes inside a short
/// window point to one bad cable, NIC or switch port, not independent
/// failures. Each episode already alerted on its own; this one names the
/// pattern and the runbook. Returns whether it was delivered (retry on a later
/// tick if not).
pub async fn send_flap_storm_alert(episodes: usize, window_min: u64, peer: &str) -> bool {
    let body = format!(
        "TwinStor: {episodes} split-brain episodes were auto-resolved in the last \
         ~{window_min} min. This pattern means a repeatedly flapping network link (bad \
         cable, NIC, or switch port) between this node and {peer}, not independent \
         failures. Every episode discards a few seconds of the losing node's writes and \
         crash-restarts its VMs. Runbook (see FAQ, 'What if a link keeps flapping'): \
         check/replace the cable first; if the flaky side is the pool master, move \
         mastership to the healthy node (xe pool-designate-new-master); exclude the \
         suspect host from the XO load-balancer plan or evacuate and disable it until \
         the hardware is fixed."
    );
    // One condition, re-sent with its current figures: one held message.
    send_alert_keyed("twinstor_flapping_link", "twinstor_flapping_link", &body).await
}

/// Raise the diverged-split-brain alert: the two replicas are a true fork.
/// Both nodes wrote committed data while disconnected, so no merge exists and
/// converging must roll back one side. TwinStor keeps the copy with more
/// unreplicated writes (losing the fewest) and rolls back the other; this alert
/// makes that loss visible. The body names the node whose writes were
/// discarded and its approximate unreplicated block count as of the last DRBD
/// handshake. The common trigger is an HA failover onto a stale node while the
/// fresher node was still serving; workloads most exposed to it should keep
/// their replicas anti-affine (never co-resident) so a single host loss cannot
/// fork the data. Returns whether it was delivered.
pub async fn send_split_brain_diverged_alert(key: &str, body: &str) -> bool {
    // Block counts change between handshakes, so the key names the fork and a
    // held version is replaced by the newer one of the same episode.
    send_alert_keyed("twinstor_split_brain_diverged", key, body).await
}

/// Raise the held-isolated alert: a host has been disabled with healthy storage
/// but an unreachable client network long enough that its auto-enable is held
/// (it will not run VMs clients cannot reach). Correct when the host really is
/// isolated, but if the default gateway simply does not answer ICMP the host
/// could stay disabled indefinitely, so this surfaces it. Returns whether it
/// was delivered.
pub async fn send_host_held_isolated_alert(minutes: u64) -> bool {
    let body = format!(
        "TwinStor: this host has been disabled with healthy storage but an unreachable \
         client network (default gateway) for ~{minutes} min, so TwinStor is not \
         auto-enabling it (enabling would run VMs clients cannot reach; if it is an \
         isolated master its VMs have moved to the reachable peer). If this host is in \
         fact reachable by clients and only its gateway does not answer ping, enable it \
         manually: xe host-enable host=<this host>. Otherwise reconnect its client network."
    );
    // One condition, re-sent with its current figures: one held message.
    send_alert_keyed(
        "twinstor_host_held_isolated",
        "twinstor_host_held_isolated",
        &body,
    )
    .await
}

/// Raise the cannot-go-online alert: this node has been failing to bring its
/// storage stack up for a sustained period (kernel module missing after an
/// update, invalid DRBD config, missing backing device, ...). Otherwise a node
/// stuck in its bring-up loop says nothing: the peer's degraded alert fires
/// but does not name the cause. A separate message name so operators can alarm
/// on it. Returns whether it was delivered (retry on a later tick if not).
pub async fn send_cannot_go_online_alert(body: &str) -> bool {
    // One condition, re-sent with its current duration: one held message.
    send_alert_keyed(
        "twinstor_cannot_go_online",
        "twinstor_cannot_go_online",
        body,
    )
    .await
}

/// Raise the reboot-drain alert on the next boot after this host rebooted while
/// holding the only UpToDate replica (cap reached, peer unreachable, or
/// operator override). The drain runs during shutdown, when xapi may already be
/// going away, so it cannot reliably send this; it leaves a marker that the
/// daemon turns into this alert on startup. `body` names the outcome and the
/// peer resync state at the time.
pub async fn send_reboot_unsafe_alert(body: &str) -> bool {
    send_alert("twinstor_reboot_unsafe", body).await
}

/// Raise the backing-device-missing alert: the configured DRBD backing device
/// path no longer resolves to a block device on this node. A replaced disk or
/// a recreated mdadm array leaves this behind: the device comes back under a
/// new /dev/disk/by-id path, the stored reference points at nothing, DRBD
/// cannot attach a local replica, and the node silently stops resyncing (no
/// local redundancy). Unlike the diskless alert (device present, DRBD
/// detached), the device path itself is gone, so the fix is to point TwinStor
/// at the replacement. The body names the missing device and the one-command
/// recovery. Returns whether it was delivered (retry on a later tick if not).
pub async fn send_backing_missing_alert(device: &str) -> bool {
    let body = format!(
        "TwinStor: the configured DRBD backing device {device} no longer exists on this \
         node. DRBD cannot attach a local replica to a device that is not there, so this \
         node is not resyncing and provides no local redundancy (it can only serve through \
         the peer, and only while the peer is healthy). A second failure would then lose \
         data. This is the shape a replaced disk or a recreated mdadm array leaves behind: \
         the disk/array came back under a new /dev/disk/by-id path, so the stored reference \
         no longer resolves. To fix, re-point twinstor at the replacement in one step: run \
         `twinstor replace-backing <new-device>` (it updates backing_device in \
         twinstor.toml, regenerates the DRBD resource, and attaches the new disk so the \
         full resync from the peer begins). If you have already hand-edited backing_device \
         in twinstor.toml to the new path, run `twinstor regenerate-config` instead."
    );
    send_alert("twinstor_backing_missing", &body).await
}

/// Raise the dual-master / HA-wedge alert: after a prolonged partition
/// the XHA statefile (which rides DRBD) forks, both hosts believe they are the
/// pool master, so the split-brain resolver finds two winners, neither
/// discards, and DRBD is stuck StandAlone. A separate message name so
/// operators can alarm on it. The body carries the collapse-to-one-master
/// recovery. Returns whether it was delivered (retry on a later tick if not).
pub async fn send_ha_wedge_alert(body: &str) -> bool {
    send_alert("twinstor_ha_wedge", body).await
}

/// Raise the replication-fallback alert (arbitrated mode): replication
/// has left the direct link and is running on the switch path. Safe (the
/// topology is temporarily fate-shared, which resists dual-run better, and
/// data stays replicated) but degraded: every synchronous write now shares the
/// switch with management, heartbeat and iSCSI traffic, and a single switch
/// fault would take the whole pool down. A separate message name so operators
/// can alarm on it. Returns whether it was delivered (retry on a later tick if
/// not).
pub async fn send_replication_fallback_alert(direct_peer: &str, failback_why: &str) -> bool {
    let body = format!(
        "TwinStor: replication is running on the switch fallback path instead of the \
         direct link (peer direct address {direct_peer}). Data is safe and stays \
         replicated, but writes now share the switch with all other traffic and the \
         switch is temporarily a single point of failure for the pool: do not take \
         the switch down for maintenance until `twinstor status` shows `repl path: \
         direct link`.{failback_why} Check the direct cable and NICs on both hosts \
         (carrier, link speed). Once the link is stable the daemon fails back by \
         itself after a short hold; to force it immediately run: twinstor \
         failback-replication (brief single-path moment on one node, no data loss)."
    );
    send_alert("twinstor_replication_fallback", &body).await
}

/// Pacing for alerts while XAPI cannot even name this host (toolstack still
/// starting, or the pool master unreachable). Callers retry an undelivered
/// alert every tick; without this gate each retry would run an `xe` lookup and
/// log a warning every few seconds while XAPI is down. Global, because it
/// describes XAPI, not any one alert.
#[derive(Debug, Default)]
struct AlertGate {
    next_at: Option<std::time::Instant>,
    backoff: std::time::Duration,
}

const ALERT_GATE_MIN: std::time::Duration = std::time::Duration::from_secs(30);
const ALERT_GATE_MAX: std::time::Duration = std::time::Duration::from_secs(120);

impl AlertGate {
    fn open(&self, now: std::time::Instant) -> bool {
        self.next_at.is_none_or(|t| now >= t)
    }

    /// The lookup failed: back off. True when this is the first failure since
    /// XAPI last answered (worth one warning).
    fn unreachable(&mut self, now: std::time::Instant) -> bool {
        let first = self.backoff.is_zero();
        self.backoff = if first {
            ALERT_GATE_MIN
        } else {
            (self.backoff * 2).min(ALERT_GATE_MAX)
        };
        self.next_at = Some(now + self.backoff);
        first
    }

    fn reachable(&mut self) {
        *self = Self::default();
    }
}

static ALERT_GATE: std::sync::Mutex<AlertGate> = std::sync::Mutex::new(AlertGate {
    next_at: None,
    backoff: std::time::Duration::ZERO,
});

/// Undelivered alerts, delivered once XAPI answers again: a caller that sends
/// an alert once per episode must not lose it to an XAPI outage. Kept per
/// name and message identity: one alert name carries distinct messages (the
/// split-brain discard, the suspect guests after it, the restart report, one
/// per VM), and none may overwrite another. The identity is the exact body,
/// except for a message re-sent with changing content ("holding for 5
/// minutes", then 6, ...): its caller passes a stable key
/// ([`send_alert_keyed`]), so it replaces its own earlier version instead of
/// piling up. Bounded per name and in total, oldest first.
#[derive(Debug, Default)]
struct PendingAlerts {
    /// (name, identity key, body)
    items: Vec<(String, String, String)>,
}

const PENDING_ALERTS_MAX: usize = 32;
const PENDING_PER_NAME_MAX: usize = 8;

impl PendingAlerts {
    fn push(&mut self, name: &str, key: &str, body: &str) {
        if let Some(item) = self
            .items
            .iter_mut()
            .find(|(n, k, _)| n == name && k == key)
        {
            // The same message, a newer version: keep its place, update it.
            item.2 = body.to_string();
            return;
        }
        if self.items.iter().filter(|(n, _, _)| n == name).count() >= PENDING_PER_NAME_MAX
            && let Some(i) = self.items.iter().position(|(n, _, _)| n == name)
        {
            let (n, _, b) = self.items.remove(i);
            warn!(alert = %n, body = %log_body(&b), "held-back XAPI alert dropped undelivered (too many of this name held)");
        }
        if self.items.len() >= PENDING_ALERTS_MAX {
            let (n, _, b) = self.items.remove(0);
            warn!(alert = %n, body = %log_body(&b), "held-back XAPI alert dropped undelivered (held set full)");
        }
        self.items
            .push((name.to_string(), key.to_string(), body.to_string()));
    }

    /// The message `(name, key)` reached XAPI: drop its held version.
    /// Whether one was held.
    fn delivered(&mut self, name: &str, key: &str) -> bool {
        let before = self.items.len();
        self.items.retain(|(n, k, _)| !(n == name && k == key));
        self.items.len() != before
    }

    /// Up to `max` pending alerts other than `(name, key)`, removed from the set.
    fn take(&mut self, name: &str, key: &str, max: usize) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        self.items.retain(|(n, k, b)| {
            if !(n == name && k == key) && out.len() < max {
                out.push((n.clone(), k.clone(), b.clone()));
                false
            } else {
                true
            }
        });
        out
    }
}

static PENDING_ALERTS: std::sync::Mutex<PendingAlerts> =
    std::sync::Mutex::new(PendingAlerts { items: Vec::new() });

/// Withdraw a held-back alert `(name, key)` (`key` as passed to the keyed
/// send) that no longer holds, so it is never delivered late. Whether a held
/// version was removed: `false` when it was delivered (possibly by a
/// background flush) or never held.
pub fn drop_held_alert(name: &str, key: &str) -> bool {
    let dropped = PENDING_ALERTS
        .lock()
        .map(|mut p| p.delivered(name, key))
        .unwrap_or(false);
    if dropped {
        info!(alert = %name, key = %log_body(key), "held-back XAPI alert withdrawn before delivery (no longer true)");
    }
    dropped
}

/// An alert body for the daemon log: one line (as sent to XAPI), bounded.
fn log_body(body: &str) -> String {
    let one = xe_message_body(body);
    if one.chars().count() > 1500 {
        format!("{}…", one.chars().take(1500).collect::<String>())
    } else {
        one
    }
}

/// Paces the operator-visible trace of an undeliverable alert (warn log +
/// kernel log): the same message (name and identity key, see
/// [`PendingAlerts`]) at most once per interval, and a bounded number of
/// distinct messages of one name per interval.
#[derive(Debug, Default)]
struct PerNameLimiter {
    last: Vec<(String, u64, u64)>,
}

const UNDELIVERED_TRACE_EVERY_SECS: u64 = 600;
const UNDELIVERED_TRACES_PER_NAME: usize = 8;

impl PerNameLimiter {
    fn allow(&mut self, name: &str, key: &str, now_secs: u64) -> bool {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut h);
        let key = h.finish();
        // Entries outside the interval (or ahead of a clock stepped back) expire.
        self.last
            .retain(|(_, _, t)| now_secs >= *t && now_secs - *t < UNDELIVERED_TRACE_EVERY_SECS);
        if self.last.iter().any(|(n, k, _)| n == name && *k == key) {
            return false;
        }
        if self.last.iter().filter(|(n, _, _)| n == name).count() >= UNDELIVERED_TRACES_PER_NAME {
            return false;
        }
        self.last.push((name.to_string(), key, now_secs));
        true
    }
}

static UNDELIVERED_TRACE: std::sync::Mutex<PerNameLimiter> =
    std::sync::Mutex::new(PerNameLimiter { last: Vec::new() });

/// An alert XAPI could not take: never dropped silently. Warn and write it to
/// the kernel log (paced per alert name), and keep it for delivery once XAPI
/// answers again.
fn undelivered(name: &str, key: &str, body: &str, why: &str) {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if UNDELIVERED_TRACE
        .lock()
        .map(|mut l| l.allow(name, key, now_secs))
        .unwrap_or(true)
    {
        warn!(alert = %name, why = %why, body = %log_body(body),
              "XAPI alert undeliverable; written to the kernel log and kept for delivery once XAPI answers");
        kmsg_fallback(name, body);
    } else {
        debug!(alert = %name, why = %why, "XAPI alert still undeliverable");
    }
    if let Ok(mut p) = PENDING_ALERTS.lock() {
        p.push(name, key, body);
    }
}

async fn message_create(host_uuid: &str, name: &str, body: &str) -> Result<(), String> {
    let body = xe_message_body(body);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tokio::process::Command::new("xe")
            .args([
                "message-create",
                &format!("host-uuid={host_uuid}"),
                &format!("name={name}"),
                "priority=1",
                &format!("body={body}"),
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await;
    match result {
        Err(_) => Err("timed out (60s); toolstack wedged?".to_string()),
        Ok(Err(e)) => Err(e.to_string()),
        Ok(Ok(out)) if out.status.success() => Ok(()),
        Ok(Ok(out)) => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
    }
}

/// An alert body as one line: `xe` refuses an argument containing a line
/// break ("Syntax error"), so a multi-line alert (the crash boundary's suspect
/// guests, the per-VM halted list) would never be delivered. Lines are joined
/// with ` | `, blank ones dropped, and any other control character becomes a
/// space.
fn xe_message_body(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" | ")
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

async fn send_alert(name: &str, body: &str) -> bool {
    send_alert_keyed(name, body, body).await
}

/// [`send_alert`] for a message re-sent with changing content: `key` is its
/// stable identity, so a held older version is replaced, not accumulated.
async fn send_alert_keyed(name: &str, key: &str, body: &str) -> bool {
    let now = std::time::Instant::now();
    if !ALERT_GATE.lock().map(|g| g.open(now)).unwrap_or(true) {
        undelivered(
            name,
            key,
            body,
            "XAPI could not name this host recently (retrying with backoff)",
        );
        return false;
    }
    let host_uuid = xe_minimal(&[
        "host-list",
        "params=uuid",
        &format!(
            "hostname={}",
            hostname::get().unwrap_or_default().to_string_lossy()
        ),
    ])
    .await
    .unwrap_or_default();

    if host_uuid.is_empty() {
        let _ = ALERT_GATE.lock().map(|mut g| g.unreachable(now));
        undelivered(name, key, body, "host UUID not found (XAPI not answering)");
        return false;
    }
    if let Ok(mut g) = ALERT_GATE.lock() {
        g.reachable();
    }
    let sent = match message_create(&host_uuid, name, body).await {
        Ok(()) => {
            info!(alert = %name, body = %log_body(body), "XAPI alert sent");
            if let Ok(mut p) = PENDING_ALERTS.lock() {
                p.delivered(name, key);
            }
            true
        }
        Err(e) => {
            undelivered(name, key, body, &e);
            false
        }
    };
    // XAPI answers again: deliver what an outage held back (a few per call,
    // each bounded), in the background, so a caller on a critical path (a
    // split-brain resolution) never waits on them.
    if sent {
        let held = PENDING_ALERTS
            .lock()
            .map(|mut p| p.take(name, key, 4))
            .unwrap_or_default();
        if !held.is_empty() && !cfg!(test) {
            tokio::spawn(async move {
                for (n, k, b) in held {
                    match message_create(&host_uuid, &n, &b).await {
                        Ok(()) => {
                            info!(alert = %n, body = %log_body(&b), "held-back XAPI alert delivered")
                        }
                        Err(e) => undelivered(&n, &k, &b, &e),
                    }
                }
            });
        }
    }
    sent
}

/// Last-resort operator channel for an undeliverable alert. A pool-wide xapi
/// wedge takes every alert with it, which is when an operator signal matters
/// most. The alert goes into the kernel log at ALERT priority: it survives in
/// `dmesg` and kern.log, shows on the console, and is captured by support
/// bundles. The `xe` delivery is still retried (by callers and the held-back
/// set); this never replaces it.
///
/// Paced by [`undelivered`] to one line per message (name and identity key)
/// per 10 minutes, and a bounded number of distinct messages per name:
/// retry-until-delivered callers re-attempt every tick, and the kernel's
/// devkmsg ratelimit is per open file (opened fresh on each call). A message
/// re-sent with changing content under a stable key counts once.
fn kmsg_fallback(name: &str, body: &str) {
    // One bounded line (kmsg truncates near 1 KiB), with every control
    // character (newlines, NULs, CRs) replaced so the record stays a single
    // parseable line. Truncation walks back to a char boundary: a plain
    // `String::truncate` panics inside a multibyte char, and this fallback
    // must not abort the daemon.
    let mut msg: String = body
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut cut = 800.min(msg.len());
    while !msg.is_char_boundary(cut) {
        cut -= 1;
    }
    msg.truncate(cut);
    let line = format!("<1>twinstor: undelivered alert ({name}): {msg}\n");
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = f.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod alert_delivery_tests {
    use super::{
        PENDING_ALERTS_MAX, PENDING_PER_NAME_MAX, PendingAlerts, PerNameLimiter,
        UNDELIVERED_TRACE_EVERY_SECS,
    };

    #[test]
    fn an_undelivered_alert_is_traced_per_message_never_muting_another() {
        let mut l = PerNameLimiter::default();
        assert!(l.allow("twinstor_split_brain", "discard", 1000));
        assert!(
            !l.allow("twinstor_split_brain", "discard", 1001),
            "the same alert repeating is paced"
        );
        assert!(
            l.allow("twinstor_split_brain", "suspect guests", 1001),
            "a distinct message of the same name shows"
        );
        assert!(
            l.allow("twinstor_degraded", "x", 1001),
            "a different alert still gets through"
        );
        assert!(l.allow("twinstor_split_brain", "restarts", 1002));
        assert!(l.allow("twinstor_split_brain", "hold", 1003));
        for m in ["e", "f", "g", "h"] {
            assert!(l.allow("twinstor_split_brain", m, 1003));
        }
        assert!(
            !l.allow("twinstor_split_brain", "ninth", 1004),
            "a bounded number of distinct messages per name per interval"
        );
        assert!(l.allow(
            "twinstor_split_brain",
            "discard",
            1000 + UNDELIVERED_TRACE_EVERY_SECS
        ));
        assert!(
            l.allow("twinstor_degraded", "x", 10),
            "a backwards clock step never mutes it"
        );
    }

    #[test]
    fn keyed_alert_replaces_itself_without_crowding_others() {
        // An undelivered "holding for N minutes" re-sent every tick of an
        // XAPI outage must not evict the other held alerts of its name.
        let mut p = PendingAlerts::default();
        let b = |s: &str| (s.to_string(), s.to_string());
        let (k1, b1) = b("discarded; restarting web on node-b");
        let (k2, b2) = b("suspect guests: db (VDIs 1, 2)");
        p.push("twinstor_split_brain", &k1, &b1);
        p.push("twinstor_split_brain", &k2, &b2);
        for m in 1..60 {
            p.push(
                "twinstor_split_brain",
                "boundary-hold",
                &format!("holding for {m} minutes, 3 VMs"),
            );
        }
        let held: Vec<&str> = p.items.iter().map(|(_, _, b)| b.as_str()).collect();
        assert_eq!(
            held,
            [b1.as_str(), b2.as_str(), "holding for 59 minutes, 3 VMs"]
        );
        p.delivered("twinstor_split_brain", "boundary-hold");
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.take("twinstor_split_brain", "boundary-hold", 4).len(), 2);

        let mut l = PerNameLimiter::default();
        assert!(l.allow("twinstor_degraded", "resync-stall", 1000));
        for m in 6..60 {
            assert!(
                !l.allow("twinstor_degraded", "resync-stall", 1000 + m),
                "one message, one trace"
            );
        }
        assert!(
            l.allow("twinstor_degraded", "a different alert", 1061),
            "never muted by the re-sent one"
        );
    }

    #[test]
    fn alerts_about_different_vms_never_merge() {
        // Two per-VM escalations differing only by digits in the VM name are
        // distinct operator actions; both are held and both traced.
        let mut p = PendingAlerts::default();
        let a = "the VM web-01 has still not been started after 10 attempts";
        let b = "the VM web-02 has still not been started after 10 attempts";
        p.push("twinstor_split_brain", a, a);
        p.push("twinstor_split_brain", b, b);
        assert_eq!(p.items.len(), 2);
        p.delivered("twinstor_split_brain", b);
        assert_eq!(
            p.items
                .iter()
                .map(|(_, _, x)| x.as_str())
                .collect::<Vec<_>>(),
            [a],
            "delivering one never drops the other"
        );
        let mut l = PerNameLimiter::default();
        assert!(l.allow("twinstor_split_brain", a, 1000));
        assert!(
            l.allow("twinstor_split_brain", b, 1000),
            "each VM's alert reaches the kernel log"
        );
    }

    #[test]
    fn a_withdrawn_held_alert_is_never_delivered_and_others_stay() {
        // A split-brain discard alert held by an XAPI outage, then a flip to
        // winner: the stale "this node discards" must not arrive later.
        let mut p = PendingAlerts::default();
        // Two versions of the discard alert under one key: withdrawing the
        // key removes whichever version is held.
        p.push(
            "twinstor_split_brain",
            "sb-discard-K",
            "discard (VM list unreadable)",
        );
        p.push("twinstor_split_brain", "sb-discard-K", "discard: web-01");
        p.push("twinstor_split_brain", "suspect", "suspect");
        assert!(
            p.delivered("twinstor_split_brain", "sb-discard-K"),
            "a held version was removed"
        );
        assert!(
            !p.delivered("twinstor_split_brain", "sb-discard-K"),
            "nothing left: it reached XAPI or was never held"
        );
        assert_eq!(
            p.take("x", "", 4)
                .iter()
                .map(|(_, _, b)| b.as_str())
                .collect::<Vec<_>>(),
            ["suspect"]
        );
    }

    #[test]
    fn an_evolving_hold_alert_never_crowds_out_one_shot_alerts() {
        // A node isolated from the master re-sends its hold alert every minute
        // with a new figure. Keyed, it stays one held message, and one-shot
        // alerts of the same name survive twenty versions of it.
        let mut p = PendingAlerts::default();
        p.push(
            "twinstor_degraded",
            "host-enable failed",
            "host-enable failed",
        );
        for n in 1..=20 {
            p.push(
                "twinstor_degraded",
                "init-hold",
                &format!("Holding for {n} minutes."),
            );
        }
        let held = p.take("x", "", 32);
        assert_eq!(held.len(), 2, "{held:?}");
        assert!(held.iter().any(|(_, _, b)| b == "host-enable failed"));
        assert!(held.iter().any(|(_, _, b)| b == "Holding for 20 minutes."));
    }

    #[test]
    fn held_back_alerts_are_kept_by_name_and_key_and_handed_out_later() {
        let mut p = PendingAlerts::default();
        let e = |n: &str, b: &str| (n.to_string(), b.to_string(), b.to_string());
        p.push("a", "one", "one");
        p.push("b", "two", "two");
        p.push("a", "three", "three");
        p.push("a", "three", "three");
        assert_eq!(
            p.take("b", "two", 4),
            vec![e("a", "one"), e("a", "three")],
            "distinct bodies of one name both kept; the alert being sent excluded"
        );
        assert_eq!(p.take("x", "", 4), vec![e("b", "two")]);
        assert!(p.take("x", "", 4).is_empty());
        p.push("c", "four", "four");
        p.push("c", "five", "five");
        p.delivered("c", "four");
        assert_eq!(
            p.take("x", "", 4),
            vec![e("c", "five")],
            "only the delivered message is dropped"
        );
        for i in 0..PENDING_PER_NAME_MAX + 3 {
            let m = format!("distinct message {}", "x".repeat(i + 1));
            p.push("noisy", &m, &m);
        }
        p.push("quiet", "once", "once");
        assert_eq!(
            p.items.iter().filter(|(n, _, _)| n == "noisy").count(),
            PENDING_PER_NAME_MAX,
            "a name keeps its latest few"
        );
        assert!(
            p.items.iter().any(|(n, _, _)| n == "quiet"),
            "never crowded out by a noisy name"
        );
        for i in 0..PENDING_ALERTS_MAX + 5 {
            p.push(&format!("n{i}"), "x", "x");
        }
        assert_eq!(p.items.len(), PENDING_ALERTS_MAX, "bounded");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_pending_pool_ha_task_is_found_by_name() {
        use super::pending_ha_task_in;
        assert_eq!(pending_ha_task_in(""), None);
        assert_eq!(pending_ha_task_in("VM.start, host.call_plugin"), None);
        assert_eq!(
            pending_ha_task_in("VM.start,pool.disable_ha"),
            Some("pool.disable_ha".into())
        );
        assert_eq!(
            pending_ha_task_in("Async.pool.enable_ha"),
            Some("Async.pool.enable_ha".into())
        );
    }

    use super::{ALERT_GATE_MAX, ALERT_GATE_MIN, AlertGate, parse_sr_uuid_for_iqn};
    use super::{
        first_error_line, first_token, fs_freeze_in_other_config, parse_maintenance,
        parse_pool_role,
    };

    #[test]
    fn fs_freeze_is_read_from_the_other_config_map() {
        assert!(fs_freeze_in_other_config(
            "twinstor-version: 0.7.0; twinstor-fs-freeze: on; twinstor-schema: 1"
        ));
        assert!(fs_freeze_in_other_config(
            "twinstor-fs-freeze: on; twinstor-version: 0.7.0-rc33"
        ));
        assert!(!fs_freeze_in_other_config(
            "twinstor-version: 0.7.0; twinstor-fs-freeze: off"
        ));
        assert!(
            super::mixed_release_note("0.7.0", "0.6.12")
                .is_some_and(|n| n.contains("0.7.0 or later"))
        );
        assert_eq!(
            super::mixed_release_note("0.7.0", "dev-build"),
            None,
            "an unparsable peer version is not called 0.6.x"
        );
        assert!(super::mixed_release_note("0.7.0-rc65", "0.6.12").is_some());
        assert_eq!(
            super::xe_message_body("Head:\n- a (u1): x\n\n- b\tc\r\n\nTail."),
            "Head: | - a (u1): x | - b c | Tail.",
            "one line for xe, nothing lost"
        );
        assert_eq!(super::xe_message_body("single line"), "single line");
        assert_eq!(
            super::mixed_release_note("0.7.0", "0.7.0-rc64"),
            None,
            "both carry it: the generic skew note only"
        );
        assert_eq!(super::mixed_release_note("0.6.12", "0.7.0"), None);
        assert!(!fs_freeze_in_other_config(
            "twinstor-version: 0.6.12; twinstor-alive: 1"
        ));
        // A downgraded peer: the stale key stays, the version does not.
        assert!(!fs_freeze_in_other_config(
            "twinstor-fs-freeze: on; twinstor-version: 0.6.12"
        ));
        assert!(!fs_freeze_in_other_config("twinstor-fs-freeze: on"));
        assert!(!fs_freeze_in_other_config(
            "twinstor-version: 0.7.0; twinstor-fs-freeze: onward"
        ));
        assert!(!fs_freeze_in_other_config(""));
    }

    #[test]
    fn an_unreachable_xapi_paces_alert_attempts_and_warns_once() {
        // With XAPI not up yet, retries back off and only the first failure
        // warns, instead of "host UUID not found" on every tick.
        let t0 = std::time::Instant::now();
        let mut g = AlertGate::default();
        assert!(g.open(t0));
        assert!(g.unreachable(t0), "the first failure is worth one warning");
        assert!(
            !g.open(t0 + ALERT_GATE_MIN / 2),
            "no new attempt inside the backoff"
        );
        assert!(g.open(t0 + ALERT_GATE_MIN));
        assert!(
            !g.unreachable(t0 + ALERT_GATE_MIN),
            "later failures stay quiet"
        );
        assert_eq!(g.backoff, ALERT_GATE_MIN * 2);
        for _ in 0..10 {
            g.unreachable(t0);
        }
        assert_eq!(
            g.backoff, ALERT_GATE_MAX,
            "capped so an alert is never held back long"
        );
        g.reachable();
        assert!(g.open(t0) && g.backoff.is_zero());
    }

    const TWINSTOR_IQN: &str = "iqn.2025-01.tech.vates:twinstor-data";

    #[test]
    fn first_error_line_names_the_xapi_reason() {
        let err = "\nThe VM's power state is not what the operation expects.\nvm: 9128e483 (Test 3)\nexpected: halted\nactual: paused\n";
        assert_eq!(
            first_error_line(err).as_deref(),
            Some("The VM's power state is not what the operation expects.")
        );
        assert_eq!(first_error_line("  \n \n"), None);
    }

    // Two SRs as `xe pbd-list params=sr-uuid,device-config` renders them: one
    // twinstor lvmoiscsi PBD and one Synology backup PBD, blank-line separated.
    const TWO_SR_PBD_LIST: &str = "\
sr-uuid ( RO)       : 11111111-1111-1111-1111-111111111111
device-config (MRO): target: 198.51.100.2; targetIQN: iqn.2025-01.tech.vates:twinstor-data; SCSIid: 36001405abc; port: 3260

sr-uuid ( RO)       : 22222222-2222-2222-2222-222222222222
device-config (MRO): target: 10.0.0.50; targetIQN: iqn.2000-01.com.synology:backup.Target-1; SCSIid: 36001405def; port: 3260
";

    #[test]
    fn parse_sr_uuid_for_iqn_matches_twinstor_among_two() {
        assert_eq!(
            parse_sr_uuid_for_iqn(TWO_SR_PBD_LIST, TWINSTOR_IQN),
            Some("11111111-1111-1111-1111-111111111111".to_string())
        );
        // The Synology IQN resolves to the other SR.
        assert_eq!(
            parse_sr_uuid_for_iqn(TWO_SR_PBD_LIST, "iqn.2000-01.com.synology:backup.Target-1"),
            Some("22222222-2222-2222-2222-222222222222".to_string())
        );
    }

    #[test]
    fn parse_sr_uuid_for_iqn_no_match_is_none() {
        assert_eq!(
            parse_sr_uuid_for_iqn(TWO_SR_PBD_LIST, "iqn.2025-01.tech.vates:does-not-exist"),
            None
        );
    }

    #[test]
    fn parse_sr_uuid_for_iqn_empty_input_is_none() {
        assert_eq!(parse_sr_uuid_for_iqn("", TWINSTOR_IQN), None);
        // An empty IQN must never match (would otherwise `contains("")` everything).
        assert_eq!(parse_sr_uuid_for_iqn(TWO_SR_PBD_LIST, ""), None);
    }

    #[test]
    fn parse_sr_uuid_for_iqn_iqn_as_substring_of_longer_line() {
        // The target IQN appears inside a longer device-config line; still matches.
        let out = "\
sr-uuid ( RO)       : aaaa1111-1111-1111-1111-111111111111
device-config (MRO): multihomelist: 198.51.100.2:3260,198.51.100.1:3260; target: 198.51.100.2; targetIQN: iqn.2025-01.tech.vates:twinstor-data; SCSIid: 36001405abcdef0123456789; port: 3260; chapuser: twinstor
";
        assert_eq!(
            parse_sr_uuid_for_iqn(out, TWINSTOR_IQN),
            Some("aaaa1111-1111-1111-1111-111111111111".to_string())
        );
    }

    #[test]
    fn parse_sr_uuid_for_iqn_prefix_collision_does_not_false_match() {
        // A foreign SR whose IQN has the TwinStor IQN as a prefix, listed first.
        // A substring match would pick it; the field-bounded match must skip it
        // and return the real TwinStor SR, or None if that one is absent.
        let out = "\
sr-uuid ( RO)       : ffffffff-old-migration-snapshot-sr-00
device-config (MRO): target: 198.51.100.9; targetIQN: iqn.2025-01.tech.vates:twinstor-data-old; SCSIid: 36001405ffff; port: 3260

sr-uuid ( RO)       : 11111111-1111-1111-1111-111111111111
device-config (MRO): target: 198.51.100.2; targetIQN: iqn.2025-01.tech.vates:twinstor-data; SCSIid: 36001405abc; port: 3260
";
        assert_eq!(
            parse_sr_uuid_for_iqn(out, TWINSTOR_IQN),
            Some("11111111-1111-1111-1111-111111111111".to_string()),
            "must skip the prefix-colliding foreign SR and match the exact twinstor IQN"
        );
        // Only the colliding foreign SR present: None, not the foreign SR.
        let only_foreign = "\
sr-uuid ( RO)       : ffffffff-old-migration-snapshot-sr-00
device-config (MRO): target: 198.51.100.9; targetIQN: iqn.2025-01.tech.vates:twinstor-data-old; SCSIid: 36001405ffff; port: 3260
";
        assert_eq!(parse_sr_uuid_for_iqn(only_foreign, TWINSTOR_IQN), None);
    }

    #[test]
    fn parse_sr_uuid_for_iqn_device_config_before_sr_uuid() {
        // Robust to field order: device-config line precedes the sr-uuid line.
        let out = "\
device-config (MRO): target: 198.51.100.2; targetIQN: iqn.2025-01.tech.vates:twinstor-data; SCSIid: 36001405abc; port: 3260
sr-uuid ( RO)       : bbbb2222-2222-2222-2222-222222222222

device-config (MRO): target: 10.0.0.50; targetIQN: iqn.2000-01.com.synology:backup.Target-1; SCSIid: 36001405def
sr-uuid ( RO)       : cccc3333-3333-3333-3333-333333333333
";
        assert_eq!(
            parse_sr_uuid_for_iqn(out, TWINSTOR_IQN),
            Some("bbbb2222-2222-2222-2222-222222222222".to_string())
        );
        assert_eq!(
            parse_sr_uuid_for_iqn(out, "iqn.2000-01.com.synology:backup.Target-1"),
            Some("cccc3333-3333-3333-3333-333333333333".to_string())
        );
    }

    #[test]
    fn maintenance_fails_closed_on_unknown() {
        // Confirmed maintenance (either spacing xe may emit).
        assert_eq!(
            parse_maintenance(Some("MAINTENANCE_MODE: true")),
            Some(true)
        );
        assert_eq!(
            parse_maintenance(Some("other-config (MRO): MAINTENANCE_MODE:true; foo: bar")),
            Some(true)
        );
        // Confirmed not in maintenance (query answered, no marker).
        assert_eq!(
            parse_maintenance(Some("other-config (MRO): foo: bar")),
            Some(false)
        );
        assert_eq!(parse_maintenance(Some("")), Some(false));
        // The toolstack did not answer: unknown, not "no maintenance". The
        // caller holds the aggressive re-arm on this (`!= Some(false)`).
        assert_eq!(parse_maintenance(None), None);
    }

    #[test]
    fn first_token_handles_single_and_multi_match() {
        // Normal single-match `--minimal` output.
        assert_eq!(first_token("abc-123"), Some("abc-123".to_string()));
        assert_eq!(first_token(" abc-123 \n"), Some("abc-123".to_string()));
        // Multi-match comma-join: take the first, never pass "a,b" downstream.
        assert_eq!(first_token("uuid1,uuid2"), Some("uuid1".to_string()));
        // Empty or whitespace: None (no match).
        assert_eq!(first_token(""), None);
        assert_eq!(first_token("  "), None);
        assert_eq!(first_token(","), None);
    }

    #[test]
    fn parses_master() {
        assert_eq!(parse_pool_role("master"), Some(true));
        assert_eq!(parse_pool_role("master\n"), Some(true));
        assert_eq!(parse_pool_role("  master \n"), Some(true));
    }

    #[test]
    fn parses_slave_regardless_of_address() {
        assert_eq!(parse_pool_role("slave:192.0.2.102"), Some(false));
        assert_eq!(parse_pool_role("slave:192.0.2.102\n"), Some(false));
        // The self-referential pool.conf corruption: a slave whose recorded
        // master is its own address is still unambiguously a slave, so resolution
        // proceeds (see CONVERGENCE.md, resilient mastership read).
        assert_eq!(parse_pool_role("slave:192.0.2.101"), Some(false));
    }

    #[test]
    fn rejects_unrecognized() {
        // Refuse to guess from a malformed file: caller treats None as
        // "undecidable" and does not discard.
        assert_eq!(parse_pool_role(""), None);
        assert_eq!(parse_pool_role("garbage"), None);
        assert_eq!(parse_pool_role("slave"), None);
    }
}

/// The pool master the peer believes it follows, probed without credentials:
/// a xapi slave rejects any login with HOST_IS_SLAVE and the address it
/// follows, before authentication. `Some(addr)` when the peer answers as a
/// slave of `addr`; `None` when it answers as a master (the login proceeds to
/// an auth failure instead), is unreachable, or the probe times out. This is
/// the only probe that can prove a mutual-slave pool without a daemon protocol.
pub async fn peer_claims_slave_of(peer_addr: &str) -> Option<String> {
    let script = format!(
        "import XenAPI,sys\n\
         s=XenAPI.Session('https://{peer_addr}', ignore_ssl=True)\n\
         try:\n s.xenapi.login_with_password('twinstor-probe','x')\nexcept XenAPI.Failure as f:\n\
         \x20if f.details and f.details[0]=='HOST_IS_SLAVE':\n  print('SLAVE_OF:'+f.details[1])\n\
         except Exception:\n pass\n"
    );
    let out = crate::cmd::cmd_output_for("python3", &["-c", &script], 15)
        .await
        .ok()?;
    out.lines()
        .find_map(|l| l.strip_prefix("SLAVE_OF:"))
        .map(|s| s.trim().to_string())
}

/// The peer's xapi role, probed without credentials by [`peer_xapi_role`].
/// Unlike `peer_claims_slave_of`, which returns `None` both for a live master
/// and for a dead peer, this has three outcomes, so a returning node can tell
/// a survivor that is already master (defer to it, never create a second
/// master) from a dead master (take over). A bogus login yields:
/// - HOST_IS_SLAVE: the peer is a slave (`SlaveOf`);
/// - any other xapi failure: the peer's xapi answered and rejected the login,
///   so it is the master (`Master`);
/// - a transport or connect error: the peer's xapi is not answering
///   (`Unreachable`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerXapiRole {
    Master,
    SlaveOf(String),
    Unreachable,
}

pub async fn peer_xapi_role(peer_addr: &str) -> PeerXapiRole {
    let script = format!(
        "import XenAPI\n\
         try:\n\
         \x20s=XenAPI.Session('https://{peer_addr}', ignore_ssl=True)\n\
         \x20s.xenapi.login_with_password('twinstor-probe','x')\n\
         \x20print('MASTER')\n\
         except XenAPI.Failure as f:\n\
         \x20if f.details and f.details[0]=='HOST_IS_SLAVE':\n  print('SLAVE_OF:'+f.details[1])\n\
         \x20else:\n  print('MASTER')\n\
         except Exception:\n print('UNREACHABLE')\n"
    );
    let Ok(out) = crate::cmd::cmd_output_for("python3", &["-c", &script], 15).await else {
        return PeerXapiRole::Unreachable;
    };
    for l in out.lines() {
        if let Some(a) = l.strip_prefix("SLAVE_OF:") {
            return PeerXapiRole::SlaveOf(a.trim().to_string());
        }
        if l.trim() == "MASTER" {
            return PeerXapiRole::Master;
        }
    }
    PeerXapiRole::Unreachable
}

/// Running guest count via `xl`: a local read with no xapi dependency, usable
/// on an isolated host whose `xe` does not answer.
pub async fn local_running_vm_count() -> u32 {
    crate::cmd::cmd_output_for(
        "sh",
        &[
            "-c",
            "xl list 2>/dev/null | tail -n +3 | grep -vc '^Domain-0'",
        ],
        10,
    )
    .await
    .ok()
    .and_then(|s| s.trim().parse().ok())
    .unwrap_or(0)
}

/// The peer host's `host-metrics-live` in this node's pool DB, or `None` if
/// unreadable. Used to detect a stranded former master: a peer that is alive on
/// the replication link but reads not live here is acting as its own pool
/// master, so this node is the spurious second master.
pub async fn peer_host_live(peer_name: &str) -> Option<bool> {
    match xe_minimal(&[
        "host-list",
        &format!("name-label={peer_name}"),
        "params=host-metrics-live",
    ])
    .await?
    .trim()
    {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// The peer host's `enabled` flag in this node's pool DB, or `None` if
/// unreadable. In arbitrated mode `host-metrics-live` stays true through the
/// direct-link statefile even while the peer's management network is
/// switch-isolated; `enabled` is false throughout an isolation and the whole
/// rejoin, and true only once the peer is fully re-admitted. That makes it the
/// right precondition for a `pool-ha-enable` re-arm.
pub async fn peer_host_enabled(peer_name: &str) -> Option<bool> {
    match xe_minimal(&[
        "host-list",
        &format!("name-label={peer_name}"),
        "params=enabled",
    ])
    .await?
    .trim()
    {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}
