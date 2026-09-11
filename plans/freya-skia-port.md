# Evaluation: porting the UI from GPUI to Freya

Status: **evaluation only — not committed.** This document describes what
Freya and Skia are, what a port would touch, and the one build-system
problem that has to be solved before the port is worth starting. It is not
a ticketed build sequence; if we decide to go ahead, that gets written
separately in the style of `multi-screen-and-playback.md`.

Researched 2026-09-07 against freya 0.4.3 / 0.5.0-rc.5.
**Spike run 2026-09-10 — x86_64 passes; see [Spike results](#spike-results).**
Several version details below were corrected by the spike.

## Why consider it at all

Not maturity — GPUI is perfectly capable and is battle-tested inside Zed.
The argument is **API surface intent**. GPUI is Zed's internal rendering
engine that happens to be published; its API changes to suit Zed, its
docs are the source, and `gpui-component` is a third-party layer filling
the gap where a widget library should be. Freya is built as a GUI library
*for external consumers*: semver'd releases, published guides, a
first-party components crate, and a changelog written for downstream
users.

That is a real difference in maintenance posture, and it is the whole
reason to look at this. "Freya is more mature" would be false — it just
rewrote its entire core in 0.4 and dropped Dioxus for a bespoke reactive
model. We would be trading a stable-but-inward-facing dependency for a
younger-but-outward-facing one.

## What Freya is

A cross-platform, non-web Rust GUI library rendering through Skia, with
winit for windowing and GL / Vulkan / softbuffer backends.

Relevant version history:

| Version | Date | Notes |
| --- | --- | --- |
| 0.4.0 | 2026-07-16 | Core rewrite: Dioxus dropped for own reactive core |
| 0.4.3 | 2026-08-30 | Current stable — **the version to target** |
| 0.5.0-rc.5 | 2026-09-07 | Mid-RC; would move under us during a port |

0.4 replaced the `rsx!` macro with a typed builder API returning
`impl IntoElement`, and `use_signal` with `use_state`. The builder style
is structurally close to what we already write in GPUI:

```rust
// GPUI (today)                      // Freya 0.4
div().size_full()                    rect().expanded()
    .flex().items_center()               .center()
    .bg(rgb(0x0a1033))                   .background((10, 16, 51))
    .child("Guest Info Display")         .child("Guest Info Display")
```

So the layout port is closer to transliteration than redesign.

Confirmed available and relevant to us: `canvas()` with direct Skia
access (for the Wi-Fi QR), custom font embedding via
`LaunchConfig::with_font` (for Adwaita Mono), linear gradients, `image`
with a `cache_key` for runtime-fetched bytes (cover art), multi-window,
`WindowConfig` decorations (for the hide-titlebar setting), and
Button / Input / Radio / Switch / Tooltip / Menu components.

Async: Freya runs its own reactive runtime. Tokio-ecosystem crates
(librespot, iroh, ashpd — i.e. all of our backend) work, but `main` must
enter a Tokio runtime context, and UI-touching tasks must use Freya's
`spawn()` rather than `tokio::spawn` — only the former can write state.

## Skia, and the build problem

Skia is a large C++ graphics library. Compiling it from source needs
python3, clang, ninja and a multi-GB checkout, and takes tens of minutes.
Doing that inside `flatpak-builder`, which runs the build phase with **no
network**, is the single thing that could sink this port. It is a
well-known pain point — LibreOffice's Flatpak disables Skia for
essentially this reason.

`skia-bindings` avoids the source build by downloading a prebuilt archive
matching the repo hash, target triple, and enabled features. `binary-cache`
is one of its default features. The decision tree is roughly:

```
SKIA_SOURCE_DIR set?        → build from that source tree
FORCE_SKIA_BUILD set?       → build from source
binary-cache enabled?       → try download; on success, import and skip the build
                            → on failure, fall back to a source build
                              (unless FORCE_SKIA_BINARIES_DOWNLOAD, which panics)
```

### The fork

**Freya does not depend on upstream `skia-safe`.** It depends on
`freya-skia-safe`, a fork maintained at `github.com/marc2332/rust-skia`.

This matters twice. Upstream's `rust-skia/skia-binaries` releases will
*not* match, so the obvious place to look is the wrong one. But the fork
publishes its own binaries — and builds exactly the feature combination
Freya needs, which upstream declined to add (see rust-skia discussion
#961: the maintainer pushed back on new feature combos, so Freya started
publishing its own).

Release tags on the fork equal the `freya-skia-safe` version:

| Freya | freya-skia-safe | binaries tag |
| --- | --- | --- |
| 0.4.3 | 0.98.1 (resolved from `^0.98.0`) | `0.98.1` |
| 0.5.0-rc | 0.100.0 | `0.100.0` |

`freya-engine` 0.4.3 requires `freya-skia-safe ^0.98.0`, a caret range, so
a fresh lockfile resolves 0.98.1, whose binaries have a different hash
from `0.98.0`. `freya-skia-safe 0.98.1` pins `freya-skia-bindings =0.98.1`.

Both Linux architectures we care about are covered — including
**aarch64**, which is what the Pi-class panels run:

```
skia-binaries-<hash>-x86_64-unknown-linux-gnu-egl-gl-jpegd-jpege-svg-\
  textlayout-vulkan-wayland-webpd-webpe-x11.tar.gz
skia-binaries-<hash>-aarch64-unknown-linux-gnu-egl-gl-jpegd-jpege-svg-\
  textlayout-vulkan-wayland-webpd-webpe-x11.tar.gz
```

~17 MiB each. For tag `0.98.1` the hash is `b5756d8613bf27909a64`
(`0.98.0` was `a9bd25883c31d7ac2b2b`).

**Conclusion: no source build is needed.** The Flatpak sandbox does not
need python3, ninja, clang, or a Skia checkout.

### Wiring it into the Flatpak manifest

`skia-bindings` accepts `SKIA_BINARIES_URL` pointing at a local file.
flatpak-builder fetches sources with network on and builds with network
off, which is exactly the right shape. Patch for
`xyz.mooshq.GuestInfoDisplay.json`:

```json
"build-options": {
  "append-path": "/usr/lib/sdk/rust-stable/bin",
  "env": {
    "CARGO_HOME": "/run/build/app/cargo",
    "CARGO_NET_OFFLINE": "true",
    "SKIA_BINARIES_URL": "file:///run/build/app/skia-binaries.tar.gz"
  }
},
"sources": [
  { "type": "dir", "path": "./" },
  "generated-sources.json",
  {
    "type": "file",
    "only-arches": ["x86_64"],
    "url": "https://github.com/marc2332/rust-skia/releases/download/0.98.1/skia-binaries-b5756d8613bf27909a64-x86_64-unknown-linux-gnu-egl-gl-jpegd-jpege-svg-textlayout-vulkan-wayland-webpd-webpe-x11.tar.gz",
    "sha256": "6a70402b54dac8647a0b4f914ff437298a55eb8de0d6c6357521296817e44dc6",
    "dest-filename": "skia-binaries.tar.gz"
  },
  {
    "type": "file",
    "only-arches": ["aarch64"],
    "url": "https://github.com/marc2332/rust-skia/releases/download/0.98.1/skia-binaries-b5756d8613bf27909a64-aarch64-unknown-linux-gnu-egl-gl-jpegd-jpege-svg-textlayout-vulkan-wayland-webpd-webpe-x11.tar.gz",
    "sha256": "103e1d19d7de6f58736f7e2aea091efb6264f26e1ed38205746724ab9002a84e",
    "dest-filename": "skia-binaries.tar.gz"
  }
]
```

`only-arches` with a shared `dest-filename` lets one env var serve both
targets. Checksums above were computed from downloaded archives, not
copied from a listing.

### The trap

Each archive carries a `key.txt` that the build script checks:

```
b5756d8613bf27909a64-x86_64-unknown-linux-gnu-egl-gl-jpegd-jpege-svg-textlayout-vulkan-wayland-webpd-webpe-x11
```

If our enabled feature set computes a different key, the archive is
rejected and the build **silently falls back to compiling Skia from
source** — which, offline, means a confusing failure after a long wait.
Two defenses:

1. Stay on Freya's default `winit` renderer features. The published combo
   was built for exactly that.
2. Enable the `no-compile` feature on `skia-bindings`, which panics
   instead of compiling. Turn this on from day one so a key mismatch
   fails in seconds with a clear message rather than after a 40-minute
   detour.

Also note the tag tracks the `freya-skia-bindings` version, and pinning
Freya alone does **not** pin that: Freya's own caret range on
`freya-skia-safe` lets `cargo update` move it. Pin both exactly, and enable
`no-compile` from the same entry, with default features off so the entry
adds nothing to the key:

```toml
freya = "=0.4.3"
freya-skia-bindings = { version = "=0.98.1", default-features = false, features = ["no-compile"] }
```

## What the port touches

| Module | Lines | GPUI coupling |
| --- | --- | --- |
| `src/main.rs` | 956 | Total — view, state, task orchestration |
| `src/settings_dialog.rs` | 407 | Total — `open_dialog`, Input/Select/Switch/Radio |
| `src/qr_code.rs` | 82 | Total — `canvas` + `paint_quad` |
| `src/spotify.rs` | 795 | One field: `CoverImage.bgra` (`src/spotify.rs:54`) |
| `src/multi_screen.rs` | 475 | None |
| `src/persistence.rs` | 430 | None |
| `src/inhibitor.rs` | 85 | None |
| `src/audio_devices.rs` | 16 | None |

~1,445 lines rewritten, ~1,800 untouched. The backend is already cleanly
separated behind `SharedSpotifyState` plus an `async_channel` of events —
exactly the shape Freya wants — so the multi-screen, persistence,
inhibitor and audio work all survives intact.

### Mapping

| Today | Freya equivalent |
| --- | --- |
| `canvas` + `paint_quad` (QR) | `canvas()` → real Skia `draw_rect` / `Path` |
| `RenderImage` from BGRA | `ImageViewer::new((url, bytes))` (¹) |
| `linear_gradient` | Freya gradients |
| `font_family("Adwaita Mono")` | `LaunchConfig::with_font` |
| `gpui_component::Button` etc. | `freya::components` equivalents |
| `window.open_dialog` | hand-rolled overlay (we already do this in `approval_overlay`) |
| `IconName::Settings` | ship our own SVG — Freya has no bundled icon set |
| `TitleBar` + `hide_titlebar` | `WindowConfig` decorations |
| `window.set_rem_size(...)` | **no equivalent — see below** |

Two things get *easier*: the QR becomes real Skia drawing calls, and
cover art can feed `image` the encoded bytes `spotify.rs` already keeps
for multi-screen broadcast, deleting the manual BGRA decode entirely.

¹ Verified in the freya-components 0.4.3 source (`src/image_viewer.rs`).
`static_bytes` / `cache_key` is the 0.3 API. In 0.4, `image()` takes an
`ImageHandle`, and runtime bytes go through
`ImageSource: From<(impl Hash, Bytes)>`. The hashed id serves as the cache
key, and decoding runs off the UI thread (at most 4 in parallel).

## Remaining risks

1. **UI scaling has no rem indirection.** `ui_scale()` (`src/main.rs:333`)
   works by setting `rem_size` so every `text_2xl` / `px_8` / `gap_6`
   scales for free. Freya sizes are plain typed values, so every spacing
   and font size becomes an explicit `× scale`. Mechanical, but it
   touches every layout line and it is easy to miss one.
2. **No dialog primitive.** The settings dialog's title/footer chrome is
   built by hand.
3. **Runtime decoration toggling.** `hide_titlebar` maps to
   `WindowConfig`, but decorations are set at window creation; toggling
   live may need a window relaunch. Unverified.
4. **Young API.** 0.4 rewrote the core. Expect churn, and expect to be an
   early enough adopter that some bugs are ours to report.

## Estimate

| Work | Days |
| --- | --- |
| App shell, window config, Tokio interop | 0.5–1 |
| Main view layout | 1.5–2 |
| Settings dialog + form widgets + pairing sections | 1.5–2 |
| QR canvas | 0.5 |
| Cover-art pipeline + event plumbing | 1 |
| UI-scale rework | 0.5 |
| Flatpak + Skia binaries | 0.5 |
| Hardware regression (Pi panels, multi-screen, X11 + Wayland) | 1–2 |

**~7–9 days focused.** The Skia line was originally estimated at 1–3 days
and was the dominant source of variance; the prebuilt-binaries finding
retires it and tightens the whole range.

## Cut points

- **Spike only** (~0.5 day): empty Freya app + the manifest patch above,
  built for both arches. Retires the entire build-system risk for a
  fraction of the cost. **Do this before committing to anything else.**
- **Main view only** (~4 days): port the display, keep GPUI for the
  settings dialog. Not actually possible — two toolkits cannot share a
  window — but useful as a checkpoint measure.
- **Full port**: ~7–9 days focused, ~3 weeks part-time.

Calendar estimates assume part-time work, consistent with
`multi-screen-and-playback.md`; halve for focused full-time effort.

## Spike results

Run 2026-09-10. Code lives in `spike/freya-hello/`: a 40-line app with its
own `Cargo.toml`, `generated-sources.json` and `xyz.mooshq.FreyaSpike.json`,
using the manifest patch above.

**x86_64: pass.**

- `flatpak-builder` (25.08 SDK, rust-stable) built offline in ~60 s,
  including fetch. Cargo itself took 25 s after the first run.
  `freya-skia-bindings` built with `no-compile` against the local prebuilt,
  so the key matched, and no gn, ninja or python3 ran.
- The built app ran from the build dir under waypuppet (headless Wayland,
  `flatpak build --socket=wayland --device=dri`). Text rendered with the
  runtime's fonts, the background was correct, and winit drew client-side
  decorations.
- A task started with Freya's `spawn()`, sleeping on `tokio::time::sleep`
  inside an entered Tokio runtime, updated `use_state`. The on-screen
  counter advanced between captures.

**aarch64: not run.** There's no qemu binfmt on the dev machine, and it was
deliberately left out of the spike. The prebuilt exists and its checksum
is in the manifest above, but it has not been built against. Before
committing to the port, either build on an arm64 CI runner or register
binfmt and run `flatpak-builder --arch=aarch64`.

Incidental findings:

- The plan's original `0.98.0` URLs would have been wrong. See the version
  table.
- A host (non-Flatpak) `cargo build` on this workstation fails at the link
  step: `rust-lld: unable to find library -lstdc++`. The image ships only
  `libstdc++.so.6` with no dev symlink, and pkg-config picks up linuxbrew's
  freetype/mesa. Develop inside the SDK (or a toolbox) rather than on the
  host.
- `org.flatpak.Builder` has a private `/tmp`, so `--state-dir` and the build
  dir must live under `$HOME`.

## References

- Freya: <https://freyaui.dev/> · <https://github.com/marc2332/freya>
- Skia binaries fork: <https://github.com/marc2332/rust-skia/releases>
- skia-bindings offline builds:
  <https://github.com/rust-skia/rust-skia/blob/master/skia-bindings/README.md>
- Binary generation / decision tree:
  <https://github.com/rust-skia/rust-skia/wiki/Generation-of-Binaries>
- Upstream feature-combo discussion:
  <https://github.com/rust-skia/rust-skia/discussions/961>
