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
//! Gapless needs the next clip to carry the same kinds of stream as the one
//! ending. With audio on, a cut from a clip with a soundtrack to one without
//! leaves playbin3 waiting forever for audio that never comes, and a night of
//! mixed clips will contain exactly that. So an unmuted playlist cuts between
//! *different* files the slow way — let the clip end, then restart on the next
//! — and keeps gapless for the safe cases: muted, or one file looping onto
//! itself.

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
use super::{FrameKind, frame_kind};
use crate::inhibitor::VideoLease;

/// What a `video` node asked to play, resolved to files on this device.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Config {
    /// One entry per clip the layout names, in document order: the file, or
    /// `None` while it is missing — on a reflection, still on its way.
    pub items: Vec<Option<PathBuf>>,
    pub muted: bool,
    pub looping: bool,
    pub shuffle: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Opening the first clip; nothing on screen yet.
    Starting,
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
        // Which item is decoding, for naming the file in an error. Moves on
        // STREAM_START, not at `about-to-finish`, because the old clip is
        // still playing out when the next one is queued.
        let queued = Arc::new(AtomicUsize::new(first));
        let current = Arc::new(AtomicUsize::new(first));
        // Cleared at each STREAM_START; the first frame after proves the clip
        // decodes, which is what lets a broken file count as a failure.
        let produced_frame = Arc::new(AtomicBool::new(false));

        let video_sink = video_sink(&kind, &shared, &playlist, &produced_frame)?;
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

        // The next clip, when it has to wait for this one's EOS rather than
        // being queued gaplessly.
        let after_eos: Arc<Mutex<Option<usize>>> = Arc::new(Mutex::new(None));
        {
            let uris = uris.clone();
            let playlist = playlist.clone();
            let queued = queued.clone();
            let current = current.clone();
            let after_eos = after_eos.clone();
            let muted = config.muted;
            playbin.connect("about-to-finish", false, move |values| {
                let playbin = values[0].get::<gst::Element>().ok()?;
                let next = playlist.lock().unwrap().advance()?;
                if muted || next == current.load(Ordering::Relaxed) {
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
        };
        let bus = playbin.bus().ok_or("pipeline has no bus")?;
        thread::Builder::new()
            .name("video-bus".into())
            .spawn(move || watcher.run(bus))
            .map_err(|e| format!("cannot start the bus thread: {e}"))?;

        playbin
            .set_state(gst::State::Playing)
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

    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// Fires whenever there is something new to paint.
    pub fn wake(&self) -> async_channel::Receiver<()> {
        self.wake.clone()
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

/// The appsink branch for `kind`, with its frame callback installed.
fn video_sink(
    kind: &FrameKind,
    shared: &Arc<Shared>,
    playlist: &Arc<Mutex<Playlist>>,
    produced_frame: &Arc<AtomicBool>,
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
    let produced_frame = produced_frame.clone();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            // vapostproc refuses to export DMA-BUFs unless downstream takes
            // VideoMeta, which carries the plane offsets and strides. The
            // raster path reads strides from it too.
            .propose_allocation(|_, query| {
                query.add_allocation_meta::<gst_video::VideoMeta>(None);
                true
            })
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                *shared.frame.lock().unwrap() = Some(sample);
                if !produced_frame.swap(true, Ordering::Relaxed) {
                    playlist.lock().unwrap().played();
                    let mut status = shared.status.lock().unwrap();
                    if *status != Status::Playing {
                        *status = Status::Playing;
                    }
                }
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
/// end-of-stream finishes the playlist, and the inhibit lease tracks whether
/// anything is actually playing.
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

    fn run(self, bus: gst::Bus) {
        // Polled rather than blocking forever: a pipeline set to NULL flushes
        // its bus, so a shutdown message could never be relied on to arrive.
        while !self.stop.load(Ordering::Relaxed) {
            let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) else {
                continue;
            };
            let Some(pipeline) = self.pipeline.upgrade() else {
                break;
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
                    self.skip_past_failure(&pipeline, &file, &err.error().to_string());
                }
                _ => {}
            }
        }
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
        let _ = pipeline.set_state(gst::State::Ready);
        self.queued.store(next, Ordering::Relaxed);
        self.current.store(next, Ordering::Relaxed);
        pipeline.set_property("uri", &uri);
        let _ = pipeline.set_state(gst::State::Playing);
    }
}
