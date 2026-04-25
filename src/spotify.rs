use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_channel::{Receiver, Sender, bounded};
use futures_util::StreamExt;
use librespot::{
    connect::{ConnectConfig, Spirc},
    core::{Session, SessionConfig, SpotifyUri, authentication::Credentials, config::DeviceType},
    discovery::Discovery,
    metadata::audio::{AudioItem, UniqueFields},
    playback::{
        audio_backend::{Sink, SinkResult},
        config::PlayerConfig,
        convert::Converter,
        decoder::AudioPacket,
        mixer::{self, MixerConfig, NoOpVolume},
        player::{Player, PlayerEvent},
    },
};

struct NullSink;

impl Sink for NullSink {
    fn write(&mut self, _packet: AudioPacket, _converter: &mut Converter) -> SinkResult<()> {
        Ok(())
    }
}

#[derive(Clone, Default)]
pub struct SpotifyTrackInfo {
    pub name: String,
    pub artists: String,
    pub cover_url: Option<String>,
}

#[derive(Clone, Default)]
pub struct SpotifyState {
    pub current: Option<SpotifyTrackInfo>,
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
}

pub struct SpotifyHandle {
    pub state: SharedSpotifyState,
    pub events: Receiver<Event>,
}

pub fn start(device_id: String) -> SpotifyHandle {
    let state: SharedSpotifyState = Arc::new(Mutex::new(SpotifyState::default()));
    let (tx, rx) = bounded::<Event>(16);

    let state_clone = state.clone();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(discovery_loop(device_id, state_clone, tx));
    });

    SpotifyHandle { state, events: rx }
}

async fn discovery_loop(device_id: String, state: SharedSpotifyState, tx: Sender<Event>) {
    let session_config = SessionConfig {
        device_id: device_id.clone(),
        ..SessionConfig::default()
    };
    let client_id = session_config.client_id.clone();

    let mut discovery = match Discovery::builder(device_id, client_id)
        .name("Guest Info Display")
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

    while let Some(credentials) = discovery.next().await {
        if let Err(e) = run_session(
            session_config.clone(),
            credentials,
            state.clone(),
            tx.clone(),
        )
        .await
        {
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
    credentials: Credentials,
    state: SharedSpotifyState,
    tx: Sender<Event>,
) -> Result<(), librespot::core::Error> {
    let session = Session::new(session_config, None);
    // Do NOT call session.connect() here — Spirc::new() does it internally
    // after registering dealer listeners; connecting early causes a double-connect.

    let player = Player::new(
        PlayerConfig::default(),
        session.clone(),
        Box::new(NoOpVolume),
        || Box::new(NullSink),
    );

    let mut event_rx = player.get_player_event_channel();

    let mk_mixer =
        mixer::find(None).ok_or_else(|| librespot::core::Error::unavailable("no mixer"))?;
    let mixer = mk_mixer(MixerConfig::default())?;

    let connect_config = ConnectConfig {
        name: "Guest Info Display".to_string(),
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

    tokio::pin!(spirc_task);

    loop {
        tokio::select! {
            _ = &mut spirc_task => {
                log::info!("spotify: Spirc task ended");
                break;
            }
            event = event_rx.recv() => {
                let Some(event) = event else { break; };
                handle_event(event, &session, &state, &tx, &fetched, &metadata_cache).await;
            }
        }
    }

    Ok(())
}

/// Maximum number of queue tracks to hydrate metadata for per update.
/// Keeps network usage bounded — the UI only shows a handful anyway.
const MAX_QUEUE_HYDRATE: usize = 10;

async fn handle_event(
    event: PlayerEvent,
    session: &Session,
    state: &SharedSpotifyState,
    tx: &Sender<Event>,
    fetched: &Arc<tokio::sync::Mutex<HashSet<String>>>,
    metadata_cache: &Arc<tokio::sync::Mutex<HashMap<String, SpotifyTrackInfo>>>,
) {
    let mut changed = false;
    let mut cover_to_fetch: Option<String> = None;

    match event {
        PlayerEvent::TrackChanged { audio_item } => {
            let info = track_info(&audio_item);
            cover_to_fetch = info.cover_url.clone();
            if let Ok(mut st) = state.lock() {
                st.current = Some(info);
                st.is_playing = true;
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
                if let Ok(mut st) = state.lock() {
                    st.current = Some(info);
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
        PlayerEvent::SetQueue { next_tracks, .. } => {
            handle_set_queue(next_tracks, session, state, tx, fetched, metadata_cache).await;
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

/// Handle a `SetQueue` player event emitted by Spirc whenever the queue changes
/// (context loaded, track added, queue set via Connect).
async fn handle_set_queue(
    next_tracks: Vec<librespot::playback::player::QueueTrack>,
    session: &Session,
    state: &SharedSpotifyState,
    tx: &Sender<Event>,
    fetched: &Arc<tokio::sync::Mutex<HashSet<String>>>,
    metadata_cache: &Arc<tokio::sync::Mutex<HashMap<String, SpotifyTrackInfo>>>,
) {
    log::debug!("spotify: SetQueue event, {} next tracks", next_tracks.len());

    let uris: Vec<String> = next_tracks
        .iter()
        .filter(|t| !t.uri.is_empty())
        .map(|t| t.uri.clone())
        .collect();

    // Hydrate metadata for the first N tracks we haven't seen yet.
    let cache = metadata_cache.lock().await;
    let mut to_hydrate: Vec<String> = Vec::new();
    for uri in &uris {
        if !cache.contains_key(uri) && to_hydrate.len() < MAX_QUEUE_HYDRATE {
            to_hydrate.push(uri.clone());
        }
    }
    drop(cache);

    // Fetch metadata in parallel for unknown tracks.
    let hydrate_futures: Vec<_> = to_hydrate
        .iter()
        .filter_map(|uri| {
            SpotifyUri::from_uri(uri).ok().map(|spotify_uri| {
                let session = session.clone();
                let uri = uri.clone();
                async move {
                    match AudioItem::get_file(&session, spotify_uri).await {
                        Ok(item) => Some((uri, track_info(&item))),
                        Err(e) => {
                            log::debug!("spotify: metadata hydrate failed for {uri}: {e}");
                            None
                        }
                    }
                }
            })
        })
        .collect();

    let results = futures_util::future::join_all(hydrate_futures).await;
    let mut cache = metadata_cache.lock().await;
    for result in results.into_iter().flatten() {
        cache.insert(result.0, result.1);
    }

    // Build the queue from cached metadata, falling back to URI-only placeholder.
    let queue: Vec<SpotifyTrackInfo> = uris
        .iter()
        .map(|uri| {
            cache.get(uri).cloned().unwrap_or_else(|| SpotifyTrackInfo {
                name: uri.clone(),
                artists: String::new(),
                cover_url: None,
            })
        })
        .collect();
    drop(cache);

    // Kick off cover fetches for queue items.
    let urls: Vec<String> = queue.iter().filter_map(|t| t.cover_url.clone()).collect();

    let changed = if let Ok(mut st) = state.lock() {
        let different = st.queue.len() != queue.len()
            || st
                .queue
                .iter()
                .zip(queue.iter())
                .any(|(a, b)| a.name != b.name);
        st.queue = queue;
        different
    } else {
        false
    };

    if changed {
        let _ = tx.send(Event::StateChanged).await;
    }

    for url in urls {
        kick_off_cover_fetch(url, session, tx, fetched).await;
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
        match fetch_cover(&session, &url).await {
            Ok((width, height, bgra)) => {
                let _ = tx
                    .send(Event::CoverLoaded(CoverImage {
                        url,
                        width,
                        height,
                        bgra,
                    }))
                    .await;
            }
            Err(e) => {
                log::debug!("spotify: cover fetch failed: {e}");
                // Drop from dedupe set so a retry can happen on the next event.
                fetched.lock().await.remove(&url);
            }
        }
    });
}

async fn fetch_cover(
    session: &Session,
    url: &str,
) -> Result<(u32, u32, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
    let req = http::Request::builder()
        .method("GET")
        .uri(url)
        .body(bytes::Bytes::new())?;
    let bytes = session.http_client().request_body(req).await?;

    let img = image::load_from_memory(&bytes)?.into_rgba8();
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
        name: item.name.clone(),
        artists,
        cover_url,
    }
}
