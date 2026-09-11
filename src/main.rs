use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use bytes::Bytes;
use chrono::Local;
use freya::prelude::*;
use iroh::EndpointId;

mod audio_devices;
mod inhibitor;
mod multi_screen;
mod persistence;
mod qr_code;
mod settings_dialog;
mod spotify;

/// Whichever task tree is producing `(state, events)` for the UI consumer.
/// Holding the appropriate variant keeps that backend alive; replacing it
/// (e.g. on role switch) drops the previous handles and aborts their tasks.
enum BackendHandle {
    Primary(spotify::SpotifyHandle),
    Reflection(multi_screen::ReflectionHandle),
}

/// Everything the UI shows or acts on. Owned by the root component as a
/// single `State<Model>`; tasks and event handlers mutate it through
/// `write()`, which re-renders every component that read it.
pub struct Model {
    db: persistence::Database,
    wifi_creds: Option<persistence::WifiCredentials>,
    role: persistence::Role,
    hide_titlebar: bool,
    /// Stable iroh identity for this device. Surfaced in the UI so users can
    /// recognize each instance when pairing primaries and reflections.
    endpoint_id: EndpointId,
    /// Active backend producing the spotify event stream. `None` when role is
    /// reflection but no primary is paired yet — the UI shows the "Primary
    /// unavailable" empty state in that case.
    backend: Option<BackendHandle>,
    /// iroh endpoint + RPC service. Identity is symmetric across roles, so we
    /// keep one runtime alive for the lifetime of the model regardless of role.
    multi_screen: multi_screen::MultiScreenHandle,
    /// Pending pairing approval requests from unknown peers. Head is rendered
    /// as a popup; user clicks Approve/Reject to advance.
    pending_approvals: VecDeque<multi_screen::PendingApproval>,
    /// Live discovery + paired-peer state surfaced to the settings dialog.
    pairing: settings_dialog::PairingState,
    current_track: Option<spotify::SpotifyTrackInfo>,
    queue: Vec<spotify::SpotifyTrackInfo>,
    /// Encoded cover art keyed by URL. `ImageViewer` decodes and caches it.
    covers: HashMap<String, Bytes>,
    is_playing: bool,
    /// Reflection-only: tracks whether the active subscription has delivered
    /// a snapshot recently. `false` after the session drops; flipped back to
    /// `true` on the next snapshot. Always `true` in primary role.
    connected: bool,
    inhibitor: inhibitor::Inhibitor,
    /// Listener forwarding the active backend's events into the model.
    spotify_task: Option<TaskHandle>,
}

impl Model {
    fn new() -> Self {
        let db = persistence::Database::open().expect("failed to open settings database");
        let wifi_creds = db.wifi_credentials().ok().flatten();
        let role = db.role().expect("failed to load role");
        let hide_titlebar = db.hide_titlebar().unwrap_or(false);

        let secret = db.node_secret().expect("failed to load iroh identity");
        let endpoint_id = secret.public();
        log::debug!("multi_screen: EndpointId = {endpoint_id}");
        let multi_screen = multi_screen::start(secret);

        let inbound_paired = db
            .paired_peers(persistence::Direction::Inbound)
            .unwrap_or_default();
        // Seed the inbound trusted set from already-paired reflections so they
        // can subscribe right after startup.
        let inbound_ids: HashSet<_> = inbound_paired.iter().map(|p| p.endpoint_id).collect();
        multi_screen.seed_inbound_trusted(inbound_ids);

        let pairing = settings_dialog::PairingState {
            paired_primary: db.paired_primary().ok().flatten(),
            paired_reflections: inbound_paired,
            discovered_primaries: HashSet::new(),
        };

        Self {
            db,
            wifi_creds,
            role,
            hide_titlebar,
            endpoint_id,
            backend: None,
            multi_screen,
            pending_approvals: VecDeque::new(),
            pairing,
            current_track: None,
            queue: Vec::new(),
            covers: HashMap::new(),
            is_playing: false,
            connected: true,
            inhibitor: inhibitor::Inhibitor::new(),
            spotify_task: None,
        }
    }
}

/// Tear down whatever backend is running and start the one matching the
/// model's role and the persisted audio-device selection. Called on role
/// change, on audio-device change, and at startup.
fn rebuild_backend(mut model: State<Model>) {
    let mut m = model.write();
    // Drop in-flight work first. Dropping the BackendHandle closes the
    // backend's shutdown channel — Primary exits the librespot discovery
    // loop and releases the rodio sink; Reflection cancels the dial loop.
    // Cancelling the listener task means no late updates land in the model.
    if let Some(task) = m.spotify_task.take() {
        task.cancel();
    }
    m.backend = None;

    m.current_track = None;
    m.queue.clear();
    m.covers.clear();
    m.is_playing = false;
    // Primaries are always "connected" (they generate their own state).
    // Reflections start disconnected and flip on the first snapshot.
    m.connected = matches!(m.role, persistence::Role::Primary);
    m.inhibitor.set(false);

    let new_backend = match m.role {
        persistence::Role::Primary => {
            let device_id = m.endpoint_id.fmt_short().to_string();
            let audio_device = m
                .db
                .audio_device_name()
                .expect("failed to load audio device name");
            Some(BackendHandle::Primary(spotify::start(
                device_id,
                audio_device,
            )))
        }
        persistence::Role::Reflection => m.db.paired_primary().ok().flatten().map(|primary| {
            BackendHandle::Reflection(m.multi_screen.start_reflection(primary.endpoint_id))
        }),
    };

    if let Some(backend) = new_backend {
        let (state, events) = match &backend {
            BackendHandle::Primary(h) => (h.state.clone(), h.events.clone()),
            BackendHandle::Reflection(h) => (h.state.clone(), h.events.clone()),
        };
        m.backend = Some(backend);
        // Not tied to the calling component: rebuilds are triggered from the
        // settings dialog, which unmounts when it closes.
        m.spotify_task = Some(spawn_forever(spotify_listener(model, state, events)));
    }
}

async fn spotify_listener(
    mut model: State<Model>,
    state: spotify::SharedSpotifyState,
    events: async_channel::Receiver<spotify::Event>,
) {
    while let Ok(event) = events.recv().await {
        let mut m = model.write();
        match event {
            spotify::Event::StateChanged => {
                if let Ok(sp) = state.lock() {
                    m.current_track = sp.current.clone();
                    m.queue = sp.queue.clone();
                    if sp.is_playing != m.is_playing {
                        m.is_playing = sp.is_playing;
                        m.inhibitor.set(sp.is_playing);
                    }
                    m.multi_screen.broadcast_state(sp.clone());
                }
            }
            spotify::Event::CoverLoaded(cover) => {
                m.multi_screen
                    .broadcast_cover(cover.url.clone(), cover.encoded.clone());
                m.covers.insert(cover.url, Bytes::from(cover.encoded));
            }
            spotify::Event::CoversCleared => {
                m.multi_screen.broadcast_covers_cleared();
                m.covers.clear();
            }
            spotify::Event::ConnectionLost => {
                m.connected = false;
                // is_playing was already cleared via the preceding
                // StateChanged, which also released the inhibit; nothing
                // else to do here.
            }
            spotify::Event::ConnectionRestored => {
                m.connected = true;
            }
        }
    }
}

/// Persist the settings dialog's values and apply whatever changed.
fn save_settings(
    mut model: State<Model>,
    values: settings_dialog::DialogValues,
    platform: &Platform,
) {
    let needs_rebuild = {
        let mut m = model.write();
        m.db.set_wifi_credentials(&values.wifi).ok();
        m.wifi_creds = Some(values.wifi);

        let role_changed = values.role != m.role;
        if role_changed {
            m.db.set_role(values.role).ok();
            m.role = values.role;
        }

        let prev_audio = m.db.audio_device_name().ok().flatten();
        let audio_changed = prev_audio != values.audio_device;
        if audio_changed {
            m.db.set_audio_device_name(values.audio_device.as_deref())
                .ok();
        }

        if values.hide_titlebar != m.hide_titlebar {
            m.db.set_hide_titlebar(values.hide_titlebar).ok();
            m.hide_titlebar = values.hide_titlebar;
            let decorations = !values.hide_titlebar;
            platform.with_window(None, move |window| window.set_decorations(decorations));
        }

        role_changed || audio_changed
    };
    if needs_rebuild {
        rebuild_backend(model);
    }
}

/// Ask `primary_id` to accept this device as a reflection; on acceptance,
/// persist the pairing and switch the backend over to it.
fn pair_with(model: State<Model>, primary_id: EndpointId) {
    let recv = model
        .peek()
        .multi_screen
        .request_pairing(primary_id, local_hostname());
    spawn_forever(async move {
        let mut model = model;
        let response = match recv.recv().await {
            Ok(Some(r)) => r,
            _ => {
                log::warn!("pairing: no response from primary");
                return;
            }
        };
        if !matches!(response, multi_screen::PairingResponse::Accepted) {
            log::info!("pairing: rejected by primary");
            return;
        }
        {
            let mut m = model.write();
            let label = format!("Primary {}", primary_id.fmt_short());
            if let Err(e) =
                m.db.add_paired_peer(primary_id, &label, persistence::Direction::Outbound)
            {
                log::warn!("pairing: persist failed: {e}");
                return;
            }
            m.pairing.paired_primary = Some(persistence::PairedPeer {
                endpoint_id: primary_id,
                friendly_name: label,
                direction: persistence::Direction::Outbound,
            });
        }
        rebuild_backend(model);
    });
}

fn remove_reflection(mut model: State<Model>, id: EndpointId) {
    let mut m = model.write();
    if let Err(e) = m.db.remove_paired_peer(id) {
        log::warn!("remove reflection: persist failed: {e}");
        return;
    }
    m.multi_screen.remove_inbound_trusted(id);
    m.multi_screen.disconnect_subscriber(id);
    m.pairing.paired_reflections.retain(|p| p.endpoint_id != id);
}

fn forget_primary(mut model: State<Model>) {
    {
        let mut m = model.write();
        if let Ok(Some(p)) = m.db.paired_primary()
            && let Err(e) = m.db.remove_paired_peer(p.endpoint_id)
        {
            log::warn!("forget primary: persist failed: {e}");
            return;
        }
        m.pairing.paired_primary = None;
    }
    rebuild_backend(model);
}

fn reject_pending_approval(mut model: State<Model>) {
    if let Some(approval) = model.write().pending_approvals.pop_front() {
        let _ = approval
            .respond
            .send(multi_screen::PairingResponse::Rejected);
    }
}

fn approve_pending_approval(mut model: State<Model>) {
    let mut m = model.write();
    let Some(approval) = m.pending_approvals.pop_front() else {
        return;
    };
    let endpoint_id = approval.endpoint_id;
    let friendly_name = approval.friendly_name.clone();
    if let Err(e) =
        m.db.add_paired_peer(endpoint_id, &friendly_name, persistence::Direction::Inbound)
    {
        log::warn!("approval: persist failed: {e}");
    } else {
        let reflections = &mut m.pairing.paired_reflections;
        reflections.retain(|p| p.endpoint_id != endpoint_id);
        reflections.push(persistence::PairedPeer {
            endpoint_id,
            friendly_name,
            direction: persistence::Direction::Inbound,
        });
    }
    m.multi_screen.add_inbound_trusted(endpoint_id);
    let _ = approval
        .respond
        .send(multi_screen::PairingResponse::Accepted);
}

/// Best-effort hostname lookup for use as the default `friendly_name` in the
/// pairing handshake. Falls back through `$HOSTNAME` → `/etc/hostname` → a
/// generic literal so it always returns something sensible.
fn local_hostname() -> String {
    if let Ok(name) = std::env::var("HOSTNAME") {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Ok(contents) = std::fs::read_to_string("/etc/hostname") {
        let trimmed = contents.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    "Reflection".to_string()
}

/// Width the layout was originally tuned for. The Pi-class panels we ship to
/// run around this; anything wider scales up proportionally.
const DESIGN_WIDTH: f32 = 1280.0;
/// Cap so 4K monitors don't blow up to absurd sizes.
const MAX_UI_SCALE: f32 = 1.75;

/// Scale the whole UI up on larger-than-design monitors. Freya's per-window
/// zoom multiplies the scale factor, so text, spacing and the built-in
/// components all grow together and layout code can use plain design sizes.
fn use_ui_zoom() {
    let platform = Platform::get();
    use_side_effect(move || {
        // Subscribe to resizes. The width itself is read from winit, in
        // physical pixels, so HiDPI panels (scale_factor > 1) also count as
        // "larger" and the zoom we apply can't feed back into the input.
        let _ = *platform.root_size.read();
        let _ = platform.post_callback(|window_id, ctx| {
            if let Some(window) = ctx.windows_mut().get_mut(&window_id) {
                let physical_width = window.window().inner_size().width as f32;
                window.set_user_zoom((physical_width / DESIGN_WIDTH).clamp(1.0, MAX_UI_SCALE));
            }
        });
    });
}

const SURFACE: (u8, u8, u8, u8) = (255, 255, 255, 15);
const SURFACE_BORDER: (u8, u8, u8, u8) = (255, 255, 255, 31);
const PLACEHOLDER: (u8, u8, u8, u8) = (255, 255, 255, 20);
const MUTED_TEXT: (u8, u8, u8, u8) = (255, 255, 255, 140);

static SETTINGS_ICON: &[u8] = include_bytes!("../assets/settings.svg");

fn card() -> Rect {
    rect()
        .corner_radius(16.)
        .background(SURFACE)
        .border(Border::new().fill(SURFACE_BORDER).width(1.))
}

fn app() -> impl IntoElement {
    use_init_theme(dark_theme);
    let model = use_state(Model::new);
    let now = use_state(Local::now);
    let settings_open = use_state(|| false);
    use_ui_zoom();

    use_hook(move || {
        let (approvals_rx, discovered_rx) = {
            let m = model.peek();
            (m.multi_screen.approvals(), m.multi_screen.discovered())
        };

        spawn(async move {
            let mut model = model;
            while let Ok(approval) = approvals_rx.recv().await {
                model.write().pending_approvals.push_back(approval);
            }
        });

        spawn(async move {
            let mut model = model;
            while let Ok(node) = discovered_rx.recv().await {
                let known = model
                    .peek()
                    .pairing
                    .discovered_primaries
                    .contains(&node.endpoint_id);
                if !known {
                    model
                        .write()
                        .pairing
                        .discovered_primaries
                        .insert(node.endpoint_id);
                }
            }
        });

        // Clock: updates the displayed time every second.
        spawn(async move {
            let mut now = now;
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                now.set(Local::now());
            }
        });

        rebuild_backend(model);
    });

    let m = model.read();
    let (date_str, time_str) = {
        let now = now.read();
        (
            now.format("%A, %B %-d").to_string(),
            now.format("%H:%M:%S").to_string(),
        )
    };

    let show_disconnected = matches!(m.role, persistence::Role::Reflection) && !m.connected;

    let header = rect()
        .horizontal()
        .width(Size::fill())
        .padding((20., 32.))
        .main_align(Alignment::SpaceBetween)
        .cross_align(Alignment::Center)
        .child(
            label()
                .font_size(24.)
                .font_weight(FontWeight::SEMI_BOLD)
                .text(date_str),
        )
        .child(
            label()
                .font_size(30.)
                .font_family("Adwaita Mono")
                .font_weight(FontWeight::BOLD)
                .text(time_str),
        );

    let body = rect()
        .horizontal()
        .width(Size::fill())
        .height(Size::flex(1.))
        .content(Content::flex())
        .spacing(24.)
        .padding((0., 32., 8., 32.))
        .child(if show_disconnected {
            disconnected_card()
        } else {
            now_playing_card(&m)
        })
        .child(wifi_sidebar(&m, settings_open));

    let footer = label()
        .width(Size::fill())
        .margin((0., 0., 16., 0.))
        .font_size(14.)
        .color(MUTED_TEXT)
        .text_align(TextAlign::Center)
        .text(format!(
            "{} · {}",
            match m.role {
                persistence::Role::Primary => "Primary",
                persistence::Role::Reflection => "Reflection",
            },
            m.endpoint_id.fmt_short(),
        ));

    let approval = approval_popup(model, &m);

    rect()
        .expanded()
        .content(Content::flex())
        .color(Color::WHITE)
        .background(
            // Freya measures the angle opposite to CSS: 0° runs top to bottom.
            LinearGradient::new()
                .angle(0.)
                .stop(((10, 16, 51), 0.))
                .stop(((59, 29, 110), 100.)),
        )
        .child(header)
        .child(body)
        .child(footer)
        .child(approval)
        .child(
            Popup::new()
                .on_close_request(move |_| {
                    let mut settings_open = settings_open;
                    settings_open.set(false);
                })
                .maybe_child(settings_open.read().then(|| settings_dialog::SettingsDialog {
                    model,
                    open: settings_open,
                })),
        )
}

fn now_playing_card(m: &Model) -> Rect {
    let (track_name, track_artist) = match &m.current_track {
        Some(t) => (t.name.clone(), t.artists.clone()),
        None => ("Nothing playing".to_string(), String::new()),
    };
    let cover = m
        .current_track
        .as_ref()
        .and_then(|t| t.cover_url.as_ref())
        .and_then(|url| m.covers.get(url).map(|bytes| (url.clone(), bytes.clone())));

    let cover_box = rect()
        .width(Size::px(220.))
        .height(Size::px(220.))
        .corner_radius(12.)
        .overflow(Overflow::Clip);
    let cover_box = if let Some(source) = cover {
        cover_box.child(
            ImageViewer::new(source)
                .expanded()
                .aspect_ratio(AspectRatio::Max)
                .image_cover(ImageCover::Center),
        )
    } else {
        cover_box
            .center()
            .background(PLACEHOLDER)
            .child(label().color(MUTED_TEXT).text("Cover Art"))
    };

    let now_playing = rect()
        .width(Size::flex(1.))
        .height(Size::fill())
        .overflow(Overflow::Clip)
        .spacing(16.)
        .child(
            label()
                .color(MUTED_TEXT)
                .font_size(30.)
                .font_weight(FontWeight::SEMI_BOLD)
                .text("Now Playing"),
        )
        .child(
            rect()
                .horizontal()
                .width(Size::fill())
                .content(Content::flex())
                .overflow(Overflow::Clip)
                .spacing(20.)
                .cross_align(Alignment::Center)
                .child(cover_box)
                .child(
                    rect()
                        .width(Size::flex(1.))
                        .spacing(8.)
                        .child(
                            label()
                                .width(Size::fill())
                                .max_lines(1)
                                .text_overflow(TextOverflow::Ellipsis)
                                .font_size(24.)
                                .font_weight(FontWeight::BOLD)
                                .text(track_name),
                        )
                        .child(
                            label()
                                .width(Size::fill())
                                .max_lines(1)
                                .text_overflow(TextOverflow::Ellipsis)
                                .font_size(20.)
                                .color(MUTED_TEXT)
                                .text(track_artist),
                        ),
                ),
        );

    let up_next_items: Element = if m.queue.is_empty() {
        label()
            .font_size(14.)
            .color(MUTED_TEXT)
            .text("—")
            .into()
    } else {
        rect()
            .width(Size::fill())
            .spacing(12.)
            .children(m.queue.iter().take(5).map(|t| {
                if t.is_resolved() {
                    queue_item(t.name.clone(), t.artists.clone())
                } else {
                    queue_item("--".to_string(), String::new())
                }
                .into()
            }))
            .into()
    };

    let up_next = rect()
        .width(Size::px(280.))
        .height(Size::fill())
        .spacing(16.)
        .child(
            label()
                .color(MUTED_TEXT)
                .font_size(14.)
                .font_weight(FontWeight::SEMI_BOLD)
                .text("Up Next"),
        )
        .child(up_next_items);

    card()
        .horizontal()
        .width(Size::flex(1.))
        .height(Size::fill())
        .content(Content::flex())
        .overflow(Overflow::Clip)
        .spacing(32.)
        .padding(24.)
        .child(now_playing)
        .child(up_next)
}

fn disconnected_card() -> Rect {
    card()
        .width(Size::flex(1.))
        .height(Size::fill())
        .center()
        .child(
            label()
                .color(MUTED_TEXT)
                .font_size(24.)
                .font_weight(FontWeight::SEMI_BOLD)
                .text("Primary unavailable"),
        )
}

fn wifi_sidebar(m: &Model, settings_open: State<bool>) -> Rect {
    let qr_box = rect()
        .width(Size::px(220.))
        .height(Size::px(220.))
        .corner_radius(12.)
        .overflow(Overflow::Clip);
    let qr_box = if let Some(creds) = &m.wifi_creds {
        qr_box.child(qr_code::wifi_qr_element(creds))
    } else {
        qr_box
            .center()
            .background(PLACEHOLDER)
            .child(label().color(MUTED_TEXT).text("Not configured"))
    };

    let settings_button = TooltipContainer::new(Tooltip::new("Settings")).child(
        Button::new()
            .flat()
            .on_press(move |_| {
                let mut settings_open = settings_open;
                settings_open.set(true);
            })
            .child(
                // Without an explicit color the viewer waits to inherit one
                // before rasterizing, and the flat button draws nothing.
                SvgViewer::new(("settings-icon", SETTINGS_ICON))
                    .color(Color::WHITE)
                    .width(Size::px(24.))
                    .height(Size::px(24.)),
            ),
    );

    card()
        .width(Size::px(320.))
        .height(Size::fill())
        .padding(24.)
        .main_align(Alignment::SpaceBetween)
        .cross_align(Alignment::Center)
        .child(
            rect()
                .width(Size::fill())
                .spacing(16.)
                .cross_align(Alignment::Center)
                .child(
                    label()
                        .font_size(20.)
                        .font_weight(FontWeight::SEMI_BOLD)
                        .text("Scan to connect to Wi-Fi"),
                )
                .child(qr_box),
        )
        .child(settings_button)
}

fn approval_popup(model: State<Model>, m: &Model) -> Popup {
    // No close request handler: like a modal, the only way out is a decision.
    let children: Vec<Element> = match m.pending_approvals.front() {
        None => Vec::new(),
        Some(approval) => vec![
            PopupTitle::new("Pair this device?".to_string()).into(),
            PopupContent::new()
                .child(
                    rect()
                        .spacing(4.)
                        .child(approval.friendly_name.clone())
                        .child(
                            label()
                                .font_size(13.)
                                .color(MUTED_TEXT)
                                .text(approval.endpoint_id.fmt_short().to_string()),
                        ),
                )
                .into(),
            PopupButtons::new()
                .child(
                    Button::new()
                        .outline()
                        .on_press(move |_| reject_pending_approval(model))
                        .child("Reject"),
                )
                .child(
                    Button::new()
                        .filled()
                        .on_press(move |_| approve_pending_approval(model))
                        .child("Approve"),
                )
                .into(),
        ],
    };
    Popup::new().children(children)
}

fn queue_item(title: String, artist: String) -> Rect {
    rect()
        .width(Size::fill())
        .spacing(2.)
        .child(
            label()
                .width(Size::fill())
                .max_lines(1)
                .text_overflow(TextOverflow::Ellipsis)
                .text(title),
        )
        .child(
            label()
                .width(Size::fill())
                .max_lines(1)
                .text_overflow(TextOverflow::Ellipsis)
                .font_size(14.)
                .color(MUTED_TEXT)
                .text(artist),
        )
}

fn main() {
    pretty_env_logger::init();

    // Freya runs its own executor. The backends start their own runtimes;
    // this one only drives UI-side timers such as the clock.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("failed to start tokio runtime");
    let _guard = rt.enter();

    // Decorations are a window-creation attribute, so read this before
    // launch; later changes are applied live from the settings dialog.
    let hide_titlebar = persistence::Database::open()
        .ok()
        .and_then(|db| db.hide_titlebar().ok())
        .unwrap_or(false);

    launch(
        LaunchConfig::new().with_window(
            WindowConfig::new(app)
                .with_title("Guest Info Display")
                .with_app_id("xyz.mooshq.GuestInfoDisplay")
                .with_size(1280., 800.)
                .with_background((10, 16, 51))
                .with_decorations(!hide_titlebar),
        ),
    )
}
