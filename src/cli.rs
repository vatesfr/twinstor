use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::{info, warn};

use crate::config::Config;
use crate::drbd::DrbdController;
use crate::iscsi::IscsiController;
use crate::setup;
use crate::state_machine::StateMachine;

#[derive(Parser)]
#[command(
    name = "twinstor",
    version,
    about = "DRBD 9 dual-primary storage for 2-node XCP-ng",
    after_help = "examples:
  twinstor status                 health at a glance
  twinstor status --plain         one key<TAB>value per line, for scripts
  twinstor support-bundle         collect diagnostics for a bug report

`twinstor <command> --help` explains each command in detail."
)]
pub struct Cli {
    /// Path to this node's config (written by `twinstor setup`)
    #[arg(
        short,
        long,
        default_value = "/etc/twinstor/twinstor.toml",
        global = true,
        display_order = 900
    )]
    config: PathBuf,

    /// Disable color even on an interactive terminal (the NO_COLOR
    /// environment variable does the same)
    #[arg(long, global = true, default_value_t = false, display_order = 901)]
    no_color: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the twinstor daemon
    #[command(display_order = 8)]
    Daemon,
    /// Refresh the DRBD resource file after an upgrade (guarded, verify-or-rollback)
    ///
    /// Regenerates the resource file from the current template (upgraded
    /// pools keep their setup-time file otherwise). Guarded: refuses unless
    /// replication is fully healthy, never changes addresses or backing
    /// devices, applies via live `drbdadm adjust` with verification and
    /// rollback. Run on each node in turn.
    #[command(display_order = 6)]
    RegenerateConfig {
        /// Apply the change (without this flag: show the diff and exit)
        #[arg(long, default_value_t = false)]
        yes: bool,
        /// Stage the new file on disk without applying it live (for a
        /// maintenance-window reboot). On a connected, serving dual-Primary the
        /// live `drbdadm adjust` cannot run; this writes the file so the next
        /// `drbdadm up` (a reboot of this node) loads it. Requires --yes to
        /// write (without it, prints the diff and exits). Reboot this node
        /// promptly after staging; see the printed warning.
        #[arg(long, default_value_t = false)]
        persist_only: bool,
    },
    /// Safely restart this node's daemon for a rolling upgrade (health-gated)
    ///
    /// Applies a new binary without risking a dual-master: restarting the
    /// daemon on both nodes close together can produce one. Refuses unless a
    /// restart is safe on this node now (DRBD Connected, both UpToDate, no heal
    /// episode open, the peer daemon alive and not itself restarting), then
    /// restarts the local daemon and blocks until this node has reconverged.
    /// Run it on each node in turn: the daemon clears its liveness stamp on
    /// shutdown, so a run on the other node refuses until this one has
    /// restarted and stamped itself alive again. Runs a few seconds apart are
    /// therefore serialized, but a simultaneous push to both nodes is not safe
    /// (do not `--forks` this). RPU (one host at a time) and manual sequential
    /// use are safe.
    #[command(display_order = 7)]
    UpgradeApply {
        /// Only check the safety gates and exit (0 = safe to restart/reboot this
        /// node, non-zero = not safe). Restarts nothing. This is the exit-code
        /// contract for an RPM `%pre` precheck, so `yum update` (manual or via
        /// RPU) refuses to update a node while the pool is mid-transition.
        #[arg(long, default_value_t = false)]
        check: bool,
        /// Skip the confirmation prompt (does not bypass the safety gates).
        #[arg(long, default_value_t = false)]
        yes: bool,
        /// Seconds to wait for this node to reconverge after the restart.
        #[arg(long, default_value_t = 180)]
        timeout: u64,
        /// Do not auto-migrate a stale DRBD .res template. By default, when the
        /// on-disk .res is older than this binary's version, upgrade-apply migrates
        /// it live (drbdadm adjust) after the health gate, so config changes ride
        /// the normal upgrade with no extra step; it falls back to a note if the
        /// change needs a maintenance window. Pass this to handle the config by hand.
        #[arg(long, default_value_t = false)]
        skip_config: bool,
    },
    /// Re-point twinstor at a replaced backing disk and start the full resync
    ///
    /// Use after a disk swap or an mdadm array recreate that created a new
    /// /dev/disk/by-id path, which leaves the stored `backing_device`
    /// pointing at nothing so DRBD cannot attach and the node stops
    /// resyncing. Updates `backing_device` in twinstor.toml, regenerates the
    /// DRBD resource with the new local disk, and attaches it so the full
    /// resync from the peer begins. Allowed when the local disk is missing or
    /// Diskless; refuses under an open heal episode unless `--yes`.
    #[command(display_order = 4)]
    ReplaceBacking {
        /// The replacement block device, given by its stable /dev/disk/by-id
        /// path (a volatile /dev/sdX or /dev/md0 name can point at a different
        /// disk after a reboot).
        new_device: PathBuf,
        /// Skip the confirmation prompt (and proceed even with a heal episode
        /// open).
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// Resolve a held split-brain by hand: discard this node's copy and resync from the peer
    ///
    /// For the states where TwinStor holds instead of guessing (winner
    /// undecidable, or the loser could not stop its VMs safely): run it on the
    /// node whose copy you decided to give up. Its writes that the peer does not
    /// have are lost. Refuses unless DRBD is StandAlone or a split-brain marker
    /// is present, while the twinstor daemon runs on this node (stop it first;
    /// the peer keeps serving), or while any VM on this host still uses the
    /// TwinStor SR (stop them first so none sees its disk rolled back while
    /// running), or when the DRBD generation lineage says this node holds the
    /// copy to keep (see --discard-kept-copy). Then withdraws this node's
    /// target, demotes, and reconnects discarding the local copy. Start the
    /// daemon again afterwards: it keeps this node down until the resync from
    /// the peer has started.
    #[command(display_order = 5)]
    DiscardAndResync {
        /// Skip the confirmation prompt (the safety refusals still apply).
        #[arg(long, default_value_t = false)]
        yes: bool,
        /// Discard this copy even though the DRBD generation lineage says it
        /// is the one to keep (it is ahead of the peer, or holds more of the
        /// writes made during the split).
        #[arg(long, default_value_t = false)]
        discard_kept_copy: bool,
    },
    /// Show cluster and storage health at a glance (--json / --plain for scripts)
    ///
    /// One-line verdict first (healthy / degraded / critical), then DRBD,
    /// readiness, iSCSI, multipath, and tiebreaker state. Color and symbols
    /// appear only on an interactive terminal; piped output is plain text.
    #[command(display_order = 1)]
    Status {
        /// Output as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
        /// One key<TAB>value record per line (grep/awk friendly, never
        /// colored)
        #[arg(long, default_value_t = false, conflicts_with = "json")]
        plain: bool,
    },
    /// Diagnose (and with --fix, clear) the post-incident toolstack knots
    ///
    /// For the state where storage is perfectly healthy but VMs will not
    /// start: an HA-state disagreement ("pool has HA enabled but this server
    /// has HA disabled") and/or stale VDI `activating` locks. Without --fix it
    /// is read-only: reports both knots and the safety gates that decide
    /// whether it is safe to clear them. With --fix it clears them, but only
    /// when every gate passes and you confirm each action; it defers to
    /// auto-heal and will not interrupt an in-progress recovery. See the
    /// FAQ, "Storage is healthy but VMs will not start".
    #[command(display_order = 3)]
    Doctor {
        /// Clear the knots (gated, confirm-first). Without it, diagnose only.
        #[arg(long, default_value_t = false)]
        fix: bool,
    },
    /// Configure TwinStor on this node (interactive when run without
    /// arguments on a terminal)
    #[command(display_order = 2)]
    Setup {
        /// Peer node's IP address (omit to run the interactive wizard)
        #[arg(long)]
        peer_ip: Option<IpAddr>,
        /// Block device for DRBD: a free whole disk or partition, given by its
        /// stable /dev/disk/by-id path. Omit to let the wizard list the eligible
        /// disks/partitions and pick one interactively.
        #[arg(long)]
        backing_device: Option<PathBuf>,
        /// Local IP (auto-detected from route to peer)
        #[arg(long)]
        local_ip: Option<IpAddr>,
        /// DRBD replication port (default: 7789)
        #[arg(long)]
        drbd_port: Option<u16>,
        /// Force a full initial sync instead of skipping it
        #[arg(long, default_value_t = false)]
        full_sync: bool,
        /// CHAP password (auto-generated if not provided, copy from first node's output)
        #[arg(long)]
        chap_password: Option<String>,
        /// Extra always-on local IP to probe as a partition tiebreaker
        /// (repeatable). The default gateway is always probed; add a local
        /// device (e.g. managed switch) so one flaky router can't fence both
        /// nodes. Recommended but optional.
        #[arg(long = "ping-target")]
        ping_target: Vec<IpAddr>,
        /// Peer's IP on a back-to-back direct link (arbitrated mode). The
        /// direct link becomes the default replication path (cable-active):
        /// synchronous writes and resyncs stay off the switch, and the
        /// switched network is the automatic fallback a cable/NIC failure
        /// fails over to (loud: alert + `twinstor status` repl path line).
        /// The direct interface must already have a static IP. Omit for the
        /// default single-path fate-shared mode. See
        /// NETWORK_TOPOLOGY_SAFETY.md.
        #[arg(long = "direct-ip")]
        direct_ip: Option<IpAddr>,
        /// Accept a deployment with no partition tiebreaker (no default
        /// gateway, no ping targets). WARNING: the fence-peer handler then
        /// has nothing to probe, so any replication interruption, including
        /// the peer host dying, freezes this node's storage until the
        /// peer returns. Only for sites that knowingly accept
        /// freeze-on-disconnect semantics.
        #[arg(long = "allow-no-tiebreaker", default_value_t = false)]
        allow_no_tiebreaker: bool,
    },
    /// Move replication back to the direct link now (arbitrated mode)
    ///
    /// Asks the daemon to run the guarded failback dance immediately,
    /// skipping the automatic stability hold and clearing a flap latch;
    /// safety gates (both UpToDate, no resync, side selection) still apply.
    /// Use after fixing the direct cable, and before any switch maintenance.
    #[command(display_order = 5)]
    FailbackReplication {
        /// Seconds to wait for the daemon to complete the dance
        #[arg(long, default_value_t = 90)]
        timeout: u64,
    },
    /// Inspect/repair the recorded TwinStor SR uuid (used for HA re-arm, PBD re-plug, capacity)
    ///
    /// The daemon resolves the SR by its immutable uuid, recorded in a state
    /// file, rather than by its (renameable) name-label. Use these to view,
    /// re-detect (after an upgrade or an SR rename), or explicitly set that uuid.
    #[command(display_order = 6)]
    Sr {
        #[command(subcommand)]
        action: SrAction,
    },
    /// Hold this host's reboot until it is safe for storage (shutdown-time backstop)
    ///
    /// Not meant to be run by hand: it is invoked from a systemd ExecStop early in
    /// the shutdown transaction so that a reboot from any channel (a manual
    /// `reboot`, a kernel update, `yum`, `xe host-reboot`, XO RPU) cannot take down
    /// the last UpToDate replica while the peer is still resyncing. It engages only
    /// when this host holds the only current copy and the peer is catching up;
    /// then it waits (bounded) for the peer to become UpToDate, an operator
    /// override (touch /run/twinstor/allow-unsafe-reboot), or the time cap. Every
    /// other state proceeds instantly, and it fails open on any error, so it never
    /// wedges a reboot.
    #[command(display_order = 9)]
    RebootDrain,
    /// Collect logs, config, and state into a redacted .tar.gz for support
    ///
    /// Run this on both nodes and attach both files to your report.
    #[command(display_order = 3)]
    SupportBundle {
        /// Output path for the bundle (default: ./twinstor-bundle-<host>-<ts>.tar.gz)
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    /// Remove TwinStor: disable XHA, destroy SR, stop iSCSI + DRBD, clean up configs
    #[command(display_order = 7)]
    Uninstall {
        /// Skip confirmation prompt
        #[arg(long, default_value_t = false)]
        yes: bool,
        /// Keep the backing device (don't wipe the data)
        #[arg(long, default_value_t = false)]
        keep_data: bool,
        /// Proceed even when the VM-safety check cannot confirm the SR is
        /// unused (e.g. on a node whose XAPI/HA view is degraded). Use only
        /// when you know no VMs depend on this node's storage.
        #[arg(long, default_value_t = false)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum SrAction {
    /// Auto-detect the SR by its target IQN and record its uuid (idempotent; run after an upgrade or SR rename).
    Record,
    /// Set the recorded SR uuid explicitly (validates it exists).
    Set { uuid: String },
    /// Show the recorded/resolved SR uuid and how it was resolved.
    Show,
}

/// The bare `twinstor` overview: a short map of the common commands instead
/// of clap's usage error (per clig.dev, no arguments should print concise
/// help that points at `--help`).
pub fn print_overview() {
    let st = crate::render::Style::for_stdout();
    println!(
        "twinstor {} - DRBD 9 dual-primary storage for 2-node XCP-ng",
        env!("CARGO_PKG_VERSION")
    );
    println!();
    println!("usage: twinstor <command> [options]");
    println!();
    println!("{}", st.bold("common commands:"));
    println!("  status                 cluster health at a glance (--json/--plain for scripts)");
    println!("  setup                  configure this node (interactive wizard on a terminal)");
    println!("  support-bundle         collect logs and state for a bug report (both nodes)");
    println!();
    println!("{}", st.bold("recovery:"));
    println!("  replace-backing        re-point at a replaced backing disk, start the resync");
    println!("  failback-replication   move replication back to the direct link now");
    println!("  regenerate-config      refresh the DRBD resource file after an upgrade");
    println!();
    println!("`twinstor --help` lists all commands; `twinstor <command> --help` explains each.");
}

impl Cli {
    pub fn is_daemon(&self) -> bool {
        matches!(self.command, Command::Daemon)
    }

    pub async fn run(self) -> Result<()> {
        crate::render::set_no_color(self.no_color);
        match self.command {
            Command::Setup {
                peer_ip,
                backing_device,
                local_ip,
                drbd_port,
                full_sync,
                chap_password,
                ping_target,
                direct_ip,
                allow_no_tiebreaker,
            } => {
                let params = match peer_ip {
                    Some(peer_ip) => setup::SetupParams {
                        peer_ip,
                        backing_device,
                        local_ip,
                        drbd_port,
                        full_sync,
                        chap_password,
                        ping_targets: ping_target,
                        direct_ip,
                        allow_no_tiebreaker,
                    },
                    None => {
                        let other_args_given = backing_device.is_some()
                            || local_ip.is_some()
                            || drbd_port.is_some()
                            || full_sync
                            || chap_password.is_some()
                            || !ping_target.is_empty()
                            || direct_ip.is_some()
                            || allow_no_tiebreaker;
                        if other_args_given {
                            return Err(crate::render::CliError::new(
                                "--peer-ip is required when other setup flags are given",
                                "add --peer-ip <IP> to this command, or run `twinstor setup` \
                                 with no arguments on a terminal for the interactive wizard",
                            )
                            .explain(
                                "partial flags are ambiguous: the wizard only runs when \
                                 nothing is specified, so setup cannot guess the rest.",
                            )
                            .into());
                        }
                        if !crate::wizard::stdin_is_tty() {
                            return Err(crate::render::CliError::new(
                                "the interactive wizard needs a terminal (stdin is not a TTY)",
                                "run `twinstor setup` from an interactive shell, or pass \
                                 --peer-ip <IP> (plus flags) for a non-interactive setup",
                            )
                            .into());
                        }
                        match crate::wizard::run().await? {
                            Some(params) => params,
                            None => return Ok(()), // handled in place (quit/uninstall)
                        }
                    }
                };
                setup::run_setup(params).await
            }
            Command::Uninstall {
                yes,
                keep_data,
                force,
            } => crate::uninstall::run_uninstall(yes, keep_data, force).await,
            Command::RebootDrain => {
                // Fail open on config too: this runs from a systemd ExecStop during
                // shutdown, so a missing or broken config (host installed but not
                // yet set up) must let the reboot proceed, not fail the stop. Only
                // a loadable config engages the guard, hence the check before the
                // strict Config::load below.
                match Config::load(&self.config) {
                    Ok(config) => crate::reboot_drain::run(&config).await,
                    Err(e) => {
                        tracing::info!(
                            "reboot-drain: no usable config at {} ({e}); proceeding (fail-open)",
                            self.config.display()
                        );
                        Ok(())
                    }
                }
            }
            _ => {
                let config = Arc::new(Config::load(&self.config).map_err(|e| {
                    crate::render::CliError::new(
                        format!("cannot load this node's config ({})", self.config.display()),
                        "if TwinStor is not set up on this node yet, run `twinstor setup`; \
                         otherwise point at the right file with -c <path>",
                    )
                    .caused_by(e)
                })?);
                let config_path = self.config.clone();
                match self.command {
                    Command::Daemon => run_daemon(config).await,
                    Command::Status { json, plain } => run_status(config, json, plain).await,
                    Command::Doctor { fix } => crate::doctor::run(config, fix).await,
                    Command::RegenerateConfig { yes, persist_only } => {
                        run_regenerate_config(config, yes, persist_only).await
                    }
                    Command::UpgradeApply {
                        check,
                        yes,
                        timeout,
                        skip_config,
                    } => run_upgrade_apply(config, check, yes, timeout, skip_config).await,
                    Command::ReplaceBacking { new_device, yes } => {
                        run_replace_backing(config, config_path, new_device, yes).await
                    }
                    Command::FailbackReplication { timeout } => {
                        run_failback_replication(config, timeout).await
                    }
                    Command::DiscardAndResync {
                        yes,
                        discard_kept_copy,
                    } => run_discard_and_resync(config, yes, discard_kept_copy).await,
                    Command::SupportBundle { output } => {
                        crate::diagnostics::run(&config, &config_path, output).await
                    }
                    Command::Sr { action } => run_sr(config, action).await,
                    Command::Setup { .. } | Command::Uninstall { .. } | Command::RebootDrain => {
                        unreachable!()
                    }
                }
            }
        }
    }
}

async fn run_daemon(config: Arc<Config>) -> Result<()> {
    info!(
        version = env!("CARGO_PKG_VERSION"),
        node = %config.node.name,
        peer = %config.node.peer,
        "twinstor starting"
    );

    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);

    // Under a WatchdogSec= unit the tick loop pings the watchdog; readiness is
    // announced once here. A no-op outside systemd.
    crate::sd_notify::ready();

    let drbd = DrbdController::new(&config);
    let iscsi = IscsiController::new();

    let (mut sm, _state_rx) = StateMachine::new(config.clone(), Box::new(drbd), Box::new(iscsi));

    // The SR is resolved by its immutable uuid, recorded in a state file, so an
    // SR rename cannot break HA re-arm, PBD re-plug, capacity or doctor. Setup
    // records it; this covers installs upgraded from before it did. Runs once at
    // daemon startup, before the tick loop.
    if crate::xapi::read_recorded_sr_uuid().is_none() {
        match crate::xapi::record_sr_uuid(&config.iscsi.target_iqn).await {
            Some(uuid) => info!(sr_uuid = %uuid, "recorded twinstor SR uuid"),
            None => warn!(
                target_iqn = %config.iscsi.target_iqn,
                "could not resolve the twinstor SR to record its uuid (PBD unplugged or IQN mismatch?); \
                 falling back to name-label lookup this session"
            ),
        }
    }

    // If the last shutdown's reboot-drain let the reboot proceed while this host
    // was the last UpToDate replica, it left a breadcrumb (xapi is already gone
    // during shutdown, so it could not alert then). Deliver that alert in the
    // background, retrying until the toolstack accepts it: the daemon starts
    // before xapi is ready, so a single attempt here would usually be dropped.
    tokio::spawn(crate::reboot_drain::deliver_pending_reboot_alert());

    let sm_shutdown = shutdown_tx.subscribe();
    let shutdown_tx_signal = shutdown_tx.clone();

    tokio::spawn(async move {
        let sigint = tokio::signal::ctrl_c();

        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("failed to register SIGTERM handler");
            tokio::select! {
                _ = sigint => info!("received SIGINT"),
                _ = sigterm.recv() => info!("received SIGTERM"),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = sigint.await;
            info!("received SIGINT");
        }

        info!("shutting down");
        let _ = shutdown_tx_signal.send(());
    });

    sm.run(sm_shutdown).await
}

/// `twinstor sr <record|set|show>`: inspect or repair the recorded SR uuid the
/// daemon uses to find the twinstor SR (immutable, so an SR rename cannot break
/// HA re-arm, PBD re-plug, capacity or doctor).
async fn run_sr(config: Arc<Config>, action: SrAction) -> Result<()> {
    match action {
        SrAction::Record => match crate::xapi::record_sr_uuid(&config.iscsi.target_iqn).await {
            Some(u) => println!("recorded SR uuid {u}"),
            None => println!(
                "could not resolve the twinstor SR (is the PBD plugged? target_iqn correct?)"
            ),
        },
        SrAction::Set { uuid } => {
            if !crate::xapi::sr_uuid_exists(&uuid).await {
                return Err(crate::render::CliError::new(
                    format!("no SR with uuid {uuid}"),
                    "check `xe sr-list` for the right uuid, or use `twinstor sr record` to \
                     auto-detect it by target IQN",
                )
                .into());
            }
            // Warn, but allow the explicit override, if this is not the SR whose PBD
            // targets our IQN: recording a foreign SR silently redirects HA and
            // capacity.
            if let Some(expected) =
                crate::xapi::discover_sr_uuid_by_iqn(&config.iscsi.target_iqn).await
                && expected != uuid
            {
                println!(
                    "warning: uuid {uuid} is not the SR matching target_iqn {} (that is \
                     {expected}); recording it anyway as an explicit override",
                    config.iscsi.target_iqn
                );
            }
            crate::xapi::write_recorded_sr_uuid(&uuid)?;
            println!("recorded SR uuid {uuid}");
        }
        SrAction::Show => {
            if let Some(u) = crate::xapi::read_recorded_sr_uuid() {
                if crate::xapi::sr_uuid_exists(&u).await {
                    println!("{u} (recorded)");
                } else {
                    println!(
                        "{u} (recorded, but not found in XAPI, so it is stale; run `twinstor sr record`)"
                    );
                }
            } else if let Some(u) = crate::xapi::twinstor_sr_uuid().await {
                println!("{u} (name-label fallback)");
            } else {
                println!("no twinstor SR resolved");
            }
        }
    }
    Ok(())
}

/// Why `discard-and-resync` must not run, as (what, how to fix).
fn discard_refusal(
    daemon_active: bool,
    connection: Option<crate::drbd::ConnectionState>,
    marker_present: bool,
    lineage_keeps_local: Option<bool>,
    lineage_tie: bool,
    discard_kept_copy: bool,
    scan: &std::result::Result<crate::loser_boundary::LocalScan, String>,
) -> Option<(String, String)> {
    use crate::drbd::ConnectionState;
    if daemon_active {
        return Some((
            "the twinstor daemon is running on this node".into(),
            "stop it first (systemctl stop twinstor; the peer keeps serving), re-run this command, \
             then systemctl start twinstor"
                .into(),
        ));
    }
    match connection {
        None => {
            return Some((
                "DRBD status could not be read".into(),
                "check `drbdadm status` and that the resource is up".into(),
            ));
        }
        Some(ConnectionState::StandAlone) => {}
        Some(_) if marker_present => {}
        Some(c) => {
            return Some((
                format!("no split-brain to resolve here (DRBD is {c:?}, no split-brain marker)"),
                "nothing to discard; a connected or connecting resource resyncs by itself".into(),
            ));
        }
    }
    if lineage_tie && !discard_kept_copy {
        return Some((
            "the DRBD generation lineage cannot tell which copy to keep: both diverged and neither \
             holds more of the writes made during the split, so it reads \"keep\" on both nodes"
                .into(),
            "decide which copy to give up (for example, the node whose VMs did not keep serving), \
             then run this there with --discard-kept-copy"
                .into(),
        ));
    }
    if lineage_keeps_local == Some(true) && !discard_kept_copy {
        return Some((
            "the DRBD generation lineage says this node holds the copy to keep (it is ahead of the \
             peer, or holds more of the writes made during the split); discarding it here loses \
             those writes"
                .into(),
            "run it on the other node instead; if you decided to give up this copy anyway, re-run \
             with --discard-kept-copy"
                .into(),
        ));
    }
    match scan {
        Err(e) => Some((
            format!("cannot tell which VMs on this host use the TwinStor SR ({e})"),
            "check `xl list` and stop every VM on this host that uses the SR, then retry".into(),
        )),
        Ok(scan) if !scan.on_sr.is_empty() => {
            let ids: Vec<String> = scan
                .on_sr
                .iter()
                .map(|(d, u)| format!("domid {d} ({u})"))
                .collect();
            Some((
                format!(
                    "VMs on this host still use the TwinStor SR: {}",
                    ids.join(", ")
                ),
                "stop them first (xe vm-shutdown --force uuid=<uuid>) so none keeps running on a \
                 disk rolled back underneath it, then retry"
                    .into(),
            ))
        }
        Ok(_) => None,
    }
}

async fn run_discard_and_resync(
    config: Arc<Config>,
    yes: bool,
    discard_kept_copy: bool,
) -> Result<()> {
    use crate::drbd::DrbdControl;
    use crate::iscsi::IscsiControl;
    use crate::loser_boundary::GuestControl;
    use std::io::Write;

    let drbd = DrbdController::new(&config);
    let guests = crate::loser_boundary::RealGuests::new(&config);
    // Every precondition, read fresh: run once before the prompt and again
    // right before the first admission change (the prompt can wait indefinitely).
    let check = || async {
        let daemon = crate::cmd::cmd_status_for("systemctl", &["is-active", "twinstor"], 10)
            .await
            .map(|(_, out, _)| out);
        let status = drbd.status().await.ok();
        let marker_present =
            std::path::Path::new(&crate::state_machine::split_brain_marker()).exists();
        let (keeps, tie) = match drbd.handshake_read().await {
            Ok(crate::drbd::LineageRead { pair: Some(h), .. }) => (
                crate::state_machine::lineage_keeps_local(&h),
                crate::state_machine::lineage_tie(&h),
            ),
            _ => (None, false),
        };
        let scan = guests.local_scan().await;
        match discard_refusal(
            daemon_may_be_running(&daemon),
            status.as_ref().map(|s| s.connection),
            marker_present,
            keeps,
            tie,
            discard_kept_copy,
            &scan,
        ) {
            Some((what, fix)) => Err(crate::render::CliError::new(what, fix)),
            None => Ok((status.expect("checked by discard_refusal"), marker_present)),
        }
    };
    let (s, marker_present) = check().await?;
    println!(
        "resource {}: role {:?}, disk {:?}, connection {:?}{}",
        config.storage.drbd_resource,
        s.role,
        s.disk,
        s.connection,
        if marker_present {
            ", split-brain marker present"
        } else {
            ""
        }
    );
    println!(
        "\nThis node's copy will be discarded and resynced from {}: every write on this node that \
         {} does not have is lost for good. Run this only on the node whose copy you decided to \
         give up.\n",
        config.node.peer, config.node.peer
    );
    if !yes {
        print!("type DISCARD to proceed: ");
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if input.trim() != "DISCARD" {
            println!("aborted, nothing changed.");
            return Ok(());
        }
    }

    // The prompt may have waited a long time, so the host is disabled (nothing
    // new placed or started here) and the checks run again. Disabling is
    // best-effort (refused with pool HA armed on a two-host pool); the fresh
    // local scan is the authority. Only a host that was enabled is recorded:
    // one an operator had already disabled stays as it is.
    // A suspended copy must be liftable before anything is changed: a refusal
    // here leaves the host, the marker and the copy as they were.
    let suspended_now = drbd
        .status()
        .await
        .is_ok_and(|st| st.suspended_user || st.suspended_fencing);
    if suspended_now && let Err(why) = crate::loser_boundary::operator_lift_check(&guests).await {
        return Err(crate::render::CliError::new(
                format!("{why}; this copy's I/O is suspended and stays suspended, nothing was changed"),
                "stop that VM (xe vm-shutdown --force uuid=<vm>), or wait for its start to finish and stop it, then retry",
            )
            .into());
    }
    // Only the enabled flag: a failed maintenance-marker read must not lose it.
    let was_enabled = crate::xapi::is_host_enabled().await;
    let store = crate::loser_boundary::Store::new(crate::loser_boundary::default_record_path());
    if crate::xapi::disable_local_host().await {
        println!("this host is disabled in XAPI (nothing new is placed on it)");
        if was_enabled == Some(true) {
            // The daemon enables it again once the pair has converged; nothing
            // else would, and its readiness gate does not serve a disabled host.
            if let Err(e) = store.note_host_disabled() {
                println!(
                    "WARNING: could not record that this host was disabled ({e}); once the resync \
                     has finished, enable it by hand: xe host-enable uuid=<this host>"
                );
            }
        }
    }
    check().await?;
    // This copy is given up: a split-brain win this node recorded (and
    // announces on the beacon) must not make the peer discard too, and a
    // yield latch has nothing left to decide.
    let had_win = std::path::Path::new(&crate::state_machine::win_claim_marker()).exists();
    let had_latch = std::path::Path::new(&crate::state_machine::yield_latch_marker()).exists();
    let _ = std::fs::remove_file(crate::state_machine::win_claim_marker());
    let _ = std::fs::remove_file(crate::state_machine::yield_latch_marker());
    // From here on the daemon must never promote or export this copy again
    // before it has resynced from the peer, even across a reboot. One line,
    // logged by the daemon at its next start (`startup_state`): the daemon log
    // then says who discarded this copy, when, and how.
    let requested_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let marker_line = format!(
        "requested by twinstor discard-and-resync: at={requested_at} version={} discard_kept_copy={discard_kept_copy} \
         removed_win_record={had_win} removed_yield_latch={had_latch}\n",
        env!("CARGO_PKG_VERSION")
    );
    crate::state_machine::write_file_durably(
        &crate::state_machine::operator_discard_marker(),
        &marker_line,
    )
    .map_err(|e| {
        crate::render::CliError::new(
            format!("could not record the discard request durably ({e})"),
            "check that /var/lib/twinstor is writable, then retry; nothing was changed",
        )
    })?;

    let armed = format!(
        "the discard request stays armed ({m}): the daemon keeps this node down and finishes \
         it once the cause is fixed; to cancel it instead, delete {m}",
        m = crate::state_machine::operator_discard_marker()
    );
    // Disk handles left on the SR by guests destroyed earlier (a crash
    // boundary, a manual `xl destroy`): XAPI's storage layer keeps their
    // datapaths activated and their tapdisks keep the old VHD metadata cached.
    // Release them first, while this node's own loopback session and target
    // still give this host's multipath map a path to this node's own copy: a
    // close writes that metadata, and it must land on the copy being
    // discarded, never on the peer's (the only path left once the loopback is
    // logged out and the target is down). Nothing on the SR runs here
    // (checked above), so lifting a leftover suspend first acknowledges the
    // frozen writes to nobody; the sweep also refuses to close anything while
    // the active path is not this node's own.
    let was_suspended = drbd
        .status()
        .await
        .is_ok_and(|st| st.suspended_user || st.suspended_fencing);
    // A suspended copy is lifted only while no guest here could acknowledge a
    // write on it: none runs on the SR and none is being built with an SR
    // disk (see `operator_lift_check`; a dom0 consumer only gets a warning).
    if was_suspended {
        match crate::loser_boundary::operator_lift_check(&guests).await {
            Err(why) => {
                return Err(crate::render::CliError::new(
                    format!("{why}; this copy's I/O stays suspended and not discarding"),
                    format!(
                        "stop that VM (xe vm-shutdown --force uuid=<vm>), or wait for its start to \
                             finish and stop it, then retry; {armed}"
                    ),
                )
                .into());
            }
            Ok(others) if !others.is_empty() => println!(
                "note: VDI(s) {} are held by a non-guest consumer on this host (a dom0 backup, export \
                 or copy the suspend froze); lifting the suspend lets its pending I/O finish on this \
                 copy, which is being discarded: re-run that job after the resync",
                others.join(", ")
            ),
            Ok(_) => {}
        }
    }
    let _ = drbd.resume_io().await;
    let known_leaked = store
        .load()
        .ok()
        .flatten()
        .map(|r| r.leaked)
        .unwrap_or_default();
    match crate::loser_boundary::sweep_disk_handles(
        &guests,
        &known_leaked,
        crate::loser_boundary::Side::Loser,
        None,
    )
    .await
    {
        Ok(sweep) => {
            if sweep.released + sweep.tapdisks_closed > 0 {
                println!(
                    "released {} datapath(s) and closed {} tapdisk(s) left on the SR by destroyed guests",
                    sweep.released, sweep.tapdisks_closed
                );
            }
            if !sweep.leaked.is_empty() {
                println!(
                    "WARNING: XAPI's storage layer did not release the datapaths of VDI(s) {}; the \
                     daemon retries, and does not start a VM using them on this host until it \
                     succeeds. `{}` lists them (look for ** LEAKED, or a vbd/<domid>/ of a domain \
                     `xl list` no longer shows); release one by hand with `{}`.",
                    sweep.leaked.join(", "),
                    crate::xapi_storage::diagnostics_command(),
                    crate::xapi_storage::destroy_command("vbd/<domid>/<device>")
                );
                if let Err(e) = store.note_leaked(&sweep.leaked) {
                    println!("WARNING: could not record them ({e})");
                }
            }
        }
        Err(e) => {
            // Nothing is discarded: a suspend lifted for the release goes back,
            // so the copy left in place acknowledges no new write.
            if was_suspended && let Err(se) = drbd.suspend_io().await {
                println!("WARNING: could not suspend this copy's I/O again ({se})");
            }
            let fix = if e.to_string().contains("storage path") {
                // Nothing re-creates this node's own path while the daemon is
                // stopped: the operator decides, knowing where a close lands.
                format!(
                    "a close goes through this host's multipath map, and its own path to this copy is \
                     not active (its loopback session or target is down), so it would write the stale \
                     disk metadata onto the peer's copy, the one that is kept. Bring this node's target \
                     and loopback session back (start the daemon without the discard request: delete \
                     {m}), then retry. Release a handle as it is (`{}`) only if it was opened on the \
                     peer's history and every VM using that disk is halted on every host of the pool \
                     (`xe vm-list power-state=running` shows none of them): a VM started again on the \
                     peer would have its disk metadata overwritten; {armed}",
                    crate::xapi_storage::destroy_command("vbd/<domid>/<device>"),
                    m = crate::state_machine::operator_discard_marker()
                )
            } else {
                format!(
                    "find the datapath of each with the storage layer's DP.diagnostics and release it \
                     with `{}`; only if that fails, `tap-ctl destroy -p <pid> -m <minor>` (see \
                     `tap-ctl list`) followed by the same release; then retry; {armed}",
                    crate::xapi_storage::destroy_command("vbd/<domid>/<device>")
                )
            };
            return Err(crate::render::CliError::new(
                format!(
                    "could not release the disk handles left open on the SR ({e}); not discarding"
                ),
                fix,
            )
            .into());
        }
    }
    // Then the daemon's loser order: drop this node's own loopback session (its
    // path then drops cleanly instead of returning hard errors once this copy
    // goes Secondary), lift a leftover suspend so the target teardown cannot
    // block, tear the target down, demote, then reconnect discarding the copy.
    //
    // Last look first: a guest started on this host since the checks above
    // (an operator, or pool HA: with HA armed the host stays enabled) would
    // read this copy through the loopback path and run on across the
    // discard. After the logout a new start only reaches the peer's copy.
    if let Err(e) = crate::loser_boundary::confirm_clear(&guests).await {
        if was_suspended && let Err(se) = drbd.suspend_io().await {
            println!("WARNING: could not suspend this copy's I/O again ({se})");
        }
        return Err(crate::render::CliError::new(
            format!("the SR is in use on this host again ({e}); not discarding"),
            format!(
                "stop those VMs (and close what they left open, as above), then retry; {armed}"
            ),
        )
        .into());
    }
    if let Some(local_ip) =
        crate::multipath::local_ip_toward(&config.network.peer_address.to_string()).await
    {
        crate::iscsi::logout_portal(&format!("{local_ip}:{}", config.iscsi.portal_port)).await;
    }
    let _ = drbd.resume_io().await;
    let mut iscsi = crate::iscsi::IscsiController::new();
    match tokio::time::timeout(std::time::Duration::from_secs(30), iscsi.stop()).await {
        Ok(Ok(())) => {}
        // Same rule as the daemon: never discard while LIO may still export
        // this copy to a session that survived.
        Ok(Err(e)) => {
            return Err(crate::render::CliError::new(
                format!("the iSCSI backstore is still exported ({e}); not discarding"),
                format!("check `targetcli ls` on this host and remove the twinstor backstore, then retry; {armed}"),
            )
            .into())
        }
        Err(_) => {
            return Err(crate::render::CliError::new(
                "the iSCSI target teardown did not return within 30 s (likely wedged in the kernel)",
                format!("reboot this node; the peer keeps serving, and it rejoins as a sync target; {armed}"),
            )
            .into())
        }
    }
    drbd.secondary().await.map_err(|e| {
        crate::render::CliError::new(
            "could not demote this node to Secondary (the device is still open)",
            format!("check `xl list` and `targetcli ls` for a remaining user of the device, then retry; {armed}"),
        )
        .caused_by(e)
    })?;
    let _ = drbd.disconnect().await;
    drbd.connect_discard().await?;
    // Owed restarts of a crash boundary that held: this node is discarding now,
    // so the daemon starts them on the peer once it runs again.
    if store.exists() {
        let _ = store.set_phase(crate::loser_boundary::Phase::Discarding);
    }
    print!("discarding; waiting for the peer to connect");
    std::io::stdout().flush()?;
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        if let Ok(st) = drbd.status().await
            && st.connection == crate::drbd::ConnectionState::Connected
        {
            println!(
                "\nconnected: this node is resyncing from {}.",
                config.node.peer
            );
            println!(
                "now start the daemon again: systemctl start twinstor (it keeps this node down \
                 until the resync has started{})",
                if store.load().ok().flatten().is_some_and(|r| r.host_disabled) {
                    ", and enables this host again in XAPI once the two copies are equal"
                } else {
                    ""
                }
            );
            return Ok(());
        }
        print!(".");
        std::io::stdout().flush()?;
    }
    println!(
        "\nnot connected yet: the discard is armed and the resync starts as soon as {} accepts \
         the connection (if its daemon holds it StandAlone, run `drbdadm connect {}` there). \
         Start the daemon again: systemctl start twinstor. It keeps this node down (Secondary, \
         no target) and re-arms the discard until the resync has started{}. To cancel instead, \
         delete {}.",
        config.node.peer,
        format_args!("twinstor-{}", config.storage.drbd_resource),
        if store.load().ok().flatten().is_some_and(|r| r.host_disabled) {
            ", then enables this host again in XAPI once the two copies are equal"
        } else {
            ""
        },
        crate::state_machine::operator_discard_marker()
    );
    Ok(())
}

/// Whether `systemctl is-active twinstor` leaves any chance the daemon runs:
/// only a definite `inactive` or `failed` says it does not; an unreadable
/// answer or a transitional state refuses.
fn daemon_may_be_running(answer: &std::result::Result<String, anyhow::Error>) -> bool {
    !matches!(answer.as_ref().map(|s| s.trim()), Ok("inactive" | "failed"))
}

/// `twinstor failback-replication`: hand the daemon an immediate-failback
/// request and watch the outcome. The daemon runs the failback itself (doing
/// it from here would race the Online tick); this places the request file and
/// polls the exported state and the established path.
async fn run_failback_replication(config: Arc<Config>, timeout: u64) -> Result<()> {
    use crate::drbd::DrbdControl;

    let direct_ips: Vec<String> = match std::fs::read_to_string(crate::setup::DIRECT_PEER_PATH) {
        Ok(text) => text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_string)
            .collect(),
        Err(_) => Vec::new(),
    };
    if direct_ips.is_empty() {
        return Err(crate::render::CliError::new(
            format!(
                "this node is not in arbitrated mode (no {})",
                crate::setup::DIRECT_PEER_PATH
            ),
            "nothing to fail back: a single-path deployment has no direct link \
             (arbitrated mode is configured at setup time with --direct-ip)",
        )
        .into());
    }
    let daemon_active = tokio::process::Command::new("systemctl")
        .args(["is-active", "--quiet", "twinstor"])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    if !daemon_active {
        return Err(crate::render::CliError::new(
            "the twinstor daemon is not running",
            "systemctl start twinstor, then re-run this command",
        )
        .explain("the daemon owns the failback dance; this command only asks it to run one.")
        .into());
    }

    let drbd = DrbdController::new(&config);
    let on_direct = |paths: &[crate::drbd::PathState]| {
        paths
            .iter()
            .find(|p| p.established)
            .map(|p| direct_ips.contains(&p.peer))
    };
    if let Ok(paths) = drbd.paths().await
        && on_direct(&paths) == Some(true)
    {
        println!("replication is already on the direct link; nothing to do");
        return Ok(());
    }

    crate::failback_state::place_request()
        .map_err(|e| anyhow::anyhow!("cannot place the failback request: {e}"))?;
    println!("failback requested; waiting for the daemon (up to {timeout}s)...");

    // On a TTY the spinner line carries the elapsed count while mode changes
    // print above it; piped output keeps only the deduplicated mode lines.
    let mut sp = crate::render::Spinner::new("waiting for the daemon");
    let start = std::time::Instant::now();
    let mut last_shown = String::new();
    let mut next_poll = std::time::Duration::ZERO;
    while start.elapsed().as_secs() < timeout {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        sp.tick(&format!(" ({}/{timeout}s)", start.elapsed().as_secs()));
        if start.elapsed() < next_poll {
            continue;
        }
        next_poll = start.elapsed() + std::time::Duration::from_secs(2);
        if let Ok(paths) = drbd.paths().await
            && on_direct(&paths) == Some(true)
        {
            sp.finish();
            println!("done: replication re-established on the direct link");
            return Ok(());
        }
        if let Some(s) = crate::failback_state::read() {
            let line = format!("{}: {}", s.mode, s.reason);
            if line != last_shown {
                sp.println(&format!("  {line}"));
                last_shown = line;
            }
        }
    }
    sp.finish();
    Err(crate::render::CliError::new(
        format!("failback did not complete within {timeout}s"),
        "check `twinstor status` and /var/log/twinstor.log on both nodes",
    )
    .explain(
        "the dance refuses to run unless both disks are UpToDate with no \
         resync in progress.",
    )
    .into())
}

/// Staleness threshold for the peer's `twinstor-alive` stamp. The daemon stamps
/// every `ALIVE_STAMP_INTERVAL` (60 s); this allows about 2.5 intervals for
/// jitter and xe latency. An older stamp means the peer daemon is down or
/// itself restarting, so restarting here now would risk a dual-master.
const PEER_ALIVE_STALE_SECS: u64 = 150;

/// A reason `upgrade-apply` refuses to restart the daemon right now. An enum
/// rather than formatted strings so the interlock can be unit-tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpgradeBlocker {
    NotConnected,
    NotUpToDate,
    EpisodeOpen,
    PeerNotAlive,
}

/// The peer daemon is alive if its `twinstor-alive` stamp exists and is no
/// older than `PEER_ALIVE_STALE_SECS`. A restarting daemon lets its own stamp go
/// stale, which is what makes a concurrent upgrade-apply on the peer refuse.
fn peer_daemon_alive_from_stamp(stamp: Option<u64>, now: u64) -> bool {
    stamp.is_some_and(|s| now.saturating_sub(s) <= PEER_ALIVE_STALE_SECS)
}

/// Which conditions block a rolling-upgrade restart on this node right now;
/// empty means safe. The peer-alive term serializes the two nodes; the storage
/// terms ensure a restart cannot strand a resync or interfere with an open
/// incident.
fn upgrade_apply_blockers(
    connected: bool,
    both_uptodate: bool,
    episode_open: bool,
    peer_alive: bool,
) -> Vec<UpgradeBlocker> {
    let mut v = Vec::new();
    if !connected {
        v.push(UpgradeBlocker::NotConnected);
    }
    if !both_uptodate {
        v.push(UpgradeBlocker::NotUpToDate);
    }
    if episode_open {
        v.push(UpgradeBlocker::EpisodeOpen);
    }
    if !peer_alive {
        v.push(UpgradeBlocker::PeerNotAlive);
    }
    v
}

async fn run_upgrade_apply(
    config: Arc<Config>,
    check: bool,
    yes: bool,
    timeout: u64,
    skip_config: bool,
) -> Result<()> {
    use crate::drbd::{ConnectionState, DiskState, DrbdControl};
    use anyhow::{Context, bail};
    use std::io::Write;

    let drbd = DrbdController::new(&config);

    // 1. Safety gates. All must hold; --yes does not bypass them. The
    // peer-alive gate serializes a push to both nodes: while this node's daemon
    // is down for its restart, its twinstor-alive stamp goes stale, so a
    // concurrent upgrade-apply on the peer refuses until this one is healthy.
    let st = drbd.status().await.context("reading DRBD status")?;
    let connected = st.connection == ConnectionState::Connected;
    let both_uptodate = st.disk == DiskState::UpToDate && st.peer_disk == Some(DiskState::UpToDate);
    let episode_open = std::path::Path::new("/var/lib/twinstor/open-episode").exists();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (stamp, _live, ver) = crate::xapi::peer_daemon_alive(&config.node.peer).await;
    let peer_alive = peer_daemon_alive_from_stamp(stamp, now);

    let blockers = upgrade_apply_blockers(connected, both_uptodate, episode_open, peer_alive);
    if !blockers.is_empty() {
        let detail = blockers
            .iter()
            .map(|b| match b {
                UpgradeBlocker::NotConnected => format!(
                    "DRBD is not Connected (state: {:?}): the peer is unreachable or itself \
                     mid-restart",
                    st.connection
                ),
                UpgradeBlocker::NotUpToDate => format!(
                    "DRBD is not both-UpToDate (local: {:?}, peer: {:?}): a resync is in flight",
                    st.disk, st.peer_disk
                ),
                UpgradeBlocker::EpisodeOpen => {
                    "a heal episode is open (/var/lib/twinstor/open-episode): an incident is \
                     resolving"
                        .to_string()
                }
                UpgradeBlocker::PeerNotAlive => format!(
                    "the peer daemon ({}) is not alive (twinstor-alive stamp stale or missing): \
                     it may be mid-restart; restarting now would risk a dual-master",
                    config.node.peer
                ),
            })
            .collect::<Vec<_>>();
        return Err(crate::render::CliError::new(
            "refusing to restart the daemon: it is not safe on this node right now",
            "wait until `twinstor status` is healthy on both nodes (Connected + UpToDate, no \
             incident), then re-run. Upgrade one node at a time; never restart both close together.",
        )
        .explain(format!("blocking condition(s):\n  - {}", detail.join("\n  - ")))
        .into());
    }

    // Detect a stale on-disk .res template (an older version than this binary's).
    // This does not block the restart: the daemon enforces the split-brain safety
    // policies live whatever the .res version, so an old template is safe. By
    // default upgrade-apply migrates it live after the gate (step 2b), because
    // disk-options changes such as resync tuning apply via `drbdadm adjust` with
    // no connection reset. A change that needs a connection reset (topology)
    // rolls back inside regenerate-config and becomes a maintenance-window note.
    let stale_version: Option<u32> = {
        let res_path = format!("/etc/drbd.d/twinstor-{}.res", config.storage.drbd_resource);
        tokio::fs::read_to_string(&res_path)
            .await
            .ok()
            .and_then(|t| {
                let v = crate::setup::res_config_version(&t);
                (v != crate::setup::CONFIG_VERSION).then_some(v)
            })
    };

    // --check (RPM %pre precheck): gates passed, report safe and exit 0 without
    // touching the daemon. The unsafe path above already returned non-zero.
    if check {
        println!(
            "safe to restart/reboot this node ({}): DRBD Connected + both UpToDate, peer daemon \
             alive, no heal episode open",
            config.node.name
        );
        if let Some(v) = stale_version {
            let cur = crate::setup::CONFIG_VERSION;
            if skip_config {
                println!(
                    "\nnote: DRBD .res is at template version {v} (current {cur}); --skip-config \
                     is set, so upgrade-apply will leave it as-is. Safe either way: the daemon \
                     enforces the split-brain safety policies live regardless of .res version."
                );
            } else {
                println!(
                    "\nnote: DRBD .res is at template version {v} (current {cur}); upgrade-apply \
                     will migrate it live (drbdadm adjust) after the gate. If the change needs a \
                     connection reset it rolls back to a maintenance-window note. Either way it \
                     is safe and does not block the upgrade."
                );
            }
        }
        return Ok(());
    }

    // 2. Confirm.
    println!("TwinStor upgrade-apply on this node ({})", config.node.name);
    println!("  DRBD: Connected, both UpToDate");
    println!(
        "  peer daemon ({}): alive{}",
        config.node.peer,
        ver.map(|v| format!(", version {v}")).unwrap_or_default()
    );
    println!("  no heal episode open\n");
    if let Some(v) = stale_version {
        let cur = crate::setup::CONFIG_VERSION;
        if skip_config {
            println!("  config: .res at v{v} (current v{cur}); left as-is (--skip-config)\n");
        } else {
            println!("  config: .res at v{v} (current v{cur}); will migrate live before restart\n");
        }
    }
    println!(
        "this restarts the local twinstor daemon (storage and iSCSI stay up across the restart) \
         and blocks until this node reconverges.\n"
    );
    if !yes {
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

    // 2b. Migrate a stale DRBD .res template live before the restart, so the new
    // daemon starts on the current template. regenerate-config applies
    // disk-options changes via `drbdadm adjust` with no connection reset; for a
    // change that needs del-peer on a connected, serving dual-Primary it rolls
    // back to the consistent file and returns Ok after printing maintenance-window
    // guidance (see run_regenerate_config). It returns Err only on a real failure
    // (a rollback that itself failed, for example). That is reported but does not
    // abort the upgrade: the stale template is safe to run, since the daemon
    // enforces the safety policies live whatever the .res version.
    if let Some(v) = stale_version {
        if skip_config {
            println!("skipping .res migration (--skip-config); leaving template at v{v}.\n");
        } else {
            println!(
                "migrating DRBD .res template v{v} -> v{} (live)...",
                crate::setup::CONFIG_VERSION
            );
            if let Err(e) = run_regenerate_config(config.clone(), true, false).await {
                println!(
                    "\ncould not migrate the .res live: {e}\n  This is safe and does not hold the \
                     upgrade: the running daemon enforces the split-brain safety policies \
                     regardless of template version. To adopt the new template later, do it in a \
                     maintenance window one node at a time: `twinstor regenerate-config \
                     --persist-only --yes` then reboot that node, waiting for Connected + both \
                     UpToDate before the other.\n"
                );
            }
            println!();
        }
    }

    // 3. Restart the local daemon. Record the time first: the shutdown clears
    // our twinstor-alive stamp, so a stamp newer than this proves the new daemon
    // has initialized, not merely that systemctl started a process.
    let restart_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(now);
    let out = tokio::process::Command::new("systemctl")
        .args(["restart", "twinstor"])
        .output()
        .await
        .context("running systemctl restart twinstor")?;
    if !out.status.success() {
        bail!(
            "systemctl restart twinstor failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    // 4. Block until this node has reconverged: daemon active, DRBD Connected
    // and both UpToDate, and a new alive stamp since the restart. The last term
    // is what makes it safe to move on to the peer: until this node stamps
    // again, the peer's own peer-alive gate reads it as down.
    let start = std::time::Instant::now();
    let deadline = std::time::Duration::from_secs(timeout);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let active = tokio::process::Command::new("systemctl")
            .args(["is-active", "--quiet", "twinstor"])
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        let healthy = matches!(drbd.status().await, Ok(s)
            if s.connection == ConnectionState::Connected
                && s.disk == DiskState::UpToDate
                && s.peer_disk == Some(DiskState::UpToDate));
        let restamped = crate::xapi::local_daemon_alive_stamp()
            .await
            .is_some_and(|s| s >= restart_unix);
        if active && healthy && restamped {
            let (_, _, peer_version) = crate::xapi::peer_daemon_alive(&config.node.peer).await;
            let local_ver = env!("CARGO_PKG_VERSION");
            let tail = match peer_version.as_deref() {
                Some(pv) if pv == local_ver => {
                    format!("Both nodes are now on {local_ver}; rolling upgrade complete.")
                }
                Some(pv) => match crate::xapi::mixed_release_note(local_ver, pv) {
                    Some(note) => format!(
                        "The peer {} is on {pv}; install this bundle there and run `twinstor \
                         upgrade-apply` on it next. It runs {note}.",
                        config.node.peer
                    ),
                    None => format!(
                        "The peer {} is on {pv}; run `twinstor upgrade-apply` on it next.",
                        config.node.peer
                    ),
                },
                None => format!(
                    "Run `twinstor upgrade-apply` on {} next (its version could not be read).",
                    config.node.peer
                ),
            };
            println!(
                "done: daemon restarted and reconverged (Connected + UpToDate, re-stamped alive).\n{tail}"
            );
            return Ok(());
        }
        if start.elapsed() >= deadline {
            return Err(crate::render::CliError::new(
                format!("daemon restarted but did not reconverge within {timeout}s"),
                "check `twinstor status` and /var/log/twinstor.log on this node before touching \
                 the peer",
            )
            .explain(format!(
                "daemon active: {active}, DRBD healthy: {healthy}, re-stamped alive: {restamped}"
            ))
            .into());
        }
    }
}

async fn run_status(config: Arc<Config>, json: bool, plain: bool) -> Result<()> {
    use crate::drbd::DrbdControl;
    use crate::render::Sev;

    let drbd = DrbdController::new(&config);
    let res = format!("twinstor-{}", config.storage.drbd_resource);

    // Every probe below is wall-clock bounded and independent of the others,
    // so they run concurrently. `status` is the operator's first move during a
    // disk or toolstack failure, which is when many probes hit their bounds at
    // once: run in sequence the worst case adds up to minutes, run
    // concurrently it is the longest single bound, tens of seconds.
    let peer_ip = config.network.peer_address.to_string();
    let (
        drbd_status,
        (_, _, peer_version),
        reboot_guard,
        host_enabled,
        path_degraded,
        sync,
        (direct_ips, repl_path),
        iscsi_active,
        (mpath_answered, mpath),
        cache,
        daemon_active,
        pool_master,
        ha_enabled,
        capacity,
        res_version,
    ) = tokio::join!(
        // DRBD status
        async { drbd.status().await.ok() },
        // Peer daemon's advertised version (third element), for a version-skew
        // line during a rolling upgrade. None means the peer is unreachable or
        // unknown.
        crate::xapi::peer_daemon_alive(&config.node.peer),
        // Reboot-safety guard arming (systemd unit enabled and active, plus the
        // config flag), so a guard that is not armed shows up in `status`.
        crate::reboot_drain::reboot_guard_state(&config),
        // Storage readiness (see VM_PLACEMENT_SAFETY.md): toolstack
        // convergence (host enabled=true) ...
        crate::xapi::is_host_enabled(),
        // ... plus the consumer-side path health (multipath map, sessions,
        // active path). Without the local storage address the probe would
        // miscount paths: skip it and report unknown (fail open), same rule
        // as the daemon.
        async {
            match crate::multipath::local_ip_toward(&peer_ip).await {
                Some(local_ip) => crate::multipath::degradation(
                    &crate::multipath::probe(&local_ip, &peer_ip).await,
                ),
                None => None,
            }
        },
        // Sync progress (from raw drbdadm output)
        get_sync_progress(&res),
        // Topology, from the direct-link peer path that setup records:
        // arbitrated (two replication paths, direct link and switched network)
        // or fate-shared (single path). The choice is made once at setup and
        // easily forgotten, so status shows it. Then, in arbitrated mode, which
        // DRBD path is established: the direct link (the normal state) or the
        // switch fallback. `None` means fate-shared or not determinable.
        async {
            let direct_ips: Vec<String> =
                match tokio::fs::read_to_string(crate::setup::DIRECT_PEER_PATH).await {
                    Ok(text) => text
                        .lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty() && !l.starts_with('#'))
                        .map(str::to_string)
                        .collect(),
                    Err(_) => Vec::new(),
                };
            let repl_path: Option<(String, bool)> = if direct_ips.is_empty() {
                None
            } else {
                drbd.paths().await.ok().and_then(|paths| {
                    paths.iter().find(|p| p.established).map(|p| {
                        let on_direct = direct_ips.contains(&p.peer);
                        (p.peer.clone(), on_direct)
                    })
                })
            };
            (direct_ips, repl_path)
        },
        // iSCSI target status (tri-state: unknown when targetcli is wedged)
        is_iscsi_active(&config.iscsi.target_iqn),
        // Multipath paths
        get_multipath_status(),
        // Backing-disk volatile write cache (must be disabled for durability,
        // see write_cache.rs).
        crate::write_cache::status(&config.storage.backing_device),
        // Daemon running? (local systemctl, cannot hang on dead backing)
        async {
            tokio::process::Command::new("systemctl")
                .args(["is-active", "--quiet", "twinstor"])
                .status()
                .await
                .map(|s| s.success())
                .unwrap_or(false)
        },
        // Which node XAPI reports as the pool master, so `status` can be read
        // without cross-referencing hostnames (the DRBD block shows
        // Primary/Secondary roles, which are unrelated to pool master).
        crate::xapi::pool_master_hostname(),
        // Whether pool HA is armed. TwinStor relies on XHA for host failover
        // (and its isolated-master stand-down hands VMs to XHA), so disabled
        // HA means no automatic failover. None means XAPI could not answer.
        crate::xapi::is_ha_enabled(),
        // SR physical utilisation, which the daemon otherwise ignores. The
        // first symptom of a full SR is guest ENOSPC.
        crate::xapi::twinstor_sr_utilisation_pct(),
        // Generated-config staleness: the .res keeps its setup-time settings
        // across binary upgrades; surface a format mismatch.
        async {
            tokio::fs::read_to_string(format!("/etc/drbd.d/{res}.res"))
                .await
                .ok()
                .map(|text| crate::setup::res_config_version(&text))
        },
    );
    let arbitrated = !direct_ips.is_empty();
    let readiness =
        crate::readiness::assess(drbd_status.as_ref(), host_enabled, path_degraded.as_deref());

    // Identity: which host this is and its peer.
    let local_host = config.node.name.clone();
    let peer_host = config.node.peer.clone();

    if json {
        let drbd_obj = match &drbd_status {
            Some(s) => format!(
                r#"{{"role":"{}","disk":"{}","connection":"{}","peer_role":"{}","peer_disk":"{}"}}"#,
                fmt_field(&format!("{:?}", s.role)),
                fmt_field(&format!("{:?}", s.disk)),
                fmt_field(&format!("{:?}", s.connection)),
                fmt_field(&format!(
                    "{:?}",
                    s.peer_role.unwrap_or(crate::drbd::Role::Unknown)
                )),
                fmt_field(&format!(
                    "{:?}",
                    s.peer_disk.unwrap_or(crate::drbd::DiskState::Unknown)
                )),
            ),
            None => "null".to_string(),
        };
        let sync_obj = match &sync {
            Some((pct, eta)) => format!(r#"{{"percent":{},"eta":"{}"}}"#, pct, eta),
            None => "null".to_string(),
        };
        let mpath_obj = match &mpath {
            Some(paths) => format!(r#""{}""#, paths.replace('"', r#"\""#)),
            None => "null".to_string(),
        };
        let readiness_obj = format!(
            r#"{{"status":"{}","reason":"{}"}}"#,
            readiness.label(),
            readiness.reason().replace('"', r#"\""#),
        );
        let path_obj = match &path_degraded {
            Some(r) => format!(r#""{}""#, r.replace('"', r#"\""#)),
            None => r#""ok""#.to_string(),
        };
        let repl_path_obj = match &repl_path {
            Some((peer, on_direct)) => format!(
                r#"{{"active":"{}","peer":"{}","fallback":{}}}"#,
                if *on_direct { "direct" } else { "switch" },
                fmt_field(peer),
                !on_direct,
            ),
            None => "null".to_string(),
        };
        // The failback-state file is already JSON (daemon-written while on
        // the fallback path, absent otherwise): embed it verbatim.
        let failback_obj = crate::failback_state::read_raw()
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "null".to_string());
        let cache_obj = cache
            .iter()
            .map(|(d, s)| format!(r#"{{"disk":"{d}","write_cache":"{}"}}"#, s.word()))
            .collect::<Vec<_>>()
            .join(",");
        // The heal-state file is already JSON (written by the daemon during an
        // auto-heal episode, absent otherwise): embed it verbatim.
        let heal_obj = crate::heal_state::read_raw()
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "null".to_string());
        let config_obj = match res_version {
            Some(v) => format!(
                r#"{{"deployed":{v},"binary":{}}}"#,
                crate::setup::CONFIG_VERSION
            ),
            None => "null".to_string(),
        };
        let host_obj = format!(
            r#"{{"local":"{}","peer":"{}","pool_master":{},"ha_enabled":{},"ha_rearm_pending":{}}}"#,
            fmt_field(&local_host),
            fmt_field(&peer_host),
            match &pool_master {
                Some(m) => format!(r#""{}""#, fmt_field(m)),
                None => "null".to_string(),
            },
            match ha_enabled {
                Some(b) => b.to_string(),
                None => "null".to_string(),
            },
            crate::state_machine::ha_rearm_obligation_pending(),
        );
        let peer_version_obj = match &peer_version {
            Some(v) => format!(r#""{}""#, fmt_field(v)),
            None => "null".to_string(),
        };
        println!(
            r#"{{"version":"{}","peer_version":{},"reboot_guard":"{}","resource":"{}","topology":"{}","host":{},"daemon":{},"readiness":{},"drbd":{},"sync":{},"replication_path":{},"failback":{},"iscsi":{},"capacity_pct":{},"multipath":{},"path_health":{},"auto_heal":{},"config_version":{},"backing_disks":[{}]}}"#,
            env!("CARGO_PKG_VERSION"),
            peer_version_obj,
            reboot_guard.tag(),
            res,
            if arbitrated {
                "arbitrated"
            } else {
                "fate-shared"
            },
            host_obj,
            daemon_active,
            readiness_obj,
            drbd_obj,
            sync_obj,
            repl_path_obj,
            failback_obj,
            iscsi_json(iscsi_active),
            capacity
                .map(|p| p.to_string())
                .unwrap_or_else(|| "null".to_string()),
            mpath_obj,
            path_obj,
            heal_obj,
            config_obj,
            cache_obj
        );
        return Ok(());
    }

    // Auto-heal episode, if one is open (also read by the human block below).
    let heal = crate::heal_state::read();

    // Fence-peer tiebreaker: the same probe set the handler would use at
    // fence time. With nothing reachable a partition freezes this node
    // instead of letting it serve, so surface the state before it matters.
    let mut tb_targets: Vec<String> = Vec::new();
    if let Some(gw) = crate::setup::detect_default_gateway().await {
        tb_targets.push(format!("{gw} (gateway)"));
    }
    if let Ok(text) = tokio::fs::read_to_string(crate::setup::PING_TARGETS_PATH).await {
        tb_targets.extend(crate::setup::parse_ping_targets(&text));
    }

    // Kernel/module coverage: warn before a reboot into a kernel that has no
    // DRBD module (a host that boots without storage after a kernel update).
    let missing_kernels = kernels_missing_drbd(std::path::Path::new("/lib/modules"));

    // Per-field severities. These drive the field colors, the section
    // markers, and the top-line verdict, so a value can never read green in
    // one place and red in another.
    let syncing = sync.is_some();
    let sev_local_disk = |d: crate::drbd::DiskState| -> Sev {
        use crate::drbd::DiskState::*;
        match d {
            UpToDate => Sev::Good,
            Consistent | Outdated | Unknown => Sev::Warn,
            Inconsistent | Diskless => Sev::Bad,
        }
    };
    let sev_peer_disk = |d: crate::drbd::DiskState| -> Sev {
        use crate::drbd::DiskState::*;
        match d {
            UpToDate => Sev::Good,
            // Inconsistent is the normal look of a resync target; without a
            // resync running it means the peer copy is unusable.
            Inconsistent if syncing => Sev::Warn,
            Inconsistent | Diskless => Sev::Bad,
            Consistent | Outdated | Unknown => Sev::Warn,
        }
    };
    let sev_conn = |c: crate::drbd::ConnectionState| -> Sev {
        use crate::drbd::ConnectionState::*;
        match c {
            Connected => Sev::Good,
            StandAlone => Sev::Bad,
            Connecting | Disconnecting | Unknown => Sev::Warn,
        }
    };
    let cache_sev = |s: &crate::write_cache::CacheState| -> Sev {
        use crate::write_cache::CacheState::*;
        match s {
            NoVolatileCache | Disabled => Sev::Good,
            Enabled => Sev::Bad,
            Unknown(_) => Sev::Warn,
        }
    };

    let drbd_sev = match &drbd_status {
        None => Sev::Bad,
        Some(s) => sev_local_disk(s.disk)
            .max(sev_conn(s.connection))
            .max(sev_peer_disk(
                s.peer_disk.unwrap_or(crate::drbd::DiskState::Unknown),
            )),
    };
    let readiness_sev = match &readiness {
        crate::readiness::Readiness::Ready => Sev::Good,
        crate::readiness::Readiness::NotReady { .. } => Sev::Bad,
        _ => Sev::Warn,
    };
    let iscsi_sev = iscsi_sev(iscsi_active);
    let worst_cache = cache
        .iter()
        .map(|(_, s)| cache_sev(s))
        .max()
        .unwrap_or(Sev::Good);
    let mp_sev = match path_degraded.as_deref() {
        // Probe skipped (no local IP toward the peer, fail open) and no map
        // visible: unknown is not "ok".
        None if mpath.is_none() => Sev::Warn,
        None => Sev::Good,
        // Nothing visible at all is worse than a map of the wrong shape.
        Some(r) if r.starts_with("no iSCSI path devices") || r.starts_with("no multipath map") => {
            Sev::Bad
        }
        Some(_) => Sev::Warn,
    };
    let tb_sev = if tb_targets.is_empty() {
        Sev::Bad
    } else {
        Sev::Good
    };
    let cap_sev = match capacity {
        Some(p) if p >= 95 => Sev::Bad,
        Some(p) if p >= 80 => Sev::Warn,
        _ => Sev::Good,
    };
    let config_stale = res_version.is_some_and(|v| v != crate::setup::CONFIG_VERSION);
    let on_fallback = matches!(&repl_path, Some((_, false)));

    // The verdict goes critical only when the node is not safely serving
    // storage; anything broken-but-still-serving reads degraded, and the red
    // field itself points at the line to fix.
    // An unanswered iSCSI probe (targetcli wedged on dead backing) is not
    // "not exported": it degrades the verdict but does not make it critical.
    let critical = !daemon_active
        || drbd_status.is_none()
        || !readiness.can_serve()
        || iscsi_active == Some(false);
    let degraded = drbd_sev > Sev::Good
        || readiness_sev > Sev::Good
        || iscsi_active.is_none()
        || syncing
        || heal.is_some()
        || ha_enabled != Some(true)
        || worst_cache > Sev::Good
        || mp_sev > Sev::Good
        || tb_sev > Sev::Good
        || on_fallback
        || config_stale
        || cap_sev > Sev::Good
        || !missing_kernels.is_empty();

    let (verdict_sev, verdict_word, verdict_reason) = if critical {
        let reason = if !daemon_active {
            "daemon not running (storage unprotected)".to_string()
        } else if drbd_status.is_none() {
            "cannot read DRBD status".to_string()
        } else if !readiness.can_serve() {
            format!("not serving ({})", readiness.reason())
        } else {
            "not serving (iSCSI target not exported)".to_string()
        };
        (Sev::Bad, "critical", reason)
    } else if degraded {
        let reason = if let Some((pct, _)) = &sync {
            format!("resyncing (peer catching up, {pct:.1}%)")
        } else if readiness_sev > Sev::Good {
            readiness.reason().to_string()
        } else if iscsi_active.is_none() {
            "iSCSI target state unknown (targetcli timed out or could not run; backing may be wedged)"
                .to_string()
        } else if let Some(h) = &heal {
            format!("auto-heal in progress: {}", h.summary)
        } else if ha_enabled == Some(false) {
            "pool HA disabled (no automatic host failover)".to_string()
        } else if ha_enabled.is_none() {
            "pool HA unknown (XAPI did not answer)".to_string()
        } else if cap_sev > Sev::Good {
            format!(
                "SR {}% full (guests hit ENOSPC when it fills)",
                capacity.unwrap_or(0)
            )
        } else if worst_cache > Sev::Good {
            "volatile write cache on a backing disk (not durable)".to_string()
        } else if tb_targets.is_empty() {
            "no partition tiebreaker configured".to_string()
        } else if let Some(r) = &path_degraded {
            r.clone()
        } else if !mpath_answered && mp_sev > Sev::Good {
            "multipath state unknown (multipath -ll did not answer)".to_string()
        } else if mpath.is_none() && mp_sev > Sev::Good {
            "no twinstor multipath device visible (path probe skipped)".to_string()
        } else if on_fallback {
            "replication on switch fallback".to_string()
        } else if config_stale {
            format!(
                "DRBD config file format v{} behind this binary (v{})",
                res_version.unwrap_or(0),
                crate::setup::CONFIG_VERSION
            )
        } else if !missing_kernels.is_empty() {
            "installed kernel(s) without a DRBD module".to_string()
        } else {
            "replication degraded (see drbd section)".to_string()
        };
        (Sev::Warn, "degraded", reason)
    } else {
        (Sev::Good, "healthy", "serving, peer redundant".to_string())
    };

    if plain {
        // Machine mode: one key<TAB>value record per line, stable keys, no
        // color, no alignment. Absent sections (no resync, no heal episode,
        // fate-shared replication) omit their keys entirely.
        println!("health\t{verdict_word}");
        println!("health_reason\t{verdict_reason}");
        println!("version\t{}", env!("CARGO_PKG_VERSION"));
        println!(
            "peer_version\t{}",
            peer_version.as_deref().unwrap_or("unknown")
        );
        println!("reboot_guard\t{}", reboot_guard.tag());
        println!("resource\t{res}");
        println!(
            "topology\t{}",
            if arbitrated {
                "arbitrated"
            } else {
                "fate-shared"
            }
        );
        println!(
            "daemon\t{}",
            if daemon_active { "running" } else { "stopped" }
        );
        println!("host\t{local_host}");
        println!("peer\t{peer_host}");
        println!(
            "pool_master\t{}",
            pool_master.as_deref().unwrap_or("unknown")
        );
        println!(
            "pool_ha\t{}",
            match ha_enabled {
                Some(true) => "enabled",
                Some(false) => "disabled",
                None => "unknown",
            }
        );
        println!(
            "ha_rearm_pending\t{}",
            crate::state_machine::ha_rearm_obligation_pending()
        );
        match &drbd_status {
            Some(s) => {
                println!("drbd_role\t{}", fmt_field(&format!("{:?}", s.role)));
                println!("drbd_disk\t{}", fmt_field(&format!("{:?}", s.disk)));
                println!(
                    "drbd_connection\t{}",
                    fmt_field(&format!("{:?}", s.connection))
                );
                println!(
                    "drbd_peer_role\t{}",
                    fmt_field(&format!(
                        "{:?}",
                        s.peer_role.unwrap_or(crate::drbd::Role::Unknown)
                    ))
                );
                println!(
                    "drbd_peer_disk\t{}",
                    fmt_field(&format!(
                        "{:?}",
                        s.peer_disk.unwrap_or(crate::drbd::DiskState::Unknown)
                    ))
                );
            }
            None => println!("drbd\tunavailable"),
        }
        if let Some((pct, eta)) = &sync {
            println!("sync_percent\t{pct:.1}");
            println!("sync_eta\t{eta}");
        }
        if let Some((peer, on_direct)) = &repl_path {
            println!(
                "repl_path\t{}",
                if *on_direct { "direct" } else { "switch" }
            );
            println!("repl_peer\t{peer}");
            if !on_direct && let Some(fb) = crate::failback_state::read() {
                println!("failback_mode\t{}", fb.mode);
                println!("failback_reason\t{}", fb.reason);
            }
        }
        println!("readiness\t{}", readiness.label());
        if !readiness.reason().is_empty() {
            println!("readiness_reason\t{}", readiness.reason());
        }
        if let Some(h) = &heal {
            println!("heal_stage\t{}", h.stage);
            println!("heal_summary\t{}", h.summary);
        }
        if let Some(v) = res_version {
            println!("config_deployed\t{v}");
            println!("config_binary\t{}", crate::setup::CONFIG_VERSION);
        }
        for (disk, state) in &cache {
            println!("write_cache\t{disk}={}", state.word());
        }
        println!("iscsi\t{}", iscsi_word(iscsi_active));
        println!(
            "capacity\t{}",
            capacity
                .map(|p| format!("{p}%"))
                .unwrap_or_else(|| "unknown".to_string())
        );
        println!(
            "multipath\t{}",
            match (&path_degraded, &mpath) {
                (Some(_), _) => "degraded",
                (None, None) => "unknown",
                (None, Some(_)) => "ok",
            }
        );
        if let Some(r) = &path_degraded {
            println!("multipath_reason\t{r}");
        }
        println!(
            "tiebreaker\t{}",
            if tb_targets.is_empty() {
                "none".to_string()
            } else {
                tb_targets.join(", ")
            }
        );
        if !missing_kernels.is_empty() {
            println!("kernels_missing_drbd\t{}", missing_kernels.join(", "));
        }
        return Ok(());
    }

    // Human-readable output. State-carrying lines get a health marker in the
    // left gutter; every other line gets a blank gutter of the same width so
    // the label column stays put. The verdict line comes first, as the title.
    let st = crate::render::Style::for_stdout();
    let g = st.gutter();
    let sub = format!("{g}  ");

    println!("{}", st.verdict(verdict_sev, verdict_word, &verdict_reason));
    println!();
    println!("{g}version:     {}", env!("CARGO_PKG_VERSION"));
    match peer_version.as_deref() {
        Some(pv) if pv == env!("CARGO_PKG_VERSION") => {
            println!("{}peer version: {pv}", st.marker(Sev::Good))
        }
        Some(pv) => match crate::xapi::mixed_release_note(env!("CARGO_PKG_VERSION"), pv) {
            Some(note) => println!(
                "{}peer version: {pv}  (mixed releases: the peer runs {note})",
                st.marker(Sev::Warn)
            ),
            None => println!(
                "{}peer version: {pv}  (version skew: finish the rolling upgrade; both nodes should be on {})",
                st.marker(Sev::Warn),
                env!("CARGO_PKG_VERSION")
            ),
        },
        None => println!("{}peer version: unknown", st.marker(Sev::Good)),
    }
    println!(
        "{g}daemon:      {}",
        if daemon_active {
            st.paint(Sev::Good, "running")
        } else {
            st.paint(Sev::Bad, "stopped")
        }
    );
    match reboot_guard {
        crate::reboot_drain::GuardState::Armed => {
            println!("{}reboot guard: armed", st.marker(Sev::Good))
        }
        crate::reboot_drain::GuardState::OffByConfig => {
            println!(
                "{}reboot guard: off (disabled in config)",
                st.marker(Sev::Good)
            )
        }
        crate::reboot_drain::GuardState::NotArmed => println!(
            "{}reboot guard: not armed; run: systemctl enable --now twinstor-reboot-drain",
            st.marker(Sev::Warn)
        ),
    }
    println!("{g}resource:    {res}");
    if arbitrated {
        println!(
            "{g}topology:    {}",
            st.paint(Sev::Good, "arbitrated (direct link + switched network)")
        );
    } else {
        println!("{g}topology:    fate-shared (single replication path)");
    }
    println!("{g}host:        {local_host} (this node), peer {peer_host}");
    match &pool_master {
        Some(m) if *m == local_host => println!("{g}pool master: {m} (this node)"),
        Some(m) if *m == peer_host => println!("{g}pool master: {m} (peer)"),
        Some(m) => println!("{g}pool master: {m}"),
        None => println!(
            "{g}pool master: {}",
            st.paint(Sev::Warn, "unknown (XAPI did not answer)")
        ),
    }
    match ha_enabled {
        Some(true) => println!("{g}pool HA:     {}", st.paint(Sev::Good, "enabled")),
        Some(false) => println!(
            "{g}pool HA:     {}",
            st.paint(
                Sev::Warn,
                "disabled: no automatic host failover (enable HA for failover)"
            )
        ),
        None => {
            println!(
                "{g}pool HA:     {}",
                st.paint(Sev::Warn, "unknown (XAPI did not answer)")
            )
        }
    }
    if crate::state_machine::ha_rearm_obligation_pending() {
        println!(
            "{g}ha re-arm:   {}",
            st.paint(
                Sev::Warn,
                "pending: TwinStor disarmed HA itself and is re-arming it automatically (no action needed)"
            )
        );
    }
    println!();

    match &drbd_status {
        Some(s) => {
            println!("{}drbd:", st.marker(drbd_sev));
            println!("{sub}role:       {:?}", s.role);
            println!(
                "{sub}disk:       {}",
                st.paint(sev_local_disk(s.disk), &format!("{:?}", s.disk))
            );
            println!(
                "{sub}connection: {}",
                st.paint(sev_conn(s.connection), &format!("{:?}", s.connection))
            );
            println!(
                "{sub}peer role:  {:?}",
                s.peer_role.unwrap_or(crate::drbd::Role::Unknown)
            );
            let pd = s.peer_disk.unwrap_or(crate::drbd::DiskState::Unknown);
            println!(
                "{sub}peer disk:  {}",
                st.paint(sev_peer_disk(pd), &format!("{pd:?}"))
            );
            if let Some((pct, eta)) = &sync {
                println!(
                    "{sub}sync:       {}",
                    st.paint(Sev::Warn, &format!("{pct:.1}% done, ETA {eta}"))
                );
            }
            if let Some((peer, on_direct)) = &repl_path {
                if *on_direct {
                    println!("{sub}repl path:  direct link ({peer})");
                } else {
                    println!(
                        "{sub}repl path:  switch ({peer}), {}",
                        st.paint(Sev::Warn, "fallback")
                    );
                    match crate::failback_state::read() {
                        Some(fb) if fb.mode == "holding" => println!(
                            "{sub}failback:   in ~{}s (stability hold; force now: \
                             twinstor failback-replication)",
                            fb.eta_secs
                        ),
                        Some(fb) => println!(
                            "{sub}failback:   {}: {}",
                            st.paint(Sev::Warn, &fb.mode),
                            fb.reason
                        ),
                        None => println!(
                            "{sub}failback:   {}",
                            st.paint(
                                Sev::Warn,
                                "state unknown (daemon evaluating; check cable/NICs)"
                            )
                        ),
                    }
                }
            }
        }
        None => println!(
            "{}drbd:        {}",
            st.marker(Sev::Bad),
            st.paint(Sev::Bad, "not available")
        ),
    }

    println!();
    match &readiness {
        crate::readiness::Readiness::Ready => {
            println!(
                "{}readiness:   {} (local serving, peer redundant)",
                st.marker(Sev::Good),
                st.paint(Sev::Good, "ready")
            );
        }
        other => {
            println!(
                "{}readiness:   {}: {}",
                st.marker(readiness_sev),
                st.paint(readiness_sev, other.label()),
                other.reason()
            );
        }
    }

    // Auto-heal episode in progress: the resolution ladder is slow, so tell
    // the operator the daemon is working on it and that manual recovery
    // commands would interfere.
    if let Some(h) = &heal {
        let now = crate::heal_state::unix_now();
        let age = now.saturating_sub(h.updated_unix);
        let mins = now.saturating_sub(h.started_unix) / 60;
        let cont = format!("{g}             ");
        println!();
        println!(
            "{}auto-heal:   {} [{}]",
            st.marker(Sev::Warn),
            h.summary,
            h.stage
        );
        println!(
            "{cont}running for ~{mins} min (attempts={} restarts={} holds={})",
            h.attempts, h.restarts, h.holds
        );
        if let Some(n) = &h.next {
            println!("{cont}next: {n}");
        }
        if age > 300 {
            println!(
                "{cont}last update {age}s ago: the daemon may have restarted; \
                 check /var/log/twinstor.log"
            );
        }
        println!(
            "{cont}{}",
            st.bold("auto-recovery in progress; do not run manual drbdadm/xe recovery commands")
        );
    }

    if config_stale {
        println!();
        println!(
            "{}config:      {}",
            st.marker(Sev::Warn),
            st.paint(
                Sev::Warn,
                &format!(
                    "DRBD resource file format v{}, this binary generates v{}; the \
                     file keeps its setup-time settings; handler scripts are auto-refreshed at \
                     daemon start (see release notes for whether the .res needs regenerating)",
                    res_version.unwrap_or(0),
                    crate::setup::CONFIG_VERSION
                )
            )
        );
    }

    if !cache.is_empty() {
        println!();
        for (disk, state) in &cache {
            let cs = cache_sev(state);
            println!(
                "{}backing:     {disk}: write cache {}",
                st.marker(cs),
                st.paint(cs, state.label())
            );
        }
    }

    println!();
    println!(
        "{}iscsi:       {}",
        st.marker(iscsi_sev),
        st.paint(iscsi_sev, iscsi_word(iscsi_active))
    );
    println!(
        "{}capacity:    {}",
        st.marker(cap_sev),
        match capacity {
            Some(p) => st.paint(cap_sev, &format!("{p}% used")),
            None => st.paint(Sev::Warn, "unknown (XAPI did not answer)"),
        }
    );

    println!();
    match &mpath {
        Some(info) => {
            let summary = match &path_degraded {
                None => st.paint(Sev::Good, "ok"),
                Some(r) => st.paint(mp_sev, &format!("degraded: {r}")),
            };
            println!("{}multipath:   {}", st.marker(mp_sev), summary);
            for line in info.lines() {
                println!("{sub}{}", st.dim(line));
            }
        }
        None => {
            let what = if mpath_answered {
                "no twinstor device found"
            } else {
                "unknown (multipath -ll did not answer)"
            };
            match &path_degraded {
                Some(r) => println!(
                    "{}multipath:   {what}; {}",
                    st.marker(mp_sev),
                    st.paint(mp_sev, &format!("degraded: {r}"))
                ),
                None => println!("{}multipath:   {what}", st.marker(mp_sev)),
            }
        }
    }

    println!();
    if tb_targets.is_empty() {
        println!(
            "{}tiebreaker:  {}",
            st.marker(Sev::Bad),
            st.paint(
                Sev::Bad,
                &format!(
                    "none (no default gateway, no ping targets): any replication \
                     interruption freezes this node; add an always-on local IP to {}",
                    crate::setup::PING_TARGETS_PATH
                )
            )
        );
    } else {
        println!(
            "{}tiebreaker:  {}",
            st.marker(Sev::Good),
            st.paint(Sev::Good, &tb_targets.join(", "))
        );
    }

    if !missing_kernels.is_empty() {
        println!();
        println!(
            "{}WARNING:     {}",
            st.marker(Sev::Bad),
            st.paint(
                Sev::Bad,
                &format!(
                    "installed kernel(s) without a DRBD module: {}; rebooting \
                     into one of these bricks this node's storage; install a matching \
                     kmod-drbd first",
                    missing_kernels.join(", ")
                )
            )
        );
    }

    Ok(())
}

/// Installed kernels (directories under `root`, normally /lib/modules) that
/// contain no drbd kernel module. The kmod package installs to `extra/drbd/`
/// and kABI-compatible kernels get `weak-updates/` symlinks; a kernel with
/// neither must not be booted. Only the well-known subdirectories are scanned,
/// so the check is fast.
fn kernels_missing_drbd(root: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut missing = Vec::new();
    for e in entries.flatten() {
        let kdir = e.path();
        if !kdir.is_dir() {
            continue;
        }
        let has_module = ["extra/drbd", "weak-updates/drbd", "updates/drbd", "extra"]
            .iter()
            .any(|sub| {
                std::fs::read_dir(kdir.join(sub)).ok().is_some_and(|d| {
                    d.flatten()
                        .any(|f| f.file_name().to_string_lossy().starts_with("drbd.ko"))
                })
            });
        if !has_module {
            missing.push(e.file_name().to_string_lossy().to_string());
        }
    }
    missing.sort();
    missing
}

fn fmt_field(s: &str) -> String {
    s.to_lowercase()
}

async fn get_sync_progress(resource: &str) -> Option<(f64, String)> {
    let out = probe_output("drbdadm", &["status", resource], 15).await?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.contains("done:") {
            let pct = trimmed
                .split("done:")
                .nth(1)?
                .split_whitespace()
                .next()?
                .parse::<f64>()
                .ok()?;
            let eta = if trimmed.contains("eta:") {
                trimmed
                    .split("eta:")
                    .nth(1)
                    .unwrap_or("unknown")
                    .split_whitespace()
                    .next()
                    .unwrap_or("unknown")
                    .to_string()
            } else {
                "unknown".to_string()
            };
            return Some((pct, eta));
        }
    }
    None
}

/// Run a read-only status probe with a hard timeout, killing the child on expiry.
/// `twinstor status` must not hang: targetcli, multipath and drbdadm all block
/// indefinitely on dead backing (pulled disks), and status is the operator's first
/// move during a disk failure. `None` means the command could not be spawned or
/// timed out, and is rendered as unknown.
async fn probe_output(cmd: &str, args: &[&str], secs: u64) -> Option<std::process::Output> {
    let child = tokio::process::Command::new(cmd)
        .args(args)
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(std::time::Duration::from_secs(secs), child).await {
        Ok(Ok(out)) => Some(out),
        Ok(Err(e)) => {
            tracing::warn!(
                command = %format!("{cmd} {}", args.join(" ")),
                error = %e,
                "status probe could not run; reporting unknown"
            );
            None
        }
        Err(_) => {
            tracing::warn!(
                command = %format!("{cmd} {}", args.join(" ")),
                timeout_secs = secs,
                "status probe timed out (backing likely wedged); reporting unknown"
            );
            None
        }
    }
}

/// Whether the iSCSI target is exported. `None` means targetcli did not answer
/// (timed out or could not run), which is not "not exported": on dead backing
/// the target is usually still exported with a wedged backstore, and reporting
/// it missing would push the operator to re-export it. Unknown is never
/// collapsed into false.
async fn is_iscsi_active(iqn: &str) -> Option<bool> {
    match probe_output("targetcli", &["ls /iscsi"], 10).await {
        Some(o) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).contains(iqn)),
        // targetcli ran and exited non-zero (no /iscsi tree at all, for
        // example): a real answer, the target is not exported.
        Some(_) => Some(false),
        None => None,
    }
}

/// Rendering of the tri-state iSCSI probe, shared by the JSON, plain and
/// human outputs so unknown cannot read as a confirmed answer in one mode and
/// as unknown in another.
fn iscsi_sev(active: Option<bool>) -> crate::render::Sev {
    use crate::render::Sev;
    match active {
        Some(true) => Sev::Good,
        Some(false) => Sev::Bad,
        None => Sev::Warn,
    }
}

fn iscsi_word(active: Option<bool>) -> &'static str {
    match active {
        Some(true) => "active",
        Some(false) => "inactive",
        None => "unknown",
    }
}

fn iscsi_json(active: Option<bool>) -> &'static str {
    match active {
        Some(true) => "true",
        Some(false) => "false",
        None => "null",
    }
}

/// `(answered, map)`: `answered == false` means `multipath -ll` did not answer
/// (timed out or could not run), which renders as unknown, unlike an answered
/// probe that shows no twinstor map. The bound matches the daemon's path probe
/// (`multipath::probe`, 10 s): `-ll` runs the path checkers synchronously, and
/// a checker on a session in recovery blocks for the iSCSI replacement
/// timeout, so a tighter bound reports the first seconds of a normal peer
/// outage as "no device found".
async fn get_multipath_status() -> (bool, Option<String>) {
    let Some(out) = probe_output("multipath", &["-ll"], 10).await else {
        return (false, None);
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut result = Vec::new();
    let mut capture = false;
    for line in text.lines() {
        if line.contains("twinstor") || line.contains("LIO-ORG") {
            capture = true;
            result.push(line.to_string());
        } else if capture {
            if line.starts_with(' ')
                || line.starts_with('\t')
                || line.starts_with('`')
                || line.starts_with('|')
            {
                result.push(line.to_string());
            } else {
                break;
            }
        }
    }
    (
        true,
        if result.is_empty() {
            None
        } else {
            Some(result.join("\n"))
        },
    )
}

/// Parse the (local, peer) DRBD node-ids from an existing `.res`, or `None` if it
/// has no explicit `node-id` (a fate-shared pool created before setup wrote them).
/// `regenerate-config` uses this to preserve the pool's ids: writing a
/// sorted-hostname id over one DRBD assigned differently would mismatch the on-disk
/// metadata and break the resource. Keeping an id-less pool id-less is safe: the
/// daemon reads the real id from DRBD at runtime.
///
/// Assumes `node-id N;` on its own line, which is how setup.rs renders it; a
/// hand-edited inline `node-id 1; device ...` parses as id-less, which is the safe
/// direction (regenerate keeps it id-less, the daemon still reads the real id).
fn parse_res_node_ids(text: &str, local: &str, peer: &str) -> Option<(u32, u32)> {
    let (mut cur, mut local_id, mut peer_id): (Option<bool>, Option<u32>, Option<u32>) =
        (None, None, None);
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("on ") {
            let name = rest.trim_end_matches('{').trim();
            cur = if name == local {
                Some(true)
            } else if name == peer {
                Some(false)
            } else {
                None
            };
        } else if let Some(rest) = t.strip_prefix("node-id ") {
            let id = rest.trim_end_matches(';').trim().parse().ok();
            match cur {
                Some(true) => local_id = id,
                Some(false) => peer_id = id,
                None => {}
            }
        }
    }
    match (local_id, peer_id) {
        (Some(l), Some(p)) => Some((l, p)),
        _ => None,
    }
}

/// Rebuild the DRBD `.res` text from the current template, recovering topology
/// (DRBD device, backings, and the address path-pairs) from the existing file
/// `old`. Shared by `regenerate-config` (topology preserved, pass
/// `local_backing_override = None`) and `replace-backing` (this node's backing
/// device swapped for the replacement, pass `Some(new_device)`). The peer's
/// backing is always preserved from the parsed file; only this node's local
/// disk line changes when an override is given.
async fn rebuild_res_text(
    config: &Config,
    old: &str,
    local_backing_override: Option<&str>,
) -> Result<String> {
    use anyhow::bail;
    let res = format!("twinstor-{}", config.storage.drbd_resource);
    let res = res.as_str();
    let local = &config.node.name;
    let peer = &config.node.peer;
    let parsed = crate::setup::parse_res_file(old, local, peer);
    let (Some(parsed_local_backing), Some(drbd_dev)) =
        (parsed.local_backing.clone(), parsed.drbd_dev.clone())
    else {
        bail!("could not parse backing/device from the DRBD resource file; refusing")
    };
    // The effective local backing: the replacement (replace-backing) or the
    // existing one (regenerate). The peer keeps its own recorded path.
    let local_backing = local_backing_override
        .map(str::to_string)
        .unwrap_or(parsed_local_backing);
    let peer_backing = parsed
        .peer_backing
        .clone()
        .unwrap_or_else(|| local_backing.clone());

    // Addresses: recover the two path pairs from the existing file. The
    // fallback pair contains the configured peer_address; the remainder (if
    // any) is the direct pair.
    let peer_ip: std::net::IpAddr = config.network.peer_address;
    let port = config.network.drbd_port;
    let mut fallback_local: Option<std::net::IpAddr> = None;
    let mut direct: Vec<std::net::IpAddr> = Vec::new();
    for a in &parsed.addresses {
        let Some((ip, _)) = a.rsplit_once(':') else {
            continue;
        };
        let Ok(ip) = ip.parse::<std::net::IpAddr>() else {
            continue;
        };
        if ip == peer_ip {
            continue;
        }
        // Only direct versus fallback matters here, decided by matching the
        // subnet of peer_address (the same /24 heuristic setup uses).
        if format!("{ip}")
            .rsplit_once('.')
            .map(|(pfx, _)| pfx.to_string())
            == format!("{peer_ip}")
                .rsplit_once('.')
                .map(|(pfx, _)| pfx.to_string())
        {
            fallback_local = Some(ip);
        } else {
            direct.push(ip);
        }
    }
    let direct_pair = if direct.len() == 2 {
        // Deterministic local/peer assignment: our own address is the one
        // bound on this host.
        let mine = direct
            .iter()
            .find(|ip| std::net::UdpSocket::bind((**ip, 0)).is_ok())
            .copied();
        match mine {
            Some(m) => Some((m, *direct.iter().find(|i| **i != m).unwrap())),
            None => bail!("cannot determine which direct address is local; refusing"),
        }
    } else if direct.is_empty() {
        None
    } else {
        bail!("unexpected direct-address count {}; refusing", direct.len())
    };

    let Some(local_ip) = fallback_local else {
        bail!("could not recover the local fallback address from the resource file; refusing")
    };

    // Preserve the pool's existing node-ids. Never reassign: a sorted-hostname id
    // over a differently assigned metadata id would break the resource.
    let node_ids = parse_res_node_ids(old, local, peer);
    let topology = crate::setup::drbd_topology(
        local,
        peer,
        &drbd_dev,
        &local_backing,
        local_ip,
        peer_ip,
        port,
        direct_pair,
        node_ids,
    );
    // drbd_topology renders our backing for both host sections; the peer's
    // section must keep its own path when they differ. The local host renders
    // first, so only the second (peer) disk line is patched.
    let topology = if peer_backing != local_backing {
        let needle = format!("disk \"{local_backing}\"");
        match topology.match_indices(&needle).nth(1).map(|(i, _)| i) {
            Some(i) => {
                let mut t = topology.clone();
                t.replace_range(i..i + needle.len(), &format!("disk \"{peer_backing}\""));
                t
            }
            None => bail!("could not locate the peer disk line to patch; refusing"),
        }
    } else {
        topology
    };

    let link = crate::setup::detect_link_speed_mbps(peer_ip).await;
    let fill = crate::setup::resync_fill_target(link);
    let min_rate = crate::setup::resync_min_rate(link, direct_pair.is_some());
    let fencing = if direct_pair.is_some() {
        "resource-and-stonith"
    } else {
        "resource-only"
    };
    Ok(crate::setup::render_res_file(
        res, fencing, fill, &min_rate, &topology,
    ))
}

/// Replace the value of the `backing_device = "..."` line in a twinstor.toml
/// body, preserving every other line (comments, formatting, the CHAP secret)
/// verbatim. A line edit rather than a serde round-trip, because the config
/// carries comments and a field order that re-serializing (with `toml`, not
/// `toml_edit`) would lose.
///
/// The key lives under `[storage]` and appears once in a twinstor-generated
/// file; this matches a line whose trimmed form starts with `backing_device`
/// followed by `=`. Errors if the key is absent (a hand-mangled file) or
/// appears more than once (ambiguous, so refuse rather than guess).
fn replace_backing_device_line(toml: &str, new_device: &str) -> Result<String> {
    use anyhow::bail;
    let is_backing_line = |line: &str| {
        let t = line.trim_start();
        t.strip_prefix("backing_device")
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    };
    let matches = toml.lines().filter(|l| is_backing_line(l)).count();
    match matches {
        0 => bail!(
            "no `backing_device = ...` line found in the config; refusing to edit \
             (is this a twinstor.toml?)"
        ),
        1 => {}
        n => bail!("found {n} `backing_device` lines in the config; refusing (ambiguous)"),
    }
    // Preserve the original indentation of the matched line.
    let out: Vec<String> = toml
        .lines()
        .map(|line| {
            if is_backing_line(line) {
                let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
                format!("{indent}backing_device = \"{new_device}\"")
            } else {
                line.to_string()
            }
        })
        .collect();
    // Preserve a trailing newline if the input had one (lines() drops it).
    let mut result = out.join("\n");
    if toml.ends_with('\n') {
        result.push('\n');
    }
    Ok(result)
}

/// `twinstor replace-backing <new-device>`: re-point twinstor at a replaced
/// backing device in one step. This is the recovery for a disk swap or an
/// mdadm array recreate that created a new /dev/disk/by-id path: the stored
/// `backing_device` no longer resolves, so DRBD cannot attach a local replica
/// and the node stops resyncing. Updates `backing_device` in twinstor.toml
/// (atomic temp file and rename, preserving comments and the CHAP secret),
/// regenerates the DRBD `.res` with the new local disk, then runs `drbdadm
/// adjust` (and `attach` if still detached) so DRBD attaches the empty
/// replacement and the automatic full resync from the peer begins. Not gated
/// on both UpToDate, since the local disk is expected to be missing; it only
/// refuses under an open heal episode unless `--yes`.
async fn run_replace_backing(
    config: Arc<Config>,
    config_path: PathBuf,
    new_device: PathBuf,
    yes: bool,
) -> Result<()> {
    use crate::drbd::DrbdControl;
    use anyhow::{Context, bail};
    use std::io::Write;
    use std::os::unix::fs::FileTypeExt;

    // 1. Validate the replacement device exists and is a block device.
    let meta = std::fs::metadata(&new_device)
        .with_context(|| format!("{} does not exist", new_device.display()))?;
    if !meta.file_type().is_block_device() {
        bail!("{} is not a block device", new_device.display());
    }
    let new_backing = new_device.display().to_string();
    let old_backing = config.storage.backing_device.display().to_string();
    if old_backing == new_backing {
        bail!(
            "backing_device is already {new_backing}; nothing to do \
             (if DRBD is detached, `drbdadm attach` the resource)"
        );
    }

    let res = format!("twinstor-{}", config.storage.drbd_resource);
    let res = res.as_str();
    let res_path = format!("/etc/drbd.d/{res}.res");

    // 2. Refuse under an open heal episode unless --yes: an active incident is
    // resolving DRBD state and a topology change would fight it.
    let episode_open = std::path::Path::new("/var/lib/twinstor/open-episode").exists();
    if episode_open && !yes {
        return Err(crate::render::CliError::new(
            "a heal episode is open (/var/lib/twinstor/open-episode); refusing to \
             replace the backing device under an active incident",
            "wait for `twinstor status` to settle, then re-run (or re-run with --yes \
             if you are certain the disk swap must happen now)",
        )
        .explain(
            "the daemon is resolving DRBD state right now; a topology change \
             would fight it.",
        )
        .into());
    }

    // Read + validate the config and .res up front so a bad state fails before
    // we print the plan or touch anything.
    let old_toml = tokio::fs::read_to_string(&config_path)
        .await
        .with_context(|| format!("reading {}", config_path.display()))?;
    let new_toml = replace_backing_device_line(&old_toml, &new_backing)?;
    let old_res = tokio::fs::read_to_string(&res_path)
        .await
        .with_context(|| format!("reading {res_path}"))?;
    // Build the new .res now (also validates we can parse the existing one).
    let new_res = rebuild_res_text(&config, &old_res, Some(&new_backing)).await?;

    // 3. Summarize and confirm.
    println!("TwinStor replace-backing\n");
    println!("this will, on this node ({}):", config.node.name);
    println!("  1. set backing_device in {} to:", config_path.display());
    println!("       {old_backing}");
    println!("     -> {new_backing}");
    println!("  2. regenerate {res_path} with the new local disk");
    println!("  3. Initialize {new_backing} with fresh DRBD metadata");
    println!("     (drbdadm create-md --force): any existing content on that device");
    println!("     is discarded (this is meant for a blank replacement disk)");
    println!("  4. drbdadm adjust {res} (+ attach): DRBD attaches the new disk and");
    println!("     the full resync from the peer begins\n");
    if episode_open {
        println!("WARNING: a heal episode is open; --yes given, proceeding anyway.\n");
    }
    if !yes {
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

    // 4. Update twinstor.toml atomically (temp file in the same dir + rename),
    // preserving mode. A partial write here would leave an unparseable config.
    write_atomic(&config_path, new_toml.as_bytes())
        .await
        .with_context(|| format!("updating {}", config_path.display()))?;
    println!("updated {}", config_path.display());

    // 5. Write the regenerated .res (keep a backup for manual rollback).
    let bak = format!("{res_path}.pre-replace");
    tokio::fs::copy(&res_path, &bak)
        .await
        .context("backing up res file")?;
    tokio::fs::write(&res_path, &new_res)
        .await
        .context("writing regenerated res file")?;
    println!("regenerated {res_path} (backup at {bak})");

    // 6. Initialize DRBD metadata on the replacement. A new or recreated disk
    // has none, and adjust and attach fail without it, leaving the resync
    // stuck. `-- --force` skips the interactive confirmation (the operator
    // already confirmed above); it destroys any existing content on the
    // device, which is why the plan spells that out.
    let mut steps = crate::render::Steps::new(3);
    steps
        .run(
            &format!("initializing DRBD metadata on {new_backing} (drbdadm create-md)"),
            crate::cmd::run_cmd_for("drbdadm", &["--", "--force", "create-md", res], 60),
        )
        .await
        .with_context(|| {
            format!(
                "drbdadm create-md failed on {new_backing}; config and .res are updated \
             (res backup at {bak}). Fix the device, then run \
             `drbdadm -- --force create-md {res} && drbdadm adjust {res}`"
            )
        })?;

    // 7. Apply live. `adjust` reconciles the running resource with the new
    // config (new disk path); if the resource is still detached/Diskless
    // afterwards, `attach` explicitly attaches the replacement.
    steps
        .run(
            &format!("attaching the new disk (drbdadm adjust {res})"),
            crate::cmd::run_cmd_for("drbdadm", &["adjust", res], 60),
        )
        .await
        .with_context(|| {
            format!(
                "drbdadm adjust {res} failed; config and .res are updated (res backup at {bak}). \
             Fix the device and re-run `drbdadm adjust {res}`"
            )
        })?;

    let drbd = DrbdController::new(&config);
    let attached = match drbd.status().await {
        Ok(s) => s.disk != crate::drbd::DiskState::Diskless,
        // Status unreadable: attempt the attach anyway (idempotent if already up).
        Err(_) => false,
    };
    if !attached {
        // Best-effort: on an already attached resource this is a harmless
        // no-op or known error; adjust above is the step that matters.
        if let Err(e) = crate::cmd::run_cmd_for("drbdadm", &["attach", res], 60).await {
            println!("note: drbdadm attach reported: {e}");
        }
    }

    // 8. Restart the running daemon to load the new config so its own
    // backing-device watchdog clears (it holds the config from startup; the
    // DRBD resync is a kernel operation and continues across the restart).
    let _ = steps
        .run(
            "reloading the daemon (systemctl try-restart twinstor)",
            async {
                tokio::process::Command::new("systemctl")
                    .args(["try-restart", "twinstor"])
                    .status()
                    .await
                    .map(|_| ())
            },
        )
        .await;

    println!();
    match drbd.status().await {
        Ok(s) => {
            println!(
                "done. DRBD disk={:?} connection={:?} peer-disk={:?}",
                s.disk,
                s.connection,
                s.peer_disk.unwrap_or(crate::drbd::DiskState::Unknown)
            );
            if s.disk == crate::drbd::DiskState::Diskless {
                println!(
                    "WARNING: the resource is still Diskless: the attach did not take \
                     (see the notes above). The config and .res are already updated; fix \
                     the device, then run `drbdadm attach {res}` to start the resync."
                );
            } else {
                println!(
                    "the new disk is initialized and DRBD is now resyncing from the \
                     peer; watch progress with `twinstor status`."
                );
            }
        }
        Err(_) => println!(
            "done, but DRBD status was unreadable; verify with `twinstor status` that \
             the disk is attached and resyncing before trusting redundancy."
        ),
    }
    Ok(())
}

/// Write `contents` to `path` atomically: a temp file in the same directory
/// (so the rename is atomic, not cross-device), synced, then renamed over the
/// target, and the directory synced, so a crash leaves the old or the new
/// file, never an empty one. The temp file is created with the target's
/// permissions (0600 for a new file): the config carries the CHAP secret and
/// must never be readable by others, even briefly.
async fn write_atomic(path: &std::path::Path, contents: &[u8]) -> Result<()> {
    use anyhow::Context;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::AsyncWriteExt;
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    ));
    let mode = tokio::fs::metadata(path)
        .await
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0o600);
    let _ = tokio::fs::remove_file(&tmp).await;
    let mut f = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .await
        .with_context(|| format!("creating {}", tmp.display()))?;
    // The umask may have narrowed the creation mode; set it exactly.
    f.set_permissions(std::fs::Permissions::from_mode(mode))
        .await
        .with_context(|| format!("setting permissions on {}", tmp.display()))?;
    f.write_all(contents)
        .await
        .with_context(|| format!("writing {}", tmp.display()))?;
    f.sync_all()
        .await
        .with_context(|| format!("syncing {}", tmp.display()))?;
    drop(f);
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    if let Ok(d) = tokio::fs::File::open(dir).await {
        let _ = d.sync_all().await;
    }
    Ok(())
}

/// Classify a `drbdadm adjust` failure: `true` if it is the benign DRBD (-7)
/// refusal to reset the peer connection on a connected, serving dual-Primary
/// ("Refusing to be Primary while peer is not outdated"), as opposed to a real
/// adjust failure (parse error, partial apply, and so on). Matches only the two
/// signatures specific to that refusal, not a bare "del-peer" token, so any
/// unrelated failure still takes the hard-error and rollback path with
/// verification.
fn adjust_refused_live_reconfigure(err: &str) -> bool {
    // "not outdated" alone is not enough: it must come with "Refusing to be
    // Primary" or the (-7) state-change code, so an unrelated failure that
    // contains it still takes the reconciling-adjust and hard-error path.
    (err.contains("Refusing to be Primary") && err.contains("not outdated"))
        || (err.contains("(-7)") && err.contains("not outdated"))
}

/// Re-assert the two split-brain data-safety policies on the running resource via
/// `drbdsetup` (no del-peer, so DRBD cannot refuse them). Idempotent and only ever
/// increases safety. Mirrors the daemon's `enforce_live_safety_policies`; used by
/// `regenerate-config` after any rollback so that a reconciling `drbdadm adjust`
/// (which reverts kernel options to the on-disk template) cannot leave the pair on
/// the old, unsafe handlers. Best-effort, but a failure is reported.
///
/// The peer node-id is read from the live DRBD config (`crate::drbd::peer_node_id`),
/// not guessed: DRBD may assign node-ids out of hostname order (a .res with no
/// explicit node-id), and a guess from hostname order would then name our own id.
async fn reassert_live_safety(res: &str, local: &str, peer: &str) {
    // Read the peer's actual DRBD node-id (see enforce_live_safety_policies): DRBD
    // may not assign node-ids in hostname order, and guessing targets our own id.
    let peer_id = crate::drbd::peer_node_id(res)
        .await
        .map(|n| n.to_string())
        .unwrap_or_else(|| if local < peer { "1".into() } else { "0".into() });
    let r1 = crate::cmd::run_cmd_for(
        "drbdsetup",
        &[
            "resource-options",
            res,
            "--on-no-data-accessible=suspend-io",
        ],
        10,
    )
    .await;
    let r2 = crate::cmd::run_cmd_for(
        "drbdsetup",
        &[
            "net-options",
            res,
            &peer_id,
            "--after-sb-0pri=disconnect",
            "--after-sb-1pri=disconnect",
            "--after-sb-2pri=disconnect",
        ],
        10,
    )
    .await;
    // Best-effort, but a failure is not hidden: the daemon enforces these only
    // once at start, so a silent failure here could leave the pair on the old,
    // unsafe handlers until a daemon restart, with no signal to the operator.
    if r1.is_err() || r2.is_err() {
        println!(
            "WARNING: could not re-assert the split-brain safety policies live via drbdsetup \
             (on-no-data-accessible ok={}, after-sb ok={}). Verify with \
             `drbdsetup show --show-defaults {res}` that on-no-data-accessible=suspend-io and \
             after-sb-0pri=disconnect are set; if not, restart the twinstor daemon on this node \
             to re-enforce them.",
            r1.is_ok(),
            r2.is_ok()
        );
    }
}

/// (host, node-id) pairs from `on <host> { node-id N; }`, sorted, for comparison.
fn node_ids_by_host(res: &str) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    let mut cur_host: Option<String> = None;
    for line in res.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("on ") {
            cur_host = rest
                .split_whitespace()
                .next()
                .map(|s| s.trim_matches('"').to_string());
            continue;
        }
        let Some(rest) = t.strip_prefix("node-id ") else {
            continue;
        };
        if let (Some(h), Ok(id)) = (
            cur_host.clone(),
            rest.trim_end_matches(';').trim().parse::<u32>(),
        ) {
            out.push((h, id));
        }
    }
    out.sort();
    out
}

/// Detect a `.res` change the peer must match to connect: a DRBD connection
/// topology or transport-mode change. Such a change cannot be applied one node at
/// a time, because a reconfigured node cannot reconnect to a peer still on the old
/// topology (switching `load-balance-paths` to active-backup leaves the rebooted
/// node in `Connecting`). Returns a human-readable reason when the change is
/// peer-incompatible, else `None`. Address and port changes are caught separately
/// by the address-set invariant.
fn peer_incompatible_change(old_res: &str, new_res: &str) -> Option<String> {
    // Path transport mode: load-balance-paths (lb-tcp, all paths active) vs the
    // default active-backup (one active). Both peers must agree or the link never
    // negotiates.
    let old_lb = old_res.contains("load-balance-paths yes");
    let new_lb = new_res.contains("load-balance-paths yes");
    if old_lb != new_lb {
        return Some(format!(
            "the DRBD path transport mode changes ({} -> {})",
            if old_lb {
                "load-balance-paths"
            } else {
                "active-backup"
            },
            if new_lb {
                "load-balance-paths"
            } else {
                "active-backup"
            },
        ));
    }
    // Per-host DRBD node-ids: a reassignment means the peers no longer agree on
    // identity and cannot connect.
    let old_ids = node_ids_by_host(old_res);
    let new_ids = node_ids_by_host(new_res);
    if !old_ids.is_empty() && !new_ids.is_empty() && old_ids != new_ids {
        return Some(format!(
            "the DRBD node-ids change ({old_ids:?} -> {new_ids:?})"
        ));
    }
    None
}

/// `twinstor regenerate-config`: bring an upgraded pool's setup-time DRBD
/// resource file up to the current template, live. Never changes topology
/// (addresses, backings, device): those are carried over from the existing
/// file; only the surrounding template (tuning, comments, version marker)
/// regenerates. Preflight-guarded, verify-or-rollback.
async fn run_regenerate_config(config: Arc<Config>, yes: bool, persist_only: bool) -> Result<()> {
    use anyhow::{Context, bail};
    let res = format!("twinstor-{}", config.storage.drbd_resource);
    let res = res.as_str();
    let path = format!("/etc/drbd.d/{res}.res");
    let old = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("reading {path}"))?;

    let deployed = crate::setup::res_config_version(&old);
    if deployed == crate::setup::CONFIG_VERSION {
        println!("{path} is already at config version {deployed}; nothing to do");
        return Ok(());
    }

    // Preflight: only a fully settled pair may regenerate.
    let st = crate::cmd::cmd_output_for("drbdadm", &["status", res], 10)
        .await
        .context("drbdadm status")?;
    let flat = st.replace('\n', " ");
    if !(flat.contains("disk:UpToDate")
        && flat.contains("peer-disk:UpToDate")
        && (flat.contains("role:Primary") || flat.contains("role:Secondary")))
        || flat.contains("connection:")
    {
        return Err(crate::render::CliError::new(
            "replication is not settled (need Connected, both UpToDate)",
            "wait until `twinstor status` shows Connected with both disks UpToDate, \
             then re-run",
        )
        .explain(
            "regenerate-config applies the new file live (drbdadm adjust), so it \
             only runs on a fully healthy pair.",
        )
        .into());
    }
    if std::path::Path::new("/var/lib/twinstor/open-episode").exists() {
        return Err(crate::render::CliError::new(
            "a heal episode is open; refusing to regenerate under an incident",
            "wait for the auto-heal episode to close (watch `twinstor status`), then re-run",
        )
        .into());
    }

    let local = &config.node.name;
    let peer = &config.node.peer;
    let parsed = crate::setup::parse_res_file(&old, local, peer);

    // Regenerate carries topology (device, backings, addresses) over from the
    // existing file unchanged: pass no backing override.
    let new = rebuild_res_text(&config, &old, None).await?;

    // Invariant: the address set must be unchanged.
    let reparsed = crate::setup::parse_res_file(&new, local, peer);
    if reparsed.addresses != parsed.addresses {
        bail!(
            "regeneration would change the address set ({:?} -> {:?}); refusing",
            parsed.addresses,
            reparsed.addresses
        );
    }

    println!(
        "--- current ({} lines, version {deployed})",
        old.lines().count()
    );
    println!(
        "+++ regenerated ({} lines, version {})",
        new.lines().count(),
        crate::setup::CONFIG_VERSION
    );
    for (i, (a, b)) in old.lines().zip(new.lines()).enumerate() {
        if a != b {
            println!("  line {:>3}: -{a}\n            +{b}", i + 1);
        }
    }

    // A peer-incompatible (connection-topology) change cannot be applied one
    // node at a time: the reconfigured node cannot reconnect to a peer still on
    // the old topology. Detected once, used by the dry-run note, the live-path
    // refusal and the persist-only guidance below.
    let topo = peer_incompatible_change(&old, &new);

    if !yes {
        if let Some(reason) = &topo {
            println!(
                "\nNote: this is a DRBD connection-topology change ({reason}). It must be \
                 applied to both nodes together in a maintenance window, not one node at a \
                 time (a node reconfigured alone hangs in Connecting against the old-topology \
                 peer)."
            );
        }
        println!("\ndry run (no --yes): nothing written");
        return Ok(());
    }

    // A connection-topology change on the live path: refuse before touching the
    // file. `drbdadm adjust` here would either be refused (del-peer) or strand
    // this node once it reloads the new topology while the peer is still on the
    // old one. There is no safe one-node live apply, so point to the joint
    // maintenance procedure instead.
    if let (false, Some(reason)) = (persist_only, &topo) {
        println!("\n{path} was left unchanged: not applied.");
        println!(
            "this update changes the DRBD connection topology ({reason}), which the peer \
             must match to connect. It cannot be applied one node at a time: a node \
             reconfigured alone cannot reconnect to a peer still on the old topology (it \
             hangs in `Connecting`)."
        );
        println!(
            "apply it in a maintenance window on both nodes together: on each node run \
             `twinstor regenerate-config --persist-only --yes`, then restart both nodes \
             together (reboot both, or `drbdadm down twinstor-{res_short}` then \
             `drbdadm up twinstor-{res_short}` on both close together) so they come up \
             matched. The SR is briefly unavailable during the joint restart, so stop or \
             pause the VMs first. You are safe meanwhile on the current config; the safety \
             policies are enforced live regardless of the .res version.",
            res_short = config.storage.drbd_resource,
        );
        return Ok(());
    }

    let bak = format!("{path}.pre-regen");
    tokio::fs::copy(&path, &bak)
        .await
        .context("backing up res file")?;
    tokio::fs::write(&path, &new)
        .await
        .context("writing regenerated res file")?;

    if persist_only {
        // Staging for a maintenance-window reboot: write the file, leave the
        // running resource alone. `drbdadm up` at the next boot loads it cleanly
        // (no del-peer). This leaves the on-disk file ahead of the running kernel,
        // which is a hazard if left in place: the daemon's own path-restore
        // (`drbdadm adjust`) would hit the same del-peer refusal each tick. So the
        // operator must reboot this node promptly, and must not stage the peer
        // until this node is back healthy on the new file.
        println!("\nstaged {path} (backup at {bak}); not applied live (--persist-only).");
        if let Some(reason) = &topo {
            // A connection-topology change needs the opposite of the usual
            // one-at-a-time sequence: both nodes must switch together, because a
            // node that reboots alone onto the new topology cannot reconnect to
            // the old-topology peer.
            println!(
                "topology change ({reason}): the peer must match this to connect, so this is \
                 not a one-node-at-a-time change. Stage the peer too \
                 (`twinstor regenerate-config --persist-only --yes` there), then restart both \
                 nodes together (reboot both, or `drbdadm down` then `up` on both close \
                 together). Do not reboot this node alone: it will hang in `Connecting` \
                 against the old-topology peer. The SR is briefly unavailable \
                 during the joint restart, so stop/pause the VMs first. Undo: cp {bak} {path}"
            );
        } else {
            println!(
                "WARNING: the on-disk file is now ahead of the running kernel. Do not leave it \
                 this way: reboot this node now to load it (`drbdadm up` on boot applies it \
                 cleanly). Until you reboot, avoid any direct-link/failback event: the daemon's \
                 path-restore would fail against the staged file."
            );
            println!(
                "sequence for the pair: stage + reboot one node, wait until `twinstor status` shows \
                 Connected with both UpToDate again, then stage + reboot the other. Staging both nodes \
                 before rebooting either leaves both files ahead of both kernels; a direct-link event \
                 then fails path-restore on both sides and sets up a split-brain, so do not do it. \
                 If you change your mind, restore the old file: cp {bak} {path}"
            );
        }
        return Ok(());
    }

    let mut steps = crate::render::Steps::new(2);
    match steps
        .run(
            &format!("applying live (drbdadm adjust {res})"),
            crate::cmd::run_cmd_for("drbdadm", &["adjust", res], 30),
        )
        .await
    {
        Ok(()) => {}
        Err(e) => {
            let es = e.to_string();
            // A connected, serving dual-Primary cannot be reconfigured live: to apply
            // the new template `drbdadm adjust` must reset the peer connection
            // (del-peer/add-peer), and DRBD refuses del-peer while this node is Primary
            // and the peer is not Outdated ("State change failed: (-7) Refusing to be
            // Primary while peer is not outdated"). adjust parsed the file fine; it
            // refused only the live connection reset.
            let cant_reconfigure_live = adjust_refused_live_reconfigure(&es);

            // Roll back to the stale but consistent file in every case. The new file
            // must never stay on disk while the kernel runs the old topology: the
            // daemon runs `drbdadm adjust` on this resource during failback and every
            // steered reconnect (restore_paths), which would hit the same del-peer
            // refusal each tick, leave the connection single-path and, through the
            // daemon's own safeguard, set up a split-brain cycle on the next
            // direct-link loss. A stale .res is harmless (the daemon enforces the
            // safety policies live whatever the .res version); a diverged one is a
            // hazard. A rollback failure is not swallowed: the diverged file would
            // survive and the "safe" messages below would be wrong.
            let rolled_back = tokio::fs::copy(&bak, &path).await;

            if cant_reconfigure_live {
                // del-peer was refused, so nothing was applied live and the connection is
                // unchanged. Do not run `drbdadm adjust` now: it would reconcile the kernel
                // down to the old template and could strip the daemon's live safety options
                // (the daemon applies them once per start, so it would not re-assert them).
                // Re-assert those policies instead, which also covers a stopped daemon.
                reassert_live_safety(res, local, peer).await;
                if let Err(ce) = rolled_back {
                    return Err(crate::render::CliError::new(
                        format!("the live reconfigure was refused ({e}); then restoring {bak} failed ({ce})"),
                        format!(
                            "CRITICAL: {path} may still hold the new template while the kernel runs \
                             the old topology; restore it now: `cp {bak} {path}`. Until you do, the \
                             daemon's path-restore will fail on this resource on the next \
                             failback/reconnect."
                        ),
                    )
                    .into());
                }
                println!("\n{path} was left unchanged (restored from {bak}).");
                println!(
                    "the new template could not be applied live: the pair is a connected, \
                     serving dual-Primary, so DRBD refuses to reset the peer connection while \
                     both nodes are Primary/UpToDate. Leaving the new file on disk while the \
                     kernel runs the old topology would break the daemon's own path-restore, \
                     so the old file was kept."
                );
                println!(
                    "you are safe meanwhile: the split-brain data-safety policies (after-sb \
                     disconnect, on-no-data-accessible suspend-io) are enforced live by the \
                     daemon regardless of the .res version, so the `config outdated` line in \
                     `twinstor status` is informational here, not a risk."
                );
                println!(
                    "to persist the new template you must stage it first, then reboot: this \
                     live attempt just restored the old file, so a bare reboot reloads the old \
                     one (nothing changed on disk). In a maintenance window, one node at a time: \
                     run `twinstor regenerate-config --persist-only --yes` (writes the new .res \
                     without touching the running kernel), reboot that node so `drbdadm up` loads \
                     it cleanly, wait until `twinstor status` shows Connected with both UpToDate, \
                     then repeat on the other node. (Alternatively, re-run this live path while \
                     the peer is briefly Secondary+Outdated.)"
                );
                return Ok(());
            }

            // A different adjust failure: a partial apply may have landed. If the rollback
            // itself failed, do not run the reconciling adjust (it might re-apply the
            // still-new file); return a hard error naming the manual fix. Otherwise
            // reconcile the kernel to the restored file, then re-assert the safety policies
            // (the reconciling adjust may have reverted them to the old template's values).
            if let Err(ce) = rolled_back {
                reassert_live_safety(res, local, peer).await;
                return Err(crate::render::CliError::new(
                    format!("drbdadm adjust failed ({e}); and restoring {bak} failed ({ce})"),
                    format!(
                        "CRITICAL: restore {path} manually now: `cp {bak} {path}`; the on-disk \
                         file may be the new template while the kernel runs the old one."
                    ),
                )
                .into());
            }
            let _ = crate::cmd::run_cmd_for("drbdadm", &["adjust", res], 30).await;
            reassert_live_safety(res, local, peer).await;
            return Err(crate::render::CliError::new(
                format!("drbdadm adjust failed ({e}); rolled back to {bak}"),
                "inspect the adjust error above, fix the cause, and re-run \
                 (the previous file is back in place; replication was not changed)",
            )
            .into());
        }
    }
    let verified = steps
        .run("verifying replication stayed healthy", async {
            let st2 = crate::cmd::cmd_output_for("drbdadm", &["status", res], 10)
                .await
                .unwrap_or_default();
            let flat2 = st2.replace('\n', " ");
            if flat2.contains("disk:UpToDate") && flat2.contains("peer-disk:UpToDate") {
                Ok(())
            } else {
                Err(())
            }
        })
        .await;
    if verified.is_err() {
        // Roll back the applied-but-unhealthy change. If the restore fails, do not
        // re-adjust (it would re-apply the new file); name the manual fix.
        if let Err(ce) = tokio::fs::copy(&bak, &path).await {
            reassert_live_safety(res, local, peer).await;
            return Err(crate::render::CliError::new(
                format!("post-adjust verification failed; and restoring {bak} failed ({ce})"),
                format!(
                    "CRITICAL: restore {path} manually: `cp {bak} {path}`, then check \
                     `twinstor status` on both nodes."
                ),
            )
            .into());
        }
        let _ = crate::cmd::run_cmd_for("drbdadm", &["adjust", res], 30).await;
        reassert_live_safety(res, local, peer).await;
        return Err(crate::render::CliError::new(
            format!("post-adjust verification failed; rolled back to {bak}"),
            "check `twinstor status` on both nodes before retrying; if the pair \
             is healthy again, re-run this command",
        )
        .explain(
            "the resource did not come back UpToDate/UpToDate after the live \
             adjust, so the previous file was restored and re-applied.",
        )
        .into());
    }
    println!("regenerated {path} (backup at {bak}); replication verified healthy");
    println!("run the same command on {peer} to complete the pair");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn discard_and_resync_treats_an_unclear_daemon_state_as_running() {
        use super::daemon_may_be_running;
        assert!(!daemon_may_be_running(&Ok("inactive\n".into())));
        assert!(!daemon_may_be_running(&Ok("failed".into())));
        for s in [
            "active",
            "activating",
            "deactivating",
            "reloading",
            "unknown",
            "",
        ] {
            assert!(daemon_may_be_running(&Ok(s.into())), "{s}");
        }
        assert!(daemon_may_be_running(&Err(anyhow::anyhow!("timed out"))));
    }

    #[tokio::test]
    async fn write_atomic_replaces_the_file_and_keeps_it_private() {
        use super::write_atomic;
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tw-write-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("twinstor.toml");
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

        // A new file is created 0600, never with the umask's default.
        write_atomic(&path, b"one").await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"one");
        assert_eq!(mode(&path), 0o600);

        // An existing file keeps its mode and gets the new contents.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_atomic(&path, b"two").await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(mode(&path), 0o640);

        // A temp file left by an interrupted run does not block the next write.
        std::fs::write(dir.join(".twinstor.toml.tmp"), b"stale").unwrap();
        write_atomic(&path, b"three").await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"three");
        assert!(!dir.join(".twinstor.toml.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discard_and_resync_refuses_every_unsafe_state() {
        use super::discard_refusal;
        use crate::drbd::ConnectionState::*;
        use crate::loser_boundary::LocalScan;
        let clear: std::result::Result<LocalScan, String> = Ok(LocalScan::default());
        let busy: std::result::Result<LocalScan, String> = Ok(LocalScan {
            all: vec![(4, "u".into())],
            on_sr: vec![(4, "u".into())],
        });
        let unreadable: std::result::Result<LocalScan, String> = Err("xl failed".into());
        // The safe case: daemon stopped, StandAlone, no guest on the SR.
        assert_eq!(
            discard_refusal(false, Some(StandAlone), false, None, false, false, &clear),
            None
        );
        assert_eq!(
            discard_refusal(false, Some(Connecting), true, None, false, false, &clear),
            None,
            "marker present"
        );
        // Each refusal names what is wrong.
        assert!(
            discard_refusal(true, Some(StandAlone), true, None, false, false, &clear)
                .unwrap()
                .0
                .contains("daemon is running")
        );
        assert!(
            discard_refusal(false, Some(Connected), false, None, false, false, &clear)
                .unwrap()
                .0
                .contains("no split-brain")
        );
        assert!(
            discard_refusal(false, None, true, None, false, false, &clear)
                .unwrap()
                .0
                .contains("could not be read")
        );
        assert!(
            discard_refusal(false, Some(StandAlone), true, None, false, false, &busy)
                .unwrap()
                .0
                .contains("domid 4")
        );
        assert!(
            discard_refusal(
                false,
                Some(StandAlone),
                true,
                None,
                false,
                false,
                &unreadable
            )
            .unwrap()
            .0
            .contains("cannot tell")
        );
        // The lineage says this node keeps: refused unless explicitly overridden.
        let kept = discard_refusal(
            false,
            Some(StandAlone),
            true,
            Some(true),
            false,
            false,
            &clear,
        )
        .unwrap();
        assert!(
            kept.0.contains("copy to keep") && kept.1.contains("--discard-kept-copy"),
            "{kept:?}"
        );
        assert_eq!(
            discard_refusal(
                false,
                Some(StandAlone),
                true,
                Some(true),
                false,
                true,
                &clear
            ),
            None
        );
        assert_eq!(
            discard_refusal(
                false,
                Some(StandAlone),
                true,
                Some(false),
                false,
                false,
                &clear
            ),
            None
        );
        // A tie: the lineage cannot pick; refused on either node until the
        // operator decides, and never told to try the other node.
        let tie = discard_refusal(
            false,
            Some(StandAlone),
            true,
            Some(true),
            true,
            false,
            &clear,
        )
        .unwrap();
        assert!(
            tie.0.contains("cannot tell which copy")
                && tie.1.contains("--discard-kept-copy")
                && !tie.1.contains("other node"),
            "{tie:?}"
        );
        assert_eq!(
            discard_refusal(
                false,
                Some(StandAlone),
                true,
                Some(true),
                true,
                true,
                &clear
            ),
            None
        );
    }

    use super::{
        PEER_ALIVE_STALE_SECS, UpgradeBlocker, adjust_refused_live_reconfigure, iscsi_json,
        iscsi_sev, iscsi_word, kernels_missing_drbd, node_ids_by_host, parse_res_node_ids,
        peer_daemon_alive_from_stamp, peer_incompatible_change, probe_output,
        replace_backing_device_line, upgrade_apply_blockers,
    };

    // An unanswered iSCSI probe (targetcli wedged on dead backing) renders as
    // unknown in every output mode, never as "not exported", which would push
    // the operator to re-export a target that is still there.
    #[test]
    fn iscsi_probe_timeout_renders_unknown_not_inactive() {
        use crate::render::Sev;
        assert_eq!(iscsi_word(None), "unknown");
        assert_eq!(iscsi_json(None), "null");
        assert_eq!(iscsi_sev(None), Sev::Warn);
        // The two real answers are unchanged.
        assert_eq!(iscsi_word(Some(true)), "active");
        assert_eq!(iscsi_json(Some(true)), "true");
        assert_eq!(iscsi_sev(Some(true)), Sev::Good);
        assert_eq!(iscsi_word(Some(false)), "inactive");
        assert_eq!(iscsi_json(Some(false)), "false");
        assert_eq!(iscsi_sev(Some(false)), Sev::Bad);
    }

    // A probe whose command wedges (as targetcli, multipath and drbdadm do when
    // the backing disks are pulled) returns None within its bound instead of
    // hanging `twinstor status`.
    #[tokio::test]
    async fn probe_output_times_out_on_a_wedged_command() {
        let start = std::time::Instant::now();
        // `sleep 30` stands in for a probe blocked on dead I/O; the 1s bound must win.
        let got = probe_output("sleep", &["30"], 1).await;
        let elapsed = start.elapsed();
        assert!(
            got.is_none(),
            "a wedged probe must report unknown (None), not block"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "probe must return at ~the bound, not run to completion (took {elapsed:?})"
        );
    }

    #[tokio::test]
    async fn probe_output_returns_output_for_a_fast_command() {
        let got = probe_output("true", &[], 5)
            .await
            .expect("fast command should return Some");
        assert!(got.status.success());
    }

    #[tokio::test]
    async fn probe_output_none_when_command_missing() {
        // Spawn failure (no such binary) is unknown, not a panic.
        assert!(
            probe_output("twinstor-no-such-binary-xyz", &[], 5)
                .await
                .is_none()
        );
    }

    #[test]
    fn peer_incompatible_change_detects_lb_and_nodeids() {
        // multi-line `on` blocks, as render_res_file / real .res files emit them.
        let ids = "    on a {\n        node-id 0;\n    }\n    on b {\n        node-id 1;\n    }\n";
        let lb = format!(
            "resource r {{\n{ids}    connection {{\n        net {{ load-balance-paths yes; }}\n        path {{ }}\n    }}\n}}\n"
        );
        let ab = format!(
            "resource r {{\n{ids}    connection {{\n        path {{ }}\n        path {{ }}\n    }}\n}}\n"
        );
        // Switching load-balance-paths to active-backup is peer-incompatible.
        assert!(
            peer_incompatible_change(&lb, &ab)
                .unwrap()
                .contains("path transport mode")
        );
        // And the reverse.
        assert!(peer_incompatible_change(&ab, &lb).is_some());
        // Same path mode and node-ids (a disk-options-only change) is not flagged.
        assert!(peer_incompatible_change(&ab, &ab).is_none());
        // A node-id swap is flagged.
        let swapped =
            "    on a {\n        node-id 1;\n    }\n    on b {\n        node-id 0;\n    }\n";
        let ab_sw = format!(
            "resource r {{\n{swapped}    connection {{\n        path {{ }}\n        path {{ }}\n    }}\n}}\n"
        );
        assert!(
            peer_incompatible_change(&ab, &ab_sw)
                .unwrap()
                .contains("node-ids")
        );
    }

    #[test]
    fn node_ids_by_host_parses_pairs() {
        let res = "    on host-a {\n        node-id 1;\n    }\n    on host-b {\n        node-id 0;\n    }\n";
        assert_eq!(
            node_ids_by_host(res),
            vec![("host-a".to_string(), 1), ("host-b".to_string(), 0)]
        );
        // No node-ids (very old .res) gives an empty list, so
        // peer_incompatible_change does not flag a config that never had them.
        assert!(node_ids_by_host("    on a {\n        device /dev/drbd0;\n    }").is_empty());
    }

    #[test]
    fn parse_res_node_ids_preserves_or_absent() {
        // Inverted ids (local host-a is node-id 1, peer host-b is 0) happen in
        // practice; regenerate must re-emit these, never reassign by hostname.
        let with_ids = "resource twinstor-data {\n\
            \x20   on host-a {\n        node-id 1;\n        device /dev/drbd0; meta-disk internal;\n    }\n\
            \x20   on host-b {\n        node-id 0;\n        device /dev/drbd0; meta-disk internal;\n    }\n}\n";
        assert_eq!(
            parse_res_node_ids(with_ids, "host-a", "host-b"),
            Some((1, 0))
        );
        // A fate-shared pool with no explicit node-id gives None, so regenerate
        // keeps it id-less rather than injecting a mismatch.
        let no_ids = "resource twinstor-data {\n\
            \x20   on host-a {\n        device /dev/drbd0; address 10.0.0.1:7789; meta-disk internal;\n    }\n\
            \x20   on host-b {\n        device /dev/drbd0; address 10.0.0.2:7789; meta-disk internal;\n    }\n}\n";
        assert_eq!(parse_res_node_ids(no_ids, "host-a", "host-b"), None);
    }

    #[test]
    fn adjust_refusal_is_classified_narrowly() {
        // The benign -7 refusal on a connected, serving dual-Primary, full
        // signature (both phrases).
        assert!(adjust_refused_live_reconfigure(
            "drbdadm adjust twinstor-r0: twinstor-r0: State change failed: (-7) \
             Refusing to be Primary while peer is not outdated"
        ));
        // The -7 code plus "not outdated" still classifies as benign, in case
        // the "Refusing to be Primary" wording changes.
        assert!(adjust_refused_live_reconfigure(
            "twinstor-r0: State change failed: (-7): peer is not outdated"
        ));
        // A bare "not outdated" without the -7 or "Refusing" signature is too
        // broad to trust; it takes the reconciling-adjust and hard-error path.
        assert!(!adjust_refused_live_reconfigure(
            "Command 'drbdmeta ...': peer is not outdated"
        ));
        // A parse error or any other failure is not benign.
        assert!(!adjust_refused_live_reconfigure(
            "drbdadm adjust twinstor-r0: Parse error in /etc/drbd.d/twinstor-r0.res line 12"
        ));
        // Nor is a del-peer failure for another reason.
        assert!(!adjust_refused_live_reconfigure(
            "drbdadm adjust twinstor-r0: del-peer failed: connection has active paths"
        ));
    }

    #[test]
    fn peer_alive_stamp_freshness() {
        let now = 1_000_000u64;
        // A fresh stamp means alive.
        assert!(peer_daemon_alive_from_stamp(Some(now), now));
        assert!(peer_daemon_alive_from_stamp(
            Some(now - PEER_ALIVE_STALE_SECS),
            now
        ));
        // Just past the window: not alive (peer daemon down or restarting).
        assert!(!peer_daemon_alive_from_stamp(
            Some(now - PEER_ALIVE_STALE_SECS - 1),
            now
        ));
        // No stamp (peer never ran, or xe unreadable): not alive.
        assert!(!peer_daemon_alive_from_stamp(None, now));
        // Clock skew (a stamp in the future) must not underflow: alive.
        assert!(peer_daemon_alive_from_stamp(Some(now + 5), now));
    }

    #[test]
    fn upgrade_apply_gate_matrix() {
        use UpgradeBlocker::*;
        // All healthy (Connected, UpToDate, no episode, peer alive): no blockers.
        assert!(upgrade_apply_blockers(true, true, false, true).is_empty());
        // The serialization interlock: peer daemon not alive (restarting) blocks.
        assert_eq!(
            upgrade_apply_blockers(true, true, false, false),
            vec![PeerNotAlive]
        );
        // Storage gates.
        assert_eq!(
            upgrade_apply_blockers(false, true, false, true),
            vec![NotConnected]
        );
        assert_eq!(
            upgrade_apply_blockers(true, false, false, true),
            vec![NotUpToDate]
        );
        assert_eq!(
            upgrade_apply_blockers(true, true, true, true),
            vec![EpisodeOpen]
        );
        // Double-restart race: a restarting peer takes DRBD to Connecting and
        // lets its stamp go stale, so both terms fire.
        assert_eq!(
            upgrade_apply_blockers(false, true, false, false),
            vec![NotConnected, PeerNotAlive]
        );
        // Everything wrong: all four, in a fixed order.
        assert_eq!(
            upgrade_apply_blockers(false, false, true, false),
            vec![NotConnected, NotUpToDate, EpisodeOpen, PeerNotAlive]
        );
    }

    const SAMPLE_CONFIG: &str = r#"[node]
name = "node-a"
peer = "node-b"

[storage]
drbd_resource = "data"
backing_device = "/dev/disk/by-id/md-uuid-OLD"
drbd_device = "/dev/drbd0"

[iscsi]
target_iqn = "iqn.2025-01.com.xcp-ng:twinstor-data"
chap_password = "s3cr3t"
"#;

    #[test]
    fn replace_backing_swaps_only_the_backing_line() {
        let out =
            replace_backing_device_line(SAMPLE_CONFIG, "/dev/disk/by-id/md-uuid-NEW").unwrap();
        assert!(out.contains("backing_device = \"/dev/disk/by-id/md-uuid-NEW\""));
        assert!(!out.contains("md-uuid-OLD"));
        // Everything else is preserved verbatim: the CHAP secret, the other
        // keys, the section headers, and the trailing newline.
        assert!(out.contains("chap_password = \"s3cr3t\""));
        assert!(out.contains("drbd_device = \"/dev/drbd0\""));
        assert!(out.contains("drbd_resource = \"data\""));
        assert!(out.ends_with('\n'));
        // Exactly one line changed.
        let diff = SAMPLE_CONFIG
            .lines()
            .zip(out.lines())
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(diff, 1);
    }

    #[test]
    fn replace_backing_preserves_indentation() {
        let toml = "[storage]\n    backing_device = \"/dev/old\"\n";
        let out = replace_backing_device_line(toml, "/dev/new").unwrap();
        assert!(out.contains("    backing_device = \"/dev/new\""));
    }

    #[test]
    fn replace_backing_is_not_fooled_by_similar_keys() {
        // A `backing_device_note` key must not be mistaken for the real one.
        let toml = "backing_device_note = \"x\"\nbacking_device = \"/dev/old\"\n";
        let out = replace_backing_device_line(toml, "/dev/new").unwrap();
        assert!(out.contains("backing_device_note = \"x\""));
        assert!(out.contains("backing_device = \"/dev/new\""));
    }

    #[test]
    fn replace_backing_rejects_missing_key() {
        let toml = "[storage]\ndrbd_device = \"/dev/drbd0\"\n";
        assert!(replace_backing_device_line(toml, "/dev/new").is_err());
    }

    #[test]
    fn replace_backing_rejects_ambiguous_duplicate() {
        let toml = "backing_device = \"/dev/a\"\nbacking_device = \"/dev/b\"\n";
        assert!(replace_backing_device_line(toml, "/dev/new").is_err());
    }

    #[test]
    fn kernel_module_coverage_scan() {
        let root = std::env::temp_dir().join("twinstor-test-modules");
        let _ = std::fs::remove_dir_all(&root);
        // Kernel A has the kmod in extra/drbd, kernel B has a weak-updates
        // symlink dir, kernel C has nothing.
        std::fs::create_dir_all(root.join("4.19.0+a/extra/drbd")).unwrap();
        std::fs::write(root.join("4.19.0+a/extra/drbd/drbd.ko"), b"x").unwrap();
        std::fs::create_dir_all(root.join("4.19.0+b/weak-updates/drbd")).unwrap();
        std::fs::write(root.join("4.19.0+b/weak-updates/drbd/drbd.ko.xz"), b"x").unwrap();
        std::fs::create_dir_all(root.join("4.19.0+c/kernel")).unwrap();

        assert_eq!(kernels_missing_drbd(&root), vec!["4.19.0+c".to_string()]);

        // Missing root: nothing to report (fail open, this is advisory).
        assert!(kernels_missing_drbd(std::path::Path::new("/nonexistent-xyz")).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
