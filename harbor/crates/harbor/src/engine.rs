//! On-demand engine loading — the binary carries no load-time
//! dependency on libduckdb.
//!
//! The loadable-extension bindings route every DuckDB C call through an
//! AtomicPtr that starts null, so the binary launches on a machine with
//! no libduckdb at all; pure-client invocations never come here. When a
//! code path becomes the engine (serving, or opening a file in-process),
//! this module plays the extension HOST that the bindings' generated
//! init expects: dlopen the library, fill a `duckdb_ext_api_v1` by
//! symbol name (engine_fill.rs, generated), and hand it over through a
//! synthetic `duckdb_extension_access` whose `get_api` returns our
//! struct.
//!
//! Unix opens with RTLD_NOW | RTLD_GLOBAL. GLOBAL is load-bearing:
//! DuckDB's own extension loading (httpfs, community) expects engine
//! symbols resolvable from the global namespace — which they got for
//! free back when libduckdb was a load-command dependency. Windows
//! needs nothing: DLL imports bind by name to a module.

use std::env;
use std::ffi::{CStr, c_char, c_void};
use std::path::PathBuf;
use std::sync::OnceLock;

use duckdb::ffi;
use libloading::Library;

use crate::engine_fill;

#[cfg(target_os = "macos")]
const LIB_NAME: &str = "libduckdb.dylib";
#[cfg(all(unix, not(target_os = "macos")))]
const LIB_NAME: &str = "libduckdb.so";
#[cfg(windows)]
const LIB_NAME: &str = "duckdb.dll";

/// The api struct must outlive the process: the generated init keeps
/// raw pointers out of it, and `get_api` hands its address across FFI.
static API: OnceLock<ffi::duckdb_ext_api_v1> = OnceLock::new();
static LOADED: OnceLock<Result<String, String>> = OnceLock::new();

unsafe extern "C" fn get_api(_: ffi::duckdb_extension_info, _: *const c_char) -> *const c_void {
    match API.get() {
        Some(api) => api as *const ffi::duckdb_ext_api_v1 as *const c_void,
        None => std::ptr::null(),
    }
}

/// Load libduckdb if it is not already loaded. Idempotent and cheap
/// after the first call. Returns the engine's version string.
pub fn ensure_loaded() -> Result<&'static str, String> {
    match LOADED.get_or_init(load) {
        Ok(v) => Ok(v),
        Err(e) => Err(e.clone()),
    }
}

/// Where to look, in order. First hit wins; the bare name at the end
/// lets the system loader's own search have the final say.
fn candidates() -> Vec<PathBuf> {
    let mut c = Vec::new();
    if let Ok(p) = env::var("HARBOR_LIBDUCKDB") {
        c.push(PathBuf::from(p));
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            // Release archive layout: bin/ and lib/ as siblings (unix),
            // the DLL beside the exe (Windows) — and run-in-place.
            c.push(dir.join("../lib").join(LIB_NAME));
            c.push(dir.join(LIB_NAME));
        }
    }
    if let Some(home) = env::home_dir() {
        c.push(home.join(".local/lib").join(LIB_NAME));
        // make fetch-duckdb puts the engine here; prefer the pinned
        // `latest` symlink, then any versioned dir, newest name last
        // so .last() after sort is the highest.
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

fn load() -> Result<String, String> {
    let tried = candidates();
    let (lib, path) = tried
        .iter()
        .filter(|p| p.as_os_str() == LIB_NAME.as_ref() as &std::ffi::OsStr || p.exists())
        .find_map(|p| open_lib(p).ok().map(|l| (l, p.clone())))
        .ok_or_else(|| {
            format!(
                "libduckdb not found (searched: {}) — only needed to serve or open files locally",
                tried.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
            )
        })?;

    let mut api: ffi::duckdb_ext_api_v1 = unsafe { std::mem::zeroed() };
    let n = unsafe { engine_fill::fill(&lib, &mut api) };
    if n == 0 {
        return Err(format!("{}: loaded, but no duckdb symbols in it", path.display()));
    }
    API.set(api).map_err(|_| "engine already initialized".to_string())?;

    let access = ffi::duckdb_extension_access {
        set_error: None,
        get_database: None,
        get_api: Some(get_api),
    };
    // The version string only reaches our own get_api, which ignores it.
    let ok = unsafe { ffi::duckdb_rs_extension_api_init(std::ptr::null_mut(), &access, "harbor") }
        .map_err(|e| format!("engine init: {e}"))?;
    if !ok {
        return Err("engine init: api struct was rejected".to_string());
    }

    // The engine stays for the life of the process — closing it would
    // turn every filled pointer into a dangling one.
    std::mem::forget(lib);

    let ver = unsafe { CStr::from_ptr(ffi::duckdb_library_version()) };
    Ok(ver.to_string_lossy().into_owned())
}
