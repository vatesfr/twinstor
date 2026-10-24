use anyhow::{Context, Result, bail};
use std::time::Duration;
use tracing::{debug, info, warn};

/// Wall-clock bounds for the plain (non-`_for`) command runners.
///
/// A wedged xapi can accept the connection and never answer, so an unbounded
/// `xe` call blocks the tick loop for as long as the wedge lasts. The recovery
/// paths run `xe` precisely when xapi is unhealthy, so every runner here is
/// bounded and a hung call turns into a retry instead of a stall.
///
/// The defaults are far above any healthy `xe`, `drbdadm`, `iscsiadm` or
/// `targetcli` call. Use the `_for` variants for a tighter bound (status reads
/// on the hot path) or a longer one.
const DEFAULT_CMD_TIMEOUT: Duration = Duration::from_secs(120); // side-effecting
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(60); // read-only (cmd_output)

/// Unbounded run. Private so that every public entry point goes through
/// `bounded`.
async fn run_cmd_unbounded(cmd: &str, args: &[&str], quiet: bool) -> Result<()> {
    let full = format!("{cmd} {}", args.join(" "));
    debug!(command = %full, "running");
    let out = tokio::process::Command::new(cmd)
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| full.clone())?;
    let code = out.status.code();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if quiet {
            debug!(command = %full, exit = ?code, stderr = %stderr.trim(), "command failed (quiet)");
        } else {
            warn!(command = %full, exit = ?code, stderr = %stderr.trim(), "command failed");
        }
        bail!("{full}: exit {code:?}: {stderr}");
    }
    if changes_drbd_state(cmd, args) {
        // Every DRBD state change the daemon made is in the log, whatever
        // path issued it (the caller's own line says why).
        info!(command = %full, "drbd state change done");
    } else {
        debug!(command = %full, "ok");
    }
    Ok(())
}

/// A `drbdadm` / `drbdsetup` call that changes state (connect, discard,
/// promote, demote, suspend or resume I/O, a new generation, peer objects,
/// net options), as opposed to a read.
fn changes_drbd_state(cmd: &str, args: &[&str]) -> bool {
    const READS: [&str; 10] = [
        "status",
        "show",
        "dstate",
        "cstate",
        "role",
        "show-gi",
        "get-gi",
        "events2",
        "dump",
        "sh-resources",
    ];
    (cmd == "drbdadm" || cmd == "drbdsetup")
        && args
            .iter()
            .find(|a| !a.starts_with('-'))
            .is_some_and(|verb| !READS.contains(verb) && !verb.starts_with("sh-"))
}

/// Unbounded read. Private, see `run_cmd_unbounded`.
async fn cmd_output_unbounded(cmd: &str, args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new(cmd)
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("{cmd} {}", args.join(" ")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "{cmd} {}: exit {:?}: {stderr}",
            args.join(" "),
            out.status.code()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Bound a command future. On timeout the future is dropped, and
/// `kill_on_drop` on the inner `Command` kills the child so it does not keep
/// the wedged service's socket open.
async fn bounded<T>(
    full: &str,
    bound: Duration,
    fut: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout(bound, fut).await {
        Ok(r) => r,
        Err(_) => {
            warn!(command = %full, timeout_secs = bound.as_secs(), "command timed out; killed");
            bail!("{full}: timed out after {}s", bound.as_secs());
        }
    }
}

pub async fn run_cmd(cmd: &str, args: &[&str]) -> Result<()> {
    let full = format!("{cmd} {}", args.join(" "));
    bounded(
        &full,
        DEFAULT_CMD_TIMEOUT,
        run_cmd_unbounded(cmd, args, false),
    )
    .await
}

/// Like `run_cmd`, but logs failures at debug level. For best-effort commands
/// whose failure the caller expects and handles, such as iSCSI discovery
/// against a peer target that is still starting.
pub async fn run_cmd_quiet(cmd: &str, args: &[&str]) -> Result<()> {
    let full = format!("{cmd} {}", args.join(" "));
    bounded(
        &full,
        DEFAULT_CMD_TIMEOUT,
        run_cmd_unbounded(cmd, args, true),
    )
    .await
}

pub async fn cmd_output(cmd: &str, args: &[&str]) -> Result<String> {
    let full = format!("{cmd} {}", args.join(" "));
    bounded(&full, DEFAULT_READ_TIMEOUT, cmd_output_unbounded(cmd, args)).await
}

/// `run_cmd` with an explicit bound in seconds instead of the default.
pub async fn run_cmd_for(cmd: &str, args: &[&str], secs: u64) -> Result<()> {
    let full = format!("{cmd} {}", args.join(" "));
    bounded(
        &full,
        Duration::from_secs(secs),
        run_cmd_unbounded(cmd, args, false),
    )
    .await
}

/// `cmd_output` with an explicit wall-clock bound (see `run_cmd_for`).
pub async fn cmd_output_for(cmd: &str, args: &[&str], secs: u64) -> Result<String> {
    let full = format!("{cmd} {}", args.join(" "));
    bounded(
        &full,
        Duration::from_secs(secs),
        cmd_output_unbounded(cmd, args),
    )
    .await
}

/// Run to completion and return `(success, stdout, stderr)`. A non-zero exit
/// is not an error here, so callers can classify the failure text (drbdmeta's
/// "no valid meta data" versus "no such file", for example). Returns `Err`
/// only when the command cannot be spawned or times out.
pub async fn cmd_status_for(cmd: &str, args: &[&str], secs: u64) -> Result<(bool, String, String)> {
    let full = format!("{cmd} {}", args.join(" "));
    bounded(&full, Duration::from_secs(secs), async {
        let out = tokio::process::Command::new(cmd)
            .args(args)
            .kill_on_drop(true)
            .output()
            .await
            .with_context(|| full.clone())?;
        Ok((
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        ))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_drbd_state_changes_are_logged_at_info() {
        assert!(changes_drbd_state(
            "drbdadm",
            &["connect", "--discard-my-data", "twinstor-data"]
        ));
        assert!(changes_drbd_state("drbdadm", &["primary", "twinstor-data"]));
        assert!(changes_drbd_state("drbdsetup", &["resume-io", "0"]));
        assert!(changes_drbd_state(
            "drbdadm",
            &["new-current-uuid", "twinstor-data"]
        ));
        assert!(!changes_drbd_state("drbdadm", &["status", "twinstor-data"]));
        assert!(!changes_drbd_state(
            "drbdsetup",
            &["show", "--show-defaults", "twinstor-data"]
        ));
        assert!(!changes_drbd_state(
            "drbdadm",
            &["sh-ll-dev", "twinstor-data"]
        ));
        assert!(!changes_drbd_state("xe", &["vm-start"]));
    }

    // A command that never returns, like `xe` against a hung xapi, must not
    // hold the caller forever.
    #[tokio::test(start_paused = true)]
    async fn bounded_runner_cuts_off_a_hung_command() {
        let fut = run_cmd_for("sleep", &["3600"], 2);
        // The paused clock advances on its own while idle; kill_on_drop reaps
        // the real child.
        let res = fut.await;
        assert!(res.is_err(), "a hung command must time out, not block");
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("timed out"), "error should say timeout: {msg}");
    }

    #[tokio::test]
    async fn bounded_output_passes_through_fast_commands() {
        let out = cmd_output_for("echo", &["ok"], 5).await.unwrap();
        assert_eq!(out, "ok");
    }

    // The plain runners are bounded too, so a new call site cannot
    // reintroduce an unbounded wait.
    #[tokio::test(start_paused = true)]
    async fn plain_run_cmd_is_bounded_by_default() {
        let res = run_cmd("sleep", &["3600"]).await;
        assert!(res.is_err(), "plain run_cmd must not block forever");
        assert!(res.unwrap_err().to_string().contains("timed out"));
    }

    #[tokio::test(start_paused = true)]
    async fn plain_cmd_output_is_bounded_by_default() {
        let res = cmd_output("sleep", &["3600"]).await;
        assert!(res.is_err(), "plain cmd_output must not block forever");
        assert!(res.unwrap_err().to_string().contains("timed out"));
    }

    // An explicit `_for` bound longer than the default is honoured, not
    // clamped.
    #[tokio::test]
    async fn explicit_for_bypasses_default_ceiling() {
        // 180 s is above DEFAULT_CMD_TIMEOUT.
        run_cmd_for("true", &[], 180).await.unwrap();
    }
}
