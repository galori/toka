use gtk::glib::translate::IntoGlib;
use gtk::prelude::*;
use libloading::Library;
use serde::Serialize;
use std::{
    cell::RefCell,
    ffi::{c_char, c_int, c_void, CStr, CString},
    ptr,
    sync::{Arc, Mutex},
};
use tauri::{App, Manager};

const MPV_FORMAT_FLAG: c_int = 3;
const MPV_FORMAT_DOUBLE: c_int = 5;
const MPV_RENDER_PARAM_API_TYPE: c_int = 1;
const MPV_RENDER_PARAM_OPENGL_INIT_PARAMS: c_int = 2;
const MPV_RENDER_PARAM_OPENGL_FBO: c_int = 3;
const MPV_RENDER_PARAM_FLIP_Y: c_int = 4;
const GL_FRAMEBUFFER_BINDING: u32 = 0x8CA6;

type MpvHandle = c_void;
type MpvRenderContext = c_void;

#[repr(C)]
struct MpvRenderParam {
    kind: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct MpvOpenGlInitParams {
    get_proc_address: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void>,
    get_proc_address_ctx: *mut c_void,
}

#[repr(C)]
struct MpvOpenGlFbo {
    fbo: c_int,
    width: c_int,
    height: c_int,
    internal_format: c_int,
}

type MpvCreate = unsafe extern "C" fn() -> *mut MpvHandle;
type MpvInitialize = unsafe extern "C" fn(*mut MpvHandle) -> c_int;
type MpvTerminateDestroy = unsafe extern "C" fn(*mut MpvHandle);
type MpvSetOptionString =
    unsafe extern "C" fn(*mut MpvHandle, *const c_char, *const c_char) -> c_int;
type MpvCommand = unsafe extern "C" fn(*mut MpvHandle, *const *const c_char) -> c_int;
type MpvSetProperty =
    unsafe extern "C" fn(*mut MpvHandle, *const c_char, c_int, *mut c_void) -> c_int;
type MpvGetProperty =
    unsafe extern "C" fn(*mut MpvHandle, *const c_char, c_int, *mut c_void) -> c_int;
type MpvErrorString = unsafe extern "C" fn(c_int) -> *const c_char;
type MpvRenderContextCreate =
    unsafe extern "C" fn(*mut *mut MpvRenderContext, *mut MpvHandle, *mut MpvRenderParam) -> c_int;
type MpvRenderContextRender =
    unsafe extern "C" fn(*mut MpvRenderContext, *mut MpvRenderParam) -> c_int;
type MpvRenderContextFree = unsafe extern "C" fn(*mut MpvRenderContext);

struct MpvApi {
    _library: Library,
    create: MpvCreate,
    initialize: MpvInitialize,
    terminate_destroy: MpvTerminateDestroy,
    set_option_string: MpvSetOptionString,
    command: MpvCommand,
    set_property: MpvSetProperty,
    get_property: MpvGetProperty,
    error_string: MpvErrorString,
    render_context_create: MpvRenderContextCreate,
    render_context_render: MpvRenderContextRender,
    render_context_free: MpvRenderContextFree,
}

impl MpvApi {
    unsafe fn load() -> Result<Self, String> {
        let library = Library::new("libmpv.so.2")
            .map_err(|error| format!("libmpv could not be loaded: {error}"))?;
        macro_rules! symbol {
            ($name:literal, $kind:ty) => {{
                *library
                    .get::<$kind>(concat!($name, "\0").as_bytes())
                    .map_err(|error| format!("libmpv is missing {}: {error}", $name))?
            }};
        }
        Ok(Self {
            create: symbol!("mpv_create", MpvCreate),
            initialize: symbol!("mpv_initialize", MpvInitialize),
            terminate_destroy: symbol!("mpv_terminate_destroy", MpvTerminateDestroy),
            set_option_string: symbol!("mpv_set_option_string", MpvSetOptionString),
            command: symbol!("mpv_command", MpvCommand),
            set_property: symbol!("mpv_set_property", MpvSetProperty),
            get_property: symbol!("mpv_get_property", MpvGetProperty),
            error_string: symbol!("mpv_error_string", MpvErrorString),
            render_context_create: symbol!("mpv_render_context_create", MpvRenderContextCreate),
            render_context_render: symbol!("mpv_render_context_render", MpvRenderContextRender),
            render_context_free: symbol!("mpv_render_context_free", MpvRenderContextFree),
            _library: library,
        })
    }
}

struct Mpv {
    api: MpvApi,
    handle: *mut MpvHandle,
    render_context: *mut MpvRenderContext,
}

// libmpv serializes access to a handle. Toka additionally protects it with the mutex below.
unsafe impl Send for Mpv {}

impl Mpv {
    fn new() -> Result<Self, String> {
        let api = unsafe { MpvApi::load()? };
        let handle = unsafe { (api.create)() };
        if handle.is_null() {
            return Err("libmpv could not create a playback context.".into());
        }
        let mut player = Self {
            api,
            handle,
            render_context: ptr::null_mut(),
        };
        player.set_option("vo", "libmpv")?;
        player.set_option("hwdec", "auto-safe")?;
        player.set_option("keep-open", "yes")?;
        player.set_option("terminal", "no")?;
        player.set_option("input-default-bindings", "no")?;
        player.check(unsafe { (player.api.initialize)(player.handle) })?;
        Ok(player)
    }

    fn check(&self, code: c_int) -> Result<(), String> {
        if code >= 0 {
            return Ok(());
        }
        let message = unsafe { CStr::from_ptr((self.api.error_string)(code)) }.to_string_lossy();
        Err(format!("libmpv: {message}"))
    }

    fn set_option(&mut self, name: &str, value: &str) -> Result<(), String> {
        let name = CString::new(name).unwrap();
        let value = CString::new(value).unwrap();
        self.check(unsafe {
            (self.api.set_option_string)(self.handle, name.as_ptr(), value.as_ptr())
        })
    }

    fn command(&mut self, values: &[&str]) -> Result<(), String> {
        let values: Vec<CString> = values
            .iter()
            .map(|value| {
                CString::new(*value).map_err(|_| "A playback value contained a null byte.")
            })
            .collect::<Result<_, _>>()?;
        let mut pointers: Vec<*const c_char> = values.iter().map(|value| value.as_ptr()).collect();
        pointers.push(ptr::null());
        self.check(unsafe { (self.api.command)(self.handle, pointers.as_ptr()) })
    }

    fn set_flag(&mut self, name: &str, value: bool) -> Result<(), String> {
        let name = CString::new(name).unwrap();
        let mut value: c_int = value.into();
        self.check(unsafe {
            (self.api.set_property)(
                self.handle,
                name.as_ptr(),
                MPV_FORMAT_FLAG,
                (&mut value as *mut c_int).cast(),
            )
        })
    }

    fn get_double(&self, name: &str) -> Option<f64> {
        let name = CString::new(name).ok()?;
        let mut value: f64 = 0.0;
        let code = unsafe {
            (self.api.get_property)(
                self.handle,
                name.as_ptr(),
                MPV_FORMAT_DOUBLE,
                (&mut value as *mut f64).cast(),
            )
        };
        (code >= 0 && value.is_finite()).then_some(value)
    }

    fn get_flag(&self, name: &str) -> Option<bool> {
        let name = CString::new(name).ok()?;
        let mut value: c_int = 0;
        let code = unsafe {
            (self.api.get_property)(
                self.handle,
                name.as_ptr(),
                MPV_FORMAT_FLAG,
                (&mut value as *mut c_int).cast(),
            )
        };
        (code >= 0).then_some(value != 0)
    }

    fn initialize_renderer(&mut self) -> Result<(), String> {
        if !self.render_context.is_null() {
            return Ok(());
        }
        let api_type = b"opengl\0";
        let mut init = MpvOpenGlInitParams {
            get_proc_address: Some(get_proc_address),
            get_proc_address_ctx: ptr::null_mut(),
        };
        let mut params = [
            MpvRenderParam {
                kind: MPV_RENDER_PARAM_API_TYPE,
                data: api_type.as_ptr().cast_mut().cast(),
            },
            MpvRenderParam {
                kind: MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
                data: (&mut init as *mut MpvOpenGlInitParams).cast(),
            },
            MpvRenderParam {
                kind: 0,
                data: ptr::null_mut(),
            },
        ];
        let mut context = ptr::null_mut();
        self.check(unsafe {
            (self.api.render_context_create)(&mut context, self.handle, params.as_mut_ptr())
        })?;
        self.render_context = context;
        Ok(())
    }

    fn render(&mut self, width: c_int, height: c_int) -> Result<(), String> {
        self.initialize_renderer()?;
        let mut framebuffer: c_int = 0;
        unsafe { epoxy_gl_get_integerv(GL_FRAMEBUFFER_BINDING, &mut framebuffer) };
        let mut fbo = MpvOpenGlFbo {
            fbo: framebuffer,
            width,
            height,
            internal_format: 0,
        };
        let mut flip: c_int = 1;
        let mut params = [
            MpvRenderParam {
                kind: MPV_RENDER_PARAM_OPENGL_FBO,
                data: (&mut fbo as *mut MpvOpenGlFbo).cast(),
            },
            MpvRenderParam {
                kind: MPV_RENDER_PARAM_FLIP_Y,
                data: (&mut flip as *mut c_int).cast(),
            },
            MpvRenderParam {
                kind: 0,
                data: ptr::null_mut(),
            },
        ];
        self.check(unsafe {
            (self.api.render_context_render)(self.render_context, params.as_mut_ptr())
        })
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        unsafe {
            if !self.render_context.is_null() {
                (self.api.render_context_free)(self.render_context);
            }
            (self.api.terminate_destroy)(self.handle);
        }
    }
}

#[link(name = "epoxy")]
extern "C" {
    #[link_name = "epoxy_glGetIntegerv"]
    fn epoxy_gl_get_integerv(pname: u32, data: *mut c_int);
}

#[link(name = "EGL")]
extern "C" {
    fn eglGetProcAddress(name: *const c_char) -> *mut c_void;
}

#[link(name = "GLX")]
extern "C" {
    fn glXGetProcAddress(name: *const u8) -> *mut c_void;
}

unsafe extern "C" fn get_proc_address(_context: *mut c_void, name: *const c_char) -> *mut c_void {
    let address = eglGetProcAddress(name);
    if address.is_null() {
        glXGetProcAddress(name.cast())
    } else {
        address
    }
}

pub struct NativePlayer {
    mpv: Mutex<Result<Mpv, String>>,
}

impl NativePlayer {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            mpv: Mutex::new(Mpv::new()),
        })
    }

    fn with_mpv<T>(
        &self,
        operation: impl FnOnce(&mut Mpv) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut guard = self
            .mpv
            .lock()
            .map_err(|_| "The video player stopped unexpectedly.".to_string())?;
        match guard.as_mut() {
            Ok(mpv) => operation(mpv),
            Err(error) => Err(error.clone()),
        }
    }
}

thread_local! {
    static VIDEO_AREA: RefCell<Option<gtk::GLArea>> = const { RefCell::new(None) };
}

fn disconnect_incompatible_resize_handlers(webview: &gtk::Widget) {
    // Tauri's Linux mouse and touch resize handlers assume the webview is still a
    // direct child of Tao's GtkBox. The player wraps it in a GtkOverlay, so those
    // handlers panic while walking the widget hierarchy. Toka uses a decorated
    // window and does not need the borderless-window resize handlers.
    unsafe {
        for signal_name in [
            b"button-press-event\0".as_slice(),
            b"touch-event\0".as_slice(),
        ] {
            let signal_id = gtk::glib::gobject_ffi::g_signal_lookup(
                signal_name.as_ptr().cast(),
                webview.type_().into_glib(),
            );
            if signal_id != 0 {
                gtk::glib::gobject_ffi::g_signal_handlers_disconnect_matched(
                    webview.as_ptr().cast(),
                    gtk::glib::gobject_ffi::G_SIGNAL_MATCH_ID,
                    signal_id,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
            }
        }
    }
}

pub fn install(app: &mut App, player: Arc<NativePlayer>) -> Result<(), Box<dyn std::error::Error>> {
    let window = app
        .get_webview_window("main")
        .ok_or("The main window was not created.")?;
    let vbox = window.default_vbox()?;
    let webview = vbox
        .children()
        .into_iter()
        .next()
        .ok_or("The web view was not created.")?;
    disconnect_incompatible_resize_handlers(&webview);
    vbox.remove(&webview);

    let overlay = gtk::Overlay::new();
    overlay.add(&webview);
    vbox.pack_start(&overlay, true, true, 0);

    let video_area = gtk::GLArea::new();
    video_area.set_auto_render(false);
    video_area.set_has_alpha(false);
    video_area.set_halign(gtk::Align::Start);
    video_area.set_valign(gtk::Align::Start);
    video_area.set_visible(false);
    overlay.add_overlay(&video_area);

    let render_player = player.clone();
    video_area.connect_render(move |area, _| {
        let scale = area.scale_factor();
        let width = area.allocated_width().saturating_mul(scale);
        let height = area.allocated_height().saturating_mul(scale);
        if let Err(error) = render_player.with_mpv(|mpv| mpv.render(width, height)) {
            eprintln!("{error}");
        }
        gtk::glib::Propagation::Stop
    });
    video_area.add_tick_callback(|area, _| {
        area.queue_render();
        gtk::glib::ControlFlow::Continue
    });
    VIDEO_AREA.with(|slot| *slot.borrow_mut() = Some(video_area));
    overlay.show_all();
    VIDEO_AREA.with(|slot| {
        if let Some(area) = slot.borrow().as_ref() {
            area.hide();
        }
    });
    Ok(())
}

pub fn set_bounds(x: i32, y: i32, width: i32, height: i32, visible: bool) {
    VIDEO_AREA.with(|slot| {
        if let Some(area) = slot.borrow().as_ref() {
            area.set_margin_start(x.max(0));
            area.set_margin_top(y.max(0));
            area.set_size_request(width.max(1), height.max(1));
            area.set_visible(visible);
        }
    });
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackState {
    duration: f64,
    current_time: f64,
    paused: bool,
    ended: bool,
}

pub fn load(player: &NativePlayer, path: &str) -> Result<(), String> {
    player.with_mpv(|mpv| mpv.command(&["loadfile", path, "replace"]))
}

pub fn set_paused(player: &NativePlayer, paused: bool) -> Result<(), String> {
    player.with_mpv(|mpv| mpv.set_flag("pause", paused))
}

pub fn seek(player: &NativePlayer, seconds: f64) -> Result<(), String> {
    player.with_mpv(|mpv| mpv.command(&["seek", &seconds.max(0.0).to_string(), "absolute+exact"]))
}

pub fn state(player: &NativePlayer) -> Result<PlaybackState, String> {
    player.with_mpv(|mpv| {
        Ok(PlaybackState {
            duration: mpv.get_double("duration").unwrap_or(0.0),
            current_time: mpv.get_double("time-pos").unwrap_or(0.0),
            paused: mpv.get_flag("pause").unwrap_or(true),
            ended: mpv.get_flag("eof-reached").unwrap_or(false),
        })
    })
}

pub fn stop(player: &NativePlayer) -> Result<(), String> {
    player.with_mpv(|mpv| mpv.command(&["stop"]))
}
