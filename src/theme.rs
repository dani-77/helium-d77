//! Matugen colors, shared with the other d77 shells (Utumno, quickshell-d77,
//! fabric-d77, Frost).
//!
//! Every helium UI reads its colors from the Slint `Theme` global
//! (ui/theme.slint), whose defaults are helium's own palette. When a wallpaper
//! is chosen, Matugen writes a palette for it to `~/.cache/d77/palette.json`;
//! `~/.config/d77/theme-mix` blends it over helium's colors, from 0.0 (only
//! helium's) to 1.0 (only the wallpaper's). A palette made for a different
//! wallpaper than helium's current one is ignored, as in Utumno.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::SystemTime;

use helium_wsl::slint::{Brush, Color};
use helium_wsl::slint_interpreter::Value;

/// The `Theme` global's source, prepended to every helium UI.
pub const SLINT: &str = include_str!("../ui/theme.slint");

/// The global's name in ui/theme.slint.
pub const GLOBAL: &str = "Theme";

/// Each `Theme` property, the palette.json key it follows (the same roles
/// Frost uses), and helium's own color for it.
const ROLES: [(&str, &str, u32); 9] = [
    ("bg", "bg", 0x14_14_14),
    ("bg_dark", "bgDark", 0x0d_0d_0d),
    ("bg_high", "bgHighlight", 0x1f_1f_1f),
    ("line", "muted", 0x33_33_33),
    ("fg", "fg", 0xd4_d4_d4),
    ("fg_dim", "fgDark", 0x99_99_99),
    ("fg_muted", "comment", 0x66_66_66),
    ("accent", "blue", 0x76_b9_00),
    ("danger", "red", 0xd6_45_45),
];

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// The shared palette. Always under `~/.cache`, as the other shells read it.
fn palette_path() -> PathBuf {
    home().join(".cache/d77/palette.json")
}

fn mix_path() -> PathBuf {
    home().join(".config/d77/theme-mix")
}

/// The wallpaper helium's picker last applied (see src/apps/wallpaper.rs).
pub fn wallpaper_state_path() -> PathBuf {
    home().join(".cache/helium/wallpaper/current")
}

fn rgb(value: u32) -> Color {
    Color::from_rgb_u8((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

fn parse_hex(hex: &str) -> Option<Color> {
    let digits = hex.strip_prefix('#')?;
    (digits.len() == 6)
        .then(|| u32::from_str_radix(digits, 16).ok())
        .flatten()
        .map(rgb)
}

fn lerp(from: Color, to: Color, t: f32) -> Color {
    let channel = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
    Color::from_rgb_u8(
        channel(from.red(), to.red()),
        channel(from.green(), to.green()),
        channel(from.blue(), to.blue()),
    )
}

/// The current `Theme` values: helium's colors, blended toward the wallpaper
/// palette when there's one for the current wallpaper.
pub fn values() -> Vec<(&'static str, Value)> {
    let palette: Option<serde_json::Value> = std::fs::read_to_string(palette_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    let mix = std::fs::read_to_string(mix_path())
        .ok()
        .and_then(|text| text.trim().parse::<f32>().ok())
        .unwrap_or(0.0)
        .clamp(0.0, 1.0);
    // Without a wallpaper of its own (chosen from another shell), helium takes
    // the palette as it is.
    let current = std::fs::read_to_string(wallpaper_state_path())
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|path| !path.is_empty());
    let palette = palette.filter(
        |p| match (&current, p.get("source").and_then(|s| s.as_str())) {
            (Some(current), Some(source)) => current == source,
            (None, _) => true,
            (Some(_), None) => false,
        },
    );

    ROLES
        .iter()
        .map(|&(prop, key, own)| {
            let base = rgb(own);
            let color = palette
                .as_ref()
                .filter(|_| mix > 0.0)
                .and_then(|p| p.get(key)?.as_str().and_then(parse_hex))
                .map_or(base, |wall| lerp(base, wall, mix));
            (prop, Value::Brush(Brush::SolidColor(color)))
        })
        .collect()
}

/// Applies the current colors to one component instance (the standalone
/// applets, built on raw layer-shika).
pub fn apply(instance: &helium_wsl::slint_interpreter::ComponentInstance) {
    for (prop, value) in values() {
        let _ = instance.set_global_property(GLOBAL, prop, value);
    }
}

/// `ui/theme.slint` followed by `source`.
pub fn with_theme(source: &str) -> String {
    format!("{SLINT}\n{source}")
}

/// Changes whenever the palette, the mix or helium's wallpaper does: their
/// modification times.
#[derive(PartialEq, Eq, Clone, Default)]
pub struct Stamp([Option<SystemTime>; 3]);

pub fn stamp() -> Stamp {
    let modified = |path: PathBuf| std::fs::metadata(path).and_then(|m| m.modified()).ok();
    Stamp([
        modified(palette_path()),
        modified(mix_path()),
        modified(wallpaper_state_path()),
    ])
}

/// Matugen config and template, the same Utumno ships in theme/, embedded so
/// a system-wide install needs no separate files.
const MATUGEN_CONFIG: &str = r##"[config]

[config.custom_colors]
green = "#9ece6a"
yellow = "#e0af68"
orange = "#ff9e64"

[templates.d77]
input_path = "./d77-palette.json"
output_path = "~/.cache/d77/palette.json.tmp"
"##;

const MATUGEN_TEMPLATE: &str = r##"{
  "version": 1,
  "source": "{{image}}",
  "bg": "{{colors.surface.default.hex}}",
  "bgDark": "{{colors.surface_container_lowest.default.hex}}",
  "bgHighlight": "{{colors.surface_container_high.default.hex}}",
  "fg": "{{colors.on_surface.default.hex}}",
  "fgDark": "{{colors.on_surface_variant.default.hex}}",
  "comment": "{{colors.outline.default.hex}}",
  "muted": "{{colors.outline_variant.default.hex}}",
  "blue": "{{colors.primary.default.hex}}",
  "cyan": "{{colors.secondary.default.hex}}",
  "magenta": "{{colors.tertiary.default.hex}}",
  "cyanAccent": "{{colors.secondary_fixed_dim.default.hex}}",
  "red": "{{colors.error.default.hex}}",
  "green": "{{colors.green.default.hex}}",
  "yellow": "{{colors.yellow.default.hex}}",
  "orange": "{{colors.orange.default.hex}}"
}
"##;

/// Makes sure the shared palette is the one for `wallpaper`, generating it
/// when it was made for another one (or doesn't exist).
pub fn ensure_palette(wallpaper: &str) {
    let source = std::fs::read_to_string(palette_path())
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|p| p.get("source")?.as_str().map(str::to_string));
    if source.as_deref() != Some(wallpaper) {
        generate_palette(wallpaper);
    }
}

/// Generates the shared palette for `wallpaper` with Matugen, in the
/// background: written to a .tmp file and renamed, so no shell reads a
/// half-written palette. Does nothing without matugen.
pub fn generate_palette(wallpaper: &str) {
    let dir = home().join(".cache/helium-d77/matugen");
    let config = dir.join("matugen.toml");
    if std::fs::create_dir_all(&dir).is_err()
        || std::fs::create_dir_all(home().join(".cache/d77")).is_err()
        || std::fs::write(&config, MATUGEN_CONFIG).is_err()
        || std::fs::write(dir.join("d77-palette.json"), MATUGEN_TEMPLATE).is_err()
    {
        return;
    }
    let palette = palette_path();
    let tmp = palette.with_extension("json.tmp");
    // --source-color-index 0: without a TTY, matugen 4.x asks which source
    // color to use and aborts (see Utumno's set-wallpaper.sh).
    let script = r#"command -v matugen >/dev/null 2>&1 || exit 0
matugen -c "$1" image "$2" --source-color-index 0 -t scheme-vibrant -m dark -q >/dev/null 2>&1 &&
    mv -f "$3" "$4""#;
    if let Ok(mut child) = Command::new("sh")
        .arg("-c")
        .arg(script)
        .arg("sh")
        .arg(&config)
        .arg(wallpaper)
        .arg(&tmp)
        .arg(&palette)
        .stdin(Stdio::null())
        .spawn()
    {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}
