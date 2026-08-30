//! pilot's half of the config.
//!
//! The schema, the roots and the trust check live in `harbor-common`, so the
//! client and the server cannot drift on where config lives or what a berth
//! is called. What stays here is the one thing that must not be shared:
//! [`resolve_token`] runs `token-cmd` through `sh -c`, and harbor has no
//! business linking a code path that shells out for a credential.

pub use harbor_common::config::{Connection, Defaults, FileConfig};
pub use harbor_common::paths::expand;

use std::path::PathBuf;

/// Where the live fleet is on disk. harbor creates and guards it; pilot only
/// reads and writes inside it.
///
/// A `Result`, deliberately. The old version fell back to `"."` when no home
/// variable was set, which meant pilot silently looked for sockets in a
/// *relative* directory and found none — a failure that looked like an empty
/// fleet rather than a broken environment.
pub fn runtime_dir() -> Result<PathBuf, String> {
    harbor_common::runtime_dir()
}

/// The REPL history file, or nothing if there is no home to put it in — in
/// which case the caller keeps history in memory and says so.
pub fn history_file() -> Option<PathBuf> {
    harbor_common::history_file().ok()
}

pub fn load() -> FileConfig {
    harbor_common::config::load_or_empty("pilot")
}

/// flag > env beat the config; within it: token > token-file > token-cmd.
pub fn resolve_token(c: &Connection) -> Option<String> {
    if let Some(t) = &c.token {
        return Some(t.clone());
    }
    if let Some(f) = &c.token_file {
        if let Ok(t) = std::fs::read_to_string(expand(f)) {
            let t = t.trim().to_string();
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    if let Some(cmd) = &c.token_cmd {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(toml_src: &str) -> Connection {
        let c: FileConfig = toml::from_str(toml_src).unwrap();
        c.connection.into_values().next().unwrap()
    }

    #[test]
    fn a_literal_token_beats_a_command_that_would_run() {
        // If precedence ever inverted, this would shell out. It must not.
        let c = conn(
            r#"
            [connection.x]
            url = "https://h"
            token = "literal"
            token-cmd = "exit 1"
            "#,
        );
        assert_eq!(resolve_token(&c), Some("literal".into()));
    }

    #[test]
    fn token_cmd_is_the_last_resort_and_its_output_is_trimmed() {
        let c = conn(
            r#"
            [connection.x]
            url = "https://h"
            token-cmd = "printf '  shelled  \n'"
            "#,
        );
        assert_eq!(resolve_token(&c), Some("shelled".into()));
    }

    #[test]
    fn a_failing_or_empty_token_cmd_yields_nothing() {
        for cmd in ["exit 1", "printf ''", "printf '   '"] {
            let c = conn(&format!("[connection.x]\nurl = \"https://h\"\ntoken-cmd = \"{cmd}\"\n"));
            assert_eq!(resolve_token(&c), None, "{cmd:?}");
        }
    }

    #[test]
    fn nothing_configured_is_not_an_error() {
        let c = conn("[connection.x]\nurl = \"https://h\"\n");
        assert_eq!(resolve_token(&c), None);
    }
}
