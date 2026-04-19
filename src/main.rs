use gpui::{
    App, Application, Context, IntoElement, Render, SharedString, Styled, Window, WindowOptions,
    div, prelude::*,
};
use gpui_component::{
    Root, TitleBar,
    button::{Button, ButtonVariants},
};

struct HelloApp {
    text: SharedString,
}

impl Render for HelloApp {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(
                TitleBar::new().child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child("Guest Info Display"),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .items_center()
                    .justify_center()
                    .child(format!("Hello, {}!", &self.text))
                    .child(
                        Button::new("ok")
                            .primary()
                            .label("Let's Go!")
                            .on_click(|_, _, _| println!("Clicked!")),
                    ),
            )
    }
}

fn main() {
    Application::new()
        .with_assets(gpui_component_assets::Assets)
        .run(|cx: &mut App| {
            // CRITICAL: we must initialize gpui components.
            gpui_component::init(cx);

            cx.open_window(
                WindowOptions {
                    titlebar: Some(TitleBar::title_bar_options()),
                    ..Default::default()
                },
                |window, cx| {
                    let app = cx.new(|_| HelloApp {
                        text: "GPUI World".into(),
                    });

                    cx.new(|cx| Root::new(app, window, cx))
                },
            )
            .unwrap();
        });
}
