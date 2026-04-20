# Guest Info Display

A sleek, modern Linux desktop dashboard designed for guest rooms, home offices, or common areas. Built with Rust and the high-performance [GPUI](https://github.com/zed-industries/zed) framework, it provides essential information to visitors at a glance.

![Screenshot of Guest Info Display](data/xyz.mooshq.GuestInfoDisplay.svg) *(Note: Placeholder icon)*

## Features

- **Dynamic Clock & Date**: A prominent, real-time display of the current time and date.
- **WiFi Guest Access**: Generates a scan-to-connect QR code for your guest WiFi. Credentials are securely stored locally and can be updated via the built-in settings.
- **Media Dashboard (In Progress)**: A dedicated "Now Playing" area designed for Spotify integration, showing current tracks and the upcoming queue.
- **Glassmorphism UI**: A beautiful, translucent interface with deep gradients, optimized for high-resolution displays and kiosks.

## Installation

### Flatpak (Recommended)

The preferred way to install Guest Info Display is via Flatpak, which ensures all necessary system libraries are bundled and provides a secure sandbox.

To build and install the Flatpak locally:

1. Ensure you have `flatpak` and `flatpak-builder` installed.
2. Run the build script:
   ```bash
   ./scripts/make-flatpak.sh
   ```

### Building from Source

If you prefer to build locally without Flatpak, you will need the Rust toolchain and several system development libraries (`xcb`, `xkbcommon`, `xkbcommon-x11`).

```bash
# Install dependencies (Example for Fedora/RHEL)
sudo dnf install libxcb-devel libxkbcommon-devel libxkbcommon-x11-devel

# Run the application
cargo run
```

## Configuration

Click the **Settings (Gear)** icon in the bottom right of the WiFi sidebar to configure your network credentials.
- **SSID**: Your WiFi network name.
- **Password**: Your WiFi password.
- **Security**: Supports WPA (default) or open networks.

Settings are persisted in a local SQLite database located at `~/.local/share/xyz.mooshq.GuestInfoDisplay/guest-info-display.db`.

## Tech Stack

- **[Rust](https://www.rust-lang.org/)**: For performance and safety.
- **[GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui)**: A GPU-accelerated UI framework.
- **[SQLite](https://sqlite.org/)**: For lightweight, local data persistence.
- **[Flatpak](https://flatpak.org/)**: For cross-distribution packaging and sandboxing.

## License

Dual MIT/Apache-2.0.
