//! `~/.config/harbor/config.toml` — one file, read by both binaries.
//!
//! One namespace, so "where do I configure medlabs?" has one answer. What
//! kind of thing an entry is follows from which key it sets:
//!
//! ```toml
//! [defaults]
//! mode      = "duckbox"     # pilot's taste
//! idle-exit = "90s"         # harbor's spawn policy
//!
//! [connection.medlabs]      # has `path` -> a local berth, harbor can start it
//! path = "~/Data/Code/medlabs/api/db/medlabs.duckdb"
//!
//! [connection.prod]         # has `url` -> a remote, harbor never touches it
//! url       = "https://db.example.com"
//! token-cmd = "op read op://vault/prod/token"
//! ```
//!
//! Harbor reads this file too, which is what makes `harbor start medlabs`
//! mean anything. It is safe for it to: the deserializer below has no
//! `token-cmd` field on the server's side of the fence, so the server cannot
//! be made to shell out for a credential no matter what the file says.
//!
//! Every berth key is the matching `harbor serve` flag with the dashes
//! stripped, so `harbor serve --help` is the reference and there is no second
//! dialect to learn. A key that is only ever passed as a flag would make the
//! config decorative, so there aren't any.

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
    // --- pilot: how output looks -------------------------------------------
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
    /// What bare `pilot` opens. Unset, bare `pilot` lists what is openable
    /// instead — deliberately, rather than connecting to the only berth when
    /// there happens to be one, which would make adding a second database
    /// silently change what the command does.
    pub connection: Option<String>,

    // --- harbor: how a berth is started ------------------------------------
    /// Ten lines above the entry it explains, which is the whole reason a
    /// shared default is tolerable here and would not be across two files.
    pub idle_exit: Option<String>,
    pub memory_limit: Option<String>,
    pub workers: Option<usize>,
    pub threads: Option<usize>,
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Connection {
    // --- a remote: pilot only ----------------------------------------------
    pub url: Option<String>,
    pub token: Option<String>,
    pub token_file: Option<String>,
    pub token_cmd: Option<String>,

    // --- a local berth: harbor starts it, pilot may summon it --------------
    pub path: Option<String>,
    pub idle_exit: Option<String>,
    pub memory_limit: Option<String>,
    pub threads: Option<usize>,
    pub workers: Option<usize>,
    pub statement_timeout: Option<String>,
    pub max_temp_size: Option<String>,
    pub sealed: Option<bool>,
    pub unsigned: Option<bool>,
    pub create: Option<bool>,
    pub log: Option<bool>,
    pub init: Option<Vec<String>>,
    pub port: Option<u16>,
    pub bind: Option<String>,
    /// Start this berth at login. The flag lives here, in the file you edit,
    /// and never in runtime state — launchd moved exactly one field of
    /// desired state (`Disabled`) out into an opaque database and spent a
    /// decade explaining why editing the plist did nothing.
    pub autostart: Option<bool>,
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

    /// Berths marked `autostart`, sorted. One platform unit runs `harbor
    /// boot`, which starts these — rather than one unit per berth, so this
    /// file stays the only place that says what starts at login.
    pub fn autostarted(&self) -> Vec<(&str, &Connection)> {
        self.berths().into_iter().filter(|(_, c)| c.autostart == Some(true)).collect()
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
    /// No config file. Normal: zero-config local always works.
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

/// Read and parse the config, or say precisely why not.
pub fn load() -> Result<FileConfig, Error> {
    let root = crate::paths::config_root().map_err(Error::NoHome)?;
    let file = root.join("config.toml");
    let Ok(text) = std::fs::read_to_string(&file) else {
        return Err(Error::Missing(file));
    };
    // Anyone who can rewrite this file is not editing settings, they are
    // writing a program: `token-cmd` runs through `sh -c`, `init` runs SQL
    // that can load native code, and `url` decides who receives the bearer
    // token. Refuse it whole, the way ssh refuses a loose identity file,
    // rather than trust it in part.
    for p in [&file, &root] {
        if crate::perms::exposed(p) {
            return Err(Error::Refused { file: file.clone(), offender: p.clone() });
        }
    }
    toml::from_str(&text).map_err(|e| Error::Invalid { file, why: e.to_string() })
}

/// Load, or carry on with nothing. A missing file is silent — that is the
/// zero-config path, not a problem. Anything else is worth a line on stderr,
/// because it means settings the user wrote are not in effect.
pub fn load_or_empty(who: &str) -> FileConfig {
    match load() {
        Ok(c) => c,
        Err(Error::Missing(_)) => FileConfig::default(),
        Err(e) => {
            eprintln!("{who}: {e}");
            FileConfig::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        [defaults]
        mode = "duckbox"
        idle-exit = "90s"

        [connection.medlabs]
        path = "~/Data/Code/medlabs/api/db/medlabs.duckdb"

        [connection.warehouse]
        path = "/srv/data/inventory.duckdb"
        memory-limit = "8GB"
        init = ["LOAD ui"]

        [connection.prod]
        url = "https://db.example.com"
        token-cmd = "op read op://vault/prod/token"
    "#;

    #[test]
    fn one_namespace_two_kinds() {
        let c: FileConfig = toml::from_str(SAMPLE).unwrap();
        assert_eq!(c.defaults.mode.as_deref(), Some("duckbox"));
        assert_eq!(c.defaults.idle_exit.as_deref(), Some("90s"));

        let berths: Vec<_> = c.berths().iter().map(|(n, _)| *n).collect();
        assert_eq!(berths, ["medlabs", "warehouse"]);
        let remotes: Vec<_> = c.remotes().iter().map(|(n, _)| *n).collect();
        assert_eq!(remotes, ["prod"]);

        let w = c.get("warehouse").unwrap();
        assert_eq!(w.memory_limit.as_deref(), Some("8GB"));
        assert_eq!(w.init.as_deref(), Some(&["LOAD ui".to_string()][..]));
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
}
