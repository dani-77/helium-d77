//! On-screen display for helium-d77.
//!
//! A small overlay, top-right, that briefly appears when volume/mute or the
//! active power-profiles-daemon profile changes. Polls `amixer` /
//! `powerprofilesctl` directly on a fast timer rather than reacting to the
//! bar's own click handlers, so it also picks up changes made outside the
//! bar (hardware media keys, `powerprofilesctl` run from a terminal, etc.) —
//! the same trigger model quickshell-d77/fabric-d77 use for their OSDs.
//!
//! Built directly on `layer_shika::Shell` (not helium-wsl's `Helium`/
//! `ShellInstance` wrapper): that wrapper doesn't expose per-surface resize
//! or direct `AppState` access, both needed here. The surface is kept at
//! 1x1 while idle and resized up only while showing (`ShellControl::
//! surface().resize()`, reachable from a `'static` timer closure since it
//! doesn't borrow `Shell`), and properties are set through
//! `AppState::surfaces_by_name` from inside the timer callback itself
//! (its `&mut AppState` argument) rather than `Shell::select()`, which
//! *does* borrow `Shell` and so can't be reached from a `'static` closure.
//!
//! Meant to be autostarted alongside `helium-shell` (see the README);
//! there's nothing for the bar itself to launch it on demand for.

use std::time::{Duration, Instant};

use layer_shika::calloop::TimeoutAction;
use layer_shika::prelude::*;
use layer_shika::slint_interpreter::Value;

const OSD_WIDTH: u32 = 260;
const OSD_HEIGHT: u32 = 64;
// Bar margin (10px) + bar height (36px) + a small gap, so the OSD sits below
// the bar instead of overlapping it: Layer::Overlay ignores the bar's own
// exclusive zone, so this has to be accounted for by hand.
const TOP_MARGIN: i32 = 54;
const SIDE_MARGIN: i32 = 16;
const POLL_INTERVAL: Duration = Duration::from_millis(300);
const VISIBLE_FOR: Duration = Duration::from_millis(2500);

/// Reads Master volume via `amixer`, same control name and parsing as the
/// bar's own `sysinfo::volume()` (not shared code: binaries under src/bin/
/// don't share modules with src/main.rs in this project, see helium-session
/// and helium-locker for the same pattern).
fn read_volume() -> Option<(u8, bool)> {
    let output = std::process::Command::new("amixer").args(["sget", "Master"]).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().find(|l| l.contains('%'))?;
    let percent: u8 = line.split('[').nth(1)?.split('%').next()?.parse().ok()?;
    let muted = line.contains("[off]");
    Some((percent, muted))
}

fn read_power_profile() -> Option<String> {
    let output = std::process::Command::new("powerprofilesctl").arg("get").output().ok()?;
    let profile = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!profile.is_empty()).then_some(profile)
}

fn main() -> Result<()> {
    let mut shell = Shell::from_source(include_str!("../../ui/osd.slint"))
        .surface("Osd")
        .size(1, 1)
        .anchor(AnchorEdges::empty().with_top().with_right())
        .margin(Margins::new(TOP_MARGIN, SIDE_MARGIN, 0, 0))
        .layer(Layer::Overlay)
        .exclusive_zone(0)
        .keyboard_interactivity(KeyboardInteractivity::None)
        .build()?;

    let osd_surface = shell.control().surface("Osd");
    let event_loop = shell.event_loop_handle();

    // Seeded before the first tick so startup itself never counts as a
    // "change" and flashes the OSD on launch.
    let mut last_volume = read_volume();
    let mut last_profile = read_power_profile();
    let mut hide_at: Option<Instant> = None;

    event_loop.add_timer(POLL_INTERVAL, move |_, app_state| {
        let mut changed = false;

        if let Some((percent, muted)) = read_volume() {
            if last_volume != Some((percent, muted)) {
                last_volume = Some((percent, muted));
                let icon = if muted { "\u{F026}" } else { "\u{F028}" };
                let label =
                    if muted { "Muted".to_string() } else { format!("Volume {percent}%") };
                for surface in app_state.surfaces_by_name("Osd") {
                    let instance = surface.component_instance();
                    instance.set_property("glyph", Value::String(icon.into())).ok();
                    instance.set_property("label", Value::String(label.clone().into())).ok();
                    instance.set_property("level", Value::Number(percent as f64 / 100.0)).ok();
                    instance.set_property("show_progress", Value::Bool(true)).ok();
                }
                changed = true;
            }
        }

        if let Some(profile) = read_power_profile() {
            if last_profile.as_deref() != Some(profile.as_str()) {
                last_profile = Some(profile.clone());
                let label = profile.replace('-', " ");
                for surface in app_state.surfaces_by_name("Osd") {
                    let instance = surface.component_instance();
                    instance.set_property("glyph", Value::String("\u{F013}".into())).ok();
                    instance.set_property("label", Value::String(label.clone().into())).ok();
                    instance.set_property("show_progress", Value::Bool(false)).ok();
                }
                changed = true;
            }
        }

        if changed {
            osd_surface.resize(OSD_WIDTH, OSD_HEIGHT).ok();
            hide_at = Some(Instant::now() + VISIBLE_FOR);
        } else if hide_at.is_some_and(|deadline| Instant::now() >= deadline) {
            osd_surface.resize(1, 1).ok();
            hide_at = None;
        }

        TimeoutAction::ToDuration(POLL_INTERVAL)
    })?;

    shell.run()?;
    Ok(())
}
