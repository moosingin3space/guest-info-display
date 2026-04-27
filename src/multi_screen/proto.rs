//! Wire contract for the primary↔reflection RPC service.
//!
//! `DisplayProtocol` is the typed request enum consumed by `irpc`. The
//! `rpc_requests` macro generates the `DisplayMessage` enum that handler
//! code matches on (each variant carries a `WithChannels` wrapper providing
//! the typed reply channel).
//!
//! `iroh-irpc` owns framing and serialization (via postcard). This file only
//! declares the contract — there is no manual envelope or version field; the
//! ALPN in the parent module bumps when the protocol breaks compatibility.

use iroh::EndpointId;
use irpc::channel::{mpsc, oneshot};
use irpc::rpc_requests;
use serde::{Deserialize, Serialize};

use crate::spotify::SpotifyState;

#[rpc_requests(message = DisplayMessage, no_spans)]
#[derive(Debug, Serialize, Deserialize)]
pub enum DisplayProtocol {
    /// Initial handshake from a reflection asking to be added to the primary's
    /// trusted set. Resolves once the user approves or rejects on the primary.
    #[rpc(tx = oneshot::Sender<PairingResponse>)]
    RequestPairing(PairingRequest),

    /// Subscribe to playback state. The primary sends an immediate `Snapshot`
    /// followed by every subsequent state change until the channel closes.
    #[rpc(tx = mpsc::Sender<WireMessage>)]
    Subscribe(SubscribeRequest),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingRequest {
    /// Caller's iroh `EndpointId`. The primary trusts this on the LAN-only
    /// trust boundary established by mDNS discovery — see the spec note about
    /// `RequestPairing` being unauthenticated in v1.
    pub endpoint_id: EndpointId,
    /// Display name the reflection wants to register under (typically the
    /// hostname). Editable later by the primary's user.
    pub friendly_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PairingResponse {
    Accepted,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribeRequest {
    /// Caller's iroh `EndpointId`. Checked against the primary's set of paired
    /// inbound peers; calls from unknown ids are rejected with no payload.
    pub endpoint_id: EndpointId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WireMessage {
    /// Sent first on every new subscription so reflections start with a
    /// complete view without waiting for the next state change.
    Snapshot(SpotifyState),
    /// Full-state replace after the initial snapshot. No deltas — reflections
    /// just overwrite their local copy.
    StateChanged(SpotifyState),
    /// Original encoded bytes (JPEG/PNG from Spotify's CDN). The reflection
    /// decodes via `image::load_from_memory` so the wire stays small.
    CoverArt { url: String, encoded: Vec<u8> },
    /// Session ended on the primary — drop any cached artwork.
    CoversCleared,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spotify::SpotifyTrackInfo;

    fn sample_state() -> SpotifyState {
        let track = |uri: &str, name: &str| SpotifyTrackInfo {
            uri: uri.into(),
            name: name.into(),
            artists: "Some Artist".into(),
            cover_url: Some(format!("https://example.test/{uri}.jpg")),
        };
        SpotifyState {
            current: Some(track("spotify:track:cur", "Now Playing")),
            history: (0..5).map(|i| track(&format!("h{i}"), "old")).collect(),
            queue: (0..50).map(|i| track(&format!("q{i}"), "next")).collect(),
            is_playing: true,
        }
    }

    #[test]
    fn wire_message_postcard_roundtrip() {
        let cases = [
            WireMessage::Snapshot(sample_state()),
            WireMessage::StateChanged(SpotifyState::default()),
            WireMessage::CoverArt {
                url: "https://example.test/cover.jpg".into(),
                encoded: vec![0xff, 0xd8, 0xff, 0xe0],
            },
            WireMessage::CoversCleared,
        ];

        for original in cases {
            let bytes = postcard::to_allocvec(&original).expect("encode");
            let decoded: WireMessage = postcard::from_bytes(&bytes).expect("decode");

            // No PartialEq on SpotifyState/SpotifyTrackInfo (and we don't want
            // to add it just for tests), so compare by re-encoding the decoded
            // value — postcard is canonical for our types.
            let reencoded = postcard::to_allocvec(&decoded).expect("re-encode");
            assert_eq!(bytes, reencoded);
        }
    }

    #[test]
    fn realistic_snapshot_under_one_megabyte() {
        let snap = WireMessage::Snapshot(sample_state());
        let bytes = postcard::to_allocvec(&snap).expect("encode");
        assert!(
            bytes.len() < 1_000_000,
            "snapshot was {} bytes, expected well under 1 MB",
            bytes.len()
        );
    }
}
