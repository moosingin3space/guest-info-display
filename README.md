# Guest Info Display

A sleek, modern Linux desktop dashboard designed for guest rooms, home offices, or common areas. Built with Rust and the high-performance [GPUI](https://github.com/zed-industries/zed) framework, it provides essential information and media control to visitors at a glance.

![Screenshot of Guest Info Display](data/xyz.mooshq.GuestInfoDisplay.svg) *(Note: Placeholder icon)*

## Features

- **Dynamic Clock & Date**: A prominent, real-time display of the current time and date.
- **WiFi Guest Access**: Generates a scan-to-connect QR code for your guest WiFi. Credentials can be updated via the built-in settings and are persisted in a local database.
- **Spotify Connect Dashboard**: Acts as a Spotify Connect target ("Guest Info Display"). Shows "Now Playing" metadata, live cover art fetching, and the upcoming track queue.
- **Glassmorphism UI**: A beautiful, translucent interface with deep gradients, optimized for high-resolution displays and kiosks.
- **Offline-First Packaging**: Distributed as a Flatpak for cross-distro compatibility and security.

## Installation

### Flatpak (Recommended)

The preferred way to install Guest Info Display is via Flatpak. This bundles all necessary system libraries and provides a secure sandbox.

Builds are published to a Flatpak repository at
<https://moosingin3space.github.io/guest-info-display/>:

```bash
# Latest release
flatpak install --user https://moosingin3space.github.io/guest-info-display/xyz.mooshq.GuestInfoDisplay.flatpakref
# Latest build of main, installable alongside the release
flatpak install --user https://moosingin3space.github.io/guest-info-display/xyz.mooshq.GuestInfoDisplay.Devel.flatpakref
```

To build and install the Flatpak locally:

1. Ensure you have `flatpak` and `flatpak-builder` installed.
2. Run the build script:
   ```bash
   ./scripts/make-flatpak.sh
   ```

The build script generates the temporary Cargo source manifest from
`Cargo.lock` before invoking Flatpak Builder. It uses `uv` when available,
or the `flatpak-cargo-generator` command from `org.flatpak.Builder`.

### Building from Source

If you prefer to build locally, you will need the Rust toolchain and several system development libraries (`xcb`, `xkbcommon`, `xkbcommon-x11`).

```bash
# Install dependencies (Example for Fedora/RHEL)
sudo dnf install libxcb-devel libxkbcommon-devel libxkbcommon-x11-devel

# Run the application
cargo run
```

## Configuration

- **WiFi**: Click the **Settings (Gear)** icon in the bottom right of the WiFi sidebar to configure your network credentials.
- **Spotify**: The application appears as "Guest Info Display" in your Spotify app's device list. It uses a persistent device ID to maintain its identity across restarts.

Settings are persisted in a local SQLite database at `~/.local/share/xyz.mooshq.GuestInfoDisplay/guest-info-display.db`.

## Tech Stack

- **[Rust](https://www.rust-lang.org/)**: Core application logic.
- **[GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui)**: GPU-accelerated UI rendering.
- **[librespot](https://github.com/librespot-org/librespot)**: Spotify Connect integration.
- **[SQLite](https://sqlite.org/)**: Local data persistence.
- **[Flatpak](https://flatpak.org/)**: Distribution and sandboxing.

## Releasing

Every push to `main` publishes `xyz.mooshq.GuestInfoDisplay.Devel`. To publish a release:

1. Set `version` in `Cargo.toml` and add a matching `<release>` to
   `data/xyz.mooshq.GuestInfoDisplay.metainfo.xml`.
2. Tag the commit `vX.Y.Z` and push the tag. CI refuses a tag that doesn't match both,
   attaches the bundle to a GitHub Release, and publishes it to the stable branch.

One-time repository setup:

1. `just repo-key`, then store the key with the `gh secret set` command it prints.
2. In **Settings → Pages**, set the source to **GitHub Actions**.
3. In **Settings → Environments → github-pages**, allow deployments from tags matching `v*`
   as well as `main`.

## License

Dual MIT/Apache-2.0.
