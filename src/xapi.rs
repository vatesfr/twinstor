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
    let my_uuid = local_host_uuid().await?;
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

/// The peer's management address as XAPI records it (its host record's
/// `address`). XAPI uses this path for inter-host RPC, including the
/// pool-master demotion that the dual-master auto-heal triggers with a
/// toolstack restart; in an arbitrated topology it differs from the DRBD
/// replication link. The address is cached in the local DB, so it is
/// answerable even while the pool is wedged. `None` if XAPI cannot answer or
/// returns empty.
pub async fn peer_management_address(peer_hostname: &str) -> Option<String> {
    let addr = host_param(peer_hostname, "address").await?;
    first_token(&addr)
}

// Host identity. A host is named in XAPI queries by its UUID, never by a name:
// the operator can rename a host in XO at any time (its `name-label`), and a
// lookup by `name-label` then silently matches nothing. This host's UUID comes
// from the local inventory ([`local_host_uuid`]). The peer is configured by its
// hostname, which DRBD needs and which a rename in XO does not change; it is
// resolved to a UUID once and cached for the daemon's lifetime. Every peer
// query then goes by `uuid=`, and only [`resolve_host_uuid`] queries by
// hostname.

/// Host UUIDs resolved from hostnames. Host UUIDs do not change while a host
/// stays in the pool; a host re-installed or re-joined under a new UUID is
/// noticed when its cached UUID matches no host, and resolved again.
struct HostUuidCache {
    entries: Vec<(String, String)>,
}

impl HostUuidCache {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn get(&self, hostname: &str) -> Option<String> {
        self.entries
            .iter()
            .find(|(h, _)| h == hostname)
            .map(|(_, u)| u.clone())
    }

    fn put(&mut self, hostname: &str, uuid: &str) {
        self.entries.retain(|(h, _)| h != hostname);
        self.entries.push((hostname.to_string(), uuid.to_string()));
    }

    /// Drop `hostname`'s entry if it still holds `uuid` (another caller may
    /// have re-resolved it meanwhile).
    fn forget(&mut self, hostname: &str, uuid: &str) {
        self.entries.retain(|(h, u)| !(h == hostname && u == uuid));
    }
}

static HOST_UUIDS: std::sync::Mutex<HostUuidCache> = std::sync::Mutex::new(HostUuidCache::new());

/// The UUID of the host whose hostname is `hostname`, from the cache or by one
/// `host-list hostname=` query, with whether it came from the cache. `None`
/// when XAPI cannot answer or no host has that hostname. Only a single match is
/// cached: a duplicate hostname (a misconfigured pool) yields the first match
/// uncached, so it is looked up again next time.
async fn resolve_host_uuid<Q, F>(
    cache: &std::sync::Mutex<HostUuidCache>,
    hostname: &str,
    xe: &Q,
) -> Option<(String, bool)>
where
    Q: Fn(Vec<String>) -> F,
    F: std::future::Future<Output = Option<String>>,
{
    if let Some(u) = cache.lock().ok().and_then(|c| c.get(hostname)) {
        return Some((u, true));
    }
    let answer = xe(vec![
        "host-list".into(),
        format!("hostname={hostname}"),
        "params=uuid".into(),
    ])
    .await?;
    let uuid = first_token(&answer)?;
    if !answer.contains(',')
        && let Ok(mut c) = cache.lock()
    {
        c.put(hostname, &uuid);
    }
    Some((uuid, false))
}

/// One parameter of the host whose hostname is `hostname`, queried by its
/// UUID. `None` when XAPI cannot answer or no host has that hostname. An empty
/// answer for a cached UUID means that UUID names no host any more: the entry
/// is dropped and the hostname resolved again, once.
async fn host_param_with<Q, F>(
    cache: &std::sync::Mutex<HostUuidCache>,
    hostname: &str,
    param: &str,
    xe: &Q,
) -> Option<String>
where
    Q: Fn(Vec<String>) -> F,
    F: std::future::Future<Output = Option<String>>,
{
    let query = |uuid: &str| {
        xe(vec![
            "host-list".into(),
            format!("uuid={uuid}"),
            format!("params={param}"),
        ])
    };
    let (uuid, cached) = resolve_host_uuid(cache, hostname, xe).await?;
    let answer = query(&uuid).await?;
    if !answer.is_empty() || !cached {
        return Some(answer);
    }
    if let Ok(mut c) = cache.lock() {
        c.forget(hostname, &uuid);
    }
    let (fresh, _) = resolve_host_uuid(cache, hostname, xe).await?;
    if fresh == uuid {
        return Some(answer);
    }
    query(&fresh).await
}

/// [`host_param_with`] against live XAPI and the daemon-wide cache.
pub(crate) async fn host_param(hostname: &str, param: &str) -> Option<String> {
    let xe = |args: Vec<String>| async move {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        xe_minimal(&refs).await
    };
    host_param_with(&HOST_UUIDS, hostname, param, &xe).await
}

/// The XAPI UUID of the host whose hostname is `hostname`, checked against the
/// pool. `None` when XAPI cannot answer or no host has that hostname.
pub(crate) async fn host_uuid(hostname: &str) -> Option<String> {
    host_param(hostname, "uuid")
        .await
        .and_then(|s| first_token(&s))
}

/// One parameter of this host, queried by its inventory UUID. `None` when the
/// inventory is unreadable or XAPI cannot answer.
async fn local_host_param(param: &str) -> Option<String> {
    let uuid = local_host_uuid().await?;
    xe_minimal(&[
        "host-list",
        &format!("uuid={uuid}"),
        &format!("params={param}"),
    ])
    .await
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

/// How many VMs the pool database shows running on the host named
/// `hostname` (resident on it, power-state running, no control domain).
/// `None` when XAPI cannot answer.
pub async fn running_vms_on(hostname: &str) -> Option<u32> {
    let uuid = host_uuid(hostname).await?;
    let list = xe_minimal(&[
        "vm-list",
        &format!("resident-on={uuid}"),
        "power-state=running",
        "is-control-domain=false",
        "params=uuid",
    ])
    .await?;
    Some(list.split(',').filter(|u| !u.trim().is_empty()).count() as u32)
}

/// Path to the recorded SR UUID state file, under `/var/lib/twinstor` unless
/// overridden by `TWINSTOR_SR_UUID_FILE`. Mirrors `ha_rearm_obligation_path()`.
pub fn sr_uuid_path() -> String {
    std::env::var("TWINSTOR_SR_UUID_FILE")
        .unwrap_or_else(|_| crate::heal_state::state_path("/var/lib/twinstor/sr-uuid"))
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
    // Fallback for installs that predate the recording.
    twinstor_sr_uuid_by_name().await
}

/// The SR setup names "twinstor". If two SRs carry that name, the first UUID,
/// rather than a comma-joined pair that `pool-ha-enable heartbeat-sr-uuids=`
/// would reject.
pub async fn twinstor_sr_uuid_by_name() -> Option<String> {
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
/// Whether XAPI positively answers that the pool has no TwinStor SR: each
/// lookup (recorded uuid, target IQN, default name) answered and found none.
/// A lookup that fails or times out is no answer, so this reads false: the
/// lookups behind `sr_uuid_exists` and friends read a failure as "none".
pub async fn twinstor_sr_absent_confirmed(target_iqn: &str) -> bool {
    if let Some(u) = read_recorded_sr_uuid() {
        match xe_minimal(&["sr-list", &format!("uuid={u}"), "params=uuid"]).await {
            Some(s) if s.is_empty() => {}
            _ => return false,
        }
    }
    match xe_raw(&["pbd-list", "params=sr-uuid,device-config"]).await {
        Some(out) if parse_sr_uuid_for_iqn(&out, target_iqn).is_none() => {}
        _ => return false,
    }
    matches!(
        xe_minimal(&["sr-list", "name-label=twinstor", "params=uuid"]).await,
        Some(s) if s.is_empty()
    )
}

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
/// is healthy.
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

/// Start a VM by UUID within `secs`. `Err` carries the first line of XAPI's
/// error text.
#[cfg_attr(test, allow(dead_code))]
pub async fn start_vm_reason(uuid: &str, secs: u64) -> Result<(), String> {
    match crate::cmd::cmd_status_for("xe", &["vm-start", &format!("uuid={uuid}")], secs).await {
        Ok((true, _, _)) => Ok(()),
        Ok((false, out, err)) => Err(first_error_line(&err)
            .or_else(|| first_error_line(&out))
            .unwrap_or_else(|| "no error text".to_string())),
        Err(e) => Err(format!("{e:#}")),
    }
}

/// Whether the pool carries `other-config:auto_poweron=true`, the flag
/// XCP-ng's boot-time autostart requires. `None` when XAPI does not answer.
#[cfg_attr(test, allow(dead_code))]
pub async fn pool_auto_poweron() -> Option<bool> {
    let out = crate::cmd::cmd_output_for(
        "xe",
        &["pool-list", "other-config:auto_poweron=true", "--minimal"],
        15,
    )
    .await
    .ok()?;
    Some(!out.trim().is_empty())
}

/// Halted VMs marked `other-config:auto_poweron=true` (templates, snapshots
/// and dom0 excluded): what XCP-ng's boot-time autostart starts.
#[cfg_attr(test, allow(dead_code))]
pub async fn halted_auto_poweron_vms() -> Option<Vec<String>> {
    xe_vm_uuids(&[
        "vm-list",
        "is-control-domain=false",
        "is-a-snapshot=false",
        "is-a-template=false",
        "power-state=halted",
        "other-config:auto_poweron=true",
    ])
    .await
}

/// Hosts in the pool, enabled hosts, and hosts whose metrics read live.
/// `None` when any read fails.
#[cfg_attr(test, allow(dead_code))]
pub async fn pool_host_counts() -> Option<(usize, usize, usize)> {
    let count = |s: &str| s.split(',').filter(|t| !t.trim().is_empty()).count();
    let all = crate::cmd::cmd_output_for("xe", &["host-list", "--minimal"], 15)
        .await
        .ok()?;
    let enabled = crate::cmd::cmd_output_for("xe", &["host-list", "enabled=true", "--minimal"], 15)
        .await
        .ok()?;
    let live = crate::cmd::cmd_output_for(
        "xe",
        &["host-list", "params=host-metrics-live", "--minimal"],
        15,
    )
    .await
    .ok()?;
    let live = live.split(',').filter(|t| t.trim() == "true").count();
    Some((count(&all), count(&enabled), live))
}

/// PBDs of an SR, and how many are attached. `None` when XAPI does not
/// answer.
#[cfg_attr(test, allow(dead_code))]
pub async fn sr_pbds_attached(sr_uuid: &str) -> Option<(usize, usize)> {
    let out = crate::cmd::cmd_output_for(
        "xe",
        &[
            "pbd-list",
            &format!("sr-uuid={sr_uuid}"),
            "params=currently-attached",
            "--minimal",
        ],
        15,
    )
    .await
    .ok()?;
    let all: Vec<&str> = out
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    Some((all.len(), all.iter().filter(|t| **t == "true").count()))
}

/// What the autostart completion re-reads about a VM right before starting
/// it. `None` when any read fails.
#[cfg_attr(test, allow(dead_code))]
pub async fn autostart_vm_facts(uuid: &str) -> Option<crate::autostart::VmFacts> {
    let power_state = vm_param(uuid, "power-state", None).await?;
    let ha_always_run = vm_param(uuid, "ha-always-run", None).await?;
    let start_time = vm_param(uuid, "start-time", None).await?;
    let map = xe_minimal(&[
        "vm-param-get",
        &format!("uuid={uuid}"),
        "param-name=other-config",
    ])
    .await?;
    Some(crate::autostart::VmFacts {
        power_state,
        ha_always_run,
        auto_poweron: other_config_value(&map, "auto_poweron").unwrap_or_default(),
        start_time,
        disks_suspect: other_config_value(&map, DISKS_SUSPECT_KEY).as_deref() == Some("true"),
    })
}

/// One key of an `other-config` map as `xe` prints it (`k1: v1; k2: v2`).
pub(crate) fn other_config_value(map: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}: ");
    map.split(';').find_map(|kv| {
        kv.trim()
            .strip_prefix(prefix.as_str())
            .map(|v| v.trim().to_string())
    })
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
        std::time::Duration::from_secs(5),
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
    let Some(uuid) = local_host_uuid().await else {
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
    let Some(uuid) = local_host_uuid().await else {
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
    let uuid = local_host_uuid().await?;
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
    let Some(uuid) = local_host_uuid().await else {
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
    let Some(config) = host_param(peer_hostname, "other-config").await else {
        return (None, None, None);
    };
    let stamp = other_config_value(&config, "twinstor-alive").and_then(|s| s.parse::<u64>().ok());
    let version = other_config_value(&config, "twinstor-version").filter(|s| !s.is_empty());
    let live = match host_param(peer_hostname, "host-metrics-live")
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
    let config = host_param(peer_hostname, "other-config").await?;
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
        "TwinStor: no liveness report from the twinstor daemon on {peer} has \
         arrived for ~{minutes} minutes, while its management address answers and \
         XAPI shows the host up. The daemon may be stopped or hung; if it is, that \
         node's safety layer (failure handling, split-brain resolution, alerting) \
         is not running while storage keeps being served by the kernel. On {peer} \
         run: systemctl status twinstor; journalctl -u twinstor -n 100; if it is \
         not running or not logging, systemctl restart twinstor."
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
    let host_uuid = host_uuid(hostname).await?;
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

/// XAPI's persistent pool default for the HA timeout: a `pool-ha-enable`
/// without `ha-config:timeout` uses this pool `other-config` key, and only
/// without it XAPI's built-in 60 s (`Xapi_ha.Timeouts.get_base_t`).
pub const DEFAULT_HA_TIMEOUT_KEY: &str = "default_ha_timeout";

/// Pool `other-config` key recording that twinstor set `DEFAULT_HA_TIMEOUT_KEY`:
/// `<value set>:<value found>`, the second part empty when the key was unset.
pub const DEFAULT_HA_TIMEOUT_PRIOR_KEY: &str = "twinstor-default-ha-timeout-prior";

/// The record to write when setting the pool default to `set`, given the
/// value found and any existing record. An existing record is kept as it is
/// (a setup re-run must not record twinstor's own value as the prior one), so
/// this returns `None` then.
pub fn default_ha_timeout_record(
    set: u32,
    found: Option<&str>,
    existing: Option<&str>,
) -> Option<String> {
    if existing.is_some() {
        return None;
    }
    Some(format!("{set}:{}", found.unwrap_or("")))
}

/// The pool default HA timeout `upgrade-apply` sets on a pool that predates
/// setup setting it, as `(value, record)`, or `None` to leave the pool alone.
/// It acts only when the key is absent and twinstor has no record: a value
/// already there is the operator's, and a record without the key means the
/// operator removed what twinstor set. The value is the pool's explicit
/// `ha-config:timeout` when there is one, else the mode's. Pure.
pub fn upgrade_default_ha_timeout(
    current: Option<&str>,
    record: Option<&str>,
    ha_config_timeout: Option<u32>,
    mode_timeout: u32,
) -> Option<(u32, String)> {
    if current.is_some() || record.is_some() {
        return None;
    }
    let secs = ha_config_timeout.unwrap_or(mode_timeout);
    Some((secs, format!("{secs}:")))
}

/// What uninstall does with the pool default HA timeout.
#[derive(Debug, PartialEq, Eq)]
pub enum DefaultHaTimeoutRestore {
    /// No record: twinstor never set it.
    Nothing,
    /// The key still holds what twinstor set: put back the value found
    /// (`None` removes the key), then drop the record.
    Restore(Option<String>),
    /// Someone changed the key since (or the record is unreadable): leave the
    /// key, drop the record.
    DropRecord,
}

/// Decide the uninstall step from twinstor's record and the key's current
/// value. Pure.
pub fn default_ha_timeout_restore(
    record: Option<&str>,
    current: Option<&str>,
) -> DefaultHaTimeoutRestore {
    let Some(record) = record else {
        return DefaultHaTimeoutRestore::Nothing;
    };
    match record.split_once(':') {
        Some((set, found)) if !set.is_empty() && current == Some(set) => {
            DefaultHaTimeoutRestore::Restore((!found.is_empty()).then(|| found.to_string()))
        }
        _ => DefaultHaTimeoutRestore::DropRecord,
    }
}

async fn pool_other_config_key(pool: &str, key: &str) -> Option<String> {
    xe_minimal(&[
        "pool-param-get",
        &format!("uuid={pool}"),
        "param-name=other-config",
        &format!("param-key={key}"),
    ])
    .await
    .filter(|v| !v.is_empty())
}

/// The pool's `default_ha_timeout` and twinstor's record of setting it, as
/// `(current, record)`. `None` when XAPI does not answer.
pub async fn pool_default_ha_timeout() -> Option<(Option<String>, Option<String>)> {
    let pool = xe_minimal(&["pool-list", "params=uuid"]).await?;
    if pool.is_empty() {
        return None;
    }
    // The map read proves XAPI answers; the exact-key reads then fail only
    // when a key is absent.
    xe_minimal(&[
        "pool-param-get",
        &format!("uuid={pool}"),
        "param-name=other-config",
    ])
    .await?;
    Some((
        pool_other_config_key(&pool, DEFAULT_HA_TIMEOUT_KEY).await,
        pool_other_config_key(&pool, DEFAULT_HA_TIMEOUT_PRIOR_KEY).await,
    ))
}

/// Set the pool's `default_ha_timeout` to `secs`, writing `record` in the same
/// call when given. `true` when XAPI accepted it.
pub async fn set_pool_default_ha_timeout(secs: u32, record: Option<&str>) -> bool {
    let Some(pool) = xe_minimal(&["pool-list", "params=uuid"])
        .await
        .filter(|p| !p.is_empty())
    else {
        return false;
    };
    let mut args = vec![
        "pool-param-set".to_string(),
        format!("uuid={pool}"),
        format!("other-config:{DEFAULT_HA_TIMEOUT_KEY}={secs}"),
    ];
    if let Some(r) = record {
        args.push(format!("other-config:{DEFAULT_HA_TIMEOUT_PRIOR_KEY}={r}"));
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    xe_minimal(&args).await.is_some()
}

/// Apply an uninstall decision: put the value found back (or remove the key)
/// when asked, then drop the record. `true` when XAPI accepted every step.
pub async fn restore_pool_default_ha_timeout(restore: Option<Option<String>>) -> bool {
    let Some(pool) = xe_minimal(&["pool-list", "params=uuid"])
        .await
        .filter(|p| !p.is_empty())
    else {
        return false;
    };
    let restored = match restore {
        None => true,
        Some(Some(v)) => xe_minimal(&[
            "pool-param-set",
            &format!("uuid={pool}"),
            &format!("other-config:{DEFAULT_HA_TIMEOUT_KEY}={v}"),
        ])
        .await
        .is_some(),
        Some(None) => xe_minimal(&[
            "pool-param-remove",
            &format!("uuid={pool}"),
            "param-name=other-config",
            &format!("param-key={DEFAULT_HA_TIMEOUT_KEY}"),
        ])
        .await
        .is_some(),
    };
    if !restored {
        return false;
    }
    xe_minimal(&[
        "pool-param-remove",
        &format!("uuid={pool}"),
        "param-name=other-config",
        &format!("param-key={DEFAULT_HA_TIMEOUT_PRIOR_KEY}"),
    ])
    .await
    .is_some()
}

/// Whether HA is enabled on the pool. `None` if XAPI is unreachable or the
/// value can't be parsed.
pub async fn is_ha_enabled() -> Option<bool> {
    let v = match xe_minimal(&["pool-list", "params=ha-enabled"])
        .await?
        .as_str()
    {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    };
    if let Some(on) = v {
        LAST_POOL_HA.store(pool_ha_cache_pack(on, cache_secs()), std::sync::atomic::Ordering::Relaxed);
    }
    v
}

/// The last pool HA state any `is_ha_enabled` read got, with when (packed by
/// `pool_ha_cache_pack`; 0: none yet). Already-known state for readers that
/// must not add a management-plane call of their own.
static LAST_POOL_HA: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How long a cached pool HA read stays known. Reads come every minute
/// while XAPI answers (the re-admission check); a host whose own XAPI stops
/// answering (left disarmed under the armed pool, say) must not keep an old
/// "off" for good.
pub const POOL_HA_CACHE_MAX_AGE_SECS: u64 = 300;

/// Seconds on a monotonic clock since the first use, plus one (0: never).
fn cache_secs() -> u64 {
    static BASE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    BASE.get_or_init(std::time::Instant::now).elapsed().as_secs() + 1
}

fn pool_ha_cache_pack(on: bool, at: u64) -> u64 {
    (at << 2) | if on { 2 } else { 1 }
}

/// The cached value at `now`: `None` when never read or older than
/// `POOL_HA_CACHE_MAX_AGE_SECS`.
fn pool_ha_cache_value(packed: u64, now: u64) -> Option<bool> {
    let at = packed >> 2;
    if packed == 0 || now.saturating_sub(at) > POOL_HA_CACHE_MAX_AGE_SECS {
        return None;
    }
    match packed & 3 {
        1 => Some(false),
        2 => Some(true),
        _ => None,
    }
}

/// The last pool HA state read by this daemon in the last few minutes,
/// `None` if never read or too old.
pub fn last_known_pool_ha() -> Option<bool> {
    pool_ha_cache_value(LAST_POOL_HA.load(std::sync::atomic::Ordering::Relaxed), cache_secs())
}

/// The name of a pool HA enable or disable task XAPI still holds pending
/// (one that never returned keeps the HA operations blocked), if any. None
/// also when XAPI does not answer: the caller's own command then fails.
pub async fn pending_pool_ha_task() -> Option<String> {
    let out = xe_minimal(&["task-list", "status=pending", "params=name-label"]).await?;
    pending_ha_task_in(&out)
}

/// A pending pool HA task (`pool.enable_ha` or `pool.disable_ha`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingHaTask {
    pub uuid: String,
    pub name: String,
    /// When XAPI created it (unix seconds), if readable.
    pub created_unix: Option<u64>,
}

/// XAPI's date format (`20261117T08:05:23Z`, UTC) as unix seconds.
pub fn parse_xapi_time(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.len() != 18 || s.as_bytes()[8] != b'T' || !s.ends_with('Z') {
        return None;
    }
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<u32>().ok();
    let month = time::Month::try_from(num(4, 6)? as u8).ok()?;
    let date = time::Date::from_calendar_date(num(0, 4)? as i32, month, num(6, 8)? as u8).ok()?;
    let t = time::Time::from_hms(num(9, 11)? as u8, num(12, 14)? as u8, num(15, 17)? as u8).ok()?;
    u64::try_from(time::PrimitiveDateTime::new(date, t).assume_utc().unix_timestamp()).ok()
}

/// The pending pool HA tasks in `xe task-list params=uuid,name-label,created`
/// record output.
pub fn parse_pending_ha_tasks(out: &str) -> Vec<PendingHaTask> {
    let mut tasks = Vec::new();
    for rec in out.split("\n\n") {
        let field = |k: &str| {
            rec.lines().find_map(|l| {
                let (key, val) = l.split_once(':')?;
                (key.split('(').next()?.trim() == k).then(|| val.trim().to_string())
            })
        };
        let (Some(uuid), Some(name)) = (field("uuid"), field("name-label")) else {
            continue;
        };
        if !(name.ends_with("pool.disable_ha") || name.ends_with("pool.enable_ha")) {
            continue;
        }
        let created_unix = field("created").and_then(|c| parse_xapi_time(&c));
        tasks.push(PendingHaTask {
            uuid,
            name,
            created_unix,
        });
    }
    tasks
}

/// Every pending pool HA task, with its creation time. `None` when xe does
/// not answer.
pub async fn pending_pool_ha_tasks() -> Option<Vec<PendingHaTask>> {
    let out = xe_raw(&[
        "task-list",
        "status=pending",
        "params=uuid,name-label,created",
    ])
    .await?;
    Some(parse_pending_ha_tasks(&out))
}

/// Cancel one task. `true` when xe accepted the cancel.
pub async fn cancel_task(uuid: &str) -> bool {
    crate::cmd::run_cmd_for("xe", &["task-cancel", &format!("uuid={uuid}")], 30)
        .await
        .is_ok()
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
    let out = local_host_param("enabled").await?;
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
         heartbeat-sr-uuids=<twinstor SR uuid> ha-config:timeout={}",
        crate::setup::configured_ha_timeout()
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

/// The identity key of an alert that advises an HA disable.
pub fn ha_advice_key(body: &str) -> String {
    format!("{}{body}", crate::arbitration::HA_DISABLE_ADVICE_KEY)
}

/// [`send_xapi_alert`] for an alert whose remedy is `pool-ha-disable` or
/// `host-emergency-ha-disable`: held back while a host is parked by XAPI,
/// otherwise prefixed with when the advice is safe.
pub async fn send_xapi_alert_advising_ha_disable(body: &str) -> bool {
    send_alert_keyed("twinstor_degraded", &ha_advice_key(body), body).await
}

/// [`send_xapi_alert_advising_ha_disable`] sent and queued under `key`
/// (see [`send_alert_keyed`]): a later message under the same key replaces
/// an undelivered one.
pub async fn send_xapi_alert_advising_ha_disable_keyed(key: &str, body: &str) -> bool {
    send_alert_keyed("twinstor_degraded", &ha_advice_key(key), body).await
}

/// [`send_ha_wedge_alert`] for an alert that advises an HA disable.
pub async fn send_ha_wedge_alert_advising_ha_disable(body: &str) -> bool {
    send_alert_keyed("twinstor_ha_wedge", &ha_advice_key(body), body).await
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

/// When a held alert was last raised, and whether it is held back for a
/// parked host (an HA-disable advice) rather than for an XAPI that did not
/// answer.
#[derive(Debug, Clone, Copy)]
struct Held {
    at: std::time::Instant,
    park: bool,
}

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
    /// (name, identity key, body, when and why held)
    items: Vec<(String, String, String, Held)>,
    /// Taken by a background flush and not yet sent: (name, key).
    in_flight: Vec<(String, String)>,
    /// Of those, withdrawn (or delivered by a newer send) after they were
    /// taken: the flush drops them instead of sending.
    withdrawn: Vec<(String, String)>,
}

const PENDING_ALERTS_MAX: usize = 32;
const PENDING_PER_NAME_MAX: usize = 8;
/// An alert held because XAPI did not answer is dropped (with a log line)
/// once this old unless raised again meanwhile: its condition may have
/// cleared, and a late delivery would tell the operator something false.
/// An advice held for a parked host is kept for the park. Applied on every
/// daemon tick ([`expire_held_alerts`]) and again right before a held alert
/// is delivered, so none older is ever delivered.
const PENDING_ALERT_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(15 * 60);

impl PendingAlerts {
    #[cfg(test)]
    fn push(&mut self, name: &str, key: &str, body: &str) {
        self.push_at(name, key, body, false, std::time::Instant::now());
    }

    fn push_at(&mut self, name: &str, key: &str, body: &str, park: bool, at: std::time::Instant) {
        if let Some(item) = self
            .items
            .iter_mut()
            .find(|(n, k, _, _)| n == name && k == key)
        {
            // The same message raised again: keep its place, update it.
            item.2 = body.to_string();
            item.3 = Held { at, park };
            return;
        }
        if self.items.iter().filter(|(n, _, _, _)| n == name).count() >= PENDING_PER_NAME_MAX
            && let Some(i) = self.items.iter().position(|(n, _, _, _)| n == name)
        {
            let (n, _, b, _) = self.items.remove(i);
            warn!(alert = %n, body = %log_body(&b), "held-back XAPI alert dropped undelivered (too many of this name held)");
        }
        if self.items.len() >= PENDING_ALERTS_MAX {
            let (n, _, b, _) = self.items.remove(0);
            warn!(alert = %n, body = %log_body(&b), "held-back XAPI alert dropped undelivered (held set full)");
        }
        self.items
            .push((name.to_string(), key.to_string(), body.to_string(), Held { at, park }));
    }

    /// The message `(name, key)` reached XAPI: drop its held version.
    /// Whether one was held.
    fn delivered(&mut self, name: &str, key: &str) -> bool {
        let before = self.items.len();
        self.items.retain(|(n, k, _, _)| !(n == name && k == key));
        let in_flight = self.in_flight.iter().any(|(n, k)| n == name && k == key);
        if in_flight && !self.withdrawn.iter().any(|(n, k)| n == name && k == key) {
            self.withdrawn.push((name.to_string(), key.to_string()));
        }
        self.items.len() != before || in_flight
    }

    /// The body held under `(name, key)`, if any.
    fn held_body(&self, name: &str, key: &str) -> Option<String> {
        self.items
            .iter()
            .find(|(n, k, _, _)| n == name && k == key)
            .map(|(_, _, b, _)| b.clone())
    }

    /// A flush is about to send `(name, key)`, taken earlier: whether to
    /// drop it instead, because it was withdrawn (or delivered) since, or a
    /// newer version was raised meanwhile (that one goes out instead).
    fn flush_skip(&self, name: &str, key: &str) -> bool {
        self.withdrawn.iter().any(|(n, k)| n == name && k == key)
            || self.items.iter().any(|(n, k, _, _)| n == name && k == key)
    }

    /// A taken alert the flush could not send goes back, keeping its age,
    /// unless it was withdrawn meanwhile or a newer version is queued.
    fn flush_requeue(&mut self, name: &str, key: &str, body: &str, held: Held) {
        if !self.flush_skip(name, key) {
            self.push_at(name, key, body, held.park, held.at);
        }
    }

    /// The flush is done with `(name, key)`.
    fn flush_end(&mut self, name: &str, key: &str) {
        self.in_flight.retain(|(n, k)| !(n == name && k == key));
        self.withdrawn.retain(|(n, k)| !(n == name && k == key));
    }

    /// Up to `max` pending alerts other than `(name, key)`, removed from the
    /// set, each with when and why it was held.
    fn take(&mut self, name: &str, key: &str, max: usize) -> Vec<(String, String, String, Held)> {
        self.take_at(name, key, max, std::time::Instant::now())
    }

    /// Drop (with a log line) every alert held for an XAPI that did not
    /// answer and older than `PENDING_ALERT_MAX_AGE`.
    fn expire_at(&mut self, now: std::time::Instant) {
        self.items.retain(|(n, _, b, h)| !stale_held(n, b, h, now));
    }

    /// `take`, after [`PendingAlerts::expire_at`].
    fn take_at(
        &mut self,
        name: &str,
        key: &str,
        max: usize,
        now: std::time::Instant,
    ) -> Vec<(String, String, String, Held)> {
        self.expire_at(now);
        let mut out = Vec::new();
        let in_flight = &mut self.in_flight;
        self.items.retain(|(n, k, b, h)| {
            if !(n == name && k == key) && out.len() < max {
                in_flight.push((n.clone(), k.clone()));
                out.push((n.clone(), k.clone(), b.clone(), *h));
                false
            } else {
                true
            }
        });
        out
    }
}

/// Whether a held alert is past the age limit (logged when it is).
fn stale_held(name: &str, body: &str, h: &Held, now: std::time::Instant) -> bool {
    let stale = !h.park && now.saturating_duration_since(h.at) >= PENDING_ALERT_MAX_AGE;
    if stale {
        info!(alert = %name, body = %log_body(body), held_secs = now.saturating_duration_since(h.at).as_secs(),
              "held-back XAPI alert dropped undelivered: held too long, its condition may have cleared");
    }
    stale
}

/// Apply the held alerts' age limit now (called on every daemon tick, so a
/// host that sends no alert for a while still drops them on time).
pub fn expire_held_alerts() {
    if let Ok(mut p) = PENDING_ALERTS.lock() {
        p.expire_at(std::time::Instant::now());
    }
}

static PENDING_ALERTS: std::sync::Mutex<PendingAlerts> =
    std::sync::Mutex::new(PendingAlerts {
        items: Vec::new(),
        in_flight: Vec::new(),
        withdrawn: Vec::new(),
    });

/// Withdraw a held-back alert `(name, key)` (`key` as passed to the keyed
/// send) that no longer holds, so it is never delivered late. Whether a held
/// version was removed: `false` when it was delivered (possibly by a
/// background flush) or never held.
pub fn drop_held_alert(name: &str, key: &str) -> bool {
    let (dropped, body) = PENDING_ALERTS
        .lock()
        .map(|mut p| {
            let body = p.held_body(name, key);
            (p.delivered(name, key), body)
        })
        .unwrap_or((false, None));
    if dropped {
        // The held body, as it would have been sent (its SR uuid filled in),
        // not the key, which may carry the placeholder.
        let shown = body.unwrap_or_else(|| key.to_string());
        info!(alert = %name, body = %log_body(&shown), "held-back XAPI alert withdrawn before delivery (no longer true)");
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
    undelivered_since(name, key, body, why, std::time::Instant::now());
}

/// [`undelivered`] for an alert held since `since` (a held alert whose
/// delivery failed keeps its age).
fn undelivered_since(name: &str, key: &str, body: &str, why: &str, since: std::time::Instant) {
    undelivered_log(name, key, body, why);
    if let Ok(mut p) = PENDING_ALERTS.lock() {
        p.push_at(name, key, body, false, since);
    }
}

/// [`undelivered`]'s log line and kernel-log copy (paced), without queuing.
fn undelivered_log(name: &str, key: &str, body: &str, why: &str) {
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
}

/// Keep an HA-disable advice back while a host is parked (logged once per
/// pacing interval), for delivery once none is.
fn hold_ha_advice(name: &str, key: &str, body: &str, view: crate::arbitration::PartitionView) {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if UNDELIVERED_TRACE
        .lock()
        .map(|mut l| l.allow(name, key, now_secs))
        .unwrap_or(true)
    {
        info!(alert = %name, view = view.name(), body = %log_body(body),
              "alert advising an HA disable held back while a host is parked by XAPI; \
               delivered once no host is parked");
    }
    if let Ok(mut p) = PENDING_ALERTS.lock() {
        p.push_at(name, key, body, true, std::time::Instant::now());
    }
}

/// [`hold_ha_advice`] for an advice a flush took: back in the queue for the
/// park, unless withdrawn meanwhile or a newer version is queued.
fn hold_ha_advice_requeue(name: &str, key: &str, body: &str, view: crate::arbitration::PartitionView) {
    let fresh = PENDING_ALERTS.lock().map(|p| !p.flush_skip(name, key)).unwrap_or(false);
    if fresh {
        hold_ha_advice(name, key, body, view);
    }
}

/// Whether an alert `(name, key)` is held back for later delivery.
#[cfg(test)]
pub fn alert_is_held(name: &str, key: &str) -> bool {
    PENDING_ALERTS
        .lock()
        .map(|p| p.items.iter().any(|(n, k, _, _)| n == name && k == key))
        .unwrap_or(false)
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

/// The alert's manual commands with the SR's uuid in place of the
/// placeholder, when known.
pub fn fill_sr_uuid(body: &str, sr: Option<&str>) -> String {
    match sr {
        Some(u) if !u.is_empty() => body.replace("<twinstor SR uuid>", u),
        _ => body.to_string(),
    }
}

/// [`send_alert`] for a message re-sent with changing content: `key` is its
/// stable identity, so a held older version is replaced, not accumulated.
async fn send_alert_keyed(name: &str, key: &str, body: &str) -> bool {
    send_alert_as(name, key, body, true).await
}

/// An alert its caller keeps raising until this returns true, and marks
/// sent only then: not queued when undelivered (the caller's next pass is
/// the retry), so a queued copy can never be delivered after the caller's
/// own re-send. Advice is still routed (not sent while a host is parked).
pub async fn send_xapi_alert_unqueued(body: &str) -> bool {
    send_alert_as("twinstor_degraded", body, body, false).await
}

/// [`send_xapi_alert_unqueued`] for advice to disable HA.
pub async fn send_xapi_alert_advising_ha_disable_unqueued(body: &str) -> bool {
    send_alert_as("twinstor_degraded", &ha_advice_key(body), body, false).await
}

/// [`send_alert_keyed`]; `queue`: kept for later delivery when undelivered.
async fn send_alert_as(name: &str, key: &str, body: &str, queue: bool) -> bool {
    // The manual commands name the SR by its uuid when this host has it
    // recorded; the placeholder stays only when it does not.
    let filled = fill_sr_uuid(body, read_recorded_sr_uuid().as_deref());
    let body = filled.as_str();
    // Advice to disable HA (pool-wide or locally) releases a host XAPI parks
    // in its HA startup loop as a second master: held back (not delivered)
    // while a host is parked, otherwise prefixed with when it is safe.
    let advice = key.starts_with(crate::arbitration::HA_DISABLE_ADVICE_KEY);
    let view = crate::arbitration::current();
    let routed = match crate::arbitration::route_alert(crate::arbitration::advice_held(), advice, body) {
        crate::arbitration::AlertRoute::Send(b) => b,
        crate::arbitration::AlertRoute::Hold => {
            if queue {
                hold_ha_advice(name, key, body, view);
            }
            return false;
        }
    };
    let body = routed.as_str();
    let undelivered = |name: &str, key: &str, body: &str, why: &str| {
        if queue {
            undelivered(name, key, body, why);
        } else {
            undelivered_log(name, key, body, why);
        }
    };
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
    // The local query doubles as the reachability probe the gate needs: it
    // reads empty when XAPI does not answer.
    let host_uuid = local_host_param("uuid").await.unwrap_or_default();

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
                for (n, k, b, h) in held {
                    flush_one(&host_uuid, &n, &k, &b, h).await;
                    if let Ok(mut p) = PENDING_ALERTS.lock() {
                        p.flush_end(&n, &k);
                    }
                }
            });
        } else if let Ok(mut p) = PENDING_ALERTS.lock() {
            for (n, k, _, _) in &held {
                p.flush_end(n, k);
            }
        }
    }
    sent
}

/// Deliver one alert a flush took from the queue.
async fn flush_one(host_uuid: &str, n: &str, k: &str, b: &str, h: Held) {
    {
                    // The age limit again at delivery: nothing older goes out.
                    if stale_held(n, b, &h, std::time::Instant::now()) {
                        return;
                    }
                    // Withdrawn after it was taken, or a newer version queued.
                    if PENDING_ALERTS.lock().map(|p| p.flush_skip(n, k)).unwrap_or(false) {
                        return;
                    }
                    // Advice routed again at delivery: still held while a
                    // host is parked.
                    let view = crate::arbitration::current();
                    let advice = k.starts_with(crate::arbitration::HA_DISABLE_ADVICE_KEY);
                    let routed = match crate::arbitration::route_alert(
                        crate::arbitration::advice_held(),
                        advice,
                        b,
                    ) {
                        crate::arbitration::AlertRoute::Send(b) => b,
                        crate::arbitration::AlertRoute::Hold => {
                            hold_ha_advice_requeue(n, k, b, view);
                            return;
                        }
                    };
                    match message_create(host_uuid, n, &routed).await {
                        Ok(()) => {
                            info!(alert = %n, body = %log_body(&routed), "held-back XAPI alert delivered")
                        }
                        Err(e) => {
                            undelivered_log(n, k, &routed, &e);
                            if let Ok(mut p) = PENDING_ALERTS.lock() {
                                p.flush_requeue(n, k, b, h);
                            }
                        }
                    }
    }
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
mod domu_count_tests {
    /// Only running domUs count, a runnable one waiting for a CPU included:
    /// not dom0, not paused, shut down, crashed or dying ones; names with
    /// spaces read right.
    #[test]
    fn only_running_domus_are_counted() {
        let out = "Name                                        ID   Mem VCPUs\tState\tTime(s)\n\
Domain-0                                     0  4096     8     r-----   12345.6\n\
Debian Test 1                                3  2048     2     -b----     120.1\n\
busy                                         4  1024     1     r-----      99.0\n\
waiting for a cpu                            9  1024     1     ------       5.0\n\
paused one                                   5  1024     1     --p---      10.0\n\
dying                                        6  1024     1     -b---d       1.0\n\
shut                                         7  1024     1     ---s--       1.0\n\
crashed                                      8  1024     1     ----c-       1.0\n";
        assert_eq!(super::running_domus(out), 3);
        // Lines that are not domain rows never count, whatever they hold.
        let noise = "Name ID Mem VCPUs State Time(s)\n\
libxl: error: something went wrong here now\n\
a b c d ------ e\n\
guest 12 1024 1 -b---- 1.0 extra\n\
guest 13 1024 1 -b-x-- 1.0\n\
guest 14 1024 1 -b----- 1.0\n";
        assert_eq!(super::running_domus(noise), 0);
        assert_eq!(super::running_domus(""), 0);
    }
}

#[cfg(test)]
mod alert_delivery_tests {
    use super::{
        PENDING_ALERTS_MAX, PENDING_PER_NAME_MAX, PendingAlerts, PerNameLimiter,
        UNDELIVERED_TRACE_EVERY_SECS,
    };

    /// An alert held because XAPI did not answer is dropped once older than
    /// the maximum age, unless raised again meanwhile; one held for a parked
    /// host is kept.
    #[test]
    fn a_held_alert_too_old_is_dropped_unless_raised_again() {
        let t0 = std::time::Instant::now();
        let mut p = PendingAlerts::default();
        p.push_at("twinstor_degraded", "old", "serving without redundancy", false, t0);
        p.push_at("twinstor_degraded", "raised", "x", false, t0);
        p.push_at("twinstor_degraded", "park", "advice", true, t0);
        let later = t0 + super::PENDING_ALERT_MAX_AGE;
        p.push_at("twinstor_degraded", "raised", "x again", false, later);
        let out = p.take_at("other", "other", 8, later);
        let bodies: Vec<&str> = out.iter().map(|(_, _, b, _)| b.as_str()).collect();
        assert_eq!(bodies, vec!["x again", "advice"], "the old one dropped");
    }

    /// A flush takes alerts out of the queue before it sends them: one
    /// withdrawn (or delivered by a newer send) after it was taken is not
    /// sent, a newer version raised meanwhile wins over the taken one, and
    /// a failed send puts the taken one back only when neither happened.
    #[test]
    fn a_flush_respects_withdrawals_and_newer_versions() {
        let t0 = std::time::Instant::now();
        let held = super::Held { at: t0, park: false };
        let mut p = PendingAlerts::default();
        p.push_at("d", "isolated", "old", false, t0);
        p.push_at("d", "hold", "old hold", false, t0);
        p.push_at("d", "other", "x", false, t0);
        let taken = p.take_at("sending", "k", 8, t0);
        assert_eq!(taken.len(), 3);
        // Withdrawn after it was taken: dropped at send time.
        assert!(p.delivered("d", "isolated"), "an in-flight entry counts as withdrawn");
        assert!(p.flush_skip("d", "isolated"));
        p.flush_requeue("d", "isolated", "old", held);
        assert!(p.items.is_empty(), "a withdrawn entry is not put back");
        // A newer version raised meanwhile: the taken one is not sent, and
        // a failed send does not replace the newer one.
        p.push_at("d", "hold", "new hold", false, t0);
        assert!(p.flush_skip("d", "hold"));
        p.flush_requeue("d", "hold", "old hold", held);
        assert_eq!(p.items.iter().find(|(_, k, _, _)| k == "hold").map(|i| i.2.as_str()), Some("new hold"));
        // Neither: a failed send puts it back with its age.
        assert!(!p.flush_skip("d", "other"));
        p.flush_requeue("d", "other", "x", held);
        assert!(p.items.iter().any(|(_, k, _, h)| k == "other" && h.at == t0));
        for k in ["isolated", "hold", "other"] {
            p.flush_end("d", k);
        }
        assert!(p.in_flight.is_empty() && p.withdrawn.is_empty());
        // Once done, a later withdrawal of the same key is a plain one.
        assert!(!p.delivered("d", "isolated"));
    }

    /// A withdrawal is logged with the held body (its SR uuid filled in),
    /// not with the key, which may carry the placeholder.
    #[test]
    fn a_withdrawal_names_the_held_body() {
        let mut p = PendingAlerts::default();
        p.push_at("d", "run 'xe x heartbeat-sr-uuids=<twinstor SR uuid>'", "run 'xe x heartbeat-sr-uuids=00000000-aaaa'", false, std::time::Instant::now());
        assert_eq!(p.held_body("d", "run 'xe x heartbeat-sr-uuids=<twinstor SR uuid>'").as_deref(), Some("run 'xe x heartbeat-sr-uuids=00000000-aaaa'"));
        assert_eq!(p.held_body("d", "other"), Option::None);
    }

    /// The age limit applies without a send: on a tick pass, so an alert
    /// held on a host that sends nothing else for half an hour is dropped on
    /// time.
    #[test]
    fn the_age_limit_applies_without_a_send() {
        let t0 = std::time::Instant::now();
        let mut p = PendingAlerts::default();
        p.push_at("twinstor_degraded", "a", "isolated", false, t0);
        p.push_at("twinstor_degraded", "b", "advice", true, t0);
        p.expire_at(t0 + super::PENDING_ALERT_MAX_AGE - std::time::Duration::from_secs(1));
        assert_eq!(p.items.len(), 2);
        p.expire_at(t0 + super::PENDING_ALERT_MAX_AGE);
        let keys: Vec<&str> = p.items.iter().map(|(_, k, _, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["b"], "the parked host's advice is kept");
        // At delivery: a taken alert past the limit is not delivered.
        let h = super::Held { at: t0, park: false };
        assert!(super::stale_held("x", "y", &h, t0 + super::PENDING_ALERT_MAX_AGE));
        assert!(!super::stale_held("x", "y", &h, t0));
    }

    /// A held alert whose condition cleared is withdrawn by its key before
    /// delivery (the "serving without redundancy" alert when redundancy
    /// returns).
    #[test]
    fn a_held_alert_is_withdrawn_when_its_condition_clears() {
        let key = "test-only: serving without redundancy";
        super::PENDING_ALERTS.lock().unwrap().push("twinstor_degraded", key, key);
        assert!(super::alert_is_held("twinstor_degraded", key));
        assert!(super::drop_held_alert("twinstor_degraded", key));
        assert!(!super::alert_is_held("twinstor_degraded", key));
    }

    /// The manual commands carry the recorded SR uuid; the placeholder
    /// stays only when it is unknown.
    #[test]
    fn the_sr_uuid_is_filled_in_when_known() {
        let b = "on the master run 'xe pool-ha-enable heartbeat-sr-uuids=<twinstor SR uuid> ha-config:timeout=120'";
        assert_eq!(
            super::fill_sr_uuid(b, Some("00000000-aaaa")),
            "on the master run 'xe pool-ha-enable heartbeat-sr-uuids=00000000-aaaa ha-config:timeout=120'"
        );
        assert_eq!(super::fill_sr_uuid(b, Option::None), b);
        assert_eq!(super::fill_sr_uuid(b, Some("")), b);
    }

    /// A held message under a key is replaced by a newer one under the same
    /// key, and dropped when forgotten (a reset's "freed" superseded by its
    /// futile verdict sent as advice).
    #[test]
    fn a_held_alert_is_replaced_or_dropped_by_its_key() {
        let mut p = PendingAlerts::default();
        p.push("twinstor_degraded", "peer-emergency-verdict-1", "freed");
        p.push("twinstor_degraded", "peer-emergency-verdict-1", "futile");
        assert_eq!(p.items.len(), 1);
        assert_eq!(p.items[0].2, "futile");
        assert!(p.delivered("twinstor_degraded", "peer-emergency-verdict-1"));
        assert!(p.items.is_empty());
    }

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
        let held: Vec<&str> = p.items.iter().map(|(_, _, b, _)| b.as_str()).collect();
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
                .map(|(_, _, x, _)| x.as_str())
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
                .map(|(_, _, b, _)| b.as_str())
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
        assert!(held.iter().any(|(_, _, b, _)| b == "host-enable failed"));
        assert!(held.iter().any(|(_, _, b, _)| b == "Holding for 20 minutes."));
    }

    #[test]
    fn held_back_alerts_are_kept_by_name_and_key_and_handed_out_later() {
        let mut p = PendingAlerts::default();
        let e = |n: &str, b: &str| (n.to_string(), b.to_string(), b.to_string());
        let strip = |v: Vec<(String, String, String, super::Held)>| v.into_iter().map(|(n, k, b, _)| (n, k, b)).collect::<Vec<_>>();
        p.push("a", "one", "one");
        p.push("b", "two", "two");
        p.push("a", "three", "three");
        p.push("a", "three", "three");
        assert_eq!(
            strip(p.take("b", "two", 4)),
            vec![e("a", "one"), e("a", "three")],
            "distinct bodies of one name both kept; the alert being sent excluded"
        );
        assert_eq!(strip(p.take("x", "", 4)), vec![e("b", "two")]);
        assert!(p.take("x", "", 4).is_empty());
        p.push("c", "four", "four");
        p.push("c", "five", "five");
        p.delivered("c", "four");
        assert_eq!(
            strip(p.take("x", "", 4)),
            vec![e("c", "five")],
            "only the delivered message is dropped"
        );
        for i in 0..PENDING_PER_NAME_MAX + 3 {
            let m = format!("distinct message {}", "x".repeat(i + 1));
            p.push("noisy", &m, &m);
        }
        p.push("quiet", "once", "once");
        assert_eq!(
            p.items.iter().filter(|(n, _, _, _)| n == "noisy").count(),
            PENDING_PER_NAME_MAX,
            "a name keeps its latest few"
        );
        assert!(
            p.items.iter().any(|(n, _, _, _)| n == "quiet"),
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
    fn other_config_keys_are_matched_whole() {
        use super::other_config_value;
        // As xe printed a pool's map on a test pool, colons inside keys and values.
        let map = "xo:clientInfo:5051: {\"a\":1}; twinstor-default-ha-timeout-prior: 120:; \
                   auto_poweron: true; default_ha_timeout: 120";
        assert_eq!(
            other_config_value(map, "auto_poweron").as_deref(),
            Some("true")
        );
        assert_eq!(
            other_config_value(map, "default_ha_timeout").as_deref(),
            Some("120")
        );
        assert_eq!(other_config_value(map, "poweron"), None);
        assert_eq!(other_config_value("", "auto_poweron"), None);
    }

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

    #[test]
    fn default_ha_timeout_record_keeps_the_first_prior_value() {
        use super::default_ha_timeout_record;
        assert_eq!(
            default_ha_timeout_record(120, None, None).as_deref(),
            Some("120:")
        );
        assert_eq!(
            default_ha_timeout_record(90, Some("75"), None).as_deref(),
            Some("90:75")
        );
        // A setup re-run finds twinstor's own value: the record stays as is.
        assert_eq!(
            default_ha_timeout_record(120, Some("120"), Some("120:")),
            None
        );
    }

    #[test]
    fn upgrade_sets_the_default_ha_timeout_only_on_an_untouched_pool() {
        use super::upgrade_default_ha_timeout;
        // Unset: the pool's explicit timeout wins over the mode's.
        assert_eq!(
            upgrade_default_ha_timeout(None, None, Some(180), 120),
            Some((180, "180:".to_string()))
        );
        assert_eq!(
            upgrade_default_ha_timeout(None, None, None, 90),
            Some((90, "90:".to_string()))
        );
        // The operator's value stays, whatever it is.
        assert_eq!(
            upgrade_default_ha_timeout(Some("60"), None, Some(90), 90),
            None
        );
        // Set by setup or an earlier upgrade.
        assert_eq!(
            upgrade_default_ha_timeout(Some("120"), Some("120:"), Some(120), 120),
            None
        );
        // The operator removed what twinstor set: not put back.
        assert_eq!(
            upgrade_default_ha_timeout(None, Some("90:"), Some(90), 90),
            None
        );
    }

    #[test]
    fn default_ha_timeout_restore_never_undoes_an_operator_change() {
        use super::DefaultHaTimeoutRestore::*;
        use super::default_ha_timeout_restore;
        assert_eq!(default_ha_timeout_restore(None, Some("120")), Nothing);
        assert_eq!(
            default_ha_timeout_restore(Some("120:"), Some("120")),
            Restore(None)
        );
        assert_eq!(
            default_ha_timeout_restore(Some("90:75"), Some("90")),
            Restore(Some("75".into()))
        );
        // Changed (or removed) since twinstor set it: leave it alone.
        assert_eq!(
            default_ha_timeout_restore(Some("120:"), Some("150")),
            DropRecord
        );
        assert_eq!(default_ha_timeout_restore(Some("120:"), None), DropRecord);
        assert_eq!(
            default_ha_timeout_restore(Some("garbage"), Some("120")),
            DropRecord
        );
        assert_eq!(default_ha_timeout_restore(Some(":"), Some("")), DropRecord);
    }

    use super::{
        ALERT_GATE_MAX, ALERT_GATE_MIN, AlertGate, parse_sr_uuid_for_iqn,
        twinstor_sr_absent_confirmed,
    };
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

    #[tokio::test]
    async fn sr_absence_is_never_confirmed_without_an_answer() {
        // No xe here: every lookup fails, which must not read as "no SR".
        assert!(!twinstor_sr_absent_confirmed("iqn.2025-01.com.xcp-ng:twinstor-data").await);
    }

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
        // proceeds.
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

/// The peer's xapi role, probed without credentials by [`peer_xapi_role`] on
/// its management address. A bogus login yields, per the answers observed on
/// the lab pools:
/// - success or `SESSION_AUTHENTICATION_FAILED`: the peer's xapi is the
///   master (`Master`);
/// - `HOST_IS_SLAVE`: the peer is a slave and names its master (`SlaveOf`);
/// - `HA_POOL_IS_ENABLED_BUT_HOST_IS_DISABLED`: the peer's xapi restarted
///   while the host was locally disarmed under an armed pool (`ArmedMismatch`),
///   a state XAPI never leaves by itself. A host that is locally disarmed but
///   whose xapi did not restart (after a whole-switch hold or a stand-down,
///   its xhad back in the liveset) still answers HOST_IS_SLAVE, or as master;
/// - any other XenAPI error (`HA_HEARTBEAT_DAEMON_STARTUP_FAILED` from a host
///   just released from the HA startup loop, for example), or a TCP reset
///   after TLS (a host parked in the HA startup loop, or with xapi stopped,
///   whose stunnel still accepts): `Unknown`, and the caller withholds;
/// - no TCP connection (timeout, host or network unreachable), or not sent
///   because the management address is not answering (P false):
///   `Unreachable`.
///
/// Before, every answer but `HOST_IS_SLAVE` read as `Master`, so a wedged or
/// just-released slave read as the real master.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerXapiRole {
    Master,
    SlaveOf(String),
    ArmedMismatch,
    Unknown,
    Unreachable,
}

impl PeerXapiRole {
    /// The peer answers normally (as the master or as a slave).
    pub fn answers_normally(&self) -> bool {
        matches!(self, Self::Master | Self::SlaveOf(_))
    }
}

/// Classify the probe script's output. `None` (the bounded call failed or
/// hung after the TCP connection) is `Unknown`: something answered on the
/// port, so the peer is not shown unreachable.
pub fn parse_peer_xapi_role(out: Option<&str>) -> PeerXapiRole {
    let Some(out) = out else {
        return PeerXapiRole::Unknown;
    };
    for l in out.lines().map(str::trim) {
        if let Some(a) = l.strip_prefix("SLAVE_OF:") {
            return PeerXapiRole::SlaveOf(a.trim().to_string());
        }
        match l {
            "MASTER" => return PeerXapiRole::Master,
            "ARMED_MISMATCH" => return PeerXapiRole::ArmedMismatch,
            "UNREACHABLE" => return PeerXapiRole::Unreachable,
            _ if l.starts_with("OTHER:") || l.starts_with("RESET:") => {
                return PeerXapiRole::Unknown;
            }
            _ => {}
        }
    }
    PeerXapiRole::Unknown
}

/// The probe script. The TCP connect is timed on its own (5 s): only a
/// connect that times out or meets an unreachable host or network reads
/// `UNREACHABLE`. A refused connect comes from the host itself, and anything
/// after a completed connect (a TLS or login read that times out or is
/// reset) means something holds the port: `RESET` (Unknown), never
/// unreachable.
fn peer_xapi_role_script(peer_addr: &str) -> String {
    format!(
        "import XenAPI,socket,errno,sys\n\
         U=(errno.EHOSTUNREACH, errno.ENETUNREACH, errno.ETIMEDOUT)\n\
         try:\n\
         \x20c=socket.create_connection(('{peer_addr}', 443), 5)\n\
         \x20c.close()\n\
         except socket.timeout:\n print('UNREACHABLE'); sys.exit(0)\n\
         except OSError as e:\n\
         \x20print('UNREACHABLE' if e.errno in U else 'RESET:'+type(e).__name__); sys.exit(0)\n\
         socket.setdefaulttimeout(10)\n\
         try:\n\
         \x20s=XenAPI.Session('https://{peer_addr}', ignore_ssl=True)\n\
         \x20s.xenapi.login_with_password('twinstor-probe','x')\n\
         \x20print('MASTER')\n\
         except XenAPI.Failure as f:\n\
         \x20d=f.details or ['']\n\
         \x20if d[0]=='HOST_IS_SLAVE' and len(d)>1:\n  print('SLAVE_OF:'+str(d[1]))\n\
         \x20elif d[0]=='SESSION_AUTHENTICATION_FAILED':\n  print('MASTER')\n\
         \x20elif d[0]=='HA_POOL_IS_ENABLED_BUT_HOST_IS_DISABLED':\n  print('ARMED_MISMATCH')\n\
         \x20else:\n  print('OTHER:'+str(d[0]))\n\
         except Exception as e:\n print('RESET:'+type(e).__name__)\n"
    )
}

/// The same login probe against this host's own XAPI, over its local socket
/// (no network, no address). A login that succeeds is logged out at once.
/// Over the local socket a successful login says nothing about the role (a
/// healthy slave accepts it too): it answers `OTHER:LOGIN_OK`, read as
/// `Unknown`. Only the armed-mismatch refusal is meaningful here.
fn local_xapi_role_script() -> &'static str {
    "import XenAPI,sys,socket\n\
     socket.setdefaulttimeout(4)\n\
     try:\n\
     \x20s=XenAPI.xapi_local()\n\
     \x20s.xenapi.login_with_password('root','')\n\
     \x20print('OTHER:LOGIN_OK')\n\
     \x20try:\n  s.xenapi.session.logout()\n\
     \x20except Exception:\n  pass\n\
     except XenAPI.Failure as f:\n\
     \x20d=f.details or ['']\n\
     \x20if d[0]=='HOST_IS_SLAVE' and len(d)>1:\n  print('SLAVE_OF:'+str(d[1]))\n\
     \x20elif d[0]=='HA_POOL_IS_ENABLED_BUT_HOST_IS_DISABLED':\n  print('ARMED_MISMATCH')\n\
     \x20else:\n  print('OTHER:'+str(d[0]))\n\
     except Exception as e:\n print('RESET:'+type(e).__name__)\n"
}

/// This host's own XAPI answer to a login: `ArmedMismatch` is local evidence
/// that the host is disarmed under an armed pool (XAPI then refuses every
/// login with HA_POOL_IS_ENABLED_BUT_HOST_IS_DISABLED).
#[cfg_attr(test, allow(dead_code))]
pub async fn local_xapi_role() -> PeerXapiRole {
    // A login on the local socket: 5 s is ample when the local xapi answers.
    let out = crate::cmd::cmd_output_for("python3", &["-c", local_xapi_role_script()], 5)
        .await
        .ok();
    parse_peer_xapi_role(out.as_deref())
}

#[cfg_attr(test, allow(dead_code))]
pub async fn peer_xapi_role(peer_addr: &str) -> PeerXapiRole {
    let out = crate::cmd::cmd_output_for("python3", &["-c", &peer_xapi_role_script(peer_addr)], 15)
        .await
        .ok();
    parse_peer_xapi_role(out.as_deref())
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

/// Running guests (domUs) via `xl list`, not counting paused, shut-down,
/// crashed or dying ones (a runnable guest not on a CPU, all dashes, counts);
/// bounded like `local_running_vm_count`.
pub async fn local_running_domu_count() -> u32 {
    crate::cmd::cmd_output_for("xl", &["list"], 10)
        .await
        .map(|out| running_domus(&out))
        .unwrap_or(0)
}

/// Count the running domUs in `xl list` output. The columns are Name, ID,
/// Mem, VCPUs, State, Time(s); a name may hold spaces, so the ID and State
/// are read from the end. State letters: r running, b blocked (idle but
/// running), none (runnable, waiting for a CPU), p paused, s shut down, c
/// crashed, d dying; only the last four exclude a guest.
pub fn running_domus(xl_list: &str) -> u32 {
    xl_list
        .lines()
        .skip(1)
        .filter(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 6 {
                return false;
            }
            let (id, state) = (f[f.len() - 5], f[f.len() - 2]);
            // A domain row only: a numeric ID and a six-character state from
            // xl's alphabet (r b p s c d, or a dash).
            let shaped = id.parse::<u32>().is_ok()
                && state.len() == 6
                && state.chars().all(|c| "rbpscd-".contains(c));
            shaped && id != "0" && !state.contains(['p', 's', 'c', 'd'])
        })
        .count() as u32
}

/// The peer host's `host-metrics-live` in this node's pool DB, or `None` if
/// unreadable. Used to detect a stranded former master: a peer that is alive on
/// the replication link but reads not live here is acting as its own pool
/// master, so this node is the spurious second master.
pub async fn peer_host_live(peer_hostname: &str) -> Option<bool> {
    match host_param(peer_hostname, "host-metrics-live").await?.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod host_identity_tests {
    use super::{HostUuidCache, host_param_with, resolve_host_uuid};
    use std::sync::Mutex;

    // Spelled in pieces so the source guard below does not match this module.
    const HOST_LIST: &str = concat!("host", "-list");
    const BY_HOSTNAME: &str = concat!("hostname", "=");
    const BY_NAME_LABEL: &str = concat!("name-label", "=");

    struct FakeHost {
        uuid: String,
        hostname: String,
        name_label: String,
        enabled: bool,
    }

    /// A pool DB answering `xe host-list <filters> params=<p> --minimal` the
    /// way XAPI does: matching values comma-joined, empty when nothing
    /// matches, `None` (the bounded call failing) while XAPI is down.
    struct FakeXapi {
        hosts: Mutex<Vec<FakeHost>>,
        calls: Mutex<Vec<Vec<String>>>,
        down: Mutex<bool>,
    }

    fn host(uuid: &str, hostname: &str, name_label: &str) -> FakeHost {
        FakeHost {
            uuid: uuid.into(),
            hostname: hostname.into(),
            name_label: name_label.into(),
            enabled: true,
        }
    }

    impl FakeXapi {
        /// The customer pool from the field report: both hosts renamed in XO,
        /// so no name-label equals a hostname.
        fn renamed_pool() -> Self {
            Self {
                hosts: Mutex::new(vec![
                    host(
                        "987a31e2-0000-0000-0000-000000000001",
                        "xcp-ng-zanepsdm",
                        "xcp-ng-host 1",
                    ),
                    host(
                        "5b1c0d44-0000-0000-0000-000000000002",
                        "xcp-ng-upzpwzfj",
                        "xcp-ng-host2",
                    ),
                ]),
                calls: Mutex::new(Vec::new()),
                down: Mutex::new(false),
            }
        }

        fn answer(&self, args: &[String]) -> Option<String> {
            self.calls.lock().unwrap().push(args.to_vec());
            if *self.down.lock().unwrap() {
                return None;
            }
            assert_eq!(args.first().map(String::as_str), Some(HOST_LIST));
            let hosts = self.hosts.lock().unwrap();
            let mut matching: Vec<&FakeHost> = hosts.iter().collect();
            let mut param = "uuid";
            for a in &args[1..] {
                if let Some(v) = a.strip_prefix(BY_HOSTNAME) {
                    matching.retain(|h| h.hostname == v);
                } else if let Some(v) = a.strip_prefix(BY_NAME_LABEL) {
                    matching.retain(|h| h.name_label == v);
                } else if let Some(v) = a.strip_prefix("uuid=") {
                    matching.retain(|h| h.uuid == v);
                } else if let Some(v) = a.strip_prefix("params=") {
                    param = match v {
                        "uuid" => "uuid",
                        "enabled" => "enabled",
                        other => panic!("fake XAPI has no param {other}"),
                    };
                } else {
                    panic!("fake XAPI does not understand {a}");
                }
            }
            let values: Vec<String> = matching
                .iter()
                .map(|h| match param {
                    "uuid" => h.uuid.clone(),
                    _ => h.enabled.to_string(),
                })
                .collect();
            Some(values.join(","))
        }

        fn calls_by_hostname(&self) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.iter().any(|a| a.starts_with(BY_HOSTNAME)))
                .count()
        }

        fn any_call_by_name_label(&self) -> bool {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.iter().any(|a| a.starts_with(BY_NAME_LABEL)))
        }
    }

    fn cache() -> Mutex<HostUuidCache> {
        Mutex::new(HostUuidCache::new())
    }

    const PEER: &str = "xcp-ng-upzpwzfj";
    const PEER_UUID: &str = "5b1c0d44-0000-0000-0000-000000000002";

    #[tokio::test]
    async fn a_renamed_peer_is_still_found() {
        let fake = FakeXapi::renamed_pool();
        let xe = |args: Vec<String>| {
            let r = fake.answer(&args);
            async move { r }
        };
        // The 0.7.4 lookup: the peer's hostname as a name-label matches nothing
        // once the host is renamed, which held the pool-HA re-arm forever.
        assert_eq!(
            fake.answer(&[
                HOST_LIST.into(),
                format!("{BY_NAME_LABEL}{PEER}"),
                "params=enabled".into()
            ])
            .as_deref(),
            Some("")
        );
        fake.calls.lock().unwrap().clear();
        assert_eq!(
            host_param_with(&cache(), PEER, "enabled", &xe)
                .await
                .as_deref(),
            Some("true")
        );
        assert!(!fake.any_call_by_name_label());
    }

    #[tokio::test]
    async fn the_hostname_is_resolved_once_then_every_query_goes_by_uuid() {
        let fake = FakeXapi::renamed_pool();
        let xe = |args: Vec<String>| {
            let r = fake.answer(&args);
            async move { r }
        };
        let c = cache();
        for _ in 0..3 {
            assert_eq!(
                host_param_with(&c, PEER, "enabled", &xe).await.as_deref(),
                Some("true")
            );
        }
        assert_eq!(fake.calls_by_hostname(), 1);
        assert_eq!(c.lock().unwrap().get(PEER).as_deref(), Some(PEER_UUID));
        // Renaming the host again changes nothing.
        fake.hosts.lock().unwrap()[1].name_label = "renamed once more".into();
        fake.hosts.lock().unwrap()[1].enabled = false;
        assert_eq!(
            host_param_with(&c, PEER, "enabled", &xe).await.as_deref(),
            Some("false")
        );
        assert_eq!(fake.calls_by_hostname(), 1);
    }

    #[tokio::test]
    async fn xapi_down_answers_unknown_and_caches_nothing() {
        let fake = FakeXapi::renamed_pool();
        let xe = |args: Vec<String>| {
            let r = fake.answer(&args);
            async move { r }
        };
        let c = cache();
        *fake.down.lock().unwrap() = true;
        assert_eq!(host_param_with(&c, PEER, "enabled", &xe).await, None);
        assert_eq!(c.lock().unwrap().get(PEER), None);
        *fake.down.lock().unwrap() = false;
        assert_eq!(
            host_param_with(&c, PEER, "enabled", &xe).await.as_deref(),
            Some("true")
        );
    }

    #[tokio::test]
    async fn xapi_down_after_caching_keeps_the_entry() {
        let fake = FakeXapi::renamed_pool();
        let xe = |args: Vec<String>| {
            let r = fake.answer(&args);
            async move { r }
        };
        let c = cache();
        host_param_with(&c, PEER, "enabled", &xe).await;
        *fake.down.lock().unwrap() = true;
        assert_eq!(host_param_with(&c, PEER, "enabled", &xe).await, None);
        assert_eq!(c.lock().unwrap().get(PEER).as_deref(), Some(PEER_UUID));
    }

    #[tokio::test]
    async fn a_peer_rejoined_under_a_new_uuid_is_resolved_again() {
        let fake = FakeXapi::renamed_pool();
        let xe = |args: Vec<String>| {
            let r = fake.answer(&args);
            async move { r }
        };
        let c = cache();
        host_param_with(&c, PEER, "enabled", &xe).await;
        let new_uuid = "c0ffee00-0000-0000-0000-000000000003";
        fake.hosts.lock().unwrap()[1].uuid = new_uuid.into();
        assert_eq!(
            host_param_with(&c, PEER, "enabled", &xe).await.as_deref(),
            Some("true")
        );
        assert_eq!(c.lock().unwrap().get(PEER).as_deref(), Some(new_uuid));
        assert_eq!(fake.calls_by_hostname(), 2);
    }

    #[tokio::test]
    async fn a_peer_gone_from_the_pool_reads_unknown() {
        let fake = FakeXapi::renamed_pool();
        let xe = |args: Vec<String>| {
            let r = fake.answer(&args);
            async move { r }
        };
        let c = cache();
        host_param_with(&c, PEER, "enabled", &xe).await;
        fake.hosts.lock().unwrap().remove(1);
        assert_eq!(host_param_with(&c, PEER, "enabled", &xe).await, None);
        assert_eq!(c.lock().unwrap().get(PEER), None);
        assert_eq!(
            host_param_with(&c, "no-such-host", "enabled", &xe).await,
            None
        );
        assert_eq!(c.lock().unwrap().get("no-such-host"), None);
    }

    #[tokio::test]
    async fn a_duplicate_hostname_answers_the_first_match_uncached() {
        let fake = FakeXapi::renamed_pool();
        fake.hosts.lock().unwrap().push(host(
            "dddddddd-0000-0000-0000-000000000004",
            PEER,
            "a clone",
        ));
        let xe = |args: Vec<String>| {
            let r = fake.answer(&args);
            async move { r }
        };
        let c = cache();
        assert_eq!(
            resolve_host_uuid(&c, PEER, &xe).await,
            Some((PEER_UUID.to_string(), false))
        );
        assert_eq!(c.lock().unwrap().get(PEER), None);
        resolve_host_uuid(&c, PEER, &xe).await;
        assert_eq!(fake.calls_by_hostname(), 2);
    }

    #[test]
    fn forget_drops_only_the_uuid_it_names() {
        let mut c = HostUuidCache::new();
        c.put(PEER, "old");
        c.put(PEER, "new");
        c.forget(PEER, "old");
        assert_eq!(c.get(PEER).as_deref(), Some("new"));
        c.forget(PEER, "new");
        assert_eq!(c.get(PEER), None);
    }

    /// No host query in the daemon may name a host by `name-label` (renameable
    /// in XO at any time), and only the resolver may query by hostname.
    #[test]
    fn host_queries_never_go_by_name() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let host_list = concat!("\"host", "-list\"");
        let by_hostname = concat!("hostname=", "{");
        let mut hostname_queries = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                if line.contains(host_list) {
                    for (j, l) in lines.iter().enumerate().skip(i).take(5) {
                        assert!(
                            !l.contains(BY_NAME_LABEL),
                            "{}:{}: a host-list by name-label",
                            path.display(),
                            j + 1
                        );
                    }
                }
                if line.contains(by_hostname) {
                    hostname_queries.push(format!("{}:{}", path.display(), i + 1));
                }
            }
        }
        assert_eq!(
            hostname_queries.len(),
            1,
            "only resolve_host_uuid may query by hostname: {hostname_queries:?}"
        );
    }
}

#[cfg(test)]
mod pool_ha_cache_tests {
    use super::{POOL_HA_CACHE_MAX_AGE_SECS, pool_ha_cache_pack, pool_ha_cache_value};

    /// A pool HA cycle passes through "off" and is cached; then this host's
    /// own XAPI stops answering (left disarmed under the armed pool): no read
    /// refreshes the cache, and after a few minutes it reads unknown, not
    /// "off" for good.
    #[test]
    fn a_cached_off_turns_unknown_once_reads_stop() {
        let t = 1_000;
        let off = pool_ha_cache_pack(false, t);
        assert_eq!(pool_ha_cache_value(off, t + 60), Some(false));
        assert_eq!(pool_ha_cache_value(off, t + POOL_HA_CACHE_MAX_AGE_SECS), Some(false));
        assert_eq!(pool_ha_cache_value(off, t + POOL_HA_CACHE_MAX_AGE_SECS + 1), None);
        assert_eq!(pool_ha_cache_value(pool_ha_cache_pack(true, t), t + 1), Some(true));
        assert_eq!(pool_ha_cache_value(0, t), None);
    }
}

#[cfg(test)]
mod peer_role_tests {
    use super::{
        PeerXapiRole, local_xapi_role_script, parse_peer_xapi_role, peer_xapi_role_script,
    };

    /// The local probe asks this host's own XAPI over its local socket, maps
    /// the same codes, and logs out a session it opened. A successful local
    /// login is no role (a healthy slave accepts it too): it reads Unknown,
    /// never Master.
    #[test]
    fn local_xapi_role_script_uses_the_local_socket() {
        let s = local_xapi_role_script();
        assert!(!s.contains("print('MASTER')"), "a local login says nothing about the role");
        assert!(s.contains("OTHER:LOGIN_OK"));
        assert_eq!(parse_peer_xapi_role(Some("OTHER:LOGIN_OK\n")), PeerXapiRole::Unknown);
        for code in [
            "xapi_local()",
            "HOST_IS_SLAVE",
            "HA_POOL_IS_ENABLED_BUT_HOST_IS_DISABLED",
            "session.logout()",
            "setdefaulttimeout",
            "RESET:",
        ] {
            assert!(s.contains(code), "{code}");
        }
        assert!(!s.contains("https://") && !s.contains("create_connection"));
    }

    /// Both probe scripts compile as Python (skipped where python3 is
    /// absent). Only compiled, never run: no XAPI is contacted.
    #[test]
    fn the_probe_scripts_compile_as_python() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        if Command::new("python3").arg("--version").output().is_err() {
            eprintln!("python3 absent: skipped");
            return;
        }
        for (name, script) in [
            ("local", local_xapi_role_script().to_string()),
            ("peer", peer_xapi_role_script("192.0.2.1")),
        ] {
            let mut child = Command::new("python3")
                .args(["-c", "import sys; compile(sys.stdin.read(), 'probe', 'exec')"])
                .stdin(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("python3 runs");
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(script.as_bytes())
                .expect("write the script");
            let out = child.wait_with_output().expect("python3 exits");
            assert!(
                out.status.success(),
                "{name} script does not compile: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// The answers observed on the lab pools (healthy master, healthy or cut
    /// slave, parked host, released host, wedged host, xapi stopped, cut).
    #[test]
    fn peer_xapi_role_mapping_from_observed_answers() {
        use PeerXapiRole::*;
        // Healthy master: the bogus login fails authentication.
        assert_eq!(parse_peer_xapi_role(Some("MASTER\n")), Master);
        // Healthy slave, or a cut slave whose xhad is still Online.
        assert_eq!(
            parse_peer_xapi_role(Some("SLAVE_OF:198.51.100.47\n")),
            SlaveOf("198.51.100.47".into())
        );
        // Wedged: pool HA on, host locally disarmed, and its xapi restarted
        // since (before that restart it still answers as slave or master).
        assert_eq!(parse_peer_xapi_role(Some("ARMED_MISMATCH\n")), ArmedMismatch);
        // Released from the startup loop, master not yet connected.
        assert_eq!(
            parse_peer_xapi_role(Some("OTHER:HA_HEARTBEAT_DAEMON_STARTUP_FAILED\n")),
            Unknown
        );
        // Parked in the startup loop, or xapi stopped: TCP reset after TLS.
        assert_eq!(parse_peer_xapi_role(Some("RESET:ConnectionResetError\n")), Unknown);
        assert_eq!(parse_peer_xapi_role(Some("RESET:RemoteDisconnected\n")), Unknown);
        assert_eq!(parse_peer_xapi_role(Some("RESET:ConnectionRefusedError\n")), Unknown);
        // Across the cut: no TCP connection.
        assert_eq!(parse_peer_xapi_role(Some("UNREACHABLE\n")), Unreachable);
        // The command itself failed or hung: something held the port.
        assert_eq!(parse_peer_xapi_role(None), Unknown);
        assert_eq!(parse_peer_xapi_role(Some("")), Unknown);
        assert!(Master.answers_normally() && SlaveOf("x".into()).answers_normally());
        assert!(!ArmedMismatch.answers_normally() && !Unknown.answers_normally());
        assert!(!Unreachable.answers_normally());
    }

    #[test]
    fn peer_xapi_role_script_names_every_code() {
        let s = peer_xapi_role_script("192.0.2.1");
        for code in [
            "HOST_IS_SLAVE",
            "SESSION_AUTHENTICATION_FAILED",
            "HA_POOL_IS_ENABLED_BUT_HOST_IS_DISABLED",
            "UNREACHABLE",
            "RESET:",
            "setdefaulttimeout",
            "create_connection(('192.0.2.1', 443), 5)",
            "https://192.0.2.1",
        ] {
            assert!(s.contains(code), "{code}");
        }
    }
}

#[cfg(test)]
mod ha_advice_tests {
    use crate::arbitration::{PartitionView, set_current};

    /// End to end through the real sender: while a host is parked, an alert
    /// advising an HA disable is not delivered and stays held back; an alert
    /// that merely mentions the command is not affected by the view.
    #[tokio::test]
    async fn ha_disable_advice_is_held_while_a_host_is_parked() {
        let body = "test: on the master run 'xe pool-ha-disable' (held-back check)";
        set_current(PartitionView::PeerParked);
        let delivered = super::send_xapi_alert_advising_ha_disable(body).await;
        set_current(PartitionView::None);
        assert!(!delivered, "not delivered while parked");
        assert!(super::alert_is_held("twinstor_degraded", &super::ha_advice_key(body)));
        let _ = super::drop_held_alert("twinstor_degraded", &super::ha_advice_key(body));
    }
}

#[cfg(test)]
mod pending_task_tests {
    use super::{parse_pending_ha_tasks, parse_xapi_time};

    #[test]
    fn pending_ha_tasks_parse_with_their_creation_time() {
        assert_eq!(parse_xapi_time("19700101T00:01:40Z"), Some(100));
        assert_eq!(parse_xapi_time("20261117T08:05:23Z"), Some(1_794_902_723));
        assert_eq!(parse_xapi_time("garbage"), None);
        let out = "uuid ( RO)           : aaaa\n     name-label ( RO): Async.pool.enable_ha\n        created ( RO): 20261117T08:05:23Z\n\n\
                   uuid ( RO)           : bbbb\n     name-label ( RO): VM.start\n        created ( RO): 20261117T08:05:24Z\n\n\
                   uuid ( RO)           : cccc\n     name-label ( RO): pool.disable_ha\n        created ( RO): \n";
        let t = parse_pending_ha_tasks(out);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].uuid, "aaaa");
        assert_eq!(t[0].created_unix, Some(1_794_902_723));
        assert_eq!(t[1].name, "pool.disable_ha");
        assert_eq!(t[1].created_unix, None);
    }
}
