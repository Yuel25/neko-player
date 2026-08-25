//! libmpv render-API integration with Slint's OpenGL context.
//!
//! The flow (mirrors Slint's official `opengl_texture` example):
//! 1. Slint is started with an OpenGL(ES) backend (see main.rs).
//! 2. In `RenderingSetup` we receive Slint's `get_proc_address` loader,
//!    build a `glow` context from it, and create the mpv render context.
//! 3. mpv renders each frame into an offscreen FBO backed by a GL texture.
//! 4. The texture is surfaced into the UI as a borrowed GL texture image,
//!    double-buffered so mpv never renders into the texture Slint is
//!    currently displaying.

use crate::ffi;
use crate::player::MpvPlayer;
use glow::HasContext;
use std::cell::RefCell;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::Arc;

type GlProcLoader = dyn Fn(&CStr) -> *const c_void;

thread_local! {
    static GET_PROC: RefCell<Option<&'static GlProcLoader>> =
        const { RefCell::new(None) };
}

/// Install Slint's GL proc-address loader for libmpv's use.
///
/// # Safety of the lifetime extension
/// The reference is only valid for the current notifier callback, but the
/// loader it points to is owned by the Slint backend, which outlives every
/// rendering callback. All of our GL work — including the mpv render
/// context that consumes this loader — happens on this (UI/GL) thread and
/// is fully released in `RenderingTeardown`, before the backend is dropped.
pub fn install_loader(loader: &dyn Fn(&CStr) -> *const c_void) {
    let extended: &'static dyn Fn(&CStr) -> *const c_void = unsafe { std::mem::transmute(loader) };
    GET_PROC.with(|slot| *slot.borrow_mut() = Some(extended));
}

pub fn clear_loader() {
    GET_PROC.with(|slot| *slot.borrow_mut() = None);
}

pub fn get_proc(name: &CStr) -> *const c_void {
    GET_PROC.with(|slot| match slot.borrow().as_ref() {
        Some(f) => f(name),
        None => std::ptr::null(),
    })
}

unsafe extern "C" fn mpv_get_proc_address(_ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    if name.is_null() {
        return std::ptr::null_mut();
    }
    let name = CStr::from_ptr(name);
    get_proc(name) as *mut c_void
}

const DEFAULT_SIZE: (u32, u32) = (640, 360);

pub struct VideoRenderer {
    gl: glow::Context,
    player: Arc<MpvPlayer>,
    textures: [glow::NativeTexture; 2],
    fbos: [glow::NativeFramebuffer; 2],
    size: (u32, u32),
    /// Index of the texture currently shown by Slint; mpv renders into the other.
    front: usize,
    rc: *mut ffi::mpv_render_context,
    update_ctx: *mut c_void,
}

// The render context must only be used from one thread (the UI/GL thread),
// which is exactly how main.rs drives it.
unsafe impl Send for VideoRenderer {}

impl VideoRenderer {
    /// Must be called from `RenderingSetup` (GL context current).
    pub fn new(
        gl: glow::Context,
        player: Arc<MpvPlayer>,
        on_frame_ready: impl Fn() + Send + 'static,
    ) -> Result<VideoRenderer, String> {
        let (textures, fbos) = unsafe { create_targets(&gl, DEFAULT_SIZE) };

        let api_type = b"opengl\0";
        let mut init = ffi::mpv_opengl_init_params {
            get_proc_address: Some(mpv_get_proc_address),
            get_proc_address_ctx: std::ptr::null_mut(),
        };
        let mut params = [
            ffi::mpv_render_param {
                type_: ffi::MPV_RENDER_PARAM_API_TYPE,
                data: api_type.as_ptr() as *mut c_void,
            },
            ffi::mpv_render_param {
                type_: ffi::MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
                data: &mut init as *mut ffi::mpv_opengl_init_params as *mut c_void,
            },
            ffi::mpv_render_param {
                type_: 0,
                data: std::ptr::null_mut(),
            },
        ];

        let mut rc: *mut ffi::mpv_render_context = std::ptr::null_mut();
        let err = unsafe {
            ffi::mpv_render_context_create(&mut rc, player.handle(), params.as_mut_ptr())
        };
        if err < 0 {
            return Err(format!("mpv_render_context_create failed: {err}"));
        }
        eprintln!("[neko] mpv render context created");

        // Called by mpv when a new frame is ready; must not call mpv APIs.
        // Double-boxed for the same reason as the wakeup handler in player.rs.
        let boxed: Box<dyn Fn() + Send> = Box::new(on_frame_ready);
        let ctx = Box::into_raw(Box::new(boxed)) as *mut c_void;
        unsafe extern "C" fn update_trampoline(ctx: *mut c_void) {
            let f = &*(ctx as *const Box<dyn Fn() + Send>);
            f();
        }
        unsafe { ffi::mpv_render_context_set_update_callback(rc, Some(update_trampoline), ctx) };

        let r = VideoRenderer {
            gl,
            player,
            textures,
            fbos,
            size: DEFAULT_SIZE,
            front: 0,
            rc,
            update_ctx: ctx,
        };
        r.clear_black(0);
        r.clear_black(1);
        Ok(r)
    }

    fn clear_black(&self, idx: usize) {
        unsafe {
            self.gl
                .bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbos[idx]));
            self.gl
                .viewport(0, 0, self.size.0 as i32, self.size.1 as i32);
            self.gl.clear_color(0.0, 0.0, 0.0, 1.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
    }

    /// The image Slint should display right now.
    ///
    /// The texture is *borrowed* by the image: it must stay allocated (and
    /// the GL context current) while Slint displays it. We only call this
    /// from rendering-notifier callbacks, and the double buffering in
    /// `render_frame` keeps the displayed texture away from mpv until it is
    /// replaced by the next frame.
    pub fn current_image(&self) -> slint::Image {
        unsafe {
            slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(
                self.textures[self.front].0,
                (self.size.0, self.size.1).into(),
            )
            .build()
        }
    }

    /// Render a new video frame if one is ready. Call from
    /// `BeforeRendering`. Returns a fresh image when the frame (or the
    /// video size) changed.
    pub fn render_frame(&mut self) -> Option<slint::Image> {
        if let Some(sz) = self.player.take_pending_size() {
            if sz != self.size {
                eprintln!("[neko] video size: {}x{}", sz.0, sz.1);
                unsafe {
                    destroy_targets(&self.gl, &self.textures, &self.fbos);
                }
                let (textures, fbos) = unsafe { create_targets(&self.gl, sz) };
                self.textures = textures;
                self.fbos = fbos;
                self.size = sz;
                self.clear_black(0);
                self.clear_black(1);
                return Some(self.current_image());
            }
        }

        let flags = unsafe { ffi::mpv_render_context_update(self.rc) };
        if flags & ffi::MPV_RENDER_UPDATE_FRAME == 0 {
            return None;
        }
        let back = 1 - self.front;
        unsafe {
            self.prepare_for_mpv();
            self.gl
                .bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbos[back]));
            self.gl
                .viewport(0, 0, self.size.0 as i32, self.size.1 as i32);

            let mut target = ffi::mpv_opengl_fbo {
                fbo: self.fbos[back].0.get() as c_int,
                w: self.size.0 as c_int,
                h: self.size.1 as c_int,
                internal_format: ffi::GL_RGBA8,
            };
            // This is an off-screen texture, not OpenGL's default framebuffer.
            // Slint's borrowed texture API already interprets it with a top-left
            // origin, so asking mpv to flip here turns the video upside down.
            let mut flip_y: c_int = 0;
            let mut params = [
                ffi::mpv_render_param {
                    type_: ffi::MPV_RENDER_PARAM_OPENGL_FBO,
                    data: &mut target as *mut ffi::mpv_opengl_fbo as *mut c_void,
                },
                ffi::mpv_render_param {
                    type_: ffi::MPV_RENDER_PARAM_FLIP_Y,
                    data: &mut flip_y as *mut c_int as *mut c_void,
                },
                ffi::mpv_render_param {
                    type_: 0,
                    data: std::ptr::null_mut(),
                },
            ];
            let err = ffi::mpv_render_context_render(self.rc, params.as_mut_ptr());
            // Always restore Slint's default framebuffer, including error paths.
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            if err < 0 {
                eprintln!("[neko] mpv_render_context_render failed: {err}");
                return None;
            }
        }

        self.front = back;
        Some(self.current_image())
    }

    /// libmpv's OpenGL API expects standard GL defaults on entry. Slint's
    /// femtovg renderer deliberately keeps state cached between frames, so in
    /// particular pixel-store row lengths and scissors may still describe a UI
    /// upload. Inheriting those values corrupts wide video frames row-by-row.
    unsafe fn prepare_for_mpv(&self) {
        self.gl.use_program(None);
        self.gl.bind_vertex_array(None);
        self.gl.bind_buffer(glow::ARRAY_BUFFER, None);
        self.gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, None);
        self.gl.active_texture(glow::TEXTURE0);
        self.gl.bind_texture(glow::TEXTURE_2D, None);

        self.gl.disable(glow::BLEND);
        self.gl.disable(glow::CULL_FACE);
        self.gl.disable(glow::DEPTH_TEST);
        self.gl.disable(glow::SCISSOR_TEST);
        self.gl.disable(glow::STENCIL_TEST);
        self.gl.color_mask(true, true, true, true);
        self.gl.depth_mask(true);

        self.gl.pixel_store_i32(glow::PACK_ALIGNMENT, 4);
        self.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
        self.gl.pixel_store_i32(glow::PACK_ROW_LENGTH, 0);
        self.gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
        self.gl.pixel_store_i32(glow::PACK_SKIP_PIXELS, 0);
        self.gl.pixel_store_i32(glow::PACK_SKIP_ROWS, 0);
        self.gl.pixel_store_i32(glow::UNPACK_SKIP_PIXELS, 0);
        self.gl.pixel_store_i32(glow::UNPACK_SKIP_ROWS, 0);
    }

    /// Must be called from `RenderingTeardown` (GL context current).
    pub fn teardown(&mut self) {
        unsafe {
            if !self.rc.is_null() {
                ffi::mpv_render_context_set_update_callback(self.rc, None, std::ptr::null_mut());
                ffi::mpv_render_context_free(self.rc);
                self.rc = std::ptr::null_mut();
                if !self.update_ctx.is_null() {
                    drop(Box::from_raw(self.update_ctx as *mut Box<dyn Fn() + Send>));
                    self.update_ctx = std::ptr::null_mut();
                }
            }
            destroy_targets(&self.gl, &self.textures, &self.fbos);
        }
        self.player.mark_render_released();
        eprintln!("[neko] render context released");
    }
}

unsafe fn create_targets(
    gl: &glow::Context,
    (w, h): (u32, u32),
) -> ([glow::NativeTexture; 2], [glow::NativeFramebuffer; 2]) {
    let mut textures = Vec::with_capacity(2);
    let mut fbos = Vec::with_capacity(2);
    for _ in 0..2 {
        let tex = gl.create_texture().expect("gl.create_texture");
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            w as i32,
            h as i32,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            None,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::LINEAR as i32,
        );
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

        let fbo = gl.create_framebuffer().expect("gl.create_framebuffer");
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, Some(tex), 0);
        gl.bind_texture(glow::TEXTURE_2D, None);
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);

        textures.push(tex);
        fbos.push(fbo);
    }
    let textures: [glow::NativeTexture; 2] = textures.try_into().unwrap();
    let fbos: [glow::NativeFramebuffer; 2] = fbos.try_into().unwrap();
    (textures, fbos)
}

unsafe fn destroy_targets(
    gl: &glow::Context,
    textures: &[glow::NativeTexture; 2],
    fbos: &[glow::NativeFramebuffer; 2],
) {
    for t in textures {
        gl.delete_texture(*t);
    }
    for f in fbos {
        gl.delete_framebuffer(*f);
    }
}
