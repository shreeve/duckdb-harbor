//! Token resolution, pilot's rules exactly.
//!
//! `token-cmd` runs through `sh -c`, which is why this stays in client
//! crates: harbor-common is linked by the server, and the server has no
//! business holding a code path that shells out for a credential. The
//! config file's trust check (refuse a file anyone else can write) is
//! enforced by `harbor_common::config::load`, before any of this runs.

use harbor_common::config::Connection;
use harbor_common::paths::expand;

/// Precedence within an entry: token > token-file > token-cmd. Callers put
/// `HARBOR_TOKEN` ahead of all three, the way pilot's flag and env do.
pub fn resolve(c: &Connection) -> Option<String> {
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
    use harbor_common::config::FileConfig;

    fn conn(toml_src: &str) -> Connection {
        let c: FileConfig = toml::from_str(toml_src).unwrap();
        c.connection.into_values().next().unwrap()
    }

    #[test]
    fn a_literal_token_beats_a_command_that_would_run() {
        let c = conn(
            r#"
            [connection.x]
            url = "http://h"
            token = "literal"
            token-cmd = "exit 1"
            "#,
        );
        assert_eq!(resolve(&c), Some("literal".into()));
    }

    #[test]
    fn a_failing_command_yields_nothing() {
        let c = conn(
            r#"
            [connection.x]
            url = "http://h"
            token-cmd = "exit 1"
            "#,
        );
        assert_eq!(resolve(&c), None);
    }

    #[test]
    fn command_output_is_trimmed() {
        let c = conn(
            r#"
            [connection.x]
            url = "http://h"
            token-cmd = "echo '  tok  '"
            "#,
        );
        assert_eq!(resolve(&c), Some("tok".into()));
    }
}
