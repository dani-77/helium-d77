//! A click-and-keyboard session menu (suspend / reboot / shutdown / logout).
//!
//! Same spawn-on-demand model as helium-launcher: opened by clicking the
//! power icon in the bar, closes itself after an action (or Cancel/Escape).
//! Mirrors the action set and commands from the quickshell-d77 session
//! menu — `loginctl <action>` with a `systemctl <action>` fallback for
//! suspend/reboot/poweroff, and `loginctl terminate-session` for logout,
//! since that's what actually works across both systemd-logind and elogind.
//!
//! Keyboard nav only (Left/Right to move, Enter to activate, Escape to
//! cancel) — no text field, so unlike helium-launcher this doesn't need a
//! `TextInput`, just a `FocusScope` around the button row. The selection
//! moves in Slint itself, so the only callback is `action_clicked`.
//!
//! helium-shell hosts this menu as one of its own surfaces (src/daemon.rs);
//! started on its own while helium-shell runs, this applet only asks it to
//! toggle the menu. The standalone menu below is for running it without
//! helium-shell.

use layer_shika::prelude::*;
use layer_shika::slint_interpreter::Value;
use std::process::Command;
use std::rc::Rc;

pub struct Action {
    icon: &'static str,
    label: &'static str,
    command: String,
}

fn session_command(action: &str) -> String {
    format!("loginctl {action} 2>&1 || systemctl {action} 2>&1")
}

/// Prefers the sibling `helium-locker` binary (native ext-session-lock-v1,
/// see doc/TECHNICAL.md, Locker), then external lockers.
fn lock_command() -> String {
    // The first locker installed, with loginctl lock-session as a generic
    // fallback. if/elif rather than `a && b || c`: a locker exiting with an
    // error must not fall through and run the next one.
    "if command -v helium-locker >/dev/null 2>&1; then helium-locker; \
     elif command -v amorphis >/dev/null 2>&1; then amorphis --backend wayland --effect blur; \
     elif command -v swaylock >/dev/null 2>&1; then swaylock; \
     elif command -v hyprlock >/dev/null 2>&1; then hyprlock; \
     else loginctl lock-session; fi"
        .to_string()
}

pub fn actions() -> Vec<Action> {
    let session_id = std::env::var("XDG_SESSION_ID").unwrap_or_else(|_| "self".to_string());
    vec![
        Action {
            icon: "\u{F023}",
            label: "Lock",
            command: lock_command(),
        },
        Action {
            icon: "\u{F186}",
            label: "Suspend",
            command: session_command("suspend"),
        },
        Action {
            icon: "\u{F021}",
            label: "Reboot",
            command: session_command("reboot"),
        },
        Action {
            icon: "\u{F011}",
            label: "Shutdown",
            command: session_command("poweroff"),
        },
        Action {
            icon: "\u{F08B}",
            label: "Logout",
            command: format!("loginctl terminate-session \"{session_id}\" 2>&1"),
        },
    ]
}

// 6 buttons * 70px + 5 gaps * 8px + 2 * 8px padding = 476px needed
// (spacing matches the exterior padding); a bit of slack on top so nothing
// gets clipped.
pub const WINDOW_WIDTH: u32 = 500;
pub const WINDOW_HEIGHT: u32 = 120;
// Below the bar, at the right end where its power icon is.
pub const TOP_MARGIN: i32 = 56;
pub const SIDE_MARGIN: i32 = 10;

/// The session menu's Slint source. Arrow-key navigation is done in Slint
/// itself (Cancel, then each action, wrapping around), so the only callback
/// Rust handles is `action_clicked(index)`, with -1 for Cancel/Escape.
pub fn build_slint_source(actions: &[Action]) -> String {
    let last = actions.len() as i32 - 1;
    let mut buttons = String::new();
    for (i, action) in actions.iter().enumerate() {
        buttons.push_str(&format!(
            r#"
                ActionButton {{ icon: "{}"; label: "{}"; is_selected: root.selected_index == {i}; clicked => {{ action_clicked({i}); }} }}"#,
            action.icon, action.label
        ));
    }

    format!(
        r#"
component ActionButton inherits Rectangle {{
    in property <string> icon: "";
    in property <string> label: "";
    in property <bool> is_selected: false;
    callback clicked;

    width: 70px;
    background: is_selected ? Theme.accent_bg : Theme.bg;
    border-radius: 8px;
    border-width: is_selected ? 1px : 0px;
    border-color: Theme.accent;

    VerticalLayout {{
        alignment: center;
        spacing: 6px;

        Text {{
            text: icon;
            color: Theme.fg;
            font-size: 20px;
            font-family: "Symbols Nerd Font";
            horizontal-alignment: center;
        }}

        Text {{
            text: label;
            color: Theme.fg_dim;
            font-size: 10px;
            font-family: "Space Grotesk";
            horizontal-alignment: center;
        }}
    }}

    TouchArea {{ clicked => {{ root.clicked(); }} }}
}}

export component SessionMenu inherits Window {{
    width: {WINDOW_WIDTH}px;
    height: {WINDOW_HEIGHT}px;
    background: transparent;

    callback action_clicked(int);

    in-out property <int> selected_index: 0;

    forward-focus: nav_scope;

    Rectangle {{
        background: Theme.bg_dark;
        border-radius: 14px;
        clip: true;

        nav_scope := FocusScope {{
            key-pressed(event) => {{
                if (event.text == Key.RightArrow) {{
                    root.selected_index = root.selected_index >= {last} ? -1 : root.selected_index + 1;
                    accept
                }} else if (event.text == Key.LeftArrow) {{
                    root.selected_index = root.selected_index <= -1 ? {last} : root.selected_index - 1;
                    accept
                }} else if (event.text == Key.Return || event.text == "\u{{000d}}") {{
                    // See helium-launcher.rs's key-pressed comment: layer-shika-adapters
                    // delivers raw xkbcommon UTF-8 ("\r") for Return instead of Slint's
                    // own `Key.Return` ("\n"), so both need to be checked.
                    action_clicked(root.selected_index);
                    accept
                }} else if (event.text == Key.Escape) {{
                    action_clicked(-1);
                    accept
                }} else {{
                    reject
                }}
            }}

            HorizontalLayout {{
                padding: 8px;
                spacing: 8px;
                alignment: center;

                ActionButton {{ icon: "\u{{F00D}}"; label: "Cancel"; is_selected: root.selected_index == -1; clicked => {{ action_clicked(-1); }} }}
{buttons}
            }}
        }}
    }}
}}
"#
    )
}

pub fn main() -> Result<()> {
    // helium-shell hosts the session menu (src/daemon.rs): toggle that one,
    // the same way kantele and the bar do.
    if crate::ipc::daemon_running() && crate::ipc::send("session").is_ok() {
        return Ok(());
    }

    let actions = Rc::new(actions());
    let source = build_slint_source(&actions);

    let mut shell = Shell::from_source(crate::theme::with_theme(&source))
        .surface("SessionMenu")
        .size(WINDOW_WIDTH, WINDOW_HEIGHT)
        .anchor(AnchorEdges::empty().with_top().with_right())
        .margin(Margins::new(TOP_MARGIN, SIDE_MARGIN, 0, 0))
        .layer(Layer::Overlay)
        .keyboard_interactivity(KeyboardInteractivity::Exclusive)
        .build()?;

    shell.with_surface("SessionMenu", |comp| {
        crate::theme::apply(comp);
        let actions_c = actions.clone();
        comp.set_callback("action_clicked", move |args| {
            if let Some(Value::Number(n)) = args.first() {
                run_action(&actions_c, *n as i32);
            }
            std::process::exit(0);
        })
        .ok();
    })?;

    shell.run()?;
    Ok(())
}

/// Runs the action at `index` (from `action_clicked`); -1 is Cancel.
pub fn run_action(actions: &[Action], index: i32) {
    if index < 0 {
        return;
    }
    if let Some(action) = actions.get(index as usize) {
        if let Ok(mut child) = Command::new("sh").arg("-c").arg(&action.command).spawn() {
            // Reaped from a thread: in helium-shell this process outlives it.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}
