//! `~/.config/harbor/config.toml` — one file, read by both binaries.
//!
//! One namespace, so "where do I configure medlabs?" has one answer. What
//! kind of thing an entry is follows from which key it sets:
//!
//! ```toml
//! [defaults]
//! mode      = "duckbox"     # the client's taste
//!
//! [connection.medlabs]      # has `path` -> a local berth, harbor can start it
//! path = "~/Data/Code/medlabs/api/db/medlabs.duckdb"
//! memory-limit = "8GB"      # typed tuning fields mirror `harbor start` flags
//! init = ["INSTALL ui", "LOAD ui"]   # any boot SQL: extensions, secrets, SET
//!
//! [connection.medlabs.settings]      # any DuckDB option -> SET <key> = <value>
//! enable_progress_bar = true         # keys are DuckDB's own, passed through
//! default_null_order  = "NULLS LAST" # verbatim (string quoted, bool/int bare)
//!
//! [connection.local]        # localhost -> connect directly over IPv4
//! url = "http://localhost:9495"
//!
//! [connection.warehouse]    # another host -> DuckTable owns the SSH tunnel
//! url = "http://warehouse.example.com:9495"
//! ```
//!
//! Harbor reads this file too: a berth's entry supplies its standing
//! settings — memory, threads, boot SQL, extensions — so starting it honors
//! them without flags. A config anyone else can write is refused whole
//! before any of it is read, since a berth's `init` runs SQL and `LOAD`
//! runs code.
//!
//! Every berth key that harbor acts on is the matching `harbor start` flag
//! with the dashes stripped, so there is no second dialect to learn.
//! (`port` has a key, but only an explicit start honors it; a summon stays
//! on the unix socket.) Every door is machine-local; remote policy belongs
//! to the edge proxy in front.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Deserialize, Default, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub connection: HashMap<String, Connection>,
}

#[derive(Deserialize, Default, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Defaults {
    // --- the client: how output looks --------------------------------------
    pub mode: Option<String>,
    pub timer: Option<bool>,
    pub maxrows: Option<usize>,
    pub nullvalue: Option<String>,
    /// duck | mono | vivid
    pub theme: Option<String>,
    /// auto | light | dark — `auto` asks the terminal for its background.
    pub appearance: Option<String>,
    /// auto | always | never. `NO_COLOR` in the environment beats all three.
    pub color: Option<String>,
    /// What the bare client opens. Unset, it lists what is openable
    /// instead — deliberately, rather than connecting to the only berth when
    /// there happens to be one, which would make adding a second database
    /// silently change what the command does.
    pub connection: Option<String>,

    // --- harbor: how a berth is started ------------------------------------
    pub memory_limit: Option<String>,
    pub workers: Option<usize>,
    pub threads: Option<usize>,
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Connection {
    // --- a remote: client only ---------------------------------------------
    pub url: Option<String>,

    // --- a local berth: harbor starts it, a client may summon it -----------
    pub path: Option<String>,
    pub memory_limit: Option<String>,
    pub threads: Option<usize>,
    pub workers: Option<usize>,
    pub statement_timeout: Option<String>,
    pub max_temp_size: Option<String>,
    pub sealed: Option<bool>,
    pub unsigned: Option<bool>,
    pub log: Option<bool>,
    pub init: Option<Vec<String>>,
    /// The TCP door (always loopback), honored only by an explicit
    /// `harbor <db> start` — a summon ignores it and stays on the unix
    /// socket, so opening a database never silently opens its TCP door.
    pub port: Option<u16>,
    /// Arbitrary DuckDB settings, each applied as `SET key = value` at start.
    /// The escape hatch for any option harbor has no typed field for — the
    /// keys are DuckDB's, not harbor's, so harbor passes them through without
    /// knowing them. Run after `init`, so a setting can tune an extension the
    /// entry just loaded.
    pub settings: Option<HashMap<String, toml::Value>>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Kind {
    /// `path` — a database file on this machine.
    Berth,
    /// `url` — somebody else's server.
    Remote,
    /// Neither, or both.
    Malformed,
}

impl Connection {
    pub fn kind(&self) -> Kind {
        match (self.path.is_some(), self.url.is_some()) {
            (true, false) => Kind::Berth,
            (false, true) => Kind::Remote,
            _ => Kind::Malformed,
        }
    }

    pub fn is_berth(&self) -> bool {
        self.kind() == Kind::Berth
    }

    /// The database file, expanded. `None` unless this is a berth.
    pub fn database(&self) -> Option<PathBuf> {
        self.path.as_deref().map(crate::paths::expand)
    }

    /// The `[connection.*.settings]` block rendered as `SET key = value`
    /// statements, in a stable (sorted) order so a start is reproducible.
    /// Each value's TOML type maps to a SQL literal; a non-scalar value
    /// (array, table) has no SQL setting form and is skipped.
    pub fn setting_statements(&self) -> Vec<String> {
        let Some(settings) = &self.settings else { return Vec::new() };
        let mut keys: Vec<&String> = settings.keys().collect();
        keys.sort();
        keys.into_iter()
            .filter_map(|k| sql_literal(&settings[k]).map(|lit| format!("SET {k} = {lit}")))
            .collect()
    }
}

/// A scalar TOML value as a DuckDB SQL literal: strings single-quoted (with
/// `'` doubled), numbers and booleans bare. Non-scalars have no literal form.
fn sql_literal(v: &toml::Value) -> Option<String> {
    match v {
        toml::Value::String(s) => Some(format!("'{}'", s.replace('\'', "''"))),
        toml::Value::Integer(n) => Some(n.to_string()),
        toml::Value::Float(f) => Some(f.to_string()),
        toml::Value::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

impl FileConfig {
    /// Configured local berths, sorted. The name is the section key — never
    /// the database file's stem, which is how `[connection.warehouse]`
    /// pointing at `inventory.duckdb` used to produce a berth called
    /// `inventory` and an alias nobody could use.
    pub fn berths(&self) -> Vec<(&str, &Connection)> {
        let mut v: Vec<_> = self
            .connection
            .iter()
            .filter(|(_, c)| c.is_berth())
            .map(|(k, c)| (k.as_str(), c))
            .collect();
        v.sort_by_key(|(k, _)| *k);
        v
    }

    pub fn remotes(&self) -> Vec<(&str, &Connection)> {
        let mut v: Vec<_> = self
            .connection
            .iter()
            .filter(|(_, c)| c.kind() == Kind::Remote)
            .map(|(k, c)| (k.as_str(), c))
            .collect();
        v.sort_by_key(|(k, _)| *k);
        v
    }

    pub fn get(&self, name: &str) -> Option<&Connection> {
        self.connection.get(name)
    }

    /// Entries that set neither `path` nor `url`, or both. Reported rather
    /// than guessed at: an entry with both is a question only its author can
    /// answer, and picking one silently is how you query the wrong database.
    pub fn malformed(&self) -> Vec<&str> {
        let mut v: Vec<_> = self
            .connection
            .iter()
            .filter(|(_, c)| c.kind() == Kind::Malformed)
            .map(|(k, _)| k.as_str())
            .collect();
        v.sort_unstable();
        v
    }
}

#[derive(Debug)]
pub enum Error {
    /// The optional config is absent. Normal: zero-config local always works.
    Missing(PathBuf),
    /// The file, or the directory holding it, is writable by someone else.
    Refused { file: PathBuf, offender: PathBuf },
    Invalid { file: PathBuf, why: String },
    NoHome(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Missing(p) => write!(f, "no config file at {}", p.display()),
            Error::Refused { file, offender } => write!(
                f,
                "ignoring {} — {} is writable by others or not yours (chmod go-w it)",
                file.display(),
                offender.display()
            ),
            Error::Invalid { file, why } => write!(f, "{} is not valid: {why}", file.display()),
            Error::NoHome(e) => write!(f, "{e}"),
        }
    }
}

/// Schema-check a candidate config text — the gate a writer runs BEFORE the
/// bytes land, so an edit can never leave behind a file `load` will refuse.
pub fn parse(text: &str) -> Result<FileConfig, String> {
    toml::from_str(text).map_err(|e| e.to_string()).and_then(normalize_keys)
}

/// The name law, applied at the door: a section key is the operator's
/// spelling, but every lookup and every registry file speaks the normalized
/// form — so the map is keyed by it, and `[connection.MedLabs]` answers
/// `harbor start medlabs` instead of being unreachable prose. Two spellings
/// that collapse to one name are refused whole: they would silently shadow
/// each other, and which one wins is a question only the author can answer.
fn normalize_keys(mut cfg: FileConfig) -> Result<FileConfig, String> {
    let mut out = HashMap::new();
    for (key, conn) in std::mem::take(&mut cfg.connection) {
        let name = crate::paths::normalize(&key)?;
        if out.insert(name.clone(), conn).is_some() {
            return Err(format!(
                "[connection.{key}] collides with another entry — both normalize to {name:?}"
            ));
        }
    }
    cfg.connection = out;
    Ok(cfg)
}

/// Read and parse the config, or say precisely why not.
pub fn load() -> Result<FileConfig, Error> {
    let root = crate::paths::config_root().map_err(Error::NoHome)?;
    let file = root.join("config.toml");
    let Ok(text) = std::fs::read_to_string(&file) else {
        return Err(Error::Missing(file));
    };
    // Anyone who can rewrite this file is not editing settings, they are
    // writing a program: `init` runs SQL that can load native code, and
    // `url` decides which server a client dials. Refuse it whole, the way
    // ssh refuses a loose identity file, rather than trust it in part.
    for p in [&file, &root] {
        if crate::perms::exposed(p) {
            return Err(Error::Refused { file: file.clone(), offender: p.clone() });
        }
    }
    let cfg: FileConfig = toml::from_str(&text)
        .map_err(|e| Error::Invalid { file: file.clone(), why: e.to_string() })?;
    normalize_keys(cfg).map_err(|why| Error::Invalid { file, why })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        [defaults]
        mode = "duckbox"

        [connection.medlabs]
        path = "~/Data/Code/medlabs/api/db/medlabs.duckdb"

        [connection.warehouse]
        path = "/srv/data/inventory.duckdb"
        memory-limit = "8GB"
        init = ["SET threads=4"]

        [connection.local]
        url = "http://localhost:9495"

        [connection.tunneled]
        url = "http://warehouse.example.com:9495"
    "#;

    #[test]
    fn one_namespace_two_kinds() {
        let c: FileConfig = toml::from_str(SAMPLE).unwrap();
        assert_eq!(c.defaults.mode.as_deref(), Some("duckbox"));

        let berths: Vec<_> = c.berths().iter().map(|(n, _)| *n).collect();
        assert_eq!(berths, ["medlabs", "warehouse"]);
        let remotes: Vec<_> = c.remotes().iter().map(|(n, _)| *n).collect();
        assert_eq!(remotes, ["local", "tunneled"]);

        let w = c.get("warehouse").unwrap();
        assert_eq!(w.memory_limit.as_deref(), Some("8GB"));
        assert_eq!(w.init.as_deref(), Some(&["SET threads=4".to_string()][..]));
    }

    #[test]
    fn the_name_is_the_key_not_the_file_stem() {
        // The bug this schema exists to make impossible: `warehouse` must
        // never become a berth called `inventory`.
        let c: FileConfig = toml::from_str(SAMPLE).unwrap();
        let (name, conn) = c.berths()[1];
        assert_eq!(name, "warehouse");
        assert!(conn.database().unwrap().ends_with("inventory.duckdb"));
    }

    #[test]
    fn both_or_neither_is_reported_not_guessed() {
        let c: FileConfig = toml::from_str(
            r#"
            [connection.confused]
            path = "/a.duckdb"
            url = "https://b"
            [connection.empty]
            "#,
        )
        .unwrap();
        assert_eq!(c.malformed(), ["confused", "empty"]);
        assert!(c.berths().is_empty());
    }

    #[test]
    fn unknown_keys_are_an_error_not_a_silent_typo() {
        // `default-address-pool` vs `default-address-pools` cost Docker users
        // years. A misspelled key here is a hard error naming the key.
        let e = toml::from_str::<FileConfig>("[connection.x]\npth = \"/a.duckdb\"\n");
        assert!(e.is_err());
    }

    #[test]
    fn settings_render_as_typed_sorted_set_statements() {
        let c: FileConfig = toml::from_str(
            r#"
            [connection.warehouse]
            path = "/w.duckdb"
            init = ["LOAD httpfs"]

            [connection.warehouse.settings]
            s3_region = "us-east-1"
            enable_progress_bar = true
            checkpoint_threshold = 16
            "#,
        )
        .unwrap();
        // Sorted for a reproducible start; typed: string quoted, bool/int bare.
        assert_eq!(
            c.get("warehouse").unwrap().setting_statements(),
            [
                "SET checkpoint_threshold = 16",
                "SET enable_progress_bar = true",
                "SET s3_region = 'us-east-1'",
            ]
        );
    }

    #[test]
    fn a_quote_in_a_setting_value_is_doubled_not_broken_out() {
        let c: FileConfig = toml::from_str(
            "[connection.x]\npath = \"/x.duckdb\"\n[connection.x.settings]\nname = \"a'b\"\n",
        )
        .unwrap();
        assert_eq!(c.get("x").unwrap().setting_statements(), ["SET name = 'a''b'"]);
    }
}
