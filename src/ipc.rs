//! IPC between `helium <command>` and the running helium-shell, like Frost's:
//! a Unix datagram socket per Wayland display, one command per datagram.
//!
//! helium-shell binds it at startup (see src/daemon.rs). Anything else can
//! send it a command, or check whether a helium-shell is running on this
//! display.

use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind};
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;

/// The longest command accepted, in bytes.
pub const MAX_COMMAND: usize = 256;

/// `$XDG_RUNTIME_DIR/helium-<display>.sock`.
pub fn path() -> io::Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| io::Error::other("XDG_RUNTIME_DIR must be an absolute path"))?;
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    // A display name is normally "wayland-N"; anything else is reduced to
    // characters that are safe in a file name.
    let display: String = display
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Ok(runtime.join(format!("helium-{display}.sock")))
}

/// The bound socket, plus the lock file that keeps a second helium-shell on
/// the same display from taking it over.
pub struct Server {
    pub socket: UnixDatagram,
    _lock: File,
}

impl Server {
    pub fn bind() -> io::Result<Self> {
        let path = path()?;
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_extension("lock"))?;
        // SAFETY: flock on a file descriptor we own.
        if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::other(
                "another helium-shell is already running on this display",
            ));
        }
        // A socket file left over from a helium-shell that died: the lock
        // above proves nobody is using it.
        match fs::remove_file(&path) {
            Ok(()) => (),
            Err(err) if err.kind() == ErrorKind::NotFound => (),
            Err(err) => return Err(err),
        }
        let socket = UnixDatagram::bind(&path)?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            _lock: lock,
        })
    }

    /// Every command waiting on the socket, without blocking.
    pub fn drain(&self) -> Vec<String> {
        let mut commands = Vec::new();
        let mut buf = [0u8; MAX_COMMAND];
        while let Ok(len) = self.socket.recv(&mut buf) {
            if let Ok(command) = std::str::from_utf8(&buf[..len]) {
                commands.push(command.trim().to_string());
            }
        }
        commands
    }
}

/// Sends one command to the helium-shell running on this display.
// Used by `helium <command>` once commands are routed here (session menu,
// launcher).
#[allow(dead_code)]
pub fn send(command: &str) -> io::Result<()> {
    if command.len() > MAX_COMMAND {
        return Err(io::Error::other("command too long"));
    }
    let socket = UnixDatagram::unbound()?;
    socket.send_to(command.as_bytes(), path()?)?;
    Ok(())
}

/// Whether a helium-shell is running on this display and listening.
pub fn daemon_running() -> bool {
    let Ok(path) = path() else { return false };
    UnixDatagram::unbound()
        .and_then(|socket| socket.connect(&path))
        .is_ok()
}
