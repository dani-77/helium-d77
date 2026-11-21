//! What turns helium-shell into a daemon, like Frost: panels that used to be
//! separate processes live in helium-shell as extra layer surfaces, and
//! `helium <command>` reaches it through the IPC socket (src/ipc.rs).
//!
//! A surface that's already running opens with no process start, Wayland
//! connection, GPU (EGL/Mesa) or fontconfig setup: those took most of the
//! ~200-360 ms the separate launcher and session menu needed to show up.
//!
//! So far the OSD lives here. The standalone `helium-osd` exits when it finds
//! this daemon running, so the OSD never shows twice.

use std::os::fd::AsRawFd;

use helium_wsl::prelude::*;
use helium_wsl::HeliumError;

use crate::apps::osd::{self, OsdAction, OsdState};
use crate::ipc;

/// Slint source for every surface helium-shell hosts. The files have no
/// imports and no names in common, so they can be compiled as one.
pub const SOURCE: &str = concat!(
    include_str!("../ui/bar.slint"),
    "\n",
    include_str!("../ui/osd.slint"),
);

/// Adds the hosted surfaces to the bar's builder. The OSD starts at 1x1 and
/// is only resized up while it shows something, as in the standalone one.
pub fn surfaces(builder: SurfaceInitializer) -> SurfaceInitializer {
    builder
        .surface("Osd")
        .size(1, 1)
        .anchor((AnchorEdge::Top, AnchorEdge::Right))
        .margin(osd::TOP_MARGIN, osd::SIDE_MARGIN, 0, 0)
        .exclusive_zone(0)
        .layer(Layer::Overlay)
        .keyboard(KeyboardMode::None)
}

/// Wires the OSD and the IPC socket into the running shell.
pub fn attach(shell: &mut ShellInstance) -> Result<(), HeliumError> {
    let mut state = OsdState::new();
    shell.on_tick(osd::POLL_INTERVAL, move |ctx| match state.poll() {
        OsdAction::Show(update) => {
            ctx.set("Osd", "glyph", update.glyph);
            ctx.set("Osd", "label", update.label);
            ctx.set("Osd", "level", update.level.unwrap_or(0.0));
            ctx.set("Osd", "show_progress", update.level.is_some());
            let _ = ctx
                .control()
                .surface_by_name("Osd")
                .resize(osd::OSD_WIDTH, osd::OSD_HEIGHT);
        }
        OsdAction::Hide => {
            let _ = ctx.control().surface_by_name("Osd").resize(1, 1);
        }
        OsdAction::Nothing => {}
    })?;

    // Without the socket (say, a second helium-shell on the same display)
    // the bar and its OSD still work; only `helium <command>` can't reach it.
    let server = match ipc::Server::bind() {
        Ok(server) => server,
        Err(err) => {
            eprintln!("helium-shell: no IPC socket: {err}");
            return Ok(());
        }
    };
    let fd = server.socket.as_raw_fd();
    // The closure owns `server`, so the fd stays open as long as the event
    // loop watches it.
    shell.on_ipc(fd, move |_ctx| {
        for command in server.drain() {
            handle(&command);
        }
    })?;
    Ok(())
}

/// Runs one IPC command. Nothing is routed here yet beyond the socket
/// itself, which also tells other processes that helium-shell is running.
fn handle(command: &str) {
    eprintln!("helium-shell: unknown IPC command {command:?}");
}
