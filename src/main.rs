//! `helium`: every helium-d77 program in one binary.
//!
//! The programs used to be eight separate binaries, each carrying its own copy
//! of Slint, helium-wsl, layer-shika, tokio and zbus (over 90% of each one).
//! They are now applets of this binary, chosen either by subcommand
//! (`helium launcher`) or, busybox style, by the name it was started as: the
//! packages install `helium-shell`, `helium-launcher`, ... as symlinks to
//! `helium`, so compositor configs, kantele and process lookups by name keep
//! working unchanged.
//!
//! Each applet still runs in its own process, exactly as before.

mod apps;
mod ext_workspace;
mod network;
mod sysinfo;
mod upower;
mod weather;

use std::fmt::Debug;
use std::path::Path;
use std::process::ExitCode;

/// Applet names, as subcommands. The symlinks are these with a `helium-`
/// prefix.
pub const APPLETS: [&str; 8] = [
    "shell",
    "launcher",
    "session",
    "osd",
    "wallpaper",
    "backdrop",
    "ollama",
    "locker",
];

/// What `helium start` launches: the programs that run for the whole session.
const SESSION_APPLETS: [&str; 3] = ["shell", "osd", "backdrop"];

fn usage() -> String {
    format!(
        "helium {}\n\nUsage: helium start\n       helium <{}> [args...]\n\n\
         `helium start` launches the bar, the OSD and the backdrop, each in its own\n\
         process. Each applet can also be started through its helium-<applet> symlink.",
        env!("CARGO_PKG_VERSION"),
        APPLETS.join("|")
    )
}

/// Whether a process named `name` (its `comm`, as `pgrep -x` sees it) is
/// already running for this user.
fn is_running(name: &str) -> bool {
    let uid = unsafe { libc::getuid() };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    entries.flatten().any(|entry| {
        use std::os::unix::fs::MetadataExt;
        let path = entry.path();
        entry.metadata().is_ok_and(|m| m.uid() == uid)
            && std::fs::read_to_string(path.join("comm")).is_ok_and(|comm| comm.trim_end() == name)
    })
}

/// `helium start`: launches the session applets, each in its own process
/// under its helium-<applet> name, and exits. One autostart line instead of
/// three; an applet already running is left alone.
fn start() -> ExitCode {
    use std::os::unix::process::CommandExt;
    let Ok(exe) = std::env::current_exe() else {
        eprintln!("helium: cannot find its own executable");
        return ExitCode::FAILURE;
    };
    let dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut failed = false;
    for applet in SESSION_APPLETS {
        let name = format!("helium-{applet}");
        if is_running(&name) {
            continue;
        }
        // Through the symlink when installed, so the process gets its name;
        // in a target/ build, this binary with that name as argv[0].
        let link = dir.join(&name);
        let mut cmd = if link.exists() {
            std::process::Command::new(link)
        } else {
            let mut cmd = std::process::Command::new(&exe);
            cmd.arg0(&name);
            cmd
        };
        // Its own session, so it outlives this short-lived process and
        // isn't tied to the terminal or autostart entry that ran it.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        if let Err(err) = cmd.spawn() {
            eprintln!("helium: could not start {name}: {err}");
            failed = true;
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Turns an applet's `Result` into an exit code, printing the error the way
/// returning it from `main` used to.
fn report<E: Debug>(result: Result<(), E>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {err:?}");
            ExitCode::FAILURE
        }
    }
}

fn run(applet: &str, args: &[String]) -> Option<ExitCode> {
    Some(match applet {
        "shell" => report(apps::shell::main()),
        "launcher" => report(apps::launcher::main()),
        "session" => report(apps::session::main()),
        "osd" => report(apps::osd::main()),
        "wallpaper" => report(apps::wallpaper::main(args)),
        "backdrop" => report(apps::backdrop::main()),
        "ollama" => report(apps::ollama::main()),
        "locker" | "lock" => apps::locker::main(args),
        _ => return None,
    })
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().collect();
    let argv0 = if args.is_empty() {
        String::new()
    } else {
        args.remove(0)
    };
    let name = Path::new(&argv0)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    // Started through a helium-<applet> symlink (or with that argv[0]).
    if let Some(applet) = name.strip_prefix("helium-") {
        if let Some(code) = run(applet, &args) {
            return code;
        }
    }

    // `helium <applet> [args...]`.
    match args.first().map(String::as_str) {
        Some("--version" | "-V") => {
            println!("helium {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("--help" | "-h") => {
            println!("{}", usage());
            ExitCode::SUCCESS
        }
        Some("start") if args.len() == 1 => start(),
        Some(applet) => run(applet, &args[1..]).unwrap_or_else(|| {
            eprintln!("{}", usage());
            ExitCode::from(2)
        }),
        None => {
            eprintln!("{}", usage());
            ExitCode::from(2)
        }
    }
}
