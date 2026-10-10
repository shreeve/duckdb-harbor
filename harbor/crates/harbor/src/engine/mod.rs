//! The v2 engine — DuckDB's v2 C API, loaded on demand.
//!
//! ffi.rs is generated from DuckDB's api_spec/v2 YAML (scripts/gen-v2-ffi.rb):
//! the whole surface as one dlsym-filled function table. This module is the
//! hand-written rim: find the library, load it once, and give errors and
//! string views a Rust shape.
//!
//! Unix opens the engine RTLD_NOW and, once it proves to serve the v2 API,
//! RTLD_GLOBAL. GLOBAL is load-bearing: DuckDB's own extension loading
//! expects engine symbols resolvable from the global namespace.

/// One fallible v2 call inside a `Result<_, Error>` function. Defined
/// before the modules so both conn and encode see it.
macro_rules! call {
    ($api:expr, $f:ident($($a:expr),*)) => {{
        let api = $api;
        let f = api.$f.ok_or_else(|| Error {
            code: ffi::ERROR_API,
            message: concat!("engine lacks duckdb_v2_", stringify!($f)).to_string(),
        })?;
        let mut err: ffi::error_info_handle = std::ptr::null_mut();
        // Some call sites already sit inside an unsafe block.
        #[allow(unused_unsafe)]
        let code = unsafe { f($($a,)* &mut err) };
        if code != ffi::ERROR_NONE {
            return Err(Error::take(api, code, err));
        }
    }};
}

pub mod conn;
pub mod encode;
pub mod ffi;

use std::env;
use std::fmt;
use std::path::{Path, PathBuf};
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

/// Where to look, in order. The first that loads and serves the v2 API
/// wins. On Linux the bare name at the end lets ld.so's own search have the
/// final say; macOS and Windows would search the working directory for it,
/// so they stop at the paths. `HARBOR_LIBDUCKDB` is not in this list — an
/// explicit override is handled first in `load`, where a miss is a hard
/// error rather than a fall-through.
fn candidates() -> Vec<PathBuf> {
    let mut c = Vec::new();
    if let Ok(exe) = env::current_exe() {
        // macOS names the path that was run, a symlink's included, and the
        // library sits beside the binary itself; Linux resolves it already.
        #[cfg(target_os = "macos")]
        let real = std::fs::canonicalize(&exe).ok().filter(|r| *r != exe);
        #[cfg(not(target_os = "macos"))]
        let real: Option<PathBuf> = None;
        for dir in real.iter().chain([&exe]).filter_map(|e| e.parent()) {
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
            // Numeric-aware: a plain sort puts 1.9.0 above 1.10.0.
            vers.sort_by_key(|p| {
                p.parent()
                    .and_then(|d| d.file_name())
                    .map(|n| {
                        n.to_string_lossy()
                            .split(|c: char| !c.is_ascii_digit())
                            .map(|s| s.parse::<u64>().unwrap_or(0))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            });
            if let Some(newest) = vers.pop() {
                c.push(newest);
            }
        }
    }
    #[cfg(target_os = "linux")]
    c.push(PathBuf::from(LIB_NAME));
    c
}

/// Open a library with its symbols kept to itself, or, `global`, offered to
/// every library loaded after it.
#[cfg(unix)]
fn open_lib(path: &Path, global: bool) -> Result<Library, libloading::Error> {
    use libloading::os::unix::{Library as Unix, RTLD_GLOBAL, RTLD_LOCAL, RTLD_NOW};
    let scope = if global { RTLD_GLOBAL } else { RTLD_LOCAL };
    unsafe { Unix::open(Some(path), RTLD_NOW | scope).map(Into::into) }
}

#[cfg(windows)]
fn open_lib(path: &Path, _global: bool) -> Result<Library, libloading::Error> {
    unsafe { Library::new(path) }
}

fn load() -> Result<Engine, String> {
    // An explicit override is a contract, not a hint: when HARBOR_LIBDUCKDB
    // is set, the engine comes from that file or the load fails naming it.
    // Falling through to the search would silently bind a different
    // libduckdb — in CI, exactly the failure that must be loud. The value is
    // resolved here, so the file opened is the one it names from the working
    // directory and not one the dynamic loader's own search finds.
    if let Ok(p) = env::var("HARBOR_LIBDUCKDB") {
        let path = std::fs::canonicalize(&p).map_err(|e| format!("HARBOR_LIBDUCKDB={p}: {e}"))?;
        return boot(&path).map_err(|e| format!("HARBOR_LIBDUCKDB={p}: {e}"));
    }
    search(&candidates())
}

/// The first of `tried` that boots.
fn search(tried: &[PathBuf]) -> Result<Engine, String> {
    let mut failed = Vec::new();
    for p in tried {
        if p.is_absolute() && !p.exists() {
            continue;
        }
        match boot(p) {
            Ok(engine) => return Ok(engine),
            // A candidate that exists but will not load, or predates the v2
            // API, carries the real story (wrong arch, missing dependency,
            // an old engine): keep it for the error and try the next.
            Err(e) => failed.push(format!("{}: {e}", p.display())),
        }
    }
    let mut msg = format!(
        "libduckdb not found (searched: {})",
        tried.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
    );
    if !failed.is_empty() {
        msg.push_str(&format!("; failed to load: {}", failed.join("; ")));
    }
    Err(msg)
}

/// Load the engine at `path`. It is opened with its symbols kept to itself
/// until it proves to serve the v2 API: a library that does not is closed
/// again, and must not have offered its symbols to the one loaded after it.
fn boot(path: &Path) -> Result<Engine, String> {
    let lib = open_lib(path, false).map_err(|e| e.to_string())?;
    let (api, symbols) = unsafe { ffi::Api::fill(&lib) };
    // Both symbols gate together: an engine odd enough to export one
    // without the other must not reach the unwrap-free calls below.
    let version_fn = match (api.create_environment, api.library_version) {
        (Some(_), Some(f)) => f,
        _ => {
            return Err(format!(
                "engine has no v2 C API ({symbols} v2 symbols) — needs DuckDB v2.0.0 or later"
            ));
        }
    };

    let mut ver = ffi::str_t { ptr: std::ptr::null(), len: 0 };
    let mut err = std::ptr::null_mut();
    let code = unsafe { version_fn(&mut ver, &mut err) };
    if code != ffi::ERROR_NONE {
        return Err(Error::take(&api, code, err).to_string());
    }
    let version = unsafe { str_view(&ver) }.to_owned();

    // Opened again, the same library offers its symbols globally: DuckDB's
    // own extension loading expects engine symbols resolvable from there.
    // Both handles stay for the life of the process — closing the engine
    // would turn every filled pointer into a dangling one.
    let global = open_lib(path, true).map_err(|e| e.to_string())?;
    std::mem::forget(global);
    std::mem::forget(lib);

    Ok(Engine { api, version, path: path.to_path_buf(), symbols })
}

/// A failed v2 call: the structured code plus the engine's rendered text.
#[derive(Debug, Clone)]
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

impl Error {
    /// The client-facing text: the engine's rendered message when there is
    /// one, the structured code when there is not.
    pub fn into_text(self) -> String {
        if self.message.is_empty() { format!("duckdb error {}", self.code) } else { self.message }
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

/// Destroy an engine value.
fn destroy_value(api: &ffi::Api, mut value: ffi::value_handle) {
    if let Some(f) = api.value_destroy {
        unsafe { f(&mut value) };
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A candidate that will not boot, for whatever reason, is reported and
    /// passed over: the engine after it still loads.
    #[test]
    fn the_search_keeps_going_past_a_candidate_that_fails() {
        // Without an engine there is nothing to search for, except where
        // one is promised: CI, or a library named outright.
        let good = match engine() {
            Ok(good) => good,
            Err(e) if ["HARBOR_LIBDUCKDB", "CI"].iter().any(|v| std::env::var_os(v).is_some()) => {
                panic!("no engine: {e}")
            }
            Err(_) => return,
        };
        let dir = std::env::temp_dir().join(format!("harbor-search-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join(LIB_NAME);
        std::fs::write(&bad, b"not a library").unwrap();
        let missing = dir.join("missing").join(LIB_NAME);

        let found = search(&[missing.clone(), bad.clone(), good.path.clone()]).map(|e| e.path);
        let refused = search(&[missing, bad.clone()]).err().unwrap_or_default();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found.unwrap(), good.path);
        assert!(refused.contains(&format!("failed to load: {}", bad.display())), "{refused}");
    }
}
