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
        convert::Converter,
        decoder::AudioPacket,
        config::PlayerConfig,
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

pub struct SpotifyHandle {
    pub state: SharedSpotifyState,
    /// Yields `()` whenever the Spotify state changes.
    /// Executor-agnostic — safe to `recv()` from a GPUI task.
    pub updates: Receiver<()>,
}

pub fn start() -> SpotifyHandle {
    let state: SharedSpotifyState = Arc::new(Mutex::new(SpotifyState::default()));
    let (tx, rx) = bounded::<()>(16);

    let state_clone = state.clone();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(discovery_loop(state_clone, tx));
    });

    SpotifyHandle { state, updates: rx }
}

async fn discovery_loop(state: SharedSpotifyState, tx: Sender<()>) {
    let session_config = SessionConfig::default();
    let device_id = session_config.device_id.clone();
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
        let _ = tx.send(()).await;
    }
}

async fn run_session(
    session_config: SessionConfig,
    credentials: Credentials,
    state: SharedSpotifyState,
    tx: Sender<()>,
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

    tokio::pin!(spirc_task);

    loop {
        tokio::select! {
            _ = &mut spirc_task => {
                log::info!("spotify: Spirc task ended");
                break;
            }
            event = event_rx.recv() => {
                let Some(event) = event else { break; };
                handle_event(event, &session, &state, &tx).await;
            }
        }
    }

    Ok(())
}

async fn handle_event(
    event: PlayerEvent,
    session: &Session,
    state: &SharedSpotifyState,
    tx: &Sender<()>,
) {
    let mut changed = false;

    match event {
        PlayerEvent::TrackChanged { audio_item } => {
            let info = track_info(&audio_item);
            if let Ok(mut st) = state.lock() {
                st.current = Some(info);
                st.next = None;
                st.is_playing = true;
                changed = true;
            }
        }
        // Fires before TrackChanged; catches the case where Spirc interrupts
        // the load with Stop before start_playback is reached.
        PlayerEvent::Loading { track_id, .. } => {
            match AudioItem::get_file(session, track_id).await {
                Ok(item) => {
                    if let Ok(mut st) = state.lock() {
                        st.current = Some(track_info(&item));
                        changed = true;
                    }
                }
                Err(e) => log::debug!("spotify: loading metadata: {e}"),
            }
        }
        PlayerEvent::Preloading { track_id } => {
            match AudioItem::get_file(session, track_id).await {
                Ok(item) => {
                    if let Ok(mut st) = state.lock() {
                        st.next = Some(track_info(&item));
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
        let _ = tx.send(()).await;
    }
}

fn track_info(item: &AudioItem) -> SpotifyTrackInfo {
    let artists = match &item.unique_fields {
        UniqueFields::Track { artists, .. } => {
            artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")
        }
        UniqueFields::Episode { show_name, .. } => show_name.clone(),
        UniqueFields::Local { artists, .. } => artists.as_deref().unwrap_or("").to_string(),
    };
    let album = match &item.unique_fields {
        UniqueFields::Track { album, .. } => album.clone(),
        UniqueFields::Episode { show_name, .. } => show_name.clone(),
        UniqueFields::Local { album, .. } => album.as_deref().unwrap_or("").to_string(),
    };
    let cover_url = item.covers.first().map(|c| c.url.clone());
    SpotifyTrackInfo { name: item.name.clone(), artists, album, cover_url }
}
