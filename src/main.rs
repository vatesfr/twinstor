mod advertise;
mod autostart;
mod cli;
mod cmd;
mod config;
mod diagnostics;
mod doctor;
mod drbd;
mod failback_state;
mod heal_state;
mod iscsi;
mod loser_boundary;
mod multipath;
mod peer_signal;
mod pool_join;
mod preflight;
mod readiness;
mod reboot_drain;
mod render;
mod sd_notify;
mod setup;
mod state_machine;
mod uninstall;
mod wizard;
mod write_cache;
mod xapi;
mod xapi_storage;

use clap::Parser;

fn init_logging(
    daemon: bool,
    offset: time::UtcOffset,
) -> anyhow::Result<Option<tracing_appender::non_blocking::WorkerGuard>> {
    // RUST_LOG, when set, is used as given (e.g. `twinstor=debug`); adding
    // the default on top of it would override any directive for the same
    // target.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("twinstor=info"));

    // Render timestamps in the host's local time so entries line up with the
    // XCP-ng system logs (which use local time). The offset is captured once
    // at startup, so a daemon spanning a DST change keeps the pre-transition
    // offset until it restarts; that skew is cosmetic and acceptable.
    let timer = tracing_subscriber::fmt::time::OffsetTime::new(
        offset,
        time::format_description::well_known::Rfc3339,
    );

    if daemon {
        // Single file; retention is logrotate's job (copytruncate, see
        // config/twinstor.logrotate). The built-in daily rolling is
        // unbounded: nothing prunes the dated files.
        let file_appender = tracing_appender::rolling::never("/var/log", "twinstor.log");
        // Not lossy: a burst (an incident is when lines pile up) waits
        // for the writer instead of silently dropping lines.
        let (non_blocking, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
            .lossy(false)
            .finish(file_appender);
        // Every line names its source file and line: a message or event name
        // reused by two code paths still points at the one that ran (with the
        // version logged at startup, see `startup_state`).
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(non_blocking)
            .with_ansi(false)
            .with_timer(timer)
            .with_file(true)
            .with_line_number(true)
            .init();
        Ok(Some(guard))
    } else {
        // CLI mode: diagnostics go to stderr so stdout stays machine-readable.
        // `status --json` / `--plain` are polled by monitors, and a probe that
        // times out on wedged backing logs a warning at that moment; on
        // stdout (the fmt default) it would make the JSON unparseable.
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .with_timer(timer)
            .init();
        Ok(None)
    }
}

fn main() -> std::process::ExitCode {
    // Read the local UTC offset while the process is still single-threaded.
    // `time` refuses to determine the local offset once other threads exist (a
    // soundness guard around the C library), so this must happen before the
    // tokio runtime spins up its worker threads.
    let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);

    // Promote --no-color to the NO_COLOR env var before clap parses: clap
    // renders its own usage/help errors (colored) before Cli::run could
    // record the flag, and it honors NO_COLOR natively. Safe: no other
    // threads exist yet.
    if std::env::args().any(|a| a == "--no-color") {
        unsafe { std::env::set_var("NO_COLOR", "1") };
    }

    // Bare `twinstor` gets a short command map instead of clap's usage error.
    if std::env::args_os().len() < 2 {
        cli::print_overview();
        return std::process::ExitCode::from(2);
    }

    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(anyhow::Error::from)
        .and_then(|runtime| runtime.block_on(run(offset)));
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            render::report_error(&e);
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(offset: time::UtcOffset) -> anyhow::Result<()> {
    let cli = cli::Cli::parse();
    let _guard = init_logging(cli.is_daemon(), offset)?;
    cli.run().await
}
