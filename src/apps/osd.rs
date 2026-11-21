//! On-screen display for helium-d77.
//!
//! A small overlay, top-right, that briefly appears when volume/mute,
//! screen brightness, or the active power-profiles-daemon profile changes.
//! Polls `amixer` / `brightnessctl` / `powerprofilesctl` directly on a fast
//! timer rather than reacting to the bar's own click handlers, so it also
//! picks up changes made outside the bar (hardware media keys,
//! `brightnessctl`/`powerprofilesctl` run from a terminal, etc.) — the same
//! trigger model quickshell-d77/fabric-d77 use for their OSDs.
//!
//! Built directly on `layer_shika::Shell` (not helium-wsl's `Helium`/
//! `ShellInstance` wrapper): that wrapper doesn't expose per-surface resize
//! or direct `AppState` access, both needed here. The surface is kept at
//! 2x1 (`HIDDEN_SIZE`) while idle and resized up only while showing (`ShellControl::
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

pub const OSD_WIDTH: u32 = 260;
/// Size while hidden. Not 1x1: layer-shika takes a requested width of 1 as
/// "the whole output", which left a 1px-high strip across the screen.
pub const HIDDEN_SIZE: (u32, u32) = (2, 1);
pub const OSD_HEIGHT: u32 = 64;
// Bar margin (10px) + bar height (36px) + a small gap, so the OSD sits below
// the bar instead of overlapping it: Layer::Overlay ignores the bar's own
// exclusive zone, so this has to be accounted for by hand.
pub const TOP_MARGIN: i32 = 54;
pub const SIDE_MARGIN: i32 = 16;
// Was 300ms: 3 subprocess spawns/cycle (amixer, brightnessctl,
// powerprofilesctl — the latter alone ~500ms per fabric-d77's osd.py, i.e.
// slower than this interval), forever in the background regardless of OSD
// visibility. Nothing about "someone else changed it" detection needs
// sub-second latency; 2s cuts that by ~85% for the same behavior.
pub const POLL_INTERVAL: Duration = Duration::from_millis(2000);
const VISIBLE_FOR: Duration = Duration::from_millis(2500);

/// Reads Master volume via `amixer`, same control name and parsing as the
/// bar's own `sysinfo::volume()` (kept separate: the applets were separate
/// binaries that couldn't share modules, see helium-session and helium-locker
/// for the same pattern).
fn read_volume() -> Option<(u8, bool)> {
    let output = std::process::Command::new("amixer")
        .args(["sget", "Master"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().find(|l| l.contains('%'))?;
    let percent: u8 = line.split('[').nth(1)?.split('%').next()?.parse().ok()?;
    let muted = line.contains("[off]");
    Some((percent, muted))
}

/// Reads screen brightness (0-100) via `brightnessctl -m`, same
/// machine-readable parsing (field 4 of the comma-separated output) as
/// fabric-d77/quickshell-d77's OSDs.
fn read_brightness() -> Option<u8> {
    let output = std::process::Command::new("brightnessctl")
        .arg("-m")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let field = text.trim().split(',').nth(3)?;
    field.trim_end_matches('%').parse().ok()
}

fn read_power_profile() -> Option<String> {
    let output = std::process::Command::new("powerprofilesctl")
        .arg("get")
        .output()
        .ok()?;
    let profile = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!profile.is_empty()).then_some(profile)
}

/// What the OSD should show after a change.
pub struct OsdUpdate {
    pub glyph: &'static str,
    pub label: String,
    /// `Some(0.0..=1.0)` shows the progress bar at that level; `None` hides it.
    pub level: Option<f64>,
}

impl OsdUpdate {
    /// Applies this update to an `Osd` component instance.
    pub fn apply(&self, instance: &layer_shika::slint_interpreter::ComponentInstance) {
        instance
            .set_property("glyph", Value::String(self.glyph.into()))
            .ok();
        instance
            .set_property("label", Value::String(self.label.clone().into()))
            .ok();
        instance
            .set_property("level", Value::Number(self.level.unwrap_or(0.0)))
            .ok();
        instance
            .set_property("show_progress", Value::Bool(self.level.is_some()))
            .ok();
    }
}

/// What a poll decided: show an update, hide the OSD, or leave it as it is.
pub enum OsdAction {
    Show(OsdUpdate),
    Hide,
    Nothing,
}

/// Change detection shared by the standalone `helium-osd` and the OSD hosted
/// in helium-shell: remembers the last values and when to hide again.
pub struct OsdState {
    last_volume: Option<(u8, bool)>,
    last_brightness: Option<u8>,
    last_profile: Option<String>,
    hide_at: Option<Instant>,
}

impl OsdState {
    /// Seeded with the current values, so startup itself never counts as a
    /// "change" and flashes the OSD.
    pub fn new() -> Self {
        Self {
            last_volume: read_volume(),
            last_brightness: read_brightness(),
            last_profile: read_power_profile(),
            hide_at: None,
        }
    }

    /// Reads volume, brightness and power profile once. When several changed
    /// since the last poll, the last one wins, as before.
    pub fn poll(&mut self) -> OsdAction {
        let mut update = None;

        if let Some((percent, muted)) = read_volume() {
            if self.last_volume != Some((percent, muted)) {
                self.last_volume = Some((percent, muted));
                update = Some(OsdUpdate {
                    glyph: if muted { "\u{F026}" } else { "\u{F028}" },
                    label: if muted {
                        "Muted".to_string()
                    } else {
                        format!("Volume {percent}%")
                    },
                    level: Some(percent as f64 / 100.0),
                });
            }
        }

        if let Some(percent) = read_brightness() {
            if self.last_brightness != Some(percent) {
                self.last_brightness = Some(percent);
                update = Some(OsdUpdate {
                    glyph: "\u{F185}",
                    label: format!("Brightness {percent}%"),
                    level: Some(percent as f64 / 100.0),
                });
            }
        }

        if let Some(profile) = read_power_profile() {
            if self.last_profile.as_deref() != Some(profile.as_str()) {
                self.last_profile = Some(profile.clone());
                update = Some(OsdUpdate {
                    glyph: "\u{F013}",
                    label: profile.replace('-', " "),
                    level: None,
                });
            }
        }

        if let Some(update) = update {
            self.hide_at = Some(Instant::now() + VISIBLE_FOR);
            OsdAction::Show(update)
        } else if self
            .hide_at
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.hide_at = None;
            OsdAction::Hide
        } else {
            OsdAction::Nothing
        }
    }
}

pub fn main() -> Result<()> {
    // helium-shell hosts the OSD itself (see src/daemon.rs); a second one
    // would show every change twice.
    if crate::ipc::daemon_running() {
        eprintln!("helium-osd: helium-shell already shows the OSD; nothing to do");
        return Ok(());
    }

    let mut shell = Shell::from_source(include_str!("../../ui/osd.slint"))
        .surface("Osd")
        .size(HIDDEN_SIZE.0, HIDDEN_SIZE.1)
        .anchor(AnchorEdges::empty().with_top().with_right())
        .margin(Margins::new(TOP_MARGIN, SIDE_MARGIN, 0, 0))
        .layer(Layer::Overlay)
        .exclusive_zone(0)
        .keyboard_interactivity(KeyboardInteractivity::None)
        .build()?;

    let osd_surface = shell.control().surface("Osd");
    let event_loop = shell.event_loop_handle();
    let mut state = OsdState::new();

    event_loop.add_timer(POLL_INTERVAL, move |_, app_state| {
        match state.poll() {
            OsdAction::Show(update) => {
                for surface in app_state.surfaces_by_name("Osd") {
                    update.apply(surface.component_instance());
                }
                osd_surface.resize(OSD_WIDTH, OSD_HEIGHT).ok();
            }
            OsdAction::Hide => {
                osd_surface.resize(HIDDEN_SIZE.0, HIDDEN_SIZE.1).ok();
            }
            OsdAction::Nothing => {}
        }
        TimeoutAction::ToDuration(POLL_INTERVAL)
    })?;

    shell.run()?;
    Ok(())
}
