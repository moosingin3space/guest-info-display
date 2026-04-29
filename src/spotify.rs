use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_channel::{Receiver, Sender, bounded};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use librespot::{
    connect::{ConnectConfig, Spirc},
    core::{Session, SessionConfig, SpotifyUri, authentication::Credentials, config::DeviceType},
    discovery::Discovery,
    metadata::audio::{AudioItem, UniqueFields},
    playback::{
        audio_backend,
        config::{AudioFormat, PlayerConfig},
        mixer::{self, MixerConfig},
        player::{Player, PlayerEvent, QueueTrack},
    },
};
use tokio::task::JoinHandle;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SpotifyTrackInfo {
    pub uri: String,
    pub name: String,
    pub artists: String,
    pub cover_url: Option<String>,
}

impl SpotifyTrackInfo {
    /// True once metadata has been hydrated. Placeholders set `name = uri`,
    /// so a mismatch means we have a real name.
    pub fn is_resolved(&self) -> bool {
        !self.uri.is_empty() && self.uri != self.name
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SpotifyState {
    pub current: Option<SpotifyTrackInfo>,
    /// Previously-played tracks, oldest first. Used to restore upcoming entries
    /// when the user navigates backward.
    pub history: Vec<SpotifyTrackInfo>,
    pub queue: Vec<SpotifyTrackInfo>,
    pub is_playing: bool,
}

pub type SharedSpotifyState = Arc<Mutex<SpotifyState>>;

/// A fetched and decoded piece of cover art, in BGRA8 pixel format (GPUI's internal layout).
pub struct CoverImage {
    pub url: String,
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    /// Original encoded JPEG/PNG bytes from Spotify's CDN, retained so primaries
    /// can forward to subscribers over the wire without re-fetching.
    pub encoded: Vec<u8>,
}

/// Single channel of Spotify updates to the UI. Executor-agnostic — safe to `recv()`
/// from a GPUI task.
pub enum Event {
    /// `SpotifyState` has been mutated; re-read `SharedSpotifyState`.
    StateChanged,
    /// Cover art finished fetching and decoding.
    CoverLoaded(CoverImage),
    /// Session ended — drop any cached artwork.
    CoversCleared,
    /// Reflection-only: the multi-screen subscription dropped. The local backend
    /// (librespot) never produces this; primaries always render their own state.
    ConnectionLost,
    /// Reflection-only: a fresh snapshot has been received from the primary.
    ConnectionRestored,
}

pub struct SpotifyHandle {
    pub state: SharedSpotifyState,
    pub events: Receiver<Event>,
    /// Holding this keeps the spotify thread alive. On drop, the receiver in
    /// the discovery loop sees the channel close and the loop exits, which
    /// brings down the runtime and any in-flight session.
    _shutdown: Sender<()>,
}

/// Spotify Connect display name. Includes the device-id so multiple instances
/// on the same network are distinguishable in the Spotify "Devices" picker.
fn display_name(device_id: &str) -> String {
    format!("Guest Info Display ({device_id})")
}

pub fn start(device_id: String, audio_device: Option<String>) -> SpotifyHandle {
    let state: SharedSpotifyState = Arc::new(Mutex::new(SpotifyState::default()));
    let (tx, rx) = bounded::<Event>(16);
    let (shutdown_tx, shutdown_rx) = bounded::<()>(1);

    let state_clone = state.clone();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(discovery_loop(
                device_id,
                audio_device,
                state_clone,
                tx,
                shutdown_rx,
            ));
    });

    SpotifyHandle {
        state,
        events: rx,
        _shutdown: shutdown_tx,
    }
}

async fn discovery_loop(
    device_id: String,
    audio_device: Option<String>,
    state: SharedSpotifyState,
    tx: Sender<Event>,
    shutdown: Receiver<()>,
) {
    let session_config = SessionConfig {
        device_id: device_id.clone(),
        ..SessionConfig::default()
    };
    let client_id = session_config.client_id.clone();

    let mut discovery = match Discovery::builder(device_id.clone(), client_id)
        .name(display_name(&device_id))
        .device_type(DeviceType::Computer)
        .launch()
    {
        Ok(d) => d,
        Err(e) => {
            log::error!("spotify: failed to start discovery: {e}");
            return;
        }
    };

    log::info!("spotify: listening for Spotify Connect");

    loop {
        let credentials = tokio::select! {
            biased;
            _ = shutdown.recv() => {
                log::info!("spotify: shutdown requested, stopping discovery");
                return;
            }
            cred = discovery.next() => match cred {
                Some(c) => c,
                None => return,
            },
        };

        let session_result = tokio::select! {
            biased;
            _ = shutdown.recv() => {
                log::info!("spotify: shutdown requested, ending active session");
                return;
            }
            r = run_session(
                session_config.clone(),
                audio_device.clone(),
                credentials,
                state.clone(),
                tx.clone(),
            ) => r,
        };

        if let Err(e) = session_result {
            log::warn!("spotify: session ended: {e}");
        }
        if let Ok(mut st) = state.lock() {
            *st = SpotifyState::default();
        }
        let _ = tx.send(Event::StateChanged).await;
        let _ = tx.send(Event::CoversCleared).await;
    }
}

async fn run_session(
    session_config: SessionConfig,
    audio_device: Option<String>,
    credentials: Credentials,
    state: SharedSpotifyState,
    tx: Sender<Event>,
) -> Result<(), librespot::core::Error> {
    let display_name = display_name(&session_config.device_id);
    let session = Session::new(session_config, None);
    // Do NOT call session.connect() here — Spirc::new() does it internally
    // after registering dealer listeners; connecting early causes a double-connect.

    let mk_mixer =
        mixer::find(None).ok_or_else(|| librespot::core::Error::unavailable("no mixer"))?;
    let mixer = mk_mixer(MixerConfig::default())?;

    let sink_builder = audio_backend::find(Some("rodio".to_string()))
        .ok_or_else(|| librespot::core::Error::unavailable("rodio sink not compiled in"))?;
    let format = AudioFormat::default();
    let sink_device = audio_device.clone();

    let player = Player::new(
        PlayerConfig::default(),
        session.clone(),
        mixer.get_soft_volume(),
        move || sink_builder(sink_device, format),
    );

    let mut event_rx = player.get_player_event_channel();

    let connect_config = ConnectConfig {
        name: display_name,
        device_type: DeviceType::Computer,
        emit_set_queue_events: true,
        ..ConnectConfig::default()
    };

    let (_spirc, spirc_task) =
        Spirc::new(connect_config, session.clone(), credentials, player, mixer).await?;

    log::info!("spotify: Spirc active");

    // Per-session record of cover URLs we've already started fetching, to dedupe.
    let fetched: Arc<tokio::sync::Mutex<HashSet<String>>> =
        Arc::new(tokio::sync::Mutex::new(HashSet::new()));

    // Per-session cache of track metadata (URI → display info), so we only
    // hydrate each track once.
    let metadata_cache: Arc<tokio::sync::Mutex<HashMap<String, SpotifyTrackInfo>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    // Currently-running background hydration; replaced (and aborted) on each
    // SetQueue so we never keep stale work around for tracks no longer queued.
    let mut hydration_task: Option<JoinHandle<()>> = None;

    tokio::pin!(spirc_task);

    loop {
        tokio::select! {
            _ = &mut spirc_task => {
                log::info!("spotify: Spirc task ended");
                break;
            }
            event = event_rx.recv() => {
                let Some(event) = event else { break; };
                handle_event(
                    event,
                    &session,
                    &state,
                    &tx,
                    &fetched,
                    &metadata_cache,
                    &mut hydration_task,
                )
                .await;
            }
        }
    }

    if let Some(h) = hydration_task.take() {
        h.abort();
    }

    Ok(())
}

async fn handle_event(
    event: PlayerEvent,
    session: &Session,
    state: &SharedSpotifyState,
    tx: &Sender<Event>,
    fetched: &Arc<tokio::sync::Mutex<HashSet<String>>>,
    metadata_cache: &Arc<tokio::sync::Mutex<HashMap<String, SpotifyTrackInfo>>>,
    hydration_task: &mut Option<JoinHandle<()>>,
) {
    let mut changed = false;
    let mut cover_to_fetch: Option<String> = None;

    match event {
        PlayerEvent::TrackChanged { audio_item } => {
            let info = track_info(&audio_item);
            cover_to_fetch = info.cover_url.clone();
            if navigate_to(state, info, /*is_playing*/ Some(true)) {
                changed = true;
            }
        }
        // Fires before TrackChanged; catches the case where Spirc interrupts
        // the load with Stop before start_playback is reached.
        PlayerEvent::Loading { track_id, .. } => match AudioItem::get_file(session, track_id).await
        {
            Ok(item) => {
                let info = track_info(&item);
                cover_to_fetch = info.cover_url.clone();
                if navigate_to(state, info, None) {
                    changed = true;
                }
            }
            Err(e) => log::debug!("spotify: loading metadata: {e}"),
        },
        PlayerEvent::Playing { .. } => {
            if let Ok(mut st) = state.lock() {
                st.is_playing = true;
                changed = true;
            }
        }
        PlayerEvent::Paused { .. } | PlayerEvent::Stopped { .. } => {
            if let Ok(mut st) = state.lock() {
                st.is_playing = false;
                changed = true;
            }
        }
        PlayerEvent::SetQueue {
            next_tracks,
            prev_tracks,
            current_track,
            ..
        } => {
            HandleSetQueue {
                next_tracks,
                prev_tracks,
                current_track,
                session,
                state,
                tx,
                fetched,
                metadata_cache,
                hydration_task,
            }
            .execute()
            .await;
            // handle_set_queue sends its own StateChanged if needed.
            return;
        }
        _ => {}
    }

    if changed {
        let _ = tx.send(Event::StateChanged).await;
    }

    if let Some(url) = cover_to_fetch {
        kick_off_cover_fetch(url, session, tx, fetched).await;
    }
}

/// Slide `new_current` into place, shifting tracks between `queue` and `history`
/// so the upcoming list stays accurate without a fresh `SetQueue` from librespot
/// (which is not emitted on natural advance, skip-next, or skip-prev).
///
/// `is_playing_override` lets `TrackChanged` force `is_playing = true` without a
/// separate write; pass `None` to leave the flag alone (e.g. on `Loading`).
fn navigate_to(
    state: &SharedSpotifyState,
    new_current: SpotifyTrackInfo,
    is_playing_override: Option<bool>,
) -> bool {
    let Ok(mut st) = state.lock() else {
        return false;
    };
    let new_uri = new_current.uri.clone();

    if let Some(playing) = is_playing_override {
        st.is_playing = playing;
    }

    // Same track as before: just refresh in case metadata improved.
    if st.current.as_ref().is_some_and(|c| c.uri == new_uri) {
        st.current = Some(new_current);
        return true;
    }

    // Forward: new track sits in the upcoming queue. Move the old current and
    // any tracks before it into history; pop the new current off the queue.
    if !new_uri.is_empty()
        && let Some(pos) = st.queue.iter().position(|t| t.uri == new_uri)
    {
        if let Some(old) = st.current.take() {
            st.history.push(old);
        }
        let drained: Vec<_> = st.queue.drain(..pos).collect();
        st.history.extend(drained);
        st.queue.remove(0);
        st.current = Some(new_current);
        return true;
    }

    // Backward: new track sits in history. Push the old current to the front of
    // the queue, move any history entries newer than the target back to the
    // queue (preserving order), then pop the target itself.
    if !new_uri.is_empty()
        && let Some(pos) = st.history.iter().rposition(|t| t.uri == new_uri)
    {
        if let Some(old) = st.current.take() {
            st.queue.insert(0, old);
        }
        let after: Vec<_> = st.history.drain(pos + 1..).collect();
        for t in after.into_iter().rev() {
            st.queue.insert(0, t);
        }
        st.history.pop();
        st.current = Some(new_current);
        return true;
    }

    // Unknown jump (e.g. transferred playback to a new context): just record
    // the current track and wait for the next SetQueue to repopulate.
    st.current = Some(new_current);
    true
}

/// Handle a `SetQueue` player event emitted by Spirc on context load and
/// explicit queue mutations.
struct HandleSetQueue<'a> {
    next_tracks: Vec<QueueTrack>,
    prev_tracks: Vec<QueueTrack>,
    /// The track Spirc reports as currently playing. Spirc emits this on
    /// transfer-with-song-in-progress *before* the player issues `Loading`
    /// for the same track, so seeding `state.current` here means the cover
    /// fetch can start without waiting for the player's load to complete.
    current_track: Option<QueueTrack>,
    session: &'a Session,
    state: &'a SharedSpotifyState,
    tx: &'a Sender<Event>,
    fetched: &'a Arc<tokio::sync::Mutex<HashSet<String>>>,
    metadata_cache: &'a Arc<tokio::sync::Mutex<HashMap<String, SpotifyTrackInfo>>>,
    hydration_task: &'a mut Option<JoinHandle<()>>,
}

impl<'a> HandleSetQueue<'a> {
    async fn execute(self) {
        let HandleSetQueue {
            next_tracks,
            prev_tracks,
            current_track,
            session,
            state,
            tx,
            fetched,
            metadata_cache,
            hydration_task,
        } = self;

        log::debug!(
            "spotify: SetQueue: {} next, {} prev, current = {:?}",
            next_tracks.len(),
            prev_tracks.len(),
            current_track.as_ref().map(|t| &t.uri),
        );

        let next_uris: Vec<String> = next_tracks
            .into_iter()
            .filter(|t| !t.uri.is_empty())
            .map(|t| t.uri)
            .collect();
        let prev_uris: Vec<String> = prev_tracks
            .into_iter()
            .filter(|t| !t.uri.is_empty())
            .map(|t| t.uri)
            .collect();
        let current_uri: Option<String> = current_track
            .map(|t| t.uri)
            .filter(|u| !u.is_empty());

        // Synchronously hydrate the first few visible queue entries so the panel
        // never opens with raw `spotify:track:...` strings; the background task
        // below picks up the long tail.
        let mut prehydrate_uris: Vec<String> = current_uri.iter().cloned().collect();
        prehydrate_uris.extend(next_uris.iter().cloned());
        prehydrate_visible(&prehydrate_uris, session, metadata_cache).await;

        // Build placeholder/cached entries synchronously; actual hydration runs in
        // the background task spawned below so the UI never wedges on slow lookups.
        let (queue, history, current_info, mut cached_cover_urls) = {
            let cache = metadata_cache.lock().await;
            let make = |uri: &String| {
                cache.get(uri).cloned().unwrap_or_else(|| SpotifyTrackInfo {
                    uri: uri.clone(),
                    name: uri.clone(),
                    artists: String::new(),
                    cover_url: None,
                })
            };
            let queue: Vec<_> = next_uris.iter().map(&make).collect();
            let history: Vec<_> = prev_uris.iter().map(&make).collect();
            let current_info = current_uri.as_ref().map(&make);
            let mut urls: Vec<_> = queue
                .iter()
                .chain(history.iter())
                .filter_map(|t| t.cover_url.clone())
                .collect();
            if let Some(url) = current_info.as_ref().and_then(|t| t.cover_url.clone()) {
                urls.push(url);
            }
            (queue, history, current_info, urls)
        };

        let changed = if let Ok(mut st) = state.lock() {
            let queue_different = st.queue.len() != queue.len()
                || st
                    .queue
                    .iter()
                    .zip(queue.iter())
                    .any(|(a, b)| a.uri != b.uri);
            let history_different = st.history.len() != history.len()
                || st
                    .history
                    .iter()
                    .zip(history.iter())
                    .any(|(a, b)| a.uri != b.uri);
            st.queue = queue;
            st.history = history;

            // Seed `current` from SetQueue only if we don't already have a
            // matching one — this preserves any richer info already populated
            // by a preceding `Loading` / `TrackChanged`. When the URIs differ,
            // SetQueue is authoritative (e.g. transfer-with-song-in-progress
            // before the local player has loaded the track).
            let current_different = match (&st.current, &current_info) {
                (None, Some(_)) => true,
                (Some(cur), Some(new)) if cur.uri != new.uri => true,
                _ => false,
            };
            if let Some(info) = current_info
                && current_different
            {
                st.current = Some(info);
            }

            queue_different || history_different || current_different
        } else {
            false
        };

        if changed {
            let _ = tx.send(Event::StateChanged).await;
        }

        // Drop any duplicate URLs so we don't hammer the dedupe set with
        // identical no-op calls in a tight loop.
        cached_cover_urls.sort();
        cached_cover_urls.dedup();
        for url in cached_cover_urls {
            kick_off_cover_fetch(url, session, tx, fetched).await;
        }

        // Cancel any in-flight hydration before queuing the new pass — its URIs
        // may no longer be present after this SetQueue.
        if let Some(h) = hydration_task.take() {
            h.abort();
        }

        // Hydrate upcoming tracks first (visible), then prev (only needed if the
        // user hits "previous"). Include the current track URI so a slow
        // metadata lookup eventually populates its cover even when the
        // synchronous prehydrate failed (e.g. session not fully warm yet).
        let mut to_hydrate: Vec<String> = current_uri.into_iter().collect();
        to_hydrate.extend(next_uris);
        to_hydrate.extend(prev_uris);

        *hydration_task = Some(tokio::spawn(hydrate_uris(
            to_hydrate,
            session.clone(),
            state.clone(),
            tx.clone(),
            fetched.clone(),
            metadata_cache.clone(),
        )));
    }
}

/// Number of upcoming tracks shown in the UI; we hydrate at least this many
/// before returning from `handle_set_queue` so the panel is never seeded with
/// raw URI strings.
const VISIBLE_QUEUE: usize = 5;

/// Block until the first `VISIBLE_QUEUE` uncached entries from `uris` are
/// resolved (in parallel). Failures are tolerated — the background hydrator
/// will retry on the next `SetQueue`.
async fn prehydrate_visible(
    uris: &[String],
    session: &Session,
    metadata_cache: &Arc<tokio::sync::Mutex<HashMap<String, SpotifyTrackInfo>>>,
) {
    let needed: Vec<String> = {
        let cache = metadata_cache.lock().await;
        uris.iter()
            .filter(|u| !cache.contains_key(*u))
            .take(VISIBLE_QUEUE)
            .cloned()
            .collect()
    };
    if needed.is_empty() {
        return;
    }

    let futures = needed.into_iter().filter_map(|uri| {
        SpotifyUri::from_uri(&uri).ok().map(|spotify_uri| {
            let session = session.clone();
            async move {
                match AudioItem::get_file(&session, spotify_uri).await {
                    Ok(item) => Some((uri, track_info(&item))),
                    Err(e) => {
                        log::debug!("spotify: prehydrate failed for {uri}: {e}");
                        None
                    }
                }
            }
        })
    });

    let results = futures_util::future::join_all(futures).await;
    let mut cache = metadata_cache.lock().await;
    for (uri, info) in results.into_iter().flatten() {
        cache.insert(uri, info);
    }
}

/// Background metadata hydration: fetch one URI at a time, stitch the result
/// into `state`, and emit `StateChanged` so the UI refreshes incrementally.
/// The task is cancellable by aborting its `JoinHandle`; partial progress is
/// preserved in the metadata cache for reuse on the next `SetQueue`.
async fn hydrate_uris(
    uris: Vec<String>,
    session: Session,
    state: SharedSpotifyState,
    tx: Sender<Event>,
    fetched: Arc<tokio::sync::Mutex<HashSet<String>>>,
    metadata_cache: Arc<tokio::sync::Mutex<HashMap<String, SpotifyTrackInfo>>>,
) {
    for uri in uris {
        if metadata_cache.lock().await.contains_key(&uri) {
            continue;
        }

        let spotify_uri = match SpotifyUri::from_uri(&uri) {
            Ok(s) => s,
            Err(e) => {
                log::debug!("spotify: hydrate: invalid uri {uri}: {e}");
                continue;
            }
        };

        let info = match AudioItem::get_file(&session, spotify_uri).await {
            Ok(item) => track_info(&item),
            Err(e) => {
                log::debug!("spotify: hydrate: metadata failed for {uri}: {e}");
                continue;
            }
        };

        metadata_cache
            .lock()
            .await
            .insert(uri.clone(), info.clone());

        let cover_url = info.cover_url.clone();

        let updated = if let Ok(mut st) = state.lock() {
            let mut hit = false;
            for entry in &mut st.queue {
                if entry.uri == uri {
                    *entry = info.clone();
                    hit = true;
                }
            }
            for entry in &mut st.history {
                if entry.uri == uri {
                    *entry = info.clone();
                    hit = true;
                }
            }
            if let Some(cur) = st.current.as_mut()
                && cur.uri == uri
            {
                *cur = info.clone();
                hit = true;
            }
            hit
        } else {
            false
        };

        if updated {
            let _ = tx.send(Event::StateChanged).await;
        }

        if let Some(url) = cover_url {
            kick_off_cover_fetch(url, &session, &tx, &fetched).await;
        }
    }
}

/// Spawns a tokio task to fetch + decode a cover image if we haven't already
/// started one for this URL.
async fn kick_off_cover_fetch(
    url: String,
    session: &Session,
    tx: &Sender<Event>,
    fetched: &Arc<tokio::sync::Mutex<HashSet<String>>>,
) {
    let mut guard = fetched.lock().await;
    if !guard.insert(url.clone()) {
        return;
    }
    drop(guard);
    let session = session.clone();
    let tx = tx.clone();
    let fetched = fetched.clone();
    tokio::spawn(async move {
        // Retry a few times with backoff: on first connect / transfer the
        // session's HTTP client is occasionally not ready to talk to the
        // Spotify CDN, and a one-shot fetch silently leaves the cover blank.
        let mut delay = std::time::Duration::from_millis(500);
        for attempt in 0..4 {
            match fetch_cover(&session, &url).await {
                Ok((width, height, bgra, encoded)) => {
                    let _ = tx
                        .send(Event::CoverLoaded(CoverImage {
                            url,
                            width,
                            height,
                            bgra,
                            encoded,
                        }))
                        .await;
                    return;
                }
                Err(e) => {
                    log::debug!(
                        "spotify: cover fetch failed (attempt {}): {e}",
                        attempt + 1,
                    );
                }
            }
            tokio::time::sleep(delay).await;
            delay *= 2;
        }
        // Give up: drop from dedupe set so a later StateChanged-triggered
        // call can take another swing.
        fetched.lock().await.remove(&url);
    });
}

async fn fetch_cover(
    session: &Session,
    url: &str,
) -> Result<(u32, u32, Vec<u8>, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
    let req = http::Request::builder()
        .method("GET")
        .uri(url)
        .body(bytes::Bytes::new())?;
    let bytes = session.http_client().request_body(req).await?;
    let encoded = bytes.to_vec();
    let (w, h, bgra) = decode_cover_bytes(&bytes)?;
    Ok((w, h, bgra, encoded))
}

/// Decodes encoded JPEG/PNG bytes into BGRA pixels suitable for [`CoverImage`].
/// Used both by the local cover-fetch path and by reflections receiving
/// cover-art payloads over the wire.
pub fn decode_cover_bytes(
    encoded: &[u8],
) -> Result<(u32, u32, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
    let img = image::load_from_memory(encoded)?.into_rgba8();
    let (w, h) = img.dimensions();
    let mut buf = img.into_raw();
    // GPUI's renderer expects BGRA; the `image` crate decodes to RGBA.
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Ok((w, h, buf))
}

fn track_info(item: &AudioItem) -> SpotifyTrackInfo {
    let artists = match &item.unique_fields {
        UniqueFields::Track { artists, .. } => artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        UniqueFields::Episode { show_name, .. } => show_name.clone(),
        UniqueFields::Local { artists, .. } => artists.as_deref().unwrap_or("").to_string(),
    };
    let cover_url = item.covers.first().map(|c| c.url.clone());
    SpotifyTrackInfo {
        uri: item.uri.clone(),
        name: item.name.clone(),
        artists,
        cover_url,
    }
}

