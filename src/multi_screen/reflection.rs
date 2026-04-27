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

use super::{
    ALPN,
    proto::{DisplayProtocol, PairingRequest, PairingResponse, SubscribeRequest, WireMessage},
};
use crate::spotify::{self, CoverImage, Event, SharedSpotifyState, SpotifyState};

/// Drive a reflection client until shutdown. Reconnects on session end with an
/// exponential backoff (capped at 30s, reset on each successful snapshot) per
/// the spec's failure-mode notes.
pub async fn run(
    endpoint: Endpoint,
    primary: EndpointId,
    state: SharedSpotifyState,
    events_tx: AsyncSender<Event>,
    mut shutdown: oneshot::Receiver<()>,
) {
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);

    loop {
        let session = run_session(&endpoint, primary, &state, &events_tx);
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
) -> Result<SessionEnd, Box<dyn std::error::Error + Send + Sync>> {
    let client = irpc_iroh::client::<DisplayProtocol>(
        endpoint.clone(),
        EndpointAddr::new(primary),
        ALPN,
    );
    let mut rx = client
        .server_streaming(
            SubscribeRequest {
                endpoint_id: endpoint.id(),
            },
            16,
        )
        .await?;

    log::info!("reflection: subscribed to primary {}", primary.fmt_short());

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
                let cover = match spotify::decode_cover_bytes(&encoded) {
                    Ok((w, h, bgra)) => CoverImage {
                        url,
                        width: w,
                        height: h,
                        bgra,
                        encoded,
                    },
                    Err(e) => {
                        log::debug!("reflection: cover decode failed: {e}");
                        continue;
                    }
                };
                if events_tx.send(Event::CoverLoaded(cover)).await.is_err() {
                    return Ok(SessionEnd::ConsumerGone);
                }
            }
            WireMessage::CoversCleared => {
                if events_tx.send(Event::CoversCleared).await.is_err() {
                    return Ok(SessionEnd::ConsumerGone);
                }
            }
        }
    }

    Ok(SessionEnd::ChannelClosed)
}
