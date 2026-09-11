use std::time::Duration;

use freya::prelude::*;

fn main() {
    // Freya runs its own executor; our backend crates need a Tokio context.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let _guard = rt.enter();

    launch(
        LaunchConfig::new().with_window(
            WindowConfig::new(app)
                .with_size(900., 600.)
                .with_title("Freya spike"),
        ),
    )
}

fn app() -> impl IntoElement {
    let mut ticks = use_state(|| 0u64);

    // Proves a Freya-spawned task can drive state from a Tokio timer.
    use_hook(move || {
        spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                *ticks.write() += 1;
            }
        });
    });

    rect()
        .expanded()
        .center()
        .background((10, 16, 51))
        .color((255, 255, 255))
        .spacing(12.)
        .child(label().text("Guest Info Display").font_size(48.))
        .child(label().text(format!("ticks: {}", ticks.read())).font_size(24.))
}
