//! Spike: hardware video playback that never leaves the GPU.
//!
//! VA-API decodes, `vapostproc` converts to RGBA into a DMA-BUF, EGL imports
//! that DMA-BUF as a GL texture, and Skia borrows the texture inside a Freya
//! canvas. No frame is copied through the CPU.
//!
//! Needs Freya's OpenGL renderer, since the import runs on the EGL context
//! Freya makes current while painting:
//!
//! ```text
//! FREYA_RENDERER=opengl cargo run --example video_spike -- /path/to/video.mp4
//! ```
//!
//! Logs decoded and drawn frames per second; the clip loops on EOS.

use std::cell::RefCell;
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use freya::prelude::*;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_allocators as gst_allocators;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use khronos_egl as egl;
use skia_safe::gpu::{self, SurfaceOrigin};

// EGL_EXT_image_dma_buf_import(_modifiers); khronos-egl doesn't define them.
const LINUX_DMA_BUF_EXT: egl::Enum = 0x3270;
const LINUX_DRM_FOURCC_EXT: egl::Attrib = 0x3271;
const DMA_BUF_PLANE0_FD_EXT: egl::Attrib = 0x3272;
const DMA_BUF_PLANE0_OFFSET_EXT: egl::Attrib = 0x3273;
const DMA_BUF_PLANE0_PITCH_EXT: egl::Attrib = 0x3274;
const DMA_BUF_PLANE0_MODIFIER_LO_EXT: egl::Attrib = 0x3443;
const DMA_BUF_PLANE0_MODIFIER_HI_EXT: egl::Attrib = 0x3444;

/// Packed RGB DRM fourccs in preference order. GL samples an imported
/// EGLImage in RGBA order whatever the memory layout, so all map to RGBA8888.
const RGB_FOURCCS: [&str; 4] = ["XB24", "AB24", "XR24", "AR24"];

type EglImageTargetTexture2DOes = unsafe extern "system" fn(target: u32, image: *const c_void);

/// Latest decoded sample, handed from GStreamer's streaming thread to paint.
#[derive(Default)]
struct Shared {
    latest: Mutex<Option<gst::Sample>>,
    decoded: std::sync::atomic::AtomicU64,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();
static WAKE: OnceLock<async_channel::Receiver<()>> = OnceLock::new();

fn main() {
    pretty_env_logger::init();

    if std::env::var("FREYA_RENDERER").as_deref() != Ok("opengl") {
        eprintln!("video_spike: set FREYA_RENDERER=opengl; the import needs Freya's EGL context");
        std::process::exit(2);
    }
    let path = std::env::args()
        .nth(1)
        .expect("usage: video_spike /path/to/video");
    let path = std::fs::canonicalize(&path).expect("video path must exist");

    gst::init().expect("GStreamer init failed");
    let shared = Arc::new(Shared::default());
    let (wake_tx, wake_rx) = async_channel::bounded(1);
    let pipeline = build_pipeline(&path, shared.clone(), wake_tx);
    SHARED.set(shared).ok();
    WAKE.set(wake_rx).ok();

    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline failed to start");
    watch_bus(pipeline.clone());

    launch(
        LaunchConfig::new().with_window(
            WindowConfig::new(app)
                .with_title("Video spike")
                .with_size(1280., 720.)
                .with_background((0, 0, 0)),
        ),
    );

    let _ = pipeline.set_state(gst::State::Null);
}

fn build_pipeline(
    path: &std::path::Path,
    shared: Arc<Shared>,
    wake: async_channel::Sender<()>,
) -> gst::Element {
    let drm_format = pick_drm_format();
    log::info!("requesting DMA-BUF drm-format {drm_format}");
    let sink_bin = gst::parse::bin_from_description(
        &format!(
            "vapostproc ! video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format={drm_format} \
             ! appsink name=sink max-buffers=2 drop=true"
        ),
        true,
    )
    .expect("video sink bin");
    let appsink = sink_bin
        .by_name("sink")
        .and_downcast::<gst_app::AppSink>()
        .expect("appsink");

    let mut logged_caps = false;
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            // vapostproc refuses to export DMA-BUFs unless downstream takes
            // VideoMeta, which carries the plane offsets and strides.
            .propose_allocation(|_, query| {
                query.add_allocation_meta::<gst_video::VideoMeta>(None);
                true
            })
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if !logged_caps {
                    logged_caps = true;
                    log::info!("negotiated caps: {:?}", sample.caps());
                }
                *shared.latest.lock().unwrap() = Some(sample);
                shared
                    .decoded
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let _ = wake.try_send(());
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    let uri = format!("file://{}", path.display());
    gst::ElementFactory::make("playbin3")
        .property("uri", uri)
        .property("video-sink", &sink_bin)
        .build()
        .expect("playbin3")
}

/// The first packed-RGB `fourcc:modifier` vapostproc can output as a DMA-BUF.
///
/// The modifier is the driver's tiling layout (AMD offers only tiled RGB), so
/// it can't be hard-coded; the pad template lists what this GPU produces.
fn pick_drm_format() -> String {
    let factory = gst::ElementFactory::find("vapostproc").expect("no vapostproc: VA-API missing");
    let caps = factory
        .static_pad_templates()
        .into_iter()
        .find(|t| t.direction() == gst::PadDirection::Src)
        .expect("vapostproc src template")
        .caps();
    let offered: Vec<String> = caps
        .iter_with_features()
        .filter(|(_, features)| features.contains("memory:DMABuf"))
        .flat_map(|(s, _)| match s.value("drm-format") {
            Ok(v) => match v.get::<gst::List>() {
                Ok(list) => list.iter().filter_map(|v| v.get::<String>().ok()).collect(),
                Err(_) => v.get::<String>().into_iter().collect(),
            },
            Err(_) => Vec::new(),
        })
        .collect();
    RGB_FOURCCS
        .iter()
        .find_map(|fourcc| {
            offered
                .iter()
                .find(|f| f.split(':').next() == Some(fourcc))
                .cloned()
        })
        .unwrap_or_else(|| panic!("vapostproc offers no packed RGB DMA-BUF: {offered:?}"))
}

/// Loops the clip on EOS and logs pipeline errors.
fn watch_bus(pipeline: gst::Element) {
    let bus = pipeline.bus().expect("pipeline bus");
    std::thread::spawn(move || {
        for msg in bus.iter_timed(gst::ClockTime::NONE) {
            match msg.view() {
                gst::MessageView::Eos(_) => {
                    let _ = pipeline.seek_simple(
                        gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                        gst::ClockTime::ZERO,
                    );
                }
                gst::MessageView::Error(err) => {
                    log::error!(
                        "{}: {} ({:?})",
                        err.src().map(|s| s.path_string()).unwrap_or_default(),
                        err.error(),
                        err.debug()
                    );
                }
                _ => {}
            }
        }
    });
}

fn app() -> impl IntoElement {
    let mut tick = use_state(|| 0u64);
    use_hook(move || {
        let wake = WAKE.get().expect("wake channel").clone();
        spawn(async move {
            while wake.recv().await.is_ok() {
                *tick.write() += 1;
            }
        });
    });

    let importer = use_hook(|| Rc::new(RefCell::new(Importer::default())));

    // The canvas only repaints when it changes, so a new key per frame forces it.
    rect().expanded().child(
        canvas(RenderCallback::new(move |ctx: &mut CanvasContext| {
            importer.borrow_mut().paint(ctx);
        }))
        .key(*tick.read())
        .expanded(),
    )
}

/// The frame on screen; its GL and EGL objects live until the next frame
/// replaces it, by which point Skia has flushed the draw that used them.
struct Frame {
    sample: gst::Sample,
    image: skia_safe::Image,
    texture: u32,
    egl_image: egl::Image,
}

#[derive(Default)]
struct Importer {
    gl: Option<GlFns>,
    frame: Option<Frame>,
    stats_since: Option<Instant>,
    drawn: u64,
    paints: u64,
    import_time: Duration,
    last_decoded: u64,
}

struct GlFns {
    egl: egl::Instance<egl::Static>,
    display: egl::Display,
    image_target_texture: EglImageTargetTexture2DOes,
}

impl GlFns {
    fn load() -> Self {
        let egl = egl::Instance::new(egl::Static);
        let display = egl
            .get_current_display()
            .expect("no current EGL display; is FREYA_RENDERER=opengl?");
        gl::load_with(|name| {
            egl.get_proc_address(name)
                .map_or(std::ptr::null(), |f| f as *const c_void)
        });
        let image_target_texture = egl
            .get_proc_address("glEGLImageTargetTexture2DOES")
            .expect("GL_OES_EGL_image missing");
        // SAFETY: the signature is fixed by the GL_OES_EGL_image spec.
        let image_target_texture = unsafe {
            std::mem::transmute::<extern "system" fn(), EglImageTargetTexture2DOes>(
                image_target_texture,
            )
        };
        Self {
            egl,
            display,
            image_target_texture,
        }
    }
}

impl Importer {
    fn paint(&mut self, ctx: &mut CanvasContext) {
        let Some(mut direct) = ctx.canvas.direct_context() else {
            log::error!("canvas has no GPU context");
            return;
        };
        self.paints += 1;
        let fns = self.gl.get_or_insert_with(GlFns::load);

        let latest = SHARED.get().and_then(|s| s.latest.lock().unwrap().clone());
        let is_new = match (&latest, &self.frame) {
            (Some(latest), Some(frame)) => latest.as_ptr() != frame.sample.as_ptr(),
            (Some(_), None) => true,
            _ => false,
        };
        if let (true, Some(sample)) = (is_new, latest) {
            let started = Instant::now();
            match import(fns, &mut direct, sample) {
                Ok(frame) => {
                    if let Some(old) = self.frame.replace(frame) {
                        release(fns, old);
                    }
                }
                Err(e) => log::error!("import failed: {e}"),
            }
            self.import_time += started.elapsed();
            self.drawn += 1;
        }

        if let Some(frame) = &self.frame {
            draw_contain(ctx, &frame.image);
        }
        self.log_stats();
    }

    fn log_stats(&mut self) {
        let since = *self.stats_since.get_or_insert_with(Instant::now);
        let elapsed = since.elapsed();
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let decoded = SHARED
            .get()
            .map_or(0, |s| s.decoded.load(std::sync::atomic::Ordering::Relaxed));
        let secs = elapsed.as_secs_f64();
        log::info!(
            "decoded {:.1} fps, drawn {:.1} fps, painted {:.1} fps, import {:.2} ms/frame",
            (decoded - self.last_decoded) as f64 / secs,
            self.drawn as f64 / secs,
            self.paints as f64 / secs,
            self.import_time.as_secs_f64() * 1000.0 / self.drawn.max(1) as f64,
        );
        self.last_decoded = decoded;
        self.drawn = 0;
        self.paints = 0;
        self.import_time = Duration::ZERO;
        self.stats_since = Some(Instant::now());
    }
}

fn import(
    fns: &GlFns,
    direct: &mut gpu::DirectContext,
    sample: gst::Sample,
) -> Result<Frame, String> {
    let caps = sample.caps().ok_or("sample without caps")?;
    let info = gst_video::VideoInfoDmaDrm::from_caps(caps).map_err(|e| e.to_string())?;
    let buffer = sample.buffer().ok_or("sample without buffer")?;
    if buffer.n_memory() != 1 {
        return Err(format!("expected one memory, got {}", buffer.n_memory()));
    }
    let fd = buffer
        .peek_memory(0)
        .downcast_memory_ref::<gst_allocators::DmaBufMemory>()
        .ok_or("memory is not a DMA-BUF")?
        .fd();
    let (offset, stride) = match buffer.meta::<gst_video::VideoMeta>() {
        Some(meta) => (meta.offset()[0], meta.stride()[0]),
        None => (info.offset()[0], info.stride()[0]),
    };
    let (width, height) = (info.width() as i32, info.height() as i32);
    let modifier = info.modifier();

    let attribs = [
        egl::WIDTH as egl::Attrib,
        width as egl::Attrib,
        egl::HEIGHT as egl::Attrib,
        height as egl::Attrib,
        LINUX_DRM_FOURCC_EXT,
        info.fourcc() as egl::Attrib,
        DMA_BUF_PLANE0_FD_EXT,
        fd as egl::Attrib,
        DMA_BUF_PLANE0_OFFSET_EXT,
        offset as egl::Attrib,
        DMA_BUF_PLANE0_PITCH_EXT,
        stride as egl::Attrib,
        DMA_BUF_PLANE0_MODIFIER_LO_EXT,
        (modifier & 0xffff_ffff) as egl::Attrib,
        DMA_BUF_PLANE0_MODIFIER_HI_EXT,
        (modifier >> 32) as egl::Attrib,
        egl::ATTRIB_NONE,
    ];
    // SAFETY: EGL_LINUX_DMA_BUF_EXT takes no context and a null client buffer.
    let egl_image = fns
        .egl
        .create_image(
            fns.display,
            unsafe { egl::Context::from_ptr(egl::NO_CONTEXT) },
            LINUX_DMA_BUF_EXT,
            unsafe { egl::ClientBuffer::from_ptr(std::ptr::null_mut()) },
            &attribs,
        )
        .map_err(|e| format!("eglCreateImage: {e}"))?;

    let mut texture = 0;
    // SAFETY: Freya's GL context is current for the whole paint callback.
    unsafe {
        gl::GenTextures(1, &mut texture);
        gl::BindTexture(gl::TEXTURE_2D, texture);
        gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
        gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
        (fns.image_target_texture)(gl::TEXTURE_2D, egl_image.as_ptr());
        gl::BindTexture(gl::TEXTURE_2D, 0);
    }
    // Skia caches GL state; tell it ours just changed under it.
    direct.reset(None);

    let texture_info = gpu::gl::TextureInfo {
        target: gl::TEXTURE_2D,
        id: texture,
        format: gl::RGBA8,
        protected: gpu::Protected::No,
    };
    // SAFETY: the texture outlives the image; both are released together.
    let backend = unsafe {
        gpu::backend_textures::make_gl((width, height), gpu::Mipmapped::No, texture_info, "video")
    };
    let image = gpu::images::borrow_texture_from(
        direct,
        &backend,
        SurfaceOrigin::TopLeft,
        skia_safe::ColorType::RGBA8888,
        skia_safe::AlphaType::Opaque,
        None,
    )
    .ok_or("Skia refused the texture")?;

    Ok(Frame {
        sample,
        image,
        texture,
        egl_image,
    })
}

fn release(fns: &GlFns, frame: Frame) {
    drop(frame.image);
    // SAFETY: GL defers the delete until in-flight draws finish.
    unsafe { gl::DeleteTextures(1, &frame.texture) };
    let _ = fns.egl.destroy_image(fns.display, frame.egl_image);
    drop(frame.sample);
}

/// Letterboxes the frame into the canvas, keeping its aspect ratio.
fn draw_contain(ctx: &mut CanvasContext, image: &skia_safe::Image) {
    let (cw, ch) = (ctx.size.width, ctx.size.height);
    let (iw, ih) = (image.width() as f32, image.height() as f32);
    let scale = (cw / iw).min(ch / ih);
    let (w, h) = (iw * scale, ih * scale);
    let dst = skia_safe::Rect::from_xywh((cw - w) / 2., (ch - h) / 2., w, h);
    let sampling = skia_safe::SamplingOptions::new(
        skia_safe::FilterMode::Linear,
        skia_safe::MipmapMode::None,
    );
    ctx.canvas.draw_image_rect_with_sampling_options(
        image,
        None,
        dst,
        sampling,
        &skia_safe::Paint::default(),
    );
}
