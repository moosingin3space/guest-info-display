# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo build          # debug build
cargo build --release  # release build
cargo run            # run the application
cargo test           # run tests
./scripts/make-flatpak.sh  # build Flatpak package
```

The Flatpak manifest is `xyz.mooshq.GuestInfoDisplay.json` targeting `org.freedesktop.Platform 25.08`.

## Architecture

A Rust desktop application using [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui) (v0.2.2) for rendering. The UI is composed declaratively using GPUI's `Render` trait and `div`/flex layouts, with `gpui-component` (v0.5.1) providing pre-built UI components like `TitleBar` and `Button`.

The entire application currently lives in `src/main.rs`. `build.rs` links against X11 system libraries (`xcb`, `xkbcommon`, `xkbcommon-x11`) via pkg-config to work around a library paths issue on some Linux OSes.

At runtime, GPUI drives the event loop — window creation and component initialization happen inside the `app.run()` closure. State is held in GPUI model types and accessed through `cx` (the GPUI context).

### UI Structure

```
TitleBar
Header (h_flex, justify_between)
  ├── Date label (left)
  └── 24-hour clock (right, Adwaita Mono font)
Body (h_flex, flex_1)
  ├── Spotify card (h_flex, flex_1)
  │   ├── Left column (v_flex, flex_1)
  │   │   ├── "Now Playing" section header
  │   │   └── h_flex: 220px cover art placeholder + song/artist stack
  │   └── Right column (v_flex, 280px)
  │       ├── "Up Next" section header
  │       └── Queue item list (title + artist rows)
  └── WiFi sidebar (v_flex, 320px, justify_between)
      ├── WiFi label + 220px QR code placeholder
      └── Settings gear button (bottom)
```

### State and live updates

`GuestInfoDisplay` holds a `chrono::DateTime<Local>` updated every second via a `gpui::Timer`-based loop spawned with `cx.spawn`. The task is stored as `_clock_task: Task<()>` on the struct so it lives for the entity's lifetime; each tick calls `cx.notify()` to trigger a re-render.

### Visual design

Background: `linear_gradient(180°, #0a1033 → #3b1d6e)` (dark navy to deep purple).  
Cards/panels use `hsla(0,0,1,0.06)` fill with `hsla(0,0,1,0.12)` borders (translucent white over the gradient).  
Muted text: `hsla(0,0,1,0.55)`.

### Planned features (not yet wired)

- **Spotify panel**: live playlist/queue data from the Spotify API (cover art image, real song/artist/queue).
- **WiFi sidebar**: rendered QR code for home Wi-Fi credentials.
- **Settings**: button at the bottom of the sidebar opens a settings panel.
