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

use super::videos::VideoRef;
use crate::spotify::SpotifyState;
use crate::video::sync::Cue;

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

    /// One clock-sync round trip: the primary answers with its video clock's
    /// current time, and the reflection times the trip on its own clock.
    #[rpc(tx = oneshot::Sender<TimeResponse>)]
    Time(TimeRequest),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeRequest {
    /// Checked against the paired set, like `Subscribe`.
    pub endpoint_id: EndpointId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeResponse {
    /// The primary's video clock, in nanoseconds. `None` to an unpaired peer.
    pub clock_ns: Option<u64>,
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
    /// The primary's active layout, as validated KDL source.
    ///
    /// Source text rather than a serialised tree: both ends run the same
    /// binary and therefore the same parser, so shipping text means one parse
    /// implementation, one set of diagnostics, and a wire that does not change
    /// every time a widget is added. The primary only ever sends source it has
    /// already parsed, so a reflection failing to parse it means version skew.
    ///
    /// `videos` names the content behind each `video` path. The files
    /// themselves are too big for this stream; the reflection fetches them
    /// over iroh-blobs by hash.
    Layout {
        source: String,
        videos: Vec<VideoRef>,
    },

    /// Encoded bytes behind one `image` node of the current layout.
    ///
    /// Keyed by the layout-relative path the node names, not by a content
    /// hash: the renderer already looks assets up by path, so hashing would
    /// only buy an indirection and the dedup of two nodes naming the same
    /// file, which the path key gives for free.
    ///
    /// Sent immediately after the `Layout` it belongs to, and replayed on
    /// every subscribe — reflections do not persist assets, so a cold boot
    /// shows placeholders until these arrive.
    Asset { path: String, encoded: Vec<u8> },

    /// Which clip a `video` node is playing and when it started, on the
    /// primary's clock; see `crate::video::sync`. The newest per node is
    /// replayed on subscribe, after the layout it belongs to.
    VideoCue(Cue),
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
            WireMessage::Layout {
                source: "root { video \"ghosts.mp4\" }".into(),
                videos: vec![VideoRef {
                    path: "ghosts.mp4".into(),
                    hash: iroh_blobs::Hash::new(b"not really a video"),
                    size: 18,
                }],
            },
            WireMessage::Asset {
                path: "ana.jpg".into(),
                encoded: vec![0x89, 0x50, 0x4e, 0x47],
            },
            WireMessage::VideoCue(Cue {
                node: 0,
                current: "ghosts.mp4".into(),
                pos0: 123_456_789_000,
                next: Some("bats.mp4".into()),
            }),
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
