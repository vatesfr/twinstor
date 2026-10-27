use std::net::IpAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::cmd::{cmd_output, run_cmd, run_cmd_quiet};

const CONFIG_DIR: &str = "/etc/twinstor";
const CONFIG_PATH: &str = "/etc/twinstor/twinstor.toml";
pub(crate) const PING_TARGETS_PATH: &str = "/etc/twinstor/ping-targets";
pub(crate) const DIRECT_PEER_PATH: &str = "/etc/twinstor/direct-peer";

/// Parse the ping-targets file the same way the fence-peer handler's shell
/// loop does: one entry per line, `#` comments, whitespace stripped. Shared
/// by the daemon's tiebreaker-health monitor and `twinstor status` so the
/// probe set they report is the set the handler would use.
pub(crate) fn parse_ping_targets(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim().replace(' ', ""))
        .filter(|l| !l.is_empty())
        .collect()
}

/// Format version of the generated DRBD resource file, stamped into its
/// header at setup time. Bump whenever the generated .res content changes
/// semantically, so an upgraded binary can tell a stale resource file from a
/// current one: safety fixes often land in the generated artifacts, and a
/// binary upgrade never re-runs setup. An unstamped file (written before the
/// stamp existed) reads as version 0. The handler scripts and logrotate config
/// are not stamped: they carry no per-deployment state, so the daemon
/// byte-reconciles them against the embedded copies at every start instead
/// (`reconcile_static_artifacts`).
// The resync tuning keeps c-fill-target and c-max-rate (they lift fast links
// past DRBD's ~95 MiB/s default cap) and raises c-min-rate, the floor that
// protects resync from application I/O, whose 250k default lets the resync
// collapse under load. A fixed resync-rate without those knobs re-imposes the
// cap and still collapses under application I/O. Existing pools pick up
// template changes via `regenerate-config` (a live disk-options adjust).
pub(crate) const CONFIG_VERSION: u32 = 6;
const DRBD_CONF_DIR: &str = "/etc/drbd.d";

/// Single source of truth for the arbitrated-mode caveat, shown by both the
/// interactive wizard and the non-interactive CLI path so the operator makes
/// this choice with the known scope in front of them.
pub const ARBITRATED_MODE_NOTE: &str = "\
Note: arbitrated (direct-link) mode replicates over the back-to-back cable,
with the switched network as a fallback path, so a switch failure does not
split DRBD. If the whole switch dies, each host reads over the cable that its
peer lost the gateway too; after 15 s both disarm local XHA fencing and keep
their VMs running (unreachable by clients until the switch returns), then
re-arm HA. A router outage seen by both hosts holds the same way, HA disarmed
until the gateway returns. A host that loses only its own switch link is
stood down after a confirmation window, so its VMs restart on the other host;
a host that loses only its gateway while the peer still answers over the
gateway (management) network is not stood down. That check applies only when
the peer address is on that network. The direct link protects the storage
path, not the guest network. How to choose the mode:
https://docs.vates.tech/tech-preview/twinstor/installation/";

pub struct SetupParams {
    pub peer_ip: IpAddr,
    pub backing_device: Option<PathBuf>,
    pub local_ip: Option<IpAddr>,
    pub drbd_port: Option<u16>,
    pub full_sync: bool,
    pub chap_password: Option<String>,
    pub ping_targets: Vec<IpAddr>,
    /// Peer's IP on a back-to-back direct link (arbitrated mode). When set,
    /// DRBD is configured with two replication paths in active-backup mode:
    /// the direct link is the default active path (declared first, reconnects
    /// steered onto it) and the switched network is the fallback a
    /// cable or NIC failure fails over to (not load-balance, which resets the
    /// connection on any path change). None means single-path fate-shared
    /// mode.
    pub direct_ip: Option<IpAddr>,
    /// Accept a deployment with no partition tiebreaker (no default gateway
    /// and no ping targets). The fence-peer handler then has nothing to
    /// probe, so any replication interruption, including a plain peer host
    /// death, freezes this node's storage until the peer returns. Only for
    /// sites that knowingly accept freeze-on-disconnect semantics.
    pub allow_no_tiebreaker: bool,
}

/// Set once `run_setup` starts changing this host (the config write), so a
/// failure after it is reported as a part-way setup with the way to resume.
static HOST_CHANGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the setup run in this process changed this host before failing.
pub(crate) fn host_changed() -> bool {
    HOST_CHANGED.load(std::sync::atomic::Ordering::SeqCst)
}

/// The error `twinstor setup` reports: a failure after setup started changing
/// this host gets a `next:` line that says how to resume or undo it, unless
/// it already carries its own guidance.
pub(crate) fn part_way_failure(err: anyhow::Error, host_changed: bool) -> anyhow::Error {
    if !host_changed || err.downcast_ref::<crate::render::CliError>().is_some() {
        return err;
    }
    crate::render::CliError::new(
        "setup stopped part-way on this host",
        "fix the problem above, then run `twinstor setup` again (it offers to run setup again \
         from the start), or `twinstor uninstall` to undo what it did",
    )
    .caused_by(err)
    .into()
}

pub async fn run_setup(params: SetupParams) -> Result<()> {
    let local_name = hostname::get()
        .context("hostname")?
        .to_string_lossy()
        .to_string();
    let peer_name = detect_peer_hostname(params.peer_ip).await?;
    // Refuse, before anything is written, the two name states that break
    // storage at the next reboot, and the tiebreaker state that turns a
    // 1-node failure into a full-pool outage. All three are cheap to fix
    // now and expensive to diagnose later.
    validate_node_names(&local_name, &peer_name)?;
    if let Some(xapi_name) = local_xapi_hostname().await {
        validate_local_name_matches_xapi(&local_name, &xapi_name)?;
    }
    tiebreaker_gate(
        !params.ping_targets.is_empty(),
        detect_default_gateway().await.is_some(),
        params.allow_no_tiebreaker,
    )?;
    // The first node's handoff, present when this is the second node. Read
    // before anything is written: a wrong CHAP password must stop here, not
    // at the SR creation after this host's disk is wiped.
    let inherited = inherited_shared_setup().await;
    if let Some(inh) = &inherited {
        second_node_chap_gate(
            params.chap_password.as_deref(),
            inh.chap_check.as_deref(),
            &peer_name,
        )?;
    }
    // An accidental re-run on a working pair must stop before any file is
    // rewritten or DRBD taken down, so the live-SR guard runs first.
    let pair = live_pair_state().await;
    if let Some(refusal) = rerun_refusal(
        rerun_guard(
            pair.sr_exists,
            params.full_sync,
            pair.local_uptodate,
            pair.peer_uptodate,
        ),
        &peer_name,
    ) {
        return Err(refusal.into());
    }
    let local_ip = match params.local_ip {
        Some(ip) => ip,
        None => detect_local_ip(params.peer_ip).await?,
    };
    // Arbitrated mode: a back-to-back direct link gives DRBD a second
    // replication path that does not share fate with the switch. Derive this
    // node's address on that link from the route to the peer's direct IP, and
    // reject it if it is not a distinct path (same source as the switched
    // path means the direct interface is not configured).
    let direct = match params.direct_ip {
        Some(peer_direct) => {
            let local_direct = detect_local_ip(peer_direct).await.with_context(|| {
                format!(
                    "no route to the peer direct-link IP {peer_direct}: configure this node's \
                 direct-link interface with a static IP first (e.g. `xe pif-reconfigure-ip`)"
                )
            })?;
            if local_direct == local_ip {
                bail!(
                    "--direct-ip {peer_direct} resolves to the same local source as the \
                     switched path ({local_ip}); the direct link is not a separate path. \
                     Configure a dedicated interface/subnet for the direct link."
                );
            }
            // The direct link is the default replication path and carries
            // every synchronous write, so a bad cable missed now becomes an
            // outage later. Validate it before trusting it.
            validate_direct_link(peer_direct).await?;
            Some((local_direct, peer_direct))
        }
        None => None,
    };
    let backing = match params.backing_device {
        Some(dev) => {
            if !dev.exists() {
                bail!("{} does not exist", dev.display());
            }
            dev
        }
        None => bail!(
            "no backing device specified. Pass --backing-device <path> (a free disk\n\
             or partition), or run `twinstor setup` with no arguments to pick one."
        ),
    };
    // Persist only a stable device path. Kernel names (/dev/nvme0n1, /dev/sdX)
    // can change across boots; written into the DRBD config they can point
    // DRBD at a different disk, including the system disk.
    let backing = stable_device_path(&backing).await?;

    let port = params.drbd_port.unwrap_or(7789);
    let res = "twinstor-data";
    let drbd_dev = "/dev/drbd0";
    let iqn = format!("iqn.2025-01.com.xcp-ng:{res}");

    let chap_user = "twinstor".to_string();
    // Rebuilding this host of a live pair keeps the password the SR was
    // created with: a new one would lock this host out of its own SR.
    let chap_password = match params.chap_password {
        Some(p) => p,
        None => match pair.sr_exists.then(existing_chap_password).flatten() {
            Some(p) => p,
            None => generate_chap_password()?,
        },
    };
    let initiator_iqns = get_initiator_iqns().await?;

    // The command we tell the operator to run on the peer. It carries the
    // flags that must match across the two nodes (CHAP password; a mismatched
    // --full-sync makes the second node take the wrong sync path). It does not
    // carry --backing-device: that is per-host (this node's by-id path embeds
    // this node's serial and would be wrong on the peer), so the peer chooses
    // its own disk.
    let peer_cmd = {
        let mut cmd =
            format!("twinstor setup --peer-ip {local_ip} --chap-password {chap_password}");
        if params.full_sync {
            cmd.push_str(" --full-sync");
        }
        for t in &params.ping_targets {
            cmd.push_str(&format!(" --ping-target {t}"));
        }
        // The peer's direct-link peer is this node's direct-link address.
        if let Some((local_direct, _)) = direct {
            cmd.push_str(&format!(" --direct-ip {local_direct}"));
        }
        // The no-tiebreaker decision applies to both nodes: without this the
        // printed command would fail on the peer over a choice this node
        // already made explicitly.
        if params.allow_no_tiebreaker {
            cmd.push_str(" --allow-no-tiebreaker");
        }
        cmd
    };

    println!("TwinStor setup");
    println!("  local:   {local_name} ({local_ip})");
    println!("  peer:    {peer_name} ({})", params.peer_ip);
    println!("  device:  {}", backing.display());
    println!("  iSCSI:   {iqn}");
    println!("  CHAP user:     {chap_user}");
    println!("  CHAP password: {chap_password}");
    println!();

    tokio::fs::create_dir_all(CONFIG_DIR)
        .await
        .context("creating config dir")?;
    tokio::fs::create_dir_all(DRBD_CONF_DIR)
        .await
        .context("creating drbd config dir")?;

    let iqn_list = initiator_iqns
        .iter()
        .map(|i| format!("\"{}\"", i))
        .collect::<Vec<_>>()
        .join(", ");

    let config_text = format!(
        r#"[node]
name = "{local_name}"
peer = "{peer_name}"

[network]
peer_address = "{peer_ip}"
drbd_port = {port}
allow_no_tiebreaker = {allow_no_tiebreaker}

[storage]
drbd_resource = "data"
backing_device = "{backing}"
drbd_device = "{drbd_dev}"

[iscsi]
target_iqn = "{iqn}"
lun_serial = "{res}"
chap_user = "{chap_user}"
chap_password = "{chap_password}"
initiator_iqns = [{iqn_list}]
"#,
        peer_ip = params.peer_ip,
        backing = backing.display(),
        allow_no_tiebreaker = params.allow_no_tiebreaker
    );
    // From here on a failure leaves this host part-way set up.
    HOST_CHANGED.store(true, std::sync::atomic::Ordering::SeqCst);
    // The config contains the CHAP password: created root-only, never readable
    // by others even briefly (an older config keeps its mode, so set it after).
    crate::cli::write_atomic(std::path::Path::new(CONFIG_PATH), config_text.as_bytes())
        .await
        .context("writing config")?;
    tokio::fs::set_permissions(CONFIG_PATH, std::fs::Permissions::from_mode(0o600))
        .await
        .context("setting config permissions")?;
    println!("wrote {CONFIG_PATH}");

    // A recorded SR uuid left by an earlier install names an SR that is gone.
    // The node that creates the SR records the new one below; the other
    // node's daemon records it on its first run.
    let _ = std::fs::remove_file(crate::xapi::sr_uuid_path());

    write_ping_targets(&params.ping_targets).await?;

    // Arbitrated mode: record the peer's direct-link IP so the fence-peer
    // handler can tell a switch loss (peer still alive on the cable) from real
    // isolation. The survive/yield decision stays gateway-based.
    match direct {
        Some((_, peer_direct)) => {
            tokio::fs::write(
                DIRECT_PEER_PATH,
                format!("# peer direct-link IP (arbitrated mode)\n{peer_direct}\n"),
            )
            .await
            .context("writing direct-peer")?;
        }
        None => {
            let _ = tokio::fs::remove_file(DIRECT_PEER_PATH).await;
        }
    }

    // Backing-disk volatile write cache: must be off for write-completion
    // durability (the dom0 stack does not propagate guest flushes; see
    // write_cache.rs). The daemon re-asserts this after every boot; doing
    // it here too means the disk is durable before the initial sync starts.
    for (disk, outcome) in crate::write_cache::enforce(&backing).await {
        use crate::write_cache::EnforceOutcome::*;
        match outcome {
            NoVolatileCache => {
                println!("  cache:   {disk}: no volatile write cache (durable on completion)")
            }
            AlreadyDisabled => println!("  cache:   {disk}: volatile write cache already disabled"),
            Disabled => println!("  cache:   {disk}: volatile write cache disabled"),
            ControllerDelegated(driver) => println!(
                "  cache:   {disk}: hardware RAID volume ({driver}); the controller owns the\n\
                 \x20          cache: keep it battery- or flash-backed and the disks' own\n\
                 \x20          cache off (the controller's settings)"
            ),
            ZvolDelegated => println!(
                "  cache:   {disk}: ZFS zvol; flush durability is ZFS's job; keep the\n\
                 \x20          dataset's sync property at 'standard' (never 'disabled')"
            ),
            Failed(e) => println!(
                "  cache:   WARNING: {disk}: could not disable the volatile write cache ({e}).\n\
                 \x20          fsync'd guest data can be lost if this host is fenced while\n\
                 \x20          its peer is disconnected. Use a drive with power-loss\n\
                 \x20          protection, or disable the disk write cache manually."
            ),
        }
    }

    // Resync controller tuning. The DRBD defaults keep so little resync data
    // in flight that resync caps near 95 MiB/s regardless of link speed; a
    // larger fill target unlocks fast links. On slow links a large fill
    // target makes application writes queue behind resync data (up to
    // fill/bandwidth of added latency), so it is scaled to the link. The
    // controller's c-min-rate yield to application I/O applies either way.
    let link_mbps = detect_link_speed_mbps(params.peer_ip).await;
    let fill_target = resync_fill_target(link_mbps);
    let min_rate = resync_min_rate(link_mbps, direct.is_some());
    let sync_mode = if direct.is_some() {
        "arbitrated"
    } else {
        "fate-shared"
    };
    match link_mbps {
        Some(m) => println!(
            "  link:    {m} Mbps ({sync_mode}) -> resync c-fill-target {fill_target}, c-min-rate {min_rate}"
        ),
        None => println!(
            "  link:    speed unknown ({sync_mode}) -> resync c-fill-target {fill_target}, c-min-rate {min_rate}"
        ),
    }

    // Topology: a single switched path (fate-shared) or two active-backup
    // paths (arbitrated mode, --direct-ip). See drbd_topology().
    if let Some((local_direct, _)) = direct {
        println!(
            "  direct:  arbitrated mode: replication over {local_ip} (switch) + {local_direct} (direct link)"
        );
        // Show the scope note (storage-path redundancy only) before writing
        // the config, so the operator knows what arbitrated mode does and does
        // not cover.
        println!("\n{ARBITRATED_MODE_NOTE}\n");
    }
    // New pool: write explicit sorted-hostname node-ids for both topologies so a
    // fate-shared pool never leaves DRBD to auto-assign ids, which may not follow
    // hostname order. create-md then writes these into the metadata.
    let topology = drbd_topology(
        &local_name,
        &peer_name,
        drbd_dev,
        &backing.display().to_string(),
        local_ip,
        params.peer_ip,
        port,
        direct,
        Some(sorted_node_ids(&local_name, &peer_name)),
    );

    // Arbitrated mode (two paths) uses `resource-and-stonith` so DRBD freezes
    // I/O the instant the peer is lost, before the fence-peer handler runs. That
    // closes the brief both-Primary window where each node would otherwise keep
    // writing (the XHA statefile, for example) and diverge into a split-brain on
    // a switch failure. Fate-shared mode keeps `resource-only`: there, losing the peer
    // always means losing the gateway too, so both nodes self-fence cleanly and
    // there is nothing to freeze against (and resource-and-stonith would only
    // add an I/O stall to an ordinary single-node failover).
    let fencing_policy = if direct.is_some() {
        "resource-and-stonith"
    } else {
        "resource-only"
    };

    tokio::fs::write(
        format!("{DRBD_CONF_DIR}/{res}.res"),
        render_res_file(res, fencing_policy, fill_target, &min_rate, &topology),
    )
    .await
    .context("writing DRBD config")?;
    println!("wrote {DRBD_CONF_DIR}/{res}.res");

    // Install the static artifacts: fence-peer handler (deterministic
    // survive or self-fence on partition), split-brain handler (marks real
    // two-primary divergence for the daemon to resolve), and the logrotate
    // config. The daemon reconciles these on every start, so a binary upgrade
    // also refreshes them.
    for (path, content) in static_artifacts() {
        tokio::fs::write(path, content)
            .await
            .with_context(|| format!("writing {path}"))?;
        if path.ends_with(".sh") {
            let _ = tokio::process::Command::new("chmod")
                .args(["+x", path])
                .output()
                .await;
        }
        println!("wrote {path}");
    }

    install_lvm_filter(&backing).await?;

    println!("opening firewall for DRBD and iSCSI...");
    open_firewall_ports(port).await;

    // Re-runnable after an aborted attempt: a previous run may have left the
    // service running and the resource up, and wipefs/create-md need the
    // device closed. The service goes first: it manages the resource and
    // would bring it back up behind setup's back.
    let service_active = cmd_output("systemctl", &["is-active", "twinstor"])
        .await
        .is_ok_and(|s| s.trim() == "active");
    if service_active {
        println!("stopping the twinstor service left by the previous run...");
        run_cmd("systemctl", &["stop", "twinstor"]).await?;
    }
    let res_up = tokio::process::Command::new("drbdadm")
        .args(["status", res])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);
    if res_up {
        println!("previous DRBD resource is up, taking it down first...");
        if let Err(e) = run_cmd("drbdadm", &["down", res]).await {
            return Err(crate::render::CliError::new(
                "could not take this host's DRBD resource down: something still uses it",
                "if VMs on this host use the TwinStor SR, move them to the other host or \
                 shut them down, then run `twinstor setup` again; or run `twinstor uninstall` \
                 to remove what setup did",
            )
            .caused_by(e)
            .into());
        }
    }

    run_cmd("wipefs", &["-a", &backing.display().to_string()]).await?;

    println!("creating DRBD metadata...");
    run_cmd("drbdadm", &["create-md", "--force", res]).await?;

    println!("bringing DRBD up...");
    run_cmd("drbdadm", &["up", res]).await?;

    // Probe the peer's DRBD port to learn whether the other node is already up
    // (second node or rejoin) or not yet (first node). When it is not up the
    // probe runs its full timeout, so print the bound to explain the pause
    // before the "action required" notice.
    println!("checking whether the other node ({peer_name}) is already set up");
    println!("(please wait, up to 15s)...");
    let peer_exists = wait_for_peer(params.peer_ip, port, 15).await;
    // Whether the peer joined while this (first) node was waiting for it.
    // peer_exists is the snapshot from the probe above and stays false for the
    // first node; the final guidance must reflect the state after the wait, or
    // it tells the user to set up a peer that has already connected.
    let mut peer_connected = false;

    // First node: hand the choices common to both nodes to the second node via the
    // pool DB, so its wizard inherits them instead of re-asking (the per-host
    // backing device is the only thing it then needs to choose). Best-effort.
    if !peer_exists {
        publish_shared_setup(&SharedSetup {
            arbitrated: direct.is_some(),
            peer_ip: local_ip,
            direct_ip: direct.map(|(local_direct, _)| local_direct),
            ping_targets: params.ping_targets.clone(),
            full_sync: params.full_sync,
            drbd_port: port,
            chap_user: chap_user.clone(),
            publisher_uuid: local_host_uuid().await.unwrap_or_default(),
            chap_check: Some(chap_check_value(&chap_password)),
        })
        .await;
    }

    if peer_exists && params.full_sync {
        // Second node with --full-sync: sync from the peer
        println!("peer found, syncing from peer (full sync requested)...");
        run_cmd("drbdadm", &["disconnect", res]).await.ok();
        run_cmd("drbdadm", &["connect", "--discard-my-data", res]).await?;

        println!("waiting for sync to start...");
        for _ in 0..30 {
            if let Ok(out) = tokio::process::Command::new("drbdadm")
                .args(["status", res])
                .output()
                .await
            {
                let s = String::from_utf8_lossy(&out.stdout);
                if s.contains("connection:Connected") || s.contains("SyncTarget") {
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        println!("promoting (dual-primary)...");
        run_cmd("drbdadm", &["primary", "--force", res]).await?;
    } else if peer_exists {
        // Second node: peer is up.
        println!("peer found, waiting for DRBD connection...");
        wait_for_drbd_connected(res, 30).await;

        // SyncTarget means the peer holds real data and is syncing it to us:
        // either an initial --full-sync, or a rejoin to an SR the peer already
        // serves. Skipping the sync (clear-bitmap) would then be wrong (it
        // would mark this still-empty node UpToDate), and force-promoting now
        // would expose Inconsistent data through the iSCSI target. So follow the
        // resync and do not promote until UpToDate; the daemon promotes this
        // node to dual-Primary once the resync completes. A fresh second node
        // (both devices empty) is not SyncTarget: skip-sync and promote now.
        let syncing = tokio::process::Command::new("drbdadm")
            .args(["status", res])
            .output()
            .await
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("SyncTarget"))
            .unwrap_or(false);
        if syncing {
            println!("peer is syncing real data to this node; following the resync.");
            println!("this node stays Secondary until UpToDate; the daemon then promotes");
            println!("it to dual-Primary (it will not serve Inconsistent data).");
        } else {
            establish_skip_sync(res).await?;
            println!("promoting (dual-primary)...");
            run_cmd("drbdadm", &["primary", "--force", res]).await?;
        }
    } else if params.full_sync {
        // First node with --full-sync: promote immediately, sync starts when peer joins
        println!("no peer found yet, promoting (full sync will start when peer joins)...");
        run_cmd("drbdadm", &["primary", "--force", res]).await?;
    } else {
        // First node: do not promote yet; wait for the peer to join so the
        // sync can be skipped. Lead with the interactive `twinstor setup`: it
        // auto-detects this node and picks the peer's own backing disk, which
        // the non-interactive one-liner cannot do (the disk path is per-host and
        // unknown from here). The only thing the operator must carry across is
        // the CHAP password, so it gets its own line, ready to copy.
        println!("\n======================================================================");
        println!("  Action required: now set up the other node ({peer_name}).");
        println!("======================================================================");
        println!("  This host is configured and is now waiting for {peer_name} to connect.");
        println!(
            "  Keep this window open: this host waits up to {} minutes for {peer_name}.",
            PEER_WAIT_SECS / 60
        );
        println!("  If {peer_name} is set up later, it copies the whole disk from this host");
        println!("  (hours for large disks). Do not press Ctrl-C here. If this window closes");
        println!("  anyway, run `twinstor setup` here again: it offers to start over.");
        println!();
        println!("  On {peer_name}, simply run:");
        println!();
        println!("      twinstor setup");
        println!();
        println!("  The wizard detects this node automatically, picks {peer_name}'s own");
        println!("  backing disk, then asks for the CHAP password. Paste this one in:");
        println!();
        println!("      CHAP password:  {chap_password}");
        println!();
        println!("  {peer_name} then creates the shared SR and finishes the cluster.");
        println!();
        println!("  (Non-interactive alternative, substituting {peer_name}'s own disk:");
        println!("      {peer_cmd} --backing-device <disk-on-peer> )");
        println!("  (use --full-sync to promote this node immediately and sync later instead)");
        println!("======================================================================\n");

        if wait_for_drbd_connected(res, PEER_WAIT_SECS).await {
            println!("peer connected.");
            peer_connected = true;
            establish_skip_sync(res).await?;

            println!("promoting (dual-primary)...");
            run_cmd("drbdadm", &["primary", "--force", res]).await?;
        } else {
            let size = cmd_output("blockdev", &["--getsize64", &backing.display().to_string()])
                .await
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok());
            println!(
                "{}",
                peer_wait_timeout_text(&peer_name, PEER_WAIT_SECS, size, link_mbps)
            );
            run_cmd("drbdadm", &["primary", "--force", res]).await?;
        }
    }

    if Path::new("/usr/lib/systemd/system/twinstor.service").exists() {
        run_cmd("systemctl", &["daemon-reload"]).await?;
        run_cmd("systemctl", &["enable", "--now", "twinstor"]).await?;
        println!("twinstor service started");
    }

    if peer_exists {
        // We're the second node. Both iSCSI targets are up.
        // Create the SR, plug PBDs, enable multipath and XHA.
        println!("\nboth nodes ready, configuring shared storage...\n");
        create_sr(
            local_ip,
            params.peer_ip,
            &peer_name,
            &iqn,
            &chap_user,
            &chap_password,
            direct.is_some(),
        )
        .await?;
        // Cluster is formed: the first node's shared-setup handoff has served
        // its purpose, so drop it from the pool DB rather than leave it behind.
        clear_shared_setup().await;
    } else if peer_connected {
        // First node, and the peer joined during the wait above: it is the
        // second node and is now creating the SR. Repeating the "set up the
        // peer with this CHAP password" guidance here would be wrong: the peer
        // is already configured and connected (that is why the wait ended).
        println!("\nsetup complete on {local_name}; peer connected.\n");
        println!("the peer node ({peer_name}) creates the SR and finishes the cluster.");
    } else {
        // First node, peer not up yet (the wait timed out, or --full-sync
        // promoted without waiting): the peer still needs setup, so
        // the CHAP password and next-step guidance are what the user needs.
        println!("\nsetup complete on {local_name}.\n");
        println!("CHAP password (you need it on the peer): {chap_password}\n");
        println!("on the peer node ({peer_name}), run setup with that CHAP password and");
        println!("the peer's own backing device:");
        println!("  {peer_cmd} --backing-device <disk-on-peer>");
        println!("  (or just run `twinstor setup` on the peer to pick its disk interactively)");
        println!("\nthe second node creates the SR and finishes the cluster.");
    }

    Ok(())
}

/// The second node must carry the first node's CHAP password: refuse a
/// missing one, and one the first node's published check value disproves.
fn second_node_chap_gate(given: Option<&str>, check: Option<&str>, peer: &str) -> Result<()> {
    let next = format!(
        "pass the password {peer} printed with --chap-password <password> (it is also in \
         /etc/twinstor/twinstor.toml on {peer}, key chap_password), or run `twinstor setup` \
         with no arguments for the wizard"
    );
    let why = format!(
        "with another password the iSCSI login to {peer} fails at the very end, after this \
         host's disk is wiped; nothing was changed on this host"
    );
    match given {
        None => Err(crate::render::CliError::new(
            format!("{peer} is already set up and waiting, and this host needs its CHAP password"),
            next,
        )
        .explain(why)
        .into()),
        Some(p) if chap_verdict(p, check) == ChapVerdict::Differs => {
            Err(crate::render::CliError::new(
                format!("this is not the CHAP password {peer} printed"),
                next,
            )
            .explain(why)
            .into())
        }
        Some(_) => Ok(()),
    }
}

/// The SR-creation failure when the first node's iSCSI target never accepts
/// this host's login: almost always a CHAP password that differs from the
/// first node's, or a first node whose service is not running.
fn iscsi_login_refused(peer_name: &str, peer_ip: IpAddr) -> crate::render::CliError {
    crate::render::CliError::new(
        format!("{peer_name} ({peer_ip}) refused the iSCSI login after {PROBE_ATTEMPTS} attempts"),
        format!(
            "check the CHAP password on both hosts (chap_password in \
             /etc/twinstor/twinstor.toml) and that `systemctl status twinstor` shows it active \
             on {peer_name}; then run `twinstor setup` here again (it offers to run setup again)"
        ),
    )
    .explain(format!(
        "most likely the CHAP password given here differs from the one {peer_name} printed, \
         or the twinstor service is not running on {peer_name}"
    ))
}

/// Hostnames matter: drbdadm matches `on <name>` sections against `uname -n`,
/// node-ids are assigned by sorted hostname so both nodes agree without
/// talking, and the dual-master keeper tiebreak sorts by name. Two hosts with
/// the same name (the default when operators skip naming) collide on all
/// three, and the failure shows up at the next reboot as missing storage, far
/// from the cause. Refuse at setup instead.
fn validate_node_names(local: &str, peer: &str) -> Result<()> {
    if local == peer {
        bail!(
            "both hosts are named '{local}'. TwinStor keys the DRBD config, the \
             node-id assignment and its recovery tiebreaks on unique hostnames; \
             duplicate names break storage at the next reboot. Rename one host first:\n\
             \x20 xe host-set-hostname-live host-uuid=<uuid> host-name=<new-name>\n\
             then make /etc/hostname match and re-run setup."
        );
    }
    Ok(())
}

/// The peer resolves this host's name through XAPI (`detect_peer_hostname`),
/// while the local DRBD config uses `uname -n`. If the two disagree (possible
/// after a manual rename that updated only one of them), each node writes a
/// .res that names this host differently, and in arbitrated mode the pinned
/// per-path host names then prevent the connection from ever establishing.
fn validate_local_name_matches_xapi(uname: &str, xapi: &str) -> Result<()> {
    if uname != xapi {
        bail!(
            "this host's kernel hostname ('{uname}') and its XAPI hostname record \
             ('{xapi}') disagree. The peer resolves this host via XAPI, so the two \
             nodes would generate DRBD configs that disagree about this node's name. \
             Make them match first:\n\
             \x20 xe host-set-hostname-live host-uuid=<uuid> host-name={uname}\n\
             (or set the kernel hostname to '{xapi}'), then re-run setup."
        );
    }
    Ok(())
}

/// This host's hostname as XAPI records it, or None when XAPI cannot answer
/// (the duplicate-name check above still ran; this cross-check is best-effort).
async fn local_xapi_hostname() -> Option<String> {
    let uuid = local_host_uuid().await?;
    cmd_output(
        "xe",
        &[
            "host-param-get",
            &format!("uuid={uuid}"),
            "param-name=hostname",
        ],
    )
    .await
    .ok()
    .filter(|n| !n.is_empty())
}

/// What setup sees of an existing installation, read before it changes
/// anything.
pub(crate) struct PairState {
    /// The pool has a TwinStor SR: the pair holds real data.
    pub sr_exists: bool,
    /// This host's DRBD disk is UpToDate (the resource is up here).
    pub local_uptodate: bool,
    /// The peer's disk is UpToDate as this host sees it right now.
    pub peer_uptodate: bool,
}

pub(crate) async fn twinstor_sr_exists() -> bool {
    cmd_output(
        "xe",
        &["sr-list", "name-label=twinstor", "params=uuid", "--minimal"],
    )
    .await
    .ok()
    .is_some_and(|u| !u.trim().is_empty())
}

pub(crate) async fn live_pair_state() -> PairState {
    let status = tokio::process::Command::new("drbdadm")
        .args(["status", "twinstor-data"])
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| crate::drbd::parse_drbdadm_text(&String::from_utf8_lossy(&o.stdout)).ok());
    use crate::drbd::DiskState::UpToDate;
    PairState {
        sr_exists: twinstor_sr_exists().await,
        local_uptodate: status.as_ref().is_some_and(|s| s.disk == UpToDate),
        peer_uptodate: status
            .as_ref()
            .is_some_and(|s| s.peer_disk == Some(UpToDate)),
    }
}

/// The CHAP password of the current configuration, if any.
fn existing_chap_password() -> Option<String> {
    crate::config::Config::load(Path::new(CONFIG_PATH))
        .ok()
        .and_then(|c| c.iscsi.chap_password)
}

/// Whether a setup run may go on, given what already exists.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RerunGuard {
    /// No TwinStor SR yet (a first install, or one that stopped part-way),
    /// or a deliberate rebuild of this host from a peer that holds the data.
    Proceed,
    /// A live SR and no rebuild asked: an accidental re-run.
    LiveSr,
    /// A rebuild asked while this host holds the only current copy: wiping
    /// it would lose the data.
    OnlyCurrentCopy,
}

pub(crate) fn rerun_guard(
    sr_exists: bool,
    full_sync: bool,
    local_uptodate: bool,
    peer_uptodate: bool,
) -> RerunGuard {
    if backing_wipe_blocked(sr_exists, full_sync) {
        RerunGuard::LiveSr
    } else if sr_exists && local_uptodate && !peer_uptodate {
        RerunGuard::OnlyCurrentCopy
    } else {
        RerunGuard::Proceed
    }
}

/// The refusal for a guard that stops setup, with the way out.
pub(crate) fn rerun_refusal(guard: RerunGuard, peer: &str) -> Option<crate::render::CliError> {
    match guard {
        RerunGuard::Proceed => None,
        RerunGuard::LiveSr => Some(
            crate::render::CliError::new(
                "this pool already has a TwinStor SR, so this host's copy is live; nothing was \
                 changed",
                format!(
                    "if the pair works, there is nothing to do (check it with `twinstor status`). \
                     To rebuild only this host's copy from {peer} (it keeps serving), run \
                     `twinstor setup` again and accept the rebuild, or pass --full-sync"
                ),
            )
            .explain(
                "running setup again erases this host's TwinStor disk and copies it back from \
                 the other host",
            ),
        ),
        RerunGuard::OnlyCurrentCopy => Some(
            crate::render::CliError::new(
                format!(
                    "this host holds the only current copy of the data ({peer}'s copy is not \
                     UpToDate); nothing was changed"
                ),
                format!(
                    "wait until `twinstor status` shows both copies UpToDate, then rebuild, or \
                     rebuild {peer} instead (run `twinstor setup` there)"
                ),
            )
            .explain("rebuilding this host would erase the only good copy"),
        ),
    }
}

/// Whether to block the destructive backing-device wipe on a setup re-run.
/// The wipe is safe on a fresh install or after an aborted run (no twinstor SR
/// exists yet) and is intended on a rejoin or re-init (signalled with
/// `--full-sync`, which then resyncs a fresh copy from the peer). It loses data
/// only when a live twinstor SR already exists and no such intent was given.
fn backing_wipe_blocked(twinstor_sr_exists: bool, full_sync: bool) -> bool {
    twinstor_sr_exists && !full_sync
}

/// Refuse a deployment whose fence-peer would have nothing to probe. The
/// handler collects the default gateway at fence time plus the ping-targets
/// file; with an empty set every invocation falls through to suspend-io and
/// yield, so a plain peer host death freezes the healthy survivor too and a
/// one-node failure becomes a full-pool outage, worse than no HA at all.
/// Isolated industrial and air-gapped segments often have no default gateway
/// on the storage network.
fn tiebreaker_gate(
    have_targets: bool,
    have_gateway: bool,
    allow_no_tiebreaker: bool,
) -> Result<()> {
    if have_targets || have_gateway {
        return Ok(());
    }
    if allow_no_tiebreaker {
        println!(
            "  Override (--allow-no-tiebreaker): with no gateway and no ping targets, any\n\
             \x20 replication interruption, including the peer simply dying, freezes this\n\
             \x20 node's storage until the peer returns. HA will not restart VMs anywhere."
        );
        return Ok(());
    }
    bail!(
        "no partition tiebreaker: no default gateway detected and no --ping-target \
         given. The fence-peer handler would have nothing to probe and would freeze \
         this node on any replication interruption, including a plain peer host \
         death: a 1-node failure would take down the whole pool. Add at least one \
         always-on local IP with --ping-target <ip> (a managed switch, a router, any \
         device that proves this node can still serve clients). If this site \
         deliberately accepts freeze-on-disconnect semantics, re-run with \
         --allow-no-tiebreaker."
    );
}

/// Write /etc/twinstor/ping-targets for the fence-peer handler.
///
/// The default gateway is always probed by the handler, so this file only
/// holds *additional* targets. The list is any-of-N: a node survives if it
/// can reach any target. Extra local always-on IPs (a managed switch, a
/// second router) stop one flaky device from fencing both nodes at once.
async fn write_ping_targets(targets: &[IpAddr]) -> Result<()> {
    let gateway = detect_default_gateway().await;

    let mut body = String::from(
        "# TWINSTOR fence-peer ping targets (any-of-N partition tiebreaker).\n\
         # The default gateway is always probed automatically; the entries below\n\
         # are additional local always-on IPs (e.g. your managed switch's\n\
         # management address, a second router). One IP per line; '#' comments.\n\
         #\n\
         # Probing more local targets makes a node less likely to self-fence on a\n\
         # single flaky device. Prefer local targets that prove you can still\n\
         # serve clients, not an internet host (which is only reachable via the\n\
         # gateway anyway).\n",
    );

    if targets.is_empty() {
        body.push_str("#\n# Example:\n#   192.0.2.2\n");
    } else {
        body.push('\n');
        for t in targets {
            body.push_str(&t.to_string());
            body.push('\n');
        }
    }

    tokio::fs::write(PING_TARGETS_PATH, body)
        .await
        .context("writing ping-targets")?;
    println!("wrote {PING_TARGETS_PATH}");

    if targets.is_empty() {
        match &gateway {
            Some(gw) => {
                println!(
                    "  note: no extra ping targets set. Only the default gateway ({gw}) will be\n\
                     \x20       used as the partition tiebreaker. Recommended: add a local always-on\n\
                     \x20       IP (e.g. your switch) with --ping-target, or edit {PING_TARGETS_PATH}."
                );
                // The gateway is the only tiebreaker here: report an
                // unreachable one now, not at the first partition.
                let ok = tokio::process::Command::new("ping")
                    .args(["-c", "1", "-W", "2", gw])
                    .output()
                    .await
                    .map(|o| o.status.success())
                    .unwrap_or(false);
                if !ok {
                    println!(
                        "  WARNING: the default gateway ({gw}) does not answer ping right now. \
                         As the only tiebreaker, an unreachable gateway means the fence-peer \
                         handler freezes this node on any replication interruption."
                    );
                }
            }
            None => println!(
                "  warning: no extra ping targets set and no default gateway detected \
                 (--allow-no-tiebreaker accepted). The fence-peer handler has nothing to \
                 probe and will freeze this node on any DRBD disconnect."
            ),
        }
    } else {
        // Check the targets now so a typo shows up at setup, not at the first
        // partition.
        for t in targets {
            let ok = tokio::process::Command::new("ping")
                .args(["-c", "1", "-W", "2", &t.to_string()])
                .output()
                .await
                .map(|o| o.status.success())
                .unwrap_or(false);
            if ok {
                println!("  ping target {t}: reachable");
            } else {
                println!("  ping target {t}: WARNING unreachable right now (check the address)");
            }
        }
    }

    Ok(())
}

/// `c-fill-target` for the `.res`, picked by link speed. DRBD's default keeps
/// so little resync data in flight that resync caps near 95 MiB/s whatever the
/// link speed; a larger fill target unlocks fast links. On slow links a large
/// fill only makes application writes queue behind resync traffic, so below
/// 10 GbE, and for an unknown speed, the small value is used.
pub(crate) fn resync_fill_target(link_mbps: Option<u32>) -> &'static str {
    match link_mbps {
        Some(m) if m >= 10_000 => "10M",
        _ => "1M",
    }
}

/// `c-min-rate` for the `.res`: the floor below which application I/O does not
/// throttle the resync. DRBD's 250k default lets the resync collapse under load
/// (to a few MB/s on a fast link), leaving the pool single-copy far too long and
/// risking the reboot-drain hold cap. Raised to favour redundancy, scaled by
/// topology: 80% of the link on a dedicated direct link (arbitrated, nothing else
/// uses the wire), 50% on a shared link (fate-shared, leaving headroom for
/// management, XHA heartbeat, iSCSI and VM traffic; a starved heartbeat could
/// cause a fence). Capped at c-max-rate (800M; a floor above the ceiling is
/// invalid) and floored at 10 MB/s. An unknown link is treated as 1 GbE.
pub(crate) fn resync_min_rate(link_mbps: Option<u32>, arbitrated: bool) -> String {
    let mbps = link_mbps.unwrap_or(1000); // unknown: assume 1 GbE
    let link_mbytes = mbps / 8; // Mb/s to MB/s
    let pct = if arbitrated { 80 } else { 50 }; // dedicated vs shared link
    let rate = (link_mbytes.saturating_mul(pct) / 100).clamp(10, 800); // floor 10, cap c-max-rate
    format!("{rate}M")
}

/// Preflight the direct link before making it the default replication path.
/// Reachability is a hard requirement (replication lands there on
/// every connect); MTU and link speed are advisory, matching the other
/// per-mode preflight checks. A degraded but working cable slows every
/// synchronous write and nothing else uses that wire to reveal it, so catch
/// it at setup time.
async fn validate_direct_link(peer_direct: IpAddr) -> Result<()> {
    let peer = peer_direct.to_string();
    // 3 attempts: a single lost packet must not fail a setup.
    let mut reachable = false;
    for _ in 0..3 {
        if tokio::process::Command::new("ping")
            .args(["-c", "1", "-W", "2", &peer])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            reachable = true;
            break;
        }
    }
    if !reachable {
        bail!(
            "the peer direct-link IP {peer} does not answer ping. The direct link is \
             the default replication path, so it must work before setup can trust it: \
             check the cable, the peer's direct interface configuration, and that the \
             peer host is up."
        );
    }
    println!("  direct link {peer}: reachable");

    // MTU blackhole probe: a full-MTU do-not-fragment ping catches a mismatch
    // (jumbo frames on one side only, for example) that small frames pass
    // but replication traffic hits immediately.
    let dev_mtu: Option<(String, u32)> = async {
        let out = cmd_output("ip", &["route", "get", &peer]).await.ok()?;
        let dev = out
            .split_whitespace()
            .skip_while(|w| *w != "dev")
            .nth(1)?
            .to_string();
        let mtu = tokio::fs::read_to_string(format!("/sys/class/net/{dev}/mtu"))
            .await
            .ok()?
            .trim()
            .parse::<u32>()
            .ok()?;
        Some((dev, mtu))
    }
    .await;
    if let Some((dev, mtu)) = dev_mtu {
        // IPv4 header (20) + ICMP header (8).
        let payload = mtu.saturating_sub(28).to_string();
        let ok = tokio::process::Command::new("ping")
            .args(["-c", "1", "-W", "2", "-M", "do", "-s", &payload, &peer])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            println!(
                "  WARNING: a full-MTU ({mtu} on {dev}) do-not-fragment ping to {peer} \
                 fails: MTU mismatch on the direct link. Replication will underperform \
                 or stall on large writes; align the MTU on both ends."
            );
        }
    }

    if let Some(speed) = detect_link_speed_mbps(peer_direct).await {
        if speed < 1000 {
            println!(
                "  WARNING: the direct link negotiated only {speed} Mbps. Every \
                 synchronous write rides this cable; check for a bad cable/NIC or \
                 autonegotiation failure."
            );
        } else {
            println!("  direct link speed: {speed} Mbps");
        }
    }
    Ok(())
}

/// Best-effort link speed (Mbps) of the interface routing to the peer.
/// Handles the XCP-ng case where the route device is an OVS bridge: the
/// speed is read from the bridge's physical member ports.
pub(crate) async fn detect_link_speed_mbps(peer_ip: IpAddr) -> Option<u32> {
    let dev = cmd_output("ip", &["route", "get", &peer_ip.to_string()])
        .await
        .ok()?;
    let dev = dev
        .split_whitespace()
        .skip_while(|w| *w != "dev")
        .nth(1)?
        .to_string();

    async fn sysfs_speed(dev: &str) -> Option<u32> {
        let s = tokio::fs::read_to_string(format!("/sys/class/net/{dev}/speed"))
            .await
            .ok()?;
        s.trim()
            .parse::<i64>()
            .ok()
            .filter(|v| *v > 0)
            .map(|v| v as u32)
    }

    if let Some(s) = sysfs_speed(&dev).await {
        return Some(s);
    }
    // OVS bridge: read the member ports' speed.
    if let Ok(ports) = cmd_output("ovs-vsctl", &["list-ports", &dev]).await {
        for port in ports.lines() {
            if let Some(s) = sysfs_speed(port.trim()).await {
                return Some(s);
            }
        }
    }
    None
}

/// Resolve a block device to a stable `/dev/disk/by-id` alias. Device-mapper
/// targets (LVM LVs) already have stable user-facing names and pass through
/// unchanged. Raw disks without a stable alias are refused: a volatile name in
/// the DRBD config can later point DRBD at the wrong disk.
async fn stable_device_path(dev: &Path) -> Result<PathBuf> {
    let real = tokio::fs::canonicalize(dev)
        .await
        .context("resolving backing device")?;
    let real_name = real
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if real_name.starts_with("dm-") {
        return Ok(dev.to_path_buf());
    }
    // ZFS zvols never get /dev/disk/by-id aliases, but udev's zvol rules
    // provide an equally stable name: /dev/zvol/<pool>/<dataset>, derived
    // from the dataset name (not enumeration order). Accept a given
    // /dev/zvol/* path as-is, and reverse-resolve a raw /dev/zdN to its
    // zvol alias. Community-supported: durability then depends on ZFS
    // (ZIL, sync property) below DRBD.
    if real_name.starts_with("zd") {
        let alias = if dev.starts_with("/dev/zvol/") {
            Some(dev.to_path_buf())
        } else {
            zvol_alias_for(&real, Path::new("/dev/zvol")).await
        };
        match alias {
            Some(p) => {
                println!(
                    "  device:  {} is a ZFS zvol (community-supported). Flush durability\n\
                     \x20          is governed by ZFS below DRBD: keep the dataset's sync\n\
                     \x20          property at 'standard' (never 'disabled').",
                    p.display()
                );
                return Ok(p);
            }
            None => bail!(
                "{} is a ZFS zvol but no /dev/zvol/<pool>/<dataset> alias resolves to it; \
                 refusing to write the volatile zd name into the DRBD config. Check \
                 `udevadm trigger` / the zfs udev rules, then pass the /dev/zvol path.",
                dev.display()
            ),
        }
    }

    let mut best: Option<(u8, PathBuf)> = None;
    let mut entries = tokio::fs::read_dir("/dev/disk/by-id")
        .await
        .context("reading /dev/disk/by-id")?;
    while let Some(e) = entries.next_entry().await? {
        let name = e.file_name().to_string_lossy().to_string();
        let Ok(target) = tokio::fs::canonicalize(e.path()).await else {
            continue;
        };
        if target != real {
            continue;
        }
        // Prefer model+serial names (readable, printable on a disk label)
        // over the eui/wwn forms. For mdraid, prefer the array-UUID alias:
        // md-name-* embeds the array name and creating host, both of which
        // can change on a rename or reassembly, while md-uuid-* comes from the
        // member superblocks and survives both.
        let score = if name.starts_with("nvme-eui.")
            || name.starts_with("wwn-")
            || name.starts_with("md-uuid-")
        {
            2
        } else if name.starts_with("nvme-") || name.starts_with("ata-") || name.starts_with("scsi-")
        {
            3
        } else {
            1
        };
        if best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((score, e.path()));
        }
    }
    match best {
        Some((_, p)) => Ok(p),
        None => bail!(
            "no stable /dev/disk/by-id alias found for {}; refusing to write a volatile \
             device name into the DRBD config (device enumeration can change across boots \
             and point DRBD at a different disk)",
            dev.display()
        ),
    }
}

/// Render the full DRBD resource file from its four variable inputs. Shared
/// between setup (initial write) and `regenerate-config` (upgrading a
/// setup-time file in place), so the two can never drift.
pub(crate) fn render_res_file(
    res: &str,
    fencing_policy: &str,
    fill_target: &str,
    min_rate: &str,
    topology: &str,
) -> String {
    format!(
        r#"# twinstor-config-version: {CONFIG_VERSION}
resource {res} {{
    protocol C;
    options {{
        # When a Primary has no up-to-date data reachable (local Inconsistent
        # mid-resync while the peer is momentarily unreachable, e.g. a node
        # mid split-brain loser-resolution), suspend I/O instead of returning a
        # hard error. A hard error propagates through the still-live iSCSI /
        # multipath path (which queue_if_no_path cannot hold: it only queues on
        # *no path*, not on an error returned via a live path) straight to the
        # guest, wedging a booting VM at initramfs or corrupting a running
        # guest's filesystem on a torn write. suspend-io blocks the bio and
        # resumes transparently the instant data is accessible again (~1s once
        # resync completes); Linux blkfront waits on the ring, Windows xenvbd
        # tolerates its 120s disk timeout; both ride the stall.
        on-no-data-accessible suspend-io;
    }}
    handlers {{
        fence-peer "/usr/lib/drbd/twinstor-fence-peer.sh";
        split-brain "/usr/lib/drbd/twinstor-split-brain.sh";
    }}
    startup {{
        # Honour a startup connection-wait before the daemon promotes, so a
        # staggered double reboot does not split the pair. DRBD
        # selects the budget from its persisted metadata for free: a node
        # that was a healthy pair when it went down boots non-degraded and
        # waits wfc-timeout (long enough for the peer to finish booting, so
        # they connect instead of both promoting Primary alone and
        # splitting); a node that already knew the peer was gone boots
        # degraded and waits only degr-wfc-timeout, serving fast. The daemon
        # drives this via `drbdadm wait-connect` in try_go_online; the
        # in-daemon ceiling (state_machine.rs CONNECT_WAIT_CEILING) must
        # exceed wfc-timeout.
        wfc-timeout 90;
        degr-wfc-timeout 10;
        outdated-wfc-timeout 10;
    }}
    disk {{
        on-io-error detach;
        fencing {fencing_policy};
        # Resync controller (SyncTarget side). c-fill-target unlocks fast links
        # (the DRBD default caps resync near ~95 MiB/s regardless of link speed);
        # c-max-rate is the ceiling. c-min-rate is the redundancy-first fix:
        # DRBD's 250k default is the resync-vs-application-I/O floor and collapses
        # the resync under app I/O (down to ~6 MB/s during a rolling
        # update), leaving the pool single-copy far too long and nearly capping the
        # reboot-drain hold. Raised, topology-scaled (see resync_min_rate()),
        # so the resync keeps making real progress under
        # load and the single-copy window / drain hold stay bounded.
        c-fill-target {fill_target};
        c-max-rate 800M;
        c-min-rate {min_rate};
    }}
    net {{
        allow-two-primaries;
        timeout 50;
        ping-int 2;
        ping-timeout 5;
        ko-count 3;
        # Never let DRBD silently auto-discard a diverged copy. In all split-brain
        # cases, including 0-primary (both nodes Secondary at reconnect, which the
        # heal/demote path transiently creates), DRBD disconnects and fires the
        # split-brain handler; twinstor then resolves deterministically, loudly, and
        # freshness-aware (never rolls back the fresher copy). The old
        # `after-sb-0pri discard-zero-changes` let DRBD auto-pick a winner with no
        # handler and no daemon control, silently rolling back a whole SR and
        # corrupting every guest on it.
        after-sb-0pri disconnect;
        after-sb-1pri disconnect;
        after-sb-2pri disconnect;
        # Online-verify checksum: without it every
        # `drbdadm verify` fails "Need a verify algorithm", and after any
        # lopsided recovery (rebuilt backing, force-promote) an online verify
        # is exactly the right integrity check, so it must work out of the
        # box. crc32c: hardware-accelerated, ample for scrub-grade detection.
        # Note: the verify-findings repair is `drbdsetup invalidate-remote
        # <res> <peer> 0 --reset-bitmap=no` from the verify initiator (a plain
        # disconnect/connect discards the findings).
        verify-alg crc32c;
    }}
{topology}
}}
"#
    )
}

/// Find the /dev/zvol symlink that resolves to `real` (a /dev/zdN node).
/// Bounded recursive walk: pools nest datasets, so the alias can live at
/// any depth under the zvol root.
async fn zvol_alias_for(real: &Path, zvol_root: &Path) -> Option<PathBuf> {
    let mut stack = vec![(zvol_root.to_path_buf(), 0u32)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > 6 {
            continue;
        }
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(e)) = entries.next_entry().await {
            let p = e.path();
            let Ok(ft) = e.file_type().await else {
                continue;
            };
            if ft.is_dir() {
                stack.push((p, depth + 1));
            } else if let Ok(target) = tokio::fs::canonicalize(&p).await
                && target == real
            {
                return Some(p);
            }
        }
    }
    None
}

/// Hide the raw DRBD backing device from LVM, in the system LVM config and
/// in SM's private tree (XCP-ng SM runs its LVM calls with
/// `LVM_SYSTEM_DIR=/etc/lvm/master`). The VG on the exported LUN is also
/// visible through the raw backing device (DRBD keeps its metadata at the
/// end of the device, so the PV label at the start shows through), giving
/// LVM a duplicate PV. That is a data hazard (the VG could be activated
/// directly on the backing, bypassing DRBD) and it breaks SR attach: sm's
/// PV identity check runs `scsi_id` against every PV path, which fails on a
/// non-SCSI (NVMe) backing device.
async fn install_lvm_filter(backing: &Path) -> Result<()> {
    // Raw-disk by-id paths and zvols expose the duplicate PV under another
    // name (the underlying sdX/zdN node); LV-backed setups have nothing to
    // hide.
    if !backing.starts_with("/dev/disk/by-id") && !backing.starts_with("/dev/zvol/") {
        return Ok(());
    }
    let name = backing
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    for conf in ["/etc/lvm/lvm.conf", "/etc/lvm/master/lvm.conf"] {
        let content = match tokio::fs::read_to_string(conf).await {
            Ok(c) => c,
            Err(_) => {
                println!("  note: {conf} not found, skipping LVM filter");
                continue;
            }
        };
        // None means the filter is already present.
        if let Some(patched) = patch_lvm_global_filter(&content, &name) {
            tokio::fs::write(conf, patched)
                .await
                .with_context(|| format!("writing {conf}"))?;
            println!("LVM filter: hiding {name} in {conf}");
        }
    }
    let _ = run_cmd("pvscan", &["--cache"]).await;
    Ok(())
}

/// Add a reject pattern for `name` to the active `global_filter` in an
/// lvm.conf, or create one inside `devices {` if none is active. Returns
/// `None` when the filter is already present.
fn patch_lvm_global_filter(content: &str, name: &str) -> Option<String> {
    if content.contains(name) {
        return None;
    }
    let reject = format!("\"r|{name}|\"");

    let mut out: Vec<String> = Vec::new();
    let mut patched = false;
    for line in content.lines() {
        let t = line.trim_start();
        if !patched && !t.starts_with('#') && t.starts_with("global_filter") && t.contains('[') {
            out.push(line.replacen(
                "global_filter = [",
                &format!("global_filter = [ {reject},"),
                1,
            ));
            patched = true;
        } else {
            out.push(line.to_string());
        }
    }
    if !patched {
        let mut with_insert: Vec<String> = Vec::new();
        for line in out {
            let is_devices = line.trim_start().starts_with("devices {");
            with_insert.push(line);
            if !patched && is_devices {
                with_insert.push(format!("\tglobal_filter = [ {reject} ]"));
                patched = true;
            }
        }
        out = with_insert;
    }
    if !patched {
        // No devices section either: append one.
        out.push(format!("devices {{\n\tglobal_filter = [ {reject} ]\n}}"));
    }
    Some(out.join("\n") + "\n")
}

/// Best-effort default gateway from the routing table, for setup messages.
pub(crate) async fn detect_default_gateway() -> Option<String> {
    let text = crate::cmd::cmd_output_for("ip", &["route", "show", "default"], 10)
        .await
        .ok()?;
    text.split_whitespace()
        .skip_while(|w| *w != "via")
        .nth(1)
        .map(|s| s.to_string())
}

/// How many times to probe the peer's iSCSI target for the LUN before giving
/// up, and the delay between tries. The peer daemon brings its target up
/// asynchronously after promotion, so the target can take a moment to start
/// answering CHAP discovery.
const PROBE_ATTEMPTS: u32 = 10;
const PROBE_RETRY_SECS: u64 = 2;

async fn create_sr(
    local_ip: IpAddr,
    peer_ip: IpAddr,
    peer_name: &str,
    iqn: &str,
    chap_user: &str,
    chap_password: &str,
    arbitrated: bool,
) -> Result<()> {
    // Enable multipath on both hosts. multipathhandle=dmp must be set
    // alongside multipathing=true: with it unset on a fresh host, SM tries to
    // O_EXCL-open the raw scsi- path that multipathd has already claimed, which
    // fails EBUSY (SR_BACKEND_FAILURE_47) at sr-create. dmp is the standard
    // XCP-ng value and is harmless where already set, so apply it unconditionally.
    println!("enabling multipath on both hosts...");
    let hosts = cmd_output("xe", &["host-list", "--minimal"]).await?;
    for host_uuid in hosts.split(',') {
        let host_uuid = host_uuid.trim();
        if host_uuid.is_empty() {
            continue;
        }
        let _ = run_cmd(
            "xe",
            &[
                "host-param-set",
                &format!("uuid={host_uuid}"),
                "multipathing=true",
                "other-config:multipathhandle=dmp",
            ],
        )
        .await;
    }

    // Discover both iSCSI targets so multipath has two paths from the start.
    // CHAP-aware: the targets enforce discovery auth (see iscsi.rs).
    println!("discovering iSCSI targets on both portals...");
    for ip in [&peer_ip.to_string(), &local_ip.to_string()] {
        crate::iscsi::discover_and_login(&format!("{ip}:3260"), Some((chap_user, chap_password)))
            .await;
    }
    let _ = run_cmd("multipathd", &["reconfigure"]).await;

    // Idempotent against an existing twinstor SR. On a re-run or a node rejoin
    // the SR is already present; running sr-create again would duplicate it (or
    // fail against the live one) and force-promote/probe needlessly. Reuse the
    // existing SR and only (re)plug the PBDs below. Probe and create only when
    // it is absent.
    let existing_sr = cmd_output(
        "xe",
        &["sr-list", "name-label=twinstor", "params=uuid", "--minimal"],
    )
    .await
    .ok()
    .map(|u| u.trim().to_string())
    .filter(|u| !u.is_empty());

    let sr_uuid: String = if let Some(uuid) = existing_sr {
        println!("twinstor SR already exists ({uuid}); reusing it (re-run / rejoin)");
        uuid
    } else {
        // Probe the peer's target (the first node has UpToDate data) for the
        // LUN's SCSI ID. The peer brings its target up asynchronously after
        // promotion, so at setup time the first probe can race a not-yet-ready
        // target (discovery auth fails). Retry with a short delay, re-priming
        // the peer login each round, rather than failing on a transient miss.
        println!("probing iSCSI target...");
        let peer_portal = format!("{peer_ip}:3260");
        let mut scsi_id = None;
        for attempt in 1..=PROBE_ATTEMPTS {
            let probe_out = cmd_output(
                "xe",
                &[
                    "sr-probe",
                    "type=lvmoiscsi",
                    &format!("device-config:target={peer_ip}"),
                    &format!("device-config:targetIQN={iqn}"),
                    &format!("device-config:chapuser={chap_user}"),
                    &format!("device-config:chappassword={chap_password}"),
                ],
            )
            .await;

            // sr-probe prints the LUN list on stdout, or (commonly) exits
            // non-zero with the same list in its error message.
            scsi_id = match &probe_out {
                Ok(out) => extract_scsi_id(out),
                Err(e) => extract_scsi_id(&e.to_string()),
            };
            if scsi_id.is_some() {
                break;
            }

            if attempt < PROBE_ATTEMPTS {
                println!(
                    "  target not ready yet (attempt {attempt}/{PROBE_ATTEMPTS}), retrying..."
                );
                // Re-prime the peer login in case its target only just came up.
                crate::iscsi::discover_and_login(&peer_portal, Some((chap_user, chap_password)))
                    .await;
                tokio::time::sleep(std::time::Duration::from_secs(PROBE_RETRY_SECS)).await;
            }
        }

        let Some(scsi_id) = scsi_id else {
            return Err(iscsi_login_refused(peer_name, peer_ip).into());
        };
        println!("  SCSI ID: {scsi_id}");

        // Create the SR using the peer's target (first node, UpToDate data)
        println!("creating shared LVMoiSCSI SR...");
        let multihomelist = format!("{peer_ip}:3260,{local_ip}:3260");
        let created = cmd_output(
            "xe",
            &[
                "sr-create",
                "type=lvmoiscsi",
                "name-label=twinstor",
                "shared=true",
                &format!("device-config:target={peer_ip}"),
                &format!("device-config:targetIQN={iqn}"),
                &format!("device-config:SCSIid={scsi_id}"),
                "device-config:port=3260",
                &format!("device-config:multihomelist={multihomelist}"),
                &format!("device-config:chapuser={chap_user}"),
                &format!("device-config:chappassword={chap_password}"),
            ],
        )
        .await?;
        let created = created.trim().to_string();
        println!("  SR created: {created}");
        created
    };
    let sr_uuid = sr_uuid.as_str();

    // Record the SR's uuid (immutable) so the daemon resolves it by identity, not
    // by the mutable name-label: a later SR rename must not break HA re-arm, PBD
    // re-plug, capacity, or doctor.
    if let Err(e) = crate::xapi::write_recorded_sr_uuid(sr_uuid) {
        println!("  warning: could not record SR uuid ({e}); the daemon will re-record on startup");
    }

    // Plug PBDs on both hosts
    println!("plugging storage on both hosts...");
    let pbds = cmd_output(
        "xe",
        &["pbd-list", &format!("sr-uuid={sr_uuid}"), "--minimal"],
    )
    .await?;
    for pbd_uuid in pbds.split(',') {
        let pbd_uuid = pbd_uuid.trim();
        if pbd_uuid.is_empty() {
            continue;
        }
        let _ = run_cmd("xe", &["pbd-plug", &format!("uuid={pbd_uuid}")]).await;
    }

    // Verify
    let attached = cmd_output(
        "xe",
        &[
            "pbd-list",
            &format!("sr-uuid={sr_uuid}"),
            "currently-attached=true",
            "--minimal",
        ],
    )
    .await?;
    let count = attached.split(',').filter(|s| !s.trim().is_empty()).count();
    println!("  {count} host(s) attached to SR");

    // Confirm this node has both iSCSI paths (local + peer portal) so the SR
    // starts fully redundant. The local target may have come up moments ago, so
    // re-prime the logins and give the sessions a few seconds to establish
    // rather than leaving the operator looking at a one-path (degraded) state.
    println!("verifying both storage paths...");
    let portals = [format!("{local_ip}:3260"), format!("{peer_ip}:3260")];
    let mut redundant = false;
    for _ in 0..10 {
        let mut up = 0;
        for p in &portals {
            if crate::iscsi::session_exists(p).await {
                up += 1;
            }
        }
        if up >= 2 {
            redundant = true;
            break;
        }
        for p in &portals {
            crate::iscsi::discover_and_login(p, Some((chap_user, chap_password))).await;
        }
        let _ = run_cmd("multipathd", &["reconfigure"]).await;
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    if redundant {
        println!("  both paths active: storage is redundant");
    } else {
        println!(
            "  one path active for now; the daemon's path watchdog will add the second shortly"
        );
    }

    // Enable XHA with the mode's timeout (see ha_timeout_secs). The pool
    // default is set first, so a later `pool-ha-enable` without
    // `ha-config:timeout` (by hand, or from Xen Orchestra) keeps this value
    // instead of XAPI's 60 s. The value found is recorded so uninstall can put
    // it back.
    let ha_timeout = ha_timeout_secs(arbitrated);
    match crate::xapi::pool_default_ha_timeout().await {
        Some((found, existing)) => {
            let record = crate::xapi::default_ha_timeout_record(
                ha_timeout,
                found.as_deref(),
                existing.as_deref(),
            );
            if crate::xapi::set_pool_default_ha_timeout(ha_timeout, record.as_deref()).await {
                println!("pool default HA timeout set to {ha_timeout}s");
            } else {
                println!(
                    "could not set the pool default HA timeout; run: xe pool-param-set \
                     uuid=<pool> other-config:{}={ha_timeout}",
                    crate::xapi::DEFAULT_HA_TIMEOUT_KEY
                );
            }
        }
        None => println!(
            "could not read the pool's other-config; set the default HA timeout later: \
             xe pool-param-set uuid=<pool> other-config:{}={ha_timeout}",
            crate::xapi::DEFAULT_HA_TIMEOUT_KEY
        ),
    }
    println!("enabling High Availability (timeout={ha_timeout}s)...");
    match run_cmd(
        "xe",
        &[
            "pool-ha-enable",
            &format!("heartbeat-sr-uuids={sr_uuid}"),
            &format!("ha-config:timeout={ha_timeout}"),
        ],
    )
    .await
    {
        Ok(()) => {
            println!("  XHA enabled");
            let pool_uuid = cmd_output("xe", &["pool-list", "--minimal"]).await?;
            let _ = run_cmd(
                "xe",
                &[
                    "pool-param-set",
                    &format!("uuid={pool_uuid}"),
                    "ha-host-failures-to-tolerate=1",
                ],
            )
            .await;
            println!("  host-failures-to-tolerate=1");
        }
        Err(e) => println!("  XHA not enabled (can be done later): {e}"),
    }

    println!("\nsetup complete. shared SR 'twinstor' is ready.");
    println!("you can now create VMs on this SR.");

    Ok(())
}

fn extract_scsi_id(text: &str) -> Option<String> {
    // Look for SCSIid in XML output: <SCSIid>...</SCSIid>
    if let Some(start) = text.find("<SCSIid>") {
        let start = start + 8;
        if let Some(end) = text[start..].find("</SCSIid>") {
            return Some(text[start..start + end].to_string());
        }
    }
    None
}

async fn open_firewall_ports(drbd_port: u16) {
    let iptables_file = "/etc/sysconfig/iptables";
    let marker = "# TWINSTOR";

    let content = match tokio::fs::read_to_string(iptables_file).await {
        Ok(c) => c,
        Err(_) => return,
    };

    if content.contains(marker) {
        return;
    }

    // Detect which chain to use (RH-Firewall-1-INPUT or INPUT)
    let chain = if content.contains("RH-Firewall-1-INPUT") {
        "RH-Firewall-1-INPUT"
    } else {
        "INPUT"
    };

    let rules = [
        format!("{marker}: DRBD replication"),
        format!("-A {chain} -p tcp -m tcp --dport {drbd_port} -j ACCEPT"),
        format!("{marker}: iSCSI target"),
        format!("-A {chain} -p tcp -m tcp --dport 3260 -j ACCEPT"),
    ];

    // Insert whole lines before the REJECT rule (or before COMMIT if there is
    // none). A substring replace of the REJECT rule would split that line in
    // two and corrupt the file, breaking iptables.service on every later load.
    let mut out: Vec<String> = Vec::new();
    let mut inserted = false;
    for line in content.lines() {
        let t = line.trim_start();
        if !inserted && ((t.starts_with("-A") && t.contains("-j REJECT")) || t == "COMMIT") {
            out.extend(rules.iter().cloned());
            inserted = true;
        }
        out.push(line.to_string());
    }
    if !inserted {
        out.extend(rules.iter().cloned());
    }
    let new_content = out.join("\n") + "\n";

    if let Ok(()) = tokio::fs::write(iptables_file, &new_content).await {
        let _ = run_cmd("systemctl", &["restart", "iptables"]).await;
    }

    // Also add live rules in case iptables service uses a different chain.
    // Guard each insert with `-C` so repeated `twinstor setup` runs don't stack
    // duplicate ACCEPT rules (the persisted-file write above is marker-guarded,
    // but these live inserts are reached whenever the file lacks the marker).
    // Mirrors the runtime path in state_machine.rs try_go_online.
    for port in [drbd_port.to_string(), "3260".to_string()] {
        let present = tokio::process::Command::new("iptables")
            .args(["-C", "INPUT", "-p", "tcp", "--dport", &port, "-j", "ACCEPT"])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !present {
            let _ = tokio::process::Command::new("iptables")
                .args(["-I", "INPUT", "-p", "tcp", "--dport", &port, "-j", "ACCEPT"])
                .output()
                .await;
        }
    }
}

/// How long the first node waits for the second before it promotes alone.
/// Long enough to read the second host's prompts, free its disks and paste
/// the CHAP password; after it, the second host copies the whole disk.
const PEER_WAIT_SECS: u64 = 30 * 60;

/// A rough duration for copying `bytes` over a `link_mbps` link, for the
/// operator ("about 1 h 30 min"); DRBD's resync reaches most of the link.
pub(crate) fn full_copy_estimate(bytes: u64, link_mbps: u32) -> String {
    let usable_bytes_per_sec = (u64::from(link_mbps) * 1_000_000 / 8) * 8 / 10;
    let secs = bytes / usable_bytes_per_sec.max(1);
    let (h, m) = (secs / 3600, (secs % 3600).div_ceil(60));
    match (h, m) {
        (0, m) => format!("about {} min", m.max(1)),
        (h, 0) => format!("about {h} h"),
        (h, m) => format!("about {h} h {m} min"),
    }
}

/// What the first node says when the second did not connect in time.
fn peer_wait_timeout_text(
    peer: &str,
    waited_secs: u64,
    bytes: Option<u64>,
    link_mbps: Option<u32>,
) -> String {
    let copy = match (bytes, link_mbps) {
        (Some(b), Some(l)) => format!(
            " ({} for {} GB at {l} Mb/s)",
            full_copy_estimate(b, l),
            b / 1_000_000_000
        ),
        _ => " (hours for large disks)".to_string(),
    };
    format!(
        "{peer} did not connect within {} minutes. This host now serves alone; when\n\
         {peer} is set up, it copies the whole disk from this host{copy}.\n\
         promoting this host...",
        waited_secs / 60
    )
}

async fn wait_for_peer(peer_ip: IpAddr, drbd_port: u16, timeout_secs: u64) -> bool {
    for _ in 0..timeout_secs {
        if let Ok(stream) = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            tokio::net::TcpStream::connect(format!("{peer_ip}:{drbd_port}")),
        )
        .await
            && stream.is_ok()
        {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    false
}

async fn wait_for_drbd_connected(resource: &str, timeout_secs: u64) -> bool {
    let mut sp = crate::render::Spinner::new("waiting for the peer node to connect");
    for i in 0..timeout_secs {
        if let Ok(out) = tokio::process::Command::new("drbdadm")
            .args(["status", resource])
            .output()
            .await
        {
            let s = String::from_utf8_lossy(&out.stdout);
            if s.contains("connection:Connected") || s.contains("peer-disk:") {
                sp.finish();
                return true;
            }
        }
        if !sp.is_tty() && i % 10 == 9 {
            println!(
                "  still waiting for the peer node to connect... ({}/{}s)",
                i + 1,
                timeout_secs
            );
        }
        // Four sub-ticks per second keep the spinner moving between the 1 s
        // status polls (no-ops when piped).
        for _ in 0..4 {
            sp.tick(&format!(" ({}/{}s)", i + 1, timeout_secs));
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
    sp.finish();
    false
}

fn generate_chap_password() -> Result<String> {
    use std::io::Read;
    let mut password = String::new();
    let mut file = std::fs::File::open("/dev/urandom").context("opening /dev/urandom")?;
    let mut buf = [0u8; 1];
    while password.len() < 16 {
        file.read_exact(&mut buf)?;
        if buf[0].is_ascii_alphanumeric() {
            password.push(buf[0] as char);
        }
    }
    Ok(password)
}

/// Whether LIO (rtslib) will accept this IQN as an ACL WWN. rtslib requires
/// `iqn.YYYY-MM.<authority>[:<id>]` where the naming authority has at least
/// two labels ("com.example" passes, a bare DHCP domain like "home" does
/// not). XCP-ng generates host initiator IQNs from the DNS domain, so a
/// single-label domain produces an IQN the initiator accepts but the LIO
/// target refuses, which would leave the target with no ACLs if not caught
/// here.
fn iqn_is_lio_compatible(iqn: &str) -> bool {
    let rest = match iqn.strip_prefix("iqn.") {
        Some(r) => r,
        None => return false,
    };
    let (date, rest) = match rest.split_once('.') {
        Some(x) => x,
        None => return false,
    };
    let date_ok = date.len() == 7
        && date[..4].chars().all(|c| c.is_ascii_digit())
        && date.as_bytes()[4] == b'-'
        && date[5..].chars().all(|c| c.is_ascii_digit());
    if !date_ok {
        return false;
    }
    let authority = rest.split(':').next().unwrap_or("");
    !authority.is_empty() && authority.contains('.')
}

/// Repair a LIO-incompatible IQN by appending a synthetic second label to a
/// single-label naming authority, keeping XCP-ng's date and unique id verbatim
/// (`iqn.2026-06.home:abcd` becomes `iqn.2026-06.home.lan:abcd`). Returns None when
/// the IQN is already valid or its shape is unfamiliar enough that repairing it
/// would be guessing.
fn sanitize_iqn(iqn: &str) -> Option<String> {
    if iqn_is_lio_compatible(iqn) {
        return None; // nothing to fix
    }
    let rest = iqn.strip_prefix("iqn.")?;
    let (date, rest) = rest.split_once('.')?;
    let date_ok = date.len() == 7
        && date[..4].chars().all(|c| c.is_ascii_digit())
        && date.as_bytes()[4] == b'-'
        && date[5..].chars().all(|c| c.is_ascii_digit());
    if !date_ok {
        return None; // only repair the standard iqn.YYYY-MM. shape
    }
    let (authority, id) = match rest.split_once(':') {
        Some((a, i)) => (a, Some(i)),
        None => (rest, None),
    };
    // Only the single-label-authority case is repairable here; an empty
    // authority is too broken to invent one for.
    if authority.is_empty() || authority.contains('.') {
        return None;
    }
    Some(match id {
        Some(i) => format!("iqn.{date}.{authority}.lan:{i}"),
        None => format!("iqn.{date}.{authority}.lan"),
    })
}

/// This host's UUID from the XCP-ng inventory (the canonical local identity,
/// no hostname matching needed).
pub(crate) async fn local_host_uuid() -> Option<String> {
    let inv = tokio::fs::read_to_string("/etc/xensource-inventory")
        .await
        .ok()?;
    inv.lines().find_map(|l| {
        l.strip_prefix("INSTALLATION_UUID=")
            .map(|v| v.trim().trim_matches('\'').trim_matches('"').to_string())
    })
}

/// Pool-wide XAPI key (`pool other-config:<KEY>`) the first node uses to hand
/// the setup choices common to both nodes to the second node, so the second
/// node's wizard inherits them instead of asking again. The pool DB is a trusted
/// channel the code already relies on (it reads host `other-config:iscsi_iqn`
/// the same way), so this needs no dom0-to-dom0 SSH. It is cleared once the
/// cluster is formed (second node creates the SR) or on uninstall. The CHAP
/// password is not stored: it is a secret, so the operator pastes it on the
/// second node.
const SHARED_SETUP_KEY: &str = "twinstor_setup";

/// The setup choices common to both nodes. Every field is identical on both
/// nodes (or, for `direct_ip`, names the other node's endpoint); the per-host
/// backing device is absent, since it is always chosen locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedSetup {
    pub arbitrated: bool,
    /// The publishing (first) node's replication IP: the address the second node
    /// uses to reach it, and the key to match the same replication network.
    pub peer_ip: IpAddr,
    /// The first node's direct-link IP (arbitrated only), which is the second
    /// node's direct-link peer.
    pub direct_ip: Option<IpAddr>,
    pub ping_targets: Vec<IpAddr>,
    pub full_sync: bool,
    pub drbd_port: u16,
    pub chap_user: String,
    /// UUID of the host that published this, so a first-node re-run does not
    /// mistake its own handoff for an inherited one.
    pub publisher_uuid: String,
    /// A short check value of the first node's CHAP password
    /// (`chap_check_value`), so the second node can tell a wrong paste before
    /// it wipes anything. None from a first node that predates it: the paste
    /// is then accepted unchecked, as before.
    pub chap_check: Option<String>,
}

impl SharedSetup {
    /// Encode to a single line (`k=v;k=v`) for an `xe other-config` value: all
    /// fields are simple scalars or IPs, so this is more robust in a one-line
    /// slot than multi-line TOML and needs no extra dependency.
    fn encode(&self) -> String {
        let pings = self
            .ping_targets
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let direct = self.direct_ip.map(|d| d.to_string()).unwrap_or_default();
        format!(
            "v=1;arbitrated={};peer_ip={};direct_ip={};ping={};full_sync={};drbd_port={};chap_user={};publisher={};chap_check={}",
            self.arbitrated as u8,
            self.peer_ip,
            direct,
            pings,
            self.full_sync as u8,
            self.drbd_port,
            self.chap_user,
            self.publisher_uuid,
            self.chap_check.as_deref().unwrap_or_default(),
        )
    }

    /// Parse the line `encode` produced. Returns None for an absent, foreign
    /// or malformed value so the caller falls back to asking.
    fn decode(s: &str) -> Option<Self> {
        let mut map = std::collections::HashMap::new();
        for kv in s.trim().split(';') {
            if let Some((k, v)) = kv.split_once('=') {
                map.insert(k, v);
            }
        }
        if map.get("v") != Some(&"1") {
            return None;
        }
        let ping_targets = map
            .get("ping")
            .map(|s| {
                s.split(',')
                    .filter(|x| !x.is_empty())
                    .filter_map(|x| x.parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        let direct_ip = match map.get("direct_ip") {
            Some(s) if !s.is_empty() => Some(s.parse().ok()?),
            _ => None,
        };
        Some(SharedSetup {
            arbitrated: map.get("arbitrated") == Some(&"1"),
            peer_ip: map.get("peer_ip")?.parse().ok()?,
            direct_ip,
            ping_targets,
            full_sync: map.get("full_sync") == Some(&"1"),
            drbd_port: map.get("drbd_port")?.parse().ok()?,
            chap_user: map.get("chap_user")?.to_string(),
            publisher_uuid: map.get("publisher")?.to_string(),
            // An added key, so the format stays v=1: an older second node
            // ignores it, and an older first node simply does not publish it.
            chap_check: map
                .get("chap_check")
                .filter(|c| !c.is_empty())
                .map(|c| c.to_string()),
        })
    }
}

/// SHA-256 of `data` (FIPS 180-4). Only for the CHAP check value below, which
/// does not justify a crypto dependency.
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(v);
        }
    }
    let mut out = [0u8; 32];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

/// The check value the first node publishes for its CHAP password: the first
/// 8 hex digits of SHA-256 over a fixed prefix and the password. Enough to
/// catch a wrong or truncated paste on the second node, too short to say
/// anything useful about the password itself.
pub(crate) fn chap_check_value(password: &str) -> String {
    sha256(format!("twinstor-chap:{password}").as_bytes())[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// What a CHAP password typed on the second node is worth against the check
/// value the first node published.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ChapVerdict {
    /// It is the first node's password.
    Matches,
    /// It is not: setup would fail at the very end, after the wipe.
    Differs,
    /// The first node published no check value (an older version): accepted
    /// unchecked.
    Unchecked,
}

pub(crate) fn chap_verdict(password: &str, published: Option<&str>) -> ChapVerdict {
    match published {
        None => ChapVerdict::Unchecked,
        Some(c) if c.eq_ignore_ascii_case(&chap_check_value(password)) => ChapVerdict::Matches,
        Some(_) => ChapVerdict::Differs,
    }
}

/// A CHAP password as pasted: surrounding blanks and quotes dropped, and the
/// `CHAP password:` label removed when the whole line was copied from the
/// first node's banner.
pub(crate) fn normalize_chap_paste(raw: &str) -> String {
    let mut s = raw.trim();
    let label = "chap password";
    if s.len() >= label.len()
        && s.is_char_boundary(label.len())
        && s[..label.len()].eq_ignore_ascii_case(label)
    {
        s = s[label.len()..].trim_start().trim_start_matches(':').trim();
    }
    s.trim_matches(|c| c == '"' || c == '\'').trim().to_string()
}

async fn pool_uuid() -> Option<String> {
    cmd_output("xe", &["pool-list", "--minimal"])
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Publish the first node's shared choices for the second node to inherit.
/// Best-effort: on failure the second node asks the questions itself.
async fn publish_shared_setup(s: &SharedSetup) {
    let Some(pool) = pool_uuid().await else {
        return;
    };
    let _ = run_cmd(
        "xe",
        &[
            "pool-param-set",
            &format!("uuid={pool}"),
            &format!("other-config:{SHARED_SETUP_KEY}={}", s.encode()),
        ],
    )
    .await;
}

/// Shared setup another node published, or None when absent, malformed, or
/// published by this host (a first-node re-run reading its own handoff). Only a
/// real second node gets a value back, so the wizard can key inheritance on it.
pub async fn inherited_shared_setup() -> Option<SharedSetup> {
    let s = published_shared_setup().await?;
    if local_host_uuid().await.as_deref() == Some(s.publisher_uuid.as_str()) {
        return None; // our own handoff: we are the first node re-running
    }
    Some(s)
}

/// Whether this host published the handoff that is in the pool DB now: it
/// is the first node, and the second has not created the SR yet.
pub(crate) async fn own_shared_setup_published() -> bool {
    match (published_shared_setup().await, local_host_uuid().await) {
        (Some(s), Some(me)) => s.publisher_uuid == me,
        _ => false,
    }
}

/// The handoff in the pool DB, whoever published it.
async fn published_shared_setup() -> Option<SharedSetup> {
    let pool = pool_uuid().await?;
    let raw = cmd_output(
        "xe",
        &[
            "pool-param-get",
            &format!("uuid={pool}"),
            "param-name=other-config",
            &format!("param-key={SHARED_SETUP_KEY}"),
        ],
    )
    .await
    .ok()?;
    SharedSetup::decode(&raw)
}

/// Remove the shared-setup handoff key. Called once the cluster is formed (the
/// second node created the SR) and defensively on uninstall. Idempotent.
pub async fn clear_shared_setup() {
    let Some(pool) = pool_uuid().await else {
        return;
    };
    // Fails when the key is already gone, which is the usual case.
    let _ = run_cmd_quiet(
        "xe",
        &[
            "pool-param-remove",
            &format!("uuid={pool}"),
            "param-name=other-config",
            &format!("param-key={SHARED_SETUP_KEY}"),
        ],
    )
    .await;
}

/// Whether the pool already has an iSCSI SR. Used as a brownfield guard before
/// auto-rewriting a host's initiator IQN: an existing iSCSI SR may depend on the
/// current IQN, so changing it could break those sessions. TwinStor's own SR
/// does not exist yet at this point (setup creates it later), so any match here
/// is a pre-existing one.
async fn pool_has_other_iscsi_sr() -> bool {
    for ty in ["lvmoiscsi", "iscsi"] {
        if let Ok(out) = cmd_output("xe", &["sr-list", &format!("type={ty}"), "--minimal"]).await
            && !out.trim().is_empty()
        {
            return true;
        }
    }
    false
}

/// Make this host's OS initiator name (`/etc/iscsi/initiatorname.iscsi`) match
/// the given IQN and restart iscsid, unless it already matches. The login the
/// SR makes uses this file, so it must equal the IQN the LIO ACL allows (which
/// may have been sanitized in XAPI by either node).
async fn ensure_local_initiator(iqn: &str) -> Result<()> {
    const PATH: &str = "/etc/iscsi/initiatorname.iscsi";
    let want = format!("InitiatorName={iqn}");
    let current = tokio::fs::read_to_string(PATH).await.unwrap_or_default();
    if current.lines().any(|l| l.trim() == want) {
        return Ok(());
    }
    tokio::fs::write(PATH, format!("{want}\n"))
        .await
        .with_context(|| format!("writing {PATH}"))?;
    run_cmd("systemctl", &["restart", "iscsid"])
        .await
        .context("restarting iscsid after IQN change")?;
    println!("  set local iSCSI initiator name to {iqn}");
    Ok(())
}

async fn get_initiator_iqns() -> Result<Vec<String>> {
    let hosts = cmd_output("xe", &["host-list", "--minimal"])
        .await
        .context("listing hosts (is this node in a pool?)")?;
    let local_uuid = local_host_uuid().await;
    // (host_uuid, iscsi_iqn) for every pool host that has one in XAPI.
    let mut pairs: Vec<(String, String)> = Vec::new();
    for uuid in hosts.split(',') {
        let uuid = uuid.trim();
        if uuid.is_empty() {
            continue;
        }
        if let Ok(iqn) = cmd_output(
            "xe",
            &[
                "host-param-get",
                &format!("uuid={uuid}"),
                "param-name=other-config",
                "param-key=iscsi_iqn",
            ],
        )
        .await
        {
            let iqn = iqn.trim().to_string();
            if !iqn.is_empty() {
                pairs.push((uuid.to_string(), iqn));
            }
        }
    }

    // Auto-sanitize any LIO-invalid IQN (XCP-ng derives the IQN from the DNS
    // domain, so a single-label domain like '.home' yields one LIO refuses as
    // an ACL). Skip the rewrite when an iSCSI SR already depends on the current
    // IQN (brownfield guard): in that case fall through to the bail below with
    // manual guidance.
    let any_invalid = pairs.iter().any(|(_, iqn)| !iqn_is_lio_compatible(iqn));
    if any_invalid && !pool_has_other_iscsi_sr().await {
        for (uuid, iqn) in pairs.iter_mut() {
            if let Some(fixed) = sanitize_iqn(iqn) {
                println!("  host {uuid}: iSCSI IQN '{iqn}' invalid for LIO, setting '{fixed}'");
                // Pool-wide XAPI update (forwards to the master from any node).
                run_cmd(
                    "xe",
                    &[
                        "host-param-set",
                        &format!("uuid={uuid}"),
                        &format!("other-config:iscsi_iqn={fixed}"),
                    ],
                )
                .await
                .with_context(|| format!("setting iscsi_iqn on host {uuid}"))?;
                *iqn = fixed;
            }
        }
    }

    // After any sanitize, every IQN must be valid. A survivor here means the
    // brownfield guard blocked the auto-fix or the shape was unrepairable.
    for (_, iqn) in &pairs {
        if !iqn_is_lio_compatible(iqn) {
            bail!(
                "initiator IQN '{iqn}' (from XAPI other-config:iscsi_iqn) is not accepted by \
                 LIO as an ACL: the naming authority needs at least two labels \
                 (e.g. iqn.2026-06.com.example:host). XCP-ng generates this from the host's \
                 DNS domain, so a single-label domain (e.g. '.home') produces an invalid one.\n\
                 An iSCSI SR already exists on this pool, so setup did not rewrite the IQN \
                 automatically (that could break those sessions). Fix on each affected host, \
                 then re-run setup:\n\
                 \x20  echo 'InitiatorName=iqn.2026-06.lab.example:<hostname>' > /etc/iscsi/initiatorname.iscsi\n\
                 \x20  xe host-param-set uuid=<host-uuid> other-config:iscsi_iqn=iqn.2026-06.lab.example:<hostname>\n\
                 \x20  systemctl restart iscsid"
            );
        }
    }

    // This host's OS initiator name must match its (possibly sanitized) XAPI
    // IQN, whether this node rewrote it or the peer did. The peer fixes its own
    // file when its setup runs.
    if let Some(local) = &local_uuid
        && let Some((_, iqn)) = pairs.iter().find(|(u, _)| u == local)
    {
        ensure_local_initiator(iqn).await?;
    }

    let mut iqns: Vec<String> = pairs.into_iter().map(|(_, iqn)| iqn).collect();
    if iqns.is_empty() {
        // Fallback: read local initiator IQN from file
        let content = tokio::fs::read_to_string("/etc/iscsi/initiatorname.iscsi")
            .await
            .context("reading /etc/iscsi/initiatorname.iscsi")?;
        for line in content.lines() {
            if let Some(iqn) = line.strip_prefix("InitiatorName=") {
                let iqn = iqn.trim();
                if !iqn.is_empty() {
                    iqns.push(iqn.to_string());
                }
            }
        }
    }
    if iqns.is_empty() {
        bail!(
            "no initiator IQNs found (check /etc/iscsi/initiatorname.iscsi and XAPI other-config:iscsi_iqn)"
        );
    }
    Ok(iqns)
}

/// Given this host's UUID and every pool host's UUID, pick the single peer.
/// TwinStor is strictly two-node, so the peer is the other host.
/// Returns `None` when the local UUID is unknown or the pool does not hold
/// exactly one other host, so the caller never guesses on an ambiguous pool.
fn peer_uuid_from_pool(local_uuid: Option<&str>, all_uuids: &[String]) -> Option<String> {
    let local = local_uuid?;
    let mut others = all_uuids.iter().filter(|u| u.as_str() != local);
    let peer = others.next()?;
    if others.next().is_some() {
        return None;
    }
    Some(peer.clone())
}

async fn detect_peer_hostname(peer_ip: IpAddr) -> Result<String> {
    // Try XAPI by address. This is correct and fast when storage and management
    // share a network, since XAPI matches `address=` on the management IP.
    if let Ok(out) = tokio::process::Command::new("xe")
        .args([
            "host-list",
            &format!("address={peer_ip}"),
            "params=hostname",
            "--minimal",
        ])
        .output()
        .await
    {
        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !name.is_empty() {
            return Ok(name);
        }
    }
    // On split storage/management networks the DRBD IP does not match any
    // management `address=`, so the lookup above returns empty. Fall back to
    // pool membership: in a two-node pool the peer is the other host.
    let all_uuids: Vec<String> = cmd_output("xe", &["host-list", "--minimal"])
        .await
        .unwrap_or_default()
        .split(',')
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .collect();
    if let Some(peer) = peer_uuid_from_pool(local_host_uuid().await.as_deref(), &all_uuids)
        && let Ok(name) = cmd_output(
            "xe",
            &[
                "host-param-get",
                &format!("uuid={peer}"),
                "param-name=hostname",
            ],
        )
        .await
        && !name.is_empty()
    {
        return Ok(name);
    }
    // Fall back to DNS
    if let Ok(out) = tokio::process::Command::new("getent")
        .args(["hosts", &peer_ip.to_string()])
        .output()
        .await
    {
        let name = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .to_string();
        if !name.is_empty() {
            return Ok(name);
        }
    }
    bail!("could not determine peer hostname for {peer_ip} (is the pool formed?)")
}

/// XHA failover timeout (`ha-config:timeout`), in seconds, by mode. The XHA
/// statefile lives on the DRBD-backed SR, so each host writes its own copy of it
/// while replication is split. Fate-shared mode: a node that loses its peer
/// keeps its guests off the peer's copy until the pair is Connected again, so
/// the statefile copies stay apart for the whole split (the cut, then 15-75 s
/// of resolution); a split that outlasts the timeout makes XHA reboot one host
/// of its own choosing, which can be the host running the VMs. 120 s lets a
/// split of roughly 45-100 s heal without a reboot; 60 s covers only splits up
/// to about 45 s. Arbitrated mode freezes I/O at the disconnect and rides a
/// switch fault over the direct link, for which 90 s is enough. A larger value
/// means a slower failover on a real host death.
pub(crate) fn ha_timeout_secs(arbitrated: bool) -> u32 {
    if arbitrated { 90 } else { 120 }
}

/// The HA timeout XHA actually runs with on this host (`StateFileTimeout` in
/// `xhad.conf`): what applies when the pool's `ha-config:timeout` is unset.
pub(crate) fn effective_xha_timeout() -> Option<u32> {
    let text = std::fs::read_to_string("/etc/xensource/xhad.conf").ok()?;
    let start = text.find("<StateFileTimeout>")? + "<StateFileTimeout>".len();
    let end = text[start..].find('<')? + start;
    text[start..end].trim().parse().ok()
}

/// The sorted-hostname (local, peer) DRBD node-id pair for a new pool: the lower
/// hostname is node-id 0. Both nodes compute the same mapping without talking, and
/// it matches the daemon's keeper convention. Used at setup so `drbdadm create-md`
/// writes these ids into the metadata.
pub(crate) fn sorted_node_ids(local_name: &str, peer_name: &str) -> (u32, u32) {
    if local_name < peer_name {
        (0, 1)
    } else {
        (1, 0)
    }
}

/// Build the DRBD `on`/`connection` section of the resource config.
///
/// `direct = None` gives a single switched path (fate-shared mode). `direct =
/// Some((local_direct, peer_direct))` gives arbitrated mode: two replication
/// paths (direct link first, switched second) in DRBD's default active-backup
/// mode. The direct link is the normal replication path; the switched path is
/// the fallback a cable or NIC failure fails over to (not load-balance-paths, a
/// throughput feature that resets the connection on any path state change).
/// The explicit connection/path form requires node-ids.
///
/// `node_ids` is the (local, peer) DRBD node-id pair to write into the `on`
/// sections. New-pool setup passes the sorted-hostname pair (via
/// [`sorted_node_ids`]) for both topologies, so a fate-shared pool does not
/// leave DRBD to auto-assign ids that may not follow hostname order.
/// `regenerate-config` passes the pool's existing ids (parsed from the running
/// `.res`) so it never writes an id that mismatches the on-disk metadata. `None`
/// keeps the fate-shared section id-less, preserving a pool created without
/// explicit ids; in arbitrated mode it falls back to the sorted-hostname pair.
// The args are flat node and address fields with no useful grouping, so they
// stay positional rather than becoming a struct. The backing path is
// double-quoted: by-id aliases for mdraid arrays contain colons, which DRBD's
// parser otherwise reads as the address host:port separator.
#[allow(clippy::too_many_arguments)]
pub(crate) fn drbd_topology(
    local_name: &str,
    peer_name: &str,
    drbd_dev: &str,
    backing: &str,
    local_ip: IpAddr,
    peer_ip: IpAddr,
    port: u16,
    direct: Option<(IpAddr, IpAddr)>,
    node_ids: Option<(u32, u32)>,
) -> String {
    match direct {
        None => {
            let (l_nid, p_nid) = match node_ids {
                Some((l, p)) => (
                    format!("\n        node-id {l};"),
                    format!("\n        node-id {p};"),
                ),
                None => (String::new(), String::new()),
            };
            format!(
                "    on {local_name} {{{l_nid}
        device {drbd_dev}; disk \"{backing}\"; address {local_ip}:{port}; meta-disk internal;
    }}
    on {peer_name} {{{p_nid}
        device {drbd_dev}; disk \"{backing}\"; address {peer_ip}:{port}; meta-disk internal;
    }}"
            )
        }
        Some((local_direct, peer_direct)) => {
            let (local_node_id, peer_node_id) = node_ids.unwrap_or(if local_name < peer_name {
                (0, 1)
            } else {
                (1, 0)
            });
            format!(
                "    on {local_name} {{
        node-id {local_node_id};
        device {drbd_dev}; disk \"{backing}\"; meta-disk internal;
    }}
    on {peer_name} {{
        node-id {peer_node_id};
        device {drbd_dev}; disk \"{backing}\"; meta-disk internal;
    }}
    connection {{
        # Arbitrated mode: two replication paths, the back-to-back direct link
        # and the switched network, in DRBD's default active-backup mode (one
        # path active, the other a hot standby). Losing the active path fails
        # over to the standby via a reconnect: under active dual-primary
        # writes that reconnect always sees unacknowledged in-flights on both
        # sides (bitmap-both), so the fence-peer tiebreak resolves it
        # deterministically (lower hostname serves throughout; the other side
        # yields frozen and resyncs; the discard is only never-acked
        # in-flights). Losing the standby is a no-op. We deliberately do not set
        # `load-balance-paths` here: that is a throughput feature (lb-tcp
        # stripes one stream across all paths) and any single path state change
        # (down or up) resets the whole connection (StandAlone/resync),
        # which on a dual-Primary pair opens a split-brain window.
        #
        # The direct link is declared first on purpose: replication
        # belongs on the dedicated cable in steady state. It keeps synchronous
        # writes and resync storms off the switch, and it means switch faults
        # (the common ones) never touch replication. The switch path is the
        # fallback; while replication runs there the topology is temporarily
        # fate-shared (safer, not less safe) and the daemon alerts.
        path {{
            host {local_name} address {local_direct}:{port};
            host {peer_name} address {peer_direct}:{port};
        }}
        path {{
            host {local_name} address {local_ip}:{port};
            host {peer_name} address {peer_ip}:{port};
        }}
    }}"
            )
        }
    }
}

async fn detect_local_ip(peer_ip: IpAddr) -> Result<IpAddr> {
    let out = tokio::process::Command::new("ip")
        .args(["route", "get", &peer_ip.to_string()])
        .output()
        .await
        .context("ip route get")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .split_whitespace()
        .skip_while(|w| *w != "src")
        .nth(1)
        .context("no src in ip route output")?
        .parse()
        .context("invalid source IP")
}

/// Bring the local node UpToDate without a full initial sync, tolerating the
/// race between the two nodes. `drbdadm --clear-bitmap new-current-uuid`
/// requires StandAlone, so once the peer has connected only one side can run
/// it; the other comes up UpToDate over the connection (both devices are
/// fresh, so the connect-time sync is a no-op) and must neither need nor run
/// clear-bitmap. Whichever node wins the StandAlone window initialises the
/// current-UUID; the loser is tolerated as long as it ends up UpToDate.
async fn establish_skip_sync(res: &str) -> Result<()> {
    if drbd_disk_uptodate(res).await {
        println!("already UpToDate, no initial sync needed...");
        return Ok(());
    }
    println!("skipping initial sync (both devices are fresh)...");
    if let Err(e) = run_cmd(
        "drbdadm",
        &["--", "--clear-bitmap", "new-current-uuid", res],
    )
    .await
    {
        // Lost the race: clear-bitmap is rejected now that we are Connected.
        // That is fine as long as the connection brings us UpToDate shortly.
        for _ in 0..5 {
            if drbd_disk_uptodate(res).await {
                println!("  peer already initialised the bitmap, continuing...");
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        return Err(e);
    }
    Ok(())
}

/// Whether the local DRBD disk for `res` is UpToDate, per `drbdadm status`.
async fn drbd_disk_uptodate(res: &str) -> bool {
    tokio::process::Command::new("drbdadm")
        .args(["status", res])
        .output()
        .await
        .ok()
        .and_then(|o| crate::drbd::parse_drbdadm_text(&String::from_utf8_lossy(&o.stdout)).ok())
        .map(|s| s.disk == crate::drbd::DiskState::UpToDate)
        .unwrap_or(false)
}

/// The static generated artifacts: content ships embedded in the binary and
/// carries no per-deployment state, so overwriting is always safe. Written by
/// setup and byte-reconciled by the daemon at every start: safety fixes
/// often land in the handlers, and a binary upgrade never re-runs setup.
pub(crate) fn static_artifacts() -> [(&'static str, &'static str); 3] {
    [
        (
            "/usr/lib/drbd/twinstor-fence-peer.sh",
            include_str!("../config/twinstor-fence-peer.sh"),
        ),
        (
            "/usr/lib/drbd/twinstor-split-brain.sh",
            include_str!("../config/twinstor-split-brain.sh"),
        ),
        (
            "/etc/logrotate.d/twinstor",
            include_str!("../config/twinstor.logrotate"),
        ),
    ]
}

/// Bring the on-disk static artifacts back in line with the embedded copies.
/// Best-effort (the daemon must come up even on a read-only or half-broken
/// filesystem); a drifted file is backed up once to `<path>.bak-prev` before
/// the overwrite so a hand-patched handler is recoverable. Missing files are
/// (re)created: the daemon only runs on set-up hosts, so absence is damage,
/// not a fresh system.
pub(crate) async fn reconcile_static_artifacts() {
    for (path, want) in static_artifacts() {
        let current = tokio::fs::read_to_string(path).await.ok();
        if current.as_deref() == Some(want) {
            continue;
        }
        if current.is_some() {
            let _ = tokio::fs::copy(path, format!("{path}.bak-prev")).await;
        }
        match tokio::fs::write(path, want).await {
            Ok(()) => {
                if path.ends_with(".sh") {
                    let _ = tokio::process::Command::new("chmod")
                        .args(["+x", path])
                        .output()
                        .await;
                }
                tracing::info!(
                    path,
                    existed = current.is_some(),
                    "reconciled managed artifact with this binary's embedded copy (config drift)"
                );
            }
            Err(e) => tracing::warn!(path, error = %e, "could not reconcile managed artifact"),
        }
    }
}

/// The `# twinstor-config-version: N` stamp of a generated .res, 0 when the
/// file predates the stamp. Detection only: rewriting the .res needs `drbdadm
/// adjust` and a topology-change guard, which `regenerate-config` (also run by
/// `upgrade-apply`) provides.
pub(crate) fn res_config_version(res_text: &str) -> u32 {
    res_text
        .lines()
        .find_map(|l| l.trim().strip_prefix("# twinstor-config-version:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{CONFIG_VERSION, res_config_version, static_artifacts};

    #[test]
    fn res_version_stamp_roundtrip() {
        let stamped = format!("# twinstor-config-version: {CONFIG_VERSION}\nresource r {{}}\n");
        assert_eq!(res_config_version(&stamped), CONFIG_VERSION);
        // Pre-stamp deployments read as version 0.
        assert_eq!(res_config_version("resource r {}\n"), 0);
        assert_eq!(res_config_version(""), 0);
    }

    /// Run the embedded fence-peer handler against PATH shims in a scratch
    /// root. `state` files drive the shims: role, susp (the resource's
    /// suspended: set), ping_rc, hostname. Nothing on the replication or
    /// beacon addresses listens (127.0.0.254), so the peer reads as down.
    struct FenceRig {
        root: std::path::PathBuf,
    }

    impl FenceRig {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("twinstor-test-fence-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            for d in ["bin", "etc", "run", "state"] {
                std::fs::create_dir_all(root.join(d)).unwrap();
            }
            let script = static_artifacts()
                .into_iter()
                .find(|(p, _)| p.ends_with("twinstor-fence-peer.sh"))
                .unwrap()
                .1;
            std::fs::write(root.join("fence-peer.sh"), script).unwrap();
            std::fs::write(
                root.join("etc/twinstor.toml"),
                "peer_address = \"127.0.0.254\"\ndrbd_port = 1\n",
            )
            .unwrap();
            let st = root.join("state");
            let shims = [
                (
                    "drbdadm",
                    format!(
                        "#!/bin/sh\nS={st}\necho \"$*\" >> $S/drbdadm.calls\ncase \"$1\" in\n\
                     role) [ -f $S/role_fails ] && exit 1; cat $S/role ;;\n\
                     status) [ -f $S/status_fails ] && exit 1; echo \"$2 role:$(cat $S/role) suspended:$(cat $S/susp)\" ;;\n\
                     cstate) echo StandAlone ;;\n\
                     get-gi) cat $S/gi 2>/dev/null ;;\n\
                     dstate) cat $S/dstate 2>/dev/null || echo UpToDate/UpToDate ;;\n\
                     suspend-io) [ -f $S/suspend_rc ] && exit $(cat $S/suspend_rc); echo user,fencing > $S/susp; [ -f $S/suspend_hangs ] && sleep 30 ;;\n\
                     esac\nexit 0\n",
                        st = st.display()
                    ),
                ),
                ("drbdsetup", "#!/bin/sh\nexit 0\n".to_string()),
                (
                    "ping",
                    format!("#!/bin/sh\nexit $(cat {}/ping_rc)\n", st.display()),
                ),
                (
                    "ip",
                    "#!/bin/sh\necho 'default via 127.0.0.253 dev lo'\n".to_string(),
                ),
                (
                    "hostname",
                    format!("#!/bin/sh\ncat {}/hostname\n", st.display()),
                ),
                ("logger", "#!/bin/sh\nexit 0\n".to_string()),
            ];
            for (name, body) in shims {
                let p = root.join("bin").join(name);
                std::fs::write(&p, body).unwrap();
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let rig = FenceRig { root };
            rig.set("role", "Primary");
            rig.set("susp", "fencing");
            rig.set("ping_rc", "0");
            rig.set("hostname", "twinstor-2");
            rig
        }
        fn set(&self, key: &str, val: &str) {
            std::fs::write(self.root.join("state").join(key), val).unwrap();
        }
        fn marker(&self) -> std::path::PathBuf {
            self.root.join("run/fence-yielded")
        }
        fn lock(&self) -> std::path::PathBuf {
            self.root.join("run/fence-peer.lock")
        }
        fn command(&self, lock_wait: u32) -> std::process::Command {
            let mut c = std::process::Command::new("bash");
            c.arg(self.root.join("fence-peer.sh"))
                .env("TWINSTOR_SHIM_PATH", self.root.join("bin"))
                .env("TWINSTOR_ETC_DIR", self.root.join("etc"))
                .env("TWINSTOR_RUN_DIR", self.root.join("run"))
                .env("TWINSTOR_FENCE_LOG", self.root.join("fence.log"))
                .env("TWINSTOR_FENCE_LOCK_WAIT", lock_wait.to_string())
                .env("DRBD_RESOURCE", "twinstor-data")
                .env("TWINSTOR_DAEMON_EXE", self.daemon_exe());
            c
        }
        fn run(&self, lock_wait: u32) -> i32 {
            self.command(lock_wait).status().unwrap().code().unwrap()
        }
        fn log(&self) -> String {
            std::fs::read_to_string(self.root.join("fence.log")).unwrap_or_default()
        }
        /// The freeze verdict of the latest run (its `started:` line).
        fn last_freeze(&self) -> String {
            let log = self.log();
            let line = log
                .lines()
                .rfind(|l| l.contains("started:"))
                .unwrap_or("")
                .to_string();
            line.rsplit("freeze=[")
                .next()
                .unwrap_or("")
                .trim_end_matches(']')
                .to_string()
        }
        fn daemon_exe(&self) -> std::path::PathBuf {
            self.root.join("daemon/twinstor")
        }
        /// A process standing in for the daemon: the rig's "installed binary"
        /// (a copy of bash) running with `arg` among its arguments, as the
        /// unit runs `twinstor ... daemon`.
        fn fake_process(&self, arg: &str) -> FakeProc {
            let exe = self.daemon_exe();
            if !exe.exists() {
                std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
                let bash = std::process::Command::new("sh")
                    .args(["-c", "command -v bash"])
                    .output()
                    .unwrap()
                    .stdout;
                std::fs::copy(String::from_utf8_lossy(&bash).trim(), &exe).unwrap();
            }
            // `; :` keeps bash from exec'ing sleep (the exe must stay ours).
            // A fork elsewhere in the test process can still hold the copy's
            // write descriptor for a moment (ETXTBSY): retry.
            let child = (0..50)
                .find_map(|_| {
                    std::process::Command::new(&exe)
                        .args(["-c", "sleep 300; :", arg])
                        .spawn()
                        .map_err(|_| std::thread::sleep(std::time::Duration::from_millis(20)))
                        .ok()
                })
                .expect("spawn the fake daemon");
            // Wait until the kernel shows it running the rig's binary.
            for _ in 0..100 {
                if std::fs::read_link(format!("/proc/{}/exe", child.id())).is_ok_and(|p| p == exe) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            FakeProc(child)
        }
        fn flag(&self, pid: u32) {
            std::fs::write(self.root.join("run/fs-freeze"), pid.to_string()).unwrap();
        }
    }

    struct FakeProc(std::process::Child);
    impl FakeProc {
        fn pid(&self) -> u32 {
            self.0.id()
        }
    }
    impl Drop for FakeProc {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A listener on `ip:port`, waiting while another test holds it.
    fn bind_or_wait(ip: &str, port: u16) -> std::net::TcpListener {
        (0..1500)
            .find_map(|_| {
                std::net::TcpListener::bind((ip, port))
                    .map_err(|_| std::thread::sleep(std::time::Duration::from_millis(100)))
                    .ok()
            })
            .unwrap_or_else(|| panic!("could not bind {ip}:{port}"))
    }

    impl Drop for FenceRig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn fence_rig_usable() -> bool {
        ["bash", "flock"].iter().all(|b| {
            std::process::Command::new("sh")
                .args(["-c", &format!("command -v {b}")])
                .output()
                .is_ok_and(|o| o.status.success())
        })
    }

    #[test]
    fn fence_peer_baseline_gateway_reachable_survives() {
        if !fence_rig_usable() {
            return;
        }
        let rig = FenceRig::new("base");
        assert_eq!(rig.run(5), 4, "{}", rig.log());
        rig.set("ping_rc", "1");
        assert_eq!(rig.run(5), 1, "{}", rig.log());
        assert!(rig.marker().exists(), "an isolated node records its yield");
    }

    #[test]
    fn fence_peer_arbitrated_keep_intent_keeps_serving() {
        if !fence_rig_usable() {
            return;
        }
        // Arbitrated, gateway down, peer reachable: without a keep intent the
        // node yields. With the daemon's mutual-freeze keep intent bound to
        // this node's generation it keeps serving, before any yield rule (a
        // run that waited on the lock while the daemon resumed must not freeze
        // the keeper again), and only while the intent names this node's
        // current or bitmap generation.
        let rig = FenceRig::new("keepintent");
        std::fs::write(rig.root.join("etc/direct-peer"), "127.0.0.253\n").unwrap();
        let intent = rig.root.join("run/arb-mutual-keep");
        rig.set("ping_rc", "1");
        rig.set("susp", "user");
        std::fs::write(rig.marker(), "yielded\n").unwrap();
        // Bound to the current generation (before the keeper's new generation).
        std::fs::write(&intent, "711F99C562B81B52\n").unwrap();
        rig.set(
            "gi",
            "711F99C562B81B53:0000000000000000:264E6B831F610398:0:1:1:1:1:0:0:0:1:0:1:0:1",
        );
        assert_eq!(rig.run(5), 4, "{}", rig.log());
        assert!(rig.log().contains("keep_win"), "{}", rig.log());
        // Bound to the bitmap generation (after it).
        rig.set(
            "gi",
            "06B424565A635FA7:711F99C562B81B53:264E6B831F610398:0:1:1:1:1:0:0:0:1:0:1:0:1",
        );
        assert_eq!(rig.run(5), 4, "{}", rig.log());
        assert_eq!(rig.log().matches("keep_win").count(), 2, "{}", rig.log());
        // An intent from an earlier episode (the pair resynced since): the
        // yield in force stands.
        rig.set(
            "gi",
            "38DD10FC3F5DEEFF:0000000000000000:06B424565A635FA6:711F99C562B81B52:1:1:1:1:0:0:0:1:0:1:0:1",
        );
        assert_eq!(rig.run(5), 1, "{}", rig.log());
        assert_eq!(rig.log().matches("keep_win").count(), 2, "{}", rig.log());
        // Fate-shared (no direct link): never.
        std::fs::remove_file(rig.root.join("etc/direct-peer")).unwrap();
        rig.set(
            "gi",
            "711F99C562B81B53:0000000000000000:264E6B831F610398:0:1:1:1:1:0:0:0:1:0:1:0:1",
        );
        let _ = rig.run(5);
        assert_eq!(rig.log().matches("keep_win").count(), 2, "{}", rig.log());
    }

    #[test]
    fn fence_peer_diskless_node_keeps_the_peer_portal() {
        if !fence_rig_usable() {
            return;
        }
        // Fate-shared (no direct link), Primary: an UpToDate node fences the
        // path through the peer's portal; a Diskless one holds no copy, so
        // that path is its guests' only one and stays.
        let rig = FenceRig::new("diskless");
        let fenced = rig.root.join("run/peer-portal-fenced");
        rig.set("dstate", "Diskless/UpToDate");
        let _ = rig.run(5);
        assert!(!fenced.exists(), "{}", rig.log());
        assert!(rig.log().contains("Diskless"), "{}", rig.log());
        rig.set("dstate", "UpToDate/UpToDate");
        let _ = rig.run(5);
        assert!(fenced.exists(), "{}", rig.log());
    }

    #[test]
    fn fence_peer_suspend_is_judged_by_the_live_state_not_the_exit_code() {
        // Called from inside the handler, suspend-io can apply the suspend
        // within seconds yet not return before its 10 s timeout, and a retry
        // then fails on the suspend already in place. Without checking the
        // live state every yield would be logged as a failed suspend, late.
        if !fence_rig_usable() {
            return;
        }
        let rig = FenceRig::new("susphang");
        rig.set("ping_rc", "1");
        rig.set("suspend_hangs", "");
        let t0 = std::time::Instant::now();
        assert_eq!(rig.run(5), 1, "{}", rig.log());
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(5),
            "took {:?}: {}",
            t0.elapsed(),
            rig.log()
        );
        assert!(rig.marker().exists(), "{}", rig.log());
        assert!(!rig.log().contains("suspend-io failed"), "{}", rig.log());
        // A suspend that really fails is still reported.
        let rig = FenceRig::new("suspfail");
        rig.set("ping_rc", "1");
        rig.set("suspend_rc", "20");
        assert_eq!(rig.run(5), 1, "{}", rig.log());
        assert!(
            rig.log().contains("suspend-io failed (rc=20"),
            "{}",
            rig.log()
        );
        assert!(rig.marker().exists(), "{}", rig.log());
    }

    #[test]
    fn fence_peer_freeze_yields_to_live_peer_over_keeper() {
        // A cut shorter than the handler: the link is back, the peer's DRBD
        // listens and its portal answers, DRBD sits StandAlone. Without freeze
        // mode the lower hostname keeps serving; in freeze mode it yields
        // frozen so where the guests ran decides.
        if !fence_rig_usable() {
            return;
        }
        // Freeze mode probes the peer's beacon (7900), not its portal (3260,
        // fenced by this same handler on a fate-shared pool): no portal here.
        // A loopback address of its own, so no other test's beacon answers.
        let beacon = bind_or_wait("127.0.0.8", 7900);
        let drbd = bind_or_wait("127.0.0.8", 0);
        let _keep = (&drbd, &beacon);
        let rig = FenceRig::new("fzkeeper");
        std::fs::write(
            rig.root.join("etc/twinstor.toml"),
            format!(
                "peer_address = \"127.0.0.8\"\ndrbd_port = {}\n",
                drbd.local_addr().unwrap().port()
            ),
        )
        .unwrap();
        let shim = rig.root.join("bin/drbdsetup");
        std::fs::write(&shim, "#!/bin/sh\necho 'exists connection name:twinstor-data peer-node-id:1 conn-name:twinstor-2 connection:StandAlone'\necho 'exists path name:twinstor-data peer-node-id:1 conn-name:twinstor-2 local:ipv4:127.0.0.1:7789 peer:ipv4:127.0.0.8:7789 established:no'\n").unwrap();
        rig.set("hostname", "twinstor-1");
        assert_eq!(rig.run(60), 4, "{}", rig.log());
        assert!(rig.log().contains("tiebreak KEEPER"), "{}", rig.log());
        // A flag left by a daemon that is gone counts for nothing.
        std::fs::write(rig.root.join("run/fs-freeze"), "999999999").unwrap();
        assert_eq!(rig.run(60), 4, "{}", rig.log());
        let _ = std::fs::remove_file(rig.root.join("run/fs-freeze"));
        // The non-keeper without freeze mode still needs the portal: none
        // answers here, so it survives.
        rig.set("hostname", "twinstor-3");
        assert_eq!(rig.run(60), 4, "{}", rig.log());
        assert!(rig.log().contains("port 3260"), "{}", rig.log());
        rig.set("hostname", "twinstor-1");
        let daemon = rig.fake_process("daemon");
        rig.flag(daemon.pid());
        rig.set("susp", "fencing");
        assert_eq!(rig.run(60), 1, "{}", rig.log());
        assert!(
            rig.marker().exists(),
            "the yield is recorded: {}",
            rig.log()
        );
        assert!(rig.log().contains("freeze mode"), "{}", rig.log());
    }

    #[test]
    fn fence_peer_freeze_yields_when_gateway_answers_after_lost_probe() {
        // The cut node's handler started during the cut: its first echo is
        // lost, the next ones answer once the link is back. Without freeze
        // mode it survives as before; in freeze mode it yields frozen.
        if !fence_rig_usable() {
            return;
        }
        let rig = FenceRig::new("fzlateping");
        let st = rig.root.join("state");
        let shim = rig.root.join("bin/ping");
        std::fs::write(&shim, format!(
            "#!/bin/sh\nif [ -f {s}/first_lost ]; then rm -f {s}/first_lost; exit 1; fi\nexit 0\n",
            s = st.display())).unwrap();
        std::fs::write(st.join("first_lost"), "").unwrap();
        assert_eq!(rig.run(60), 4, "{}", rig.log());
        assert!(!rig.marker().exists());
        let daemon = rig.fake_process("daemon");
        rig.flag(daemon.pid());
        // No live peer daemon (nothing listens on this address's beacon):
        // yielding would leave no one to decide, so it survives.
        std::fs::write(
            rig.root.join("etc/twinstor.toml"),
            "peer_address = \"127.0.0.9\"\ndrbd_port = 1\n",
        )
        .unwrap();
        std::fs::write(st.join("first_lost"), "").unwrap();
        assert_eq!(rig.run(60), 4, "{}", rig.log());
        assert!(!rig.marker().exists());
        let beacon = bind_or_wait("127.0.0.7", 7900);
        let _keep = &beacon;
        std::fs::write(
            rig.root.join("etc/twinstor.toml"),
            "peer_address = \"127.0.0.7\"\ndrbd_port = 1\n",
        )
        .unwrap();
        std::fs::write(st.join("first_lost"), "").unwrap();
        assert_eq!(rig.run(60), 1, "{}", rig.log());
        assert!(
            rig.marker().exists(),
            "the yield is recorded: {}",
            rig.log()
        );
        assert!(
            rig.log().contains("network just came back"),
            "{}",
            rig.log()
        );
        // A gateway that answers at once: survive, freeze mode or not.
        let _ = std::fs::remove_file(rig.marker());
        rig.set("susp", "fencing");
        assert_eq!(rig.run(60), 4, "{}", rig.log());
    }

    #[test]
    fn fence_peer_freeze_mode_needs_the_running_daemon_itself() {
        if !fence_rig_usable() {
            return;
        }
        let rig = FenceRig::new("fzpid");
        assert_eq!(rig.run(5), 4, "{}", rig.log());
        assert_eq!(rig.last_freeze(), "0", "no flag: {}", rig.log());
        let daemon = rig.fake_process("daemon");
        rig.flag(daemon.pid());
        rig.run(5);
        assert_eq!(rig.last_freeze(), "1", "the daemon's PID: {}", rig.log());
        // An upgrade replaced the binary and has not restarted the daemon yet:
        // the kernel names it "<path> (deleted)", and it is still the daemon.
        std::fs::remove_file(rig.daemon_exe()).unwrap();
        assert!(
            std::fs::read_link(format!("/proc/{}/exe", daemon.pid()))
                .unwrap()
                .to_string_lossy()
                .ends_with(" (deleted)")
        );
        rig.run(5);
        assert_eq!(rig.last_freeze(), "1", "a replaced binary: {}", rig.log());
        drop(daemon);
        // The CLI runs the same binary, without `daemon`.
        let cli = rig.fake_process("status");
        rig.flag(cli.pid());
        rig.run(5);
        assert_eq!(
            rig.last_freeze(),
            "0",
            "the CLI reusing the PID: {}",
            rig.log()
        );
        // Another program reusing the PID, and a PID that is gone.
        rig.flag(std::process::id());
        rig.run(5);
        assert_eq!(rig.last_freeze(), "0", "another binary: {}", rig.log());
        rig.flag(999_999_999);
        rig.run(5);
        assert_eq!(rig.last_freeze(), "0", "a gone PID: {}", rig.log());
        // Arbitrated (a direct-link peer): never freeze mode, whatever the flag.
        let daemon = rig.fake_process("daemon");
        rig.flag(daemon.pid());
        std::fs::write(rig.root.join("etc/direct-peer"), "127.0.0.253\n").unwrap();
        rig.run(5);
        assert_eq!(rig.last_freeze(), "0", "arbitrated: {}", rig.log());
    }

    #[test]
    fn fence_peer_freeze_waits_for_live_peer_with_closed_drbd_port() {
        // Both copies StandAlone after an aborted handshake: the peer's daemon
        // answers but its DRBD port opens only when its daemon reconnects.
        // Surviving on the closed port while the peer's run does the same
        // makes two writers; in freeze mode the handler waits for the listener
        // and then yields to the live peer.
        if !fence_rig_usable() {
            return;
        }
        let beacon = bind_or_wait("127.0.0.6", 7900);
        let drbd_port = bind_or_wait("127.0.0.6", 0).local_addr().unwrap().port();
        let _keep = &beacon;
        let rig = FenceRig::new("fzclosed");
        std::fs::write(
            rig.root.join("etc/twinstor.toml"),
            format!("peer_address = \"127.0.0.6\"\ndrbd_port = {drbd_port}\n"),
        )
        .unwrap();
        std::fs::write(rig.root.join("bin/drbdsetup"), "#!/bin/sh\necho 'exists connection name:twinstor-data peer-node-id:1 conn-name:twinstor-2 connection:StandAlone'\necho 'exists path name:twinstor-data peer-node-id:1 conn-name:twinstor-2 local:ipv4:127.0.0.1:7789 peer:ipv4:127.0.0.6:7789 established:no'\n").unwrap();
        rig.set("hostname", "twinstor-1");
        // Without freeze mode: the closed port reads as a gone peer, survive.
        assert_eq!(rig.run(60), 4, "{}", rig.log());
        assert!(!rig.log().contains("waiting up to"), "{}", rig.log());
        let daemon = rig.fake_process("daemon");
        rig.flag(daemon.pid());
        // The port never opens: survive after the wait.
        assert_eq!(rig.run(60), 4, "{}", rig.log());
        assert!(rig.log().contains("waiting up to 6s"), "{}", rig.log());
        assert!(!rig.marker().exists());
        // The peer's daemon reconnects 2 s in: its port opens, yield frozen.
        let opener = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(2));
            bind_or_wait("127.0.0.6", drbd_port)
        });
        let waits = rig.log().matches("waiting up to 6s").count();
        assert_eq!(rig.run(60), 1, "{}", rig.log());
        assert!(
            rig.marker().exists(),
            "the yield is recorded: {}",
            rig.log()
        );
        assert_eq!(
            rig.log().matches("waiting up to 6s").count(),
            waits + 1,
            "{}",
            rig.log()
        );
        let log = rig.log();
        let last: Vec<&str> = log.lines().rev().take(2).collect();
        assert!(last[0].contains("verdict=tiebreak_yield exit=1"), "{log}");
        assert!(last[1].contains("freeze mode"), "{log}");
        // The run took the wait and the defusal (over 15 s); the marker is
        // dated from its start, where the freeze budget must count from.
        let age = std::fs::metadata(rig.marker())
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .unwrap();
        assert!(
            age >= std::time::Duration::from_secs(15),
            "marker dated {age:?} ago: {}",
            rig.log()
        );
        drop(opener.join().unwrap());
        // A peer whose daemon is silent too (dead or still cut off) costs no wait.
        let _ = std::fs::remove_file(rig.marker());
        rig.set("susp", "fencing");
        std::fs::write(
            rig.root.join("etc/twinstor.toml"),
            "peer_address = \"127.0.0.9\"\ndrbd_port = 1\n",
        )
        .unwrap();
        let before = rig.log().matches("waiting up to").count();
        assert_eq!(rig.run(60), 4, "{}", rig.log());
        assert_eq!(
            rig.log().matches("waiting up to").count(),
            before,
            "{}",
            rig.log()
        );
    }

    /// A rig whose DRBD (events2) names the peer `twinstor-2` at `ip`,
    /// StandAlone, with the toml pointing at `ip` and `drbd_port`.
    fn lossy_rig(tag: &str, ip: &str, drbd_port: u16) -> FenceRig {
        let rig = FenceRig::new(tag);
        std::fs::write(
            rig.root.join("etc/twinstor.toml"),
            format!("peer_address = \"{ip}\"\ndrbd_port = {drbd_port}\n"),
        )
        .unwrap();
        std::fs::write(rig.root.join("bin/drbdsetup"), format!("#!/bin/sh\necho 'exists connection name:twinstor-data peer-node-id:1 conn-name:twinstor-2 connection:StandAlone'\necho 'exists path name:twinstor-data peer-node-id:1 conn-name:twinstor-2 local:ipv4:127.0.0.1:7789 peer:ipv4:{ip}:7789 established:no'\n")).unwrap();
        rig
    }

    fn run_window(rig: &FenceRig, secs: u32) -> i32 {
        rig.command(90)
            .env("TWINSTOR_PEER_SILENT_SECS", secs.to_string())
            .status()
            .unwrap()
            .code()
            .unwrap()
    }

    #[test]
    fn fence_peer_non_keeper_rides_out_a_lossy_probe() {
        // Heavy loss on both hosts: the first probe of the live peer's DRBD
        // port is lost. Both runs reading the peer as gone and surviving made
        // two writers. On arbitrated the non-keeper probes for a window and
        // yields to the keeper it then sees; the keeper still decides on one
        // probe; a peer silent for the whole window is gone.
        if !fence_rig_usable() {
            return;
        }
        let drbd_port = bind_or_wait("127.0.0.5", 0).local_addr().unwrap().port();
        let rig = lossy_rig("lossy", "127.0.0.5", drbd_port);
        std::fs::write(rig.root.join("etc/direct-peer"), "127.0.0.253\n").unwrap();
        // Keeper, nothing answers: survives at once, no window.
        rig.set("hostname", "twinstor-1");
        let t0 = std::time::Instant::now();
        assert_eq!(run_window(&rig, 6), 4, "{}", rig.log());
        assert!(!rig.log().contains("probing it"), "{}", rig.log());
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(5),
            "{}",
            rig.log()
        );
        // Non-keeper, nothing answers for the window: the peer is gone.
        rig.set("hostname", "twinstor-3");
        let t0 = std::time::Instant::now();
        assert_eq!(run_window(&rig, 2), 4, "{}", rig.log());
        assert!(rig.log().contains("silent for 2s"), "{}", rig.log());
        assert!(
            t0.elapsed() >= std::time::Duration::from_secs(1),
            "{}",
            rig.log()
        );
        assert!(!rig.marker().exists());
        // Non-keeper, the keeper's DRBD port and portal answer 2 s in (the
        // first probes lost): yield frozen to it.
        let opener = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(2));
            (
                bind_or_wait("127.0.0.5", drbd_port),
                bind_or_wait("127.0.0.5", 3260),
            )
        });
        assert_eq!(run_window(&rig, 8), 1, "{}", rig.log());
        assert!(rig.marker().exists(), "{}", rig.log());
        let log = rig.log();
        assert!(log.contains("answered within the window"), "{log}");
        assert!(
            log.lines()
                .last()
                .unwrap()
                .contains("verdict=tiebreak_yield exit=1"),
            "{log}"
        );
        drop(opener.join().unwrap());
        // Freeze mode (fate-shared, no keeper at the tiebreak) and
        // resource-only: no window, the decision is the 0.7.3 one.
        std::fs::remove_file(rig.root.join("etc/direct-peer")).unwrap();
        let _ = std::fs::remove_file(rig.marker());
        rig.set("susp", "fencing");
        let daemon = rig.fake_process("daemon");
        rig.flag(daemon.pid());
        assert_eq!(run_window(&rig, 6), 4, "{}", rig.log());
        assert_eq!(rig.last_freeze(), "1", "{}", rig.log());
        drop(daemon);
        assert_eq!(run_window(&rig, 6), 4, "{}", rig.log());
        let tail: String = rig.log().lines().rev().take(8).collect();
        assert!(!tail.contains("probing it"), "{tail}");
    }

    #[test]
    fn fence_peer_arbitrated_non_keeper_retries_the_portal_proof() {
        // Arbitrated, heavy loss: the non-keeper sees the peer's DRBD port,
        // but its probes of the keeper's portal are lost at first. One pass
        // read the keeper as gray and survived beside it; the retry yields.
        if !fence_rig_usable() {
            return;
        }
        let drbd = bind_or_wait("127.0.0.4", 0);
        let rig = lossy_rig("arbportal", "127.0.0.4", drbd.local_addr().unwrap().port());
        std::fs::write(rig.root.join("etc/direct-peer"), "127.0.0.253\n").unwrap();
        rig.set("hostname", "twinstor-3");
        let opener = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(6));
            bind_or_wait("127.0.0.4", 3260)
        });
        assert_eq!(run_window(&rig, 8), 1, "{}", rig.log());
        assert!(
            rig.log()
                .lines()
                .last()
                .unwrap()
                .contains("verdict=tiebreak_yield exit=1"),
            "{}",
            rig.log()
        );
        let portal = opener.join().unwrap();
        // A portal that never answers in the window: gray peer, survive.
        drop(portal);
        let _ = std::fs::remove_file(rig.marker());
        rig.set("susp", "fencing");
        assert_eq!(run_window(&rig, 2), 4, "{}", rig.log());
        assert!(rig.log().contains("probed for 2s"), "{}", rig.log());
        drop(drbd);
    }

    #[test]
    fn fence_peer_arbitrated_non_keeper_reads_a_serving_portal_as_alive() {
        // After a lossy disconnect the keeper's DRBD can stay in teardown for
        // half a minute, its port closed, while it already serves. The
        // non-keeper must not read that as a dead peer and survive beside it:
        // the keeper's portal answers, so it goes to the tiebreak and yields.
        if !fence_rig_usable() {
            return;
        }
        let rig = lossy_rig("arbportalgate", "127.0.0.3", 1);
        std::fs::write(rig.root.join("etc/direct-peer"), "127.0.0.253\n").unwrap();
        rig.set("hostname", "twinstor-3");
        let portal = bind_or_wait("127.0.0.3", 3260);
        assert_eq!(run_window(&rig, 6), 1, "{}", rig.log());
        let log = rig.log();
        assert!(
            log.contains("iSCSI portal answered within the window"),
            "{log}"
        );
        assert!(
            log.lines()
                .last()
                .unwrap()
                .contains("verdict=tiebreak_yield exit=1"),
            "{log}"
        );
        // Target down too (a peer that serves nobody): gone, survive.
        drop(portal);
        let _ = std::fs::remove_file(rig.marker());
        rig.set("susp", "fencing");
        assert_eq!(run_window(&rig, 2), 4, "{}", rig.log());
        assert!(rig.log().contains("silent for 2s"), "{}", rig.log());
    }

    #[test]
    fn fence_peer_keeps_a_yield_in_force() {
        // An earlier run yielded (suspend-io and marker); the next run finds
        // the gateway reachable and the peer's listener closed
        // (it is StandAlone). Deciding afresh would resume I/O beside the
        // serving peer; the yield must hold.
        if !fence_rig_usable() {
            return;
        }
        let rig = FenceRig::new("inherit");
        std::fs::write(rig.marker(), "earlier").unwrap();
        rig.set("susp", "user,fencing");
        assert_eq!(rig.run(5), 1, "{}", rig.log());
        assert!(rig.log().contains("still in force"), "{}", rig.log());
        // A marker whose suspend was lifted is history, not a verdict.
        rig.set("susp", "no");
        assert_eq!(rig.run(5), 4, "{}", rig.log());
        // Promote-time runs keep their own guard.
        rig.set("susp", "user");
        rig.set("role", "Secondary");
        assert_eq!(rig.run(5), 4, "{}", rig.log());
    }

    #[test]
    fn fence_peer_keeps_a_yield_when_its_state_cannot_be_read() {
        // Deciding afresh on an unreadable state could resume this node's I/O
        // beside the serving peer: with a marker present, keep the yield.
        if !fence_rig_usable() {
            return;
        }
        let rig = FenceRig::new("failclosed");
        std::fs::write(rig.marker(), "earlier").unwrap();
        rig.set("status_fails", "1");
        assert_eq!(rig.run(5), 1, "{}", rig.log());
        assert!(
            rig.log().contains("cannot be read -> keep the yield"),
            "{}",
            rig.log()
        );
        std::fs::remove_file(rig.root.join("state/status_fails")).unwrap();
        rig.set("susp", "no");
        rig.set("role_fails", "1");
        assert_eq!(rig.run(5), 1, "{}", rig.log());
        // Without a marker an unreadable state changes nothing.
        std::fs::remove_file(rig.marker()).unwrap();
        assert_eq!(rig.run(5), 4, "{}", rig.log());
    }

    #[test]
    fn fence_peer_second_run_waits_for_the_first_and_inherits_its_yield() {
        if !fence_rig_usable() {
            return;
        }
        let rig = FenceRig::new("serial");
        // Stand-in for the first run: holds the handler's lock while it
        // decides, then yields the way the handler does.
        let st = rig.root.join("state");
        let mut first = std::process::Command::new("flock")
            .arg(rig.lock())
            .args([
                "sh",
                "-c",
                &format!(
                    "touch {ready}; sleep 2; echo user > {susp}; date > {marker}",
                    ready = st.join("first-holds").display(),
                    susp = st.join("susp").display(),
                    marker = rig.marker().display(),
                ),
            ])
            .spawn()
            .unwrap();
        while !st.join("first-holds").exists() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let code = rig.run(20);
        first.wait().unwrap();
        assert_eq!(code, 1, "{}", rig.log());
        assert!(rig.log().contains("waiting for it"), "{}", rig.log());
        assert!(rig.log().contains("still in force"), "{}", rig.log());
    }

    #[test]
    fn fence_peer_lock_timeout_freezes_without_outdating() {
        if !fence_rig_usable() {
            return;
        }
        let rig = FenceRig::new("timeout");
        let st = rig.root.join("state");
        let mut holder = std::process::Command::new("flock")
            .arg(rig.lock())
            .args([
                "sh",
                "-c",
                &format!("touch {}; sleep 4", st.join("held").display()),
            ])
            .spawn()
            .unwrap();
        while !st.join("held").exists() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let code = rig.run(1);
        holder.wait().unwrap();
        assert_eq!(code, 1, "{}", rig.log());
        assert!(!rig.marker().exists(), "a timed-out run records no yield");
        let calls = std::fs::read_to_string(st.join("drbdadm.calls")).unwrap_or_default();
        assert!(!calls.contains("suspend-io"), "{calls}");
        // The lock dies with its holder: the next run decides normally.
        assert_eq!(rig.run(1), 4, "{}", rig.log());
    }

    #[test]
    fn static_artifacts_are_nonempty_and_absolute() {
        for (path, content) in static_artifacts() {
            assert!(path.starts_with('/'), "{path} not absolute");
            assert!(!content.is_empty(), "{path} embedded content empty");
        }
    }
    use super::{
        SharedSetup, chap_check_value, drbd_topology, ha_timeout_secs, iqn_is_lio_compatible,
        patch_lvm_global_filter, peer_uuid_from_pool, resync_fill_target, resync_min_rate,
        sanitize_iqn,
    };
    use super::{tiebreaker_gate, validate_local_name_matches_xapi, validate_node_names};

    #[test]
    fn duplicate_hostnames_are_refused() {
        // Duplicate names collide in the DRBD on-sections, the sorted node-id
        // assignment and the keeper tiebreak, and the failure would show up at
        // the next reboot. Setup must refuse before generating any topology.
        assert!(validate_node_names("xcp-ng", "xcp-ng").is_err());
        let err = validate_node_names("xcp-ng", "xcp-ng")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("host-set-hostname-live"),
            "error must carry the remedy: {err}"
        );
        assert!(validate_node_names("node-a", "node-b").is_ok());
    }

    #[test]
    fn uname_vs_xapi_hostname_drift_is_refused() {
        // The peer resolves this host via XAPI while the local .res uses
        // uname; drift between them writes configs that disagree about this
        // node's name.
        assert!(validate_local_name_matches_xapi("node-a", "node-a-renamed").is_err());
        assert!(validate_local_name_matches_xapi("node-a", "node-a").is_ok());
    }

    #[test]
    fn zero_tiebreaker_deployment_is_refused_unless_overridden() {
        // With nothing to probe, the fence-peer freezes the healthy survivor
        // on a plain peer death: a one-node failure becomes a full-pool outage.
        assert!(tiebreaker_gate(false, false, false).is_err());
        let err = tiebreaker_gate(false, false, false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("--allow-no-tiebreaker"),
            "error must name the override: {err}"
        );
        // Any tiebreaker source, or the explicit override, passes.
        assert!(tiebreaker_gate(true, false, false).is_ok());
        assert!(tiebreaker_gate(false, true, false).is_ok());
        assert!(tiebreaker_gate(false, false, true).is_ok());
    }

    fn ip(s: &str) -> std::net::IpAddr {
        s.parse().unwrap()
    }

    fn uuids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn peer_is_the_single_other_pool_host() {
        let all = uuids(&["local-uuid", "peer-uuid"]);
        assert_eq!(
            peer_uuid_from_pool(Some("local-uuid"), &all),
            Some("peer-uuid".to_string())
        );
    }

    #[test]
    fn peer_none_when_local_is_only_host() {
        let all = uuids(&["local-uuid"]);
        assert_eq!(peer_uuid_from_pool(Some("local-uuid"), &all), None);
    }

    #[test]
    fn peer_none_when_more_than_one_other() {
        // A pool of three or more hosts is ambiguous: never guess.
        let all = uuids(&["local-uuid", "peer-a", "peer-b"]);
        assert_eq!(peer_uuid_from_pool(Some("local-uuid"), &all), None);
    }

    #[test]
    fn peer_none_when_local_uuid_unknown() {
        let all = uuids(&["a", "b"]);
        assert_eq!(peer_uuid_from_pool(None, &all), None);
    }

    #[test]
    fn shared_setup_round_trips_fate_shared() {
        let s = SharedSetup {
            arbitrated: false,
            peer_ip: ip("192.0.2.20"),
            direct_ip: None,
            ping_targets: vec![ip("192.0.2.1"), ip("192.0.2.2")],
            full_sync: false,
            drbd_port: 7789,
            chap_user: "twinstor".to_string(),
            publisher_uuid: "5e5e0004-0000-4000-8000-000000000004".to_string(),
            chap_check: Some(chap_check_value("s3cret")),
        };
        assert_eq!(SharedSetup::decode(&s.encode()), Some(s));
    }

    #[test]
    fn shared_setup_round_trips_arbitrated_with_empties() {
        let s = SharedSetup {
            arbitrated: true,
            peer_ip: ip("10.0.0.1"),
            direct_ip: Some(ip("198.51.100.1")),
            ping_targets: vec![],
            full_sync: true,
            drbd_port: 7790,
            chap_user: "twinstor".to_string(),
            publisher_uuid: "abc".to_string(),
            chap_check: None,
        };
        assert_eq!(SharedSetup::decode(&s.encode()), Some(s));
    }

    #[test]
    fn shared_setup_decode_rejects_foreign_and_malformed() {
        assert_eq!(SharedSetup::decode(""), None);
        assert_eq!(SharedSetup::decode("garbage"), None);
        // Missing version tag (some other other-config value, for example).
        assert_eq!(SharedSetup::decode("peer_ip=1.2.3.4;drbd_port=7789"), None);
        // Wrong or future version.
        assert_eq!(SharedSetup::decode("v=2;peer_ip=1.2.3.4"), None);
    }

    #[test]
    fn ha_timeout_by_mode() {
        // Fate-shared: a split must heal within it or XHA reboots a host.
        assert_eq!(ha_timeout_secs(false), 120);
        assert_eq!(ha_timeout_secs(true), 90); // arbitrated: covers a switch-fault reconnect
    }

    #[test]
    fn topology_single_path_fate_shared_node_ids() {
        // New setup: a fate-shared pool writes explicit sorted-hostname node-ids
        // so DRBD does not auto-assign ids that mismatch hostname order.
        let with_ids = drbd_topology(
            "node-a",
            "node-b",
            "/dev/drbd0",
            "/dev/disk/by-id/x",
            "192.0.2.10".parse().unwrap(),
            "192.0.2.11".parse().unwrap(),
            7789,
            None,
            Some((0, 1)),
        );
        assert!(with_ids.contains("address 192.0.2.10:7789;"));
        assert!(with_ids.contains("disk \"/dev/disk/by-id/x\";"));
        assert!(
            !with_ids.contains("connection"),
            "still single-path, no explicit connection"
        );
        assert!(!with_ids.contains("load-balance-paths"));
        assert!(
            with_ids.contains("on node-a {\n        node-id 0;"),
            "node-a must be node-id 0"
        );
        assert!(
            with_ids.contains("on node-b {\n        node-id 1;"),
            "node-b must be node-id 1"
        );
        // regenerate-config on an id-less pool passes None and stays id-less, so
        // it never injects an id that mismatches the on-disk metadata.
        let id_less = drbd_topology(
            "node-a",
            "node-b",
            "/dev/drbd0",
            "/dev/disk/by-id/x",
            "192.0.2.10".parse().unwrap(),
            "192.0.2.11".parse().unwrap(),
            7789,
            None,
            None,
        );
        assert!(!id_less.contains("node-id"));
        assert!(!id_less.contains("connection"));
    }

    #[test]
    fn topology_quotes_colon_bearing_md_backing() {
        // mdraid by-id aliases (md-uuid-..., md-name-host:N) contain colons.
        // Unquoted, DRBD parses the first colon as address:port and errors
        // ("';' expected, but got ':'"). Quoting keeps the whole path one token.
        let md = "/dev/disk/by-id/md-uuid-1c2f3a4b:5d6e7f80:91a2b3c4:d5e6f708";
        for direct in [
            None,
            Some((
                "198.51.100.1".parse().unwrap(),
                "198.51.100.2".parse().unwrap(),
            )),
        ] {
            let t = drbd_topology(
                "node-a",
                "node-b",
                "/dev/drbd0",
                md,
                "192.0.2.10".parse().unwrap(),
                "192.0.2.11".parse().unwrap(),
                7789,
                direct,
                Some((0, 1)),
            );
            assert!(
                t.contains(&format!("disk \"{md}\";")),
                "md backing must be quoted: {t}"
            );
            // The bare (unquoted) form must never appear.
            assert!(
                !t.contains(&format!("disk {md};")),
                "md backing must not be bare: {t}"
            );
        }
    }

    #[test]
    fn topology_direct_link_is_arbitrated() {
        let t = drbd_topology(
            "node-a",
            "node-b",
            "/dev/drbd0",
            "/dev/disk/by-id/x",
            "192.0.2.10".parse().unwrap(),
            "192.0.2.11".parse().unwrap(),
            7789,
            Some((
                "198.51.100.1".parse().unwrap(),
                "198.51.100.2".parse().unwrap(),
            )),
            None,
        );
        // Both the switched and the direct path are present, in active-backup
        // (two `path` blocks in one connection, no load-balance-paths). The
        // comment in the generated config names the directive, so assert on
        // the `load-balance-paths yes` setting, not the bare substring.
        assert!(t.contains("address 192.0.2.10:7789;"));
        assert!(t.contains("address 198.51.100.1:7789;"));
        assert!(t.contains("address 198.51.100.2:7789;"));
        assert!(!t.contains("load-balance-paths yes"));
        assert_eq!(t.matches("path {").count(), 2);
        // Addresses live in the paths, not the on-sections.
        assert!(!t.contains("disk /dev/disk/by-id/x; address"));
        // The direct path is declared first: replication belongs on the
        // dedicated cable in steady state, the switched path is the fallback.
        let direct_at = t.find("address 198.51.100.1:7789").unwrap();
        let switch_at = t.find("address 192.0.2.10:7789").unwrap();
        assert!(
            direct_at < switch_at,
            "direct path must be declared before the switched path"
        );
    }

    #[test]
    fn topology_node_ids_agree_across_both_nodes() {
        // Node A's view (local = node-a) and node B's view (local = node-b)
        // must assign the same node-id to each host, or the DRBD metadata is
        // inconsistent.
        let a = drbd_topology(
            "node-a",
            "node-b",
            "/dev/drbd0",
            "/d",
            "10.0.0.1".parse().unwrap(),
            "10.0.0.2".parse().unwrap(),
            7789,
            Some((
                "203.0.113.1".parse().unwrap(),
                "203.0.113.2".parse().unwrap(),
            )),
            None,
        );
        let b = drbd_topology(
            "node-b",
            "node-a",
            "/dev/drbd0",
            "/d",
            "10.0.0.2".parse().unwrap(),
            "10.0.0.1".parse().unwrap(),
            7789,
            Some((
                "203.0.113.2".parse().unwrap(),
                "203.0.113.1".parse().unwrap(),
            )),
            None,
        );
        for cfg in [&a, &b] {
            assert!(
                cfg.contains("on node-a {\n        node-id 0;"),
                "node-a must be node-id 0"
            );
            assert!(
                cfg.contains("on node-b {\n        node-id 1;"),
                "node-b must be node-id 1"
            );
        }
    }

    #[tokio::test]
    async fn zvol_alias_reverse_resolves_and_misses_cleanly() {
        let td = std::env::temp_dir().join("twinstor-test-zvol");
        let _ = std::fs::remove_dir_all(&td);
        std::fs::create_dir_all(td.join("dev")).unwrap();
        std::fs::create_dir_all(td.join("zvol/localzfs/nested")).unwrap();
        std::fs::write(td.join("dev/zd0"), b"").unwrap();
        std::fs::write(td.join("dev/zd16"), b"").unwrap();
        std::os::unix::fs::symlink("../../dev/zd0", td.join("zvol/localzfs/disk1")).unwrap();
        std::os::unix::fs::symlink("../../../dev/zd16", td.join("zvol/localzfs/nested/disk2"))
            .unwrap();

        let real0 = std::fs::canonicalize(td.join("dev/zd0")).unwrap();
        let hit = super::zvol_alias_for(&real0, &td.join("zvol")).await;
        assert_eq!(hit, Some(td.join("zvol/localzfs/disk1")));

        // Nested dataset resolves too.
        let real16 = std::fs::canonicalize(td.join("dev/zd16")).unwrap();
        let hit = super::zvol_alias_for(&real16, &td.join("zvol")).await;
        assert_eq!(hit, Some(td.join("zvol/localzfs/nested/disk2")));

        // A device with no alias returns None (caller refuses with guidance).
        let other = td.join("dev/zd32");
        std::fs::write(&other, b"").unwrap();
        let real32 = std::fs::canonicalize(&other).unwrap();
        assert_eq!(super::zvol_alias_for(&real32, &td.join("zvol")).await, None);
        let _ = std::fs::remove_dir_all(&td);
    }

    #[test]
    fn lvm_filter_patches_existing_line() {
        let conf = "devices {\n\tglobal_filter = [ \"r|^/dev/drbd.*|\" ]\n}\n";
        let out = patch_lvm_global_filter(conf, "nvme-MODEL_SERIAL123").unwrap();
        assert!(
            out.contains("global_filter = [ \"r|nvme-MODEL_SERIAL123|\", \"r|^/dev/drbd.*|\" ]")
        );
        // Idempotent: a second call is a no-op.
        assert!(patch_lvm_global_filter(&out, "nvme-MODEL_SERIAL123").is_none());
    }

    #[test]
    fn lvm_filter_inserts_when_absent() {
        let conf = "# comment\ndevices {\n\tdir = \"/dev\"\n}\n";
        let out = patch_lvm_global_filter(conf, "nvme-MODEL_SERIAL123").unwrap();
        assert!(out.contains("global_filter = [ \"r|nvme-MODEL_SERIAL123|\" ]"));
        // Inserted inside devices{}, right after the opening line.
        let dev_pos = out.find("devices {").unwrap();
        let filt_pos = out.find("global_filter").unwrap();
        assert!(filt_pos > dev_pos);
    }

    #[test]
    fn lvm_filter_ignores_commented_lines() {
        let conf = "devices {\n\t# global_filter = [ \"a|.*|\" ]\n\tglobal_filter = [ \"r|^/dev/drbd.*|\" ]\n}\n";
        let out = patch_lvm_global_filter(conf, "nvme-X_1").unwrap();
        // The comment is untouched, the active line is patched.
        assert!(out.contains("# global_filter = [ \"a|.*|\" ]"));
        assert!(out.contains("global_filter = [ \"r|nvme-X_1|\", \"r|^/dev/drbd.*|\" ]"));
    }

    #[test]
    fn resync_fill_target_scales_with_link() {
        assert_eq!(resync_fill_target(Some(25_000)), "10M");
        assert_eq!(resync_fill_target(Some(10_000)), "10M");
        assert_eq!(resync_fill_target(Some(2_500)), "1M");
        assert_eq!(resync_fill_target(Some(1_000)), "1M");
        assert_eq!(resync_fill_target(None), "1M");
    }

    #[test]
    fn resync_min_rate_scales_with_link_and_topology() {
        // Arbitrated (dedicated link): 80% of link. Fate-shared (shared): 50%.
        // Capped at c-max-rate (800M), floored at 10 MB/s.
        assert_eq!(resync_min_rate(Some(10_000), true), "800M"); // 1250*0.8=1000, capped at 800
        assert_eq!(resync_min_rate(Some(10_000), false), "625M"); // 1250*0.5
        assert_eq!(resync_min_rate(Some(1_000), true), "100M"); // 125*0.8
        assert_eq!(resync_min_rate(Some(1_000), false), "62M"); // 125*0.5
        assert_eq!(resync_min_rate(Some(40_000), true), "800M"); // 5000*0.8, capped at 800
        // Unknown link: assume 1 GbE, still topology-scaled.
        assert_eq!(resync_min_rate(None, true), "100M");
        assert_eq!(resync_min_rate(None, false), "62M");
        // Never below the 10 MB/s floor on a very slow link.
        assert_eq!(resync_min_rate(Some(100), false), "10M"); // 12*0.5=6, floored at 10
    }

    #[test]
    fn lio_iqn_validation() {
        // The shapes that matter: XCP-ng's standard form passes; the
        // single-label DHCP-domain form must fail.
        assert!(iqn_is_lio_compatible("iqn.2026-06.lab.vates:twinstor-1"));
        assert!(iqn_is_lio_compatible("iqn.1993-08.org.debian:01:abcdef"));
        assert!(iqn_is_lio_compatible(
            "iqn.2025-01.com.xcp-ng:twinstor-data"
        ));
        assert!(!iqn_is_lio_compatible("iqn.2026-06.home:371e5ce6"));
        assert!(!iqn_is_lio_compatible("iqn.26-06.com.example:x"));
        assert!(!iqn_is_lio_compatible("eui.02004567A425678D"));
        assert!(!iqn_is_lio_compatible("iqn.2026-06.:x"));
    }

    #[test]
    fn backing_wipe_guard_blocks_only_accidental_reruns() {
        use super::backing_wipe_blocked;
        // Fresh install or aborted run: no SR yet, so the wipe is safe.
        assert!(!backing_wipe_blocked(false, false));
        assert!(!backing_wipe_blocked(false, true));
        // Live SR and no rejoin intent: an accidental re-run, blocked.
        assert!(backing_wipe_blocked(true, false));
        // Live SR, but --full-sync signals an intended rejoin or re-init: allowed.
        assert!(!backing_wipe_blocked(true, true));
    }

    #[test]
    fn sanitize_iqn_repairs_single_label_authority() {
        // The main case: keep the date and unique id, fix the authority.
        let fixed = sanitize_iqn("iqn.2026-06.home:371e5ce6").unwrap();
        assert_eq!(fixed, "iqn.2026-06.home.lan:371e5ce6");
        assert!(iqn_is_lio_compatible(&fixed));
        // No unique id is fine too.
        assert_eq!(
            sanitize_iqn("iqn.2026-06.home").unwrap(),
            "iqn.2026-06.home.lan"
        );
    }

    #[test]
    fn sanitize_iqn_leaves_valid_and_unrepairable_alone() {
        // Already valid: no change.
        assert_eq!(sanitize_iqn("iqn.2026-06.com.example:host"), None);
        // Unfamiliar shapes: do not guess.
        assert_eq!(sanitize_iqn("eui.02004567A425678D"), None);
        assert_eq!(sanitize_iqn("iqn.26-06.home:x"), None);
        assert_eq!(sanitize_iqn("iqn.2026-06.:x"), None);
    }
}

/// Fields regenerate-config needs from an existing res file: the per-host
/// facts that live only there (the peer's backing path can differ from
/// ours) plus the topology addresses that must never change under a live
/// regeneration.
pub(crate) struct ParsedRes {
    pub local_backing: Option<String>,
    pub peer_backing: Option<String>,
    pub drbd_dev: Option<String>,
    pub addresses: Vec<String>,
}

/// Tolerant line-scan of a TwinStor-generated res file. Only the shapes our
/// own templates emit are recognized; anything else is not captured
/// (the caller refuses to proceed on missing fields).
pub(crate) fn parse_res_file(text: &str, local_name: &str, peer_name: &str) -> ParsedRes {
    let mut out = ParsedRes {
        local_backing: None,
        peer_backing: None,
        drbd_dev: None,
        addresses: Vec::new(),
    };
    let mut in_host: Option<bool> = None; // Some(true)=local section
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("on ") {
            let name = rest.trim_end_matches('{').trim();
            in_host = Some(name == local_name);
            if name != local_name && name != peer_name {
                in_host = None;
            }
        }
        if t.contains("device ") {
            // Statement-split on ';' so "meta-disk internal" never matches
            // and both the quoted (v1+) and unquoted (v0) disk forms parse:
            // the backing path itself contains the substring "disk"
            // (/dev/disk/by-id/...), so keyword splitting is unsafe.
            for seg in t.split(';') {
                let seg = seg.trim();
                if let Some(dev) = seg.strip_prefix("device ") {
                    out.drbd_dev = Some(dev.trim().to_string());
                } else if let Some(disk) = seg.strip_prefix("disk ") {
                    let disk = disk.trim().trim_matches('"').to_string();
                    if !disk.is_empty() {
                        match in_host {
                            Some(true) => out.local_backing = Some(disk),
                            Some(false) => out.peer_backing = Some(disk),
                            None => {}
                        }
                    }
                }
            }
        }
        if t.contains("address") {
            for w in t.split_whitespace() {
                if w.contains(':') && w.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                    out.addresses.push(w.trim_end_matches(';').to_string());
                }
            }
        }
    }
    out.addresses.sort();
    out.addresses.dedup();
    out
}

#[cfg(test)]
mod regen_tests {
    use super::parse_res_file;

    #[test]
    fn parse_res_extracts_backings_device_and_addresses() {
        let text = r#"# twinstor-config-version: 0
resource twinstor-data {
    on twinstor-1 {
        node-id 0;
        device /dev/drbd0; disk "/dev/disk/by-id/nvme-X"; meta-disk internal;
    }
    on twinstor-2 {
        node-id 1;
        device /dev/drbd0; disk "/dev/disk/by-id/nvme-Y"; meta-disk internal;
    }
    connection {
        path {
            host twinstor-2 address 198.51.100.2:7789;
            host twinstor-1 address 198.51.100.1:7789;
        }
        path {
            host twinstor-2 address 192.0.2.101:7789;
            host twinstor-1 address 192.0.2.102:7789;
        }
    }
}
"#;
        let p = parse_res_file(text, "twinstor-1", "twinstor-2");
        assert_eq!(p.local_backing.as_deref(), Some("/dev/disk/by-id/nvme-X"));
        assert_eq!(p.peer_backing.as_deref(), Some("/dev/disk/by-id/nvme-Y"));
        assert_eq!(p.drbd_dev.as_deref(), Some("/dev/drbd0"));
        assert_eq!(p.addresses.len(), 4);

        // Version 0 files have the disk path unquoted.
        let v0 = r#"    on twinstor-2 {
        device /dev/drbd0; disk /dev/disk/by-id/nvme-Z; meta-disk internal;
    }
"#;
        let p = parse_res_file(v0, "twinstor-2", "twinstor-1");
        assert_eq!(p.local_backing.as_deref(), Some("/dev/disk/by-id/nvme-Z"));
        assert_eq!(p.drbd_dev.as_deref(), Some("/dev/drbd0"));
    }
}

#[cfg(test)]
mod setup_flow_tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn sha256_matches_the_standard_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Two blocks: the padding spills into a second one.
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn chap_check_is_short_stable_and_discriminating() {
        let c = chap_check_value("Abcdef0123456789");
        assert_eq!(c.len(), 8);
        assert!(c.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_eq!(c, chap_check_value("Abcdef0123456789"));
        assert_ne!(c, chap_check_value("Abcdef012345678"), "a truncated paste");
        assert_ne!(c, chap_check_value("abcdef0123456789"), "a case slip");
    }

    #[test]
    fn chap_verdict_checks_only_when_a_value_was_published() {
        let check = chap_check_value("right");
        assert_eq!(chap_verdict("right", Some(&check)), ChapVerdict::Matches);
        assert_eq!(
            chap_verdict("right", Some(&check.to_uppercase())),
            ChapVerdict::Matches
        );
        assert_eq!(chap_verdict("wrong", Some(&check)), ChapVerdict::Differs);
        // A first node that predates the check value: accepted unchecked.
        assert_eq!(chap_verdict("anything", None), ChapVerdict::Unchecked);
    }

    #[test]
    fn pasted_chap_lines_are_cleaned() {
        assert_eq!(normalize_chap_paste("  Xy12  "), "Xy12");
        assert_eq!(normalize_chap_paste("CHAP password:  Xy12"), "Xy12");
        assert_eq!(normalize_chap_paste("chap password: \"Xy12\""), "Xy12");
        assert_eq!(normalize_chap_paste("'Xy12'"), "Xy12");
        assert_eq!(normalize_chap_paste(""), "");
        assert_eq!(normalize_chap_paste("CHAP password:"), "");
        // A password that merely starts like the label is kept whole.
        assert_eq!(normalize_chap_paste("chapXy12"), "chapXy12");
    }

    #[test]
    fn second_node_refuses_a_missing_or_disproved_password() {
        let check = chap_check_value("right");
        let err = second_node_chap_gate(None, Some(&check), "host-a").unwrap_err();
        let g = err
            .downcast_ref::<crate::render::CliError>()
            .expect("guidance");
        assert!(g.to_string().contains("host-a"), "{g}");
        let err = second_node_chap_gate(Some("wrong"), Some(&check), "host-a")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not the CHAP password host-a printed"),
            "{err}"
        );
        assert!(second_node_chap_gate(Some("right"), Some(&check), "host-a").is_ok());
        // No check value published (an older first node): only the empty
        // password is refused.
        assert!(second_node_chap_gate(Some("x"), None, "host-a").is_ok());
        assert!(second_node_chap_gate(None, None, "host-a").is_err());
    }

    #[test]
    fn handoff_without_a_check_value_still_decodes() {
        // As a first node before the check value existed published it.
        let old = "v=1;arbitrated=0;peer_ip=192.0.2.20;direct_ip=;ping=;full_sync=0;\
                   drbd_port=7789;chap_user=twinstor;publisher=abc";
        let s = SharedSetup::decode(old).expect("decodes");
        assert_eq!(s.chap_check, None);
    }

    #[test]
    fn iscsi_refusal_names_chap_and_the_service() {
        let e = iscsi_login_refused("host-a", "192.0.2.11".parse().unwrap());
        let text = format!("{e} {e:?}");
        assert!(text.contains("refused the iSCSI login"), "{text}");
        assert!(text.contains("CHAP password"), "{text}");
        assert!(text.contains("systemctl status twinstor"), "{text}");
    }

    #[test]
    fn rerun_guard_protects_a_live_pair() {
        // Fresh install, or one that stopped before the SR existed.
        assert_eq!(rerun_guard(false, false, false, false), RerunGuard::Proceed);
        assert_eq!(rerun_guard(false, false, true, true), RerunGuard::Proceed);
        // A live SR and no rebuild asked: an accidental re-run.
        assert_eq!(rerun_guard(true, false, true, true), RerunGuard::LiveSr);
        // A rebuild from a peer that holds the data.
        assert_eq!(rerun_guard(true, true, true, true), RerunGuard::Proceed);
        // A rebuild of the host that holds the only current copy.
        assert_eq!(
            rerun_guard(true, true, true, false),
            RerunGuard::OnlyCurrentCopy
        );
        // This host's copy is not current (down, or still syncing): the
        // rebuild is what it needs.
        assert_eq!(rerun_guard(true, true, false, false), RerunGuard::Proceed);
    }

    #[test]
    fn rerun_refusals_say_nothing_changed_and_the_way_out() {
        assert!(rerun_refusal(RerunGuard::Proceed, "host-a").is_none());
        let live = rerun_refusal(RerunGuard::LiveSr, "host-a").expect("refused");
        let text = format!("{live} {live:?}");
        assert!(text.contains("nothing was changed"), "{text}");
        assert!(text.contains("--full-sync"), "{text}");
        assert!(text.contains("twinstor status"), "{text}");
        let only = rerun_refusal(RerunGuard::OnlyCurrentCopy, "host-a").expect("refused");
        let text = format!("{only} {only:?}");
        assert!(text.contains("only current copy"), "{text}");
        assert!(text.contains("host-a"), "{text}");
    }

    #[test]
    fn a_failure_after_the_first_change_says_how_to_resume() {
        let e = part_way_failure(anyhow::anyhow!("wipefs failed"), true);
        let g = e
            .downcast_ref::<crate::render::CliError>()
            .expect("guidance attached");
        let text = format!("{g:?}");
        assert!(text.contains("twinstor setup"), "{text}");
        assert!(text.contains("twinstor uninstall"), "{text}");
        assert!(e.chain().any(|c| c.to_string() == "wipefs failed"));
        // Before any change, the error stays as it is.
        let e = part_way_failure(anyhow::anyhow!("bad flag"), false);
        assert!(e.downcast_ref::<crate::render::CliError>().is_none());
        // An error with its own guidance keeps it.
        let own: anyhow::Error = crate::render::CliError::new("refused", "do this").into();
        let e = part_way_failure(own, true);
        assert_eq!(e.to_string(), "refused");
    }

    #[test]
    fn full_copy_estimates_read_naturally() {
        // 500 GB at 1 Gb/s: about 83 min at 80 % of the link.
        assert_eq!(
            full_copy_estimate(500_000_000_000, 1000),
            "about 1 h 24 min"
        );
        assert_eq!(full_copy_estimate(1_000_000_000, 10_000), "about 1 min");
        assert_eq!(full_copy_estimate(3_600_000_000_000, 10_000), "about 1 h");
        // An unknown speed reported as 0 does not divide by zero.
        assert!(full_copy_estimate(1, 0).starts_with("about"));
    }

    #[test]
    fn the_wait_timeout_says_what_happens_next() {
        let t = peer_wait_timeout_text("host-b", PEER_WAIT_SECS, Some(500_000_000_000), Some(1000));
        assert!(t.contains("within 30 minutes"), "{t}");
        assert!(t.contains("serves alone"), "{t}");
        assert!(t.contains("500 GB at 1000 Mb/s"), "{t}");
        let t = peer_wait_timeout_text("host-b", PEER_WAIT_SECS, None, Some(1000));
        assert!(t.contains("hours for large disks"), "{t}");
    }
}
