//! One `video` node's GStreamer pipeline.
//!
//! `playbin3` does the demuxing, decoding and A/V sync. Its video sink is an
//! `appsink` we read frames from; its audio sink, when the node is not muted,
//! is a PulseAudio stream of its own, so the host balances the clip against
//! Spotify in the system mixer.
//!
//! The playlist rides on `about-to-finish`: playbin asks for the next URI while
//! the current clip still has data queued, which is what makes the cut between
//! clips — and a single clip looping onto itself — gapless.
//!
//! Gapless is only used for a clip looping onto itself. A gapless cut to a
//! *different* file stalls playbin3 without an error whenever the two differ
//! enough — a codec change swaps the VA decoder, which opens a display the
//! running postprocessor refuses to adopt; a soundtrack that disappears leaves
//! it waiting for audio forever — and a night of mixed clips will contain
//! exactly that. So a cut between files is a restart: let the clip end, then
//! start the next one from NULL.
//!
//! ## Across screens
//!
//! On a primary the player *leads*: it runs on the shared clock and, at each
//! clip's first frame, publishes a [`Cue`] saying which clip and when its
//! position 0 was on screen. On a reflection it *follows*: it locks onto a
//! cued clip by choosing its pipeline's base time so the same frame lands at
//! the same instant, nudges the base time when a later cue shows it has
//! drifted, and plays the clip the primary queued next. Until it can follow —
//! no clock yet, or a cued clip it does not have — it plays on its own.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;

use super::playlist::Playlist;
use super::sync::{self, Cue, Mode, Readiness};
use super::{FrameKind, clock, frame_kind};
use crate::inhibitor::VideoLease;

/// Drift a follower lets stand: well under a frame at 30 fps.
const TOLERANCE_NS: i64 = 8_000_000;
/// Drift a follower corrects by moving its base time; beyond this, it reloads.
const NUDGE_LIMIT_NS: i64 = 2_000_000_000;
/// How far ahead of now a follower aims when it locks on, to cover opening,
/// seeking and prerolling the clip.
const LOCK_LEAD_NS: u64 = 400_000_000;
/// How far ahead a primary schedules a start it has waited for, so the cue
/// reaches every reflection before the moment it names.
const START_LEAD_NS: u64 = 500_000_000;
/// How long a primary holding its first clip also waits for paired reflections
/// that have not connected yet. At a cold start the primary is usually up
/// first; without this it would find nobody to wait for and start alone.
const ARRIVAL_GRACE: Duration = Duration::from_secs(10);

/// What a `video` node asked to play, resolved to files on this device.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Config {
    /// The node's document-order id, which names it in cues.
    pub node: u32,
    /// The layout paths of its clips, which name them in cues.
    pub paths: Vec<String>,
    /// One entry per path: the file, or `None` while it is missing — on a
    /// reflection, still on its way.
    pub items: Vec<Option<PathBuf>>,
    pub muted: bool,
    pub looping: bool,
    pub shuffle: bool,
    /// On a primary: hold the first clip until the reflections have it.
    pub wait: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// Opening the first clip; nothing on screen yet.
    Starting,
    /// A primary holding its first clip on its first frame for `screens`
    /// reflections, the slowest of which has downloaded `progress` of it.
    Waiting {
        screens: usize,
        progress: Option<f32>,
    },
    /// A reflection getting the clip its primary is holding ready to start
    /// together: downloading it (`progress` of it so far), loading it, or —
    /// once `ready` — waiting for the start.
    Joining {
        ready: bool,
        progress: Option<f32>,
    },
    Playing,
    /// A non-looping playlist ran out. Its last frame stays up.
    Finished,
    /// Nothing in the playlist could be played.
    Failed(String),
}

/// State shared between GStreamer's threads and the painter.
pub struct Shared {
    /// The newest decoded frame. Painting takes a reference, never the sample
    /// itself, so a repaint without a new frame redraws the same one.
    pub frame: Mutex<Option<gst::Sample>>,
    pub status: Mutex<Status>,
    pub kind: FrameKind,
    /// Nudges the view to repaint: a new frame or a new status.
    wake: async_channel::Sender<()>,
}

impl Shared {
    fn set_status(&self, status: Status) {
        *self.status.lock().unwrap() = status;
        let _ = self.wake.try_send(());
    }
}

/// File URIs by playlist index; `None` where the file is not here (yet).
type Uris = Arc<Mutex<Vec<Option<String>>>>;

/// The cross-screen half of a player's state.
struct Sync {
    node: u32,
    paths: Vec<String>,
    mode: Mode,
    /// Lead: the cue last published.
    published: Mutex<Option<Cue>>,
    /// Lead: the clip already picked to follow this one. Picked as a clip
    /// starts, not at its end, so the cue announcing the start can name it and
    /// reflections have the whole clip's length to queue the same one.
    planned: Mutex<Option<usize>>,
    /// Follow: the pipeline runs on the shared clock with a base time chosen
    /// from a cue, so its timings compare with the primary's.
    locked: AtomicBool,
    /// Follow: the clip the primary queued next, by our index.
    next: Mutex<Option<usize>>,
    /// Pipeline-clock time position 0 of the current clip was on screen,
    /// measured from its first frame.
    pos0: Mutex<Option<u64>>,
    /// Bumped on every restart from NULL. A restarted clip opens with the same
    /// segment as the last restarted one, so the segment alone cannot tell
    /// two of them apart.
    restarts: AtomicUsize,
}

impl Sync {
    fn index_of(&self, path: &str) -> Option<usize> {
        self.paths.iter().position(|p| p == path)
    }
}

pub struct Player {
    pipeline: gst::Element,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    wake: async_channel::Receiver<()>,
    uris: Uris,
    playlist: Arc<Mutex<Playlist>>,
}

fn to_uris(items: &[Option<PathBuf>]) -> Vec<Option<String>> {
    items
        .iter()
        .map(|item| {
            let path = item.as_ref()?;
            match gst::glib::filename_to_uri(path, None) {
                Ok(uri) => Some(uri.to_string()),
                Err(e) => {
                    log::warn!("video: {}: {e}", path.display());
                    None
                }
            }
        })
        .collect()
}

impl Player {
    /// Build the pipeline and start the first clip.
    pub fn start(config: Config, lease: VideoLease) -> Result<Self, String> {
        let kind = frame_kind();
        let (wake_tx, wake) = async_channel::bounded(1);
        let shared = Arc::new(Shared {
            frame: Mutex::new(None),
            status: Mutex::new(Status::Starting),
            kind: kind.clone(),
            wake: wake_tx,
        });

        let uris = to_uris(&config.items);
        let playlist = Arc::new(Mutex::new(Playlist::new(
            uris.iter().map(Option::is_some).collect(),
            config.looping,
            config.shuffle,
        )));
        let first = playlist
            .lock()
            .unwrap()
            .advance()
            .ok_or("none of the clips are here yet")?;
        let first_uri = uris[first].clone().expect("advance picks available items");
        let uris: Uris = Arc::new(Mutex::new(uris));
        // The clip whose frames come next: set when a clip is queued, read at
        // its first frame. `current` names the clip decoding, for errors, and
        // moves on STREAM_START.
        let queued = Arc::new(AtomicUsize::new(first));
        let current = Arc::new(AtomicUsize::new(first));
        // Cleared at each STREAM_START; the first frame after proves the clip
        // decodes, which is what lets a broken file count as a failure.
        let produced_frame = Arc::new(AtomicBool::new(false));
        let sync = Arc::new(Sync {
            node: config.node,
            paths: config.paths.clone(),
            mode: sync::mode(),
            published: Mutex::new(None),
            planned: Mutex::new(None),
            locked: AtomicBool::new(false),
            next: Mutex::new(None),
            pos0: Mutex::new(None),
            restarts: AtomicUsize::new(0),
        });

        log::debug!(
            "video: node {} starting as {:?}, muted {}",
            config.node,
            sync.mode,
            config.muted
        );
        let video_sink = video_sink(&kind, &shared, &playlist, &queued, &produced_frame, &sync)?;
        let playbin = gst::ElementFactory::make("playbin3")
            .property("uri", &first_uri)
            .property("video-sink", &video_sink)
            .build()
            .map_err(|e| format!("playbin3: {e}"))?;
        // No subtitle overlay: it cannot draw onto DMA-BUF frames and would
        // break negotiation on the zero-copy path.
        playbin.set_property_from_str(
            "flags",
            if config.muted {
                "video+deinterlace"
            } else {
                "video+audio+deinterlace"
            },
        );
        if !config.muted
            && let Some(sink) = audio_sink()
        {
            playbin.set_property("audio-sink", &sink);
        }
        // A primary's pipelines run on the clock its reflections measure, not
        // on whatever playbin would pick (the audio sink's, when unmuted).
        if sync.mode == Mode::Lead
            && let Some(pipeline) = playbin.downcast_ref::<gst::Pipeline>()
        {
            pipeline.use_clock(Some(&clock::current()));
        }

        // The next clip, when it has to wait for this one's EOS rather than
        // being queued gaplessly.
        let after_eos: Arc<Mutex<Option<usize>>> = Arc::new(Mutex::new(None));
        {
            let uris = uris.clone();
            let playlist = playlist.clone();
            let queued = queued.clone();
            let current = current.clone();
            let after_eos = after_eos.clone();
            let sync = sync.clone();
            playbin.connect("about-to-finish", false, move |values| {
                let playbin = values[0].get::<gst::Element>().ok()?;
                let next = pick_next(&sync, &playlist);
                log::debug!("video: about-to-finish, next {next:?}");
                let next = next?;
                if next == current.load(Ordering::Relaxed) {
                    let uri = uris.lock().unwrap()[next].clone()?;
                    queued.store(next, Ordering::Relaxed);
                    playbin.set_property("uri", &uri);
                } else {
                    *after_eos.lock().unwrap() = Some(next);
                }
                None
            });
        }

        let stop = Arc::new(AtomicBool::new(false));
        let holding = sync.mode == Mode::Lead && config.wait;
        if holding {
            // Pick the next clip now, so the pending cue can name it.
            *sync.planned.lock().unwrap() = playlist.lock().unwrap().advance();
        }
        let watcher = BusWatcher {
            pipeline: playbin.downgrade(),
            shared: shared.clone(),
            stop: stop.clone(),
            uris: uris.clone(),
            playlist: playlist.clone(),
            queued,
            current,
            after_eos,
            produced_frame,
            lease,
            sync,
            seen_cue: 0,
            aligned: 0,
            patience: 0,
            prepared: None,
            holding: holding.then_some(first),
            started: std::time::Instant::now(),
        };
        let bus = playbin.bus().ok_or("pipeline has no bus")?;
        thread::Builder::new()
            .name("video-bus".into())
            .spawn(move || watcher.run(bus))
            .map_err(|e| format!("cannot start the bus thread: {e}"))?;

        // Holding for reflections: preroll to the first frame and wait there;
        // the bus watcher starts it once they are ready.
        let target = if holding {
            gst::State::Paused
        } else {
            gst::State::Playing
        };
        playbin
            .set_state(target)
            .map_err(|e| format!("cannot start playback: {e}"))?;

        Ok(Self {
            pipeline: playbin,
            shared,
            stop,
            wake,
            uris,
            playlist,
        })
    }

    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// Fires whenever there is something new to paint.
    pub fn wake(&self) -> async_channel::Receiver<()> {
        self.wake.clone()
    }

    /// Take a newer view of which clips are here, without interrupting the one
    /// on screen: an arrival joins the rotation at the next pick, and a clip
    /// that went away is passed over from then on.
    pub fn update_items(&self, items: &[Option<PathBuf>]) {
        let fresh = to_uris(items);
        let mut uris = self.uris.lock().unwrap();
        let mut playlist = self.playlist.lock().unwrap();
        for (index, uri) in fresh.into_iter().enumerate().take(uris.len()) {
            playlist.set_available(index, uri.is_some());
            uris[index] = uri;
        }
    }

    /// Tear the pipeline down. Idempotent; also runs on drop.
    pub fn stop(&self) {
        if self.stop.swap(true, Ordering::Relaxed) {
            return;
        }
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The clip to queue after this one. A locked follower takes the primary's
/// pick when it has it. Without one — the cue naming it can land a moment
/// after this is asked — it guesses from its own rotation and stays locked:
/// for a clip looping onto itself the guess is right, and the cue for the
/// clip the primary did start puts a wrong guess right.
fn pick_next(sync: &Sync, playlist: &Mutex<Playlist>) -> Option<usize> {
    if sync.mode == Mode::Lead
        && let Some(planned) = sync.planned.lock().unwrap().take()
    {
        return Some(planned);
    }
    if sync.mode == Mode::Follow
        && sync.locked.load(Ordering::Relaxed)
        && let Some(next) = sync.next.lock().unwrap().take()
    {
        return Some(next);
    }
    playlist.lock().unwrap().advance()
}

/// Clock time at which position 0 of the sample's clip is on screen:
/// `base + running(pts) - position(pts)`. No latency term: GStreamer only
/// configures one for live sources, and a file plays with none.
fn pos0_of(sink: &gst_app::AppSink, sample: &gst::Sample) -> Option<u64> {
    let base = sink.base_time()?;
    let pts = sample.buffer()?.pts()?;
    let segment = sample.segment()?.downcast_ref::<gst::ClockTime>()?;
    let running = segment.to_running_time(pts)?;
    let position = segment.to_stream_time(pts)?;
    (base + running).nseconds().checked_sub(position.nseconds())
}

/// The appsink branch for `kind`, with its frame callback installed.
fn video_sink(
    kind: &FrameKind,
    shared: &Arc<Shared>,
    playlist: &Arc<Mutex<Playlist>>,
    queued: &Arc<AtomicUsize>,
    produced_frame: &Arc<AtomicBool>,
    sync: &Arc<Sync>,
) -> Result<gst::Element, String> {
    let description = match kind {
        // vapostproc converts to packed RGB on the GPU's video engine and
        // exports it as a DMA-BUF the painter imports without a copy.
        FrameKind::Dmabuf(drm_format) => format!(
            "vapostproc ! video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format={drm_format} \
             ! appsink name=sink max-buffers=2 drop=true"
        ),
        FrameKind::Raster => "videoconvert ! video/x-raw,format=RGBA \
             ! appsink name=sink max-buffers=2 drop=true"
            .to_string(),
    };
    let bin = gst::parse::bin_from_description(&description, true)
        .map_err(|e| format!("video sink: {e}"))?;
    let appsink = bin
        .by_name("sink")
        .and_downcast::<gst_app::AppSink>()
        .ok_or("video sink has no appsink")?;

    let shared = shared.clone();
    let playlist = playlist.clone();
    let queued = queued.clone();
    let produced_frame = produced_frame.clone();
    let sync = sync.clone();
    // The segment the last clip start was seen in, with the restart count. A
    // new clip, a loop of the same clip and a seek each bring a new segment,
    // and a restart a new count. Not keyed on the queued clip: the next clip
    // is queued while the old one's last frames are still arriving, so the
    // queued clip only names a segment's clip at the moment that segment
    // begins.
    let mut last_start: Option<(usize, Option<gst::ClockTime>, Option<gst::ClockTime>)> = None;
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            // vapostproc refuses to export DMA-BUFs unless downstream takes
            // VideoMeta, which carries the plane offsets and strides. The
            // raster path reads strides from it too.
            .propose_allocation(|_, query| {
                query.add_allocation_meta::<gst_video::VideoMeta>(None);
                true
            })
            // A paused pipeline's first frame — a primary holding for its
            // reflections, a reflection with the next clip loaded — is on
            // screen while it waits.
            .new_preroll({
                let shared = shared.clone();
                move |sink| {
                    if let Ok(sample) = sink.pull_preroll() {
                        *shared.frame.lock().unwrap() = Some(sample);
                        let _ = shared.wake.try_send(());
                    }
                    Ok(gst::FlowSuccess::Ok)
                }
            })
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if !produced_frame.swap(true, Ordering::Relaxed) {
                    playlist.lock().unwrap().played();
                    let mut status = shared.status.lock().unwrap();
                    if *status != Status::Playing {
                        *status = Status::Playing;
                    }
                }

                let key = sample
                    .segment()
                    .and_then(|s| s.downcast_ref::<gst::ClockTime>().cloned())
                    .map(|s| (sync.restarts.load(Ordering::Relaxed), s.base(), s.start()));
                if key.is_some() && key != last_start {
                    last_start = key;
                    let item = queued.load(Ordering::Relaxed);
                    let pos0 = pos0_of(sink, &sample);
                    *sync.pos0.lock().unwrap() = pos0;
                    if sync.mode == Mode::Lead
                        && let Some(pos0) = pos0
                    {
                        // Already picked while holding for reflections, or
                        // picked now.
                        let next = {
                            let mut planned = sync.planned.lock().unwrap();
                            if planned.is_none() {
                                *planned = playlist.lock().unwrap().advance();
                            }
                            *planned
                        };
                        let cue = Cue {
                            node: sync.node,
                            current: sync.paths[item].clone(),
                            pos0,
                            next: next.map(|n| sync.paths[n].clone()),
                            pending: false,
                        };
                        *sync.published.lock().unwrap() = Some(cue.clone());
                        sync::publish(cue);
                    }
                }

                *shared.frame.lock().unwrap() = Some(sample);
                let _ = shared.wake.try_send(());
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    Ok(bin.upcast())
}

/// A PulseAudio stream named for the app, so the host can find "the video"
/// in their mixer and turn it down without touching the music.
fn audio_sink() -> Option<gst::Element> {
    // No `media.name`: pulsesink names the stream itself and overrides it.
    let props = gst::Structure::builder("props")
        .field("media.role", "video")
        .field("application.name", "Guest Info Display")
        .build();
    match gst::ElementFactory::make("pulsesink")
        .property("client-name", "Guest Info Display")
        .property("stream-properties", &props)
        .build()
    {
        Ok(sink) => Some(sink),
        Err(e) => {
            log::warn!("video: no pulsesink ({e}); audio goes to the default sink unnamed");
            None
        }
    }
}

/// Follows the pipeline's bus on its own thread: errors skip to the next clip,
/// end-of-stream finishes the playlist, the inhibit lease tracks whether
/// anything is actually playing, and — on a reflection — cues are acted on.
struct BusWatcher {
    pipeline: gst::glib::WeakRef<gst::Element>,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    uris: Uris,
    playlist: Arc<Mutex<Playlist>>,
    queued: Arc<AtomicUsize>,
    current: Arc<AtomicUsize>,
    after_eos: Arc<Mutex<Option<usize>>>,
    produced_frame: Arc<AtomicBool>,
    lease: VideoLease,
    sync: Arc<Sync>,
    /// Arrival counter of the last cue acted on.
    seen_cue: u64,
    /// `pos0` of the clip start we last lined up with. A cue repeating it only
    /// adds the clip after; an older one is stale.
    aligned: u64,
    /// Passes a large drift has been given to resolve on its own: at a clip
    /// boundary, our first frame of the new clip may be a moment behind the
    /// cue for it.
    patience: u32,
    /// Follow: a clip loaded and paused on its first frame, waiting for the
    /// primary's cue to say when to play it.
    prepared: Option<usize>,
    /// Lead: the first clip, held on its first frame until every subscribed
    /// reflection has it loaded.
    holding: Option<usize>,
    /// When this player started, for [`ARRIVAL_GRACE`].
    started: std::time::Instant,
}

impl BusWatcher {
    /// Item `index` as a human would know it.
    fn describe(&self, index: usize) -> String {
        match &self.uris.lock().unwrap()[index] {
            Some(uri) => gst::glib::filename_from_uri(uri)
                .map(|(path, _)| path.display().to_string())
                .unwrap_or_else(|_| uri.clone()),
            None => format!("clip {index}"),
        }
    }

    fn run(mut self, bus: gst::Bus) {
        // Polled rather than blocking forever: a pipeline set to NULL flushes
        // its bus, so a shutdown message could never be relied on to arrive.
        // The timeout also paces how soon a follower sees a new cue.
        while !self.stop.load(Ordering::Relaxed) {
            let msg = bus.timed_pop(gst::ClockTime::from_mseconds(100));
            let Some(pipeline) = self.pipeline.upgrade() else {
                break;
            };
            if self.sync.mode == Mode::Follow {
                self.follow(&pipeline);
            }
            if let Some(first) = self.holding {
                self.hold(&pipeline, first);
            }
            let Some(msg) = msg else {
                continue;
            };
            match msg.view() {
                gst::MessageView::StreamStart(_) => {
                    let item = self.queued.load(Ordering::Relaxed);
                    self.current.store(item, Ordering::Relaxed);
                    self.produced_frame.store(false, Ordering::Relaxed);
                    log::info!("video: playing {}", self.describe(item));
                }
                gst::MessageView::StateChanged(change)
                    if change.src() == Some(pipeline.upcast_ref()) =>
                {
                    self.lease.set(change.current() == gst::State::Playing);
                }
                gst::MessageView::Eos(_) => {
                    let next = self.after_eos.lock().unwrap().take();
                    match next {
                        // A locked follower never starts a clip on its own
                        // clock: it loads the primary's next one, paused on its
                        // first frame, and plays it when the cue says when.
                        _ if self.sync.locked.load(Ordering::Relaxed) => {
                            let planned = next.or_else(|| self.sync.next.lock().unwrap().take());
                            if let Some(next) = planned {
                                self.prepare(&pipeline, next);
                            }
                        }
                        Some(next) => self.restart_on(&pipeline, next),
                        None => {
                            self.lease.set(false);
                            self.shared.set_status(Status::Finished);
                        }
                    }
                }
                gst::MessageView::Error(err) => {
                    let file = self.describe(self.current.load(Ordering::Relaxed));
                    log::warn!(
                        "video: {file}: {} ({})",
                        err.error(),
                        err.debug().unwrap_or_default()
                    );
                    self.sync.locked.store(false, Ordering::Relaxed);
                    self.skip_past_failure(&pipeline, &file, &err.error().to_string());
                }
                _ => {}
            }
        }
    }

    /// Lead: keep holding the first clip until no subscribed reflection is
    /// still getting it ready, then schedule the start for everyone.
    ///
    /// The cue naming the start goes out *before* the start, half a second
    /// ahead, so a reflection with the clip already loaded starts on the same
    /// instant rather than catching up after it.
    fn hold(&mut self, pipeline: &gst::Element, first: usize) {
        let path = &self.sync.paths[first];
        let next = *self.sync.planned.lock().unwrap();
        let cue = |pending: bool, pos0: u64| Cue {
            node: self.sync.node,
            current: path.clone(),
            pos0,
            next: next.map(|n| self.sync.paths[n].clone()),
            pending,
        };

        let (mut screens, progress) = sync::waiting_on(self.sync.node, path);
        if self.started.elapsed() < ARRIVAL_GRACE {
            screens += sync::absent();
        }
        if screens > 0 {
            let waiting = Status::Waiting { screens, progress };
            if *self.shared.status.lock().unwrap() != waiting {
                // Re-announced on each change, so a reflection that subscribes
                // mid-wait hears about it from the cue cache.
                sync::publish(cue(true, 0));
                self.shared.set_status(waiting);
            }
            return;
        }

        self.holding = None;
        let start = clock::now() + START_LEAD_NS;
        sync::publish(cue(false, start));
        // Position 0 of the prerolled clip is running time 0: put it at `start`.
        pipeline.set_start_time(gst::ClockTime::NONE);
        pipeline.set_base_time(gst::ClockTime::from_nseconds(start));
        log::info!(
            "video: no reflection is still getting ready, starting {}",
            self.describe(first)
        );
        let _ = pipeline.set_state(gst::State::Playing);
    }

    /// Follow: get a pending clip ready and say so, or say how far its
    /// download has got. Asked again every pass until it is ready, which is
    /// what keeps the primary's progress caption moving.
    fn ready_for(&mut self, pipeline: &gst::Element, generation: u64, cue: &Cue) {
        let uris = self.uris.clone();
        let available = move |index: usize| uris.lock().unwrap()[index].is_some();
        let index = self.sync.index_of(&cue.current).filter(|i| available(*i));
        if let Some(index) = index
            && self.prepared != Some(index)
        {
            self.prepare(pipeline, index);
        }
        let ready = index.is_some() && self.prepared == index;
        let progress = if ready {
            None
        } else {
            sync::progress(&cue.current)
        };
        sync::report(
            self.sync.node,
            Readiness {
                path: cue.current.clone(),
                ready,
                progress,
            },
        );
        let joining = Status::Joining { ready, progress };
        if *self.shared.status.lock().unwrap() != joining {
            self.shared.set_status(joining);
        }
        if ready {
            self.seen_cue = generation;
            *self.sync.next.lock().unwrap() = cue
                .next
                .as_deref()
                .and_then(|path| self.sync.index_of(path))
                .filter(|i| available(*i));
        }
    }

    /// Act on the newest cue for this node, once.
    fn follow(&mut self, pipeline: &gst::Element) {
        let Some((generation, cue)) = sync::latest(self.sync.node) else {
            return;
        };
        if generation == self.seen_cue {
            return;
        }
        // No clock to line up against yet: try again on a later pass.
        if !clock::synced() {
            return;
        }
        if cue.pending {
            self.ready_for(pipeline, generation, &cue);
            return;
        }
        // A clip start older than one we already lined up with: stale.
        if cue.pos0 < self.aligned {
            self.seen_cue = generation;
            return;
        }

        let available = |index: usize| self.uris.lock().unwrap()[index].is_some();
        *self.sync.next.lock().unwrap() = cue
            .next
            .as_deref()
            .and_then(|path| self.sync.index_of(path))
            .filter(|i| available(*i));
        // The same clip start again, now naming the clip after: noted above.
        if cue.pos0 == self.aligned {
            self.seen_cue = generation;
            return;
        }

        let Some(index) = self.sync.index_of(&cue.current).filter(|i| available(*i)) else {
            // Not here yet: keep playing what we have, and look again on the
            // next pass — the file may land while this clip is still on.
            return;
        };

        // Already loaded and waiting at position 0: play it on the cue.
        if self.prepared == Some(index) {
            self.done_with(generation, cue.pos0);
            self.play_prepared(pipeline, cue.pos0, index);
            return;
        }

        let playing = self.queued.load(Ordering::Relaxed) == index;
        let measured = *self.sync.pos0.lock().unwrap();
        if let (true, true, Some(pos0)) =
            (self.sync.locked.load(Ordering::Relaxed), playing, measured)
        {
            let drift = pos0 as i64 - cue.pos0 as i64;
            if drift.abs() <= TOLERANCE_NS {
                self.done_with(generation, cue.pos0);
                return;
            }
            if drift.abs() < NUDGE_LIMIT_NS {
                log::debug!(
                    "video: {:.1} ms off the primary, nudging",
                    drift as f64 / 1e6
                );
                shift_base_time(pipeline, drift);
                *self.sync.pos0.lock().unwrap() = Some(cue.pos0);
                self.done_with(generation, cue.pos0);
                return;
            }
            // Probably our first frame of this clip is still on its way.
            if self.patience < 10 {
                self.patience += 1;
                return;
            }
        }
        self.done_with(generation, cue.pos0);
        self.lock_on(pipeline, index, cue.pos0);
    }

    fn done_with(&mut self, generation: u64, pos0: u64) {
        self.seen_cue = generation;
        self.aligned = pos0;
        self.patience = 0;
    }

    /// Load clip `index` paused on its first frame, on the shared clock, with
    /// the base time ours to set. `None` if there is nothing to load.
    fn load(&mut self, pipeline: &gst::Element, index: usize) -> Option<gst::Clock> {
        if self.stop.load(Ordering::Relaxed) {
            return None;
        }
        let uri = self.uris.lock().unwrap()[index].clone()?;
        let bin = pipeline.downcast_ref::<gst::Pipeline>()?;
        let clock = clock::current();
        self.after_eos.lock().unwrap().take();
        self.prepared = None;
        // NULL, not READY: in READY the VA postprocessor keeps its display,
        // the new clip's decoder opens another, and every frame then fails
        // with an invalid surface — so the preroll below would never finish.
        let _ = pipeline.set_state(gst::State::Null);
        self.sync.restarts.fetch_add(1, Ordering::Relaxed);
        self.queued.store(index, Ordering::Relaxed);
        self.current.store(index, Ordering::Relaxed);
        pipeline.set_property("uri", &uri);
        bin.use_clock(Some(&clock));
        // Ours to choose, not the pipeline's: it must not pick a fresh base
        // time when it goes to PLAYING.
        pipeline.set_start_time(gst::ClockTime::NONE);
        let _ = pipeline.set_state(gst::State::Paused);
        let _ = pipeline.state(gst::ClockTime::from_seconds(5));
        Some(clock)
    }

    /// Get the primary's next clip ready, so its cue can start it at once.
    fn prepare(&mut self, pipeline: &gst::Element, index: usize) {
        if self.load(pipeline, index).is_some() {
            self.prepared = Some(index);
        }
    }

    /// Play the prepared clip so its position 0 falls at primary time `pos0`.
    /// Usually that is a moment ago — the cue is sent as the primary shows the
    /// frame — and the few frames already due are dropped to catch up. Much
    /// later than that, seeking is quicker.
    fn play_prepared(&mut self, pipeline: &gst::Element, pos0: u64, index: usize) {
        self.prepared = None;
        let behind = clock::now().saturating_sub(pos0);
        if behind > LOCK_LEAD_NS {
            self.lock_on(pipeline, index, pos0);
            return;
        }
        pipeline.set_base_time(gst::ClockTime::from_nseconds(pos0));
        self.sync.locked.store(true, Ordering::Relaxed);
        log::info!(
            "video: started {} with the primary ({:.0} ms behind)",
            self.describe(index),
            behind as f64 / 1e6
        );
        let _ = pipeline.set_state(gst::State::Playing);
    }

    /// Load clip `index` on the shared clock so its position 0 falls at
    /// primary time `pos0`: aim a little ahead of now, seek there, and pick
    /// the base time that puts that position on screen at the right moment.
    fn lock_on(&mut self, pipeline: &gst::Element, index: usize, pos0: u64) {
        let t = std::time::Instant::now();
        let Some(clock) = self.load(pipeline, index) else {
            return;
        };
        log::debug!("video: loading took {:?}", t.elapsed());
        let target = clock.time().nseconds() + LOCK_LEAD_NS;
        let position = target.saturating_sub(pos0);
        // A cue placing us past the clip's end is stale — the primary will
        // have moved on. Play from the top and wait for a fresher one.
        if let Some(duration) = pipeline.query_duration::<gst::ClockTime>()
            && position >= duration.nseconds()
        {
            log::debug!(
                "video: cue is {:.1}s past the clip's end, ignoring it",
                (position - duration.nseconds()) as f64 / 1e9
            );
            pipeline.set_start_time(gst::ClockTime::ZERO);
            self.sync.locked.store(false, Ordering::Relaxed);
            let _ = pipeline.set_state(gst::State::Playing);
            return;
        }
        if position > 0 {
            let t = std::time::Instant::now();
            let _ = pipeline.seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                gst::ClockTime::from_nseconds(position),
            );
            let _ = pipeline.state(gst::ClockTime::from_seconds(5));
            log::debug!("video: seek took {:?}", t.elapsed());
        }
        // After a flushing seek, running time 0 is at `position`.
        pipeline.set_base_time(gst::ClockTime::from_nseconds(pos0 + position));
        self.sync.locked.store(true, Ordering::Relaxed);
        let late = clock.time().nseconds() as i64 - target as i64;
        log::info!(
            "video: locked onto the primary at {:.2}s into {} ({})",
            position as f64 / 1e9,
            self.describe(index),
            if late > 0 {
                format!("{:.0} ms late, dropping to catch up", late as f64 / 1e6)
            } else {
                format!("{:.0} ms to spare", -late as f64 / 1e6)
            }
        );
        let _ = pipeline.set_state(gst::State::Playing);
    }

    /// An errored pipeline stops, so restarting on the next clip is ours to
    /// do. Gives up once every clip has failed in a row.
    fn skip_past_failure(&self, pipeline: &gst::Element, file: &str, why: &str) {
        let next = {
            let mut playlist = self.playlist.lock().unwrap();
            if playlist.failed() {
                playlist.advance()
            } else {
                None
            }
        };
        let Some(next) = next else {
            self.lease.set(false);
            let _ = pipeline.set_state(gst::State::Null);
            self.shared
                .set_status(Status::Failed(format!("{file}: {why}")));
            return;
        };
        // A beat before retrying, so a playlist of broken files on a loop
        // cannot spin the CPU.
        thread::sleep(Duration::from_millis(250));
        self.restart_on(pipeline, next);
    }

    /// Stop, point the pipeline at item `next`, and play it from the top.
    fn restart_on(&self, pipeline: &gst::Element, next: usize) {
        if self.stop.load(Ordering::Relaxed) {
            return;
        }
        // An error can leave a clip queued for after an EOS that never comes.
        self.after_eos.lock().unwrap().take();
        let Some(uri) = self.uris.lock().unwrap()[next].clone() else {
            return;
        };
        // NULL for the same reason as in `lock_on`.
        let _ = pipeline.set_state(gst::State::Null);
        self.sync.restarts.fetch_add(1, Ordering::Relaxed);
        self.queued.store(next, Ordering::Relaxed);
        self.current.store(next, Ordering::Relaxed);
        pipeline.set_property("uri", &uri);
        // Hand base-time selection back to the pipeline: a follower that was
        // locked fixed it, and playing on from a stale one would drop every
        // frame as late.
        pipeline.set_start_time(gst::ClockTime::ZERO);
        let _ = pipeline.set_state(gst::State::Playing);
    }
}

/// Move a playing pipeline's base time by `drift`, on every element: a bin
/// does not hand a new base time to its children until its next state change,
/// and the sinks are what wait on it. Frames that are now late get dropped;
/// early ones wait.
fn shift_base_time(pipeline: &gst::Element, drift: i64) {
    let Some(base) = pipeline.base_time() else {
        return;
    };
    let shifted = gst::ClockTime::from_nseconds((base.nseconds() as i64 - drift).max(0) as u64);
    pipeline.set_base_time(shifted);
    if let Some(bin) = pipeline.downcast_ref::<gst::Bin>() {
        for element in bin.iterate_recurse().into_iter().flatten() {
            element.set_base_time(shifted);
        }
    }
}
