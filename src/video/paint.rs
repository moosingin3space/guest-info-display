//! Turning the player's newest frame into pixels inside a Freya canvas.
//!
//! On the zero-copy path the frame is a DMA-BUF: EGL wraps it as an image, GL
//! binds that to a texture, and Skia borrows the texture. All of it happens on
//! the EGL context Freya makes current while painting, which is why that path
//! needs Freya's OpenGL renderer. The fallback copies RGBA into a raster image.

use std::ffi::c_void;

use freya::prelude::CanvasContext;
use gstreamer as gst;
use gstreamer_allocators as gst_allocators;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;
use khronos_egl as egl;
use skia_safe::gpu::{self, SurfaceOrigin};
use skia_safe::{AlphaType, ColorType, Data, FilterMode, Image, ImageInfo, MipmapMode, Paint};
use skia_safe::{Rect, SamplingOptions};

use super::FrameKind;
use super::player::Shared;
use crate::layout::schema::Fit;

// EGL_EXT_image_dma_buf_import(_modifiers); khronos-egl doesn't define them.
const LINUX_DMA_BUF_EXT: egl::Enum = 0x3270;
const LINUX_DRM_FOURCC_EXT: egl::Attrib = 0x3271;
const DMA_BUF_PLANE0_FD_EXT: egl::Attrib = 0x3272;
const DMA_BUF_PLANE0_OFFSET_EXT: egl::Attrib = 0x3273;
const DMA_BUF_PLANE0_PITCH_EXT: egl::Attrib = 0x3274;
const DMA_BUF_PLANE0_MODIFIER_LO_EXT: egl::Attrib = 0x3443;
const DMA_BUF_PLANE0_MODIFIER_HI_EXT: egl::Attrib = 0x3444;

type EglImageTargetTexture2DOes = unsafe extern "system" fn(target: u32, image: *const c_void);

/// The frame on screen, plus whatever GPU objects back it. Those live until the
/// next frame replaces this one, by which point Skia has flushed the draw that
/// used them.
struct Shown {
    sample: gst::Sample,
    image: Image,
    gpu: Option<(u32, egl::Image)>,
}

/// Per-view paint state. Lives in the view's hooks, so it is created and torn
/// down on the UI thread, where the GL context is.
#[derive(Default)]
pub struct Painter {
    gl: Option<GlFns>,
    shown: Option<Shown>,
    /// Logged once per view: a broken import would otherwise log every frame.
    reported: bool,
}

/// Whether an EGL context is current on this thread. Freya's is, on the UI
/// thread, when its OpenGL renderer runs on EGL. Under X11 it usually runs on
/// GLX instead (glutin tries GLX first there), and a DMA-BUF has nothing to be
/// imported into.
pub fn egl_is_current() -> bool {
    egl::Instance::new(egl::Static)
        .get_current_display()
        .is_some()
}

struct GlFns {
    egl: egl::Instance<egl::Static>,
    display: egl::Display,
    image_target_texture: EglImageTargetTexture2DOes,
}

impl GlFns {
    fn load() -> Result<Self, String> {
        let egl = egl::Instance::new(egl::Static);
        let display = egl
            .get_current_display()
            .ok_or("no current EGL display; is Freya on its OpenGL renderer?")?;
        gl::load_with(|name| {
            egl.get_proc_address(name)
                .map_or(std::ptr::null(), |f| f as *const c_void)
        });
        let image_target_texture = egl
            .get_proc_address("glEGLImageTargetTexture2DOES")
            .ok_or("GL_OES_EGL_image is missing")?;
        // SAFETY: the signature is fixed by the GL_OES_EGL_image spec.
        let image_target_texture = unsafe {
            std::mem::transmute::<extern "system" fn(), EglImageTargetTexture2DOes>(
                image_target_texture,
            )
        };
        Ok(Self {
            egl,
            display,
            image_target_texture,
        })
    }
}

impl Painter {
    pub fn paint(&mut self, ctx: &mut CanvasContext, shared: &Shared, fit: Fit) {
        let latest = shared.frame.lock().unwrap().clone();
        let is_new = match (&latest, &self.shown) {
            (Some(latest), Some(shown)) => latest.as_ptr() != shown.sample.as_ptr(),
            (Some(_), None) => true,
            (None, _) => false,
        };

        if let (true, Some(sample)) = (is_new, latest) {
            match self.import(ctx, &shared.kind, sample) {
                Ok(shown) => {
                    if let Some(old) = self.shown.replace(shown) {
                        self.release(old);
                    }
                }
                Err(e) if !self.reported => {
                    self.reported = true;
                    log::error!("video: cannot show a frame: {e}");
                }
                Err(_) => {}
            }
        }

        if let Some(shown) = &self.shown {
            draw(ctx, &shown.image, fit);
        }
    }

    fn import(
        &mut self,
        ctx: &mut CanvasContext,
        kind: &FrameKind,
        sample: gst::Sample,
    ) -> Result<Shown, String> {
        match kind {
            FrameKind::Dmabuf(_) => self.import_dmabuf(ctx, sample),
            FrameKind::Raster => import_raster(sample),
        }
    }

    fn import_dmabuf(
        &mut self,
        ctx: &mut CanvasContext,
        sample: gst::Sample,
    ) -> Result<Shown, String> {
        let mut direct = ctx
            .canvas
            .direct_context()
            .ok_or("the canvas has no GPU context")?;
        if self.gl.is_none() {
            self.gl = Some(GlFns::load()?);
        }
        let fns = self.gl.as_ref().expect("loaded above");

        let caps = sample.caps().ok_or("frame without caps")?;
        let info = gst_video::VideoInfoDmaDrm::from_caps(caps).map_err(|e| e.to_string())?;
        let buffer = sample.buffer().ok_or("frame without a buffer")?;
        if buffer.n_memory() != 1 {
            return Err(format!(
                "expected a single-plane DMA-BUF, got {} memories",
                buffer.n_memory()
            ));
        }
        let fd = buffer
            .peek_memory(0)
            .downcast_memory_ref::<gst_allocators::DmaBufMemory>()
            .ok_or("frame memory is not a DMA-BUF")?
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
        // SAFETY: EGL_LINUX_DMA_BUF_EXT takes no context and a null client
        // buffer; the attributes describe the buffer instead.
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
        // Skia caches GL state, and ours just changed under it.
        direct.reset(None);

        // GL samples an imported EGLImage in RGBA order whatever its memory
        // layout, so every packed-RGB fourcc maps to RGBA8888 here.
        let texture_info = gpu::gl::TextureInfo {
            target: gl::TEXTURE_2D,
            id: texture,
            format: gl::RGBA8,
            protected: gpu::Protected::No,
        };
        // SAFETY: the texture outlives the image; `release` frees both together.
        let backend = unsafe {
            gpu::backend_textures::make_gl(
                (width, height),
                gpu::Mipmapped::No,
                texture_info,
                "video",
            )
        };
        let Some(image) = gpu::images::borrow_texture_from(
            &mut direct,
            &backend,
            SurfaceOrigin::TopLeft,
            ColorType::RGBA8888,
            AlphaType::Opaque,
            None,
        ) else {
            // SAFETY: nothing else holds the texture yet.
            unsafe { gl::DeleteTextures(1, &texture) };
            let _ = fns.egl.destroy_image(fns.display, egl_image);
            return Err("Skia refused the video texture".to_string());
        };

        Ok(Shown {
            sample,
            image,
            gpu: Some((texture, egl_image)),
        })
    }

    fn release(&self, shown: Shown) {
        drop(shown.image);
        if let (Some((texture, egl_image)), Some(fns)) = (shown.gpu, &self.gl) {
            // SAFETY: GL defers the delete until in-flight draws finish.
            unsafe { gl::DeleteTextures(1, &texture) };
            let _ = fns.egl.destroy_image(fns.display, egl_image);
        }
    }
}

impl Drop for Painter {
    fn drop(&mut self) {
        if let Some(shown) = self.shown.take() {
            self.release(shown);
        }
    }
}

/// The fallback: copy the RGBA plane into a raster image Skia uploads itself.
fn import_raster(sample: gst::Sample) -> Result<Shown, String> {
    let caps = sample.caps().ok_or("frame without caps")?;
    let info = gst_video::VideoInfo::from_caps(caps).map_err(|e| e.to_string())?;
    let buffer = sample.buffer().ok_or("frame without a buffer")?;
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
        .map_err(|_| "cannot map the frame".to_string())?;
    let stride = frame.plane_stride()[0] as usize;
    let data = frame.plane_data(0).map_err(|e| e.to_string())?;
    let image_info = ImageInfo::new(
        (info.width() as i32, info.height() as i32),
        ColorType::RGBA8888,
        AlphaType::Opaque,
        None,
    );
    let image = skia_safe::images::raster_from_data(&image_info, Data::new_copy(data), stride)
        .ok_or("Skia refused the frame")?;
    drop(frame);
    Ok(Shown {
        sample,
        image,
        gpu: None,
    })
}

/// Draw `image` into the canvas: `cover` fills and crops, `contain` letterboxes.
fn draw(ctx: &mut CanvasContext, image: &Image, fit: Fit) {
    let (cw, ch) = (ctx.size.width, ctx.size.height);
    let (iw, ih) = (image.width() as f32, image.height() as f32);
    if cw <= 0. || ch <= 0. || iw <= 0. || ih <= 0. {
        return;
    }
    let scale = match fit {
        Fit::Cover => (cw / iw).max(ch / ih),
        Fit::Contain => (cw / iw).min(ch / ih),
    };
    let (w, h) = (iw * scale, ih * scale);
    let dst = Rect::from_xywh((cw - w) / 2., (ch - h) / 2., w, h);
    let sampling = SamplingOptions::new(FilterMode::Linear, MipmapMode::None);

    ctx.canvas.save();
    ctx.canvas
        .clip_rect(Rect::from_wh(cw, ch), None, Some(true));
    ctx.canvas
        .draw_image_rect_with_sampling_options(image, None, dst, sampling, &Paint::default());
    ctx.canvas.restore();
}
