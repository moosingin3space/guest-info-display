# Implementation plan: Multi-Screen, Playback, Inhibitor

Source spec: `specs/multi-screen-and-playback.md`. Read that first for design
rationale; this document is the build sequence.

## Dependency graph

```
T1 (Inhibitor) ──────────────────────────────────────┐
                                                     │
T2 (rodio sink) ── T3 (device picker) ───────────────┤
                                                     ▼
T4 (identity + role storage) ── T5 (role UI) ── T6 (irpc service)
                                                     │
                                                     ▼
                            T7 (endpoint + plumbing) ── T8 (primary handlers)
                                                     │
                                                     ├── T9 (reflection ingest)
                                                     │
                                                     ├── T10 (discovery + pairing)
                                                     │
                                                     ├── T11 (peer management UI)
                                                     │
                                                     ├── T12 (role lifecycle)
                                                     │
                                                     └── T13 (disconnected UI)
                                                                  │
                            T14 (reflection inhibit) ─────────────┤
                            T15 (flatpak manifest) ───────────────┤
                            T16 (docs) ───────────────────────────┘
```

T1 / T2 / T3 are independent and can land in any order. Everything from T4
onward is multi-screen and should land in roughly the order shown — later
tickets assume earlier scaffolding. T15 (manifest) gates the first user-facing
release because ashpd, cpal, and iroh all need Flatpak permission grants.

---

## Phase 1 — Idle inhibitor

### T1: XDG Inhibit portal integration

**Phase:** 1  **Depends on:** none

**Goal:** While `is_playing == true`, hold an idle inhibit so the screen
doesn't blank. Release on pause/stop and on shutdown.

**New deps:**

- `ashpd` (XDG portals wrapper)
- `zbus` (transitive via ashpd; pin if needed for tokio compatibility)

**Files:**

- `src/inhibitor.rs` (new): RAII handle wrapping
  `ashpd::desktop::inhibit::InhibitProxy`. `Drop` closes the request.
- `src/main.rs`: store an `Option<InhibitHandle>` on `GuestInfoDisplay`.
  Update on every `Event::StateChanged` reception by inspecting
  `spotify_state.lock().is_playing`.

**Sketch:**

The inhibitor module owns its own zbus connection (created lazily on first
inhibit). Construction failure (no portal available) yields a stub handle
that logs once at warn level and otherwise no-ops.

The model's spotify-event handler diffs the new `is_playing` against the
prior value:

- false → true: take an inhibit, store the handle.
- true → false: drop the handle.

Always release on model `Drop`.

**Acceptance:**

- Manual: start playback on a desktop session with screen-blank set to ~1
  minute; verify the screen stays awake while playing and blanks ~1 minute
  after pause.
- Manual: kill the app mid-playback; portal lock is released (screen blanks
  on schedule).
- No measurable startup cost when the portal is unavailable (e.g. CI).

**Risks:**

- ashpd's `InhibitProxy::new` may require a window handle in some portal
  implementations. Use `WindowIdentifier::default()` if we can't easily get
  the GPUI window handle; the portal accepts that.

---

## Phase 2 — Audio playback

### T2: Replace stub player with rodio sink

**Phase:** 2  **Depends on:** none

**Goal:** Real audio comes out of the speakers when Spotify Connect plays
a track. Default audio device only — device picker comes in T3.

**Cargo:**

- `librespot` dep gains `features = ["rodio-backend"]` (verify exact feature
  name in the upstream Cargo.toml at pin time).

**Files:**

- `src/spotify.rs`: drop `NullSink` and the `|| Box::new(NullSink)` closure.
  Pull the rodio backend via `librespot::playback::audio_backend::find("rodio")`
  (or the equivalent named export). Replace `NoOpVolume` with the real
  `SoftMixer` already returned by `mixer::find(None)`.

**Sketch:**

`Player::new` currently takes a sink-builder closure and a volume-getter.
Both stubs go away. The mixer factory call in `mk_mixer` already returns a
real software mixer — we just stop passing `NoOpVolume` next to it.

**Acceptance:**

- Connect to the device via Spotify Connect from a phone, play a track,
  hear it through the system default output.
- Volume changes from the controlling phone are audible.
- App still builds when the rodio backend's transitive deps (alsa-sys etc.)
  are present.

**Risks:**

- ALSA-sys requires `libasound2-dev` headers at build time. Flatpak manifest
  needs the appropriate runtime extension (covered in T15).
- librespot's exact feature name may have churned; check at the pinned commit.

---

### T3: Audio device picker in settings

**Phase:** 2  **Depends on:** T2

**Goal:** User can pick which output device plays audio. Selection persists,
and changing it hot-swaps the running player.

**New deps:**

- `cpal` (direct dep; rodio uses it transitively, this just exposes
  enumeration).

**Files:**

- `src/persistence.rs`: new column on `spotify_config` for `audio_device_name`
  (nullable; null = system default). Migration adds it with `IF NOT EXISTS`.
- `src/audio_devices.rs` (new): thin wrapper around
  `cpal::traits::HostTrait::output_devices()` returning `Vec<String>`.
- `src/settings_dialog.rs`: dropdown listing `["System default", ...names]`.
  On change, persist and emit a callback that triggers player rebuild.
- `src/spotify.rs`: accept an optional device name in `start()`. Wire
  through to the rodio sink construction.
- `src/main.rs`: on settings change, tear down the existing spotify task
  and start a new one with the new device name. Spirc state survives because
  the controller-side device identity (the persisted UUID) is unchanged.

**Sketch:**

Picking a device by name and matching against `cpal` enumeration is a
brittle lookup (names can collide and reorder). We accept that — if the
saved name doesn't match anything at startup, fall back to system default
and log a warn.

The hot-swap path is the same one we'd hit if Spotify Connect dropped and
reconnected: the discovery loop's outer `while let Some(credentials)` in
`spotify::discovery_loop` runs again on the new task.

**Acceptance:**

- Dropdown lists all expected output devices on the test machine.
- Selecting a device while a track is playing causes a brief gap, then
  audio resumes on the new device.
- Restarting the app keeps the previous selection.
- Removing a previously-selected device (e.g. unplug USB DAC) → next
  startup falls back to system default with a warn log.
- Reflections do not show the dropdown (gated on role; if role storage
  is not yet in place from T4, gate on a TODO and revisit).

**Risks:**

- cpal device enumeration on PipeWire-via-pulse-shim sometimes returns
  duplicates. Acceptable for v1; worth a follow-up.

---

## Phase 3 — Multi-screen scaffolding (no wire)

### T4: Identity and role persistence

**Phase:** 3  **Depends on:** none (but lands before any other multi-screen ticket)

**Goal:** Persist the iroh `SecretKey` and the current role across restarts.
No networking yet.

**New deps:**

- `iroh` (just for the `SecretKey` type at this stage)

**Files:**

- `src/persistence.rs`:
  - `node_identity` table: single row, raw 32-byte secret-key bytes.
    Generated on first read.
  - `role` table: single row, text column with `'primary'` or
    `'reflection'`. Default `'primary'`.
  - Methods: `node_secret() -> Result<SecretKey>`, `role() -> Result<Role>`,
    `set_role(Role) -> Result<()>`.
- `src/main.rs`: read role at startup; pass to `spotify::start` (which
  becomes a no-op call in reflection role) — actual no-op behavior is
  T12, this just plumbs the value.

**Sketch:**

Use blob storage for the secret key (32 bytes); avoid stringifying to base64
unless we want to debug-print it. Add a `Role` enum in `src/persistence.rs`
that derives `Clone, Copy, PartialEq` and `ToSql`/`FromSql` like
`WifiSecurity` already does.

**Acceptance:**

- Two restarts of the app yield the same `NodeId` (printed at debug log
  level for verification).
- Setting role to reflection and restarting persists.

**Risks:**

- Migration ordering. The DB is small; if we ever break compatibility
  it's fine to wipe — but we should not in this release.

---

### T5: Role radio and role indicator label

**Phase:** 3  **Depends on:** T4

**Goal:** UI shows the current role at all times, and the user can switch
between Primary and Reflection in settings (no network effect yet — toggling
just persists and requires a restart in this ticket; T12 makes it live).

**Files:**

- `src/settings_dialog.rs`: add a "Role" section with two radio buttons.
  On change, call back into the model to persist via `Database::set_role`.
- `src/main.rs`: add a faded label below the body's main card row showing
  `Primary` or `Reflection`. Style: `text_color(muted_text)`, `text_sm()`,
  centered. The label reads from a `role: Role` field on
  `GuestInfoDisplay`, populated at startup from `Database::role()`.

**Sketch:**

The label sits just inside the body row, after the existing card+sidebar
`h_flex`. It does not need its own card surface — just a centered text
node with the muted color.

This ticket explicitly does not implement live role switching. Document
in the dialog: "Restart required to apply." T12 removes the restart
requirement.

**Acceptance:**

- Label visible on every screen, top-right of the body padding.
- Toggling role in settings + restart shows the new label.

---

## Phase 4 — Multi-screen v1 (the wire)

### T6: irpc service definition

**Phase:** 4  **Depends on:** T4

**Goal:** Define the `DisplayService` RPC contract and the streamed
`WireMessage` payload, exactly as specified in the spec. No networking
integration yet — just types and a round-trip serde test on `WireMessage`.

**New deps:**

- `iroh` (gets us `Endpoint`, `NodeId`, `SecretKey`)
- `iroh-irpc` (typed RPC; bundles framing/serialization)
- `serde` with `derive` (the `Request` and `WireMessage` enums need it)

`postcard` is a transitive dep via `iroh-irpc` — do not pull it in
directly.

**Files:**

- `src/multi_screen/mod.rs` (new module dir, `pub mod` exports)
- `src/multi_screen/proto.rs`: the typed RPC contract from the spec —
  the `Request` enum with `#[rpc_requests(DisplayService, message = DisplayRequest)]`,
  `RequestPairing` (oneshot reply) and `Subscribe` (mpsc stream reply)
  variants, plus `PairingRequest`, `PairingResponse`, `SubscribeRequest`,
  and the `WireMessage` enum.
- `src/spotify.rs`: derive `Serialize`/`Deserialize` on `SpotifyTrackInfo`
  and `SpotifyState`. Wrap chrono / non-serde fields if any exist.

**Sketch:**

There is no manual envelope or version field — `iroh-irpc` owns framing.
Forward compatibility is handled by serde's tolerant `#[serde(other)]`
on enum variants where it matters; bump a service version constant if
we ever break the protocol.

Cover art payload carries the **encoded** bytes (JPEG/PNG), not BGRA.
The reflection side decodes through `image::load_from_memory` and feeds
the result through the existing `build_render_image` helper — pull that
helper out of `main.rs` into a shared module if it's not already shared.

**Acceptance:**

- The service compiles; the irpc derive macros expand cleanly.
- Round-trip serde test on `WireMessage` (including the `Snapshot`
  variant with a representative `SpotifyState`) passes.
- Snapshot of a realistic state (1 current, 5 history, 50 queue) fits
  in well under 1 MB after serialization.

---

### T7: iroh endpoint + service plumbing

**Phase:** 4  **Depends on:** T4, T6

**Goal:** A single `iroh::Endpoint` per process, bound to the persisted
secret key, hosts the `DisplayService` defined in T6. Handlers are stubs
that log and return errors — wiring real behavior is T8/T9.

**Files:**

- `src/multi_screen/endpoint.rs`: `build_endpoint(secret_key) ->
  Endpoint` configures discovery (mDNS at this stage; full discovery
  in T10) and binds the irpc-required ALPN.
- `src/multi_screen/service.rs`: stub `DisplayService` impl that handles
  the two RPC methods with logging-only bodies.
- `src/main.rs`: bring the endpoint up at startup regardless of role
  (identity is symmetric). Hold the handle on the model.

**Sketch:**

`iroh-irpc` typically owns the accept loop once you register a service
implementation against an endpoint — favor the library's bundling over
a hand-rolled accept loop.

The endpoint runs on its own named tokio runtime spawned in
`multi_screen::start`, parallel to the existing spotify runtime. Easier
to cancel cleanly than sharing.

**Acceptance:**

- App starts with the endpoint up; `NodeId` logged.
- Two instances on the same network can see each other's `NodeId`s in
  iroh's discovery debug logs.
- Calling `RequestPairing` from one to the other returns an explicit
  "not implemented" error (proves the wire is end-to-end live).

**Risks:**

- ALPN: irpc may set this for you, but pin a stable service name string
  somewhere (`xyz.mooshq.guest-info-display.display/1`) and check what
  irpc surfaces.

---

### T8: Primary-side service implementation

**Phase:** 4  **Depends on:** T6, T7

**Goal:** A primary serves real `Subscribe` and `RequestPairing` calls.
Subscribers receive an immediate `Snapshot` and then every subsequent
state change via their per-subscription `mpsc::Sender<WireMessage>`.

**Files:**

- `src/multi_screen/service.rs`: replace stubs with real handlers.
- `src/multi_screen/primary.rs`: subscriber registry — a `Mutex<Vec<
  mpsc::Sender<WireMessage>>>` (or a slab keyed by NodeId for easier
  removal) that the spotify event loop pushes into.
- `src/spotify.rs`: tap the existing event channel — when a
  `StateChanged` arrives in the primary path, snapshot
  `SharedSpotifyState` and fan out a `WireMessage::StateChanged(...)`
  to every sender. Drop senders that return `SendError` (channel closed).
- `src/multi_screen/mod.rs`: export `start_primary(state, db) ->
  PrimaryHandle`.

**Access control:**

- `RequestPairing`: accept from any NodeId. The handler enqueues a
  pending approval onto a channel that the GPUI side drains (see T10
  for the UI; the channel itself lives here).
- `Subscribe`: reject unless the caller's NodeId is in
  `paired_peers` with `direction = "inbound"`. T10 populates that
  table; until then, hard-code one trusted peer for testing.

**Sketch:**

The first message sent on a new subscription is always a `Snapshot`
built from `state.lock()`. Subsequent messages are full-state replaces
via `StateChanged(SpotifyState)`. This matches the spec — no deltas,
no diffing.

Cover art: when `Event::CoverLoaded` fires in the primary's spotify
loop, also emit `WireMessage::CoverArt { url, encoded }` to subscribers.
The encoded bytes are already on hand from the cover-fetch path — keep
a copy when we go to decode it locally so we don't refetch for the wire.

**Acceptance:**

- A second instance with a hard-coded trusted-peer entry can call
  `Subscribe` and receives the snapshot + each subsequent state change.
- A second instance *without* a trusted-peer entry gets a permission
  error from `Subscribe`.
- `RequestPairing` from any NodeId queues a pending approval (verify
  via log line; UI integration is T10).

**Risks:**

- Slow subscriber backpressure. If a reflection's `mpsc::Receiver` fills
  up, `Sender::try_send` returns `Full`. On full, drop that subscription
  and let the reflection reconnect for a fresh snapshot — match the
  spec's "no greyed-out last-known state" approach.
- Encoded cover bytes need to live somewhere — adding a `bytes: Bytes`
  field to `CoverImage` (or a parallel cache) is the cleanest path.

---

### T9: Reflection-side ingest

**Phase:** 4  **Depends on:** T6, T7, T8

**Goal:** A reflection calls `Subscribe` against its paired primary,
consumes the resulting `mpsc::Receiver<WireMessage>`, and updates the
local `SharedSpotifyState` and cover cache so the GPUI side renders
without knowing the source.

**Files:**

- `src/multi_screen/reflection.rs`: dial the saved primary's `NodeId`,
  invoke `Subscribe`, loop on the returned receiver:
  - `Snapshot(state)` / `StateChanged(state)` → replace `SharedSpotifyState`
    contents, push `Event::StateChanged` into the existing async-channel.
  - `CoverArt { url, encoded }` → decode via `image::load_from_memory`,
    push `Event::CoverLoaded` (built like the local path does today).
  - `CoversCleared` → push `Event::CoversCleared`.
- `src/multi_screen/mod.rs`: `start_reflection(primary_node, state, tx)
  -> ReflectionHandle`.

**Sketch:**

The reflection's spotify thread is replaced by this client. The GPUI
event consumer in `main.rs` doesn't need to know — same channel, same
events, same `SharedSpotifyState`. No `Player`, no `Spirc`, no audio
device.

When the receiver closes (primary went away, network dropped), surface
a `ConnectionLost` signal (added in T13) and start the reconnection loop.

**Acceptance:**

- Reflection display mirrors primary's now-playing and queue within
  ~500ms of a state change on the primary.
- Cover art appears on the reflection within ~1s of appearing on the
  primary (network-dependent).
- Killing the primary closes the receiver cleanly (no panic, no zombie
  task).

---

### T10: Local discovery and pairing UX

**Phase:** 4  **Depends on:** T8

**Goal:** Reflections discover primaries on the local network and can
initiate pairing. Primaries surface incoming `RequestPairing` calls in
the settings dialog and persist on approval.

**Files:**

- `src/persistence.rs`: `paired_peers` table — columns: `node_id` (blob,
  PK), `friendly_name` (text), `direction` (text: `"outbound"` =
  "this peer is my primary", `"inbound"` = "this peer is one of my
  reflections"). Methods to add, remove, list, and check membership.
- `src/multi_screen/discovery.rs`: wraps iroh's local (mDNS) discovery
  and exposes `discover_primaries() -> Stream<DiscoveredNode>` for the
  reflection-side UI.
- `src/multi_screen/reflection.rs`: add a `request_pairing(node_id,
  friendly_name) -> Result<PairingResponse>` helper that opens a
  short-lived endpoint connection and calls the `RequestPairing` RPC.
- `src/multi_screen/primary.rs`: drain the pending-approval channel set
  up in T8 into a UI-facing `Receiver<PendingApproval>`. On user
  approval, persist the reflection in `paired_peers` and resolve the
  pending oneshot reply with `PairingResponse::Accepted`. On rejection
  (or dialog close), reply with `Rejected`.
- `src/settings_dialog.rs`: discovery list + approval prompts (full UI
  for paired peers comes in T11).
- `src/main.rs`: route the pending-approval channel into the GPUI side.

**Sketch:**

`RequestPairing` is just an RPC method on the same service from T6 —
its access-control rule is "always allow." The handler stalls awaiting
the user's decision via an `oneshot::Sender<PairingResponse>` that the
UI resolves.

`friendly_name` defaults to `gethostname()`; user-editable later.

**Acceptance:**

- Fresh reflection install: settings dialog discovers the primary on
  the same LAN within ~10s.
- Pairing approval round-trip completes in <5s after the primary user
  clicks Approve.
- Approved peers persist across restarts on both sides; subsequent
  `Subscribe` from the reflection is accepted without re-pairing.
- Closing the approval dialog returns `Rejected`; no DB rows on either
  side.

**Risks:**

- mDNS inside Flatpak needs `--share=network` plus possibly Avahi
  D-Bus access; revisit at T15.
- `RequestPairing` is unauthenticated — a hostile LAN peer can spam the
  primary with approval popups. Acceptable for v1 (the user just clicks
  Reject). Worth flagging in `specs/remaining.md`.

---

### T11: Peer management UI

**Phase:** 4  **Depends on:** T10

**Goal:** Both roles can list and manage their paired peers from settings.

**Files:**

- `src/settings_dialog.rs`:
  - Primary view: list of paired reflections with a remove button each
    and a section showing pending approval requests.
  - Reflection view: shows the currently paired primary with a "Forget"
    button.

**Acceptance:**

- Removing a reflection from the primary's list disconnects it.
- "Forget primary" on a reflection drops the connection and goes back
  to the discovery list.

---

### T12: Role lifecycle (live switching)

**Phase:** 4  **Depends on:** T8, T9, T11

**Goal:** Toggling role in settings tears down the active backend and
spins up the new one without restart.

**Files:**

- `src/main.rs`: replace the one-shot `spotify::start` and `multi_screen`
  calls at startup with a small role-aware supervisor that owns whichever
  task tree is appropriate. Settings calls into a "swap role" function
  that drops the current handles and constructs the new ones.

**Sketch:**

Roughly:

```rust
enum BackendHandle {
    Primary { spotify: SpotifyHandle, server: PrimaryHandle },
    Reflection { client: ReflectionHandle },
}
```

The model holds `Option<BackendHandle>`. Swap functions take `&mut self`,
drop the old handle (which aborts internal tasks), and construct the new.

**Acceptance:**

- Switch from primary to reflection without restart: settings dialog
  reopens cleanly, role label updates, primary discovery panel appears.
- Switch from reflection to primary: librespot starts up, audio device
  initializes.
- No leaked tasks (verify with a `ps`-equivalent or by repeated toggles).

---

### T13: "Primary unavailable" empty state

**Phase:** 4  **Depends on:** T9

**Goal:** When a reflection is disconnected from its primary, clear the
card and show a centered "Primary unavailable" message. Keep retrying
in the background (no auto-promotion).

**Files:**

- `src/main.rs`: introduce a `connection_status` field on
  `GuestInfoDisplay` (only meaningful in reflection role). Render branch:
  if reflection + disconnected, replace the whole Spotify card body with
  a centered text node.
- `src/multi_screen/client.rs`: emit a `ConnectionLost` / `ConnectionUp`
  event into the existing event channel so the model knows.

**Sketch:**

Reconnect backoff: start at 1s, double up to 30s, reset on successful
snapshot. iroh handles QUIC-level retries; we're handling RPC-level
re-establishment.

**Acceptance:**

- Kill the primary; within ~30s the reflection shows "Primary unavailable"
  and the card body clears.
- Bring the primary back; reflection rebuilds state within ~5s.

---

## Phase 5 — Polish

### T14: Inhibit on reflections

**Phase:** 5  **Depends on:** T1, T9

**Goal:** Reflections also hold the inhibit lock while their displayed
`is_playing` is true.

**Files:**

- `src/main.rs`: the inhibitor handler from T1 is already triggered by
  `is_playing` flips on `SharedSpotifyState`. Reflections write to the
  same field via T9, so this should *already work* once both tickets
  land. This ticket is just the verification step + any small tweaks
  needed.

**Acceptance:**

- On a reflection, with primary playing, screen does not blank.
- On a reflection, with primary paused, screen blanks normally.

---

### T15: Flatpak manifest updates

**Phase:** 5  **Depends on:** T1, T2, T7, T10

**Goal:** All new permissions and host services the new features need
are granted in the manifest. `generated-sources.json` is regenerated.

**Files:**

- `xyz.mooshq.GuestInfoDisplay.json`:
  - Network share is already needed for librespot; verify mDNS still
    works inside the sandbox (may need `--talk-name=org.freedesktop.Avahi`).
  - PulseAudio/PipeWire socket: `--socket=pulseaudio` (probably already
    present for audio output).
  - XDG portal access is implicit for sandboxed Flatpaks; no extra grant.
- `generated-sources.json`: regenerate with the new dependency tree:
  ```
  python3 flatpak-cargo-generator.py Cargo.lock -o generated-sources.json
  ```

**Acceptance:**

- Flatpak build succeeds with the new deps.
- Inside the Flatpak runtime: audio plays, mDNS discovers peers, screen
  inhibitor works.

**Risks:**

- iroh's transitive C deps (if any — TLS via rustls keeps it light) may
  need additional packages from `org.freedesktop.Sdk`.

---

### T16: README + spec note updates

**Phase:** 5  **Depends on:** all of phase 4

**Goal:** External-facing docs reflect the new capabilities.

**Files:**

- `README.md`: short section on multi-screen setup (one-paragraph: enable
  on second device → settings → choose role → pair).
- `CLAUDE.md`: update the architecture summary to mention `multi_screen`
  module and the role-aware backend supervisor.
- `specs/remaining.md`: prune anything that this work resolved.

**Acceptance:**

- A new contributor can read README + CLAUDE.md and understand the
  multi-screen feature without opening the spec.

---

## Cut points

If we need to ship sooner than the full plan allows:

- **Minimum viable**: T1, T2, T15. Inhibitor + audio + manifest. No
  multi-screen at all. ~2-3 days of work.
- **Single-display playback**: above + T3 (device picker). ~3-4 days.
- **Multi-screen MVP**: through T13, skipping T11 (peer management UI
  becomes "delete the DB row"). ~2 weeks.
- **Full plan**: all 16 tickets. ~3 weeks.

These are calendar estimates assuming part-time work; halve them for
focused full-time effort.
