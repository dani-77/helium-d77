//! A minimal click-only session menu (suspend / reboot / shutdown / logout).
//!
//! Same spawn-on-demand model as helium-launcher: opened by clicking the
//! power icon in the bar, closes itself after an action (or "Cancel").
//! Mirrors the action set and commands from the quickshell-d77 session
//! menu — `loginctl <action>` with a `systemctl <action>` fallback for
//! suspend/reboot/poweroff, and `loginctl terminate-session` for logout,
//! since that's what actually works across both systemd-logind and elogind.

use helium_wsl::prelude::*;
use helium_wsl::slint_interpreter;
use std::process::Command;

struct Action {
    icon: &'static str,
    label: &'static str,
    command: String,
}

fn session_command(action: &str) -> String {
    format!("loginctl {action} 2>&1 || systemctl {action} 2>&1")
}

/// Shells out to hyprlock / loginctl rather than the sibling `helium-locker`
/// binary (native ext-session-lock-v1, see its own doc comment).
///
/// helium-locker is temporarily disabled here: under niri, layer-shika's
/// session-lock surface (which uses wp_fractional_scale + wp_viewporter)
/// gets keyboard/pointer focus and then niri immediately cancels the lock
/// via `ext_session_lock_v1.finished()` ~30ms later — confirmed with a
/// WAYLAND_DEBUG=1 trace, not a bug in helium-locker.rs/lock.slint. Revert
/// to preferring helium-locker (see git blame) once that's fixed upstream.
fn lock_command() -> String {
    // hyprlock if present (matches quickshell-d77's lock keybind);
    // loginctl lock-session as a generic fallback otherwise.
    "command -v hyprlock >/dev/null 2>&1 && hyprlock || loginctl lock-session".to_string()
}

fn actions() -> Vec<Action> {
    let session_id = std::env::var("XDG_SESSION_ID").unwrap_or_else(|_| "self".to_string());
    vec![
        Action {
            icon: "\u{F023}",
            label: "Lock",
            command: lock_command(),
        },
        Action { icon: "\u{F186}", label: "Suspend", command: session_command("suspend") },
        Action { icon: "\u{F021}", label: "Reboot", command: session_command("reboot") },
        Action { icon: "\u{F011}", label: "Shutdown", command: session_command("poweroff") },
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
const WINDOW_WIDTH: u32 = 500;
const WINDOW_HEIGHT: u32 = 120;

fn build_slint_source(actions: &[Action]) -> String {
    let mut buttons = String::new();
    for (i, action) in actions.iter().enumerate() {
        buttons.push_str(&format!(
            r#"
                ActionButton {{ icon: "{}"; label: "{}"; clicked => {{ action_clicked({i}); }} }}"#,
            action.icon, action.label
        ));
    }

    format!(
        r#"
component ActionButton inherits Rectangle {{
    in property <string> icon: "";
    in property <string> label: "";
    callback clicked;

    width: 70px;
    background: #141414;
    border-radius: 8px;

    VerticalLayout {{
        alignment: center;
        spacing: 6px;

        Text {{
            text: icon;
            color: #d4d4d4;
            font-size: 20px;
            font-family: "Symbols Nerd Font";
            horizontal-alignment: center;
        }}

        Text {{
            text: label;
            color: #999;
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

    Rectangle {{
        background: #0d0d0d;
        border-radius: 14px;
        clip: true;

        HorizontalLayout {{
            padding: 8px;
            spacing: 8px;
            alignment: center;

            ActionButton {{ icon: "\u{{F00D}}"; label: "Cancel"; clicked => {{ action_clicked(-1); }} }}
{buttons}
        }}
    }}
}}
"#
    )
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let actions = actions();
    let source = build_slint_source(&actions);

    let mut shell = Helium::from_source(source)
        .surface("SessionMenu")
        .size(WINDOW_WIDTH, WINDOW_HEIGHT)
        .anchor((AnchorEdge::Top, AnchorEdge::Right))
        .margin(56, 10, 0, 0)
        .layer(Layer::Overlay)
        .build()?;

    shell.on_signal("SessionMenu", "action_clicked", move |args| {
        let Some(slint_interpreter::Value::Number(n)) = args.first() else { return };
        let idx = *n as i32;
        if idx >= 0 {
            if let Some(action) = actions.get(idx as usize) {
                let _ = Command::new("sh").arg("-c").arg(&action.command).spawn();
            }
        }
        std::process::exit(0);
    });

    shell.run()?;
    Ok(())
}
