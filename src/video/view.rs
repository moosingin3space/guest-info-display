//! The Freya component behind a `video` node.

use std::cell::RefCell;
use std::rc::Rc;

use freya::prelude::*;

use super::paint::Painter;
use super::player::{Config, Player, Status};
use crate::inhibitor::Inhibitor;
use crate::layout::schema::Fit;

/// One mounted `video` node.
///
/// Keyed on its config, so a layout edit that changes the playlist or its
/// options remounts it with a fresh pipeline; an unrelated re-render of the
/// layout leaves the clip playing where it was.
#[derive(Clone)]
pub struct VideoView {
    pub config: Config,
    pub fit: Fit,
    pub inhibitor: Inhibitor,
    /// Placeholder text color, for when there is nothing to show.
    pub muted_text: (u8, u8, u8, u8),
}

impl PartialEq for VideoView {
    fn eq(&self, other: &Self) -> bool {
        self.config == other.config && self.fit == other.fit && self.muted_text == other.muted_text
    }
}

impl Component for VideoView {
    fn render(&self) -> impl IntoElement {
        let fit = self.fit;
        let muted_text = self.muted_text;
        let player = use_hook({
            let config = self.config.clone();
            let inhibitor = self.inhibitor.clone();
            move || {
                Rc::new(match super::available() {
                    true => Player::start(config, inhibitor.video_lease()),
                    false => Err("video playback is unavailable on this device".to_string()),
                })
            }
        });

        // Each new frame bumps this, re-rendering only this component.
        let mut frames = use_state(|| 0u64);
        use_hook({
            let player = player.clone();
            move || {
                if let Ok(player) = &*player {
                    let wake = player.wake();
                    spawn(async move {
                        while wake.recv().await.is_ok() {
                            *frames.write() += 1;
                        }
                    });
                }
            }
        });
        use_drop({
            let player = player.clone();
            move || {
                if let Ok(player) = &*player {
                    player.stop();
                }
            }
        });
        let painter = use_hook(|| Rc::new(RefCell::new(Painter::default())));

        let player = match &*player {
            Ok(player) => player,
            Err(why) => return notice(why.clone(), muted_text),
        };
        if let Status::Failed(why) = &*player.shared().status.lock().unwrap() {
            return notice(why.clone(), muted_text);
        }

        let shared = player.shared().clone();
        rect().expanded().child(
            canvas(RenderCallback::new(move |ctx: &mut CanvasContext| {
                painter.borrow_mut().paint(ctx, &shared, fit);
            }))
            // A canvas repaints only when it changes, and its callback always
            // compares equal, so a new key per frame is what forces the paint.
            .key(*frames.read())
            .expanded(),
        )
    }

    fn render_key(&self) -> DiffKey {
        DiffKey::from(&(&self.config, self.fit))
    }
}

fn notice(text: String, color: (u8, u8, u8, u8)) -> Rect {
    rect().expanded().center().padding(16.).child(
        label()
            .color(color)
            .text_align(TextAlign::Center)
            .text(text),
    )
}
