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
//! `TextInput`, just a `FocusScope` around the button row. Built directly
//! on raw `layer_shika::Shell` (not helium-wsl's `Helium` wrapper) for the
//! same reason as the launcher: pushing `selected_index` back onto the
//! surface from inside the `navigate` callback needs
//! `ComponentInstance::as_weak()`, which the wrapper's `on_signal` doesn't
//! give access to. See helium-launcher.rs's doc comment for the full story
//! on why keyboard input works fine here despite `Helium::on_key` being a
//! stub upstream.

use layer_shika::prelude::*;
use layer_shika::slint_interpreter::{ComponentHandle, Value};
use std::cell::RefCell;
use std::process::Command;
use std::rc::Rc;

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
/// helium-locker is temporarily disabled here: its session-lock surface
/// (which uses wp_fractional_scale + wp_viewporter) gets keyboard/pointer
/// focus and then the compositor immediately cancels the lock via
/// `ext_session_lock_v1.finished()` ~30ms later — confirmed with a
/// WAYLAND_DEBUG=1 trace on niri, and reproduced the same way on Hyprland,
/// so it's an upstream layer-shika bug, not a bug in helium-locker.rs/
/// lock.slint, and not specific to either compositor. Revert to preferring
/// helium-locker (see git blame) once that's fixed upstream.
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

/// Left-to-right navigation order matching the button row: Cancel (-1)
/// first, then each action by index — so Left/Right cycles visually.
fn nav_order(action_count: usize) -> Vec<i32> {
    std::iter::once(-1).chain(0..action_count as i32).collect()
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
    background: is_selected ? #1f3319 : #141414;
    border-radius: 8px;
    border-width: is_selected ? 1px : 0px;
    border-color: #76b900;

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
    callback navigate(int);

    in property <int> selected_index: 0;

    forward-focus: nav_scope;

    Rectangle {{
        background: #0d0d0d;
        border-radius: 14px;
        clip: true;

        nav_scope := FocusScope {{
            key-pressed(event) => {{
                if (event.text == Key.RightArrow) {{
                    navigate(1);
                    accept
                }} else if (event.text == Key.LeftArrow) {{
                    navigate(-1);
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

fn main() -> Result<()> {
    let actions = Rc::new(actions());
    let source = build_slint_source(&actions);

    let mut shell = Shell::from_source(source)
        .surface("SessionMenu")
        .size(WINDOW_WIDTH, WINDOW_HEIGHT)
        .anchor(AnchorEdges::empty().with_top().with_right())
        .margin(Margins::new(56, 10, 0, 0))
        .layer(Layer::Overlay)
        .keyboard_interactivity(KeyboardInteractivity::Exclusive)
        .build()?;

    // Mirrors the Slint-side `selected_index` property so `navigate` can
    // compute the next selection without reading it back through the
    // interpreter.
    let selected_state = Rc::new(RefCell::new(0i32));

    shell.with_surface("SessionMenu", |comp| {
        let weak = comp.as_weak();
        let (actions_n, selected_n) = (actions.clone(), selected_state.clone());
        comp.set_callback("navigate", move |args| {
            let Some(Value::Number(delta)) = args.first() else { return Value::Void };
            let delta = *delta as i32;
            let order = nav_order(actions_n.len());
            let current = *selected_n.borrow();
            let pos = order.iter().position(|&i| i == current).unwrap_or(0) as i32;
            let len = order.len() as i32;
            let new_pos = ((pos + delta) % len + len) % len;
            let new_selected = order[new_pos as usize];
            *selected_n.borrow_mut() = new_selected;
            if let Some(instance) = weak.upgrade() {
                instance.set_property("selected_index", Value::Number(f64::from(new_selected))).ok();
            }
            Value::Void
        }).ok();

        let actions_c = actions.clone();
        comp.set_callback("action_clicked", move |args| {
            let Some(Value::Number(n)) = args.first() else { return Value::Void };
            let idx = *n as i32;
            if idx >= 0 {
                if let Some(action) = actions_c.get(idx as usize) {
                    let _ = Command::new("sh").arg("-c").arg(&action.command).spawn();
                }
            }
            std::process::exit(0);
        }).ok();
    })?;

    shell.run()?;
    Ok(())
}
