//! A rofi/wofi-style app launcher: search box with live filtering plus
//! Up/Down/Enter/Escape navigation, on top of a click-to-launch row list.
//!
//! This used to be click-only, on the assumption that layer-shika had no
//! real keyboard input. That assumption was wrong: it conflated
//! helium-wsl's `Helium::on_key` (a stubbed convenience callback for
//! global shortcuts) with Slint's own `TextInput`/`FocusScope`, which
//! already receive real `wl_keyboard` events forwarded as
//! `WindowEvent::KeyPressed` by `layer-shika-adapters` — the exact
//! mechanism ui/lock.slint's password field has relied on all along. Any
//! layer-shell surface with keyboard focus gets this; it isn't
//! session-lock-specific.
//!
//! Built directly on raw `layer_shika::Shell`, not helium-wsl's `Helium`
//! wrapper: filtering/selection state must be pushed back onto the Slint
//! component from inside its own callbacks (`query_changed`, `navigate`),
//! which needs a `ComponentInstance::as_weak()` captured into
//! `set_callback`'s closure. The wrapper's `on_signal` only ever hands the
//! callback its arguments, with no way to set properties back on the same
//! surface from inside it.

use layer_shika::prelude::*;
use layer_shika::slint::{ModelRc, VecModel};
use layer_shika::slint_interpreter::{ComponentHandle, ComponentInstance, Value};
use std::cell::RefCell;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::rc::Rc;

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

fn launch_app(app: &AppEntry, terminal: &str) {
    let spawn_result = if app.terminal {
        Command::new(terminal).arg("-e").arg("sh").arg("-c").arg(&app.exec).spawn()
    } else {
        Command::new("sh").arg("-c").arg(&app.exec).spawn()
    };
    let _ = spawn_result;
}

/// Indices of `apps` whose name loosely (case-insensitive substring)
/// matches `query`, in display order. Empty query matches everything.
fn visible_indices(apps: &[AppEntry], query: &str) -> Vec<usize> {
    let query = query.to_lowercase();
    apps.iter()
        .enumerate()
        .filter(|(_, app)| query.is_empty() || app.name.to_lowercase().contains(&query))
        .map(|(i, _)| i)
        .collect()
}

fn push_filter_state(instance: &ComponentInstance, apps: &[AppEntry], query: &str, selected: i32) {
    let visible = visible_indices(apps, query);
    let flags: Vec<Value> = (0..apps.len()).map(|i| Value::Bool(visible.contains(&i))).collect();
    instance.set_property("row_visible", Value::Model(ModelRc::new(VecModel::from(flags)))).ok();
    instance.set_property("selected_index", Value::Number(f64::from(selected))).ok();
}

const ROW_HEIGHT: u32 = 32;
const SEARCH_HEIGHT: u32 = 36;
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
            r#"        AppRow {{ {icon_prop}label: "{}"; row_visible: root.row_visible[{i}]; is_selected: root.selected_index == {i}; clicked => {{ app_clicked({i}); }} }}"#,
            escape_slint_string(&app.name)
        );
    }

    let init_visible = vec!["true"; apps.len()].join(", ");
    let init_selected: i32 = if apps.is_empty() { -1 } else { 0 };

    format!(
        r#"
component AppRow inherits Rectangle {{
    in property <string> label: "";
    in property <image> icon_img;
    in property <bool> row_visible: true;
    in property <bool> is_selected: false;
    callback clicked;
    height: row_visible ? {ROW_HEIGHT}px : 0px;
    background: is_selected ? #1f3319 : #141414;
    border-radius: 6px;
    border-width: is_selected ? 1px : 0px;
    border-color: #76b900;
    clip: true;

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
    callback query_changed(string);
    callback navigate(int);

    in property <[bool]> row_visible: [{init_visible}];
    in property <int> selected_index: {init_selected};

    forward-focus: search_input;

    Rectangle {{
        background: #0d0d0d;
        border-radius: 14px;
        clip: true;

        VerticalLayout {{
            padding: 8px;
            spacing: 4px;

            Rectangle {{
                height: {SEARCH_HEIGHT}px;
                background: #1f1f1f;
                border-radius: 6px;
                clip: true;

                HorizontalLayout {{
                    padding-left: 10px;
                    padding-right: 10px;
                    spacing: 6px;

                    Text {{
                        text: "\u{{F002}}";
                        color: #76b900;
                        font-size: 13px;
                        font-family: "Symbols Nerd Font";
                        vertical-alignment: center;
                    }}

                    search_input := TextInput {{
                        color: #d4d4d4;
                        font-size: 13px;
                        font-family: "Space Grotesk";
                        vertical-alignment: center;
                        single-line: true;

                        edited => {{ query_changed(self.text); }}
                        accepted => {{ app_clicked(root.selected_index); }}
                        key-pressed(event) => {{
                            if (event.text == Key.DownArrow) {{
                                navigate(1);
                                accept
                            }} else if (event.text == Key.UpArrow) {{
                                navigate(-1);
                                accept
                            }} else if (event.text == Key.Return || event.text == "\u{{000d}}") {{
                                // layer-shika-adapters hands us xkbcommon's raw UTF-8 for
                                // Return ("\r", 0x0D) instead of routing it through its
                                // named-key table, so it never matches Slint's own
                                // `Key.Return` (0x0A) and TextInput's built-in `accepted`
                                // callback above never fires. Handle it here directly.
                                app_clicked(root.selected_index);
                                accept
                            }} else if (event.text == Key.Escape) {{
                                app_clicked(-1);
                                accept
                            }} else {{
                                reject
                            }}
                        }}
                    }}
                }}
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

fn main() -> Result<()> {
    let apps = Rc::new(scan_apps());
    let terminal = Rc::new(detect_terminal());
    let source = build_slint_source(&apps);

    let mut shell = Shell::from_source(source)
        .surface("Launcher")
        .size(WINDOW_WIDTH, WINDOW_HEIGHT)
        .anchor(AnchorEdges::empty().with_top())
        .margin(Margins::new(56, 0, 0, 0))
        .layer(Layer::Overlay)
        .keyboard_interactivity(KeyboardInteractivity::Exclusive)
        .build()?;

    // Mirrors the Slint-side `row_visible`/`selected_index` properties so
    // `navigate` can compute the next selected row without a round trip
    // through the interpreter to read them back.
    let query_state = Rc::new(RefCell::new(String::new()));
    let selected_state = Rc::new(RefCell::new(if apps.is_empty() { -1i32 } else { 0i32 }));

    shell.with_surface("Launcher", |comp| {
        let weak = comp.as_weak();
        let (apps_q, query_q, selected_q) = (apps.clone(), query_state.clone(), selected_state.clone());
        comp.set_callback("query_changed", move |args| {
            let Some(Value::String(query)) = args.first() else { return Value::Void };
            let query = query.to_string();
            let visible = visible_indices(&apps_q, &query);
            let selected = visible.first().map(|&i| i as i32).unwrap_or(-1);
            *query_q.borrow_mut() = query.clone();
            *selected_q.borrow_mut() = selected;
            if let Some(instance) = weak.upgrade() {
                push_filter_state(&instance, &apps_q, &query, selected);
            }
            Value::Void
        }).ok();

        let weak = comp.as_weak();
        let (apps_n, query_n, selected_n) = (apps.clone(), query_state.clone(), selected_state.clone());
        comp.set_callback("navigate", move |args| {
            let Some(Value::Number(delta)) = args.first() else { return Value::Void };
            let delta = *delta as i32;
            let visible = visible_indices(&apps_n, &query_n.borrow());
            if !visible.is_empty() {
                let current = *selected_n.borrow();
                let pos = visible.iter().position(|&i| i as i32 == current).unwrap_or(0) as i32;
                let len = visible.len() as i32;
                let new_pos = ((pos + delta) % len + len) % len;
                let new_selected = visible[new_pos as usize] as i32;
                *selected_n.borrow_mut() = new_selected;
                if let Some(instance) = weak.upgrade() {
                    instance.set_property("selected_index", Value::Number(f64::from(new_selected))).ok();
                }
            }
            Value::Void
        }).ok();

        let (apps_c, terminal_c) = (apps.clone(), terminal.clone());
        comp.set_callback("app_clicked", move |args| {
            let Some(Value::Number(n)) = args.first() else { return Value::Void };
            let idx = *n as i32;
            if idx >= 0 {
                if let Some(app) = apps_c.get(idx as usize) {
                    launch_app(app, &terminal_c);
                }
            }
            std::process::exit(0);
        }).ok();
    })?;

    shell.run()?;
    Ok(())
}
