<p align="center">
  <img src="assets/logo-256.png" alt="helium-shell logo" width="128">
</p>

<h1 align="center">helium-shell</h1>

<p align="center">
  A clean, lightweight status bar and desktop toolkit for Wayland Linux desktops.
</p>

<p align="center">
  <a href="Cargo.toml"><img src="https://img.shields.io/badge/rust-2021-orange?logo=rust" alt="Rust"></a>
  <a href="https://github.com/zepyxunderscore/helium-wsl"><img src="https://img.shields.io/badge/wayland-layer--shell-blue?logo=wayland" alt="Wayland"></a>
  <a href="https://hyprland.org"><img src="https://img.shields.io/badge/compositor-Hyprland-00b6b6" alt="Hyprland"></a>
  <a href="https://github.com/YaLTeR/niri"><img src="https://img.shields.io/badge/compositor-niri-blueviolet" alt="niri"></a>
  <a href="https://swaywm.org"><img src="https://img.shields.io/badge/compositor-Sway-88c0d0" alt="Sway"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-green" alt="License: MIT"></a>
</p>

helium-shell puts a slim, rounded status bar at the top of your screen, plus a
handful of small companion tools for everyday desktop tasks — launching apps,
picking a wallpaper, chatting with a local AI model, and locking/shutting down
your machine. It's built for Hyprland, niri, and Sway, and aims to look and
feel consistent across all three.

## What you get

- **A status bar** at the top of the screen showing your workspaces, the
  weather, the date and time, your network status, CPU/RAM usage, battery
  level, and volume — all at a glance.
- **An app launcher** — press a key (or click the bar) to search and open any
  installed application.
- **A wallpaper picker** — browse your wallpaper folder as a grid of
  thumbnails and click one to apply it.
- **An AI chat popup** for a locally running [Ollama](https://ollama.com)
  model — ask it something without opening a terminal.
- **A session menu** — lock, suspend, reboot, shut down, or log out from one
  place.
- **A screen locker** with a clock and password field (currently only
  recommended on Hyprland/Sway — see [Known limitations](#known-limitations)).
- **An on-screen display** that briefly pops up when you change the volume or
  power profile.

## Screenshot tour

```
[apps] [AI] [ 1 2 3 4 5 ]        Clear +20°C · 14:32        Wi-Fi  92%  |  CPU 12%  |  RAM 41%  |  🔋 87%  |  🔊 60%  [⏻]
```

From left to right: app launcher, AI chat, your workspaces, the weather and
clock centered on the bar, then network, CPU, RAM, battery, and volume — with
the power/session menu on the far right.

## Installing

1. Make sure you have Rust and a few system libraries installed — see
   [Requirements](#requirements) below.
2. Build and install everything with:

   ```sh
   sudo make install
   ```

   This installs the bar and its companion tools to `/usr/bin`. To remove
   everything later:

   ```sh
   sudo make uninstall
   ```

3. Tell your compositor to start the bar automatically. Add this to your
   config:

   **Hyprland** (`hyprland.conf`):
   ```
   exec-once = /usr/bin/helium-shell
   ```

   **niri** (`config.kdl`):
   ```
   spawn-at-startup "/usr/bin/helium-shell"
   ```

   **Sway** (`config`):
   ```
   exec /usr/bin/helium-shell
   ```

That's it — restart your compositor session (or just run the command once
yourself) and the bar should appear.

### Optional pieces

A couple of the companion tools are handy to start automatically as well, and
a couple more are opened on demand — you don't need to do anything extra for
those:

| Tool | Starts... |
|---|---|
| `helium-shell` (the bar) | automatically, add to your compositor config (above) |
| `helium-osd` (volume/brightness popup) | automatically — add its own `exec-once` line, same as the bar |
| `helium-backdrop` (background shown when no wallpaper is set) | automatically — same as above, optional |
| `helium-launcher`, `helium-wallpaper`, `helium-ollama`, `helium-session` | on demand — opened by clicking their icon in the bar, or a keybind (see [Keyboard shortcuts](#keyboard-shortcuts)) |

## Keyboard shortcuts

None of these are bound automatically — add the ones you want to your
compositor config. Example for Hyprland:

```
bind = SUPER, D, exec, /usr/bin/helium-launcher   # app launcher
bind = SUPER, A, exec, /usr/bin/helium-ollama     # AI chat
bind = SUPER, W, exec, /usr/bin/helium-wallpaper  # wallpaper picker
```

Once a popup is open:
- **Type** to search (launcher).
- **Arrow keys** to move the selection.
- **Enter** to confirm, **Escape** to close.

## Requirements

- A Wayland session on **Hyprland**, **niri**, or **Sway** (the bar runs
  elsewhere too, just without workspace switching).
- A [Nerd Font](https://www.nerdfonts.com) installed — specifically
  **"Symbols Nerd Font"** — so the bar's icons render correctly.
- `amixer` (comes with `alsa-utils`) for volume control.
- `NetworkManager` and a terminal app (`foot`, `kitty`, `alacritty`,
  `wezterm`, or `xterm`) for the network status/click-to-connect.
- `curl` and an internet connection for the weather segment.
- `brightnessctl` and `power-profiles-daemon`, if you want the on-screen
  display to show brightness and power-profile changes.
- A [wallpaper backend](#) already running if you want the wallpaper picker
  to actually apply one — `hyprpaper` (Hyprland), or `swww`/`swaybg`/`feh`
  elsewhere.
- For the AI chat popup: [Ollama](https://ollama.com) installed and running
  locally.

Don't worry if you're missing one of these — the bar still runs, that one
feature just won't do anything until the dependency is installed.

## Customizing

- **Colors and fonts** live in `ui/bar.slint` if you want to tweak the look.
- **Wallpaper folder**: the picker looks in `~/Wallpaper` by default — set
  the `HELIUM_WALLPAPER_DIR` environment variable to point it somewhere else.
- The bar's width adjusts itself automatically to your screen, so there's
  nothing to configure per machine.

## Known limitations

- The screen locker (`helium-locker`) doesn't currently work on **niri** —
  the lock screen appears but niri cancels it almost immediately. It works
  fine on Hyprland and Sway. It's also not installed by `make install` yet;
  see the technical docs for how to build it by hand.
- Bluetooth controls aren't included in the bar.

## Want more detail?

This README covers everyday use. For build internals, architecture notes,
and the reasoning behind specific design choices, see
[`doc/TECHNICAL.md`](doc/TECHNICAL.md).

## License

MIT — see [LICENSE](LICENSE).
