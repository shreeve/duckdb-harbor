//! The client address book: $HARBOR_HOME/config.toml (PLAN.md D10a).
//!
//! Purely additive — zero-config local always works. The config only teaches
//! pilot what the filesystem cannot reveal: remote urls, their credentials
//! (never on argv: token-file or token-cmd), spawn aliases, and taste.
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
//! token-file = "~/.harbor/medlabs.token"     # or token-cmd = "op read ..."
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

/// The harbor registry directory: $HARBOR_HOME, else ~/.harbor. This is
/// where pilot's world lives on disk — sockets, tokens, history, config.
pub fn harbor_home() -> std::path::PathBuf {
    if let Ok(h) = std::env::var("HARBOR_HOME") {
        return std::path::PathBuf::from(h);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::Path::new(&home).join(".harbor")
}

pub fn load() -> FileConfig {
    let path = harbor_home().join("config.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return FileConfig::default();
    };
    match toml::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pilot: {} is not valid: {e}", path.display());
            FileConfig::default()
        }
    }
}

/// `~/` expansion, nothing fancier.
pub fn expand(p: &str) -> std::path::PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(h) = std::env::var("HOME") {
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
            token-file = "~/.harbor/medlabs.token"

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
}
