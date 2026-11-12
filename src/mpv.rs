//! Liaison minimale avec libmpv, chargée dynamiquement au démarrage.
//!
//! La DLL (`libmpv-2.dll` sous Windows) est cherchée à côté de l'exécutable.
//! Le chargement dynamique évite d'avoir à fournir un fichier `.lib` au moment
//! de l'édition de liens : il suffit de déposer la DLL dans le dossier de l'app.

#![allow(non_camel_case_types, dead_code)]

use std::ffi::{c_char, c_double, c_int, c_void, CStr, CString};
use std::path::PathBuf;

use libloading::Library;

pub type mpv_handle = c_void;
pub type mpv_render_context = c_void;

pub const MPV_FORMAT_NONE: c_int = 0;
pub const MPV_FORMAT_STRING: c_int = 1;
pub const MPV_FORMAT_FLAG: c_int = 3;
pub const MPV_FORMAT_INT64: c_int = 4;
pub const MPV_FORMAT_DOUBLE: c_int = 5;

pub const MPV_EVENT_NONE: c_int = 0;
pub const MPV_EVENT_SHUTDOWN: c_int = 1;
pub const MPV_EVENT_LOG_MESSAGE: c_int = 2;
pub const MPV_EVENT_START_FILE: c_int = 6;
pub const MPV_EVENT_END_FILE: c_int = 7;
pub const MPV_EVENT_FILE_LOADED: c_int = 8;
pub const MPV_EVENT_VIDEO_RECONFIG: c_int = 17;
pub const MPV_EVENT_PLAYBACK_RESTART: c_int = 21;
pub const MPV_EVENT_PROPERTY_CHANGE: c_int = 22;

pub const MPV_END_FILE_REASON_EOF: c_int = 0;
pub const MPV_END_FILE_REASON_STOP: c_int = 2;
pub const MPV_END_FILE_REASON_QUIT: c_int = 3;
pub const MPV_END_FILE_REASON_ERROR: c_int = 4;
pub const MPV_END_FILE_REASON_REDIRECT: c_int = 5;

pub const MPV_RENDER_PARAM_INVALID: c_int = 0;
pub const MPV_RENDER_PARAM_API_TYPE: c_int = 1;
pub const MPV_RENDER_PARAM_OPENGL_INIT_PARAMS: c_int = 2;
pub const MPV_RENDER_PARAM_OPENGL_FBO: c_int = 3;
pub const MPV_RENDER_PARAM_FLIP_Y: c_int = 4;
pub const MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME: c_int = 12;

pub const MPV_RENDER_UPDATE_FRAME: u64 = 1;

#[repr(C)]
pub struct mpv_event {
    pub event_id: c_int,
    pub error: c_int,
    pub reply_userdata: u64,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct mpv_event_property {
    pub name: *const c_char,
    pub format: c_int,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct mpv_event_end_file {
    pub reason: c_int,
    pub error: c_int,
    pub playlist_entry_id: i64,
    pub playlist_insert_id: i64,
    pub playlist_insert_num_entries: c_int,
}

#[repr(C)]
pub struct mpv_render_param {
    pub type_: c_int,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct mpv_opengl_init_params {
    pub get_proc_address: Option<unsafe extern "C" fn(ctx: *mut c_void, name: *const c_char) -> *mut c_void>,
    pub get_proc_address_ctx: *mut c_void,
}

#[repr(C)]
pub struct mpv_opengl_fbo {
    pub fbo: c_int,
    pub w: c_int,
    pub h: c_int,
    pub internal_format: c_int,
}

pub type UpdateCallback = unsafe extern "C" fn(cb_ctx: *mut c_void);

/// Table des fonctions libmpv utilisées par l'application.
pub struct MpvApi {
    _lib: Library,
    pub client_api_version: unsafe extern "C" fn() -> u64,
    pub create: unsafe extern "C" fn() -> *mut mpv_handle,
    pub initialize: unsafe extern "C" fn(*mut mpv_handle) -> c_int,
    pub terminate_destroy: unsafe extern "C" fn(*mut mpv_handle),
    pub set_option_string: unsafe extern "C" fn(*mut mpv_handle, *const c_char, *const c_char) -> c_int,
    pub set_property_string: unsafe extern "C" fn(*mut mpv_handle, *const c_char, *const c_char) -> c_int,
    pub set_property: unsafe extern "C" fn(*mut mpv_handle, *const c_char, c_int, *mut c_void) -> c_int,
    pub command: unsafe extern "C" fn(*mut mpv_handle, *mut *const c_char) -> c_int,
    pub command_async: unsafe extern "C" fn(*mut mpv_handle, u64, *mut *const c_char) -> c_int,
    pub wait_event: unsafe extern "C" fn(*mut mpv_handle, c_double) -> *mut mpv_event,
    pub wakeup: unsafe extern "C" fn(*mut mpv_handle),
    pub observe_property: unsafe extern "C" fn(*mut mpv_handle, u64, *const c_char, c_int) -> c_int,
    pub error_string: unsafe extern "C" fn(c_int) -> *const c_char,
    pub render_context_create:
        unsafe extern "C" fn(*mut *mut mpv_render_context, *mut mpv_handle, *mut mpv_render_param) -> c_int,
    pub render_context_free: unsafe extern "C" fn(*mut mpv_render_context),
    pub render_context_render: unsafe extern "C" fn(*mut mpv_render_context, *mut mpv_render_param) -> c_int,
    pub render_context_update: unsafe extern "C" fn(*mut mpv_render_context) -> u64,
    pub render_context_set_update_callback:
        unsafe extern "C" fn(*mut mpv_render_context, Option<UpdateCallback>, *mut c_void),
    pub render_context_report_swap: Option<unsafe extern "C" fn(*mut mpv_render_context)>,
}

// Les pointeurs de fonctions sont immuables et libmpv est thread-safe.
unsafe impl Send for MpvApi {}
unsafe impl Sync for MpvApi {}

fn candidate_paths() -> Vec<PathBuf> {
    #[cfg(windows)]
    let names: &[&str] = &["libmpv-2.dll", "mpv-2.dll", "mpv-1.dll", "libmpv.dll"];
    #[cfg(target_os = "macos")]
    let names: &[&str] = &["libmpv.2.dylib", "libmpv.dylib"];
    #[cfg(all(unix, not(target_os = "macos")))]
    let names: &[&str] = &["libmpv.so.2", "libmpv.so.1", "libmpv.so"];

    let mut out = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for n in names {
                out.push(dir.join(n));
                out.push(dir.join("mpv").join(n));
            }
        }
    }
    // Dossier courant puis recherche système (PATH / ld.so).
    for n in names {
        out.push(PathBuf::from(n));
    }
    out
}

macro_rules! sym {
    ($lib:expr, $name:literal) => {{
        let s = $lib
            .get(concat!($name, "\0").as_bytes())
            .map_err(|e| format!("symbole {} introuvable : {e}", $name))?;
        *s
    }};
}

impl MpvApi {
    pub fn load() -> Result<Self, String> {
        let mut last_err = String::from("aucune bibliothèque trouvée");
        for path in candidate_paths() {
            match unsafe { Library::new(&path) } {
                Ok(lib) => {
                    log::info!("libmpv chargée depuis {}", path.display());
                    return unsafe { Self::from_library(lib) };
                }
                Err(e) => last_err = format!("{}: {e}", path.display()),
            }
        }
        Err(format!("Impossible de charger libmpv ({last_err})"))
    }

    unsafe fn from_library(lib: Library) -> Result<Self, String> {
        let report_swap = lib
            .get::<unsafe extern "C" fn(*mut mpv_render_context)>(b"mpv_render_context_report_swap\0")
            .ok()
            .map(|s| *s);
        Ok(Self {
            client_api_version: sym!(lib, "mpv_client_api_version"),
            create: sym!(lib, "mpv_create"),
            initialize: sym!(lib, "mpv_initialize"),
            terminate_destroy: sym!(lib, "mpv_terminate_destroy"),
            set_option_string: sym!(lib, "mpv_set_option_string"),
            set_property_string: sym!(lib, "mpv_set_property_string"),
            set_property: sym!(lib, "mpv_set_property"),
            command: sym!(lib, "mpv_command"),
            command_async: sym!(lib, "mpv_command_async"),
            wait_event: sym!(lib, "mpv_wait_event"),
            wakeup: sym!(lib, "mpv_wakeup"),
            observe_property: sym!(lib, "mpv_observe_property"),
            error_string: sym!(lib, "mpv_error_string"),
            render_context_create: sym!(lib, "mpv_render_context_create"),
            render_context_free: sym!(lib, "mpv_render_context_free"),
            render_context_render: sym!(lib, "mpv_render_context_render"),
            render_context_update: sym!(lib, "mpv_render_context_update"),
            render_context_set_update_callback: sym!(lib, "mpv_render_context_set_update_callback"),
            render_context_report_swap: report_swap,
            _lib: lib,
        })
    }

    pub fn err_str(&self, code: c_int) -> String {
        unsafe {
            let p = (self.error_string)(code);
            if p.is_null() {
                format!("erreur {code}")
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
    }
}

/// Convertit une chaîne Rust en `CString` (les octets nuls sont retirés).
pub fn cstr(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}
