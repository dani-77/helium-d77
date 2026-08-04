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
//!
//! First run (nothing installed yet): if Ollama's up and its registry is
//! reachable, the fallback model is pulled automatically instead of
//! leaving the dropdown empty — shown as a distinct, dismissible banner
//! (`auto_pull_active`/`auto_pull_cancel` in `ui/ollama.slint`) rather than
//! happening silently, and cancellable mid-download via `cancel_pull`.

use layer_shika::calloop::TimeoutAction;
use layer_shika::calloop::channel::Sender;
use layer_shika::prelude::*;
use layer_shika::slint::{ModelRc, VecModel};
use layer_shika::slint_interpreter::{ComponentHandle, Value};
use serde_json::Value as Json;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
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
/// model list plus the always-present "+ install new model..." row.
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

/// Coarse GPU classification used to phrase the model-size hint shown in
/// the install panel. Computed once at startup — the machine's GPU doesn't
/// change mid-session, so there's no need to re-probe it on a timer the
/// way `ollama_running`/`fetch_models` are.
enum GpuTier {
    Nvidia { vram_mb: u64 },
    OtherDedicated,
    None,
}

/// Reads total VRAM from `nvidia-smi` (present on any machine with the
/// NVIDIA driver installed) rather than linking against CUDA/NVML — same
/// shell-out style already used here for `curl`/`sv`. Multi-GPU systems
/// report one line per card; the first is treated as the primary one.
fn nvidia_vram_mb() -> Option<u64> {
    let output = Command::new("nvidia-smi")
        .args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).lines().next()?.trim().parse().ok()
}

/// There's no portable way to read AMD/Intel VRAM without vendor-specific
/// tools that aren't always installed (`rocm-smi`, `intel_gpu_top`), so
/// this only detects the *presence* of a dedicated card via `lspci`,
/// filtering out the display-controller names integrated chipsets
/// commonly report themselves as.
fn other_dedicated_gpu_present() -> bool {
    let Ok(output) = Command::new("lspci").output() else { return false };
    if !output.status.success() {
        return false;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| l.contains("VGA compatible controller") || l.contains("3D controller"))
        .any(|l| {
            let l = l.to_lowercase();
            let integrated = l.contains("uhd graphics")
                || l.contains("hd graphics")
                || l.contains("iris")
                || (l.contains("radeon") && l.contains("graphics") && !l.contains("rx"));
            !integrated
        })
}

fn detect_gpu_tier() -> GpuTier {
    if let Some(vram_mb) = nvidia_vram_mb() {
        return GpuTier::Nvidia { vram_mb };
    }
    if other_dedicated_gpu_present() {
        return GpuTier::OtherDedicated;
    }
    GpuTier::None
}

/// Phrases `tier` as a model-size suggestion for the install panel.
/// Thresholds follow the common community rule of thumb for 4-bit
/// quantized models (roughly 0.7-1GB of VRAM per billion parameters, plus
/// headroom for context/runtime overhead) — a guideline, not a guarantee.
fn size_hint(tier: &GpuTier) -> String {
    match tier {
        GpuTier::Nvidia { vram_mb } => {
            let gb = *vram_mb as f64 / 1024.0;
            let range = if gb < 4.0 {
                "1-2B (ex: qwen2.5:1.5b)"
            } else if gb < 8.0 {
                "3B (ex: llama3.2:3b)"
            } else if gb < 12.0 {
                "7-8B (ex: llama3.1:8b)"
            } else if gb < 20.0 {
                "13-14B (ex: qwen2.5:14b)"
            } else if gb < 40.0 {
                "27-32B (ex: qwen2.5:32b)"
            } else {
                "70B (ex: llama3.1:70b)"
            };
            format!("NVIDIA GPU detected (~{gb:.0}GB VRAM) — models up to ~{range} should run well.")
        }
        GpuTier::OtherDedicated => "Dedicated GPU detected (non-NVIDIA) — exact VRAM unknown; \
            ~7B models are usually a good starting point."
            .to_string(),
        GpuTier::None => "No dedicated GPU detected — small models (≤3B, e.g. qwen2.5:1.5b) \
            run better on CPU."
            .to_string(),
    }
}

/// `sv status ollama` would need root — runit's supervise dirs are `0700
/// root:root` on this system (true for every service, not just ollama), so
/// a plain user always gets "access denied" and the status dot would read
/// down forever. Hitting the API directly, same fix already applied in
/// quickshell-d77/utumno's OllamaChat.qml, both sidesteps that and is the
/// more meaningful check anyway: it confirms ollama is actually serving
/// requests, not just that a process exists.
fn ollama_running() -> bool {
    let output = Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "--max-time", "2", OLLAMA_BASE])
        .output();
    matches!(output, Ok(o) if o.status.success() && o.stdout == b"200")
}

/// Bounded reachability check against Ollama's own site — cheap way to know
/// whether the automatic first-run pull (see `main`) has any real chance of
/// succeeding before kicking it off, same `--max-time`-bounded `curl` style
/// already used for the status/model-list polls above.
fn internet_reachable() -> bool {
    let output = Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "--max-time", "3", "https://ollama.com"])
        .output();
    matches!(output, Ok(o) if o.status.success() && o.stdout.starts_with(b"2"))
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
    PullFinished { message: String, ok: bool, auto: bool },
    GenerateToken(String),
    GenerateFinished,
}

/// Handle to a running `curl` pull, shared between `spawn_pull`'s own
/// thread and `cancel_pull` (called from the "Stop" button on the
/// first-run automatic pull — see `main`). `cancelled` lets the thread
/// phrase `PullFinished`'s message as a cancellation rather than a plain
/// failure once `child.kill()` makes the read loop below end early.
#[derive(Default)]
struct PullHandle {
    child: std::sync::Mutex<Option<std::process::Child>>,
    cancelled: std::sync::atomic::AtomicBool,
}
type SharedPullHandle = std::sync::Arc<PullHandle>;

fn cancel_pull(handle: &SharedPullHandle) {
    handle.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
    if let Some(child) = handle.child.lock().unwrap().as_mut() {
        let _ = child.kill();
    }
}

/// Same shape as `PullHandle`/`cancel_pull`, for the "Stop" button on a
/// response that's actively streaming (see `spawn_generate`).
#[derive(Default)]
struct GenerateHandle {
    child: std::sync::Mutex<Option<std::process::Child>>,
    cancelled: std::sync::atomic::AtomicBool,
}
type SharedGenerateHandle = std::sync::Arc<GenerateHandle>;

fn cancel_generate(handle: &SharedGenerateHandle) {
    handle.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
    if let Some(child) = handle.child.lock().unwrap().as_mut() {
        let _ = child.kill();
    }
}

/// Streams `POST /api/pull`, reporting each progress line back through
/// `tx`. Runs on its own thread: a model pull can take anywhere from
/// seconds to several minutes, far too long to block the event loop for.
/// `auto` just gets echoed back on every event so the UI can tell a
/// first-run automatic pull apart from a manually requested install.
/// Returns a fresh handle callers can pass to `cancel_pull` to abort it —
/// unused (and harmless to drop) for pulls that don't need to be
/// cancellable.
fn spawn_pull(tx: Sender<OllamaEvent>, model: String, auto: bool) -> SharedPullHandle {
    let handle: SharedPullHandle = Default::default();
    let handle_thread = handle.clone();
    thread::spawn(move || {
        tx.send(OllamaEvent::PullProgress(format!("Installing '{model}'..."))).ok();

        let body = serde_json::json!({ "name": model, "stream": true }).to_string();
        let child = Command::new("curl")
            .args(["-s", "-N", "-X", "POST", &format!("{OLLAMA_BASE}/api/pull"), "-d", &body])
            .stdout(Stdio::piped())
            .spawn();

        let mut child = match child {
            Ok(c) => c,
            Err(_) => {
                tx.send(OllamaEvent::PullFinished {
                    message: format!("Failed to install '{model}': curl not found"),
                    ok: false,
                    auto,
                })
                .ok();
                return;
            }
        };

        let stdout = child.stdout.take();
        *handle_thread.child.lock().unwrap() = Some(child);

        if let Some(stdout) = stdout {
            for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
                let Ok(chunk) = serde_json::from_str::<Json>(&line) else { continue };
                if let Some(err) = chunk.get("error").and_then(|v| v.as_str()) {
                    tx.send(OllamaEvent::PullProgress(format!("Error installing '{model}': {err}"))).ok();
                    continue;
                }
                let status = chunk.get("status").and_then(|v| v.as_str()).unwrap_or("");
                let total = chunk.get("total").and_then(Json::as_u64);
                let completed = chunk.get("completed").and_then(Json::as_u64);
                let text = match (total, completed) {
                    (Some(t), Some(c)) if t > 0 => {
                        format!("Installing '{model}': {status} ({}%)", c * 100 / t)
                    }
                    _ => format!("Installing '{model}': {status}"),
                };
                tx.send(OllamaEvent::PullProgress(text)).ok();
            }
        }

        // Take the child back out to reap it — cancel_pull only kills it,
        // it never takes it, so this is always the one that owns cleanup.
        let ok = handle_thread
            .child
            .lock()
            .unwrap()
            .take()
            .is_some_and(|mut c| c.wait().is_ok_and(|s| s.success()));
        let message = if handle_thread.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            format!("Installation of '{model}' cancelled.")
        } else if ok {
            format!("'{model}' installed successfully.")
        } else {
            format!("Failed to install '{model}'.")
        };
        tx.send(OllamaEvent::PullFinished { message, ok, auto }).ok();
    });
    handle
}

/// Streams `POST /api/generate` for `prompt` against `model`, sending each
/// response fragment back through `tx` as it arrives. Runs on its own
/// thread (see the module doc comment for why).
///
/// Guarded by `--speed-limit 1 --speed-time 30` rather than a hard
/// `--max-time`: a flat `--max-time 30` caps the *entire* request, so it
/// kills a slower model (e.g. qwen2.5:3b, which can take longer than 30s
/// total between cold model load and a full response on modest hardware)
/// even while it's actively streaming tokens. `--speed-limit`/`--speed-time`
/// only aborts if curl goes 30s *without any data at all* — a genuine stall
/// — and otherwise lets a slow-but-progressing generation run to
/// completion, same as fabric-d77's `requests` idle-timeout behaves. Still
/// exits with curl code 28 on trip, so the exit-code handling below is
/// unchanged.
fn spawn_generate(tx: Sender<OllamaEvent>, model: String, prompt: String) -> SharedGenerateHandle {
    let handle: SharedGenerateHandle = Default::default();
    let handle_thread = handle.clone();
    thread::spawn(move || {
        // keep_alive: 0 unloads the model right after this reply instead of
        // idling on Ollama's server-side 5min default — see
        // quickshell-d77/utumno's OllamaChat.qml, same fix, same reason.
        let body = serde_json::json!({
            "model": model, "prompt": prompt, "stream": true, "keep_alive": 0
        })
        .to_string();
        let child = Command::new("curl")
            .args([
                "-s",
                "-N",
                "--speed-limit",
                "1",
                "--speed-time",
                "30",
                &format!("{OLLAMA_BASE}/api/generate"),
                "-d",
                &body,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        let mut child = match child {
            Ok(c) => c,
            Err(_) => {
                tx.send(OllamaEvent::GenerateToken(
                    "\n[no response from Ollama — curl not found]\n".to_string(),
                ))
                .ok();
                tx.send(OllamaEvent::GenerateFinished).ok();
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

        let stdout = child.stdout.take();
        *handle_thread.child.lock().unwrap() = Some(child);

        let mut got_any = false;
        if let Some(stdout) = stdout {
            for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
                let Ok(chunk) = serde_json::from_str::<Json>(&line) else { continue };
                got_any = true;
                if let Some(err) = chunk.get("error").and_then(|v| v.as_str()) {
                    tx.send(OllamaEvent::GenerateToken(format!("\n[model error: {err}]\n"))).ok();
                } else if let Some(resp) = chunk.get("response").and_then(|v| v.as_str()) {
                    if !resp.is_empty() {
                        tx.send(OllamaEvent::GenerateToken(resp.to_string())).ok();
                    }
                }
            }
        }

        // Take the child back out to reap it — cancel_generate only kills
        // it, it never takes it, so this is always the one that owns cleanup.
        let status = handle_thread.child.lock().unwrap().take().map(|mut c| c.wait());
        let cancelled = handle_thread.cancelled.load(std::sync::atomic::Ordering::SeqCst);
        let err_buf = stderr_handle.and_then(|h| h.join().ok()).unwrap_or_default();
        let code = status.and_then(|s| s.ok()).and_then(|s| s.code()).unwrap_or(-1);

        if cancelled {
            tx.send(OllamaEvent::GenerateToken("\n[stopped]\n".to_string())).ok();
        } else
        // Mirrors quickshell-d77/utumno's three failure messages exactly.
        if code != 0 || !got_any {
            let msg = if err_buf.contains("Connection refused") || code == 7 {
                Some("\n[Ollama is not running. Check with: sv status ollama]\n".to_string())
            } else if code == 28 {
                Some("\n[Ollama took too long to respond — timeout]\n".to_string())
            } else if !got_any {
                Some(format!("\n[no response from Ollama — curl code: {code}]\n"))
            } else {
                None
            };
            if let Some(msg) = msg {
                tx.send(OllamaEvent::GenerateToken(msg)).ok();
            }
        }
        tx.send(OllamaEvent::GenerateFinished).ok();
    });
    handle
}

/// `layer_shika`'s Slint platform only implements `create_window_adapter`
/// (see `CustomSlintPlatform` in `layer-shika-adapters`), so `TextInput`'s
/// built-in Ctrl+C falls through to `Platform::set_clipboard_text`'s default
/// no-op impl — selecting the AI's output in `history_text_item` works, but
/// nothing actually reaches the Wayland clipboard. Shelling out to `wl-copy`
/// (same style as `curl`/`nvidia-smi` elsewhere here) sidesteps that gap
/// instead of waiting on upstream clipboard support.
fn copy_to_clipboard(text: &str) {
    let Ok(mut child) = Command::new("wl-copy").stdin(Stdio::piped()).spawn() else { return };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    let _ = child.wait();
}

fn main() -> Result<()> {
    let saved_model = read_saved_model();
    let initial_up = ollama_running();
    let initial_models = fetch_models().unwrap_or_default();
    let initial_model = pick_model(&initial_models, saved_model.as_deref())
        .unwrap_or_else(|| FALLBACK_MODEL.to_string());
    let hardware_hint = size_hint(&detect_gpu_tier());

    // First run: nothing installed yet. Rather than leaving the dropdown
    // empty and making the user hunt for a model name to type, grab the
    // fallback automatically — but only when it's actually likely to work
    // (service up, registry reachable), and flagged prominently in the UI
    // (`auto_pull_active`) with a "Stop" button, not silently in the
    // background.
    let auto_pull = initial_up && initial_models.is_empty() && internet_reachable();

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
                OllamaEvent::PullFinished { message, ok, auto } => {
                    instance.set_property("info_text", Value::String(message.as_str().into())).ok();
                    if *auto {
                        instance.set_property("auto_pull_active", Value::Bool(false)).ok();
                    }
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
                OllamaEvent::GenerateFinished => {
                    instance.set_property("generating", Value::Bool(false)).ok();
                }
            }
        }
    })?;

    // Kicked off before the surface callbacks below so `auto_pull_handle`
    // can be moved into the "Stop" button's callback closure.
    let auto_pull_handle = auto_pull.then(|| spawn_pull(tx.clone(), FALLBACK_MODEL.to_string(), true));

    // Holds whichever generate request is currently streaming, so
    // `generate_cancel` (the response "Stop" button) can reach it — unlike
    // `auto_pull_handle` this is replaced on every `prompt_submitted`
    // rather than set once, so it's a shared mutable slot instead of a
    // single moved value.
    let current_generate: std::sync::Arc<std::sync::Mutex<Option<SharedGenerateHandle>>> =
        Default::default();

    let initial_model_count = initial_models.len();
    shell.with_surface("Ollama", |comp| {
        comp.set_property("ollama_up", Value::Bool(initial_up)).ok();
        comp.set_property("current_model", Value::String(initial_model.as_str().into())).ok();
        comp.set_property("model_names", models_value(&initial_models)).ok();
        comp.set_property("model_count", Value::Number(initial_model_count as f64)).ok();
        comp.set_property("dropdown_height", Value::Number(dropdown_height_px(initial_model_count))).ok();
        comp.set_property("hardware_hint", Value::String(hardware_hint.as_str().into())).ok();
        comp.set_property("auto_pull_active", Value::Bool(auto_pull)).ok();

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
                spawn_pull(tx_install.clone(), name, false);
            }
            Value::Void
        }).ok();

        comp.set_callback("auto_pull_cancel", move |_| {
            if let Some(handle) = &auto_pull_handle {
                cancel_pull(handle);
            }
            Value::Void
        }).ok();

        let weak = comp.as_weak();
        let tx_prompt = tx.clone();
        let current_generate_submit = current_generate.clone();
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
            instance.set_property("generating", Value::Bool(true)).ok();
            let handle = spawn_generate(tx_prompt.clone(), model, prompt);
            *current_generate_submit.lock().unwrap() = Some(handle);
            Value::Void
        }).ok();

        let current_generate_cancel = current_generate.clone();
        comp.set_callback("generate_cancel", move |_| {
            if let Some(handle) = current_generate_cancel.lock().unwrap().as_ref() {
                cancel_generate(handle);
            }
            Value::Void
        }).ok();

        comp.set_callback("close_requested", move |_| {
            std::process::exit(0);
        }).ok();

        comp.set_callback("copy_requested", move |args| {
            let (Some(Value::String(text)), Some(Value::Number(anchor)), Some(Value::Number(cursor))) =
                (args.first(), args.get(1), args.get(2))
            else {
                return Value::Void;
            };
            let (start, end) = ((*anchor as usize).min(*cursor as usize), (*anchor as usize).max(*cursor as usize));
            if let Some(selected) = text.get(start..end) {
                copy_to_clipboard(selected);
            }
            Value::Void
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
