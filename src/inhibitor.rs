//! XDG idle-inhibit portal integration. Holds an inhibit lock while playback is
//! active so the screen does not blank.
//!
//! The portal proxy is async and the request handle has no synchronous `close`,
//! so we run a dedicated single-threaded tokio runtime in a background thread
//! and drive it via an async channel. Dropping the [`Inhibitor`] closes the
//! sender, which unblocks the loop and lets it close any held inhibit before
//! exiting.

use std::thread::{self, JoinHandle};

use ashpd::desktop::Request;
use ashpd::desktop::inhibit::{InhibitFlags, InhibitOptions, InhibitProxy};
use async_channel::{Receiver, Sender, bounded};

const INHIBIT_REASON: &str = "Spotify playback active";

pub struct Inhibitor {
    cmd: Sender<bool>,
    _thread: JoinHandle<()>,
}

impl Inhibitor {
    /// Spawn the background thread that owns the portal connection.
    pub fn new() -> Self {
        let (cmd, rx) = bounded::<bool>(8);
        let _thread = thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("inhibitor tokio runtime");
            rt.block_on(run(rx));
        });
        Self { cmd, _thread }
    }

    /// Request that the inhibit be held (`true`) or released (`false`).
    /// Idempotent — repeated calls with the same value are no-ops on the wire.
    pub fn set(&self, want_inhibit: bool) {
        // try_send is fine: the channel is bounded and the worker keeps up
        // easily. A backpressure-induced drop would just delay one transition
        // by one event tick, which doesn't matter here.
        let _ = self.cmd.try_send(want_inhibit);
    }
}

async fn run(rx: Receiver<bool>) {
    let proxy = match InhibitProxy::new().await {
        Ok(p) => p,
        Err(e) => {
            log::warn!("inhibitor: portal unavailable, screen may blank during playback: {e}");
            // Drain so senders never block; we just no-op.
            while rx.recv().await.is_ok() {}
            return;
        }
    };

    let mut current: Option<Request<()>> = None;

    while let Ok(want) = rx.recv().await {
        match (want, current.is_some()) {
            (true, false) => {
                let options = InhibitOptions::default().set_reason(INHIBIT_REASON);
                match proxy.inhibit(None, InhibitFlags::Idle.into(), options).await {
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
