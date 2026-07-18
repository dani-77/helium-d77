# helium-shell

[![Rust](https://img.shields.io/badge/rust-2021-orange?logo=rust)](Cargo.toml)
[![Wayland](https://img.shields.io/badge/wayland-layer--shell-blue?logo=wayland)](https://github.com/zepyxunderscore/helium-wsl)
[![Hyprland](https://img.shields.io/badge/compositor-Hyprland-00b6b6)](https://hyprland.org)
[![niri](https://img.shields.io/badge/compositor-niri-blueviolet)](https://github.com/YaLTeR/niri)
[![License: MIT](https://img.shields.io/badge/license-MIT-green)](LICENSE)

A personal Wayland status bar built on top of [Helium](https://github.com/zepyxunderscore/helium-wsl),
which wraps `layer-shika` to give you a clean Rust + Slint API for layer-shell
surfaces (compositor auto-detection, D-Bus services, timers, etc). Renders as
a rounded, floating pill anchored to the top of the screen.

## What it shows

```
[apps] [ 1 2 3 4 5 ]  weather | clock (center)  wifi|cpu|ram|bat|vol [power]
```

- **Apps icon** (far left) — opens `helium-launcher`, a searchable app list
  (see Launcher below).
- **Workspaces** — one pill per *actual* live workspace (a `[WorkspaceItem]`
  model built from the compositor's own workspace list, not a hardcoded
  count), active one highlighted green, a small dot marks
  occupied-but-inactive ones. Click a pill to switch to that workspace —
  works on both Hyprland and Niri (see `switch_workspace()` in
  `src/main.rs`). State is polled every second, not pushed via compositor
  events (see Limitations for why). The bar reserves real screen space
  (exclusive zone), so windows don't render underneath it.
- **Weather + date/time** — geometrically centered on the bar as one unit
  (not just centered in whatever space is left between the other two
  groups). Weather is condition + temperature only (e.g. "Clear  +20°C"),
  from wttr.in, checked every 15 minutes. Clock updates every second.
- **Network** — Wi-Fi SSID + signal strength (`SSID  72%`), "wired" on
  Ethernet, "offline" otherwise. Polled every 5s via NetworkManager D-Bus.
  Click it to open `nmtui` in a floating terminal (see Requirements for
  which terminals it tries).
- **CPU** — utilization percent since the last tick, from `/proc/stat`.
- **RAM** — used percent, from `/proc/meminfo`.
- **Battery** — charge percent from sysfs; icon switches to a bolt while
  charging. Click it to cycle the active `power-profiles-daemon` profile
  (performance → balanced → power-saver → …) via `powerprofilesctl`.
- **Volume** — ALSA `Master` level and mute state via `amixer`. Click it to
  toggle mute (`amixer set Master toggle`).
- **Power icon** (far right) — opens `helium-session`, a click-and-keyboard
  session menu (see Session menu below).

Volume and power-profile changes (from anywhere, not just the bar's own
clicks) also flash a small OSD in the top-right corner — see OSD below.

Every section has a Nerd Font icon (apps/workspaces/weather/clock/network/cpu/
ram/battery/volume/power) — see Requirements.

## Launcher (`helium-launcher`)

A rofi/wofi-style app launcher, spawned on demand rather than a panel
toggled inside the bar. Lists every non-hidden `.desktop` entry from
`/usr/share/applications` and `~/.local/share/applications` as a scrollable
list, each row's icon resolved from the entry's `Icon=` (a best-effort
search across every installed icon theme plus `/usr/share/pixmaps`, not a
full XDG icon-theme-spec implementation — entries whose icon can't be found
are shown with no icon rather than a broken image).

A search box grabs keyboard focus as soon as the launcher opens (the
surface requests `KeyboardInteractivity::Exclusive`); typing filters the
list live, case-insensitive substring match on the app name. Up/Down move
the selection among the currently-visible rows, Enter launches whichever
row is selected, Escape closes without launching anything — all real
Wayland keyboard input via Slint's `TextInput`, the same mechanism the
lock screen's password field uses (see Limitations' old note on this,
corrected). Clicking a row still works too and launches it directly
(wrapped in a detected terminal if the entry has `Terminal=true`).

Opened by clicking the apps icon in the bar, or bind it directly to a key in
your Hyprland config:

```
bind = SUPER, D, exec, /usr/bin/helium-launcher
```

## Session menu (`helium-session`)

A Lock / Suspend / Reboot / Shutdown / Logout menu, mirroring the action set
and commands from a quickshell "session menu" widget: `loginctl <action>`
with a `systemctl <action>` fallback for suspend/reboot/poweroff,
`loginctl terminate-session "$XDG_SESSION_ID"` for logout (falls back to
`self` if that variable isn't set). Lock runs `hyprlock` if it's installed,
falling back to `loginctl lock-session` otherwise.

Grabs keyboard focus as soon as it opens. Left/Right move the selection
across the row (Cancel included), Enter activates whichever button is
selected, Escape cancels — via a `FocusScope` around the button row (no
text field needed here, unlike the launcher, so no `TextInput`). Clicking a
button still works too. Opened by clicking the power icon in the bar, or
bind it to a key the same way as the launcher above.

## Locker (`helium-locker`)

A native screen locker, unlike the `hyprlock`/`loginctl` shell-outs the
session menu currently uses: it locks the session itself via the
`ext-session-lock-v1` Wayland protocol, so the compositor enforces the lock
rather than a window merely being drawn on top. Shows a clock, date, and
password field (`ui/lock.slint`), verified against PAM.

Built directly on `layer-shika`'s `Shell` (not helium-wsl's `Helium` wrapper,
which doesn't expose session-lock yet — see the comment in `Cargo.toml`).

**Not currently wired into the session menu's Lock action** — under niri,
`layer-shika`'s session-lock surface (which uses `wp_fractional_scale` +
`wp_viewporter`) gets keyboard/pointer focus and then niri immediately
cancels the lock via `ext_session_lock_v1.finished()` about 30ms later
(confirmed with a `WAYLAND_DEBUG=1` trace — not a bug in this repo's own
code). The binary and PAM setup below are otherwise complete; re-enable the
`lock_command()` preference in `src/bin/helium-session.rs` once that's fixed
upstream in `layer-shika`.

Requires a PAM service file at `/etc/pam.d/helium-locker` (source in
`pam.d/helium-locker` in this repo). Without it, PAM fails closed: the lock
screen comes up but no password will ever be accepted.

**Not installed by `make install`** while the niri incompatibility above is
unresolved (see the Makefile's own comment). Build and install it by hand if
your compositor doesn't hit that bug:

```sh
cargo build --release --bin helium-locker
sudo install -Dm755 target/release/helium-locker /usr/bin/helium-locker
sudo install -Dm644 pam.d/helium-locker /etc/pam.d/helium-locker
```

Bind it directly to a key in your compositor config if you want a dedicated
lock shortcut:

```
bind = SUPER, L, exec, /usr/bin/helium-locker
```

## OSD (`helium-osd`)

A small overlay (top-right corner, below the bar) that briefly appears when
volume/mute, screen brightness, or the active power-profiles-daemon profile
changes, mirroring the OSD in quickshell-d77/fabric-d77. It polls
`amixer`/`brightnessctl`/`powerprofilesctl` directly on its own ~300ms timer
rather than reacting to the bar's click handlers, so it also reacts to
changes made outside the bar (hardware media keys,
`brightnessctl`/`powerprofilesctl` run from a terminal, etc). Auto-hides
after 2.5s.

A separate, always-running process (not spawned on demand by the bar): it
sits at a 1x1 surface size when idle and resizes itself up only while
showing. **You need to autostart it yourself** alongside `helium-shell` —
the bar itself never launches it. For a quick one-off test without touching
your compositor config, just run it directly from a terminal (e.g.
`target/release/helium-osd &`) and then change the volume or click the
battery chip. For a real autostart entry:

```
# Hyprland (hyprland.conf)
exec-once = /usr/bin/helium-osd

# niri (config.kdl)
spawn-at-startup "/usr/bin/helium-osd"
```

Built directly on `layer-shika`'s `Shell`, like `helium-locker` — see its
own doc comment for why (per-surface resize and direct `AppState` property
access aren't exposed by helium-wsl's `Helium` wrapper).

## Requirements

- A running Wayland session with `$WAYLAND_DISPLAY` set.
- Hyprland or niri (for workspace info and click-to-switch — the bar still
  runs without either, just without that section updating).
- NetworkManager on D-Bus for the network segment, plus `nmtui` (part of
  NetworkManager) and one of `foot`/`kitty`/`alacritty`/`wezterm`/`xterm` for
  the network chip's click-to-open behavior.
- `amixer` (alsa-utils) for the volume segment and its click-to-mute.
- `brightnessctl` for the OSD's brightness display (no bar segment reads it
  yet — the OSD just reacts to whatever changed brightness, e.g. hardware
  keys or `brightnessctl` itself).
- `power-profiles-daemon` (`powerprofilesctl`) for the battery chip's
  click-to-cycle-profile behavior and the OSD's profile display — the
  segment/chip itself still works without it, that click just becomes a
  no-op.
- `curl` and internet access for the weather segment (queries wttr.in — no
  API key needed).
- A battery under `/sys/class/power_supply/*` for the battery segment (a
  desktop with none just won't get a value there).
- Rust (edition 2021) and the system deps `layer-shika`/Slint need at build
  time for Wayland, fonts, and PAM: on Void these are `wayland-devel`,
  `libxkbcommon-devel`, `fontconfig-devel`, `freetype-devel`, and
  `pam-devel` (needed to build `helium-locker`'s PAM auth even though it
  isn't installed by `make install` right now — see Locker below; `pam-libs`,
  already pulled in by anything else using PAM on your system, covers it at
  runtime).
- For `helium-osd`: same Wayland/layer-shell support the bar itself needs —
  nothing extra.
- For `helium-locker` (optional, build-and-install-by-hand only — see
  Locker below): a compositor with `ext-session-lock-v1` support that
  doesn't hit the niri incompatibility described there, and the PAM service
  file described under Locker.
- A Nerd Font installed as **"Symbols Nerd Font"** (or edit the icon
  codepoints in `ui/bar.slint` to match a different one you have — see
  `fc-list | grep -i nerd`). Regular text uses "Space Grotesk" / "Space Mono".

## Installing

```sh
sudo make install     # builds --release and installs into /usr/bin
sudo make uninstall
```

- `PREFIX` (default `/usr`) controls where binaries go (`$(PREFIX)/bin`).
- `DESTDIR` is for staged/packaging builds, e.g.
  `make install DESTDIR=/tmp/pkg` — empty for a normal install onto the
  running system.
- Needs `make` and everything listed under Requirements above (cargo, the
  `-devel` packages, since `install` always builds first).
- Installs `helium-shell`, `helium-launcher`, `helium-session`, and
  `helium-osd` to `$(DESTDIR)$(PREFIX)/bin`. `helium-locker` is deliberately
  left out — see Locker above for installing it by hand.
- `helium-shell` needs autostarting by your compositor as usual; `helium-osd`
  additionally needs its own autostart entry (see OSD above) — the bar
  doesn't launch it for you.

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
- **Workspace click-to-switch talks to the compositor directly**, since
  Helium's `Compositor` trait has no dispatch/write API — `switch_workspace()`
  in `src/main.rs` branches on `$HYPRLAND_INSTANCE_SIGNATURE` /
  `$NIRI_SOCKET` and speaks each compositor's own IPC. For Hyprland it tries
  the standard textual IPC (`dispatch workspace N`) first, falling back to
  `dispatch hl.dsp.focus({ workspace = N })` for Lua-config builds (e.g.
  "hyprland-lua") that route dispatchers through Lua instead of the classic
  `<dispatcher> <args>` protocol. For niri it sends a `FocusWorkspace`
  action over `$NIRI_SOCKET`, using the workspace's own per-output `idx` as
  helium-wsl reports it — not necessarily the same numbering niri's own UI
  shows.
- Audio (beyond ALSA volume via `amixer`), power, and power-profiles services
  in helium-wsl itself are stubbed upstream and aren't used here.
- **`helium_wsl::Helium::on_key` is still a stub** (`// todo: waiting on
  layer-shika keyboard input API`) — it's a convenience callback for
  binding global shortcuts, unrelated to normal widget-level keyboard
  input. That's a real gap, but it doesn't mean keyboard input is missing
  wholesale: Slint's own `TextInput`/`FocusScope` receive real
  `wl_keyboard` events (via `layer-shika-adapters`, dispatched as
  `WindowEvent::KeyPressed`) on any layer-shell surface with keyboard
  focus, not just session-lock surfaces. `helium-launcher`'s search box
  and `ui/lock.slint`'s password field both rely on this directly. Building
  a binary on raw `layer_shika::Shell` instead of the `Helium` wrapper is
  what it takes to wire this up, since pushing filter/selection state back
  onto the surface from inside a Slint callback needs
  `ComponentInstance::as_weak()`, which the wrapper's `on_signal` doesn't
  expose access to.
