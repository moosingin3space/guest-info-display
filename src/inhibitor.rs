//! XDG idle-inhibit portal integration. Holds an inhibit lock while anything
//! is playing — Spotify or any `video` node — so the screen does not blank.
//!
//! The portal proxy is async and the request handle has no synchronous `close`,
//! so we run a dedicated single-threaded tokio runtime in a background thread
//! and drive it via an async channel. Dropping the [`Inhibitor`] closes the
//! sender, which unblocks the loop and lets it close any held inhibit before
//! exiting.
//!
//! Each thing that can play reports for itself under its own [`Source`], and
//! the worker holds the lock while any source is active. Folding them into one
//! flag at the call sites would let the music stopping blank a screen that is
//! still showing a video.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};

use ashpd::desktop::Request;
use ashpd::desktop::inhibit::{InhibitFlags, InhibitOptions, InhibitProxy};
use async_channel::{Receiver, Sender, bounded};

const INHIBIT_REASON: &str = "Media playing";

/// Something that can keep the screen awake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    Spotify,
    /// One mounted `video` node, by [`VideoLease`] id.
    Video(u64),
}

#[derive(Clone)]
pub struct Inhibitor {
    cmd: Sender<(Source, bool)>,
    _thread: Arc<JoinHandle<()>>,
}

impl Inhibitor {
    /// Spawn the background thread that owns the portal connection.
    pub fn new() -> Self {
        let (cmd, rx) = bounded::<(Source, bool)>(32);
        let thread = thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("inhibitor tokio runtime");
            rt.block_on(run(rx));
        });
        Self {
            cmd,
            _thread: Arc::new(thread),
        }
    }

    /// Report whether `source` is currently playing. Idempotent — repeating a
    /// value is a no-op on the wire.
    pub fn set(&self, source: Source, playing: bool) {
        // try_send is fine: the channel is bounded and the worker keeps up
        // easily. A backpressure-induced drop would just delay one transition
        // by one event tick, which doesn't matter here.
        let _ = self.cmd.try_send((source, playing));
    }

    /// A fresh [`Source::Video`] for one player, released when dropped.
    pub fn video_lease(&self) -> VideoLease {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        VideoLease {
            inhibitor: self.clone(),
            source: Source::Video(NEXT.fetch_add(1, Ordering::Relaxed)),
        }
    }
}

/// One video player's say in the inhibit. Dropping it withdraws that say, so
/// a player torn down mid-clip cannot leave the screen pinned awake.
pub struct VideoLease {
    inhibitor: Inhibitor,
    source: Source,
}

impl VideoLease {
    pub fn set(&self, playing: bool) {
        self.inhibitor.set(self.source, playing);
    }
}

impl Drop for VideoLease {
    fn drop(&mut self) {
        self.inhibitor.set(self.source, false);
    }
}

async fn run(rx: Receiver<(Source, bool)>) {
    let proxy = match InhibitProxy::new().await {
        Ok(p) => p,
        Err(e) => {
            log::warn!("inhibitor: portal unavailable, screen may blank during playback: {e}");
            // Drain so senders never block; we just no-op.
            while rx.recv().await.is_ok() {}
            return;
        }
    };

    let mut active: HashSet<Source> = HashSet::new();
    let mut current: Option<Request<()>> = None;

    while let Ok((source, playing)) = rx.recv().await {
        if playing {
            active.insert(source);
        } else {
            active.remove(&source);
        }

        match (!active.is_empty(), current.is_some()) {
            (true, false) => {
                let options = InhibitOptions::default().set_reason(INHIBIT_REASON);
                match proxy
                    .inhibit(None, InhibitFlags::Idle.into(), options)
                    .await
                {
                    Ok(req) => current = Some(req),
                    Err(e) => log::warn!("inhibitor: failed to acquire inhibit: {e}"),
                }
            }
            (false, true) => {
                if let Some(req) = current.take()
                    && let Err(e) = req.close().await
                {
                    log::debug!("inhibitor: close failed: {e}");
                }
            }
            _ => {}
        }
    }

    if let Some(req) = current.take()
        && let Err(e) = req.close().await
    {
        log::debug!("inhibitor: close on shutdown failed: {e}");
    }
}
