# Mouse

A lightweight macOS driver for the Logitech MX Master 3S, written in Rust.
No account, no network access, no telemetry, near-zero idle CPU.

I made Mouse for my own use and share it in case someone else finds it
useful.

## Features

- DPI and SmartShift threshold
- Button and gesture remapping (shortcuts, Mission Control, desktops, media keys)
- Thumb wheel modes, scroll direction, speed and smoothing
- Profiles with per-app overrides
- Menu bar app with a settings window, plus a command-line interface
- Bluetooth and Logi Bolt receiver
- English and Polish

## Resource use

Idle, measured with `scripts/idle.sh` over 60 s (MX Master 3S over
Bluetooth, Apple M3, macOS 27): below 0.1% CPU (0.02–0.07% across several
runs) and up to 0.05 wakeups per second. Results vary between machines.

## Install

1. Download the DMG from
   [Releases](https://github.com/zubrworks/mouseapp/releases), open it and
   drag Mouse.app to Applications.
2. Open Mouse. The app is not signed with an Apple Developer ID, so macOS
   blocks it: click **Done**, go to System Settings → Privacy & Security,
   scroll down and click **Open Anyway**, then confirm with your password.
   This is needed only once.
3. In System Settings → Privacy & Security → **Input Monitoring**, turn on
   **MouseAgent**.
4. In System Settings → Privacy & Security → **Accessibility** (on macOS 27:
   **Device Control and Data Access**), turn on **MouseAgent**.
5. Quit Logi Options+: only one program can control the mouse.

If the mouse does not respond after step 4, quit Mouse from the menu bar and
open it again.

To update, quit Mouse, replace Mouse.app in Applications and open it. The
new version replaces the old background driver by itself; your settings stay.

## Requirements

macOS 14 or later on Apple Silicon, Rust, Xcode Command Line Tools.

## Build

    scripts/bundle.sh
    open dist/Mouse.app

Then allow MouseAgent as in steps 3–5 of Install.

## License

GPL-3.0. If you share or modify this project, please link to the original:
https://github.com/zubrworks/mouseapp

Vibe-coded with Claude Code.

Not affiliated with or endorsed by Logitech. Logitech, Logi, MX Master and
Options+ are trademarks of Logitech.
