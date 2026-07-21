//! A grid-style wallpaper picker, mirroring quickshell-d77's Wallpaper.qml.
//!
//! Spawn-on-demand like helium-launcher/helium-session (bind it directly to
//! a key, e.g. `bind = SUPER, W, exec, /usr/bin/helium-wallpaper` — there's
//! no bar icon for it, matching quickshell-d77 which only opens it via a
//! keybind too). Scans a local directory for images, shows them in a grid,
//! and applies a click by shelling out to whichever wallpaper backend the
//! running compositor actually has (see `apply_wallpaper()`), the same
//! compositor-detection approach `set-wallpaper.sh` uses there.
//!
//! Unlike the launcher/session menu, clicking a thumbnail does not close the
//! window — the grid stays open so you can keep previewing wallpapers,
//! matching quickshell-d77's own behavior. Only Escape closes it.
//!
//! Built directly on raw `layer_shika::Shell`, not helium-wsl's `Helium`
//! wrapper, for the same reason as helium-launcher: pushing `current_index`
//! back onto the surface from inside a callback needs
//! `ComponentInstance::as_weak()`, which the wrapper's `on_signal` doesn't
//! expose.
//!
//! Run with `--startup` (e.g. from compositor autostart, after the
//! compositor's own wallpaper daemon has started) to silently reapply the
//! last-saved wallpaper without opening the picker UI — mirrors
//! `set-wallpaper.sh startup` in quickshell-d77. Unlike that project's
//! Hyprland-specific `apply-saved-wallpaper.sh` (which rewrites
//! hyprpaper.conf *before* hyprpaper starts so there's no flash of a
//! default background), this always reapplies *after* the wallpaper daemon
//! is already running, so a brief flash of whatever hyprpaper/swww/swaybg
//! shows by default is possible on Hyprland specifically. Not implemented
//! here since it would need to special-case Hyprland's config file, unlike
//! every other backend used below.

use layer_shika::prelude::*;
use layer_shika::slint_interpreter::{ComponentHandle, Value};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::Duration;

struct WallpaperEntry {
    name: String,
    path: String,
}

/// Extensions scanned for, same list as quickshell-d77's Wallpaper.qml.
/// Whether each actually renders depends on the `image` crate features
/// Slint itself was built with — png/jpg/bmp are always available, webp
/// isn't guaranteed; an unsupported file just doesn't render its
/// thumbnail (same "shown with no icon rather than a broken image"
/// fallback helium-launcher's icon resolution already relies on) instead
/// of erroring the whole picker.
const EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "bmp"];

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".to_string()))
}

/// Directory scanned for wallpapers. `$HOME/Wallpaper` by default (same
/// default quickshell-d77 uses), overridable via `HELIUM_WALLPAPER_DIR` for
/// anyone who doesn't want to move/symlink their wallpaper folder to match.
fn wallpaper_dir() -> PathBuf {
    std::env::var("HELIUM_WALLPAPER_DIR").map(PathBuf::from).unwrap_or_else(|_| home_dir().join("Wallpaper"))
}

/// Where the last-applied wallpaper's path is persisted, read back both to
/// highlight the active thumbnail on open and by `--startup` at login.
/// Analogous to quickshell-d77's `~/.cache/quickshell/wallpaper/current`.
fn state_file_path() -> PathBuf {
    home_dir().join(".cache/helium/wallpaper/current")
}

fn read_saved_wallpaper() -> Option<String> {
    let contents = fs::read_to_string(state_file_path()).ok()?;
    let path = contents.trim();
    (!path.is_empty()).then(|| path.to_string())
}

fn persist_wallpaper(path: &str) {
    let state = state_file_path();
    if let Some(parent) = state.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(state, path);
}

fn clear_persisted() {
    let _ = fs::remove_file(state_file_path());
}

fn scan_wallpapers(dir: &Path) -> Vec<WallpaperEntry> {
    let Ok(entries) = fs::read_dir(dir) else { return vec![] };
    let mut out: Vec<WallpaperEntry> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file() {
                return None;
            }
            let ext = path.extension()?.to_str()?.to_lowercase();
            if !EXTENSIONS.contains(&ext.as_str()) {
                return None;
            }
            let name = path.file_name()?.to_str()?.to_string();
            let path_str = path.to_str()?.to_string();
            Some(WallpaperEntry { name, path: path_str })
        })
        .collect();
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

fn command_exists(bin: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {bin}")])
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

enum Compositor {
    Hyprland,
    Sway,
    Generic,
}

/// Same three-way split as `set-wallpaper.sh`'s `detect_compositor()`: niri
/// isn't singled out there either (it has no built-in wallpaper daemon of
/// its own, unlike Hyprland's hyprpaper), so it falls through to the
/// generic swww/swaybg/feh chain below, same as this does.
fn detect_compositor() -> Compositor {
    if std::env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok() {
        Compositor::Hyprland
    } else if std::env::var("SWAYSOCK").is_ok() {
        Compositor::Sway
    } else {
        Compositor::Generic
    }
}

/// Applies `path` as the wallpaper on whichever backend the detected
/// compositor uses, and persists it so it survives across picker
/// invocations and (via `--startup`) reboots. Always targets every
/// monitor rather than trying to resolve the focused one — this project
/// doesn't otherwise track per-monitor state (see the bar's own
/// single-primary-monitor width handling in src/main.rs), so a single
/// shared wallpaper across all outputs keeps that same scope.
///
/// Only persists on success: persisting unconditionally would let
/// `has_wallpaper()` (in helium-backdrop.rs) believe a wallpaper is active
/// and hide the backdrop even when the backend command actually failed,
/// leaving a plain black screen behind it — nothing drawing the
/// wallpaper *and* the backdrop gone.
fn apply_wallpaper(path: &str) {
    apply_wallpaper_inner(path, false);
}

/// `retry_startup`: when true (only from `--startup`, which races
/// `exec-once = hyprpaper` at login), keeps retrying the Hyprland branch
/// for up to ~2s instead of giving up after one preload+retry. Right after
/// login, `hyprpaper`'s own `exec-once` may not have its IPC socket bound
/// yet by the time `--startup` runs — every `hyprctl hyprpaper` call fails
/// instantly in that window, so the old single preload+retry could exhaust
/// itself before the socket ever came up, silently leaving no wallpaper
/// applied. Not applied to the interactive picker's click path, which would
/// otherwise stall the UI for up to 2s on a genuine failure (e.g. hyprpaper
/// not running at all).
fn apply_wallpaper_inner(path: &str, retry_startup: bool) {
    let applied = match detect_compositor() {
        Compositor::Hyprland => {
            // Empty monitor name + comma applies to every monitor (see
            // set-wallpaper.sh's own comment on this hyprctl syntax).
            let arg = format!(",{path}");
            let attempts = if retry_startup { 10 } else { 1 };
            let mut ok = false;
            for attempt in 0..attempts {
                if attempt > 0 {
                    std::thread::sleep(Duration::from_millis(200));
                }
                ok = Command::new("hyprctl")
                    .args(["hyprpaper", "wallpaper", &arg])
                    .status()
                    .is_ok_and(|s| s.success());
                if ok {
                    break;
                }
                // Not preloaded yet (or hyprpaper's socket isn't up yet
                // during the startup race above) — preload and retry.
                let _ = Command::new("hyprctl").args(["hyprpaper", "preload", path]).status();
            }
            if !ok {
                ok = Command::new("hyprctl")
                    .args(["hyprpaper", "wallpaper", &arg])
                    .status()
                    .is_ok_and(|s| s.success());
            }
            ok
        }
        Compositor::Sway => Command::new("swaymsg")
            .args(["output", "*", "bg", path, "fill"])
            .status()
            .is_ok_and(|s| s.success()),
        Compositor::Generic => {
            if command_exists("swww") {
                Command::new("swww").args(["img", path]).status().is_ok_and(|s| s.success())
            } else if command_exists("swaybg") {
                let _ = Command::new("pkill").arg("swaybg").status();
                Command::new("swaybg").args(["-i", path, "-m", "fill"]).spawn().is_ok()
            } else if command_exists("feh") {
                Command::new("feh").args(["--bg-fill", path]).status().is_ok_and(|s| s.success())
            } else {
                false
            }
        }
    };

    if applied {
        persist_wallpaper(path);
    }
}

/// Unloads the active wallpaper and clears the persisted state, so
/// `--startup` won't reapply it on the next login — mirrors
/// `set-wallpaper.sh clear`.
fn clear_wallpaper() {
    clear_persisted();
    match detect_compositor() {
        Compositor::Hyprland => {
            let _ = Command::new("hyprctl").args(["hyprpaper", "unload", "all"]).status();
        }
        Compositor::Sway => {
            let _ = Command::new("swaymsg").args(["output", "*", "bg", "none"]).status();
        }
        Compositor::Generic => {
            if command_exists("swww") {
                let _ = Command::new("swww").arg("clear").status();
            } else {
                let _ = Command::new("pkill").arg("swaybg").status();
            }
        }
    }
}

fn escape_slint_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

const CELL_W: u32 = 160;
const CELL_H: u32 = 110;
const GAP: u32 = 10;
const PADDING: u32 = 14;
const HEADER_HEIGHT: u32 = 40;
const WINDOW_WIDTH: u32 = 720;
const WINDOW_HEIGHT: u32 = 520;

fn columns() -> u32 {
    ((WINDOW_WIDTH - 2 * PADDING + GAP) / (CELL_W + GAP)).max(1)
}

fn build_slint_source(images: &[WallpaperEntry], current_index: i32, dir: &Path) -> String {
    let cols = columns();
    let mut cells = String::new();
    for (i, img) in images.iter().enumerate() {
        let row = i as u32 / cols;
        let col = i as u32 % cols;
        let x = PADDING + col * (CELL_W + GAP);
        let y = PADDING + row * (CELL_H + GAP);
        let _ = writeln!(
            cells,
            r#"        WallpaperCell {{ x: {x}px; y: {y}px; thumb: @image-url("{path}"); label: "{name}"; is_current: root.current_index == {i}; clicked => {{ wallpaper_clicked({i}); }} }}"#,
            path = escape_slint_string(&img.path),
            name = escape_slint_string(&img.name),
        );
    }

    let rows = if images.is_empty() { 1 } else { (images.len() as u32).div_ceil(cols) };
    let content_height = 2 * PADDING + rows * CELL_H + rows.saturating_sub(1) * GAP;
    let empty = images.is_empty();
    let empty_message =
        escape_slint_string(&format!("No images found in\n{}", dir.display()));

    // WallpaperCell is only defined when there's at least one image to
    // instantiate it for — an unused, non-exported component is a fatal
    // diagnostic to this project's Shell::build() (it treats any
    // diagnostic, even Slint's own "component is neither used nor
    // exported" warning, as a build failure), and exporting it instead
    // just trades that for a different fatal diagnostic ("doesn't inherit
    // Window, no code generated"), since it isn't a Window itself.
    let cell_component = if images.is_empty() {
        String::new()
    } else {
        format!(
            r#"
component WallpaperCell inherits Rectangle {{
    in property <image> thumb;
    in property <string> label;
    in property <bool> is_current: false;
    callback clicked;

    width: {CELL_W}px;
    height: {CELL_H}px;
    border-radius: 8px;
    background: #141414;
    border-width: is_current ? 3px : 1px;
    border-color: is_current ? #76b900 : #333333;
    clip: true;

    Image {{
        x: 3px;
        y: 3px;
        width: parent.width - 6px;
        height: parent.height - 6px;
        source: thumb;
        image-fit: cover;
    }}

    Rectangle {{
        x: 3px;
        y: parent.height - 22px;
        width: parent.width - 6px;
        height: 19px;
        background: #000000aa;

        Text {{
            x: 6px;
            width: parent.width - 12px;
            text: label;
            color: #d4d4d4;
            font-size: 10px;
            font-family: "Space Grotesk";
            overflow: elide;
            vertical-alignment: center;
        }}
    }}

    TouchArea {{ clicked => {{ root.clicked(); }} }}
}}
"#
        )
    };

    format!(
        r#"
{cell_component}
export component Wallpaper inherits Window {{
    width: {WINDOW_WIDTH}px;
    height: {WINDOW_HEIGHT}px;
    background: transparent;

    callback wallpaper_clicked(int);
    callback clear_clicked();
    callback close_requested();

    in property <int> current_index: {current_index};

    forward-focus: scope;

    Rectangle {{
        background: #0d0d0d;
        border-radius: 14px;
        clip: true;

        scope := FocusScope {{
            key-pressed(event) => {{
                if (event.text == Key.Escape) {{
                    close_requested();
                    accept
                }} else {{
                    reject
                }}
            }}

            VerticalLayout {{
                HorizontalLayout {{
                    height: {HEADER_HEIGHT}px;
                    padding-left: {PADDING}px;
                    padding-right: {PADDING}px;
                    spacing: 8px;

                    Text {{
                        text: "Wallpapers";
                        color: #d4d4d4;
                        font-size: 15px;
                        font-family: "Space Grotesk";
                        vertical-alignment: center;
                    }}

                    Rectangle {{ horizontal-stretch: 1; }}

                    Text {{
                        text: "{count} wallpapers";
                        color: #999999;
                        font-size: 11px;
                        font-family: "Space Grotesk";
                        vertical-alignment: center;
                    }}

                    Rectangle {{
                        width: 60px;
                        height: 26px;
                        border-radius: 6px;
                        background: clear_ta.has-hover ? #2a1414 : transparent;
                        border-width: 1px;
                        border-color: #f7768e;

                        Text {{
                            text: "Clear";
                            color: #f7768e;
                            font-size: 11px;
                            font-family: "Space Grotesk";
                            horizontal-alignment: center;
                            vertical-alignment: center;
                        }}

                        clear_ta := TouchArea {{ clicked => {{ clear_clicked(); }} }}
                    }}
                }}

                Flickable {{
                    vertical-stretch: 1;
                    viewport-width: {WINDOW_WIDTH}px;
                    viewport-height: {content_height}px;

{cells}
                    Text {{
                        visible: {empty};
                        x: {PADDING}px;
                        y: {PADDING}px;
                        width: {WINDOW_WIDTH}px - {double_padding}px;
                        text: "{empty_message}";
                        color: #999999;
                        font-size: 12px;
                        font-family: "Space Grotesk";
                        horizontal-alignment: center;
                    }}
                }}
            }}
        }}
    }}
}}
"#,
        count = images.len(),
        double_padding = 2 * PADDING,
    )
}

fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--startup") {
        if let Some(path) = read_saved_wallpaper() {
            if Path::new(&path).is_file() {
                apply_wallpaper_inner(&path, true);
            }
        }
        return Ok(());
    }

    let dir = wallpaper_dir();
    let images = Rc::new(scan_wallpapers(&dir));
    let saved = read_saved_wallpaper();
    let initial_index = saved
        .as_deref()
        .and_then(|p| images.iter().position(|w| w.path == p))
        .map_or(-1, |i| i as i32);

    let source = build_slint_source(&images, initial_index, &dir);

    let mut shell = Shell::from_source(source)
        .surface("Wallpaper")
        .size(WINDOW_WIDTH, WINDOW_HEIGHT)
        .anchor(AnchorEdges::empty().with_top())
        .margin(Margins::new(56, 0, 0, 0))
        .layer(Layer::Overlay)
        .keyboard_interactivity(KeyboardInteractivity::Exclusive)
        .build()?;

    shell.with_surface("Wallpaper", |comp| {
        let weak = comp.as_weak();
        let images_c = images.clone();
        comp.set_callback("wallpaper_clicked", move |args| {
            let Some(Value::Number(n)) = args.first() else { return Value::Void };
            let idx = *n as i32;
            if idx >= 0 {
                if let Some(entry) = images_c.get(idx as usize) {
                    apply_wallpaper(&entry.path);
                    if let Some(instance) = weak.upgrade() {
                        instance.set_property("current_index", Value::Number(f64::from(idx))).ok();
                    }
                }
            }
            Value::Void
        }).ok();

        let weak = comp.as_weak();
        comp.set_callback("clear_clicked", move |_| {
            clear_wallpaper();
            if let Some(instance) = weak.upgrade() {
                instance.set_property("current_index", Value::Number(-1.0)).ok();
            }
            Value::Void
        }).ok();

        comp.set_callback("close_requested", move |_| {
            std::process::exit(0);
        }).ok();
    })?;

    shell.run()?;
    Ok(())
}
