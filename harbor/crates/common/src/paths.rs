//! Where everything lives.
//!
//! Two roots, split by what the files *are* rather than by who writes them:
//!
//! ```text
//! ~/.config/harbor/config.toml    desired state — you edit this
//! ~/.local/state/harbor/          actual state — harbor writes this
//!     runtime/<base>-<hash>.sock      a server's listening socket
//!     runtime/<base>-<hash>.args      the options it was started with, for a restart
//!     runtime/log/<base>-<hash>.log   the log of a server started by hand or on use
//!     runtime/log/<name>.log          the log of a server its login item runs
//!     history                         the repl's command history
//! ```
//!
//! Runtime state does not belong under `~/.config/harbor/`: a config
//! directory holding sockets, server logs and a shell history is
//! unreadable enough that deleting it looks like the reasonable
//! move. `~/.local/state` is the XDG home for exactly this: files that
//! accumulate, that you would not back up, and that you are meant to be able
//! to throw away. It is also not `/tmp`, which is swept after three days on
//! macOS and ten by systemd-tmpfiles, and would take a long-running berth's
//! socket with it.
//!
//! `$HARBOR_HOME` collapses both roots into that one directory. That is the
//! self-contained form tests, containers and unit files want, and it is the
//! single escape hatch — there is no per-directory override. It must be an
//! absolute path: a relative or empty one is an error, never a quiet fall
//! back to the real config and fleet.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

const APP: &str = "harbor";

fn home() -> Result<PathBuf, String> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .map_err(|_| "neither $HOME nor %USERPROFILE% is set".to_string())
}

/// An XDG variable read as an absolute path, or nothing.
///
/// Relative values are ignored rather than resolved, as the XDG spec says: a
/// state root that moves with the working directory finds a different fleet
/// depending on where a command was run from.
fn abs_env(var: &str) -> Option<PathBuf> {
    absolute(std::env::var_os(var)?)
}

fn absolute(v: OsString) -> Option<PathBuf> {
    let p = PathBuf::from(v);
    p.is_absolute().then_some(p)
}

/// The one override: everything under a single directory. Set to anything
/// but an absolute path it is an error: whoever set it meant to keep harbor
/// away from the real config and fleet, and ignoring it does the opposite.
pub fn harbor_home() -> Result<Option<PathBuf>, String> {
    home_from(std::env::var_os("HARBOR_HOME"))
}

fn home_from(v: Option<OsString>) -> Result<Option<PathBuf>, String> {
    let Some(v) = v else { return Ok(None) };
    let shown = v.to_string_lossy().into_owned();
    absolute(v)
        .map(Some)
        .ok_or_else(|| format!("HARBOR_HOME must be an absolute path, not {shown:?}"))
}

/// Holds `config.toml`, and nothing else.
pub fn config_root() -> Result<PathBuf, String> {
    if let Some(h) = harbor_home()? {
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
    if let Some(h) = harbor_home()? {
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

/// Listening sockets and server logs.
pub fn runtime_dir() -> Result<PathBuf, String> {
    Ok(state_root()?.join("runtime"))
}

/// The one true socket for a database file — identity derived, never
/// registered. The canonical path (see [`canonical_db`]) is hashed so every
/// spelling of the same file lands on the same server, and two `data.duckdb`
/// in different directories never fight over one socket. The basename keeps
/// `ls` readable; the hash carries uniqueness; the full path cannot be the
/// name because sun_path is ~104 bytes on macOS. FNV-1a, hand-rolled,
/// because the name must be STABLE across releases — a 0.20.1 must find a
/// 0.20.0's socket — and std's hasher is not.
pub fn socket_for(runtime: &Path, db: &Path) -> Result<PathBuf, String> {
    socket_named(runtime, &canonical_db(db)?)
}

/// The runtime directory, the canonical database, and its socket: what a
/// verb that dials or serves a database needs, each resolved once.
pub fn socket_of(db: &Path) -> Result<(PathBuf, PathBuf, PathBuf), String> {
    let runtime = runtime_dir()?;
    let canon = canonical_db(db)?;
    let sock = socket_named(&runtime, &canon)?;
    Ok((runtime, canon, sock))
}

fn socket_named(runtime: &Path, canon: &Path) -> Result<PathBuf, String> {
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
    let mut cut = base.len().min(40).min(budget);
    while !base.is_char_boundary(cut) {
        cut -= 1;
    }
    Ok(runtime.join(format!("{}-{h:08x}.sock", &base[..cut], h = h as u32)))
}

/// The canonical identity of a database path: symlinks resolved and
/// absolutized. A not-yet-created file canonicalizes its parent and keeps
/// its own name. A link to a file not made yet resolves to that file: DuckDB
/// opens through the link and creates its target, and the target is what
/// every later spelling canonicalizes to.
pub fn canonical_db(db: &Path) -> Result<PathBuf, String> {
    let mut db = db.to_path_buf();
    for _ in 0..40 {
        if let Ok(c) = db.canonicalize() {
            return Ok(c);
        }
        match std::fs::read_link(&db) {
            // Relative to the link's own directory; an absolute target replaces it.
            Ok(target) => db = db.parent().unwrap_or(Path::new("")).join(target),
            Err(_) => break,
        }
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

/// Render a path for people. Keep the native canonical path for file access
/// and identity: Windows' verbatim prefix preserves long paths and names.
pub fn display_path(path: &Path) -> String {
    let text = path.display().to_string();
    #[cfg(windows)]
    {
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{rest}");
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return rest.to_string();
        }
    }
    text
}

/// A server's log: a login item's, named for its database, or a server's
/// started by hand or on use, named for its socket.
pub fn log_file(runtime: &Path, name: &str) -> PathBuf {
    runtime.join("log").join(format!("{name}.log"))
}

/// The repl's command history. State, not config, and not the fleet's
/// business, so it sits beside `runtime/` and no sweep of that has to
/// step around it.
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
    let s = display_path(p);
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
    #[cfg(windows)]
    #[test]
    fn windows_display_hides_prefix_without_changing_path() {
        for (native, shown) in [
            (r"\\?\C:\d\hb\hb.db", r"C:\d\hb\hb.db"),
            (r"\\?\UNC\nas\share\hb.db", r"\\nas\share\hb.db"),
        ] {
            let path = PathBuf::from(native);
            assert_eq!(display_path(&path), shown);
            assert_eq!(path, PathBuf::from(native));
        }
    }

    #[test]
    fn canonical_database_preserves_native_path_for_existing_and_new_files() {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("hb-path-{}-{stamp}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let native_root = root.canonicalize().unwrap();
        // A native Windows path beyond MAX_PATH must still reach the file.
        let mut deep = native_root.clone();
        for _ in 0..16 { deep.push("long-path-component"); }
        std::fs::create_dir_all(&deep).unwrap();
        let db = deep.join("existing.duckdb");
        std::fs::write(&db, b"path probe").unwrap();
        let canonical = canonical_db(&db).unwrap();
        assert_eq!(canonical, db.canonicalize().unwrap());
        assert_eq!(std::fs::read(&canonical).unwrap(), b"path probe");
        assert_eq!(canonical_db(&deep.join("new.duckdb")).unwrap(), deep.canonicalize().unwrap().join("new.duckdb"));
        #[cfg(windows)]
        assert!(canonical.to_str().unwrap().starts_with(r"\\?\"));
        std::fs::remove_dir_all(native_root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unix_display_preserves_backslashes() {
        assert_eq!(display_path(Path::new(r"\\?\C:\literal")), r"\\?\C:\literal");
    }

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
    fn harbor_home_is_absolute_or_an_error() {
        assert_eq!(home_from(None), Ok(None));
        assert_eq!(home_from(Some("/tmp/h".into())), Ok(Some(PathBuf::from("/tmp/h"))));
        // Relative or empty, it would quietly mean the real fleet.
        for bad in ["relative/harbor", ""] {
            let e = home_from(Some(bad.into())).unwrap_err();
            assert!(e.contains("absolute"), "{e}");
        }
        // The XDG variables keep the spec's rule: a relative one is ignored.
        assert_eq!(absolute("relative/state".into()), None);
        assert_eq!(absolute("/srv/state".into()), Some(PathBuf::from("/srv/state")));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_a_file_not_made_yet_is_its_target() {
        let root = std::env::temp_dir().join(format!("hb-dangle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("data")).unwrap();
        let root = root.canonicalize().unwrap();
        std::os::unix::fs::symlink("data/target.duckdb", root.join("dangle.duckdb")).unwrap();
        let want = root.join("data/target.duckdb");
        assert_eq!(canonical_db(&root.join("dangle.duckdb")).unwrap(), want);
        // Once made, canonicalize says the same.
        std::fs::write(&want, b"").unwrap();
        assert_eq!(canonical_db(&root.join("dangle.duckdb")).unwrap(), want);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
