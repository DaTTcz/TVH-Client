//! `MpvPlayer`: embeds mpv's video output directly into the egui window
//! using mpv's OpenGL render API (`libmpv2::render`), painted via an
//! `egui_glow::CallbackFn` / `egui::PaintCallback` inside a `CentralPanel`.
//!
//! ## The self-referential struct problem
//!
//! `libmpv2`'s `Mpv::create_render_context(&'_ self, ...) -> RenderContext<'_>`
//! ties the render context's lifetime to a borrow of the `Mpv` handle it was
//! created from. Storing both the `Mpv` and its `RenderContext` in the same
//! struct is therefore a classic self-referential-struct problem, which
//! ordinary safe Rust can't express. We work around it like this:
//!
//! - `mpv` lives in a `Box<Mpv>`, so its heap address is stable and never
//!   moves even if `MpvPlayer` itself is moved (e.g. wrapped in an `Arc`).
//! - We create the render context through a raw pointer to that boxed
//!   value, erasing the borrow's lifetime to `'static` with `unsafe`.
//! - This is only sound if `render_ctx` never outlives `mpv`. Rust
//!   guarantees struct fields drop top-to-bottom in declaration order, so
//!   `render_ctx` is declared *before* `mpv` below - it is always torn down
//!   first.
//!
//! ## Send/Sync
//!
//! `RenderContext` wraps a raw mpv pointer and isn't automatically
//! `Send`/`Sync`. We assert both manually on `MpvPlayer`, which is sound
//! because every mpv render call in this app happens on the single UI/
//! render thread, invoked exclusively from inside egui's paint callback -
//! never concurrently, never from a background thread.
//!
//! ## Why we render to an off-screen texture and blit it
//!
//! `RenderContext::render(fbo, width, height, flip)` has no x/y offset
//! parameter - mpv always draws starting at `(0, 0)` of whatever
//! framebuffer `fbo` names, filling exactly `width x height`. That's fine
//! if mpv owns the whole framebuffer, but our video panel is a sub-rect
//! of the shared eframe/egui window (there's a top bar and a channel-list
//! panel next to it). If we simply pointed mpv at the window's default
//! framebuffer (id `0`), it would draw at the *window's* origin instead
//! of the panel's - which is exactly the "video renders but is shifted
//! out of its box" bug this module works around.
//!
//! Instead, `render()` renders mpv's frame into a private off-screen FBO
//! sized to exactly the video rect (recreated whenever that size
//! changes), then `glBlitFramebuffer`s it into the correctly-positioned
//! sub-rect of whatever framebuffer egui_glow had bound when the paint
//! callback fired (the window's default framebuffer, id `0`, for the
//! `egui_glow` 0.36 native backend - see `Painter::intermediate_fbo()`,
//! which currently always returns `None`).

use eframe::glow::{self, HasContext as _};
use libmpv2::render::{OpenGLInitParams, RenderContext, RenderParam, RenderParamApiType};
use libmpv2::Mpv;
use std::ffi::{c_void, CStr, CString};
use std::sync::{Arc, Mutex};

/// The GL proc-address loader eframe hands us in `CreationContext`.
type ProcAddrFn = Arc<dyn Fn(&CStr) -> *const c_void + Send + Sync>;

/// Carries eframe's proc-address loader through to mpv's C callback, which
/// only accepts a plain `fn` pointer plus an opaque context value.
struct GlProcCtx {
    get_proc_address: ProcAddrFn,
}

fn resolve_gl_proc(ctx: &GlProcCtx, name: &str) -> *mut c_void {
    match CString::new(name) {
        Ok(cname) => (ctx.get_proc_address)(cname.as_c_str()) as *mut c_void,
        Err(_) => std::ptr::null_mut(),
    }
}

/// The off-screen render target mpv draws into, before we blit it into
/// place. Recreated whenever the video panel's pixel size changes.
struct VideoTarget {
    fbo: glow::Framebuffer,
    // The raw integer id of `fbo`, obtained once at creation time via
    // `GL_FRAMEBUFFER_BINDING` right after binding it - `libmpv2`'s render
    // API wants a plain `i32`, and glow doesn't expose one from a
    // `glow::Framebuffer` handle directly.
    fbo_id: i32,
    texture: glow::Texture,
    width: i32,
    height: i32,
}

pub struct MpvPlayer {
    // Declared before `mpv`: fields drop in declaration order, and this
    // must be destroyed before `mpv` is. See module docs.
    render_ctx: RenderContext<'static>,
    mpv: Box<Mpv>,
    gl: Arc<glow::Context>,
    target: Mutex<Option<VideoTarget>>,
    // Most recent playback error, if mpv is currently in one - see
    // `poll_errors` for why this exists at all (mpv's own event queue is
    // the only place an async load failure ever shows up).
    last_error: Mutex<Option<String>>,
}

// SAFETY: see module docs - all render calls happen on the UI/render
// thread only, driven from egui's paint callback.
unsafe impl Send for MpvPlayer {}
unsafe impl Sync for MpvPlayer {}

impl MpvPlayer {
    /// Create a new player bound to the OpenGL context eframe/glow set up.
    ///
    /// Requires eframe to be running with the glow renderer (see
    /// `NativeOptions::renderer` in `main.rs`) - that's what makes
    /// `cc.get_proc_address` available.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Result<Self, String> {
        let get_proc_address = cc.get_proc_address.clone().ok_or_else(|| {
            "eframe neběží s glow rendererem (chybí get_proc_address)".to_string()
        })?;
        let gl = cc
            .gl
            .clone()
            .ok_or_else(|| "eframe neposkytuje glow::Context (chybí cc.gl)".to_string())?;

        let mpv = Mpv::new().map_err(|e| format!("Mpv::new selhalo: {e}"))?;
        // We draw the video ourselves via the render API - no separate
        // native window, no on-screen controller (OSC) from mpv itself.
        let _ = mpv.set_property("vo", "libmpv");
        // Softwarové dekódování, ne "auto-safe" (HW dekódování přes GPU) -
        // diagnostický pokus kvůli blokovým/kostičkovým obrazovým
        // artefaktům, co se objevovaly během přehrávání a nešly ničím
        // (přepnutím kanálu, stop/play) odstranit - viz `render()`'s
        // vlastní oprava výše (chyba vykreslení se už nezahazuje) a
        // diskuze s Davidem. Typický projev vadného/nekompatibilního HW
        // dekodéru na některých GPU/ovladačích. Nevýhoda: vyšší zátěž CPU
        // než HW dekódování - pokud se ukáže jako problém (např. u HD
        // kanálů na slabším CPU), je tohle první místo, kam se vrátit.
        let _ = mpv.set_property("hwdec", "no");
        let _ = mpv.set_property("keep-open", "yes");
        let _ = mpv.set_property("osc", false);

        let mpv = Box::new(mpv);

        let params = vec![
            RenderParam::ApiType(RenderParamApiType::OpenGl),
            RenderParam::InitParams(OpenGLInitParams {
                get_proc_address: resolve_gl_proc,
                ctx: GlProcCtx { get_proc_address },
            }),
        ];

        // SAFETY: `mpv` is heap-allocated via `Box` and its address is
        // stable for the lifetime of `MpvPlayer`. We extend the borrow
        // below to `'static`, but `render_ctx` is declared before `mpv`
        // in the struct, so it is guaranteed to be dropped first - it
        // never actually outlives the `Mpv` value it points into.
        let mpv_static: &'static Mpv = unsafe { &*(mpv.as_ref() as *const Mpv) };
        let render_ctx = mpv_static
            .create_render_context(params)
            .map_err(|e| format!("Nepodařilo se vytvořit mpv render kontext: {e}"))?;

        Ok(Self {
            render_ctx,
            mpv,
            gl,
            target: Mutex::new(None),
            last_error: Mutex::new(None),
        })
    }

    /// Start playing the given stream URL (replaces whatever was
    /// playing), clearing any previous playback error - see
    /// `poll_errors`.
    pub fn load(&self, url: &str) -> Result<(), String> {
        *self.last_error.lock().unwrap() = None;
        self.mpv
            .command("loadfile", &[url, "replace"])
            .map_err(|e| format!("Nepodařilo se spustit stream: {e}"))
    }

    /// Drains mpv's event queue, remembering the most recent playback
    /// error (if any), and returns the *current* error state - `None`
    /// once a later `load()` starts cleanly, so callers can just
    /// overwrite whatever error banner they're showing with this every
    /// frame instead of tracking "was there ever one" themselves.
    ///
    /// This exists because `load()`'s own `Result` only reflects whether
    /// mpv *accepted* the "play this" command - whether it then actually
    /// manages to open/decode the URL happens asynchronously on mpv's
    /// side and otherwise has nowhere to surface (`keep-open` is set in
    /// `new`, so a failed load doesn't reset/advance either - without
    /// this, a bad URL, unsupported format, auth problem, or network
    /// timeout would just show a permanently black video area with no
    /// indication why).
    pub fn poll_errors(&self) -> Option<String> {
        while let Some(event) = self.mpv.wait_event(0.0) {
            if let Err(e) = event {
                *self.last_error.lock().unwrap() = Some(e.to_string());
            }
        }
        self.last_error.lock().unwrap().clone()
    }

    /// `Some(percent)` (0-100) while mpv is stalled waiting for its
    /// network cache to fill enough to (re)start playback - typically
    /// right after `load()`, or mid-playback on a slow/high-latency
    /// connection - `None` once it has enough buffered to actually play.
    /// Meant for a small "loading..." indicator so that state doesn't
    /// look identical to nothing happening at all.
    pub fn buffering_percent(&self) -> Option<i64> {
        let paused_for_cache: bool = self.mpv.get_property("paused-for-cache").unwrap_or(false);
        if !paused_for_cache {
            return None;
        }
        self.mpv.get_property("cache-buffering-state").ok()
    }

    /// Stop playback (blank frame, no channel loaded).
    pub fn stop(&self) -> Result<(), String> {
        self.mpv
            .command("stop", &[])
            .map_err(|e| format!("Nepodařilo se zastavit přehrávání: {e}"))
    }

    pub fn set_paused(&self, paused: bool) {
        let _ = self.mpv.set_property("pause", paused);
    }

    /// mpv's own volume scale (0-100, its default). Queried live each
    /// call rather than cached, so it can't drift out of sync with what
    /// mpv actually has set.
    pub fn volume(&self) -> f64 {
        self.mpv.get_property("volume").unwrap_or(100.0)
    }

    /// Set volume, clamped to 0-100.
    pub fn set_volume(&self, volume: f64) {
        let _ = self.mpv.set_property("volume", volume.clamp(0.0, 100.0));
    }

    /// Render the current video frame and blit it into place at
    /// `(dst_x, dst_y)` (bottom-left origin, i.e. `glViewport`/
    /// `glBlitFramebuffer` convention - matches
    /// `PaintCallbackInfo::viewport_in_pixels()`'s `left_px`/
    /// `from_bottom_px` fields directly), sized `width x height` pixels.
    ///
    /// Meant to be called only from inside an `egui_glow::CallbackFn`,
    /// where a valid GL context is current and the window's framebuffer
    /// is bound. See module docs for why this doesn't just render
    /// straight into that framebuffer.
    pub fn render(&self, dst_x: i32, dst_y: i32, width: i32, height: i32) {
        if width <= 0 || height <= 0 {
            return;
        }

        let gl = &self.gl;
        let mut target = self.target.lock().unwrap();

        let needs_new = match &*target {
            Some(t) => t.width != width || t.height != height,
            None => true,
        };
        if needs_new {
            if let Some(old) = target.take() {
                unsafe {
                    gl.delete_framebuffer(old.fbo);
                    gl.delete_texture(old.texture);
                }
            }
            *target = Some(unsafe { create_video_target(gl, width, height) });
        }
        let t = target.as_ref().expect("just created above");

        // Render mpv's frame into our own dedicated off-screen target -
        // mpv always draws at (0, 0) of whatever framebuffer it's given,
        // which is correct here since this FBO is exactly video-sized.
        //
        // The error case used to be silently swallowed (`let _ = ...`).
        // That meant that if this call ever started failing mid-playback
        // (bad GPU/driver state, lost context, ...), `render()` would
        // just keep re-blitting whatever was already sitting in `t`'s
        // texture from the last successful call, forever - the video
        // would visibly freeze on one (possibly already-corrupted, e.g.
        // a decode glitch caught mid-frame) image, and nothing later
        // (changing channel, stop/play, ...) could ever fix it, because
        // none of that touches this render target - only a resize
        // previously forced it to be rebuilt. Now: surfaced through the
        // same `last_error`/`poll_errors` the "Přehrávání selhalo" banner
        // already reads, and the target is torn down so the *next* call
        // rebuilds it from scratch instead of continuing to reuse
        // whatever GPU state just failed.
        if let Err(e) = self.render_ctx.render::<()>(t.fbo_id, width, height, true) {
            *self.last_error.lock().unwrap() = Some(format!("Vykreslení snímku selhalo: {e}"));
            if let Some(bad) = target.take() {
                unsafe {
                    gl.delete_framebuffer(bad.fbo);
                    gl.delete_texture(bad.texture);
                }
            }
            return;
        }

        // ...then copy it into the real, shared framebuffer at the
        // sub-rect egui actually wants it in.
        unsafe {
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(t.fbo));
            // `None` = the default (window) framebuffer. Always correct
            // for this egui_glow version - see module docs.
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, None);
            gl.blit_framebuffer(
                0,
                0,
                width,
                height,
                dst_x,
                dst_y,
                dst_x + width,
                dst_y + height,
                glow::COLOR_BUFFER_BIT,
                glow::LINEAR,
            );
            // Leave a plain FRAMEBUFFER binding on the default target, so
            // whatever egui_glow paints next (it doesn't rebind a
            // framebuffer itself between callbacks) lands in the right
            // place.
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
    }
}

/// # Safety
/// Must be called with a current, valid GL context.
unsafe fn create_video_target(gl: &glow::Context, width: i32, height: i32) -> VideoTarget {
    unsafe {
        let texture = gl.create_texture().expect("create video texture");
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            width,
            height,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(None),
        );
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_S,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_T,
            glow::CLAMP_TO_EDGE as i32,
        );

        let fbo = gl.create_framebuffer().expect("create video fbo");
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(texture),
            0,
        );
        // Grab the raw id while it's the bound framebuffer - see
        // `VideoTarget::fbo_id` docs.
        let fbo_id = gl.get_parameter_i32(glow::FRAMEBUFFER_BINDING);

        VideoTarget {
            fbo,
            fbo_id,
            texture,
            width,
            height,
        }
    }
}

impl Drop for MpvPlayer {
    fn drop(&mut self) {
        if let Some(t) = self.target.lock().unwrap().take() {
            unsafe {
                self.gl.delete_framebuffer(t.fbo);
                self.gl.delete_texture(t.texture);
            }
        }
    }
}
