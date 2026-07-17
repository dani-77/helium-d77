# helium-shell

[![Rust](https://img.shields.io/badge/rust-2021-orange?logo=rust)](Cargo.toml)
[![Wayland](https://img.shields.io/badge/wayland-layer--shell-blue?logo=wayland)](https://github.com/zepyxunderscore/helium-wsl)
[![Hyprland](https://img.shields.io/badge/compositor-Hyprland-00b6b6)](https://hyprland.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-green)](LICENSE)

A personal Wayland status bar built on top of [Helium](https://github.com/zepyxunderscore/helium-wsl),
which wraps `layer-shika` to give you a clean Rust + Slint API for layer-shell
surfaces (compositor auto-detection, D-Bus services, timers, etc). Renders as
a rounded, floating pill anchored to the top of the screen.

## What it shows

```
[apps] [ 1 2 3 4 5 ]  weather | clock (center)  wifi|cpu|ram|bat|vol [power]
```

- **Apps icon** (far left) — opens `helium-launcher`, a click-only app grid
  (see Launcher below).
- **Workspaces** — 5 numbered pills, active one highlighted green, a
  small dot marks occupied-but-inactive ones. Click a pill to switch to that
  workspace (Hyprland only — see Limitations). State is polled every second,
  not pushed via compositor events (see Limitations for why). The bar
  reserves real screen space (exclusive zone), so windows don't render
  underneath it.
- **Weather + date/time** — geometrically centered on the bar as one unit
  (not just centered in whatever space is left between the other two
  groups). Weather is condition + temperature only (e.g. "Clear  +20°C"),
  from wttr.in, checked every 15 minutes. Clock updates every second.
- **Network** — Wi-Fi SSID + signal strength (`SSID  72%`), "wired" on
  Ethernet, "offline" otherwise. Polled every 5s via NetworkManager D-Bus.
- **CPU** — utilization percent since the last tick, from `/proc/stat`.
- **RAM** — used percent, from `/proc/meminfo`.
- **Battery** — charge percent from sysfs; icon switches to a bolt while
  charging.
- **Volume** — ALSA `Master` level and mute state via `amixer`.
- **Power icon** (far right) — opens `helium-session`, a click-only session
  menu (see Session menu below).

Every section has a Nerd Font icon (apps/workspaces/weather/clock/network/cpu/
ram/battery/volume/power) — see Requirements.

## Launcher (`helium-launcher`)

A click-only app launcher, spawned on demand (like rofi/wofi) rather than a
panel toggled inside the bar — see Limitations for why there's no search
field or arrow-key navigation. Lists every non-hidden `.desktop` entry from
`/usr/share/applications` and `~/.local/share/applications` as a scrollable,
clickable list, each row's icon resolved from the entry's `Icon=` (a
best-effort search across every installed icon theme plus `/usr/share/
pixmaps`, not a full XDG icon-theme-spec implementation — entries whose icon
can't be found are shown with no icon rather than a broken image). Clicking
an entry launches it (wrapped in a detected terminal if the entry has
`Terminal=true`) and the launcher closes; a "Close" row at the top cancels
without launching anything.

Opened by clicking the apps icon in the bar, or bind it directly to a key in
your Hyprland config:

```
bind = SUPER, D, exec, /usr/bin/helium-launcher
```

## Session menu (`helium-session`)

A click-only Lock / Suspend / Reboot / Shutdown / Logout menu, mirroring the
action set and commands from a quickshell "session menu" widget:
`loginctl <action>` with a `systemctl <action>` fallback for
suspend/reboot/poweroff, `loginctl terminate-session "$XDG_SESSION_ID"` for
logout (falls back to `self` if that variable isn't set), and `hyprlock` for
Lock (falls back to `loginctl lock-session` if `hyprlock` isn't installed).
Opened by clicking the power icon in the bar, or bind it to a key the same
way as the launcher above.

## Requirements

- A running Wayland session with `$WAYLAND_DISPLAY` set.
- Hyprland (for workspace info and click-to-switch — the bar still runs
  without it, just without that section updating).
- NetworkManager on D-Bus for the network segment.
- `amixer` (alsa-utils) for the volume segment.
- `curl` and internet access for the weather segment (queries wttr.in — no
  API key needed).
- A battery under `/sys/class/power_supply/*` for the battery segment (a
  desktop with none just won't get a value there).
- Rust (edition 2021) and the system deps `layer-shika` needs for Wayland
  client libraries (`wayland-client`, `libxkbcommon`, etc. — on Void these are
  `libwayland-devel` and `libxkbcommon-devel`).
- A Nerd Font installed as **"Symbols Nerd Font"** (or edit the icon
  codepoints in `ui/bar.slint` to match a different one you have — see
  `fc-list | grep -i nerd`). Regular text uses "Space Grotesk" / "Space Mono".

## Running

```sh
cargo run --release
```

`ui/bar.slint` is embedded into the binary at compile time (`include_str!`),
so the built binary is self-contained — it can be installed anywhere (e.g.
`/usr/bin/helium-shell`) and run from any working directory, autostart
config included.

## Customizing

- **Monitor width**: derived automatically at startup from
  `compositor.monitors()` (see `primary_monitor_width()` in `src/main.rs`),
  so there's nothing to hand-edit per machine. The margin (10px on three
  sides) is a constant (`MARGIN`) in the same file if you want it different.
- **Colors/fonts**: everything lives in `ui/bar.slint`; the bar reuses the
  dark/green palette from Helium's own examples (`#0d0d0d` / `#141414`
  background, `#76b900` accent).
- **Sections**: add more properties to the `Bar` component and set them from
  `src/main.rs` the same way the existing ones are — via `shell.set(...)` /
  `ctx.set(...)`.

## Limitations (inherited from Helium 0.2.3, worked around here)

- **Workspace state is polled, not pushed.** helium-wsl's Hyprland backend
  resolves `CompositorEvent::WorkspaceChanged` by re-querying `j/workspaces` +
  `j/activeworkspace` over two separate synchronous IPC calls instead of using
  the workspace id already embedded in the raw `workspace>>N` event line.
  Under fast workspace churn those two snapshots can disagree, and
  `poll_event()` silently drops the event — the pills would desync and never
  recover. `src/main.rs` sidesteps this by polling `compositor.workspaces()`
  on a 1s timer instead of trusting the push event.
- **`helium_wsl::services::network::status()` is broken upstream.**
  `GetDevices` returns D-Bus signature `ao` (plain object paths); the crate
  deserializes it as `Vec<OwnedValue>` (expects `av`), so the call always
  errors. `src/network.rs` reimplements the NetworkManager queries directly
  with the correct type.
- **Bluetooth is not wired into this bar** (dropped in favor of CPU/RAM/
  battery/volume). `helium_wsl::services::bluetooth` still works standalone if
  you want to add it back — see `docs/services.md` in the helium-wsl repo.
- **Workspace click-to-switch is Hyprland-only**, and talks to Hyprland's
  control socket directly — Helium's `Compositor` trait has no dispatch/write
  API. Niri would need its own IPC call added to `switch_workspace()` in
  `src/main.rs`. It first tries the standard textual IPC (`dispatch workspace
  N`); if that's rejected, it falls back to `dispatch hl.dsp.focus({
  workspace = N })`, which some Lua-config Hyprland builds (e.g.
  "hyprland-lua") use instead of the classic `<dispatcher> <args>` protocol.
- Audio (beyond ALSA volume via `amixer`), power, and power-profiles services
  in helium-wsl itself are stubbed upstream and aren't used here.
- **No text search or arrow-key navigation in the launcher.** Helium's
  `on_key` is stubbed upstream too (`// todo: waiting on layer-shika keyboard
  input API`), so there's no real keyboard input to build a search field on
  top of — `helium-launcher` is click-only for exactly that reason. If
  layer-shika grows keyboard support, that's the thing to revisit.
