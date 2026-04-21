use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_channel::{Receiver, Sender, bounded};
use futures_util::StreamExt;
use librespot::{
    connect::{ConnectConfig, Spirc},
    core::{Session, SessionConfig, authentication::Credentials, config::DeviceType},
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
    pub album: String,
    pub cover_url: Option<String>,
}

#[derive(Clone, Default)]
pub struct SpotifyState {
    pub current: Option<SpotifyTrackInfo>,
    pub next: Option<SpotifyTrackInfo>,
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
        if let Err(e) =
            run_session(session_config.clone(), credentials, state.clone(), tx.clone()).await
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
        ..ConnectConfig::default()
    };

    let (_spirc, spirc_task) =
        Spirc::new(connect_config, session.clone(), credentials, player, mixer).await?;

    log::info!("spotify: Spirc active");

    // Per-session record of cover URLs we've already started fetching, to dedupe.
    let fetched: Arc<tokio::sync::Mutex<HashSet<String>>> =
        Arc::new(tokio::sync::Mutex::new(HashSet::new()));

    tokio::pin!(spirc_task);

    loop {
        tokio::select! {
            _ = &mut spirc_task => {
                log::info!("spotify: Spirc task ended");
                break;
            }
            event = event_rx.recv() => {
                let Some(event) = event else { break; };
                handle_event(event, &session, &state, &tx, &fetched).await;
            }
        }
    }

    Ok(())
}

async fn handle_event(
    event: PlayerEvent,
    session: &Session,
    state: &SharedSpotifyState,
    tx: &Sender<Event>,
    fetched: &Arc<tokio::sync::Mutex<HashSet<String>>>,
) {
    let mut changed = false;
    let mut cover_to_fetch: Option<String> = None;

    match event {
        PlayerEvent::TrackChanged { audio_item } => {
            let info = track_info(&audio_item);
            cover_to_fetch = info.cover_url.clone();
            if let Ok(mut st) = state.lock() {
                st.current = Some(info);
                st.next = None;
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
        PlayerEvent::Preloading { track_id } => {
            match AudioItem::get_file(session, track_id).await {
                Ok(item) => {
                    let info = track_info(&item);
                    cover_to_fetch = info.cover_url.clone();
                    if let Ok(mut st) = state.lock() {
                        st.next = Some(info);
                        changed = true;
                    }
                }
                Err(e) => log::debug!("spotify: preload metadata: {e}"),
            }
        }
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
        _ => {}
    }

    if changed {
        let _ = tx.send(Event::StateChanged).await;
    }

    if let Some(url) = cover_to_fetch {
        let mut guard = fetched.lock().await;
        if guard.insert(url.clone()) {
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
    }
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
    let album = match &item.unique_fields {
        UniqueFields::Track { album, .. } => album.clone(),
        UniqueFields::Episode { show_name, .. } => show_name.clone(),
        UniqueFields::Local { album, .. } => album.as_deref().unwrap_or("").to_string(),
    };
    let cover_url = item.covers.first().map(|c| c.url.clone());
    SpotifyTrackInfo {
        name: item.name.clone(),
        artists,
        album,
        cover_url,
    }
}
