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

pub use discovery::DiscoveredNode;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_channel::{Receiver as AsyncReceiver, Sender as AsyncSender};
use iroh::{Endpoint, EndpointId, SecretKey, protocol::Router};
use irpc::LocalSender;
use irpc::channel::mpsc::Sender as IrpcSender;
use irpc_iroh::IrohProtocol;
use tokio::sync::oneshot;

use proto::{DisplayProtocol, WireMessage};

pub use proto::PairingResponse;

use crate::spotify::{self, SharedSpotifyState, SpotifyState};

/// ALPN identifying our irpc service. Bumped when the protocol breaks
/// compatibility — keep this in sync with the spec's "service version" notion.
pub const ALPN: &[u8] = b"xyz.mooshq.guest-info-display.display/1";

/// Cross-runtime broadcast tunnel: GPUI thread pushes events synchronously,
/// the multi-screen runtime drains them and fans them out to subscribers.
enum BroadcastEvent {
    StateChanged(SpotifyState),
    CoverArt { url: String, encoded: Vec<u8> },
    CoversCleared,
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

/// Hands the GPUI side a primary-mirroring backend that produces the same
/// (state, events) pair as `spotify::start` would. Drop releases the dial loop.
pub struct ReflectionHandle {
    pub state: SharedSpotifyState,
    pub events: AsyncReceiver<spotify::Event>,
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
    pub endpoint_id: EndpointId,
    pub sender: IrpcSender<WireMessage>,
}

/// Subscriber-registry + snapshot cache shared between the service handler
/// (adds new subscribers, replies to Subscribe with snapshot+covers) and the
/// broadcast loop (updates the cache and fans live messages out).
struct Shared {
    subscribers: Mutex<Vec<Subscriber>>,
    last_state: Mutex<SpotifyState>,
    /// Encoded JPEG/PNG bytes keyed by Spotify cover URL. Populated each time
    /// a primary fetches a cover; cleared on session reset.
    cover_cache: Mutex<HashMap<String, Vec<u8>>>,
    inbound_trusted: TrustState,
}

impl Shared {
    fn new(inbound_trusted: TrustState) -> Arc<Self> {
        Arc::new(Self {
            subscribers: Mutex::new(Vec::new()),
            last_state: Mutex::new(SpotifyState::default()),
            cover_cache: Mutex::new(HashMap::new()),
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
        let _ = self.broadcast_tx.try_send(BroadcastEvent::StateChanged(state));
    }

    pub fn broadcast_cover(&self, url: String, encoded: Vec<u8>) {
        let _ = self
            .broadcast_tx
            .try_send(BroadcastEvent::CoverArt { url, encoded });
    }

    pub fn broadcast_covers_cleared(&self) {
        let _ = self.broadcast_tx.try_send(BroadcastEvent::CoversCleared);
    }

    /// Replace the set of endpoints that the primary accepts `Subscribe`
    /// calls from. Called at startup with the contents of
    /// `paired_peers(Direction::Inbound)`.
    pub fn seed_inbound_trusted(&self, ids: HashSet<EndpointId>) {
        if let Ok(mut guard) = self.inbound_trusted.lock() {
            *guard = ids;
        }
    }

    pub fn add_inbound_trusted(&self, id: EndpointId) {
        if let Ok(mut guard) = self.inbound_trusted.lock() {
            guard.insert(id);
        }
    }

    pub fn remove_inbound_trusted(&self, id: EndpointId) {
        if let Ok(mut guard) = self.inbound_trusted.lock() {
            guard.remove(&id);
        }
    }

    /// Drop any live subscriber stream pointing at `id`. Pair this with
    /// [`remove_inbound_trusted`] when the user removes a reflection so the
    /// stream closes immediately and the reflection can't reconnect.
    pub fn disconnect_subscriber(&self, id: EndpointId) {
        if self.cmd_tx.try_send(Command::DisconnectSubscriber(id)).is_err() {
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
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let cmd = Command::StartReflection {
            primary,
            state: state.clone(),
            events_tx,
            shutdown_rx,
        };
        if self.cmd_tx.try_send(cmd).is_err() {
            log::warn!("multi_screen: command channel full — reflection start dropped");
        }

        ReflectionHandle {
            state,
            events: events_rx,
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

    let thread = std::thread::Builder::new()
        .name("multi-screen".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("multi-screen tokio runtime");
            rt.block_on(MultiScreenRuntime {
                secret_key,
                shutdown: shutdown_rx,
                broadcast_rx,
                approvals_tx,
                discovered_tx,
                cmd_rx,
                inbound_trusted: inbound_trusted_run,
            }
            .run());
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
        let router = build_router(endpoint.clone(), shared.clone(), approvals_tx).await;
        tokio::spawn(broadcast_loop(broadcast_rx, shared.clone()));
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
                shutdown_rx,
            } => {
                tokio::spawn(reflection::run(
                    endpoint.clone(),
                    primary,
                    state,
                    events_tx,
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
async fn build_router(
    endpoint: Endpoint,
    shared: Arc<Shared>,
    approvals_tx: AsyncSender<PendingApproval>,
) -> Router {
    let (msg_tx, msg_rx) = tokio::sync::mpsc::channel(16);
    let local_sender: LocalSender<DisplayProtocol> = msg_tx.into();
    let proto = IrohProtocol::with_sender(local_sender);

    tokio::spawn(service::run(msg_rx, shared, approvals_tx));

    Router::builder(endpoint).accept(ALPN, proto).spawn()
}

/// Drain `broadcast_rx`, update the snapshot/cover cache, and fan each event
/// out to every live subscriber. Subscribers whose channels are closed or full
/// are dropped — slow reflections have to reconnect for a fresh snapshot
/// rather than wedge the broadcast.
async fn broadcast_loop(rx: AsyncReceiver<BroadcastEvent>, shared: Arc<Shared>) {
    while let Ok(event) = rx.recv().await {
        let wire = match event {
            BroadcastEvent::StateChanged(state) => {
                if let Ok(mut guard) = shared.last_state.lock() {
                    *guard = state.clone();
                }
                WireMessage::StateChanged(state)
            }
            BroadcastEvent::CoverArt { url, encoded } => {
                if let Ok(mut guard) = shared.cover_cache.lock() {
                    guard.insert(url.clone(), encoded.clone());
                }
                WireMessage::CoverArt { url, encoded }
            }
            BroadcastEvent::CoversCleared => {
                if let Ok(mut guard) = shared.cover_cache.lock() {
                    guard.clear();
                }
                WireMessage::CoversCleared
            }
        };

        let subscribers: Vec<Subscriber> = match shared.subscribers.lock() {
            Ok(mut guard) => std::mem::take(&mut *guard),
            Err(_) => continue,
        };

        let mut survivors = Vec::with_capacity(subscribers.len());
        for sub in subscribers {
            // try_send is non-blocking: returns immediately whether the
            // message was buffered or not, only erroring on closed channel.
            // That matches the spec's "drop slow subscribers" rule — they
            // reconnect for a fresh snapshot rather than wedge the broadcast.
            match sub.sender.try_send(wire.clone()).await {
                Ok(_) => survivors.push(sub),
                Err(e) => {
                    log::debug!(
                        "multi_screen: dropping subscriber {}: {e:?}",
                        sub.endpoint_id.fmt_short()
                    );
                }
            }
        }

        if let Ok(mut guard) = shared.subscribers.lock() {
            // Other subscribers may have been added by the service handler
            // while we were sending — preserve them.
            survivors.extend(guard.drain(..));
            *guard = survivors;
        }
    }
}

