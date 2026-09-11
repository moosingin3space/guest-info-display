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

Skia comes from a prebuilt archive, never a source build: `freya-skia-bindings` is
pinned with `no-compile`, so a missing or mismatched prebuilt fails the build in
seconds. Online builds download it from `marc2332/rust-skia` releases; the Flatpak
build fetches it as a manifest source and points `SKIA_BINARIES_URL` at it. Bumping
Freya means bumping `freya-skia-bindings` and the manifest's URLs and checksums
together — see `plans/freya-skia-port.md`.

On hosts whose linker can't find `libstdc++`/EGL dev libraries, build inside the SDK:

```bash
flatpak run --user --devel --filesystem=$PWD --share=network \
  --env=CARGO_TARGET_DIR=$PWD/target/sdk --command=bash org.freedesktop.Sdk//25.08 \
  -c 'export PATH=/usr/lib/sdk/rust-stable/bin:$PATH; cargo build'
```

## Architecture

A modular Rust desktop application using [Freya](https://freyaui.dev/) (0.4.3, Skia + winit) for rendering.

### Module Structure

- `src/main.rs`: Entry point and UI orchestration. Owns the `Model`, starts the Freya tasks for clock/Spotify/pairing, and renders the top-level view.
- `src/spotify.rs`: Integration with Spotify Connect via `librespot`. Runs a background Tokio runtime for discovery and playback events, exposing a `SharedSpotifyState` (Mutex-protected) and an event channel to the UI.
- `src/multi_screen.rs`: iroh-based pairing and state mirroring between a primary and its reflections.
- `src/persistence.rs`: SQLite-backed settings storage using `rusqlite` (bundled). Stores Wi-Fi credentials, role, audio device, paired peers and the iroh identity.
- `src/qr_code.rs`: Wi-Fi QR code generation using `qrcodegen`, drawn with Skia calls in a Freya `canvas`.
- `src/settings_dialog.rs`: The settings form, rendered inside a Freya `Popup`.
- `src/inhibitor.rs`: XDG idle-inhibit portal while playback is active.

### UI Structure

```
Window (native decorations, toggled by the "Hide titlebar" setting)
Header (horizontal, space-between)
  ├── Date label (left)
  └── 24-hour clock (right, Adwaita Mono font)
Body (horizontal, flex 1)
  ├── Spotify card (horizontal, flex 1)
  │   ├── Left column (flex 1)
  │   │   ├── "Now Playing" section header
  │   │   └── 220px Cover Art (ImageViewer over encoded bytes) + Song/Artist stack
  │   └── Right column (280px)
  │       ├── "Up Next" section header
  │       └── Queue item list (top 5 tracks)
  └── WiFi sidebar (320px, space-between)
      ├── WiFi label + 220px QR code (generated from persistence)
      └── Settings gear button (bottom)
Footer: role · short endpoint id
Popups: pairing approval, settings
```

### State and Live Updates

- **Model**: the root component holds a single `State<Model>`. Event handlers and tasks mutate it with `model.write()`, which re-renders readers. Tasks that must outlive the component that started them (backend listener, pairing) use `spawn_forever`.
- **Clock**: a separate `State<DateTime<Local>>` updated every second by a Freya task sleeping on `tokio::time` (`main` enters a small Tokio runtime for this).
- **Spotify**: `rebuild_backend` starts the primary (librespot) or reflection backend and a listener task forwarding its events (StateChanged, CoverLoaded, …) into the model. Covers are kept as encoded bytes keyed by URL.
- **UI scale**: `use_ui_zoom` sets Freya's per-window zoom from the physical window width (`DESIGN_WIDTH` 1280, capped at 1.75), so layout code uses plain design sizes.
- **zbus**: `ashpd` must stay on its `async-io` feature. Its default `tokio` feature switches zbus to Tokio process-wide and Freya's own zbus threads panic.

### Visual Design

- Background: linear gradient `#0a1033 → #3b1d6e` top to bottom (Freya angle `0°`; its angles run opposite to CSS).
- Cards/panels: white at alpha 15 fill with alpha 31 borders (translucent white).
- Muted text: white at alpha 140.
- Typography: Uses `Adwaita Mono` for the clock if available.
- Components use Freya's `dark_theme()`.
