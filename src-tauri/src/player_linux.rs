use gtk::glib::translate::IntoGlib;
use gtk::prelude::*;
use libloading::Library;
use serde::Serialize;
use std::{
    ffi::{c_char, c_int, c_void, CStr, CString},
    ptr,
    sync::{Arc, Mutex},
};
use tauri::{App, Manager};

const MPV_FORMAT_STRING: c_int = 1;
const MPV_FORMAT_FLAG: c_int = 3;
const MPV_FORMAT_INT64: c_int = 4;
const MPV_FORMAT_DOUBLE: c_int = 5;
const MPV_RENDER_PARAM_API_TYPE: c_int = 1;
const MPV_RENDER_PARAM_OPENGL_INIT_PARAMS: c_int = 2;
const MPV_RENDER_PARAM_OPENGL_FBO: c_int = 3;
const MPV_RENDER_PARAM_FLIP_Y: c_int = 4;
const GL_FRAMEBUFFER_BINDING: u32 = 0x8CA6;
#[cfg(feature = "native-e2e")]
const GL_READ_FRAMEBUFFER: u32 = 0x8CA8;
#[cfg(feature = "native-e2e")]
const GL_READ_FRAMEBUFFER_BINDING: u32 = 0x8CAA;
#[cfg(feature = "native-e2e")]
const GL_UNSIGNED_BYTE: u32 = 0x1401;
#[cfg(feature = "native-e2e")]
const GL_RGBA: u32 = 0x1908;

type MpvHandle = c_void;
type MpvRenderContext = c_void;
type GlGetIntegerv = unsafe extern "C" fn(u32, *mut c_int);
#[cfg(feature = "native-e2e")]
type GlReadPixels =
    unsafe extern "C" fn(c_int, c_int, c_int, c_int, u32, u32, *mut c_void);
#[cfg(feature = "native-e2e")]
type GlBindFramebuffer = unsafe extern "C" fn(u32, u32);
#[cfg(feature = "native-e2e")]
type GlGetError = unsafe extern "C" fn() -> u32;
#[cfg(feature = "native-e2e")]
type GlGetString = unsafe extern "C" fn(u32) -> *const c_char;
#[cfg(feature = "native-e2e")]
const GL_RENDERER: u32 = 0x1F01;
#[cfg(feature = "native-e2e")]
const GL_VERSION: u32 = 0x1F02;

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
type MpvFree = unsafe extern "C" fn(*mut c_void);
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
    free: MpvFree,
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
            free: symbol!("mpv_free", MpvFree),
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
    #[cfg(feature = "native-e2e")]
    last_frame_color: Option<[u8; 3]>,
    #[cfg(feature = "native-e2e")]
    last_render_size: Option<[c_int; 2]>,
    #[cfg(feature = "native-e2e")]
    last_framebuffer: Option<c_int>,
    #[cfg(feature = "native-e2e")]
    render_count: u64,
    #[cfg(feature = "native-e2e")]
    last_probe_colors: Option<[[u8; 3]; 5]>,
    #[cfg(feature = "native-e2e")]
    max_center_color: [u8; 3],
    #[cfg(feature = "native-e2e")]
    max_any_color: [u8; 3],
    #[cfg(feature = "native-e2e")]
    blue_render_count: u64,
    #[cfg(feature = "native-e2e")]
    last_gl_error: u32,
    #[cfg(feature = "native-e2e")]
    gl_renderer: Option<String>,
    #[cfg(feature = "native-e2e")]
    gl_version: Option<String>,
    #[cfg(feature = "native-e2e")]
    last_grid: Option<Vec<String>>,
    #[cfg(feature = "native-e2e")]
    last_grid_render_index: u64,
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
            #[cfg(feature = "native-e2e")]
            last_frame_color: None,
            #[cfg(feature = "native-e2e")]
            last_render_size: None,
            #[cfg(feature = "native-e2e")]
            last_framebuffer: None,
            #[cfg(feature = "native-e2e")]
            render_count: 0,
            #[cfg(feature = "native-e2e")]
            last_probe_colors: None,
            #[cfg(feature = "native-e2e")]
            max_center_color: [0; 3],
            #[cfg(feature = "native-e2e")]
            max_any_color: [0; 3],
            #[cfg(feature = "native-e2e")]
            blue_render_count: 0,
            #[cfg(feature = "native-e2e")]
            last_gl_error: 0,
            #[cfg(feature = "native-e2e")]
            gl_renderer: None,
            #[cfg(feature = "native-e2e")]
            gl_version: None,
            #[cfg(feature = "native-e2e")]
            last_grid: None,
            #[cfg(feature = "native-e2e")]
            last_grid_render_index: 0,
        };
        player.set_option("vo", "libmpv")?;
        #[cfg(feature = "native-e2e")]
        player.set_option("hwdec", "no")?;
        #[cfg(feature = "native-e2e")]
        player.set_option("log-file", "/tmp/toka-mpv-e2e.log")?;
        #[cfg(feature = "native-e2e")]
        player.set_option("msg-level", "all=debug")?;
        #[cfg(not(feature = "native-e2e"))]
        player.set_option("hwdec", "auto-safe")?;
        player.set_option("keep-open", "yes")?;
        // Pick up `talk.en.srt` beside `talk.mp4`, matching Toka's own sidecar
        // detection rather than only exact filename matches.
        player.set_option("sub-auto", "fuzzy")?;
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

    fn set_i64(&mut self, name: &str, value: i64) -> Result<(), String> {
        let name = CString::new(name).unwrap();
        let mut value = value;
        self.check(unsafe {
            (self.api.set_property)(
                self.handle,
                name.as_ptr(),
                MPV_FORMAT_INT64,
                (&mut value as *mut i64).cast(),
            )
        })
    }
    fn set_double(&mut self, name: &str, value: f64) -> Result<(), String> {
        let name = CString::new(name).unwrap(); let mut value = value;
        self.check(unsafe { (self.api.set_property)(self.handle, name.as_ptr(), MPV_FORMAT_DOUBLE, (&mut value as *mut f64).cast()) })
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

    fn get_string(&self, name: &str) -> Option<String> {
        let name = CString::new(name).ok()?;
        let mut value: *mut c_char = ptr::null_mut();
        let code = unsafe {
            (self.api.get_property)(
                self.handle,
                name.as_ptr(),
                MPV_FORMAT_STRING,
                (&mut value as *mut *mut c_char).cast(),
            )
        };
        if code < 0 || value.is_null() {
            return None;
        }
        let text = unsafe { CStr::from_ptr(value) }.to_string_lossy().into_owned();
        unsafe { (self.api.free)(value.cast()) };
        Some(text)
    }

    fn get_i64(&self, name: &str) -> Option<i64> {
        let name = CString::new(name).ok()?;
        let mut value: i64 = 0;
        let code = unsafe {
            (self.api.get_property)(
                self.handle,
                name.as_ptr(),
                MPV_FORMAT_INT64,
                (&mut value as *mut i64).cast(),
            )
        };
        (code >= 0).then_some(value)
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
        #[cfg(feature = "native-e2e")]
        unsafe {
            // Identify the GL context this launch actually created, so runs
            // that come up with a different driver or version are visible.
            let renderer = (EPOXY_GL_GET_STRING)(GL_RENDERER);
            if !renderer.is_null() {
                self.gl_renderer = Some(CStr::from_ptr(renderer).to_string_lossy().into_owned());
            }
            let version = (EPOXY_GL_GET_STRING)(GL_VERSION);
            if !version.is_null() {
                self.gl_version = Some(CStr::from_ptr(version).to_string_lossy().into_owned());
            }
        }
        Ok(())
    }

    fn render(&mut self, width: c_int, height: c_int) -> Result<(), String> {
        self.initialize_renderer()?;
        let mut framebuffer: c_int = 0;
        unsafe { (EPOXY_GL_GET_INTEGERV)(GL_FRAMEBUFFER_BINDING, &mut framebuffer) };
        #[cfg(feature = "native-e2e")]
        {
            self.last_render_size = Some([width, height]);
            self.last_framebuffer = Some(framebuffer);
            self.render_count += 1;
        }
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
        })?;
        #[cfg(feature = "native-e2e")]
        unsafe {
            // mpv's GL backend leaves framebuffer 0 bound after rendering, so
            // the frame it just drew must be read through an explicit
            // GL_READ_FRAMEBUFFER binding of the GLArea framebuffer it
            // targeted; reading the ambient binding samples the never-drawn
            // default framebuffer and always returns black.
            let mut read_framebuffer: c_int = 0;
            (EPOXY_GL_GET_INTEGERV)(GL_READ_FRAMEBUFFER_BINDING, &mut read_framebuffer);
            (EPOXY_GL_BIND_FRAMEBUFFER)(GL_READ_FRAMEBUFFER, framebuffer as u32);
            let points = [
                [width / 2, height / 2],
                [width / 4, height / 2],
                [3 * width / 4, height / 2],
                [width / 2, height / 4],
                [width / 2, 3 * height / 4],
            ];
            let mut colors = [[0_u8; 3]; 5];
            for (point, slot) in points.iter().zip(colors.iter_mut()) {
                let mut color = [0_u8; 4];
                (EPOXY_GL_READ_PIXELS)(
                    point[0],
                    point[1],
                    1,
                    1,
                    GL_RGBA,
                    GL_UNSIGNED_BYTE,
                    color.as_mut_ptr().cast(),
                );
                *slot = rgb_from_rgba(color);
            }
            self.last_gl_error = (EPOXY_GL_GET_ERROR)();
            // Periodically map the whole framebuffer coarsely so failures show
            // where anything was drawn: 'B' blue, '.' near-black, '#' other.
            if self.render_count % 120 == 0 && width > 0 && height > 0 {
                const COLUMNS: c_int = 24;
                const ROWS: c_int = 10;
                let mut grid = Vec::with_capacity(ROWS as usize);
                for row in 0..ROWS {
                    let mut line = String::with_capacity(COLUMNS as usize);
                    for column in 0..COLUMNS {
                        let x = (2 * column + 1) * width / (2 * COLUMNS);
                        let y = (2 * row + 1) * height / (2 * ROWS);
                        let mut color = [0_u8; 4];
                        (EPOXY_GL_READ_PIXELS)(
                            x,
                            y,
                            1,
                            1,
                            GL_RGBA,
                            GL_UNSIGNED_BYTE,
                            color.as_mut_ptr().cast(),
                        );
                        let (red, green, blue) =
                            (u16::from(color[0]), u16::from(color[1]), u16::from(color[2]));
                        line.push(if blue > 180 && blue > red * 2 && blue > green * 2 {
                            'B'
                        } else if red < 8 && green < 8 && blue < 8 {
                            '.'
                        } else {
                            '#'
                        });
                    }
                    grid.push(line);
                }
                self.last_grid = Some(grid);
                self.last_grid_render_index = self.render_count;
            }
            (EPOXY_GL_BIND_FRAMEBUFFER)(GL_READ_FRAMEBUFFER, read_framebuffer as u32);
            let center = colors[0];
            self.last_frame_color = Some(center);
            self.last_probe_colors = Some(colors);
            for channel in 0..3 {
                self.max_center_color[channel] = self.max_center_color[channel].max(center[channel]);
                for color in &colors {
                    self.max_any_color[channel] = self.max_any_color[channel].max(color[channel]);
                }
            }
            if colors.iter().any(|c| {
                let (red, green, blue) = (u16::from(c[0]), u16::from(c[1]), u16::from(c[2]));
                blue > 180 && blue > red * 2 && blue > green * 2
            }) {
                self.blue_render_count += 1;
            }
        }
        Ok(())
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
    // libepoxy exports OpenGL entry points as dispatch-pointer objects, not
    // functions. Declaring this symbol as a function makes the CPU execute the
    // writable pointer storage itself and segfault as soon as rendering starts.
    #[link_name = "epoxy_glGetIntegerv"]
    static EPOXY_GL_GET_INTEGERV: GlGetIntegerv;
    #[cfg(feature = "native-e2e")]
    #[link_name = "epoxy_glReadPixels"]
    static EPOXY_GL_READ_PIXELS: GlReadPixels;
    #[cfg(feature = "native-e2e")]
    #[link_name = "epoxy_glBindFramebuffer"]
    static EPOXY_GL_BIND_FRAMEBUFFER: GlBindFramebuffer;
    #[cfg(feature = "native-e2e")]
    #[link_name = "epoxy_glGetError"]
    static EPOXY_GL_GET_ERROR: GlGetError;
    #[cfg(feature = "native-e2e")]
    #[link_name = "epoxy_glGetString"]
    static EPOXY_GL_GET_STRING: GlGetString;
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
    render_error: Mutex<Option<String>>,
}

impl NativePlayer {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            mpv: Mutex::new(Mpv::new()),
            render_error: Mutex::new(None),
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

    fn render(&self, width: c_int, height: c_int) -> Result<(), String> {
        let result = self.with_mpv(|mpv| mpv.render(width, height));
        if let Err(error) = &result {
            if let Ok(mut render_error) = self.render_error.lock() {
                *render_error = Some(error.clone());
            }
        }
        result
    }

    fn render_error(&self) -> Option<String> {
        self.render_error.lock().ok()?.clone()
    }

    fn set_render_error(&self, error: String) {
        if let Ok(mut render_error) = self.render_error.lock() {
            *render_error = Some(error);
        }
    }
}

#[derive(Clone, Copy)]
struct VideoBounds {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    visible: bool,
}

static VIDEO_BOUNDS_SENDER: Mutex<Option<gtk::glib::Sender<VideoBounds>>> = Mutex::new(None);

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
    video_area.set_size_request(1, 1);
    overlay.add_overlay(&video_area);

    let realize_player = player.clone();
    video_area.connect_realize(move |area| {
        area.make_current();
        if let Some(error) = area.error() {
            realize_player.set_render_error(format!("OpenGL context creation failed: {error}"));
        } else if let Err(error) = realize_player.with_mpv(Mpv::initialize_renderer) {
            realize_player.set_render_error(error);
        }
        area.hide();
    });
    let render_player = player.clone();
    video_area.connect_render(move |area, _| {
        let scale = area.scale_factor();
        let width = area.allocated_width().saturating_mul(scale);
        let height = area.allocated_height().saturating_mul(scale);
        if let Err(error) = render_player.render(width, height) {
            eprintln!("{error}");
        }
        gtk::glib::Propagation::Stop
    });
    video_area.add_tick_callback(|area, _| {
        area.queue_render();
        gtk::glib::ControlFlow::Continue
    });

    #[allow(deprecated)]
    let (bounds_sender, bounds_receiver) =
        gtk::glib::MainContext::channel::<VideoBounds>(gtk::glib::Priority::default());
    let bounds_area = video_area.clone();
    bounds_receiver.attach(None, move |bounds| {
        bounds_area.set_margin_start(bounds.x.max(0));
        bounds_area.set_margin_top(bounds.y.max(0));
        bounds_area.set_size_request(bounds.width.max(1), bounds.height.max(1));
        bounds_area.set_visible(bounds.visible);
        gtk::glib::ControlFlow::Continue
    });
    *VIDEO_BOUNDS_SENDER
        .lock()
        .map_err(|_| "The video bounds channel stopped unexpectedly.")? = Some(bounds_sender);
    overlay.show_all();
    Ok(())
}

pub fn set_bounds(x: i32, y: i32, width: i32, height: i32, visible: bool) {
    if let Ok(sender) = VIDEO_BOUNDS_SENDER.lock() {
        if let Some(sender) = sender.as_ref() {
            let _ = sender.send(VideoBounds {
                x,
                y,
                width,
                height,
                visible,
            });
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackState {
    duration: f64,
    current_time: f64,
    paused: bool,
    ended: bool,
    #[cfg(feature = "native-e2e")]
    frame_color: Option<[u8; 3]>,
    #[cfg(feature = "native-e2e")]
    render_size: Option<[c_int; 2]>,
    #[cfg(feature = "native-e2e")]
    framebuffer: Option<c_int>,
    #[cfg(feature = "native-e2e")]
    render_count: u64,
    #[cfg(feature = "native-e2e")]
    video_format: Option<String>,
    #[cfg(feature = "native-e2e")]
    vo_configured: Option<bool>,
    #[cfg(feature = "native-e2e")]
    display_size: Option<[i64; 2]>,
    #[cfg(feature = "native-e2e")]
    decoder_frame_drops: Option<i64>,
    #[cfg(feature = "native-e2e")]
    vo_frame_drops: Option<i64>,
    #[cfg(feature = "native-e2e")]
    probe_colors: Option<[[u8; 3]; 5]>,
    #[cfg(feature = "native-e2e")]
    max_center_color: [u8; 3],
    #[cfg(feature = "native-e2e")]
    max_any_color: [u8; 3],
    #[cfg(feature = "native-e2e")]
    blue_render_count: u64,
    #[cfg(feature = "native-e2e")]
    gl_error: u32,
    #[cfg(feature = "native-e2e")]
    gl_renderer: Option<String>,
    #[cfg(feature = "native-e2e")]
    gl_version: Option<String>,
    #[cfg(feature = "native-e2e")]
    grid: Option<Vec<String>>,
    #[cfg(feature = "native-e2e")]
    grid_render_index: u64,
    #[cfg(feature = "native-e2e")]
    video_rect_margins: Option<[i64; 4]>,
}

pub fn load(player: &NativePlayer, path: &str) -> Result<(), String> {
    player.with_mpv(|mpv| mpv.command(&["loadfile", path, "replace"]))
}

pub fn set_paused(player: &NativePlayer, paused: bool) -> Result<(), String> {
  player.with_mpv(|mpv| mpv.set_flag("pause", paused))
}
pub fn set_speed(player: &NativePlayer, speed: f64) -> Result<(), String> {
    if !speed.is_finite() || !(0.5..=2.0).contains(&speed) { return Err("Playback speed must be between 0.5× and 2×.".into()); }
    player.with_mpv(|mpv| mpv.set_double("speed", speed))
}

pub fn rotation(player: &NativePlayer) -> Result<i32, String> {
    player.with_mpv(|mpv| {
        Ok(mpv
            .get_i64("video-rotate")
            .unwrap_or(0)
            .rem_euclid(360) as i32)
    })
}

pub fn set_rotation(player: &NativePlayer, degrees: i32) -> Result<(), String> {
    player.with_mpv(|mpv| mpv.set_i64("video-rotate", i64::from(degrees.rem_euclid(360))))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleTrack {
    pub id: i64,
    pub label: String,
    pub external: bool,
}

/// Every subtitle track mpv knows about for the loaded file: embedded streams
/// and the sidecar files `sub-auto` picked up beside it.
pub fn subtitle_tracks(player: &NativePlayer) -> Result<Vec<SubtitleTrack>, String> {
    player.with_mpv(|mpv| {
        let count = mpv.get_i64("track-list/count").unwrap_or(0).max(0);
        let mut tracks = Vec::new();
        for index in 0..count {
            if mpv.get_string(&format!("track-list/{index}/type")).as_deref() != Some("sub") {
                continue;
            }
            let Some(id) = mpv.get_i64(&format!("track-list/{index}/id")) else {
                continue;
            };
            tracks.push(SubtitleTrack {
                id,
                label: subtitle_label(
                    mpv.get_string(&format!("track-list/{index}/title")).as_deref(),
                    mpv.get_string(&format!("track-list/{index}/lang")).as_deref(),
                    id,
                ),
                external: mpv
                    .get_flag(&format!("track-list/{index}/external"))
                    .unwrap_or(false),
            });
        }
        Ok(tracks)
    })
}

fn subtitle_label(title: Option<&str>, language: Option<&str>, id: i64) -> String {
    let title = title.filter(|value| !value.is_empty());
    let language = language.filter(|value| !value.is_empty());
    match (title, language) {
        (Some(title), Some(language)) => format!("{title} ({})", language.to_uppercase()),
        (Some(title), None) => title.to_owned(),
        (None, Some(language)) => language.to_uppercase(),
        (None, None) => format!("Track {id}"),
    }
}

/// Selects a subtitle track, or turns subtitles off when given `None`.
pub fn set_subtitle(player: &NativePlayer, id: Option<i64>) -> Result<(), String> {
    let value = id.map_or_else(|| "no".to_owned(), |id| id.to_string());
    player.with_mpv(|mpv| mpv.command(&["set", "sid", &value]))
}

pub fn seek(player: &NativePlayer, seconds: f64) -> Result<(), String> {
    player.with_mpv(|mpv| mpv.command(&["seek", &seconds.max(0.0).to_string(), "absolute+exact"]))
}

pub fn state(player: &NativePlayer) -> Result<PlaybackState, String> {
    if let Some(error) = player.render_error() {
        return Err(error);
    }
    player.with_mpv(|mpv| {
        Ok(PlaybackState {
            duration: mpv.get_double("duration").unwrap_or(0.0),
            current_time: mpv.get_double("time-pos").unwrap_or(0.0),
            paused: mpv.get_flag("pause").unwrap_or(true),
            ended: mpv.get_flag("eof-reached").unwrap_or(false),
            #[cfg(feature = "native-e2e")]
            frame_color: mpv.last_frame_color,
            #[cfg(feature = "native-e2e")]
            render_size: mpv.last_render_size,
            #[cfg(feature = "native-e2e")]
            framebuffer: mpv.last_framebuffer,
            #[cfg(feature = "native-e2e")]
            render_count: mpv.render_count,
            #[cfg(feature = "native-e2e")]
            video_format: mpv.get_string("video-format"),
            #[cfg(feature = "native-e2e")]
            vo_configured: mpv.get_flag("vo-configured"),
            #[cfg(feature = "native-e2e")]
            display_size: match (mpv.get_i64("dwidth"), mpv.get_i64("dheight")) {
                (Some(width), Some(height)) => Some([width, height]),
                _ => None,
            },
            #[cfg(feature = "native-e2e")]
            decoder_frame_drops: mpv.get_i64("decoder-frame-drop-count"),
            #[cfg(feature = "native-e2e")]
            vo_frame_drops: mpv.get_i64("frame-drop-count"),
            #[cfg(feature = "native-e2e")]
            probe_colors: mpv.last_probe_colors,
            #[cfg(feature = "native-e2e")]
            max_center_color: mpv.max_center_color,
            #[cfg(feature = "native-e2e")]
            max_any_color: mpv.max_any_color,
            #[cfg(feature = "native-e2e")]
            blue_render_count: mpv.blue_render_count,
            #[cfg(feature = "native-e2e")]
            gl_error: mpv.last_gl_error,
            #[cfg(feature = "native-e2e")]
            gl_renderer: mpv.gl_renderer.clone(),
            #[cfg(feature = "native-e2e")]
            gl_version: mpv.gl_version.clone(),
            #[cfg(feature = "native-e2e")]
            grid: mpv.last_grid.clone(),
            #[cfg(feature = "native-e2e")]
            grid_render_index: mpv.last_grid_render_index,
            #[cfg(feature = "native-e2e")]
            video_rect_margins: match (
                mpv.get_i64("osd-dimensions/ml"),
                mpv.get_i64("osd-dimensions/mr"),
                mpv.get_i64("osd-dimensions/mt"),
                mpv.get_i64("osd-dimensions/mb"),
            ) {
                (Some(left), Some(right), Some(top), Some(bottom)) => {
                    Some([left, right, top, bottom])
                }
                _ => None,
            },
        })
    })
}

pub fn stop(player: &NativePlayer) -> Result<(), String> {
    player.with_mpv(|mpv| mpv.command(&["stop"]))
}

#[cfg(any(feature = "native-e2e", test))]
fn rgb_from_rgba(color: [u8; 4]) -> [u8; 3] {
    [color[0], color[1], color[2]]
}

#[cfg(test)]
mod tests {
    use super::{rgb_from_rgba, subtitle_label};

    #[test]
    fn extracts_rgb_from_an_aligned_rgba_pixel() {
        assert_eq!(rgb_from_rgba([12, 34, 56, 255]), [12, 34, 56]);
    }

    #[test]
    fn names_subtitle_tracks_from_whatever_metadata_the_file_carries() {
        assert_eq!(subtitle_label(Some("Forced"), Some("en"), 1), "Forced (EN)");
        assert_eq!(subtitle_label(Some("Commentary"), None, 1), "Commentary");
        assert_eq!(subtitle_label(None, Some("pt"), 2), "PT");
        assert_eq!(subtitle_label(Some(""), Some(""), 3), "Track 3");
    }
}
