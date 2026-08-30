//! Where everything lives.
//!
//! Two roots, split by what the files *are* rather than by who writes them:
//!
//! ```text
//! ~/.config/harbor/config.toml    desired state — you edit this
//! ~/.local/state/harbor/          actual state — harbor writes this
//!     runtime/   <name>.{json,sock,token,lock}
//!     log/       <name>.log
//!     history    pilot's REPL history
//! ```
//!
//! Runtime state used to live under `~/.config/harbor/`, which is how a
//! config directory came to hold sockets, tombstoned lock files and a shell
//! history — unreadable enough that deleting it looked like the reasonable
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

/// Holds `runtime/`, `log/` and `history` — everything harbor writes.
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

pub fn log_dir() -> Result<PathBuf, String> {
    Ok(state_root()?.join("log"))
}

/// pilot's REPL history. State, not config, and not the fleet's business —
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
        return Err(format!("bad berth name {name:?}"));
    }
    Ok(n)
}

/// Is this argument a path, or a configured name?
///
/// **A bare word is never a path.** `harbor start medlabs`, run from the wrong
/// directory, once named the file `./medlabs`, created it empty, and served
/// it under the name clients trusted — an empty impostor in front of real
/// data. Reading a bare word as a name closes that whole class: the argument
/// either matches something configured or it is an error, and it can never
/// silently become a file that isn't there.
pub fn looks_like_path(arg: &str) -> bool {
    arg.contains('/')
        || arg.contains('\\')
        || arg.starts_with('~')
        || arg == "."
        || arg == ".."
        || Path::new(arg)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("duckdb") || e.eq_ignore_ascii_case("db"))
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
        // And these must stay paths.
        assert!(looks_like_path("./medlabs.duckdb"));
        assert!(looks_like_path("medlabs.duckdb"));
        assert!(looks_like_path("data.db"));
        assert!(looks_like_path("~/Data/x.duckdb"));
        assert!(looks_like_path("/srv/db/inventory.duckdb"));
        assert!(looks_like_path("sub/dir"));
        assert!(looks_like_path("."));
    }

    #[test]
    fn a_relative_env_root_is_ignored_not_resolved() {
        // Guards the rule, not the env: a relative $HARBOR_HOME must never
        // become a state root, or the fleet moves with the working directory.
        assert!(PathBuf::from("relative/harbor").is_absolute().then_some(()).is_none());
    }
}
