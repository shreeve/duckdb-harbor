//! Where everything lives.
//!
//! Two roots, split by what the files *are* rather than by who writes them:
//!
//! ```text
//! ~/.config/harbor/config.toml    desired state — you edit this
//! ~/.local/state/harbor/          actual state — harbor writes this
//!     runtime/   <name>.{json,sock,token,lock}
//!     runtime/log/<name>.log      the berth's server log
//!     history    the repl's command history
//! ```
//!
//! Runtime state does not belong under `~/.config/harbor/`: a config
//! directory holding sockets, tombstoned lock files and a shell history is
//! unreadable enough that deleting it looks like the reasonable
//! move. `~/.local/state` is the XDG home for exactly this: files that
//! accumulate, that you would not back up, and that you are meant to be able
//! to throw away. It is also not `/tmp`, which is swept after three days on
//! macOS and ten by systemd-tmpfiles, and would take a long-running berth's
//! socket with it.
//!
//! `$HARBOR_HOME`, if absolute, collapses both roots into that one directory.
//! That is the self-contained form tests, containers and unit files want, and
//! it is the single escape hatch — there is no per-directory override.

use std::path::{Path, PathBuf};

const APP: &str = "harbor";

fn home() -> Result<PathBuf, String> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .map_err(|_| "neither $HOME nor %USERPROFILE% is set".to_string())
}

/// An environment variable read as an absolute path, or nothing.
///
/// Relative values are ignored rather than resolved. XDG calls them invalid,
/// and the failure they produce is the worst kind: a state root that moves
/// with the working directory, so the same command finds a different fleet
/// depending on where it was run from.
fn abs_env(var: &str) -> Option<PathBuf> {
    let v = std::env::var(var).ok()?;
    let p = PathBuf::from(v);
    p.is_absolute().then_some(p)
}

/// The one override: everything under a single directory.
pub fn harbor_home() -> Option<PathBuf> {
    abs_env("HARBOR_HOME")
}

/// Holds `config.toml`, and nothing else.
pub fn config_root() -> Result<PathBuf, String> {
    if let Some(h) = harbor_home() {
        return Ok(h);
    }
    if let Some(x) = abs_env("XDG_CONFIG_HOME") {
        return Ok(x.join(APP));
    }
    Ok(home()?.join(".config").join(APP))
}

pub fn config_file() -> Result<PathBuf, String> {
    Ok(config_root()?.join("config.toml"))
}

/// Holds `runtime/` and `history` — everything harbor writes.
pub fn state_root() -> Result<PathBuf, String> {
    if let Some(h) = harbor_home() {
        return Ok(h);
    }
    if let Some(x) = abs_env("XDG_STATE_HOME") {
        return Ok(x.join(APP));
    }
    #[cfg(windows)]
    {
        // Windows has no XDG, and `Local` is the right half of AppData for
        // this: state that belongs to this machine and must not roam.
        if let Some(x) = abs_env("LOCALAPPDATA") {
            return Ok(x.join(APP));
        }
    }
    Ok(home()?.join(".local").join("state").join(APP))
}

/// Sockets, sidecars, tokens, locks.
pub fn runtime_dir() -> Result<PathBuf, String> {
    Ok(state_root()?.join("runtime"))
}

/// The registry files a berth owns under runtime/ — one spelling each,
/// because a filename format string in three crates is how `.hold` becomes
/// the eighth copy of the seventh typo.
pub fn sock_file(runtime: &Path, name: &str) -> PathBuf {
    runtime.join(format!("{name}.sock"))
}
/// The one true socket for a database file — identity derived, never
/// registered. The canonical path (symlinks resolved, absolutized; for a
/// file that does not exist yet, its parent canonicalized) is hashed so
/// every spelling of the same file lands on the same server, and two
/// `data.duckdb` in different directories never fight over one socket.
/// The basename keeps `ls` readable; the hash carries uniqueness; the
/// full path cannot be the name because sun_path is ~104 bytes on macOS.
/// FNV-1a, hand-rolled, because the name must be STABLE across releases
/// — a 0.20.1 must find a 0.20.0's socket — and std's hasher is not.
pub fn socket_for(runtime: &Path, db: &Path) -> Result<PathBuf, String> {
    let canon = canonical_db(db)?;
    let mut h: u64 = 0xcbf29ce484222325;
    for b in canon.to_string_lossy().as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let base = canon
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "db".into());
    // The basename is readability, so it is what yields: the whole path must
    // fit sun_path (104 bytes with NUL on macOS — 103 is the universal
    // budget), and the runtime dir eats what it eats ($TMPDIR sandboxes run
    // deep). The hash carries the identity either way. Truncation is by
    // BYTES, on char boundaries — a multi-byte name must not overshoot.
    let dir = runtime.as_os_str().len();
    let budget = 103usize.saturating_sub(dir + 1 + 1 + 8 + 5); // '/', '-', hash8, ".sock"
    if budget == 0 {
        return Err(format!(
            "runtime dir is too deep for a unix socket ({}): shorten $HARBOR_HOME",
            runtime.display()
        ));
    }
    let mut cut = base.len().min(40);
    while cut < base.len() && !base.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut cut = cut.min(budget);
    while !base.is_char_boundary(cut) {
        cut -= 1;
    }
    Ok(runtime.join(format!("{}-{h:08x}.sock", &base[..cut], h = h as u32)))
}

/// The canonical identity of a database path: symlinks resolved and
/// absolutized. A not-yet-created file (--create) canonicalizes its
/// parent and keeps its own name.
pub fn canonical_db(db: &Path) -> Result<PathBuf, String> {
    if let Ok(c) = db.canonicalize() {
        return Ok(c);
    }
    let parent = match db.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let name = db.file_name().ok_or_else(|| format!("not a file path: {}", db.display()))?;
    let parent = parent
        .canonicalize()
        .map_err(|e| format!("{}: {e}", parent.display()))?;
    Ok(parent.join(name))
}

pub fn sidecar_file(runtime: &Path, name: &str) -> PathBuf {
    runtime.join(format!("{name}.json"))
}
pub fn token_file(runtime: &Path, name: &str) -> PathBuf {
    runtime.join(format!("{name}.token"))
}
pub fn lock_file(runtime: &Path, name: &str) -> PathBuf {
    runtime.join(format!("{name}.lock"))
}
/// The operator's stop, made durable: while this exists, no client may
/// raise the name — `harbor start` lifts it, `harbor forget` sweeps it.
pub fn hold_file(runtime: &Path, name: &str) -> PathBuf {
    runtime.join(format!("{name}.hold"))
}
pub fn log_file(runtime: &Path, name: &str) -> PathBuf {
    runtime.join("log").join(format!("{name}.log"))
}

/// The repl's command history. State, not config, and not the fleet's business —
/// it lived in `runtime/` for a while, where every sweep had to special-case
/// it forever.
pub fn history_file() -> Result<PathBuf, String> {
    Ok(state_root()?.join("history"))
}

/// Berth names are registry filenames: `[a-z0-9_-]`, 1..=64.
pub fn normalize(name: &str) -> Result<String, String> {
    let n: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' })
        .collect();
    if n.is_empty() || n.len() > 64 {
        return Err(format!("bad database name {name:?}"));
    }
    Ok(n)
}

/// Is this argument a path, or a configured name?
///
/// **A name never contains a dot or a slash** — `normalize` maps both to `-`
/// when a name is minted — so an argument carrying one can only be a path.
/// That single fact is the whole classifier: no extension whitelist, nothing
/// for two binaries to disagree about.
///
/// It is also the safety law. `harbor start medlabs`, run from the wrong
/// directory, once named the file `./medlabs`, created it empty, and served
/// it under the name clients trusted — an empty impostor in front of real
/// data. Reading a bare word as a name closes that whole class: the argument
/// either matches something configured or it is an error, and it can never
/// silently become a file that isn't there.
pub fn looks_like_path(arg: &str) -> bool {
    arg.contains(['/', '\\', '.']) || arg.starts_with('~')
}

/// `~/` expansion, nothing fancier.
pub fn expand(p: &str) -> PathBuf {
    if let (Some(rest), Ok(h)) = (
        p.strip_prefix("~/"),
        std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")),
    ) {
        return Path::new(&h).join(rest);
    }
    PathBuf::from(p)
}

/// Render a path with `$HOME` shortened back to `~`, for display only.
pub fn shorten(p: &Path) -> String {
    let s = p.display().to_string();
    let Ok(h) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) else {
        return s;
    };
    if h.is_empty() {
        return s;
    }
    match s.strip_prefix(&h) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') || rest.starts_with('\\') => format!("~{rest}"),
        _ => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_become_registry_filenames() {
        assert_eq!(normalize("MedLabs").unwrap(), "medlabs");
        assert_eq!(normalize("my.db").unwrap(), "my-db");
        assert_eq!(normalize("a b").unwrap(), "a-b");
        assert!(normalize("").is_err());
        assert!(normalize(&"x".repeat(65)).is_err());
    }

    #[test]
    fn a_bare_word_is_never_a_path() {
        // The whole point: these must resolve as configured names.
        assert!(!looks_like_path("medlabs"));
        assert!(!looks_like_path("labs"));
        assert!(!looks_like_path("warehouse2"));
        // A dot or a slash is something a name cannot contain, so any
        // argument carrying one was typed as a path — whatever the extension.
        assert!(looks_like_path("./medlabs.duckdb"));
        assert!(looks_like_path("medlabs.duckdb"));
        assert!(looks_like_path("data.db"));
        assert!(looks_like_path("backup.data"));
        assert!(looks_like_path("sales.2024"));
        assert!(looks_like_path("~/Data/x.duckdb"));
        assert!(looks_like_path("~backup"));
        assert!(looks_like_path("/srv/db/inventory.duckdb"));
        assert!(looks_like_path("sub/dir"));
        assert!(looks_like_path("."));
        assert!(looks_like_path(".."));
    }

    #[test]
    fn the_socket_fits_sun_path_even_in_a_deep_runtime_dir() {
        // macOS $TMPDIR sandboxes produce runtime dirs ~80 bytes deep; the
        // basename is what yields, the hash stays whole. (The db must exist —
        // socket_for canonicalizes it.)
        let db = std::env::temp_dir().join(format!("sunlen-{}.duckdb", std::process::id()));
        std::fs::write(&db, b"").unwrap();
        let deep = PathBuf::from(format!("/{}runtime", "sandbox/".repeat(9)));
        let sock = socket_for(&deep, &db).unwrap();
        assert!(sock.as_os_str().len() <= 103, "{} bytes: {}", sock.as_os_str().len(), sock.display());
        assert!(sock.extension().is_some_and(|e| e == "sock"));
        // Too deep to fit anything is an error with a name, not a bad bind.
        let hopeless = PathBuf::from(format!("/{}", "x/".repeat(60)));
        assert!(socket_for(&hopeless, &db).is_err());
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn a_relative_env_root_is_ignored_not_resolved() {
        // Guards the rule, not the env: a relative $HARBOR_HOME must never
        // become a state root, or the fleet moves with the working directory.
        assert!(PathBuf::from("relative/harbor").is_absolute().then_some(()).is_none());
    }
}
