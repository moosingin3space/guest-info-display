# Guest Info Display

A sleek, modern Linux desktop dashboard designed for guest rooms, home offices, or common areas. Built with Rust and the [Freya](https://freyaui.dev/) framework (Skia + winit), it provides essential information and media control to visitors at a glance.

![Screenshot of Guest Info Display](data/xyz.mooshq.GuestInfoDisplay.svg) *(Note: Placeholder icon)*

## Features

- **Dynamic Clock & Date**: A prominent, real-time display of the current time and date.
- **WiFi Guest Access**: Generates a scan-to-connect QR code for your guest WiFi. Credentials can be updated via the built-in settings and are persisted in a local database.
- **Spotify Connect Dashboard**: Acts as a Spotify Connect target ("Guest Info Display"). Shows "Now Playing" metadata, live cover art fetching, and the upcoming track queue.
- **Glassmorphism UI**: A beautiful, translucent interface with deep gradients, optimized for high-resolution displays and kiosks.
- **Configurable Layouts**: The whole screen is described by a KDL file you can edit in any text editor. Reloads live, and mirrors to paired displays.
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

A plain `cargo build` works wherever the linker can find `libstdc++`, EGL/GL,
Wayland and fontconfig development libraries.

```bash
# Install dependencies (Example for Fedora/RHEL)
sudo dnf install libstdc++-devel mesa-libEGL-devel wayland-devel fontconfig-devel

# Run the application
cargo run
```

Otherwise use the `Justfile`, which runs the same commands inside the Flatpak
SDK — `just build`, `just test`, `just run`. Run `just` on its own to list the
rest.

## Configuration

- **WiFi**: Click the **Settings (Gear)** icon in the bottom right of the WiFi sidebar to configure your network credentials.
- **Spotify**: The application appears as "Guest Info Display" in your Spotify app's device list. It uses a persistent device ID to maintain its identity across restarts.

Settings are persisted in a local SQLite database at `~/.local/share/xyz.mooshq.GuestInfoDisplay/guest-info-display.db`.

## Layouts

Everything on screen — the widgets, where they sit, the colours, the fonts —
comes from a single KDL document. There is no second code path: the built-in
look is itself a layout file, parsed through the same parser as yours.

### Getting one onto the display

Open **Settings → Layout**. `Copy default to config` writes the built-in layout
out as a starting point; `Import folder…` opens a chooser so you can point at a
folder on a USB stick without ever learning where the config directory is.

Put the `.kdl` and the images it references in one folder and pick that folder.
The app reads the document first and copies in only the images it actually
names, keeping any subdirectories — so a folder with a hundred holiday photos
and a layout that uses two of them costs you two files, not a hundred. Exactly
one `.kdl` per folder: with none there is nothing to import, and with several
the import is refused rather than guessing which party you meant.

The file itself lives at `~/.config/xyz.mooshq.GuestInfoDisplay/layout.kdl`.
Under Flatpak that becomes
`~/.var/app/xyz.mooshq.GuestInfoDisplay/config/xyz.mooshq.GuestInfoDisplay/layout.kdl`
— the app id really does appear twice, once for the sandbox's config root and
once for our directory inside it. Edits are picked up within a couple of
seconds, with no restart.

**A broken file cannot break a running display.** A document that fails to
parse leaves whatever is on screen exactly where it is and reports the problem
in Settings with a line number. Smaller mistakes are narrower still: an unknown
widget or property skips that one node and renders everything around it.

Worked examples live in [`assets/examples/`](assets/examples/) — a photo-led
wedding screen, a quiet guest-room panel, and an artwork-forward music screen.

### The shape of a document

```kdl
theme {
    background {
        gradient to="bottom" {
            stop "#0a1033" 0
            stop "#3b1d6e" 100
        }
    }
    text       "#ffffff"
    muted-text "#ffffff8c"
}

root {
    row width="fill" padding="20 32" main-align="space-between" {
        date font-size=24
        clock font-size=30 weight="bold" mono=#true
    }

    card flex=1 padding=24 {
        now-playing cover-size=220
    }
}
```

Both blocks are optional. A document with only a `theme` re-colours the
built-in arrangement, which is the cheapest useful edit there is.

### Widgets

| Node | Arguments and properties |
| --- | --- |
| `column` `row` `card` | Containers. `card` draws the translucent surface; `column` and `card` take `direction="row"` to flip. |
| `spacer` | Eats the leftover space. Usually `flex=1`. |
| `clock` | `format` (strftime, default `%H:%M:%S`) |
| `date` | `format` (default `%A, %B %-d`) |
| `now-playing` | `heading`, `cover-size` (220), `show-artist` |
| `up-next` | `heading`, `count` (5) |
| `cover-art` | `size` — the artwork alone, with no text |
| `wifi-qr` | `heading`, `size` (220) |
| `text` | The string to show, as the first argument |
| `image` | Path as the first argument, `fit="cover"` or `"contain"` |
| `role-label` | This device's role and short id |
| `settings-button` | `size` (24) |

### Styling

Every widget accepts `width`, `height` (a number, `"fill"` or `"auto"`),
`flex`, `padding` (one number, or a string of 1, 2 or 4), `spacing`,
`main-align` and `cross-align` (`start` `center` `end` `space-between`
`space-around`).

Anything that draws text also takes `font-size`, `weight` (`normal` `semibold`
`bold`), `color`, `align` (`start` `center` `end`) and `mono`.

`theme` takes `background` (a colour, or a `gradient` with `to=` and `stop`
entries), `surface`, `surface-border`, `placeholder`, `text`, `muted-text`,
`font-family` and `mono-font-family`. Font families have to be installed in the
runtime already; an unavailable one falls back silently.

### Showing things conditionally

Any node takes `when=`, and renders only in that state: `playing`, `idle`,
`connected`, `disconnected`, `wifi-configured`, `wifi-unconfigured`, or
`always`. One document can therefore be a photo frame while the music is off
and a player while it is on.

### Images

`image` takes a path relative to the layout file, which must stay inside the
config directory — `..` and absolute paths are refused. Subdirectories are fine:
`image "photos/ana.jpg"` imports and renders as written.

An image that is not there renders a placeholder naming the file rather than
failing the document, and Settings lists it with a **Locate…** button that opens
a chooser and files your pick under the name the layout expects. That is the way
to fix one image later without re-importing the whole folder.

### Multiple displays

A **primary** owns the layout. Pair a **reflection** to it and the document is
pushed across, with its images, and re-pushed on every edit — so re-theming a
party means editing one file on one machine.

A reflection ignores its own `layout.kdl` entirely, and remembers the last one
it was sent, so a display that reboots before its primary is up still comes
back in the party's colours instead of flashing the built-in default.

What does *not* mirror is anything local to a device: each screen shows its own
clock, its own role label, and — importantly — its own Wi-Fi QR code. Two
screens in two rooms on two networks show two different codes from one layout
file.

> Primary and reflection negotiate a protocol version on connect. Upgrade both
> ends together; a reflection on an older build will report "Primary
> unavailable" rather than mis-render.

## Tech Stack

- **[Rust](https://www.rust-lang.org/)**: Core application logic.
- **[Freya](https://freyaui.dev/)**: GPU-accelerated UI rendering, on Skia and winit.
- **[KDL](https://kdl.dev/)**: The layout document format.
- **[iroh](https://www.iroh.computer/)**: Peer-to-peer mirroring between displays.
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
