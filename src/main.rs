use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local};
use gpui::{
    App, Application, Context, FontWeight, IntoElement, Render, SharedString, Styled, Task, Timer,
    Window, WindowOptions, black, div, hsla, linear_color_stop, linear_gradient, prelude::*, px,
    rgb, white,
};
use gpui_component::{
    Icon, IconName, Root, Sizable, TitleBar,
    button::{Button, ButtonVariants},
    h_flex, v_flex,
};

mod persistence;
mod qr_code;
mod settings_dialog;
mod spotify;

struct GuestInfoDisplay {
    now: DateTime<Local>,
    db: persistence::Database,
    wifi_creds: Option<persistence::WifiCredentials>,
    spotify_state: spotify::SharedSpotifyState,
    current_track: Option<spotify::SpotifyTrackInfo>,
    next_track: Option<spotify::SpotifyTrackInfo>,
    _clock_task: Task<()>,
    _spotify_task: Task<()>,
}

impl GuestInfoDisplay {
    fn new(cx: &mut Context<Self>) -> Self {
        let handle = spotify::start();
        let spotify_state = handle.state.clone();
        let mut updates = handle.updates;

        // Dedicated task: wakes immediately when spotify state changes (no 1s lag).
        let spotify_task = cx.spawn(async move |this, cx| {
            while updates.recv().await.is_ok() {
                if this
                    .update(cx, |this, cx| {
                        if let Ok(sp) = this.spotify_state.lock() {
                            this.current_track = sp.current.clone();
                            this.next_track = sp.next.clone();
                        }
                        cx.notify();
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

        let db = persistence::Database::open().expect("failed to open settings database");
        let wifi_creds = db.wifi_credentials().ok().flatten();

        Self {
            now: Local::now(),
            db,
            wifi_creds,
            spotify_state,
            current_track: None,
            next_track: None,
            _clock_task: clock_task,
            _spotify_task: spotify_task,
        }
    }
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
        let next_name: SharedString = self
            .next_track
            .as_ref()
            .map(|t| t.name.clone())
            .unwrap_or_default()
            .into();
        let next_artist: SharedString = self
            .next_track
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
                                                    .gap_5()
                                                    .items_center()
                                                    .child(
                                                        div()
                                                            .flex()
                                                            .items_center()
                                                            .justify_center()
                                                            .size(px(220.))
                                                            .rounded(px(12.))
                                                            .bg(hsla(0.0, 0.0, 1.0, 0.08))
                                                            .child(
                                                                div()
                                                                    .text_color(muted_text)
                                                                    .child("Cover Art"),
                                                            ),
                                                    )
                                                    .child(
                                                        v_flex()
                                                            .flex_1()
                                                            .gap_2()
                                                            .child(
                                                                div()
                                                                    .text_2xl()
                                                                    .font_weight(FontWeight::BOLD)
                                                                    .child(track_name),
                                                            )
                                                            .child(
                                                                div()
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
                                            .when(self.next_track.is_some(), |el| {
                                                el.child(
                                                    v_flex()
                                                        .gap_3()
                                                        .child(queue_item(next_name, next_artist)),
                                                )
                                            })
                                            .when(self.next_track.is_none(), |el| {
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
                                                let existing =
                                                    this.db.wifi_credentials().ok().flatten();
                                                let entity = cx.entity().downgrade();
                                                settings_dialog::open(
                                                    existing,
                                                    Arc::new(move |creds, cx| {
                                                        entity
                                                            .update(cx, |this, cx| {
                                                                this.db
                                                                    .set_wifi_credentials(&creds)
                                                                    .ok();
                                                                this.wifi_creds = Some(creds);
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
    v_flex().gap_0p5().child(div().child(title)).child(
        div()
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
