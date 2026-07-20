//! Decorative background shown while no wallpaper is set, mirroring
//! quickshell-d77/fabric-d77's Backdrop — a plain dark fill with a couple of
//! accent panels instead of a blank screen before you've picked a
//! wallpaper (or after clearing one via `helium-wallpaper`'s Clear button).
//!
//! One surface per connected output, for free: `layer-shika`'s
//! `OutputPolicy` defaults to `AllOutputs`, so simply not calling
//! `.output_policy()` already gives every monitor its own instance — the
//! same effect quickshell-d77 gets explicitly via `Variants { model:
//! Quickshell.screens }`. Sized `0x0` with all four edges anchored: per the
//! wlr-layer-shell protocol (see `SurfaceDimension`'s own doc comment in
//! layer-shika-domain), that means "let the compositor assign the size",
//! which is how background/wallpaper-daemon surfaces are meant to size
//! themselves — no manual per-monitor width/height query needed, unlike
//! `primary_monitor_width()` in src/main.rs (which exists specifically
//! because the *bar* can't use this trick: it only anchors three edges and
//! needs an exact width, see that function's own doc comment for why).
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
//! Not autostarted by helium-shell itself — add it to your compositor's
//! autostart alongside helium-osd (see the README's OSD section for the
//! exact syntax on Hyprland/niri).

use std::path::{Path, PathBuf};
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

const SOURCE: &str = r#"
export component Backdrop inherits Window {
    width: 100px;
    height: 100px;
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

        Text {
            x: 48px;
            y: parent.height - 80px;
            text: "helium";
            color: #76b900;
            opacity: 0.25;
            font-size: 26px;
            font-family: "Space Grotesk";
        }
    }
}
"#;

fn main() -> Result<()> {
    let mut shell = Shell::from_source(SOURCE)
        .surface("Backdrop")
        .anchor(AnchorEdges::all())
        .layer(Layer::Bottom)
        .exclusive_zone(0)
        .keyboard_interactivity(KeyboardInteractivity::None)
        .build()?;

    let event_loop = shell.event_loop_handle();
    let mut last = has_wallpaper();

    // Seed every already-connected output's instance before the first poll,
    // so a wallpaper chosen before this process started is respected
    // immediately instead of only after the first tick.
    shell.with_all_surfaces(|_name, instance| {
        instance.set_property("has_wallpaper", Value::Bool(last)).ok();
    });

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
