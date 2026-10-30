//! Multi-screen networking. The primary serves a `DisplayService` over an iroh
//! QUIC endpoint; reflections subscribe and mirror the state.
//!
//! ## Flow on the primary
//!
//! - [`start`] brings up the iroh endpoint, registers the irpc service on a
//!   [`Router`], and spawns two tokio tasks: a service handler that processes
//!   incoming RPC messages, and a broadcast loop that fans `WireMessage`s out
//!   to all live subscribers.
//! - The GPUI side calls [`MultiScreenHandle::broadcast_state`],
//!   [`broadcast_cover`], and [`broadcast_covers_cleared`] each time the local
//!   spotify state changes. Those are synchronous — they push onto a
//!   cross-runtime channel that the broadcast loop drains.
//! - New subscribers get an immediate `Snapshot` plus every cached
//!   `CoverArt` before being added to the live registry, so they catch up
//!   without waiting for the next state change on the primary.

mod discovery;
mod endpoint;
pub mod proto;
mod reflection;
mod service;
mod videos;

pub use discovery::DiscoveredNode;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_channel::{Receiver as AsyncReceiver, Sender as AsyncSender};
use iroh::{Endpoint, EndpointId, SecretKey, protocol::Router};
use irpc::LocalSender;
use irpc_iroh::IrohProtocol;
use tokio::sync::oneshot;

use proto::{DisplayProtocol, WireMessage};
use videos::VideoRef;

pub use proto::PairingResponse;

use crate::persistence::WifiCredentials;
use crate::spotify::{self, SharedSpotifyState, SpotifyState};

/// ALPN identifying our irpc service. Bumped when the protocol breaks
/// compatibility — keep this in sync with the spec's "service version" notion.
///
/// `/2` added `WireMessage::Layout`; `/3` added `WireMessage::Asset`; `/4` gave
/// `Layout` its video hashes; `/5` added video cues and the `Time` request for
/// synchronised playback; `/6` added `WireMessage::Wifi`. Postcard rejects an
/// unknown discriminant, so an un-upgraded reflection cannot decode either;
/// bumping the ALPN means it fails to connect at all and shows "Primary
/// unavailable", which is a diagnosable failure rather than a mysterious one.
/// Both ends must be updated together.
pub const ALPN: &[u8] = b"xyz.mooshq.guest-info-display.display/6";

/// Cross-runtime broadcast tunnel: GPUI thread pushes events synchronously,
/// the multi-screen runtime drains them and fans them out to subscribers.
enum BroadcastEvent {
    StateChanged(SpotifyState),
    CoverArt {
        url: String,
        encoded: Vec<u8>,
    },
    CoversCleared,
    /// A document and every file it names, together. One event rather than
    /// two so the cache can never hold a layout whose assets belong to the
    /// document before it. Videos are local files still to be hashed.
    Layout {
        source: String,
        assets: Vec<(String, Vec<u8>)>,
        videos: Vec<(String, PathBuf)>,
    },
    /// A `Layout` whose videos have been hashed, ready to send. Raised by the
    /// hashing task, never by the UI: hashing gigabytes must not hold up the
    /// playback state queued behind it.
    LayoutReady {
        source: String,
        assets: Vec<(String, Vec<u8>)>,
        videos: Vec<VideoRef>,
    },
    /// A primary's player started a clip, or picked the one after it.
    Cue(crate::video::sync::Cue),
    Wifi(Option<WifiCredentials>),
}

/// Approval request surfaced to the GPUI side when an unknown peer calls
/// `RequestPairing`. The handler stalls on `respond` until the user decides.
pub struct PendingApproval {
    pub endpoint_id: EndpointId,
    pub friendly_name: String,
    pub respond: oneshot::Sender<PairingResponse>,
}

/// Cross-thread command sent by the GPUI side into the multi-screen runtime
/// where the iroh `Endpoint` lives.
enum Command {
    StartReflection {
        primary: EndpointId,
        state: SharedSpotifyState,
        events_tx: AsyncSender<spotify::Event>,
        layouts_tx: AsyncSender<LayoutUpdate>,
        shutdown_rx: oneshot::Receiver<()>,
    },
    RequestPairing {
        primary: EndpointId,
        friendly_name: String,
        respond: AsyncSender<Option<PairingResponse>>,
    },
    /// Drop any live subscriber stream pointing at this endpoint. Used when
    /// the primary's user removes a reflection — the in-flight subscription
    /// closes immediately rather than waiting for the next state change.
    DisconnectSubscriber(EndpointId),
}

/// What a reflection receives about its layout.
///
/// Assets ride the same channel as the document because they are the same
/// concern — what the screen looks like — and because the ordering matters:
/// the document arrives first and tells the reflection which images to expect.
#[derive(Debug)]
pub enum LayoutUpdate {
    /// Validated KDL source. Replaces whatever is rendering.
    Document(String),
    /// Bytes for one `image` path in the document that came before it.
    Asset { path: String, encoded: Vec<u8> },
    /// A `video` path of the current document, now on disk and playable.
    Video { path: String, file: PathBuf },
    /// How far along the download behind a `video` path is.
    VideoProgress {
        path: String,
        received: u64,
        total: u64,
    },
    /// The primary's Wi-Fi network, for the QR code.
    Wifi(Option<WifiCredentials>),
    /// What a `video` node is playing. On this channel rather than handed to
    /// `video::sync` straight off the wire, so it lands after the document it
    /// names: applying a new document clears the cues, and one that arrived
    /// first would be wiped, leaving its node with no cue to follow.
    Cue(crate::video::sync::Cue),
}

/// Hands the GPUI side a primary-mirroring backend that produces the same
/// (state, events) pair as `spotify::start` would. Drop releases the dial loop.
pub struct ReflectionHandle {
    pub state: SharedSpotifyState,
    pub events: AsyncReceiver<spotify::Event>,
    /// Layout documents and their images, pushed by the primary. A channel of
    /// its own rather than another `spotify::Event` variant: this carries no
    /// playback state, and only a reflection ever has one.
    pub layouts: AsyncReceiver<LayoutUpdate>,
    _shutdown: Option<oneshot::Sender<()>>,
}

impl Drop for ReflectionHandle {
    fn drop(&mut self) {
        if let Some(tx) = self._shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// Set of EndpointIds the primary accepts `Subscribe` calls from. Shared
/// between the multi-screen runtime (read on every Subscribe) and the GPUI
/// side (writes on approval / removal).
type TrustState = Arc<Mutex<HashSet<EndpointId>>>;

/// One live subscription. Tracked together so the primary can target a single
/// reflection (e.g. on removal) without having to disambiguate by sender alone.
pub(crate) struct Subscriber {
    /// This subscription, as opposed to the reflection's next one.
    pub id: u64,
    pub endpoint_id: EndpointId,
    /// Feeds the task that writes to the reflection's stream (see
    /// `service::forward`); dropping it ends the subscription.
    pub sender: tokio::sync::mpsc::Sender<WireMessage>,
}

/// Subscriber-registry + snapshot cache shared between the service handler
/// (adds new subscribers, replies to Subscribe with snapshot+covers) and the
/// broadcast loop (updates the cache and fans live messages out).
struct Shared {
    subscribers: Mutex<Vec<Subscriber>>,
    last_state: Mutex<SpotifyState>,
    /// The primary's active layout source, replayed to every new subscriber
    /// before its first snapshot so a reflection never flashes the built-in
    /// layout before switching to the real one.
    last_layout: Mutex<Option<String>>,
    /// Encoded image bytes for the layout in `last_layout`, keyed by the path
    /// the `image` node names. Replaced wholesale with the layout so the two
    /// never disagree; not persisted anywhere, since a reflection re-fetches
    /// them on its next subscribe.
    layout_assets: Mutex<Vec<(String, Vec<u8>)>>,
    /// Hashes of the files behind the `video` nodes of `last_layout`.
    layout_videos: Mutex<Vec<VideoRef>>,
    /// The newest cue per `video` node of `last_layout`, so a reflection that
    /// subscribes mid-clip can lock straight on.
    video_cues: Mutex<HashMap<u32, crate::video::sync::Cue>>,
    /// Encoded JPEG/PNG bytes keyed by Spotify cover URL. Populated each time
    /// a primary fetches a cover; cleared on session reset.
    cover_cache: Mutex<HashMap<String, Vec<u8>>>,
    /// The primary's Wi-Fi network, replayed to every new subscriber.
    last_wifi: Mutex<Option<WifiCredentials>>,
    inbound_trusted: TrustState,
}

impl Shared {
    fn new(inbound_trusted: TrustState) -> Arc<Self> {
        Arc::new(Self {
            subscribers: Mutex::new(Vec::new()),
            last_state: Mutex::new(SpotifyState::default()),
            last_layout: Mutex::new(None),
            layout_assets: Mutex::new(Vec::new()),
            layout_videos: Mutex::new(Vec::new()),
            video_cues: Mutex::new(HashMap::new()),
            cover_cache: Mutex::new(HashMap::new()),
            last_wifi: Mutex::new(None),
            inbound_trusted,
        })
    }
}

/// Holding this keeps the multi-screen runtime alive. Drop sends a shutdown
/// signal that closes the iroh router and tears down the service tasks.
pub struct MultiScreenHandle {
    broadcast_tx: AsyncSender<BroadcastEvent>,
    approvals_rx: AsyncReceiver<PendingApproval>,
    discovered_rx: AsyncReceiver<DiscoveredNode>,
    cmd_tx: AsyncSender<Command>,
    inbound_trusted: TrustState,
    shutdown: Option<oneshot::Sender<()>>,
    _thread: std::thread::JoinHandle<()>,
}

impl MultiScreenHandle {
    /// Receiver for pending pairing approvals. Drained by the GPUI side (see
    /// the settings dialog). Cloning the receiver is fine — only one consumer
    /// will see each approval.
    pub fn approvals(&self) -> AsyncReceiver<PendingApproval> {
        self.approvals_rx.clone()
    }

    /// Receiver for discovered primaries on the LAN. Cloned receiver is fine —
    /// each event is delivered to a single consumer; collisions don't matter
    /// because the same `DiscoveredNode` will recur as mDNS re-emits.
    pub fn discovered(&self) -> AsyncReceiver<DiscoveredNode> {
        self.discovered_rx.clone()
    }

    /// Push a state change to every live subscriber. Cheap: just pushes onto
    /// a bounded async channel; the multi-screen runtime does the real work.
    pub fn broadcast_state(&self, state: SpotifyState) {
        let _ = self
            .broadcast_tx
            .try_send(BroadcastEvent::StateChanged(state));
    }

    pub fn broadcast_cover(&self, url: String, encoded: Vec<u8>) {
        let _ = self
            .broadcast_tx
            .try_send(BroadcastEvent::CoverArt { url, encoded });
    }

    pub fn broadcast_covers_cleared(&self) {
        let _ = self.broadcast_tx.try_send(BroadcastEvent::CoversCleared);
    }

    /// Push the primary's layout to every live reflection, and cache it for
    /// ones that subscribe later. Called whenever the active document changes:
    /// startup, a hot reload, an import, a reload button, a role switch.
    /// `videos` are the files on disk behind the document's `video` nodes.
    pub fn broadcast_layout(
        &self,
        source: String,
        assets: Vec<(String, Vec<u8>)>,
        videos: Vec<(String, PathBuf)>,
    ) {
        let _ = self.broadcast_tx.try_send(BroadcastEvent::Layout {
            source,
            assets,
            videos,
        });
    }

    /// Tell every reflection which network the QR code should offer.
    pub fn broadcast_wifi(&self, wifi: Option<WifiCredentials>) {
        let _ = self.broadcast_tx.try_send(BroadcastEvent::Wifi(wifi));
    }

    /// Tell every reflection what a `video` node is playing.
    pub fn broadcast_cue(&self, cue: crate::video::sync::Cue) {
        let _ = self.broadcast_tx.try_send(BroadcastEvent::Cue(cue));
    }

    /// Replace the set of endpoints that the primary accepts `Subscribe`
    /// calls from. Called at startup with the contents of
    /// `paired_peers(Direction::Inbound)`.
    pub fn seed_inbound_trusted(&self, ids: HashSet<EndpointId>) {
        if let Ok(mut guard) = self.inbound_trusted.lock() {
            *guard = ids;
            // Known from the database at once, long before the endpoint is up:
            // a primary holding its first clip needs it from its first frame.
            crate::video::sync::set_paired(guard.len());
        }
    }

    pub fn add_inbound_trusted(&self, id: EndpointId) {
        if let Ok(mut guard) = self.inbound_trusted.lock() {
            guard.insert(id);
            crate::video::sync::set_paired(guard.len());
        }
    }

    pub fn remove_inbound_trusted(&self, id: EndpointId) {
        if let Ok(mut guard) = self.inbound_trusted.lock() {
            guard.remove(&id);
            crate::video::sync::set_paired(guard.len());
        }
    }

    /// Drop any live subscriber stream pointing at `id`. Pair this with
    /// [`remove_inbound_trusted`] when the user removes a reflection so the
    /// stream closes immediately and the reflection can't reconnect.
    pub fn disconnect_subscriber(&self, id: EndpointId) {
        if self
            .cmd_tx
            .try_send(Command::DisconnectSubscriber(id))
            .is_err()
        {
            log::warn!("multi_screen: command channel full — disconnect dropped");
        }
    }

    /// Initiate pairing with a discovered primary. Returns an async receiver
    /// that produces the user's decision (Accepted/Rejected) once the primary
    /// responds, or `None` on transport error. Caller awaits in `cx.spawn`.
    pub fn request_pairing(
        &self,
        primary: EndpointId,
        friendly_name: String,
    ) -> AsyncReceiver<Option<PairingResponse>> {
        let (tx, rx) = async_channel::bounded::<Option<PairingResponse>>(1);
        let cmd = Command::RequestPairing {
            primary,
            friendly_name,
            respond: tx,
        };
        if self.cmd_tx.try_send(cmd).is_err() {
            log::warn!("multi_screen: command channel full — pairing request dropped");
        }
        rx
    }

    /// Start a reflection that subscribes to `primary` and feeds the resulting
    /// state into a fresh (`state`, `events`) pair. Returned `ReflectionHandle`
    /// shape matches `spotify::SpotifyHandle` so the GPUI consumer is unchanged.
    pub fn start_reflection(&self, primary: EndpointId) -> ReflectionHandle {
        let state: SharedSpotifyState = Arc::new(std::sync::Mutex::new(SpotifyState::default()));
        let (events_tx, events_rx) = async_channel::bounded::<spotify::Event>(16);
        // Roomier than the old document-only channel: one layout now arrives
        // as the document plus a message per image, and the sender blocks
        // rather than dropping, so a narrow channel just stalls the session.
        let (layouts_tx, layouts_rx) = async_channel::bounded::<LayoutUpdate>(16);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let cmd = Command::StartReflection {
            primary,
            state: state.clone(),
            events_tx,
            layouts_tx,
            shutdown_rx,
        };
        if self.cmd_tx.try_send(cmd).is_err() {
            log::warn!("multi_screen: command channel full — reflection start dropped");
        }

        ReflectionHandle {
            state,
            events: events_rx,
            layouts: layouts_rx,
            _shutdown: Some(shutdown_tx),
        }
    }
}

impl Drop for MultiScreenHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        // The thread joins on its own — we don't block GPUI on shutdown.
    }
}

/// Spawn the multi-screen runtime. Returns immediately; the iroh endpoint binds
/// asynchronously on the worker thread. Identity is symmetric across roles, so
/// this is called regardless of whether we are primary or reflection.
pub fn start(secret_key: SecretKey) -> MultiScreenHandle {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (broadcast_tx, broadcast_rx) = async_channel::bounded::<BroadcastEvent>(64);
    let (approvals_tx, approvals_rx) = async_channel::bounded::<PendingApproval>(8);
    let (discovered_tx, discovered_rx) = async_channel::bounded::<DiscoveredNode>(32);
    let (cmd_tx, cmd_rx) = async_channel::bounded::<Command>(8);
    let inbound_trusted: TrustState = Arc::new(Mutex::new(HashSet::new()));
    let inbound_trusted_run = inbound_trusted.clone();
    let broadcast_tx_run = broadcast_tx.clone();

    let thread = std::thread::Builder::new()
        .name("multi-screen".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("multi-screen tokio runtime");
            rt.block_on(
                MultiScreenRuntime {
                    secret_key,
                    shutdown: shutdown_rx,
                    broadcast_rx,
                    broadcast_tx: broadcast_tx_run,
                    approvals_tx,
                    discovered_tx,
                    cmd_rx,
                    inbound_trusted: inbound_trusted_run,
                }
                .run(),
            );
        })
        .expect("multi-screen worker thread");

    MultiScreenHandle {
        broadcast_tx,
        approvals_rx,
        discovered_rx,
        cmd_tx,
        inbound_trusted,
        shutdown: Some(shutdown_tx),
        _thread: thread,
    }
}

struct MultiScreenRuntime {
    secret_key: SecretKey,
    shutdown: oneshot::Receiver<()>,
    broadcast_rx: AsyncReceiver<BroadcastEvent>,
    /// For the hashing task to hand finished layouts back to the loop.
    broadcast_tx: AsyncSender<BroadcastEvent>,
    approvals_tx: AsyncSender<PendingApproval>,
    discovered_tx: AsyncSender<DiscoveredNode>,
    cmd_rx: AsyncReceiver<Command>,
    inbound_trusted: TrustState,
}

impl MultiScreenRuntime {
    async fn run(self) {
        let MultiScreenRuntime {
            secret_key,
            shutdown,
            broadcast_rx,
            broadcast_tx,
            approvals_tx,
            discovered_tx,
            cmd_rx,
            inbound_trusted,
        } = self;

        let Some((endpoint, mdns)) = endpoint::build(secret_key).await else {
            let _ = shutdown.await;
            return;
        };

        log::info!("multi_screen: endpoint up, EndpointId = {}", endpoint.id());

        let shared = Shared::new(inbound_trusted);
        let library = videos::Library::open().await;
        let router = build_router(
            endpoint.clone(),
            shared.clone(),
            approvals_tx,
            library.as_ref().map(videos::Library::store),
        )
        .await;
        tokio::spawn(publish_followers(shared.clone()));
        let (hash_tx, hash_rx) = async_channel::unbounded();
        tokio::spawn(hash_loop(hash_rx, library, broadcast_tx));
        tokio::spawn(broadcast_loop(broadcast_rx, hash_tx, shared.clone()));
        tokio::spawn(command_loop(cmd_rx, endpoint, shared));
        if let Some(mdns) = mdns {
            tokio::spawn(discovery::run(mdns, discovered_tx));
        } else {
            // Drop the sender so receivers close cleanly when no mDNS is available.
            drop(discovered_tx);
        }

        let _ = shutdown.await;
        log::info!("multi_screen: shutting down");
        if let Err(e) = router.shutdown().await {
            log::debug!("multi_screen: router shutdown error: {e}");
        }
    }
}

async fn command_loop(rx: AsyncReceiver<Command>, endpoint: Endpoint, shared: Arc<Shared>) {
    while let Ok(cmd) = rx.recv().await {
        match cmd {
            Command::StartReflection {
                primary,
                state,
                events_tx,
                layouts_tx,
                shutdown_rx,
            } => {
                tokio::spawn(reflection::run(
                    endpoint.clone(),
                    primary,
                    state,
                    events_tx,
                    layouts_tx,
                    shutdown_rx,
                ));
            }
            Command::RequestPairing {
                primary,
                friendly_name,
                respond,
            } => {
                let endpoint = endpoint.clone();
                tokio::spawn(async move {
                    let result = reflection::request_pairing(&endpoint, primary, friendly_name)
                        .await
                        .map_err(|e| {
                            log::warn!("multi_screen: pairing request failed: {e}");
                        })
                        .ok();
                    let _ = respond.send(result).await;
                });
            }
            Command::DisconnectSubscriber(endpoint_id) => {
                if let Ok(mut guard) = shared.subscribers.lock() {
                    let before = guard.len();
                    guard.retain(|s| s.endpoint_id != endpoint_id);
                    let dropped = before - guard.len();
                    if dropped > 0 {
                        log::info!(
                            "multi_screen: dropped {dropped} subscriber(s) for {}",
                            endpoint_id.fmt_short()
                        );
                    }
                }
            }
        }
    }
}

/// Wires the service onto the router. The handler loop runs on its own task
/// tied to the router's lifetime via the mpsc channel close.
///
/// The blobs protocol rides the same endpoint, serving the primary's video
/// files to paired reflections. Every device registers it — identity is
/// symmetric across roles — but a reflection's store simply holds nothing.
async fn build_router(
    endpoint: Endpoint,
    shared: Arc<Shared>,
    approvals_tx: AsyncSender<PendingApproval>,
    blobs: Option<&iroh_blobs::store::fs::FsStore>,
) -> Router {
    let (msg_tx, msg_rx) = tokio::sync::mpsc::channel(16);
    let local_sender: LocalSender<DisplayProtocol> = msg_tx.into();
    let proto = IrohProtocol::with_sender(local_sender);

    let trusted = shared.inbound_trusted.clone();
    tokio::spawn(service::run(msg_rx, shared, approvals_tx));

    let router = Router::builder(endpoint).accept(ALPN, proto);
    match blobs {
        Some(store) => router
            .accept(
                iroh_blobs::ALPN,
                iroh_blobs::BlobsProtocol::new(store, Some(videos::serve_to_trusted(trusted))),
            )
            .spawn(),
        None => router.spawn(),
    }
}

/// Keep the video player's list of subscribed reflections current, so a
/// primary holding a clip for them waits only on those still connected.
async fn publish_followers(shared: Arc<Shared>) {
    let mut last = Vec::new();
    loop {
        let mut followers: Vec<String> = match shared.subscribers.lock() {
            Ok(guard) => guard.iter().map(|s| s.endpoint_id.to_string()).collect(),
            Err(_) => Vec::new(),
        };
        followers.sort();
        followers.dedup();
        if followers != last {
            crate::video::sync::set_followers(followers.clone());
            last = followers;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

/// Hash each layout's videos, in arrival order, and hand the finished layout
/// back to the broadcast loop. One at a time on purpose: the last layout in
/// is the last one out, so a slow hash can never overwrite a newer document.
async fn hash_loop(
    rx: AsyncReceiver<BroadcastEvent>,
    mut library: Option<videos::Library>,
    broadcast_tx: AsyncSender<BroadcastEvent>,
) {
    while let Ok(event) = rx.recv().await {
        let BroadcastEvent::Layout {
            source,
            assets,
            videos,
        } = event
        else {
            continue;
        };
        let videos = match library.as_mut() {
            Some(library) => library.hash_all(&videos).await,
            None => Vec::new(),
        };
        let ready = BroadcastEvent::LayoutReady {
            source,
            assets,
            videos,
        };
        if broadcast_tx.send(ready).await.is_err() {
            return;
        }
    }
}

/// Drain `broadcast_rx`, update the snapshot/cover cache, and fan each event
/// out to every live subscriber. Subscribers whose channels are closed or full
/// are dropped — slow reflections have to reconnect for a fresh snapshot
/// rather than wedge the broadcast.
async fn broadcast_loop(
    rx: AsyncReceiver<BroadcastEvent>,
    hash_tx: AsyncSender<BroadcastEvent>,
    shared: Arc<Shared>,
) {
    // The document the UI last announced, to tell a new one from a re-publish.
    let mut announced: Option<String> = None;
    while let Ok(event) = rx.recv().await {
        // A layout fans out into the document plus one message per image, so
        // every event produces a batch. Each subscriber takes the whole batch
        // or is dropped part-way through and reconnects for a fresh replay.
        let wires: Vec<WireMessage> = match event {
            BroadcastEvent::StateChanged(state) => {
                if let Ok(mut guard) = shared.last_state.lock() {
                    *guard = state.clone();
                }
                vec![WireMessage::StateChanged(state)]
            }
            BroadcastEvent::CoverArt { url, encoded } => {
                if let Ok(mut guard) = shared.cover_cache.lock() {
                    guard.insert(url.clone(), encoded.clone());
                }
                vec![WireMessage::CoverArt { url, encoded }]
            }
            BroadcastEvent::Cue(cue) => {
                if let Ok(mut guard) = shared.video_cues.lock() {
                    guard.insert(cue.node, cue.clone());
                }
                vec![WireMessage::VideoCue(cue)]
            }
            BroadcastEvent::Wifi(wifi) => {
                if let Ok(mut guard) = shared.last_wifi.lock() {
                    *guard = wifi.clone();
                }
                vec![WireMessage::Wifi(wifi)]
            }
            BroadcastEvent::CoversCleared => {
                if let Ok(mut guard) = shared.cover_cache.lock() {
                    guard.clear();
                }
                vec![WireMessage::CoversCleared]
            }
            layout @ BroadcastEvent::Layout { .. } => {
                // Cues name a document's nodes, so a different document voids
                // them — cleared here, as the UI announces it, because its
                // players start (and cue) before its videos finish hashing.
                // The channel keeps order, so this lands ahead of those cues.
                if let BroadcastEvent::Layout { source, .. } = &layout
                    && announced.as_deref() != Some(source.as_str())
                {
                    announced = Some(source.clone());
                    if let Ok(mut guard) = shared.video_cues.lock() {
                        guard.clear();
                    }
                }
                // Off to be hashed; it comes back as `LayoutReady`.
                let _ = hash_tx.send(layout).await;
                continue;
            }
            BroadcastEvent::LayoutReady {
                source,
                assets,
                videos,
            } => {
                if let Ok(mut guard) = shared.last_layout.lock() {
                    *guard = Some(source.clone());
                }
                if let Ok(mut guard) = shared.layout_assets.lock() {
                    *guard = assets.clone();
                }
                if let Ok(mut guard) = shared.layout_videos.lock() {
                    *guard = videos.clone();
                }
                // Document first: a reflection sizes its expectations from the
                // document, so an asset arriving ahead of it has nowhere to go.
                std::iter::once(WireMessage::Layout { source, videos })
                    .chain(
                        assets
                            .into_iter()
                            .map(|(path, encoded)| WireMessage::Asset { path, encoded }),
                    )
                    .collect()
            }
        };

        let subscribers: Vec<Subscriber> = match shared.subscribers.lock() {
            Ok(mut guard) => std::mem::take(&mut *guard),
            Err(_) => continue,
        };

        let mut survivors = Vec::with_capacity(subscribers.len());
        'subscriber: for sub in subscribers {
            // The spec's "drop slow subscribers" rule: one whose queue can't
            // take the whole batch reconnects for a fresh replay rather than
            // wedge the broadcast. This loop is the queue's only producer, so
            // the capacity read here still holds for the sends below.
            if sub.sender.capacity() < wires.len() {
                log::debug!(
                    "multi_screen: dropping slow subscriber {}",
                    sub.endpoint_id.fmt_short()
                );
                continue;
            }
            for wire in &wires {
                if sub.sender.try_send(wire.clone()).is_err() {
                    log::debug!(
                        "multi_screen: dropping closed subscriber {}",
                        sub.endpoint_id.fmt_short()
                    );
                    continue 'subscriber;
                }
            }
            survivors.push(sub);
        }

        if let Ok(mut guard) = shared.subscribers.lock() {
            // Other subscribers may have been added by the service handler
            // while we were sending — preserve them.
            survivors.extend(guard.drain(..));
            *guard = survivors;
        }
    }
}
