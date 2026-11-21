//! Ties helium's processes to the compositor they were started under.
//!
//! When the compositor exits, layer-shika doesn't end its event loop: it logs
//! "Io error: Broken pipe" over and over, so the process stays alive (filling
//! whatever log its stderr goes to) and `helium start` in the next session
//! took it for a running applet. The compositor's socket identifies the
//! session: it is removed when the compositor exits, and a new compositor
//! creates a new one, even under the same name.

use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::Duration;

/// How often the socket is checked.
const INTERVAL: Duration = Duration::from_secs(3);

/// The compositor's socket: `$WAYLAND_DISPLAY`, relative to
/// `$XDG_RUNTIME_DIR` unless it's absolute.
fn socket_path() -> Option<PathBuf> {
    let display = std::env::var_os("WAYLAND_DISPLAY").unwrap_or_else(|| "wayland-0".into());
    let display = PathBuf::from(display);
    if display.is_absolute() {
        return Some(display);
    }
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
    Some(runtime.join(display))
}

/// Device and inode of the compositor's socket, which change when a new
/// compositor creates it.
fn socket_identity() -> Option<(u64, u64)> {
    let meta = std::fs::metadata(socket_path()?).ok()?;
    Some((meta.dev(), meta.ino()))
}

/// Exits this process once the compositor it started under is gone. Does
/// nothing when there's no socket to watch (e.g. `WAYLAND_SOCKET` only).
pub fn exit_with_compositor() {
    let Some(identity) = socket_identity() else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("compositor-watch".into())
        .spawn(move || loop {
            std::thread::sleep(INTERVAL);
            if socket_identity() != Some(identity) {
                std::process::exit(0);
            }
        });
}

/// When the compositor's socket was created, in seconds since the epoch.
fn compositor_started() -> Option<i64> {
    let meta = std::fs::metadata(socket_path()?).ok()?;
    Some(meta.mtime())
}

/// When process `pid` started, in seconds since the epoch.
fn process_started(pid: &str) -> Option<i64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Field 22 (starttime, in clock ticks since boot), counted after the
    // parenthesised command name, which may itself contain spaces.
    let after_comm = &stat[stat.rfind(')')? + 2..];
    let ticks: i64 = after_comm.split_whitespace().nth(19)?.parse().ok()?;
    let boot: i64 = std::fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    // SAFETY: sysconf has no preconditions.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz <= 0 {
        return None;
    }
    Some(boot + ticks / hz as i64)
}

/// Whether process `pid` belongs to the current compositor, rather than being
/// left over from an earlier one. When either time can't be read, it's
/// assumed current.
pub fn is_current(pid: &str) -> bool {
    match (process_started(pid), compositor_started()) {
        // A second of slack: both times are rounded down to whole seconds.
        (Some(process), Some(compositor)) => process + 1 >= compositor,
        _ => true,
    }
}
