//! Reflection-side ingest. Dials a paired primary, subscribes to the wire
//! state stream, and translates each incoming `WireMessage` into the same
//! `spotify::Event` flow that a primary's local librespot path produces.
//!
//! The GPUI consumer (in `main::spawn_spotify_task`) is unchanged: same
//! `SharedSpotifyState`, same async-channel of events, same render output.

use std::time::Duration;

use async_channel::Sender as AsyncSender;
use iroh::{Endpoint, EndpointAddr, EndpointId};
use tokio::sync::oneshot;

use super::videos::Cache;
use super::{
    ALPN, LayoutUpdate,
    proto::{
        DisplayProtocol, PairingRequest, PairingResponse, SubscribeRequest, TimeRequest,
        WireMessage,
    },
};
use crate::spotify::{CoverImage, Event, SharedSpotifyState, SpotifyState};
use crate::video::clock;

/// Drive a reflection client until shutdown. Reconnects on session end with an
/// exponential backoff (capped at 30s, reset on each successful snapshot) per
/// the spec's failure-mode notes.
pub async fn run(
    endpoint: Endpoint,
    primary: EndpointId,
    state: SharedSpotifyState,
    events_tx: AsyncSender<Event>,
    layouts_tx: AsyncSender<LayoutUpdate>,
    mut shutdown: oneshot::Receiver<()>,
) {
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);

    loop {
        let session = run_session(&endpoint, primary, &state, &events_tx, &layouts_tx);
        tokio::select! {
            biased;
            _ = &mut shutdown => return,
            r = session => {
                match r {
                    Ok(SessionEnd::ChannelClosed) => {
                        log::info!("reflection: primary closed the subscription stream");
                    }
                    Ok(SessionEnd::ConsumerGone) => {
                        log::info!("reflection: GPUI consumer gone, exiting");
                        return;
                    }
                    Err(e) => {
                        log::warn!("reflection: session ended: {e}");
                    }
                }
            }
        }

        // Clear local state so the GPUI side renders the disconnected card.
        if let Ok(mut guard) = state.lock() {
            *guard = SpotifyState::default();
        }
        let _ = events_tx.send(Event::StateChanged).await;
        let _ = events_tx.send(Event::CoversCleared).await;
        let _ = events_tx.send(Event::ConnectionLost).await;

        tokio::select! {
            biased;
            _ = &mut shutdown => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(max_backoff);
    }
}

/// One-shot pairing handshake. Opens an irpc client to `primary`, sends our
/// `EndpointId` + `friendly_name`, and returns the user's decision. The peer
/// closing the connection mid-flight surfaces as an `Err`.
pub async fn request_pairing(
    endpoint: &Endpoint,
    primary: EndpointId,
    friendly_name: String,
) -> Result<PairingResponse, Box<dyn std::error::Error + Send + Sync>> {
    let client =
        irpc_iroh::client::<DisplayProtocol>(endpoint.clone(), EndpointAddr::new(primary), ALPN);
    let resp = client
        .rpc(PairingRequest {
            endpoint_id: endpoint.id(),
            friendly_name,
        })
        .await?;
    Ok(resp)
}

enum SessionEnd {
    /// Primary closed the subscription stream — we should reconnect.
    ChannelClosed,
    /// Local GPUI consumer dropped the receiver — reflection should exit.
    ConsumerGone,
}

async fn run_session(
    endpoint: &Endpoint,
    primary: EndpointId,
    state: &SharedSpotifyState,
    events_tx: &AsyncSender<Event>,
    layouts_tx: &AsyncSender<LayoutUpdate>,
) -> Result<SessionEnd, Box<dyn std::error::Error + Send + Sync>> {
    let client =
        irpc_iroh::client::<DisplayProtocol>(endpoint.clone(), EndpointAddr::new(primary), ALPN);
    let mut rx = client
        .server_streaming(
            SubscribeRequest {
                endpoint_id: endpoint.id(),
            },
            16,
        )
        .await?;

    log::info!("reflection: subscribed to primary {}", primary.fmt_short());

    // Fetching the current layout's videos. Replaced by each new layout, and
    // aborted with the session; partial downloads resume next time.
    let mut video_sync: Option<AbortOnDrop> = None;
    // Measuring the primary's clock for as long as the session lasts.
    let _time_sync = AbortOnDrop(tokio::spawn(sync_clock(client.clone(), endpoint.id())));
    let mut connected = false;
    while let Some(msg) = rx.recv().await? {
        match msg {
            WireMessage::Snapshot(s) | WireMessage::StateChanged(s) => {
                if let Ok(mut guard) = state.lock() {
                    *guard = s;
                }
                if events_tx.send(Event::StateChanged).await.is_err() {
                    return Ok(SessionEnd::ConsumerGone);
                }
                if !connected {
                    connected = true;
                    if events_tx.send(Event::ConnectionRestored).await.is_err() {
                        return Ok(SessionEnd::ConsumerGone);
                    }
                }
            }
            WireMessage::CoverArt { url, encoded } => {
                let cover = CoverImage { url, encoded };
                if events_tx.send(Event::CoverLoaded(cover)).await.is_err() {
                    return Ok(SessionEnd::ConsumerGone);
                }
            }
            WireMessage::CoversCleared => {
                if events_tx.send(Event::CoversCleared).await.is_err() {
                    return Ok(SessionEnd::ConsumerGone);
                }
            }
            WireMessage::Layout { source, videos } => {
                if layouts_tx
                    .send(LayoutUpdate::Document(source))
                    .await
                    .is_err()
                {
                    return Ok(SessionEnd::ConsumerGone);
                }
                let (endpoint, layouts_tx) = (endpoint.clone(), layouts_tx.clone());
                video_sync = Some(AbortOnDrop(tokio::spawn(async move {
                    Cache::new()
                        .sync(&endpoint, primary, &videos, &layouts_tx)
                        .await;
                })));
            }
            WireMessage::VideoCue(cue) => crate::video::sync::receive(cue),
            WireMessage::Asset { path, encoded } => {
                if layouts_tx
                    .send(LayoutUpdate::Asset { path, encoded })
                    .await
                    .is_err()
                {
                    return Ok(SessionEnd::ConsumerGone);
                }
            }
        }
    }

    drop(video_sync);
    Ok(SessionEnd::ChannelClosed)
}

/// Measure the primary's video clock for as long as the session lasts: a
/// quick burst to lock on soon after connecting, then one round trip a second
/// to follow drift. Each trip is timed on our own clock; see
/// [`crate::video::clock`] for what is done with them.
async fn sync_clock(client: irpc::Client<DisplayProtocol>, endpoint_id: EndpointId) {
    const BURST: usize = 2 * clock::MIN_SAMPLES;
    let mut sent = 0usize;
    loop {
        let t0 = clock::local_now();
        match client.rpc(TimeRequest { endpoint_id }).await {
            Ok(reply) => {
                let t2 = clock::local_now();
                if let Some(remote) = reply.clock_ns {
                    clock::add_sample(t0, remote, t2);
                }
            }
            Err(e) => log::debug!("reflection: clock sync round trip failed: {e}"),
        }
        sent += 1;
        let pause = if sent < BURST { 50 } else { 1000 };
        tokio::time::sleep(Duration::from_millis(pause)).await;
    }
}

/// Aborts its task when dropped, so a session that ends stops its downloads.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
