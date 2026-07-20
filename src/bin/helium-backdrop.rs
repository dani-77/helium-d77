//! Decorative background shown while no wallpaper is set, mirroring
//! quickshell-d77/fabric-d77's Backdrop — a plain dark fill with a couple of
//! accent panels instead of a blank screen before you've picked a
//! wallpaper (or after clearing one via `helium-wallpaper`'s Clear button).
//!
//! One surface per connected output, for free: `layer-shika`'s
//! `OutputPolicy` defaults to `AllOutputs`, so simply not calling
//! `.output_policy()` already gives every monitor its own instance — the
//! same effect quickshell-d77 gets explicitly via `Variants { model:
//! Quickshell.screens }`.
//!
//! Sized via the wlr-layer-shell "size 0x0 + anchor all four edges = let
//! the compositor assign the size" convention (see `SurfaceDimension`'s own
//! doc comment in layer-shika-domain) — the default when `.size()` is never
//! called on the surface builder, so nothing below needs to request it
//! explicitly. This idiom was *not* the cause of the backdrop once showing
//! up pinned to a small square in the top-left corner on Hyprland: the
//! wlr-layer-shell surface itself was already being sized correctly the
//! whole time (confirmed via `hyprctl -j layers`). The real cause was this
//! component's own root `Window` explicitly declaring `width`/`height` as
//! fixed literals — Slint treats an explicit size on the root element as
//! authoritative, so the *rendered content* stayed pinned to that literal
//! size no matter what size the surface itself grew to. Leaving `width`/
//! `height` undeclared here (as `ui/osd.slint`'s root `Window` already
//! does) lets the backend's `resize()`/`set_size()` calls actually reach
//! the rendered content, same as `helium-osd`'s surface.
//!
//! `resize_backdrop_to_outputs()` below still explicitly queries each
//! monitor's real pixel resolution and resizes each output's instance to
//! match, the same way the bar's `primary_monitor_width()` in src/main.rs
//! does (Hyprland/niri/Sway each have their own query — `helium_wsl::
//! compositors` doesn't cover Sway at all and has a known bug for niri,
//! see that function's doc comment) — now redundant with the anchor-based
//! auto-fill above (both should agree on the same size), kept as an
//! explicit belt-and-suspenders in case some compositor's "0x0 + anchor
//! all" handling turns out to need it after all.
//!
//! Sits on `Layer::Bottom`, one step above the real `Layer::Background`
//! layer where hyprpaper/swaybg/swww draw the actual wallpaper — same
//! reasoning as quickshell-d77's Backdrop.qml gives for not using
//! `Background` itself (two layer-shell clients competing for the same
//! layer causes flicker/unpredictable z-order). `layer-shika` has no
//! input-region/click-through API (nothing under
//! layer-shika-adapters/layer-shika-domain sets `wl_surface.
//! set_input_region`), so unlike quickshell-d77's Backdrop.qml (which
//! explicitly sets `mask: Region { item: Item {} }`), this surface does
//! claim pointer input over the areas it covers. In practice that's the
//! same regions hyprpaper/swaybg/swww already claim whenever a wallpaper
//! *is* set — bare desktop background in a tiling compositor with no
//! desktop-icon manager isn't interactive either way, and compositor-level
//! mouse binds (e.g. Hyprland's `bindm`) are intercepted before reaching
//! any client surface regardless — but this hasn't been confirmed against
//! every compositor/setup, so flag it if you hit a case where it matters.
//!
//! Only already-connected outputs at startup are resized (a few retries a
//! moment apart to cover output info not being known quite yet) — a
//! monitor hot-plugged later keeps whatever size it started at, same
//! single-point-in-time scope the bar's own monitor-width detection has.
//!
//! Not autostarted by helium-shell itself — add it to your compositor's
//! autostart alongside helium-osd (see the README's OSD section for the
//! exact syntax on Hyprland/niri).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use layer_shika::calloop::TimeoutAction;
use layer_shika::prelude::*;
use layer_shika::slint_interpreter::Value;

const POLL_INTERVAL: Duration = Duration::from_secs(2);

fn state_file_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    PathBuf::from(home).join(".cache/helium/wallpaper/current")
}

/// True once `helium-wallpaper` has an active wallpaper applied: the state
/// file exists, is non-empty, and the path it names still exists on disk.
/// Mirrors quickshell-d77's `Services.WallpaperState.hasWallpaper`, just
/// polled here instead of watched via inotify (`layer-shika` exposes no
/// filesystem-watch integration for its calloop event loop, so polling on
/// the same timer OSD already uses this pattern for is the simplest fit).
fn has_wallpaper() -> bool {
    let Ok(contents) = std::fs::read_to_string(state_file_path()) else { return false };
    let path = contents.trim();
    !path.is_empty() && Path::new(path).is_file()
}

/// Real pixel width/height of every connected monitor, keyed by its
/// connector name (e.g. "eDP-1", "DP-2") — the same name `OutputInfo::
/// name()` reports for a `wl_output`, so the two can be matched up in
/// `resize_backdrop_to_outputs()`. See this file's own top doc comment for
/// why this is queried directly instead of trusting layer-shika's
/// automatic output-fill sizing.
fn compositor_monitors() -> HashMap<String, (u32, u32)> {
    if std::env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok() {
        hyprland_monitors()
    } else if std::env::var("SWAYSOCK").is_ok() {
        sway_monitors()
    } else if std::env::var("NIRI_SOCKET").is_ok() {
        niri_monitors()
    } else {
        HashMap::new()
    }
}

/// Shells out to `hyprctl -j monitors` rather than talking to Hyprland's
/// socket directly (unlike `hypr_command()` in src/main.rs, which is on a
/// per-second timer and so avoids the subprocess overhead) — this only
/// runs a handful of times right at startup, where simplicity wins.
fn hyprland_monitors() -> HashMap<String, (u32, u32)> {
    let Ok(output) = Command::new("hyprctl").args(["-j", "monitors"]).output() else {
        return HashMap::new();
    };
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return HashMap::new();
    };
    let Some(list) = parsed.as_array() else { return HashMap::new() };
    list.iter()
        .filter_map(|m| {
            let name = m.get("name")?.as_str()?.to_string();
            let width = m.get("width")?.as_u64()? as u32;
            let height = m.get("height")?.as_u64()? as u32;
            Some((name, (width, height)))
        })
        .collect()
}

/// Same raw i3-ipc `GET_OUTPUTS` query as `sway_command()`/
/// `sway_monitor_width()` in src/main.rs, duplicated here rather than
/// shared — binaries under src/bin/ don't share modules with src/main.rs
/// in this project (see helium-session/helium-locker/helium-osd for the
/// same pattern) — extended to return every output's real size, not just
/// the focused one.
fn sway_monitors() -> HashMap<String, (u32, u32)> {
    const GET_OUTPUTS: u32 = 3;
    let Some(body) = sway_command(GET_OUTPUTS, b"") else { return HashMap::new() };
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return HashMap::new();
    };
    let Some(list) = parsed.as_array() else { return HashMap::new() };
    list.iter()
        .filter_map(|o| {
            let name = o.get("name")?.as_str()?.to_string();
            let width = o.get("rect")?.get("width")?.as_u64()? as u32;
            let height = o.get("rect")?.get("height")?.as_u64()? as u32;
            Some((name, (width, height)))
        })
        .collect()
}

fn sway_command(msg_type: u32, payload: &[u8]) -> Option<Vec<u8>> {
    let sock_path = std::env::var("SWAYSOCK").ok()?;
    let mut stream = UnixStream::connect(&sock_path).ok()?;

    let mut request = Vec::with_capacity(14 + payload.len());
    request.extend_from_slice(b"i3-ipc");
    request.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    request.extend_from_slice(&msg_type.to_ne_bytes());
    request.extend_from_slice(payload);
    stream.write_all(&request).ok()?;

    let mut header = [0u8; 14];
    stream.read_exact(&mut header).ok()?;
    if &header[0..6] != b"i3-ipc" {
        return None;
    }
    let len = u32::from_ne_bytes(header[6..10].try_into().ok()?) as usize;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).ok()?;
    Some(body)
}

/// Same raw JSON-over-socket `Outputs` query as `niri_command()`/
/// `niri_monitor_width()` in src/main.rs, duplicated here for the same
/// reason `sway_monitors()` above duplicates its Sway counterpart —
/// extended to return every output keyed by its own connector name
/// (the JSON object's key) instead of bailing unless there's exactly one.
fn niri_monitors() -> HashMap<String, (u32, u32)> {
    let Some(reply) = niri_command(r#"{"Outputs":null}"#) else { return HashMap::new() };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(reply.trim()) else {
        return HashMap::new();
    };
    let Some(outputs) =
        value.get("Ok").and_then(|v| v.get("Outputs")).and_then(|v| v.as_object())
    else {
        return HashMap::new();
    };
    outputs
        .iter()
        .filter_map(|(name, o)| {
            let logical = o.get("logical")?;
            let width = logical.get("width")?.as_u64()? as u32;
            let height = logical.get("height")?.as_u64()? as u32;
            Some((name.clone(), (width, height)))
        })
        .collect()
}

fn niri_command(req: &str) -> Option<String> {
    let path = std::env::var("NIRI_SOCKET").ok()?;
    let mut stream = UnixStream::connect(&path).ok()?;
    stream.write_all(req.as_bytes()).ok()?;
    stream.write_all(b"\n").ok()?;
    let mut reader = std::io::BufReader::new(stream);
    let mut line = String::new();
    std::io::BufRead::read_line(&mut reader, &mut line).ok()?;
    Some(line)
}

const SOURCE: &str = r#"
export component Backdrop inherits Window {
    background: transparent;

    in property <bool> has_wallpaper: false;

    Rectangle {
        visible: !root.has_wallpaper;
        width: 100%;
        height: 100%;
        background: #0d0d0d;
        clip: true;

        // Two oversized corner accents, same idea as quickshell-d77's
        // Backdrop.qml diagonal chevrons (which use QML's `rotation`, a
        // property Slint's own element language has no equivalent for —
        // there's no rotation transform on plain Rectangle/Image items in
        // Slint 1.17), just axis-aligned instead of angled, in this
        // project's own green accent instead of that one's purple.
        Rectangle {
            x: parent.width * 0.62;
            y: -parent.height * 0.15;
            width: parent.width * 0.55;
            height: parent.height * 0.75;
            background: #14260a;
        }

        Rectangle {
            x: parent.width * 0.8;
            y: -parent.height * 0.1;
            width: parent.width * 0.32;
            height: parent.height * 0.6;
            background: #1f3319;
        }

        // d77 emblem in the bottom-left corner, mirroring quickshell-d77's
        // Backdrop.qml logo placement (same 48px margins, same 0.25
        // opacity) but recolored to this project's green accent instead of
        // that one's purple — the source SVG is a plain black silhouette
        // with the emblem shape carried entirely in its alpha channel, so
        // `colorize` can retint it to any accent color directly.
        Image {
            x: 48px;
            y: parent.height - 48px - 130px;
            width: 130px;
            height: 130px;
            source: @image-url("/usr/share/helium-d77/d77-logo.svg");
            colorize: #76b900;
            opacity: 0.25;
        }
    }
}
"#;

const RESIZE_RETRY_INTERVAL: Duration = Duration::from_millis(500);
const RESIZE_MAX_ATTEMPTS: u32 = 6;

fn main() -> Result<()> {
    let mut shell = Shell::from_source(SOURCE)
        .surface("Backdrop")
        .anchor(AnchorEdges::all())
        .layer(Layer::Bottom)
        .exclusive_zone(-1)
        .keyboard_interactivity(KeyboardInteractivity::None)
        .build()?;

    let event_loop = shell.event_loop_handle();
    let control = shell.control();
    let mut last = has_wallpaper();

    // Seed every already-connected output's instance before the first poll,
    // so a wallpaper chosen before this process started is respected
    // immediately instead of only after the first tick.
    shell.with_all_surfaces(|_name, instance| {
        instance.set_property("has_wallpaper", Value::Bool(last)).ok();
    });

    // Explicitly resize each output's own Backdrop instance to that
    // output's real pixel resolution — see this file's top doc comment for
    // why the automatic "0x0 + anchor all edges" fill isn't trusted here.
    // Retries a few times a moment apart in case an output's name/handle
    // isn't known to layer-shika quite yet on the very first tick.
    let mut resize_attempts = 0u32;
    event_loop.add_timer(RESIZE_RETRY_INTERVAL, move |_, app_state| {
        resize_attempts += 1;
        let monitors = compositor_monitors();
        let mut all_matched = !monitors.is_empty();
        for info in app_state.all_output_info() {
            match info.name().and_then(|name| monitors.get(name)) {
                Some(&(width, height)) => {
                    let _ = control
                        .surface_by_name_and_output("Backdrop", info.handle())
                        .resize(width, height);
                }
                None => all_matched = false,
            }
        }
        if all_matched || resize_attempts >= RESIZE_MAX_ATTEMPTS {
            TimeoutAction::Drop
        } else {
            TimeoutAction::ToDuration(RESIZE_RETRY_INTERVAL)
        }
    })?;

    event_loop.add_timer(POLL_INTERVAL, move |_, app_state| {
        let now = has_wallpaper();
        if now != last {
            last = now;
            for surface in app_state.surfaces_by_name("Backdrop") {
                surface.component_instance().set_property("has_wallpaper", Value::Bool(now)).ok();
            }
        }
        TimeoutAction::ToDuration(POLL_INTERVAL)
    })?;

    shell.run()?;
    Ok(())
}
