//! The Freya component behind a `video` node.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use freya::prelude::*;

use super::paint::Painter;
use super::player::{Config, Player, Status};
use crate::inhibitor::Inhibitor;
use crate::layout::schema::Video;

/// One mounted `video` node.
///
/// Keyed on the node as the layout states it, so an edit to the playlist or
/// its options remounts it with a fresh pipeline. Which of its files are on
/// disk is *not* part of the key: when a reflection receives another clip, or
/// a host locates a missing one, the running player is told and the clip on
/// screen keeps playing.
#[derive(Clone)]
pub struct VideoView {
    pub node: Video,
    /// One entry per `node.items`: the file, or `None` while it is missing.
    pub files: Vec<Option<PathBuf>>,
    pub inhibitor: Inhibitor,
    /// Placeholder text color, for when there is nothing to show.
    pub muted_text: (u8, u8, u8, u8),
    /// This device mirrors a primary. Its video is then always silent: sound
    /// comes from the primary alone, as Spotify's does.
    pub reflection: bool,
}

impl PartialEq for VideoView {
    fn eq(&self, other: &Self) -> bool {
        self.node == other.node
            && self.files == other.files
            && self.muted_text == other.muted_text
            && self.reflection == other.reflection
    }
}

impl Component for VideoView {
    fn render(&self) -> impl IntoElement {
        let fit = self.node.fit;
        let muted_text = self.muted_text;
        let player = use_hook({
            let config = Config {
                node: self.node.id,
                paths: self.node.items.clone(),
                items: self.files.clone(),
                // Sound plays from the primary only, like the music; a
                // reflection does not even decode it.
                muted: self.node.muted || self.reflection,
                looping: self.node.looping,
                shuffle: self.node.shuffle,
                wait: self.node.wait_for_reflections,
            };
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
        // The file list the player last heard about.
        let told = use_hook({
            let files = self.files.clone();
            move || Rc::new(RefCell::new(files))
        });

        let player = match &*player {
            Ok(player) => player,
            Err(why) => return notice(why.clone(), muted_text),
        };
        if *told.borrow() != self.files {
            player.update_items(&self.files);
            *told.borrow_mut() = self.files.clone();
        }
        if let Status::Failed(why) = &*player.shared().status.lock().unwrap() {
            return notice(why.clone(), muted_text);
        }

        let waiting = match &*player.shared().status.lock().unwrap() {
            Status::Waiting { screens, progress } => Some(waiting_caption(*screens, *progress)),
            Status::Joining { ready, progress } => Some(joining_caption(*ready, *progress)),
            _ => None,
        };

        let shared = player.shared().clone();
        let view = rect().expanded().child(
            canvas(RenderCallback::new(move |ctx: &mut CanvasContext| {
                painter.borrow_mut().paint(ctx, &shared, fit);
            }))
            // A canvas repaints only when it changes, and its callback always
            // compares equal, so a new key per frame is what forces the paint.
            .key(*frames.read())
            .expanded(),
        );
        match waiting {
            // Over the frame on screen, so the host can see why it waits.
            Some(caption) => view.child(
                rect()
                    .position(Position::new_absolute().bottom(16.).left(16.))
                    .padding((6., 12.))
                    .corner_radius(8.)
                    .background((0, 0, 0, 160))
                    .child(label().color(muted_text).text(caption)),
            ),
            None => view,
        }
    }

    // The role is part of the key: a device that switches between primary
    // and reflection needs a player that leads or follows, and that plays
    // sound or does not, accordingly.
    fn render_key(&self) -> DiffKey {
        DiffKey::from(&(&self.node, self.reflection))
    }
}

/// "Waiting for 2 screens… 40%".
fn waiting_caption(screens: usize, progress: Option<f32>) -> String {
    let who = if screens == 1 {
        "1 screen".to_string()
    } else {
        format!("{screens} screens")
    };
    match progress {
        Some(done) => format!("Waiting for {who}… {:.0}%", done * 100.0),
        None => format!("Waiting for {who}…"),
    }
}

/// A reflection's side of the same wait: "Getting ready to start together… 40%".
fn joining_caption(ready: bool, progress: Option<f32>) -> String {
    match (ready, progress) {
        (true, _) => "Ready, waiting for the other screens…".to_string(),
        (false, Some(done)) => format!("Getting ready to start together… {:.0}%", done * 100.0),
        (false, None) => "Getting ready to start together…".to_string(),
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
