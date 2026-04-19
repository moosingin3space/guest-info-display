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

struct GuestInfoDisplay {
    now: DateTime<Local>,
    _clock_task: Task<()>,
}

impl GuestInfoDisplay {
    fn new(cx: &mut Context<Self>) -> Self {
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
            _clock_task: clock_task,
        }
    }
}

impl Render for GuestInfoDisplay {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let date_str: SharedString = self.now.format("%A, %B %-d").to_string().into();
        let time_str: SharedString = self.now.format("%H:%M:%S").to_string().into();

        let surface = hsla(0.0, 0.0, 1.0, 0.06);
        let surface_border = hsla(0.0, 0.0, 1.0, 0.12);
        let muted_text = hsla(0.0, 0.0, 1.0, 0.55);

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
                                                            .child("Song Title"),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_xl()
                                                            .text_color(muted_text)
                                                            .child("Artist Name"),
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
                                    .child(
                                        v_flex()
                                            .gap_3()
                                            .child(queue_item("Track Two", "Artist Name"))
                                            .child(queue_item("Track Three", "Artist Name"))
                                            .child(queue_item("Track Four", "Artist Name"))
                                            .child(queue_item("Track Five", "Artist Name"))
                                            .child(queue_item("Track Six", "Artist Name")),
                                    ),
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
                                            .child("WiFi"),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .size(px(220.))
                                            .rounded(px(12.))
                                            .bg(hsla(0.0, 0.0, 1.0, 0.08))
                                            .child(div().text_color(muted_text).child("QR code")),
                                    ),
                            )
                            .child(
                                Button::new("settings")
                                    .ghost()
                                    .icon(Icon::new(IconName::Settings).text_color(white()))
                                    .large()
                                    .tooltip("Settings")
                                    .on_click(|_, _, _| println!("Settings clicked")),
                            ),
                    ),
            )
    }
}

fn queue_item(title: &'static str, artist: &'static str) -> impl IntoElement {
    v_flex().gap_0p5().child(div().child(title)).child(
        div()
            .text_sm()
            .text_color(hsla(0.0, 0.0, 1.0, 0.55))
            .child(artist),
    )
}

fn main() {
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
