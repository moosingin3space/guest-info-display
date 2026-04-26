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

A modular Rust desktop application using [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui) (v0.2.2) for rendering.

### Module Structure

- `src/main.rs`: Entry point and UI orchestration. Manages the main event loop, GPUI tasks for clock/Spotify, and renders the top-level view.
- `src/spotify.rs`: Integration with Spotify Connect via `librespot`. Runs a background Tokio runtime for discovery and playback events, exposing a `SharedSpotifyState` (Mutex-protected) and an event channel to the UI.
- `src/persistence.rs`: SQLite-backed settings storage using `rusqlite` (bundled). Stores Wi-Fi credentials and the Spotify device ID.
- `src/qr_code.rs`: Wi-Fi QR code generation using `qrcodegen`. Renders QR codes directly to a GPUI `canvas`.
- `src/settings_dialog.rs`: A declarative GPUI dialog for configuring Wi-Fi credentials.

`build.rs` links against X11 system libraries (`xcb`, `xkbcommon`, `xkbcommon-x11`) via pkg-config to work around library paths issues on some Linux distributions.

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
  │   │   └── h_flex: 220px Cover Art (fetched asynchronously) + Song/Artist stack
  │   └── Right column (v_flex, 280px)
  │       ├── "Up Next" section header
  │       └── Queue item list (top 5 tracks, hydrated asynchronously)
  └── WiFi sidebar (v_flex, 320px, justify_between)
      ├── WiFi label + 220px QR code (generated from persistence)
      └── Settings gear button (bottom)
```

### State and Live Updates

- **Clock**: `GuestInfoDisplay` holds a `chrono::DateTime<Local>` updated every second via a `gpui::Timer` task.
- **Spotify**: Managed via `_spotify_task` in `main.rs`, which listens for events (StateChanged, CoverLoaded) from the Spotify thread. It updates the local `current_track`, `queue`, and `covers` (HashMap of `RenderImage`) and calls `cx.notify()`.
- **Persistence**: Wi-Fi credentials are re-read from the database when updated via the settings dialog.

### Visual Design

- Background: `linear_gradient(180°, #0a1033 → #3b1d6e)` (dark navy to deep purple).
- Cards/panels: `hsla(0,0,1,0.06)` fill with `hsla(0,0,1,0.12)` borders (translucent white).
- Muted text: `hsla(0,0,1,0.55)`.
- Typography: Uses `Adwaita Mono` for the clock if available.
