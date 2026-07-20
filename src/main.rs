mod network;
mod sysinfo;
mod weather;

use helium_wsl::compositors::{self, Workspace};
use helium_wsl::prelude::*;
use helium_wsl::slint::{ModelRc, VecModel};
use helium_wsl::slint_interpreter;
use helium_wsl::slint_interpreter::{Struct, Value};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::Duration;

const MARGIN: u32 = 10;
const FALLBACK_MONITOR_WIDTH: u32 = 1366;

/// Width of the primary monitor, so the bar's size is derived from the
/// actual screen instead of a value hardcoded for one machine.
///
/// This matters beyond cosmetics: Hyprland doesn't cleanly reconcile a
/// requested layer-surface width that's *larger* than the monitor with
/// `Top+Left+Right` anchoring — instead of clamping, it can offset the
/// surface so part of it renders off-screen. Deriving the width from the
/// real monitor guarantees the requested size and the anchor-stretched size
/// always agree, on any screen.
///
/// Tries `niri_monitor_width()`, then `sway_monitor_width()` — see their doc
/// comments for why neither can go through `helium_wsl` — falling back to
/// `compositors::detect()` (Hyprland) before finally giving up on the
/// hardcoded default. `helium_wsl::compositors::detect()` doesn't know about
/// Sway at all (only Hyprland/niri), so without the dedicated Sway query
/// below every Sway machine would silently hit the same fallback the niri
/// bug above used to cause — visible there as a *centered* bar with equal
/// gaps on both sides rather than niri's flush-left one, because wlroots
/// follows the wlr-layer-shell spec literally: an undersized surface
/// anchored to both opposing edges gets centered on that axis, where niri
/// just anchors it to the one edge and leaves the rest of the row empty.
fn primary_monitor_width() -> u32 {
    niri_monitor_width()
        .or_else(sway_monitor_width)
        .or_else(|| {
            compositors::detect().ok().and_then(|c| {
                let monitors = c.monitors();
                monitors
                    .iter()
                    .find(|m| m.primary)
                    .or_else(|| monitors.first())
                    .map(|m| m.width)
            })
        })
        .unwrap_or(FALLBACK_MONITOR_WIDTH)
}

/// Reads the focused/primary output's logical width straight from niri's
/// `Outputs` IPC, bypassing `helium_wsl::compositors::niri::Niri::monitors()`
/// entirely: that backend deserializes each output's `current_mode` as an
/// inline `{width, height}` object, but niri actually reports it as an
/// integer *index* into the output's own `modes` array (confirmed against a
/// live niri instance — see the raw `Outputs` reply). The type mismatch
/// makes `serde_json::from_value::<NiriOutput>` fail for every real niri
/// output, so `monitors()` silently returns an empty `Vec` and
/// `primary_monitor_width()` fell through to `FALLBACK_MONITOR_WIDTH` on
/// every niri machine, regardless of actual screen size — the bar rendered
/// ~30% too narrow with the rest of the row left empty. `logical.width` here
/// is already scale-adjusted, which is what a layer-shell surface needs
/// anyway (resolving `modes[current_mode].width` would give physical
/// pixels instead).
///
/// Returns `None` (rather than picking arbitrarily) whenever there's more
/// than one output, since niri's `Outputs` reply has no "primary" concept
/// to disambiguate — that case falls back to `compositors::detect()` above,
/// same as before this fix existed.
fn niri_monitor_width() -> Option<u32> {
    let reply = niri_command(r#"{"Outputs":null}"#)?;
    let value: serde_json::Value = serde_json::from_str(reply.trim()).ok()?;
    let outputs = value.get("Ok")?.get("Outputs")?.as_object()?;
    if outputs.len() != 1 {
        return None;
    }
    outputs
        .values()
        .find_map(|o| o.get("logical")?.get("width")?.as_u64())
        .map(|w| w as u32)
}

/// Reads the focused output's logical width from Sway over its native
/// i3-ipc socket (`$SWAYSOCK`) — `helium_wsl::compositors::detect()` has no
/// Sway backend whatsoever, Hyprland or niri only, so this is additive
/// rather than a workaround for a broken upstream parse like
/// `niri_monitor_width()` above. Speaks the protocol directly (magic bytes
/// + native-endian length/type header, per Sway's own `IPC` documentation)
/// rather than shelling out to `swaymsg`, the same reasoning `hypr_command`/
/// `niri_command` below already use for their compositors.
fn sway_monitor_width() -> Option<u32> {
    const GET_OUTPUTS: u32 = 3;
    let body = sway_command(GET_OUTPUTS, b"")?;
    let outputs: serde_json::Value = serde_json::from_slice(&body).ok()?;
    let outputs = outputs.as_array()?;
    outputs
        .iter()
        .find(|o| o.get("focused").and_then(|v| v.as_bool()).unwrap_or(false))
        .or_else(|| outputs.iter().find(|o| o.get("active").and_then(|v| v.as_bool()).unwrap_or(false)))
        .or_else(|| outputs.first())
        .and_then(|o| o.get("rect")?.get("width")?.as_u64())
        .map(|w| w as u32)
}

/// Sends one i3-ipc request to Sway's control socket and returns the raw
/// reply payload. Header format is 6 magic bytes (`b"i3-ipc"`) + a 4-byte
/// payload length + a 4-byte message type, both in the machine's native
/// byte order (client and server always run on the same host, so the
/// protocol doesn't bother specifying one) — same shape for the reply,
/// immediately followed by `length` bytes of JSON.
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
    std::io::Read::read_exact(&mut stream, &mut header).ok()?;
    if &header[0..6] != b"i3-ipc" {
        return None;
    }
    let len = u32::from_ne_bytes(header[6..10].try_into().ok()?) as usize;
    let mut body = vec![0u8; len];
    std::io::Read::read_exact(&mut stream, &mut body).ok()?;
    Some(body)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bar_width = primary_monitor_width() - 2 * MARGIN;

    // Embedded at compile time rather than loaded from "ui/bar.slint" at
    // runtime: a relative path only resolves when launched from the project
    // root, which breaks the moment the binary is installed system-wide
    // (e.g. /usr/bin/helium-shell) and run from anywhere else, or autostarted
    // by the compositor with an unrelated working directory.
    let mut shell = Helium::from_source(include_str!("../ui/bar.slint"))
        .surface("Bar")
        .size(bar_width, 36)
        .anchor((AnchorEdge::Top, AnchorEdge::Left, AnchorEdge::Right))
        .margin(MARGIN as i32, MARGIN as i32, 0, MARGIN as i32)
        // Hyprland adds the surface's own top margin on top of this value,
        // so the exclusive zone only needs to cover the bar's height itself.
        .exclusive_zone(36)
        .layer(Layer::Top)
        .build()?;

    // The Slint component's own width must match the surface size above —
    // set once here rather than hardcoded in ui/bar.slint.
    shell.set("Bar", "bar_width", bar_width as i32);

    // Seed initial workspace state so the bar isn't blank before the first tick.
    if let Ok(compositor) = compositors::detect() {
        apply_workspaces(&mut shell, &compositor.workspaces());
    }
    if let Some(w) = weather::status() {
        shell.set("Bar", "weather_text", format!("{}  {}", w.condition, w.temperature));
    }

    // Clicking a workspace pill dispatches a real workspace switch.
    shell.on_signal("Bar", "workspace_clicked", |args| {
        if let Some(slint_interpreter::Value::Number(n)) = args.first() {
            switch_workspace(*n as i32);
        }
    });

    // Clicking the apps icon / power icon spawns the sibling launcher /
    // session-menu binaries (spawn-on-demand, like rofi — see their doc
    // comments for why they're separate processes instead of toggled panels
    // inside this one).
    shell.on_signal("Bar", "launcher_clicked", |_| spawn_sibling("helium-launcher"));
    shell.on_signal("Bar", "session_clicked", |_| spawn_sibling("helium-session"));
    shell.on_signal("Bar", "network_clicked", |_| launch_nmtui());
    shell.on_signal("Bar", "volume_clicked", |_| toggle_mute());
    shell.on_signal("Bar", "battery_clicked", |_| cycle_power_profile());

    // Clock + workspace polling, once a second.
    //
    // Workspace state is polled rather than pushed via
    // `on_compositor_event`/`CompositorEvent::WorkspaceChanged`: helium-wsl's
    // Hyprland backend resolves that event by re-querying `j/workspaces` +
    // `j/activeworkspace` over two separate synchronous IPC round trips
    // instead of using the workspace id already embedded in the raw
    // `workspace>>N` event line. Under fast workspace churn those two
    // snapshots can disagree (no workspace comes back marked `active`), and
    // `poll_event()` then silently drops the event, leaving the bar's
    // workspace pills desynced. Polling `compositor.workspaces()` here
    // sidesteps that upstream bug entirely: each tick is a fresh,
    // self-consistent read.
    let mut prev_cpu = None;
    shell.on_tick(Duration::from_secs(1), move |ctx| {
        ctx.set(
            "Bar",
            "clock_text",
            helium_wsl::services::time::formatted("%a %d %b  %H:%M:%S"),
        );
        if let Ok(compositor) = compositors::detect() {
            apply_workspaces_ctx(ctx, &compositor.workspaces());
        }
        if let Some(pct) = sysinfo::cpu_usage_percent(&mut prev_cpu) {
            ctx.set("Bar", "cpu_text", format!("{pct}%"));
        }
    })?;

    // Network + RAM + battery + volume: cheap local reads except network
    // (a blocking D-Bus round trip), all on the same slower timer.
    //
    // Network uses our own `network::status()` rather than
    // `helium_wsl::services::network::status()`: the upstream function
    // deserializes NetworkManager's `GetDevices` reply (D-Bus signature `ao`)
    // as `Vec<OwnedValue>`, but it's a plain array of object paths, not
    // variants — zbus rejects that with "Signature mismatch: got 'ao',
    // expected 'av'" and the call always fails. See src/network.rs.
    shell.on_tick(Duration::from_secs(5), |ctx| {
        if let Ok(info) = network::status() {
            let label = match (&info.ssid, info.signal_strength) {
                (Some(s), Some(strength)) => format!("{s}  {strength}%"),
                (Some(s), None) => s.clone(),
                (None, _) if info.connected => "connected".to_string(),
                (None, _) => "offline".to_string(),
            };
            ctx.set("Bar", "net_text", label);
            ctx.set("Bar", "net_connected", info.connected);
        }

        if let Some(pct) = sysinfo::ram_usage_percent() {
            ctx.set("Bar", "ram_text", format!("{pct}%"));
        }

        if let Some(bat) = sysinfo::battery() {
            ctx.set("Bar", "bat_text", format!("{}%", bat.percent));
            ctx.set("Bar", "bat_charging", bat.charging);
        }

        if let Some(vol) = sysinfo::volume() {
            ctx.set("Bar", "vol_text", if vol.muted { "muted".to_string() } else { format!("{}%", vol.percent) });
            ctx.set("Bar", "vol_muted", vol.muted);
        }
    })?;

    // Weather: a network call to a third-party service (wttr.in), so it
    // gets its own long-interval timer rather than piggybacking on the
    // network/stats tick above.
    shell.on_tick(weather::POLL_INTERVAL, |ctx| {
        if let Some(w) = weather::status() {
            ctx.set("Bar", "weather_text", format!("{}  {}", w.condition, w.temperature));
        }
    })?;

    shell.run()?;
    Ok(())
}

/// Spawns another binary installed alongside this one (found via
/// `current_exe()`'s directory), so it works both in `target/debug` during
/// development and once installed system-wide (e.g. `/usr/bin`), as long as
/// the sibling binary was installed to the same directory.
fn spawn_sibling(name: &str) {
    let Ok(exe) = std::env::current_exe() else { return };
    let Some(dir) = exe.parent() else { return };
    let _ = std::process::Command::new(dir.join(name)).spawn();
}

/// Toggles Master mute via `amixer`, matching the control name `sysinfo::volume()`
/// already reads and the mute-toggle approach quickshell-d77/fabric-d77 use.
fn toggle_mute() {
    let _ = std::process::Command::new("amixer").args(["set", "Master", "toggle"]).spawn();
}

/// Power profiles cycled by clicking the battery chip, in the order
/// quickshell-d77/fabric-d77 cycle them (power-profiles-daemon's own three
/// profiles — there's no fourth to add).
const POWER_PROFILES: [&str; 3] = ["performance", "balanced", "power-saver"];

/// Advances to the next `power-profiles-daemon` profile via `powerprofilesctl`.
/// Reads the current profile synchronously (a local D-Bus round trip,
/// negligible next to a click) so the cycle has somewhere to advance from;
/// unrecognized/missing output just starts the cycle over at the first profile.
fn cycle_power_profile() {
    let current = std::process::Command::new("powerprofilesctl")
        .arg("get")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());
    let idx = current
        .and_then(|c| POWER_PROFILES.iter().position(|p| *p == c))
        .unwrap_or(POWER_PROFILES.len() - 1);
    let next = POWER_PROFILES[(idx + 1) % POWER_PROFILES.len()];
    let _ = std::process::Command::new("powerprofilesctl").args(["set", next]).spawn();
}

/// Opens a floating terminal running `nmtui`, the same way quickshell-d77 and
/// fabric-d77 do: `nmtui-float` isn't a script, it's a Wayland app-id/class
/// assigned to the launched terminal window so a compositor windowrule can
/// float it (e.g. Hyprland's `windowrulev2 = float, class:^(nmtui-float)$`,
/// expected to live in the user's own compositor config, not here).
///
/// Tries terminals in the same order as those two projects, falling through
/// via shell `||` to the next one if a given terminal isn't installed.
fn launch_nmtui() {
    let candidates = [
        ("foot", "foot --app-id=nmtui-float -e nmtui"),
        ("kitty", "kitty --class=nmtui-float -e nmtui"),
        ("alacritty", "alacritty --class=nmtui-float -e nmtui"),
        ("wezterm", "wezterm start --class nmtui-float -- nmtui"),
        ("xterm", "xterm -class nmtui-float -e nmtui"),
    ];
    let script = candidates
        .iter()
        .map(|(bin, cmd)| format!("command -v {bin} >/dev/null 2>&1 && exec setsid {cmd}"))
        .collect::<Vec<_>>()
        .join(" || ");
    let _ = std::process::Command::new("sh").arg("-c").arg(script).spawn();
}

/// Sends a command to Hyprland's control socket and returns its reply.
fn hypr_command(cmd: &str) -> Option<String> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".into());
    let path = std::path::PathBuf::from(runtime).join("hypr").join(sig).join(".socket.sock");
    let mut stream = UnixStream::connect(&path).ok()?;
    stream.write_all(cmd.as_bytes()).ok()?;
    stream.shutdown(std::net::Shutdown::Write).ok();
    let mut reply = String::new();
    std::io::Read::read_to_string(&mut stream, &mut reply).ok()?;
    Some(reply)
}

/// Sends one JSON request to niri's IPC socket and returns its reply line.
///
/// Mirrors the request/response shape niri itself uses (one line in, one
/// line out over `NIRI_SOCKET`) rather than reading to EOF like
/// `hypr_command` does — niri doesn't close the connection after replying,
/// so a `read_to_string` here would just hang waiting for EOF.
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

fn switch_workspace(n: i32) {
    // `compositors::detect()` picks Hyprland over Niri when both env vars
    // happen to be set, so mirror that same precedence here.
    if std::env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok() {
        // Standard Hyprland textual IPC — works on any normal Hyprland install.
        if let Some(reply) = hypr_command(&format!("dispatch workspace {n}")) {
            if !reply.starts_with("error") {
                return;
            }
        }
        // Fallback for compositors that route `dispatch` through a Lua layer
        // (e.g. a "hyprland-lua" build), where dispatchers are Lua calls instead
        // of the classic `<name> <args>` text protocol.
        hypr_command(&format!("dispatch hl.dsp.focus({{ workspace = {n} }})"));
        return;
    }

    if std::env::var("NIRI_SOCKET").is_ok() {
        niri_command(&format!(
            r#"{{"Action":{{"FocusWorkspace":{{"reference":{{"Index":{n}}}}}}}}}"#
        ));
    }
}

/// Builds the `[WorkspaceItem]` model value for `ui/bar.slint`'s `workspaces`
/// property directly from the compositor's live workspace list — the same
/// `Vec<Workspace>` for Hyprland and Niri alike, so the bar shows exactly as
/// many pills as actually exist instead of a hardcoded count (Niri's
/// workspaces are dynamic per-monitor, unlike Hyprland's small fixed set).
fn workspaces_value(workspaces: &[Workspace]) -> Value {
    let items: Vec<Value> = workspaces
        .iter()
        .map(|w| {
            let fields: Struct = [
                ("id".to_string(), Value::Number(w.id as f64)),
                ("label".to_string(), Value::String(w.name.clone().into())),
                ("active".to_string(), Value::Bool(w.active)),
                ("occupied".to_string(), Value::Bool(w.occupied)),
            ]
            .into_iter()
            .collect();
            Value::Struct(fields)
        })
        .collect();
    Value::Model(ModelRc::new(VecModel::from(items)))
}

fn apply_workspaces(shell: &mut ShellInstance, workspaces: &[Workspace]) {
    shell.set("Bar", "workspaces", workspaces_value(workspaces));
}

fn apply_workspaces_ctx(ctx: &mut TickContext, workspaces: &[Workspace]) {
    ctx.set("Bar", "workspaces", workspaces_value(workspaces));
}
