mod network;
mod sysinfo;
mod weather;

use helium_wsl::compositors::{self, Workspace};
use helium_wsl::prelude::*;
use helium_wsl::slint_interpreter;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::Duration;

const WORKSPACE_SLOTS: usize = 5;
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
fn primary_monitor_width() -> u32 {
    compositors::detect()
        .ok()
        .and_then(|c| {
            let monitors = c.monitors();
            monitors
                .iter()
                .find(|m| m.primary)
                .or_else(|| monitors.first())
                .map(|m| m.width)
        })
        .unwrap_or(FALLBACK_MONITOR_WIDTH)
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

fn switch_workspace(n: i32) {
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
}

fn apply_workspaces(shell: &mut ShellInstance, workspaces: &[Workspace]) {
    if let Some(active) = workspaces.iter().find(|w| w.active) {
        shell.set("Bar", "active_workspace", active.id as i32);
    }
    for slot in 1..=WORKSPACE_SLOTS {
        let occupied = workspaces
            .iter()
            .any(|w| w.id as usize == slot && w.occupied);
        shell.set("Bar", &format!("workspace_{slot}"), slot.to_string());
        shell.set("Bar", &format!("occupied_{slot}"), occupied);
    }
}

fn apply_workspaces_ctx(ctx: &mut TickContext, workspaces: &[Workspace]) {
    if let Some(active) = workspaces.iter().find(|w| w.active) {
        ctx.set("Bar", "active_workspace", active.id as i32);
    }
    for slot in 1..=WORKSPACE_SLOTS {
        let occupied = workspaces
            .iter()
            .any(|w| w.id as usize == slot && w.occupied);
        ctx.set("Bar", &format!("workspace_{slot}"), slot.to_string());
        ctx.set("Bar", &format!("occupied_{slot}"), occupied);
    }
}
