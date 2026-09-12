//! `DisplayService` handlers.
//!
//! Each variant of the generated `DisplayMessage` enum carries a typed reply
//! channel. We spawn a task per message so a long-lived subscription doesn't
//! starve other RPCs.

use std::sync::Arc;

use async_channel::Sender as AsyncSender;
use tokio::sync::mpsc::Receiver;
use tokio::sync::oneshot;

use super::{
    PendingApproval, Shared, Subscriber,
    proto::{DisplayMessage, PairingResponse, WireMessage},
};

pub async fn run(
    mut rx: Receiver<DisplayMessage>,
    shared: Arc<Shared>,
    approvals_tx: AsyncSender<PendingApproval>,
) {
    while let Some(msg) = rx.recv().await {
        match msg {
            DisplayMessage::RequestPairing(req) => {
                tokio::spawn(handle_pairing(req, approvals_tx.clone()));
            }
            DisplayMessage::Subscribe(req) => {
                tokio::spawn(handle_subscribe(req, shared.clone()));
            }
        }
    }
}

async fn handle_pairing(
    req: irpc::WithChannels<super::proto::PairingRequest, super::proto::DisplayProtocol>,
    approvals_tx: AsyncSender<PendingApproval>,
) {
    let endpoint_id = req.inner.endpoint_id;
    let friendly_name = req.inner.friendly_name.clone();
    let (respond_tx, respond_rx) = oneshot::channel();

    let pending = PendingApproval {
        endpoint_id,
        friendly_name: friendly_name.clone(),
        respond: respond_tx,
    };

    if approvals_tx.send(pending).await.is_err() {
        log::warn!(
            "multi_screen: pairing request from {friendly_name} dropped — approvals channel closed"
        );
        let _ = req.tx.send(PairingResponse::Rejected).await;
        return;
    }

    let response = respond_rx.await.unwrap_or_else(|_| {
        log::warn!("multi_screen: approver dropped without responding to {friendly_name}");
        PairingResponse::Rejected
    });

    if let Err(e) = req.tx.send(response).await {
        log::debug!("multi_screen: pairing reply send failed: {e}");
    }
}

async fn handle_subscribe(
    req: irpc::WithChannels<super::proto::SubscribeRequest, super::proto::DisplayProtocol>,
    shared: Arc<Shared>,
) {
    let trusted = match shared.inbound_trusted.lock() {
        Ok(guard) => guard.contains(&req.inner.endpoint_id),
        Err(e) => {
            log::warn!("multi_screen: inbound-trusted lock poisoned: {e}");
            return;
        }
    };
    if !trusted {
        log::warn!(
            "multi_screen: rejecting Subscribe from unpaired peer {}",
            req.inner.endpoint_id.fmt_short(),
        );
        return; // tx drops, client sees channel close
    }

    // Layout before state, so a reflection never flashes the built-in layout
    // and then switches to the real one in front of guests.
    let layout = match shared.last_layout.lock() {
        Ok(guard) => guard.clone(),
        Err(e) => {
            log::warn!("multi_screen: layout lock poisoned: {e}");
            return;
        }
    };

    let layout_assets: Vec<(String, Vec<u8>)> = match shared.layout_assets.lock() {
        Ok(guard) => guard.clone(),
        Err(e) => {
            log::warn!("multi_screen: layout-asset lock poisoned: {e}");
            return;
        }
    };

    let snapshot = match shared.last_state.lock() {
        Ok(guard) => guard.clone(),
        Err(e) => {
            log::warn!("multi_screen: snapshot lock poisoned: {e}");
            return;
        }
    };

    let covers: Vec<(String, Vec<u8>)> = match shared.cover_cache.lock() {
        Ok(guard) => guard.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        Err(e) => {
            log::warn!("multi_screen: cover-cache lock poisoned: {e}");
            return;
        }
    };

    let endpoint_id = req.inner.endpoint_id;
    let tx = req.tx;

    if let Some(source) = layout {
        if let Err(e) = tx.send(WireMessage::Layout { source }).await {
            log::debug!("multi_screen: subscriber dropped during layout: {e}");
            return;
        }
        // Images belong to the document that named them, so they follow it
        // immediately — before the snapshot, so the screen is complete at the
        // first frame a guest sees rather than filling in afterwards.
        for (path, encoded) in layout_assets {
            if let Err(e) = tx.send(WireMessage::Asset { path, encoded }).await {
                log::debug!("multi_screen: subscriber dropped during asset replay: {e}");
                return;
            }
        }
    }

    if let Err(e) = tx.send(WireMessage::Snapshot(snapshot)).await {
        log::debug!("multi_screen: subscriber dropped during snapshot: {e}");
        return;
    }
    for (url, encoded) in covers {
        if let Err(e) = tx.send(WireMessage::CoverArt { url, encoded }).await {
            log::debug!("multi_screen: subscriber dropped during cover replay: {e}");
            return;
        }
    }

    if let Ok(mut guard) = shared.subscribers.lock() {
        guard.push(Subscriber {
            endpoint_id,
            sender: tx,
        });
    }
}
