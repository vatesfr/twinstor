//! Minimal systemd notification support, dependency-free: READY once at
//! daemon start and WATCHDOG pings from the tick loop, so a `WatchdogSec=`
//! unit restarts a hung daemon. `Restart=on-failure` only covers processes
//! that exit; a daemon wedged in kernel I/O or a livelock keeps its PID and
//! systemd would leave it there indefinitely, with the whole safety layer
//! absent. A missing or foreign `NOTIFY_SOCKET` (older deployed unit,
//! running outside systemd, tests) makes every call a cheap no-op.

use std::os::unix::net::UnixDatagram;

pub fn ready() {
    notify("READY=1");
}

pub fn watchdog() {
    notify("WATCHDOG=1");
}

fn notify(state: &str) {
    let Ok(sockpath) = std::env::var("NOTIFY_SOCKET") else {
        return;
    };
    let Ok(sock) = UnixDatagram::unbound() else {
        return;
    };
    if let Some(abstract_name) = sockpath.strip_prefix('@') {
        use std::os::linux::net::SocketAddrExt;
        if let Ok(addr) =
            std::os::unix::net::SocketAddr::from_abstract_name(abstract_name.as_bytes())
        {
            let _ = sock.send_to_addr(state.as_bytes(), &addr);
        }
    } else {
        let _ = sock.send_to(state.as_bytes(), &sockpath);
    }
}
