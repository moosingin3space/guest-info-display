use std::collections::HashMap;
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
mod persistence;
mod qr_code;
mod settings_dialog;
mod spotify;

struct GuestInfoDisplay {
    now: DateTime<Local>,
    db: persistence::Database,
    wifi_creds: Option<persistence::WifiCredentials>,
    spotify: spotify::SpotifyHandle,
    current_track: Option<spotify::SpotifyTrackInfo>,
    queue: Vec<spotify::SpotifyTrackInfo>,
    covers: HashMap<String, Arc<RenderImage>>,
    is_playing: bool,
    inhibitor: inhibitor::Inhibitor,
    _clock_task: Task<()>,
    _spotify_task: Task<()>,
}

impl GuestInfoDisplay {
    fn new(cx: &mut Context<Self>) -> Self {
        let db = persistence::Database::open().expect("failed to open settings database");
        let wifi_creds = db.wifi_credentials().ok().flatten();
        let device_id = db
            .spotify_device_id()
            .expect("failed to load spotify device id");
        let audio_device = db
            .audio_device_name()
            .expect("failed to load audio device name");

        let spotify = spotify::start(device_id, audio_device);
        let spotify_task = spawn_spotify_task(spotify.events.clone(), cx);

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

        Self {
            now: Local::now(),
            db,
            wifi_creds,
            spotify,
            current_track: None,
            queue: Vec::new(),
            covers: HashMap::new(),
            is_playing: false,
            inhibitor: inhibitor::Inhibitor::new(),
            _clock_task: clock_task,
            _spotify_task: spotify_task,
        }
    }

    /// Replace the running spotify thread with a new one bound to the persisted
    /// audio-device selection. Called from the settings dialog after the user
    /// picks a different output device.
    fn restart_spotify(&mut self, cx: &mut Context<Self>) {
        let device_id = self
            .db
            .spotify_device_id()
            .expect("failed to load spotify device id");
        let audio_device = self
            .db
            .audio_device_name()
            .expect("failed to load audio device name");

        // Replace the handle first; dropping the old `_shutdown` sender ends
        // the previous discovery loop and the rodio sink it owns.
        self.spotify = spotify::start(device_id, audio_device);
        self.current_track = None;
        self.queue.clear();
        self.covers.clear();
        self.is_playing = false;
        self.inhibitor.set(false);

        self._spotify_task = spawn_spotify_task(self.spotify.events.clone(), cx);
        cx.notify();
    }
}

fn spawn_spotify_task(
    events: async_channel::Receiver<spotify::Event>,
    cx: &mut Context<GuestInfoDisplay>,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        while let Ok(event) = events.recv().await {
            if this
                .update(cx, |this, cx| {
                    match event {
                        spotify::Event::StateChanged => {
                            if let Ok(sp) = this.spotify.state.lock() {
                                this.current_track = sp.current.clone();
                                this.queue = sp.queue.clone();
                                if sp.is_playing != this.is_playing {
                                    this.is_playing = sp.is_playing;
                                    this.inhibitor.set(sp.is_playing);
                                }
                            }
                        }
                        spotify::Event::CoverLoaded(cover) => {
                            if let Some(image) = build_render_image(&cover) {
                                this.covers.insert(cover.url, image);
                            }
                        }
                        spotify::Event::CoversCleared => this.covers.clear(),
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

        div()
            .size_full()
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
                            .pb_8()
                            .child(
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
                                    ),
                            )
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
                                                let audio_devices =
                                                    audio_devices::output_device_names();
                                                let entity = cx.entity().downgrade();
                                                settings_dialog::open(
                                                    existing_wifi,
                                                    existing_audio,
                                                    audio_devices,
                                                    Arc::new(move |values, cx| {
                                                        entity
                                                            .update(cx, |this, cx| {
                                                                this.db
                                                                    .set_wifi_credentials(
                                                                        &values.wifi,
                                                                    )
                                                                    .ok();
                                                                this.wifi_creds =
                                                                    Some(values.wifi);

                                                                let prev = this
                                                                    .db
                                                                    .audio_device_name()
                                                                    .ok()
                                                                    .flatten();
                                                                if prev != values.audio_device {
                                                                    this.db
                                                                        .set_audio_device_name(
                                                                            values
                                                                                .audio_device
                                                                                .as_deref(),
                                                                        )
                                                                        .ok();
                                                                    this.restart_spotify(cx);
                                                                }
                                                                cx.notify();
                                                            })
                                                            .ok();
                                                    }),
                                                    window,
                                                    cx,
                                                );
                                            })),
                                    ),
                            ),
                    ),
            )
            .children(dialog_layer)
    }
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
