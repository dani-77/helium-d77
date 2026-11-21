//! What turns helium-shell into a daemon, like Frost: panels that used to be
//! separate processes live in helium-shell as extra layer surfaces, and
//! `helium <command>` reaches it through the IPC socket (src/ipc.rs).
//!
//! A surface that's already running opens with no process start, Wayland
//! connection, GPU (EGL/Mesa) or fontconfig setup: those took most of the
//! ~200-360 ms the separate launcher and session menu needed to show up.
//!
//! Hosted so far: the OSD and the session menu. Each stays at 2x1
//! (`HIDDEN_SIZE`) with no keyboard focus while closed and is resized up only while it shows. The
//! standalone `helium-osd` exits when it finds this daemon running, and
//! `helium-session` asks it to toggle its menu, so nothing shows twice.
//!
//! IPC commands: `session` toggles the session menu.

use std::cell::Cell;
use std::os::fd::AsRawFd;
use std::rc::Rc;

use helium_wsl::prelude::*;
use helium_wsl::raw::prelude::{KeyboardInteractivity, ShellControl};
use helium_wsl::slint_interpreter::Value;
use helium_wsl::HeliumError;

use crate::apps::osd::{self, OsdAction, OsdState, HIDDEN_SIZE};
use crate::apps::session::{self, Action};
use crate::ipc;

/// Slint source for every surface helium-shell hosts. The files have no
/// imports and no names in common, so they can be compiled as one. The
/// session menu's source is generated from its actions.
pub fn source(session_actions: &[Action]) -> String {
    [
        include_str!("../ui/bar.slint"),
        include_str!("../ui/osd.slint"),
        &session::build_slint_source(session_actions),
    ]
    .join("\n")
}

/// Adds the hosted surfaces to the bar's builder, all at `HIDDEN_SIZE` with no
/// keyboard focus until they show something.
pub fn surfaces(builder: SurfaceInitializer) -> SurfaceInitializer {
    builder
        .surface("Osd")
        .size(HIDDEN_SIZE.0, HIDDEN_SIZE.1)
        .anchor((AnchorEdge::Top, AnchorEdge::Right))
        .margin(osd::TOP_MARGIN, osd::SIDE_MARGIN, 0, 0)
        .exclusive_zone(0)
        .layer(Layer::Overlay)
        .keyboard(KeyboardMode::None)
        .surface("SessionMenu")
        .size(HIDDEN_SIZE.0, HIDDEN_SIZE.1)
        .anchor((AnchorEdge::Top, AnchorEdge::Right))
        .margin(session::TOP_MARGIN, session::SIDE_MARGIN, 0, 0)
        .exclusive_zone(0)
        .layer(Layer::Overlay)
        .keyboard(KeyboardMode::None)
}

/// A hosted panel that opens and closes: its surface name, open size and
/// whether it's open.
struct Panel {
    surface: &'static str,
    width: u32,
    height: u32,
    open: Cell<bool>,
}

impl Panel {
    fn open(&self, control: &ShellControl) {
        let handle = control.surface_by_name(self.surface);
        let _ = handle.resize(self.width, self.height);
        let _ = handle.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        self.open.set(true);
    }

    fn close(&self, control: &ShellControl) {
        let handle = control.surface_by_name(self.surface);
        let _ = handle.set_keyboard_interactivity(KeyboardInteractivity::None);
        let _ = handle.resize(HIDDEN_SIZE.0, HIDDEN_SIZE.1);
        self.open.set(false);
    }
}

/// Wires the OSD, the session menu and the IPC socket into the running shell.
pub fn attach(shell: &mut ShellInstance, session_actions: Rc<Vec<Action>>) -> Result<(), HeliumError> {
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
            let _ = ctx
                .control()
                .surface_by_name("Osd")
                .resize(HIDDEN_SIZE.0, HIDDEN_SIZE.1);
        }
        OsdAction::Nothing => {}
    })?;

    let session_menu = Rc::new(Panel {
        surface: "SessionMenu",
        width: session::WINDOW_WIDTH,
        height: session::WINDOW_HEIGHT,
        open: Cell::new(false),
    });
    {
        let menu = session_menu.clone();
        let control = shell.control();
        shell.on_signal("SessionMenu", "action_clicked", move |args| {
            if let Some(Value::Number(n)) = args.first() {
                session::run_action(&session_actions, *n as i32);
            }
            menu.close(&control);
        });
    }

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
    shell.on_ipc(fd, move |ctx| {
        for command in server.drain() {
            match command.as_str() {
                "session" => {
                    if session_menu.open.get() {
                        session_menu.close(&ctx.control());
                    } else {
                        // Every opening starts on the first action, as a
                        // freshly started menu did.
                        ctx.set("SessionMenu", "selected_index", 0);
                        session_menu.open(&ctx.control());
                    }
                }
                other => eprintln!("helium-shell: unknown IPC command {other:?}"),
            }
        }
    })?;
    Ok(())
}
