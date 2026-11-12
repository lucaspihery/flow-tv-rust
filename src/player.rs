//! Lecteur vidéo basé sur libmpv, dessiné *sous* l'interface Slint.
//!
//! Slint (moteur de rendu femtovg / OpenGL) efface le framebuffer puis appelle
//! notre callback `BeforeRendering` : on y demande à mpv de dessiner l'image
//! vidéo dans le framebuffer courant, puis Slint dessine l'interface par-dessus
//! (contrôles, EPG, liste des chaînes...). La vidéo est donc réellement intégrée
//! à la fenêtre, sans fenêtre native séparée.

use std::cell::Cell;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::mpv::*;

#[derive(Debug, Clone)]
pub enum PlayerEvent {
    Paused(bool),
    Buffering(bool),
    Volume(f64),
    Muted(bool),
    VideoHeight(i64),
    FileLoaded,
    PlaybackRestart,
    EndFile { reason: i32, error: Option<String> },
}

struct Core {
    api: MpvApi,
    handle: *mut mpv_handle,
}
unsafe impl Send for Core {}
unsafe impl Sync for Core {}

type GlGetIntegerv = unsafe extern "system" fn(pname: u32, data: *mut i32);
const GL_FRAMEBUFFER_BINDING: u32 = 0x8CA6;

pub type RedrawFn = Arc<dyn Fn() + Send + Sync>;

struct UpdateCtx {
    redraw: RedrawFn,
    pending: Arc<AtomicBool>,
}

pub struct Player {
    core: Arc<Core>,
    render_ctx: Cell<*mut mpv_render_context>,
    update_ctx: Cell<*mut UpdateCtx>,
    gl_get_integerv: Cell<Option<GlGetIntegerv>>,
    active: Cell<bool>,
    redraw_pending: Arc<AtomicBool>,
    event_thread: Cell<Option<JoinHandle<()>>>,
}

unsafe extern "C" fn get_proc_address_trampoline(ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    let f = &*(ctx as *const &dyn Fn(&CStr) -> *const c_void);
    f(CStr::from_ptr(name)) as *mut c_void
}

unsafe extern "C" fn update_trampoline(ctx: *mut c_void) {
    // Appelé depuis un thread interne de mpv : on ne touche à rien d'autre
    // que la demande de redessin (une seule à la fois).
    let ctx = &*(ctx as *const UpdateCtx);
    if !ctx.pending.swap(true, Ordering::AcqRel) {
        (ctx.redraw)();
    }
}

impl Player {
    pub fn new(on_event: impl Fn(PlayerEvent) + Send + 'static) -> Result<Self, String> {
        let api = MpvApi::load()?;
        let handle = unsafe { (api.create)() };
        if handle.is_null() {
            return Err("mpv_create a échoué".into());
        }
        let core = Arc::new(Core { api, handle });

        let opts: &[(&str, &str)] = &[
            ("vo", "libmpv"),
            ("hwdec", "auto-safe"),
            ("idle", "yes"),
            ("keep-open", "no"),
            ("terminal", "no"),
            ("msg-level", "all=warn"),
            ("osc", "no"),
            ("input-default-bindings", "no"),
            ("input-vo-keyboard", "no"),
            ("ytdl", "no"),
            ("audio-client-name", "Flow TV"),
            ("cache", "yes"),
            ("demuxer-max-bytes", "64MiB"),
            ("demuxer-max-back-bytes", "16MiB"),
            ("demuxer-readahead-secs", "20"),
            ("network-timeout", "20"),
            ("user-agent", crate::xtream::USER_AGENT),
            (
                "stream-lavf-o",
                "reconnect=1,reconnect_streamed=1,reconnect_on_network_error=1,reconnect_delay_max=5",
            ),
            ("volume-max", "130"),
        ];
        for (k, v) in opts {
            let (k, v) = (cstr(k), cstr(v));
            let r = unsafe { (core.api.set_option_string)(core.handle, k.as_ptr(), v.as_ptr()) };
            if r < 0 {
                log::warn!("option mpv {:?} refusée : {}", k, core.api.err_str(r));
            }
        }
        let r = unsafe { (core.api.initialize)(core.handle) };
        if r < 0 {
            return Err(format!("mpv_initialize : {}", core.api.err_str(r)));
        }

        let observed: &[(u64, &str, c_int)] = &[
            (1, "pause", MPV_FORMAT_FLAG),
            (2, "paused-for-cache", MPV_FORMAT_FLAG),
            (3, "volume", MPV_FORMAT_DOUBLE),
            (4, "mute", MPV_FORMAT_FLAG),
            (5, "height", MPV_FORMAT_INT64),
        ];
        for (id, name, fmt) in observed {
            let n = cstr(name);
            unsafe { (core.api.observe_property)(core.handle, *id, n.as_ptr(), *fmt) };
        }

        let thread_core = core.clone();
        let event_thread = std::thread::Builder::new()
            .name("mpv-events".into())
            .spawn(move || event_loop(thread_core, on_event))
            .map_err(|e| e.to_string())?;

        Ok(Self {
            core,
            render_ctx: Cell::new(std::ptr::null_mut()),
            update_ctx: Cell::new(std::ptr::null_mut()),
            gl_get_integerv: Cell::new(None),
            active: Cell::new(false),
            redraw_pending: Arc::new(AtomicBool::new(false)),
            event_thread: Cell::new(Some(event_thread)),
        })
    }

    /// À appeler depuis `RenderingState::RenderingSetup` (contexte GL courant).
    pub fn setup_gl(&self, get_proc_address: &dyn Fn(&CStr) -> *const c_void, redraw: RedrawFn) {
        if !self.render_ctx.get().is_null() {
            return;
        }
        let p = get_proc_address(c"glGetIntegerv");
        if !p.is_null() {
            self.gl_get_integerv.set(Some(unsafe { std::mem::transmute::<*const c_void, GlGetIntegerv>(p) }));
        }

        let gpa: &dyn Fn(&CStr) -> *const c_void = get_proc_address;
        let mut init = mpv_opengl_init_params {
            get_proc_address: Some(get_proc_address_trampoline),
            get_proc_address_ctx: &gpa as *const _ as *mut c_void,
        };
        let api_type = cstr("opengl");
        let mut params = [
            mpv_render_param { type_: MPV_RENDER_PARAM_API_TYPE, data: api_type.as_ptr() as *mut c_void },
            mpv_render_param {
                type_: MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
                data: &mut init as *mut _ as *mut c_void,
            },
            mpv_render_param { type_: MPV_RENDER_PARAM_INVALID, data: std::ptr::null_mut() },
        ];
        let mut ctx: *mut mpv_render_context = std::ptr::null_mut();
        let r =
            unsafe { (self.core.api.render_context_create)(&mut ctx, self.core.handle, params.as_mut_ptr()) };
        if r < 0 || ctx.is_null() {
            log::error!("mpv_render_context_create : {}", self.core.api.err_str(r));
            return;
        }
        let uctx = Box::into_raw(Box::new(UpdateCtx { redraw, pending: self.redraw_pending.clone() }));
        unsafe {
            (self.core.api.render_context_set_update_callback)(
                ctx,
                Some(update_trampoline),
                uctx as *mut c_void,
            )
        };
        self.update_ctx.set(uctx);
        self.render_ctx.set(ctx);
        log::info!("rendu mpv OpenGL initialisé");
    }

    /// À appeler depuis `RenderingState::BeforeRendering`.
    pub fn render(&self, width: u32, height: u32) {
        self.redraw_pending.store(false, Ordering::Release);
        let ctx = self.render_ctx.get();
        if ctx.is_null() || !self.active.get() || width == 0 || height == 0 {
            return;
        }
        unsafe { (self.core.api.render_context_update)(ctx) };

        let mut fbo_id: i32 = 0;
        if let Some(get) = self.gl_get_integerv.get() {
            unsafe { get(GL_FRAMEBUFFER_BINDING, &mut fbo_id) };
        }
        let mut fbo =
            mpv_opengl_fbo { fbo: fbo_id, w: width as c_int, h: height as c_int, internal_format: 0 };
        let mut flip: c_int = 1;
        let mut block: c_int = 0;
        let mut params = [
            mpv_render_param { type_: MPV_RENDER_PARAM_OPENGL_FBO, data: &mut fbo as *mut _ as *mut c_void },
            mpv_render_param { type_: MPV_RENDER_PARAM_FLIP_Y, data: &mut flip as *mut _ as *mut c_void },
            mpv_render_param {
                type_: MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME,
                data: &mut block as *mut _ as *mut c_void,
            },
            mpv_render_param { type_: MPV_RENDER_PARAM_INVALID, data: std::ptr::null_mut() },
        ];
        unsafe {
            (self.core.api.render_context_render)(ctx, params.as_mut_ptr());
            if let Some(swap) = self.core.api.render_context_report_swap {
                swap(ctx);
            }
        }
    }

    /// À appeler depuis `RenderingState::RenderingTeardown`.
    pub fn teardown_gl(&self) {
        let ctx = self.render_ctx.replace(std::ptr::null_mut());
        if !ctx.is_null() {
            unsafe {
                (self.core.api.render_context_set_update_callback)(ctx, None, std::ptr::null_mut());
                (self.core.api.render_context_free)(ctx);
            }
        }
        let u = self.update_ctx.replace(std::ptr::null_mut());
        if !u.is_null() {
            drop(unsafe { Box::from_raw(u) });
        }
    }

    fn command(&self, args: &[&str]) {
        let owned: Vec<_> = args.iter().map(|a| cstr(a)).collect();
        let mut ptrs: Vec<*const c_char> = owned.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        let r = unsafe { (self.core.api.command_async)(self.core.handle, 0, ptrs.as_mut_ptr()) };
        if r < 0 {
            log::warn!("commande mpv {:?} : {}", args, self.core.api.err_str(r));
        }
    }

    fn set_prop(&self, name: &str, value: &str) {
        let (n, v) = (cstr(name), cstr(value));
        let r = unsafe { (self.core.api.set_property_string)(self.core.handle, n.as_ptr(), v.as_ptr()) };
        if r < 0 {
            log::warn!("propriété mpv {name}={value} : {}", self.core.api.err_str(r));
        }
    }

    pub fn play(&self, url: &str) {
        self.active.set(true);
        self.set_prop("pause", "no");
        self.command(&["loadfile", url, "replace"]);
    }

    pub fn stop(&self) {
        self.active.set(false);
        self.command(&["stop"]);
    }

    pub fn is_active(&self) -> bool {
        self.active.get()
    }

    pub fn set_pause(&self, paused: bool) {
        self.set_prop("pause", if paused { "yes" } else { "no" });
    }

    pub fn set_volume(&self, volume: f64) {
        self.set_prop("volume", &format!("{:.0}", volume.clamp(0.0, 130.0)));
    }

    pub fn set_mute(&self, mute: bool) {
        self.set_prop("mute", if mute { "yes" } else { "no" });
    }

    /// Arrête mpv proprement. Renvoie `false` si le contexte de rendu n'a pas pu
    /// être libéré (dans ce cas on laisse le processus se terminer).
    pub fn shutdown(&self) -> bool {
        self.active.set(false);
        let quit = [cstr("quit")];
        let mut ptrs = [quit[0].as_ptr(), std::ptr::null()];
        unsafe { (self.core.api.command)(self.core.handle, ptrs.as_mut_ptr()) };
        if let Some(t) = self.event_thread.take() {
            let _ = t.join();
        }
        if !self.render_ctx.get().is_null() {
            return false;
        }
        unsafe { (self.core.api.terminate_destroy)(self.core.handle) };
        true
    }
}

fn event_loop(core: Arc<Core>, on_event: impl Fn(PlayerEvent)) {
    loop {
        let ev = unsafe { &*(core.api.wait_event)(core.handle, -1.0) };
        match ev.event_id {
            MPV_EVENT_SHUTDOWN => break,
            MPV_EVENT_FILE_LOADED => on_event(PlayerEvent::FileLoaded),
            MPV_EVENT_PLAYBACK_RESTART => on_event(PlayerEvent::PlaybackRestart),
            MPV_EVENT_END_FILE if !ev.data.is_null() => {
                let ef = unsafe { &*(ev.data as *const mpv_event_end_file) };
                let error = (ef.reason == MPV_END_FILE_REASON_ERROR).then(|| core.api.err_str(ef.error));
                on_event(PlayerEvent::EndFile { reason: ef.reason, error });
            }
            MPV_EVENT_PROPERTY_CHANGE if !ev.data.is_null() => {
                let p = unsafe { &*(ev.data as *const mpv_event_property) };
                if p.data.is_null() {
                    continue;
                }
                unsafe {
                    match (ev.reply_userdata, p.format) {
                        (1, MPV_FORMAT_FLAG) => on_event(PlayerEvent::Paused(*(p.data as *const c_int) != 0)),
                        (2, MPV_FORMAT_FLAG) => {
                            on_event(PlayerEvent::Buffering(*(p.data as *const c_int) != 0))
                        }
                        (3, MPV_FORMAT_DOUBLE) => on_event(PlayerEvent::Volume(*(p.data as *const f64))),
                        (4, MPV_FORMAT_FLAG) => on_event(PlayerEvent::Muted(*(p.data as *const c_int) != 0)),
                        (5, MPV_FORMAT_INT64) => on_event(PlayerEvent::VideoHeight(*(p.data as *const i64))),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}
