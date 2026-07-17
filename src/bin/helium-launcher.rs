//! A minimal, click-only app launcher.
//!
//! There's no search field or arrow-key navigation: Helium/layer-shika's
//! keyboard input is stubbed upstream (`on_key` — "waiting on layer-shika
//! keyboard input API"), so a real text-search launcher isn't possible on
//! top of this framework today. This instead lists every desktop entry as a
//! clickable row, spawn-on-demand (like rofi/wofi), and exits after
//! launching something (or after "Close" is clicked).

use helium_wsl::prelude::*;
use helium_wsl::slint_interpreter;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

struct AppEntry {
    name: String,
    exec: String,
    terminal: bool,
    icon: Option<String>,
}

fn parse_desktop_file(path: &std::path::Path) -> Option<AppEntry> {
    let content = fs::read_to_string(path).ok()?;
    let mut in_main_section = false;
    let mut name = None;
    let mut exec = None;
    let mut icon = None;
    let mut terminal = false;
    let mut no_display = false;
    let mut hidden = false;

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_main_section = line == "[Desktop Entry]";
            continue;
        }
        if !in_main_section {
            continue;
        }
        if let Some(v) = line.strip_prefix("Name=") {
            // Prefer the unlocalized Name= over Name[xx]=, and take the
            // first occurrence.
            if name.is_none() {
                name = Some(v.to_string());
            }
        } else if let Some(v) = line.strip_prefix("Exec=") {
            exec = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("Icon=") {
            icon = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("Terminal=") {
            terminal = v.eq_ignore_ascii_case("true");
        } else if let Some(v) = line.strip_prefix("NoDisplay=") {
            no_display = v.eq_ignore_ascii_case("true");
        } else if let Some(v) = line.strip_prefix("Hidden=") {
            hidden = v.eq_ignore_ascii_case("true");
        }
    }

    if no_display || hidden {
        return None;
    }
    let name = name?;
    let exec = clean_exec(&exec?);
    if exec.is_empty() {
        return None;
    }
    let icon = icon.and_then(|i| resolve_icon(&i));
    Some(AppEntry { name, exec, terminal, icon })
}

/// Resolves a desktop entry's `Icon=` value (usually a bare theme name, e.g.
/// "Alacritty", occasionally an absolute path) to an actual file on disk.
///
/// This is a best-effort search, not a full XDG icon-theme-spec
/// implementation (no theme inheritance, no index.theme parsing) — it just
/// checks every installed theme's common size directories plus pixmaps.
/// Apps whose icon can't be found this way are shown with no icon rather
/// than a broken image.
fn resolve_icon(name: &str) -> Option<String> {
    if name.starts_with('/') {
        return std::path::Path::new(name).is_file().then(|| name.to_string());
    }

    const SIZES: &[&str] = &["scalable", "48x48", "64x64", "128x128", "32x32", "256x256"];
    const EXTS: &[&str] = &["svg", "png"];

    if let Ok(theme_dirs) = fs::read_dir("/usr/share/icons") {
        for theme_dir in theme_dirs.flatten() {
            for size in SIZES {
                for ext in EXTS {
                    let candidate = theme_dir.path().join(size).join("apps").join(format!("{name}.{ext}"));
                    if candidate.is_file() {
                        return candidate.to_str().map(str::to_string);
                    }
                }
            }
        }
    }
    for ext in EXTS {
        let candidate = PathBuf::from("/usr/share/pixmaps").join(format!("{name}.{ext}"));
        if candidate.is_file() {
            return candidate.to_str().map(str::to_string);
        }
    }
    None
}

/// Strips desktop-entry field codes (`%f`, `%F`, `%u`, `%U`, `%i`, `%c`,
/// `%k`) that are meant to be substituted by the caller, not passed through.
fn clean_exec(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            if let Some(&code) = chars.peek() {
                if matches!(code, 'f' | 'F' | 'u' | 'U' | 'i' | 'c' | 'k') {
                    chars.next();
                    continue;
                }
            }
        }
        out.push(c);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn scan_apps() -> Vec<AppEntry> {
    let mut dirs = vec![PathBuf::from("/usr/share/applications")];
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/applications"));
    }

    let mut apps = Vec::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("desktop") {
                if let Some(app) = parse_desktop_file(&path) {
                    apps.push(app);
                }
            }
        }
    }
    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    apps.dedup_by(|a, b| a.name == b.name);
    apps
}

fn detect_terminal() -> String {
    for candidate in ["alacritty", "kitty", "foot", "wezterm", "xterm"] {
        if Command::new("sh")
            .args(["-c", &format!("command -v {candidate}")])
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            return candidate.to_string();
        }
    }
    "xterm".to_string()
}

fn escape_slint_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

const ROW_HEIGHT: u32 = 32;
const CLOSE_ROW_HEIGHT: u32 = 36;
const WINDOW_WIDTH: u32 = 380;
const WINDOW_HEIGHT: u32 = 480;

fn build_slint_source(apps: &[AppEntry]) -> String {
    let mut rows = String::new();
    for (i, app) in apps.iter().enumerate() {
        let icon_prop = match &app.icon {
            // @image-url() only accepts a literal path at each use site, so
            // it's baked directly into the generated instantiation rather
            // than passed as a plain runtime property value.
            Some(path) => format!(r#"icon_img: @image-url("{}"); "#, escape_slint_string(path)),
            None => String::new(),
        };
        let _ = writeln!(
            rows,
            r#"        AppRow {{ {icon_prop}label: "{}"; clicked => {{ app_clicked({i}); }} }}"#,
            escape_slint_string(&app.name)
        );
    }

    format!(
        r#"
component AppRow inherits Rectangle {{
    in property <string> label: "";
    in property <image> icon_img;
    callback clicked;
    height: {ROW_HEIGHT}px;
    background: #141414;

    HorizontalLayout {{
        padding-left: 10px;
        padding-right: 10px;
        spacing: 8px;

        Image {{
            source: icon_img;
            width: 18px;
            height: 18px;
        }}

        Text {{
            text: label;
            color: #d4d4d4;
            font-size: 13px;
            font-family: "Space Grotesk";
            overflow: elide;
            vertical-alignment: center;
            horizontal-alignment: left;
        }}
    }}

    TouchArea {{ clicked => {{ root.clicked(); }} }}
}}

export component Launcher inherits Window {{
    width: {WINDOW_WIDTH}px;
    height: {WINDOW_HEIGHT}px;
    background: transparent;

    callback app_clicked(int);

    Rectangle {{
        background: #0d0d0d;
        border-radius: 14px;
        clip: true;

        VerticalLayout {{
            padding: 8px;
            spacing: 4px;

            Rectangle {{
                height: {CLOSE_ROW_HEIGHT}px;
                background: #1f1f1f;
                border-radius: 6px;

                HorizontalLayout {{
                    alignment: center;
                    spacing: 6px;

                    Text {{
                        text: "\u{{F00D}}";
                        color: #999;
                        font-size: 13px;
                        font-family: "Symbols Nerd Font";
                        vertical-alignment: center;
                    }}

                    Text {{
                        text: "Close";
                        color: #999;
                        font-size: 13px;
                        font-family: "Space Grotesk";
                        vertical-alignment: center;
                    }}
                }}

                TouchArea {{ clicked => {{ app_clicked(-1); }} }}
            }}

            Flickable {{
                vertical-stretch: 1;
                viewport-height: {rows_height}px;

                VerticalLayout {{
                    spacing: 2px;
{rows}
                }}
            }}
        }}
    }}
}}
"#,
        rows_height = apps.len() as u32 * (ROW_HEIGHT + 2),
    )
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let apps = scan_apps();
    let terminal = detect_terminal();
    let source = build_slint_source(&apps);

    let mut shell = Helium::from_source(source)
        .surface("Launcher")
        .size(WINDOW_WIDTH, WINDOW_HEIGHT)
        .anchor((AnchorEdge::Top,))
        .margin(56, 0, 0, 0)
        .layer(Layer::Overlay)
        .build()?;

    shell.on_signal("Launcher", "app_clicked", move |args| {
        let Some(slint_interpreter::Value::Number(n)) = args.first() else { return };
        let idx = *n as i32;
        if idx < 0 {
            std::process::exit(0);
        }
        if let Some(app) = apps.get(idx as usize) {
            let spawn_result = if app.terminal {
                Command::new(&terminal).arg("-e").arg("sh").arg("-c").arg(&app.exec).spawn()
            } else {
                Command::new("sh").arg("-c").arg(&app.exec).spawn()
            };
            let _ = spawn_result;
        }
        std::process::exit(0);
    });

    shell.run()?;
    Ok(())
}
