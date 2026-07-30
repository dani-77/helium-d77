//! Chat popup for a locally running Ollama daemon (http://127.0.0.1:11434),
//! ported from quickshell-d77/utumno's OllamaChat.qml. Talks to Ollama via
//! `curl` (no HTTP client crate needed, same choice `weather.rs` already
//! makes for wttr.in), streams the generated response as it arrives, lets
//! you switch between installed models or pull a new one with live
//! progress, and remembers the last picked model at
//! `~/.config/ollama-chat/model.conf` — the exact path quickshell-d77/utumno
//! use, so the choice carries over between whichever of these shells you
//! run.
//!
//! Spawn-on-demand like helium-launcher/helium-session/helium-wallpaper
//! (opened from the bar's "AI" icon, or bind it directly to a key). Built
//! directly on raw `layer_shika::Shell`, not helium-wsl's `Helium` wrapper,
//! for the same reason as those three: pushing properties back onto the
//! surface from inside a callback needs `ComponentInstance::as_weak()` /
//! `EventLoopHandle::add_channel`, neither of which the wrapper's
//! `on_signal` exposes.
//!
//! Unlike every other binary in this project, two of the network calls here
//! (pulling a model, generating a response) can run for tens of seconds to
//! several minutes — blocking the event loop for that long, the way
//! `weather::status()`'s bounded `--max-time` calls already do on a timer,
//! would freeze the whole window. Both are run on their own `std::thread`
//! instead, reporting progress back to the main thread through
//! `EventLoopHandle::add_channel`'s calloop channel (a real cross-thread
//! wakeup, not a polled queue). The bounded (`--max-time 5`) status/model
//! list polls stay direct blocking calls on their own timers, same
//! trade-off `weather.rs` already makes for the bar.
//!
//! Model selection on open: picked once at startup (saved model, if it's
//! still installed; else the fallback, if installed; else whatever's
//! first) and never recomputed afterward. The 15s model-list refresh only
//! ever updates the dropdown's contents, not `current_model` — recomputing
//! that choice on every refresh would silently revert a manual dropdown
//! pick back to the saved/fallback model a few seconds after making it.

use layer_shika::calloop::TimeoutAction;
use layer_shika::calloop::channel::Sender;
use layer_shika::prelude::*;
use layer_shika::slint::{ModelRc, VecModel};
use layer_shika::slint_interpreter::{ComponentHandle, Value};
use serde_json::Value as Json;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const WINDOW_WIDTH: u32 = 560;
const WINDOW_HEIGHT: u32 = 620;
const FALLBACK_MODEL: &str = "qwen2.5:0.5b";
const OLLAMA_BASE: &str = "http://127.0.0.1:11434";
const STATUS_POLL_INTERVAL: Duration = Duration::from_secs(5);
const MODELS_POLL_INTERVAL: Duration = Duration::from_secs(15);
const ROW_HEIGHT: u32 = 28;
/// Dropdown rows shown before it scrolls instead of growing further: the
/// model list plus the always-present "+ instalar novo modelo..." row.
const MAX_VISIBLE_ROWS: u32 = 6;

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".to_string()))
}

/// Same path quickshell-d77/utumno's OllamaChat.qml read/write, so the
/// picked model is shared across whichever of these shells is running.
fn config_path() -> PathBuf {
    home_dir().join(".config/ollama-chat/model.conf")
}

fn read_saved_model() -> Option<String> {
    let text = fs::read_to_string(config_path()).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Written directly via the filesystem API rather than shelling out through
/// `sh -c` with the model name interpolated into the command string (the
/// approach quickshell-d77/utumno's QML has to use, since QML's `Process`
/// only runs shell commands) — no quoting/escaping needed here at all.
fn save_model(name: &str) {
    let path = config_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, name);
}

/// Same check quickshell-d77/utumno use: this family of projects targets a
/// runit-supervised Ollama install (Void Linux). Adjust here if yours is
/// managed differently (e.g. `systemctl is-active ollama`).
fn ollama_running() -> bool {
    Command::new("sv")
        .args(["status", "ollama"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim_start().starts_with("run:"))
        .unwrap_or(false)
}

fn fetch_models() -> Option<Vec<String>> {
    let output = Command::new("curl")
        .args(["-s", "--max-time", "5", &format!("{OLLAMA_BASE}/api/tags")])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let parsed: Json = serde_json::from_slice(&output.stdout).ok()?;
    let models = parsed.get("models")?.as_array()?;
    Some(models.iter().filter_map(|m| m.get("name")?.as_str().map(str::to_string)).collect())
}

/// Picks a model from a freshly (re)loaded install list, same precedence
/// quickshell-d77/utumno use: the saved model if it's still installed, else
/// the fallback if that's installed, else whatever's first.
fn pick_model(names: &[String], saved: Option<&str>) -> Option<String> {
    saved
        .filter(|s| names.iter().any(|n| n == s))
        .map(str::to_string)
        .or_else(|| names.iter().find(|n| n.as_str() == FALLBACK_MODEL).cloned())
        .or_else(|| names.first().cloned())
}

fn models_value(names: &[String]) -> Value {
    let items: Vec<Value> = names.iter().map(|n| Value::String(n.as_str().into())).collect();
    Value::Model(ModelRc::new(VecModel::from(items)))
}

/// Visible height of the dropdown (in logical px), capped at
/// `MAX_VISIBLE_ROWS` rows — the Flickable inside it scrolls for the rest.
fn dropdown_height_px(model_count: usize) -> f64 {
    let rows = (model_count as u32 + 1).min(MAX_VISIBLE_ROWS);
    f64::from(rows * ROW_HEIGHT + 8)
}

enum OllamaEvent {
    PullProgress(String),
    PullFinished { message: String, ok: bool },
    GenerateToken(String),
}

/// Streams `POST /api/pull`, reporting each progress line back through
/// `tx`. Runs on its own thread: a model pull can take anywhere from
/// seconds to several minutes, far too long to block the event loop for.
fn spawn_pull(tx: Sender<OllamaEvent>, model: String) {
    thread::spawn(move || {
        tx.send(OllamaEvent::PullProgress(format!("A instalar '{model}'..."))).ok();

        let body = serde_json::json!({ "name": model, "stream": true }).to_string();
        let child = Command::new("curl")
            .args(["-s", "-N", "-X", "POST", &format!("{OLLAMA_BASE}/api/pull"), "-d", &body])
            .stdout(Stdio::piped())
            .spawn();

        let mut child = match child {
            Ok(c) => c,
            Err(_) => {
                tx.send(OllamaEvent::PullFinished {
                    message: format!("Falha ao instalar '{model}': curl não encontrado"),
                    ok: false,
                })
                .ok();
                return;
            }
        };

        if let Some(stdout) = child.stdout.take() {
            for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
                let Ok(chunk) = serde_json::from_str::<Json>(&line) else { continue };
                if let Some(err) = chunk.get("error").and_then(|v| v.as_str()) {
                    tx.send(OllamaEvent::PullProgress(format!("Erro a instalar '{model}': {err}"))).ok();
                    continue;
                }
                let status = chunk.get("status").and_then(|v| v.as_str()).unwrap_or("");
                let total = chunk.get("total").and_then(Json::as_u64);
                let completed = chunk.get("completed").and_then(Json::as_u64);
                let text = match (total, completed) {
                    (Some(t), Some(c)) if t > 0 => {
                        format!("A instalar '{model}': {status} ({}%)", c * 100 / t)
                    }
                    _ => format!("A instalar '{model}': {status}"),
                };
                tx.send(OllamaEvent::PullProgress(text)).ok();
            }
        }

        let ok = child.wait().is_ok_and(|s| s.success());
        let message = if ok {
            format!("'{model}' instalado com sucesso.")
        } else {
            format!("Falha ao instalar '{model}'.")
        };
        tx.send(OllamaEvent::PullFinished { message, ok }).ok();
    });
}

/// Streams `POST /api/generate` for `prompt` against `model`, sending each
/// response fragment back through `tx` as it arrives. Runs on its own
/// thread (see the module doc comment for why): the request is capped at
/// 30s via `--max-time`, same as quickshell-d77/utumno, but that's still
/// long enough to visibly freeze the window if done on the event-loop
/// thread directly.
fn spawn_generate(tx: Sender<OllamaEvent>, model: String, prompt: String) {
    thread::spawn(move || {
        let body = serde_json::json!({ "model": model, "prompt": prompt, "stream": true }).to_string();
        let child = Command::new("curl")
            .args(["-s", "-N", "--max-time", "30", &format!("{OLLAMA_BASE}/api/generate"), "-d", &body])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        let mut child = match child {
            Ok(c) => c,
            Err(_) => {
                tx.send(OllamaEvent::GenerateToken(
                    "\n[sem resposta do Ollama — curl não encontrado]\n".to_string(),
                ))
                .ok();
                return;
            }
        };

        // Read on its own thread so a slow/blocked stderr pipe can never
        // stall the stdout streaming loop below.
        let stderr_handle = child.stderr.take().map(|stderr| {
            thread::spawn(move || {
                let mut buf = String::new();
                let _ = BufReader::new(stderr).read_to_string(&mut buf);
                buf
            })
        });

        let mut got_any = false;
        if let Some(stdout) = child.stdout.take() {
            for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
                let Ok(chunk) = serde_json::from_str::<Json>(&line) else { continue };
                got_any = true;
                if let Some(err) = chunk.get("error").and_then(|v| v.as_str()) {
                    tx.send(OllamaEvent::GenerateToken(format!("\n[erro do modelo: {err}]\n"))).ok();
                } else if let Some(resp) = chunk.get("response").and_then(|v| v.as_str()) {
                    if !resp.is_empty() {
                        tx.send(OllamaEvent::GenerateToken(resp.to_string())).ok();
                    }
                }
            }
        }

        let status = child.wait();
        let err_buf = stderr_handle.and_then(|h| h.join().ok()).unwrap_or_default();
        let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);

        // Mirrors quickshell-d77/utumno's three failure messages exactly.
        if code != 0 || !got_any {
            let msg = if err_buf.contains("Connection refused") || code == 7 {
                Some("\n[Ollama não está a correr. Verifica com: sv status ollama]\n".to_string())
            } else if code == 28 {
                Some("\n[Ollama demorou demasiado a responder — timeout]\n".to_string())
            } else if !got_any {
                Some(format!("\n[sem resposta do Ollama — código curl: {code}]\n"))
            } else {
                None
            };
            if let Some(msg) = msg {
                tx.send(OllamaEvent::GenerateToken(msg)).ok();
            }
        }
    });
}

fn main() -> Result<()> {
    let saved_model = read_saved_model();
    let initial_up = ollama_running();
    let initial_models = fetch_models().unwrap_or_default();
    let initial_model = pick_model(&initial_models, saved_model.as_deref())
        .unwrap_or_else(|| FALLBACK_MODEL.to_string());

    let mut shell = Shell::from_source(include_str!("../../ui/ollama.slint"))
        .surface("Ollama")
        .size(WINDOW_WIDTH, WINDOW_HEIGHT)
        .anchor(AnchorEdges::empty().with_top())
        .margin(Margins::new(56, 0, 0, 0))
        .layer(Layer::Overlay)
        .keyboard_interactivity(KeyboardInteractivity::Exclusive)
        .build()?;

    let event_loop = shell.event_loop_handle();
    let (_channel_token, tx) = event_loop.add_channel::<OllamaEvent, _>(move |event, app_state| {
        for surface in app_state.surfaces_by_name("Ollama") {
            let instance = surface.component_instance();
            match &event {
                OllamaEvent::PullProgress(text) => {
                    instance.set_property("info_text", Value::String(text.as_str().into())).ok();
                }
                OllamaEvent::PullFinished { message, ok } => {
                    instance.set_property("info_text", Value::String(message.as_str().into())).ok();
                    // A newly-installed model should show up in the
                    // dropdown without waiting for the next 15s tick.
                    if *ok {
                        if let Some(names) = fetch_models() {
                            instance.set_property("model_names", models_value(&names)).ok();
                            instance.set_property("model_count", Value::Number(names.len() as f64)).ok();
                            instance
                                .set_property("dropdown_height", Value::Number(dropdown_height_px(names.len())))
                                .ok();
                        }
                    }
                }
                OllamaEvent::GenerateToken(text) => {
                    let history = match instance.get_property("history_text") {
                        Ok(Value::String(s)) => s.to_string(),
                        _ => String::new(),
                    };
                    instance.set_property("history_text", Value::String((history + text).into())).ok();
                }
            }
        }
    })?;

    let initial_model_count = initial_models.len();
    shell.with_surface("Ollama", |comp| {
        comp.set_property("ollama_up", Value::Bool(initial_up)).ok();
        comp.set_property("current_model", Value::String(initial_model.as_str().into())).ok();
        comp.set_property("model_names", models_value(&initial_models)).ok();
        comp.set_property("model_count", Value::Number(initial_model_count as f64)).ok();
        comp.set_property("dropdown_height", Value::Number(dropdown_height_px(initial_model_count))).ok();

        let weak = comp.as_weak();
        comp.set_callback("model_selected", move |args| {
            let Some(Value::String(name)) = args.first() else { return Value::Void };
            let name = name.to_string();
            save_model(&name);
            if let Some(instance) = weak.upgrade() {
                instance.set_property("current_model", Value::String(name.into())).ok();
                instance.set_property("info_text", Value::String(String::new().into())).ok();
            }
            Value::Void
        }).ok();

        let tx_install = tx.clone();
        comp.set_callback("install_submitted", move |args| {
            let Some(Value::String(name)) = args.first() else { return Value::Void };
            let name = name.trim().to_string();
            if !name.is_empty() {
                spawn_pull(tx_install.clone(), name);
            }
            Value::Void
        }).ok();

        let weak = comp.as_weak();
        let tx_prompt = tx.clone();
        comp.set_callback("prompt_submitted", move |args| {
            let Some(Value::String(prompt)) = args.first() else { return Value::Void };
            let prompt = prompt.to_string();
            let Some(instance) = weak.upgrade() else { return Value::Void };
            let model = match instance.get_property("current_model") {
                Ok(Value::String(s)) => s.to_string(),
                _ => FALLBACK_MODEL.to_string(),
            };
            let history = match instance.get_property("history_text") {
                Ok(Value::String(s)) => s.to_string(),
                _ => String::new(),
            };
            instance
                .set_property("history_text", Value::String(format!("{history}\n> {prompt}\n").into()))
                .ok();
            spawn_generate(tx_prompt.clone(), model, prompt);
            Value::Void
        }).ok();

        comp.set_callback("close_requested", move |_| {
            std::process::exit(0);
        }).ok();
    })?;

    event_loop.add_timer(STATUS_POLL_INTERVAL, move |_, app_state| {
        let up = ollama_running();
        for surface in app_state.surfaces_by_name("Ollama") {
            surface.component_instance().set_property("ollama_up", Value::Bool(up)).ok();
        }
        TimeoutAction::ToDuration(STATUS_POLL_INTERVAL)
    })?;

    // Only ever refreshes the dropdown's contents — see the module doc
    // comment for why `current_model` is deliberately never touched here.
    event_loop.add_timer(MODELS_POLL_INTERVAL, move |_, app_state| {
        if let Some(names) = fetch_models() {
            for surface in app_state.surfaces_by_name("Ollama") {
                let instance = surface.component_instance();
                instance.set_property("model_names", models_value(&names)).ok();
                instance.set_property("model_count", Value::Number(names.len() as f64)).ok();
                instance.set_property("dropdown_height", Value::Number(dropdown_height_px(names.len()))).ok();
            }
        }
        TimeoutAction::ToDuration(MODELS_POLL_INTERVAL)
    })?;

    shell.run()?;
    Ok(())
}
