//! The v2 engine — DuckDB's v2 C API, loaded on demand.
//!
//! ffi.rs is generated from DuckDB's api_spec/v2 YAML (scripts/gen-v2-ffi.rb):
//! the whole surface as one dlsym-filled function table. This module is the
//! hand-written rim: find the library, load it once, and give errors and
//! string views a Rust shape. It carries its own candidate search because it
//! is the successor to the v1 loader in src/engine.rs, not a client of it —
//! when 0.21 flips, that file and the duckdb-rs dependency behind it go away.
//!
//! Unix opens with RTLD_NOW | RTLD_GLOBAL. GLOBAL is load-bearing: DuckDB's
//! own extension loading expects engine symbols resolvable from the global
//! namespace.

pub mod conn;
pub mod encode;
pub mod ffi;

use std::env;
use std::fmt;
use std::path::PathBuf;
use std::sync::OnceLock;

use libloading::Library;

#[cfg(target_os = "macos")]
const LIB_NAME: &str = "libduckdb.dylib";
#[cfg(all(unix, not(target_os = "macos")))]
const LIB_NAME: &str = "libduckdb.so";
#[cfg(windows)]
const LIB_NAME: &str = "duckdb.dll";

/// The loaded engine: the function table plus where it came from.
pub struct Engine {
    pub api: ffi::Api,
    pub version: String,
    pub path: PathBuf,
    pub symbols: usize,
}

static ENGINE: OnceLock<Result<Engine, String>> = OnceLock::new();

/// Load the v2 engine if it is not already loaded. Idempotent and cheap
/// after the first call. Errs when no library is found, or when the one
/// found predates the v2 C API.
pub fn engine() -> Result<&'static Engine, String> {
    match ENGINE.get_or_init(load) {
        Ok(e) => Ok(e),
        Err(e) => Err(e.clone()),
    }
}

/// Where to look, in order. First hit wins; the bare name at the end lets
/// the system loader's own search have the final say.
fn candidates() -> Vec<PathBuf> {
    let mut c = Vec::new();
    if let Ok(p) = env::var("HARBOR_LIBDUCKDB") {
        c.push(PathBuf::from(p));
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            c.push(dir.join("../lib").join(LIB_NAME));
            c.push(dir.join(LIB_NAME));
        }
    }
    if let Some(home) = env::home_dir() {
        c.push(home.join(".local/lib").join(LIB_NAME));
        c.push(home.join(".duckdb/cli/latest").join(LIB_NAME));
        if let Ok(entries) = std::fs::read_dir(home.join(".duckdb/cli")) {
            let mut vers: Vec<_> = entries
                .flatten()
                .map(|e| e.path().join(LIB_NAME))
                .filter(|p| p.is_file())
                .collect();
            vers.sort();
            if let Some(newest) = vers.pop() {
                c.push(newest);
            }
        }
    }
    c.push(PathBuf::from(LIB_NAME));
    c
}

#[cfg(unix)]
fn open_lib(path: &PathBuf) -> Result<Library, libloading::Error> {
    use libloading::os::unix::{Library as Unix, RTLD_GLOBAL, RTLD_NOW};
    unsafe { Unix::open(Some(path), RTLD_NOW | RTLD_GLOBAL).map(Into::into) }
}

#[cfg(windows)]
fn open_lib(path: &PathBuf) -> Result<Library, libloading::Error> {
    unsafe { Library::new(path) }
}

fn load() -> Result<Engine, String> {
    let tried = candidates();
    let (lib, path) = tried
        .iter()
        .filter(|p| p.as_os_str() == LIB_NAME.as_ref() as &std::ffi::OsStr || p.exists())
        .find_map(|p| open_lib(p).ok().map(|l| (l, p.clone())))
        .ok_or_else(|| {
            format!(
                "libduckdb not found (searched: {})",
                tried.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
            )
        })?;

    let (api, symbols) = unsafe { ffi::Api::fill(&lib) };
    if api.create_environment.is_none() {
        return Err(format!(
            "{}: engine has no v2 C API ({symbols} v2 symbols) — needs DuckDB v2.0.0 or later",
            path.display()
        ));
    }

    let mut ver = ffi::str_t { ptr: std::ptr::null(), len: 0 };
    let mut err = std::ptr::null_mut();
    let code = unsafe { (api.library_version.unwrap())(&mut ver, &mut err) };
    if code != ffi::ERROR_NONE {
        return Err(Error::take(&api, code, err).to_string());
    }
    let version = unsafe { str_view(&ver) }.to_owned();

    // The engine stays for the life of the process — closing it would turn
    // every filled pointer into a dangling one.
    std::mem::forget(lib);

    Ok(Engine { api, version, path, symbols })
}

/// A failed v2 call: the structured code plus the engine's rendered text.
#[derive(Debug)]
pub struct Error {
    pub code: ffi::ERROR,
    pub message: String,
}

impl Error {
    /// Consume an error_info out-param: read its text, destroy it, and fold
    /// both into one value. `info` may be null — the code still stands.
    pub fn take(api: &ffi::Api, code: ffi::ERROR, mut info: ffi::error_info_handle) -> Error {
        let mut message = String::new();
        if !info.is_null() {
            let mut text = ffi::str_t { ptr: std::ptr::null(), len: 0 };
            if let Some(get) = api.error_info_get_text {
                if unsafe { get(info, &mut text) } == ffi::ERROR_NONE {
                    message = unsafe { str_view(&text) }.to_owned();
                }
            }
            if let Some(destroy) = api.error_info_destroy {
                unsafe { destroy(&mut info) };
            }
        }
        Error { code, message }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.message.is_empty() {
            write!(f, "duckdb error {}", self.code)
        } else {
            write!(f, "{} (code {})", self.message, self.code)
        }
    }
}

/// View a borrowed engine string. Lossless for the UTF-8 DuckDB emits;
/// callers keep the source (and its owner) alive for the borrow.
///
/// # Safety
/// `s.ptr` must point at `s.len` live bytes (or be null with len 0).
pub unsafe fn str_view(s: &ffi::str_t) -> &str {
    if s.ptr.is_null() || s.len == 0 {
        return "";
    }
    let bytes = unsafe { std::slice::from_raw_parts(s.ptr as *const u8, s.len as usize) };
    std::str::from_utf8(bytes).unwrap_or("")
}

/// View the payload of a 16-byte `bytes` cell: inlined below the cutoff,
/// pointed-to above it. Valid only while the owning chunk is alive.
///
/// # Safety
/// `b` must be a live cell from a vector the caller has not destroyed.
pub unsafe fn bytes_view(b: &ffi::bytes_t) -> &[u8] {
    unsafe {
        let len = b.value.inlined.length as usize;
        let ptr = if len <= ffi::BYTES_INLINE_LENGTH {
            b.value.inlined.inlined.as_ptr()
        } else {
            b.value.pointer.ptr
        };
        std::slice::from_raw_parts(ptr as *const u8, len)
    }
}
