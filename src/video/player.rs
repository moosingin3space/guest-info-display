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
    pub items: Vec<PathBuf>,
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

pub struct Player {
    pipeline: gst::Element,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    wake: async_channel::Receiver<()>,
}

impl Player {
    /// Build the pipeline and start the first clip.
    pub fn start(config: Config, lease: VideoLease) -> Result<Self, String> {
        if config.items.is_empty() {
            return Err("nothing to play".to_string());
        }
        let kind = frame_kind();
        let (wake_tx, wake) = async_channel::bounded(1);
        let shared = Arc::new(Shared {
            frame: Mutex::new(None),
            status: Mutex::new(Status::Starting),
            kind: kind.clone(),
            wake: wake_tx,
        });

        let uris: Arc<Vec<String>> = Arc::new(
            config
                .items
                .iter()
                .map(|p| {
                    gst::glib::filename_to_uri(p, None)
                        .map(|u| u.to_string())
                        .map_err(|e| format!("{}: {e}", p.display()))
                })
                .collect::<Result<_, _>>()?,
        );
        let playlist = Arc::new(Mutex::new(Playlist::new(
            uris.len(),
            config.looping,
            config.shuffle,
        )));
        let first = playlist.lock().unwrap().advance().expect("non-empty");
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
            .property("uri", &uris[first])
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
                    queued.store(next, Ordering::Relaxed);
                    playbin.set_property("uri", &uris[next]);
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
            uris,
            config_items: config.items.clone(),
            playlist,
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
        })
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
    uris: Arc<Vec<String>>,
    config_items: Vec<PathBuf>,
    playlist: Arc<Mutex<Playlist>>,
    queued: Arc<AtomicUsize>,
    current: Arc<AtomicUsize>,
    after_eos: Arc<Mutex<Option<usize>>>,
    produced_frame: Arc<AtomicBool>,
    lease: VideoLease,
}

impl BusWatcher {
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
                    log::info!("video: playing {}", self.config_items[item].display());
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
                    let item = self.current.load(Ordering::Relaxed);
                    let file = self.config_items[item].display().to_string();
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
        let _ = pipeline.set_state(gst::State::Ready);
        self.queued.store(next, Ordering::Relaxed);
        self.current.store(next, Ordering::Relaxed);
        pipeline.set_property("uri", &self.uris[next]);
        let _ = pipeline.set_state(gst::State::Playing);
    }
}
