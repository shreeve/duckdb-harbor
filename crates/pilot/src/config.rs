//! The client address book: $HARBOR_HOME/config.toml (PLAN.md D10a).
//!
//! Purely additive — zero-config local always works. The config only teaches
//! pilot what the filesystem cannot reveal: remote urls, their credentials
//! (never on argv: token-file or token-cmd), spawn aliases, and taste.
//!
//! `token-cmd` is run through `sh -c`, so the file is only read when it — and
//! the directory holding it — is yours and unwritable by anyone else.
//!
//! ```toml
//! [defaults]
//! mode = "duckbox"
//! timer = true
//! theme = "duck"           # duck | mono | vivid
//! appearance = "auto"      # auto | light | dark
//!
//! [connection.medlabs]
//! url = "http://127.0.0.1:9495"
//! token-file = "~/.config/harbor/runtime/medlabs.token"     # or token-cmd = "op read ..."
//!
//! [connection.scratch]
//! path = "~/Data/scratch.duckdb"             # spawn-on-demand alias (D9)
//! idle-exit = "10m"
//! ```

use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub connection: HashMap<String, Connection>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Defaults {
    pub mode: Option<String>,
    pub timer: Option<bool>,
    pub maxrows: Option<usize>,
    pub nullvalue: Option<String>,
    /// Syntax-highlight theme name: duck | mono | vivid (default duck).
    pub theme: Option<String>,
    /// Palette to build the theme for: auto | light | dark (default auto,
    /// which asks the terminal for its background color).
    pub appearance: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Connection {
    pub url: Option<String>,
    pub path: Option<String>,
    pub token: Option<String>,
    pub token_file: Option<String>,
    pub token_cmd: Option<String>,
    pub idle_exit: Option<String>,
}

impl Connection {
    /// flag > env beat the config; within it: token > token-file > token-cmd.
    pub fn resolve_token(&self) -> Option<String> {
        if let Some(t) = &self.token {
            return Some(t.clone());
        }
        if let Some(f) = &self.token_file {
            if let Ok(t) = std::fs::read_to_string(expand(f)) {
                let t = t.trim().to_string();
                if !t.is_empty() {
                    return Some(t);
                }
            }
        }
        if let Some(cmd) = &self.token_cmd {
            let out = std::process::Command::new("sh").arg("-c").arg(cmd).output().ok()?;
            if out.status.success() {
                let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !t.is_empty() {
                    return Some(t);
                }
            }
        }
        None
    }
}

/// The config root: $HARBOR_HOME, else ~/.config/harbor. Holds config.toml
/// and the runtime/ dir.
pub fn config_root() -> std::path::PathBuf {
    if let Ok(h) = std::env::var("HARBOR_HOME") {
        return std::path::PathBuf::from(h);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    std::path::Path::new(&home).join(".config").join("harbor")
}

/// The runtime dir: $HARBOR_HOME/runtime — where the live fleet lives on
/// disk (sockets, sidecars, tokens, history). harbor creates and guards it;
/// pilot only reads and writes inside it.
pub fn harbor_home() -> std::path::PathBuf {
    config_root().join("runtime")
}

pub fn load() -> FileConfig {
    load_from(&config_root())
}

fn load_from(root: &std::path::Path) -> FileConfig {
    let path = root.join("config.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return FileConfig::default();
    };
    // Anyone who can rewrite this file is not editing settings, they are
    // writing a program: `token-cmd` runs through `sh -c`, and `url` decides
    // who receives the bearer token. So refuse it whole, the way ssh refuses
    // a loose identity file, rather than trust it in part.
    if let Some(bad) = [&path, &root.to_path_buf()].into_iter().find(|p| exposed(p)) {
        let who = match *bad == path {
            true => "it is".to_string(),
            false => format!("{} is", bad.display()),
        };
        eprintln!(
            "pilot: ignoring {} — {who} writable by others or not yours (chmod go-w it)",
            path.display()
        );
        return FileConfig::default();
    }
    match toml::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pilot: {} is not valid: {e}", path.display());
            FileConfig::default()
        }
    }
}

/// True if someone other than us could swap this path's contents out from
/// under us: group- or world-writable, or owned by another user. A sticky
/// directory (/tmp) is writable by all but hijackable by none, so it passes.
#[cfg(unix)]
fn exposed(path: &std::path::Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Ok(md) = std::fs::metadata(path) else {
        return false;
    };
    let mode = md.permissions().mode();
    let sticky = md.is_dir() && mode & 0o1000 != 0;
    let uid = unsafe { libc::getuid() };
    (mode & 0o022 != 0 && !sticky) || (md.uid() != uid && md.uid() != 0)
}

#[cfg(not(unix))]
fn exposed(_path: &std::path::Path) -> bool {
    false
}

/// `~/` expansion, nothing fancier.
pub fn expand(p: &str) -> std::path::PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(h) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
            return std::path::Path::new(&h).join(rest);
        }
    }
    std::path::PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_shape() {
        let c: FileConfig = toml::from_str(
            r#"
            [defaults]
            mode = "csv"
            timer = true

            [connection.medlabs]
            url = "http://127.0.0.1:9495"
            token-file = "~/.config/harbor/runtime/medlabs.token"

            [connection.scratch]
            path = "~/Data/scratch.duckdb"
            idle-exit = "10m"
            "#,
        )
        .unwrap();
        assert_eq!(c.defaults.mode.as_deref(), Some("csv"));
        assert_eq!(c.connection["medlabs"].url.as_deref(), Some("http://127.0.0.1:9495"));
        assert_eq!(c.connection["scratch"].idle_exit.as_deref(), Some("10m"));
    }

    #[test]
    fn empty_config_is_fine() {
        let c: FileConfig = toml::from_str("").unwrap();
        assert!(c.connection.is_empty());
    }

    #[cfg(unix)]
    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("pilot-cfg-{}-{tag}", std::process::id()));
        std::fs::remove_dir_all(&d).ok();
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("config.toml"),
            "[connection.x]\nurl = \"http://127.0.0.1:1\"\ntoken-cmd = \"echo pwned\"\n",
        )
        .unwrap();
        d
    }

    #[cfg(unix)]
    fn chmod(p: &std::path::Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn a_config_only_its_owner_can_write_is_used() {
        let d = scratch("ok");
        chmod(&d, 0o755);
        chmod(&d.join("config.toml"), 0o644);
        assert!(load_from(&d).connection.contains_key("x"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    #[cfg(unix)]
    fn a_group_writable_config_is_refused_whole() {
        let d = scratch("file");
        chmod(&d, 0o755);
        chmod(&d.join("config.toml"), 0o664);
        // Not merely token-cmd: the whole file, url and all, is untrusted.
        assert!(load_from(&d).connection.is_empty());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    #[cfg(unix)]
    fn a_config_in_a_writable_directory_is_refused() {
        // The file itself is tight, but the directory lets anyone replace it.
        let d = scratch("dir");
        chmod(&d.join("config.toml"), 0o600);
        chmod(&d, 0o777);
        assert!(load_from(&d).connection.is_empty());
        chmod(&d, 0o755);
        std::fs::remove_dir_all(&d).ok();
    }
}
