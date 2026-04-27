use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local};
use gpui::{
    App, Application, Context, FontWeight, ImageSource, IntoElement, ObjectFit, Render, RenderImage,
    SharedString, Styled, Task, Timer, Window, WindowOptions, black, div, hsla, img,
    linear_color_stop, linear_gradient, prelude::*, px, rgb, white,
};
use gpui_component::{
    Icon, IconName, Root, Sizable, TitleBar,
    button::{Button, ButtonVariants},
    h_flex, v_flex,
};
use image::{Frame, RgbaImage};
use smallvec::SmallVec;

mod audio_devices;
mod inhibitor;
mod multi_screen;
mod persistence;
mod qr_code;
mod settings_dialog;
mod spotify;

/// Whichever task tree is producing `(state, events)` for the GPUI consumer.
/// Holding the appropriate variant keeps that backend alive; replacing it
/// (e.g. on role switch) drops the previous handles and aborts their tasks.
enum BackendHandle {
    Primary(spotify::SpotifyHandle),
    Reflection(multi_screen::ReflectionHandle),
}

struct GuestInfoDisplay {
    now: DateTime<Local>,
    db: persistence::Database,
    wifi_creds: Option<persistence::WifiCredentials>,
    role: persistence::Role,
    /// Active backend producing the spotify event stream. `None` when role is
    /// reflection but no primary is paired yet — the UI shows the "Primary
    /// unavailable" empty state in that case.
    backend: Option<BackendHandle>,
    /// iroh endpoint + RPC service. Identity is symmetric across roles, so we
    /// keep one runtime alive for the lifetime of the model regardless of role.
    _multi_screen: multi_screen::MultiScreenHandle,
    /// Pending pairing approval requests from unknown peers. Head is rendered
    /// as an overlay; user clicks Approve/Reject to advance.
    pending_approvals: VecDeque<multi_screen::PendingApproval>,
    /// Primaries seen on the LAN via mDNS. Drained when settings is opened.
    discovered_primaries: HashSet<iroh::EndpointId>,
    current_track: Option<spotify::SpotifyTrackInfo>,
    queue: Vec<spotify::SpotifyTrackInfo>,
    covers: HashMap<String, Arc<RenderImage>>,
    is_playing: bool,
    /// Reflection-only: tracks whether the active subscription has delivered
    /// a snapshot recently. `false` after the session drops; flipped back to
    /// `true` on the next snapshot. Always `true` in primary role.
    connected: bool,
    inhibitor: inhibitor::Inhibitor,
    _clock_task: Task<()>,
    _spotify_task: Option<Task<()>>,
    _approvals_task: Task<()>,
    _discovery_task: Task<()>,
}

impl GuestInfoDisplay {
    fn new(cx: &mut Context<Self>) -> Self {
        let db = persistence::Database::open().expect("failed to open settings database");
        let wifi_creds = db.wifi_credentials().ok().flatten();
        let role = db.role().expect("failed to load role");

        let secret = db.node_secret().expect("failed to load iroh identity");
        log::debug!("multi_screen: EndpointId = {}", secret.public());
        let multi_screen = multi_screen::start(secret);

        // Seed the inbound trusted set from already-paired reflections so they
        // can subscribe right after startup.
        if let Ok(inbound) = db.paired_peers(persistence::Direction::Inbound) {
            let ids: HashSet<_> = inbound.into_iter().map(|p| p.endpoint_id).collect();
            multi_screen.seed_inbound_trusted(ids);
        }

        let approvals_rx = multi_screen.approvals();
        let approvals_task = cx.spawn(async move |this, cx| {
            while let Ok(approval) = approvals_rx.recv().await {
                if this
                    .update(cx, |this, cx| {
                        this.pending_approvals.push_back(approval);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        let discovered_rx = multi_screen.discovered();
        let discovery_task = cx.spawn(async move |this, cx| {
            while let Ok(node) = discovered_rx.recv().await {
                if this
                    .update(cx, |this, cx| {
                        if this.discovered_primaries.insert(node.endpoint_id) {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        // Clock task: updates the displayed time every second.
        let clock_task = cx.spawn(async move |this, cx| {
            loop {
                Timer::after(Duration::from_secs(1)).await;
                if this
                    .update(cx, |this, cx| {
                        this.now = Local::now();
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        let mut this = Self {
            now: Local::now(),
            db,
            wifi_creds,
            role,
            backend: None,
            _multi_screen: multi_screen,
            pending_approvals: VecDeque::new(),
            discovered_primaries: HashSet::new(),
            current_track: None,
            queue: Vec::new(),
            covers: HashMap::new(),
            is_playing: false,
            connected: true,
            inhibitor: inhibitor::Inhibitor::new(),
            _clock_task: clock_task,
            _spotify_task: None,
            _approvals_task: approvals_task,
            _discovery_task: discovery_task,
        };
        this.rebuild_backend(cx);
        this
    }

    /// Tear down whatever backend is running and start the one matching
    /// `self.role` and the persisted audio-device selection. Called on role
    /// change, on audio-device change, and at startup.
    fn rebuild_backend(&mut self, cx: &mut Context<Self>) {
        // Drop in-flight work first. Dropping the BackendHandle closes the
        // backend's shutdown channel — Primary exits the librespot discovery
        // loop and releases the rodio sink; Reflection cancels the dial loop.
        // Dropping the GPUI task cancels the event listener so no late updates
        // land in `self`.
        self._spotify_task = None;
        self.backend = None;

        self.current_track = None;
        self.queue.clear();
        self.covers.clear();
        self.is_playing = false;
        // Primaries are always "connected" (they generate their own state).
        // Reflections start disconnected and flip on the first snapshot.
        self.connected = matches!(self.role, persistence::Role::Primary);
        self.inhibitor.set(false);

        let new_backend = match self.role {
            persistence::Role::Primary => {
                let device_id = self
                    .db
                    .spotify_device_id()
                    .expect("failed to load spotify device id");
                let audio_device = self
                    .db
                    .audio_device_name()
                    .expect("failed to load audio device name");
                Some(BackendHandle::Primary(spotify::start(
                    device_id,
                    audio_device,
                )))
            }
            persistence::Role::Reflection => self
                .db
                .paired_primary()
                .ok()
                .flatten()
                .map(|primary| {
                    BackendHandle::Reflection(
                        self._multi_screen.start_reflection(primary.endpoint_id),
                    )
                }),
        };

        if let Some(backend) = new_backend {
            let (state, events) = match &backend {
                BackendHandle::Primary(h) => (h.state.clone(), h.events.clone()),
                BackendHandle::Reflection(h) => (h.state.clone(), h.events.clone()),
            };
            self._spotify_task = Some(spawn_spotify_task(state, events, cx));
            self.backend = Some(backend);
        }

        cx.notify();
    }
}

fn spawn_spotify_task(
    state: spotify::SharedSpotifyState,
    events: async_channel::Receiver<spotify::Event>,
    cx: &mut Context<GuestInfoDisplay>,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        while let Ok(event) = events.recv().await {
            if this
                .update(cx, |this, cx| {
                    match event {
                        spotify::Event::StateChanged => {
                            if let Ok(sp) = state.lock() {
                                this.current_track = sp.current.clone();
                                this.queue = sp.queue.clone();
                                if sp.is_playing != this.is_playing {
                                    this.is_playing = sp.is_playing;
                                    this.inhibitor.set(sp.is_playing);
                                }
                                this._multi_screen.broadcast_state(sp.clone());
                            }
                        }
                        spotify::Event::CoverLoaded(cover) => {
                            this._multi_screen
                                .broadcast_cover(cover.url.clone(), cover.encoded.clone());
                            if let Some(image) = build_render_image(&cover) {
                                this.covers.insert(cover.url, image);
                            }
                        }
                        spotify::Event::CoversCleared => {
                            this._multi_screen.broadcast_covers_cleared();
                            this.covers.clear();
                        }
                        spotify::Event::ConnectionLost => {
                            this.connected = false;
                            // is_playing was already cleared via the
                            // preceding StateChanged, which also released the
                            // inhibit; nothing else to do here.
                        }
                        spotify::Event::ConnectionRestored => {
                            this.connected = true;
                        }
                    }
                    cx.notify();
                })
                .is_err()
            {
                break;
            }
        }
    })
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

fn build_render_image(cover: &spotify::CoverImage) -> Option<Arc<RenderImage>> {
    let buffer = RgbaImage::from_raw(cover.width, cover.height, cover.bgra.clone())?;
    Some(Arc::new(RenderImage::new(SmallVec::from_elem(
        Frame::new(buffer),
        1,
    ))))
}

impl Render for GuestInfoDisplay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Important: the dialog layer must be rendered, or else dialogs will not spawn.
        let dialog_layer = Root::render_dialog_layer(window, cx);

        let date_str: SharedString = self.now.format("%A, %B %-d").to_string().into();
        let time_str: SharedString = self.now.format("%H:%M:%S").to_string().into();

        let track_name: SharedString = self
            .current_track
            .as_ref()
            .map(|t| t.name.clone())
            .unwrap_or_else(|| "Nothing playing".to_string())
            .into();
        let track_artist: SharedString = self
            .current_track
            .as_ref()
            .map(|t| t.artists.clone())
            .unwrap_or_default()
            .into();
        let surface = hsla(0.0, 0.0, 1.0, 0.06);
        let surface_border = hsla(0.0, 0.0, 1.0, 0.12);
        let muted_text = hsla(0.0, 0.0, 1.0, 0.55);

        let show_disconnected =
            matches!(self.role, persistence::Role::Reflection) && !self.connected;

        div()
            .size_full()
            .relative()
            .child(
                v_flex()
                    .size_full()
                    .text_color(white())
                    .bg(linear_gradient(
                        180.0,
                        linear_color_stop(rgb(0x0a1033), 0.0),
                        linear_color_stop(rgb(0x3b1d6e), 1.0),
                    ))
                    .child(
                        TitleBar::new().child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .text_color(black())
                                .border_2()
                                .child("Guest Info Display"),
                        ),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .px_8()
                            .py_5()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_2xl()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(date_str),
                            )
                            .child(
                                div()
                                    .text_3xl()
                                    .font_family("Adwaita Mono")
                                    .font_weight(FontWeight::BOLD)
                                    .child(time_str),
                            ),
                    )
                    .child(
                        h_flex()
                            .flex_1()
                            .w_full()
                            .gap_6()
                            .px_8()
                            .pb_2()
                            .child(if show_disconnected {
                                disconnected_card(surface, surface_border, muted_text)
                                    .into_any_element()
                            } else {
                                h_flex()
                                    .flex_1()
                                    .h_full()
                                    .overflow_hidden()
                                    .gap_8()
                                    .rounded(px(16.))
                                    .bg(surface)
                                    .border_1()
                                    .border_color(surface_border)
                                    .p_6()
                                    .items_start()
                                    .child(
                                        v_flex()
                                            .flex_1()
                                            .h_full()
                                            .overflow_hidden()
                                            .gap_4()
                                            .child(
                                                div()
                                                    .text_color(muted_text)
                                                    .text_3xl()
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .child("Now Playing"),
                                            )
                                            .child(
                                                h_flex()
                                                    .w_full()
                                                    .overflow_hidden()
                                                    .gap_5()
                                                    .items_center()
                                                    .child({
                                                        let cover = self
                                                            .current_track
                                                            .as_ref()
                                                            .and_then(|t| t.cover_url.as_ref())
                                                            .and_then(|url| self.covers.get(url))
                                                            .cloned();
                                                        let container = div()
                                                            .size(px(220.))
                                                            .rounded(px(12.))
                                                            .overflow_hidden();
                                                        if let Some(image) = cover {
                                                            container.child(
                                                                img(ImageSource::Render(image))
                                                                    .object_fit(ObjectFit::Cover)
                                                                    .size_full(),
                                                            )
                                                        } else {
                                                            container
                                                                .flex()
                                                                .items_center()
                                                                .justify_center()
                                                                .bg(hsla(0.0, 0.0, 1.0, 0.08))
                                                                .child(
                                                                    div()
                                                                        .text_color(muted_text)
                                                                        .child("Cover Art"),
                                                                )
                                                        }
                                                    })
                                                    .child(
                                                        v_flex()
                                                            .flex_1()
                                                            .overflow_hidden()
                                                            .gap_2()
                                                            .child(
                                                                div()
                                                                    .w_full()
                                                                    .truncate()
                                                                    .text_2xl()
                                                                    .font_weight(FontWeight::BOLD)
                                                                    .child(track_name),
                                                            )
                                                            .child(
                                                                div()
                                                                    .w_full()
                                                                    .truncate()
                                                                    .text_xl()
                                                                    .text_color(muted_text)
                                                                    .child(track_artist),
                                                            ),
                                                    ),
                                            ),
                                    )
                                    .child(
                                        v_flex()
                                            .w(px(280.))
                                            .h_full()
                                            .gap_4()
                                            .child(
                                                div()
                                                    .text_color(muted_text)
                                                    .text_sm()
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .child("Up Next"),
                                            )
                                            .when(!self.queue.is_empty(), |el| {
                                                el.child(
                                                    v_flex().gap_3().children(
                                                        self.queue.iter().take(5).map(|t| {
                                                            if t.is_resolved() {
                                                                queue_item(
                                                                    t.name.clone().into(),
                                                                    t.artists.clone().into(),
                                                                )
                                                            } else {
                                                                queue_item("--".into(), "".into())
                                                            }
                                                        }),
                                                    ),
                                                )
                                            })
                                            .when(self.queue.is_empty(), |el| {
                                                el.child(
                                                    div()
                                                        .text_sm()
                                                        .text_color(muted_text)
                                                        .child("—"),
                                                )
                                            }),
                                    )
                                    .into_any_element()
                            })
                            .child(
                                v_flex()
                                    .w(px(320.))
                                    .h_full()
                                    .rounded(px(16.))
                                    .bg(surface)
                                    .border_1()
                                    .border_color(surface_border)
                                    .p_6()
                                    .justify_between()
                                    .items_center()
                                    .child(
                                        v_flex()
                                            .w_full()
                                            .gap_4()
                                            .items_center()
                                            .child(
                                                div()
                                                    .text_xl()
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .child("Scan to connect to Wi-Fi"),
                                            )
                                            .child({
                                                let container = div()
                                                    .size(px(220.))
                                                    .rounded(px(12.))
                                                    .overflow_hidden();
                                                if let Some(ref creds) = self.wifi_creds {
                                                    container.child(qr_code::wifi_qr_element(creds))
                                                } else {
                                                    container
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .bg(hsla(0.0, 0.0, 1.0, 0.08))
                                                        .child(
                                                            div()
                                                                .text_color(muted_text)
                                                                .child("Not configured"),
                                                        )
                                                }
                                            }),
                                    )
                                    .child(
                                        Button::new("settings")
                                            .ghost()
                                            .icon(Icon::new(IconName::Settings).text_color(white()))
                                            .large()
                                            .tooltip("Settings")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                let existing_wifi =
                                                    this.db.wifi_credentials().ok().flatten();
                                                let existing_audio =
                                                    this.db.audio_device_name().ok().flatten();
                                                let existing_role = this.role;
                                                let audio_devices =
                                                    audio_devices::output_device_names();
                                                let discovered: Vec<_> =
                                                    this.discovered_primaries.iter().copied().collect();
                                                let paired_primary =
                                                    this.db.paired_primary().ok().flatten();
                                                let paired_reflections = this
                                                    .db
                                                    .paired_peers(persistence::Direction::Inbound)
                                                    .unwrap_or_default();
                                                let entity = cx.entity().downgrade();
                                                let entity_pair = entity.clone();
                                                let entity_remove = entity.clone();
                                                let entity_forget = entity.clone();
                                                settings_dialog::SettingsDialog {
                                                    existing_wifi,
                                                    existing_audio,
                                                    existing_role,
                                                    audio_devices,
                                                    discovered_primaries: discovered,
                                                    paired_primary,
                                                    paired_reflections,
                                                    on_save: Arc::new(move |values, cx| {
                                                        entity
                                                            .update(cx, |this, cx| {
                                                                this.db
                                                                    .set_wifi_credentials(
                                                                        &values.wifi,
                                                                    )
                                                                    .ok();
                                                                this.wifi_creds =
                                                                    Some(values.wifi);

                                                                let role_changed =
                                                                    values.role != this.role;
                                                                if role_changed {
                                                                    this.db
                                                                        .set_role(values.role)
                                                                        .ok();
                                                                    this.role = values.role;
                                                                }

                                                                let prev_audio = this
                                                                    .db
                                                                    .audio_device_name()
                                                                    .ok()
                                                                    .flatten();
                                                                let audio_changed =
                                                                    prev_audio != values.audio_device;
                                                                if audio_changed {
                                                                    this.db
                                                                        .set_audio_device_name(
                                                                            values
                                                                                .audio_device
                                                                                .as_deref(),
                                                                        )
                                                                        .ok();
                                                                }

                                                                if role_changed || audio_changed {
                                                                    this.rebuild_backend(cx);
                                                                } else {
                                                                    cx.notify();
                                                                }
                                                            })
                                                            .ok();
                                                    }),
                                                    on_pair: Arc::new(move |primary_id, cx| {
                                                        let _ = entity_pair.update(cx, |this, cx| {
                                                            let recv = this
                                                                ._multi_screen
                                                                .request_pairing(
                                                                    primary_id,
                                                                    local_hostname(),
                                                                );
                                                            cx.spawn(async move |this, cx| {
                                                                let response = match recv.recv().await {
                                                                    Ok(Some(r)) => r,
                                                                    _ => {
                                                                        log::warn!(
                                                                            "pairing: no response from primary"
                                                                        );
                                                                        return;
                                                                    }
                                                                };
                                                                if !matches!(
                                                                    response,
                                                                    multi_screen::PairingResponse::Accepted
                                                                ) {
                                                                    log::info!("pairing: rejected by primary");
                                                                    return;
                                                                }
                                                                let _ = this.update(cx, |this, cx| {
                                                                    let label = format!(
                                                                        "Primary {}",
                                                                        primary_id.fmt_short()
                                                                    );
                                                                    if let Err(e) = this.db.add_paired_peer(
                                                                        primary_id,
                                                                        &label,
                                                                        persistence::Direction::Outbound,
                                                                    ) {
                                                                        log::warn!(
                                                                            "pairing: persist failed: {e}"
                                                                        );
                                                                        return;
                                                                    }
                                                                    this.rebuild_backend(cx);
                                                                });
                                                            })
                                                            .detach();
                                                        });
                                                    }),
                                                    on_remove_reflection: Arc::new(move |id, cx| {
                                                        let _ = entity_remove.update(cx, |this, cx| {
                                                            if let Err(e) =
                                                                this.db.remove_paired_peer(id)
                                                            {
                                                                log::warn!(
                                                                    "remove reflection: persist failed: {e}"
                                                                );
                                                                return;
                                                            }
                                                            this._multi_screen.remove_inbound_trusted(id);
                                                            this._multi_screen.disconnect_subscriber(id);
                                                            cx.notify();
                                                        });
                                                    }),
                                                    on_forget_primary: Arc::new(move |cx| {
                                                        let _ = entity_forget.update(cx, |this, cx| {
                                                            if let Ok(Some(p)) = this.db.paired_primary() {
                                                                if let Err(e) = this
                                                                    .db
                                                                    .remove_paired_peer(p.endpoint_id)
                                                                {
                                                                    log::warn!(
                                                                        "forget primary: persist failed: {e}"
                                                                    );
                                                                    return;
                                                                }
                                                            }
                                                            this.rebuild_backend(cx);
                                                        });
                                                    }),
                                                    window,
                                                    cx,
                                                }
                                                .run();
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .pb_4()
                            .text_sm()
                            .text_color(muted_text)
                            .text_center()
                            .child(match self.role {
                                persistence::Role::Primary => "Primary",
                                persistence::Role::Reflection => "Reflection",
                            }),
                    ),
            )
            .when(!self.pending_approvals.is_empty(), |el| {
                el.child(approval_overlay(self, cx, surface_border, muted_text))
            })
            .children(dialog_layer)
    }
}

fn disconnected_card(
    surface: gpui::Hsla,
    surface_border: gpui::Hsla,
    muted_text: gpui::Hsla,
) -> impl IntoElement {
    h_flex()
        .flex_1()
        .h_full()
        .rounded(px(16.))
        .bg(surface)
        .border_1()
        .border_color(surface_border)
        .items_center()
        .justify_center()
        .child(
            div()
                .text_color(muted_text)
                .text_2xl()
                .font_weight(FontWeight::SEMIBOLD)
                .child("Primary unavailable"),
        )
}

fn approval_overlay(
    this: &GuestInfoDisplay,
    cx: &mut Context<GuestInfoDisplay>,
    surface_border: gpui::Hsla,
    muted_text: gpui::Hsla,
) -> impl IntoElement {
    let approval = this
        .pending_approvals
        .front()
        .expect("approval_overlay called with empty queue");
    let friendly_name: SharedString = approval.friendly_name.clone().into();
    let endpoint_short: SharedString = format!("{}", approval.endpoint_id.fmt_short()).into();

    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(hsla(0.0, 0.0, 0.0, 0.55))
        .child(
            v_flex()
                .gap_4()
                .p_8()
                .w(px(420.))
                .rounded(px(16.))
                .bg(rgb(0x1a1f3d))
                .border_1()
                .border_color(surface_border)
                .child(
                    div()
                        .text_xl()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Pair this device?"),
                )
                .child(div().child(friendly_name))
                .child(
                    div()
                        .text_sm()
                        .text_color(muted_text)
                        .child(endpoint_short),
                )
                .child(
                    h_flex()
                        .gap_3()
                        .justify_end()
                        .child(
                            Button::new("approval-reject")
                                .outline()
                                .label("Reject")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(approval) = this.pending_approvals.pop_front() {
                                        let _ = approval
                                            .respond
                                            .send(multi_screen::PairingResponse::Rejected);
                                        cx.notify();
                                    }
                                })),
                        )
                        .child(
                            Button::new("approval-approve")
                                .primary()
                                .label("Approve")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let Some(approval) = this.pending_approvals.pop_front() else {
                                        return;
                                    };
                                    if let Err(e) = this.db.add_paired_peer(
                                        approval.endpoint_id,
                                        &approval.friendly_name,
                                        persistence::Direction::Inbound,
                                    ) {
                                        log::warn!("approval: persist failed: {e}");
                                    }
                                    this._multi_screen.add_inbound_trusted(approval.endpoint_id);
                                    let _ = approval
                                        .respond
                                        .send(multi_screen::PairingResponse::Accepted);
                                    cx.notify();
                                })),
                        ),
                ),
        )
}

fn queue_item(title: SharedString, artist: SharedString) -> impl IntoElement {
    v_flex()
        .w_full()
        .gap_0p5()
        .child(div().w_full().truncate().child(title))
        .child(
            div()
                .w_full()
                .truncate()
                .text_sm()
                .text_color(hsla(0.0, 0.0, 1.0, 0.55))
                .child(artist),
        )
}

fn main() {
    pretty_env_logger::init();
    Application::new()
        .with_assets(gpui_component_assets::Assets)
        .run(|cx: &mut App| {
            gpui_component::init(cx);

            cx.open_window(
                WindowOptions {
                    titlebar: Some(TitleBar::title_bar_options()),
                    ..Default::default()
                },
                |window, cx| {
                    let app = cx.new(GuestInfoDisplay::new);
                    cx.new(|cx| Root::new(app, window, cx))
                },
            )
            .unwrap();
        });
}
