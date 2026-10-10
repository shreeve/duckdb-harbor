//! The membership store: the `[connection.<name>]` sections of config.toml.
//!
//! Local attach/detach and DuckTable's remote-database flow add or remove one
//! connection, editing the file through
//! toml_edit's DOM so the comments and ordering the operator wrote survive
//! untouched — we mutate one node, we don't reserialize the file. The full
//! schema stays with the `config` reader, which checks every edit before it
//! lands; here we only ever add or remove a section, so we need the editor.
//!
//! Section keys are matched normalized, so `[connection.MedLabs]` answers
//! `medlabs`, and every valid spelling — a standard table, an inline
//! `connection.x = {…}`, a dotted key — is handled the same, because it is the
//! same DOM either way.

use crate::{paths, perms};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use toml_edit::{value, DocumentMut, Item, Table};

/// What an `attach` did, once the already-there case is resolved.
#[derive(Debug, PartialEq, Eq)]
pub enum Attached {
    /// A new `[connection.<name>]` section was added.
    Added,
    /// The name already points at this same database — nothing to do.
    AlreadyThere,
}

// ---------------------------------------------------------------------------
// IO surface — what the CLI calls: derive the name and path, read the file, run
// the pure edit, verify the postcondition, write it back atomically.
// ---------------------------------------------------------------------------

/// The words the CLI reads as verbs where a database's name would go, so a
/// database filed under one could never be named.
pub const RESERVED: [&str; 12] = [
    "attach", "detach", "start", "stop", "restart", "autostart", "off", "backup", "restore",
    "update", "help", "version",
];

/// Add `db` to the config under its normalized stem, or confirm it is already
/// there — under whatever key already names this file. Returns the name it is
/// filed under. Errors if the stem already belongs to a different file.
pub fn attach(db: &Path) -> Result<(String, Attached), String> {
    let _lock = lock_config()?;
    let canon = paths::canonical_db(db)?;
    let mut doc = parse(&read()?)?;
    if let Some(key) = filed_as(&doc, &canon)? {
        return Ok((paths::normalize(&key)?, Attached::AlreadyThere));
    }
    let name = name_of(db)?;
    if RESERVED.contains(&name.as_str()) {
        return Err(format!(
            "'{name}' is a word harbor reads as a verb, so it could not name this database — rename the file"
        ));
    }
    if let Some((_, existing)) = find(&doc, &name)? {
        return Err(match existing {
            Some(p) => format!("'{name}' already names {p} — detach it first, or rename this one"),
            None => format!("'{name}' already exists and is not a local database — remove it by hand first"),
        });
    }

    insert(&mut doc, &name, "path", &paths::shorten(&canon))?;
    let text = doc.to_string();
    verify(&text, &name, true)?;
    write(&text)?;
    Ok((name, Attached::Added))
}

/// Remove `db` from the config: the entry whose `path` is this file, and only
/// that one. Another file that shares its stem keeps its entry. Returns the
/// name it was filed under (its stem when nothing names it) and whether an
/// entry was there to remove; nothing to remove is a quiet `false`.
pub fn detach(db: &Path) -> Result<(String, bool), String> {
    let _lock = lock_config()?;
    let canon = paths::canonical_db(db)?;
    let mut doc = parse(&read()?)?;
    let Some(key) = filed_as(&doc, &canon)? else {
        return Ok((name_of(db)?, false));
    };
    let name = paths::normalize(&key)?;
    remove(&mut doc, &key)?;
    let text = doc.to_string();
    verify(&text, &name, false)?;
    write(&text)?;
    Ok((name, true))
}

/// Add a named database reached through an existing Harbor TCP listener.
pub fn add_remote(name: &str, url: &str) -> Result<String, String> {
    let _lock = lock_config()?;
    let name = paths::normalize(name)?;
    let url = url.trim();
    if url.is_empty() {
        return Err("database address is empty".into());
    }
    let mut doc = parse(&read()?)?;
    if find(&doc, &name)?.is_some() {
        return Err(format!("'{name}' already exists — choose another name"));
    }
    insert(&mut doc, &name, "url", url)?;
    let text = doc.to_string();
    verify(&text, &name, true)?;
    write(&text)?;
    Ok(name)
}

/// Remove a named connection without interpreting what kind it is. Front ends
/// establish that policy before calling; this layer only preserves the TOML.
pub fn remove_named(name: &str) -> Result<bool, String> {
    let _lock = lock_config()?;
    let name = paths::normalize(name)?;
    let mut doc = parse(&read()?)?;
    let Some((key, _)) = find(&doc, &name)? else {
        return Ok(false);
    };
    remove(&mut doc, &key)?;
    let text = doc.to_string();
    verify(&text, &name, false)?;
    write(&text)?;
    Ok(true)
}

/// The name a database answers to: the config key that already names this
/// file, when one does, else the stem it would be filed under. A login item,
/// a footnote row and a `stop` all go through this, so `[connection.warehouse]`
/// pointing at `inventory.duckdb` is `warehouse` everywhere and never grows
/// an `inventory` twin.
pub fn name_for(db: &Path) -> Result<String, String> {
    let canon = paths::canonical_db(db)?;
    match listed_as(&canon) {
        Some(key) => Ok(key),
        None => name_of(db),
    }
}

/// The config key whose `path` is this canonical file, if any. A config that
/// will not load names nothing — the caller falls back to the stem.
fn listed_as(canon: &Path) -> Option<String> {
    let cfg = crate::config::load().ok()?;
    cfg.berths().into_iter().find_map(|(key, c)| {
        let p = c.database()?;
        (paths::canonical_db(&p).ok()? == *canon).then(|| key.to_string())
    })
}

/// The name a database files under when nothing names it yet: its stem,
/// normalized to the registry alphabet and cut to the name law's 64
/// characters. Any file DuckDB can open has one: a stem that is long or not
/// UTF-8 is shortened or spelled with `-`, never refused.
pub fn name_of(db: &Path) -> Result<String, String> {
    let stem = db
        .file_stem()
        .ok_or_else(|| format!("not a database path: {}", db.display()))?
        .to_string_lossy();
    // Lowercased first, since that can lengthen a char; `normalize` then maps
    // each char to one.
    let stem: String = stem.to_lowercase().chars().take(64).collect();
    paths::normalize(&stem)
}

/// The key of the entry whose `path` is this canonical file, in the
/// operator's spelling.
fn filed_as(doc: &DocumentMut, canon: &Path) -> Result<Option<String>, String> {
    let Some(item) = doc.get("connection") else {
        return Ok(None);
    };
    let table = item.as_table_like().ok_or("`connection` in config.toml is not a table")?;
    Ok(table.iter().find_map(|(key, val)| {
        let path = val.as_table_like()?.get("path")?.as_str()?;
        (paths::canonical_db(&paths::expand(path)).ok()? == canon).then(|| key.to_string())
    }))
}

/// Hold this separate inode across the entire read-modify-rename operation.
/// Never unlink the lock file: another process may already be waiting on it.
fn lock_config() -> Result<File, String> {
    let root = paths::config_root()?;
    perms::ensure_private_dir(&root)?;
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = opts.open(root.join("config.toml.lock"))
        .map_err(|e| format!("opening config lock: {e}"))?;
    file.lock().map_err(|e| format!("locking config: {e}"))?;
    Ok(file)
}

fn read() -> Result<String, String> {
    let file = paths::config_file()?;
    if perms::exposed(&file) {
        return Err(format!("refusing to edit exposed config {}", file.display()));
    }
    match fs::read_to_string(&file) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("reading {}: {e}", file.display())),
    }
}

/// Caller holds the config lock. Exclusive creation keeps even a stale temp
/// file from a killed writer from being overwritten or followed as a symlink.
fn write(text: &str) -> Result<(), String> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    // A config kept elsewhere and linked into place is written where it is
    // kept: a rename onto the link would replace the link itself and leave
    // the file it pointed at behind, unchanged and unread. The
    // temporary file goes beside the real one, since a rename cannot cross
    // filesystems.
    let dest = paths::config_file()?;
    let dest = fs::canonicalize(&dest).unwrap_or(dest);
    let root = dest.parent().map(Path::to_path_buf).ok_or("config.toml has no directory")?;
    let (tmp, mut out) = loop {
        let tmp = root.join(format!("config.toml.{}.{}.tmp",
            std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(&tmp) {
            Ok(out) => break (tmp, out),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("creating {}: {e}", tmp.display())),
        }
    };
    let result = (|| {
        out.write_all(text.as_bytes())?;
        out.sync_all()?;
        drop(out);
        fs::rename(&tmp, &dest)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e| format!("writing config.toml: {e}"))
}

// ---------------------------------------------------------------------------
// Pure DOM core — text in, text out. Everything the tests exercise lives here.
// ---------------------------------------------------------------------------

fn parse(text: &str) -> Result<DocumentMut, String> {
    text.parse::<DocumentMut>().map_err(|e| format!("config.toml is not valid TOML: {e}"))
}

/// Find the connection whose key normalizes to `name`, returning its raw key
/// (the operator's spelling, needed to remove it) and its stored `path` if it
/// states one. `Err` only if `connection` exists but is not a table.
fn find(doc: &DocumentMut, name: &str) -> Result<Option<(String, Option<String>)>, String> {
    let Some(item) = doc.get("connection") else {
        return Ok(None);
    };
    let table = item
        .as_table_like()
        .ok_or("`connection` in config.toml is not a table")?;
    for (key, val) in table.iter() {
        // A key that can't be a name can't be the one we're looking for.
        if paths::normalize(key).ok().as_deref() == Some(name) {
            let path = val
                .as_table_like()
                .and_then(|t| t.get("path"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            return Ok(Some((key.to_string(), path)));
        }
    }
    Ok(None)
}

/// Add `[connection.<name>]` holding the one key that says what it is —
/// `path` for a local database, `url` for a remote — without disturbing the
/// document around it. The caller has ruled out a collision, so this only
/// ever creates.
fn insert(doc: &mut DocumentMut, name: &str, key: &str, val: &str) -> Result<(), String> {
    if doc.get("connection").is_none() {
        // A fresh parent table, implicit so no bare `[connection]` header is
        // emitted — only the `[connection.<name>]` child below.
        let mut parent = Table::new();
        parent.set_implicit(true);
        doc.insert("connection", Item::Table(parent));
    }
    let conn = doc["connection"]
        .as_table_mut()
        .ok_or("`connection` in config.toml is not a table")?;
    let mut entry = Table::new();
    entry[key] = value(val);
    conn.insert(name, Item::Table(entry));
    Ok(())
}

/// Remove the connection stored under this exact key. If it was the last one,
/// drop the now-empty `connection` table so no bare header lingers.
fn remove(doc: &mut DocumentMut, key: &str) -> Result<(), String> {
    let conn = doc["connection"]
        .as_table_mut()
        .ok_or("`connection` in config.toml is not a table")?;
    conn.remove(key);
    if conn.is_empty() {
        doc.as_table_mut().remove("connection");
    }
    Ok(())
}

/// Postcondition, checked before the bytes land: the edited text is a config
/// the typed reader accepts, so no edit leaves behind a file `load` refuses,
/// and the named section is present (`want`) or gone (`!want`).
fn verify(text: &str, name: &str, want: bool) -> Result<(), String> {
    crate::config::parse(text)
        .map_err(|e| format!("config.toml is not valid, so it is left unchanged — {e}"))?;
    let ok = match parse(text) {
        Ok(doc) => find(&doc, name).map(|f| f.is_some() == want).unwrap_or(false),
        Err(_) => false,
    };
    if ok {
        Ok(())
    } else {
        Err(format!(
            "internal: {} of {name} did not verify — config left unchanged",
            if want { "attach" } else { "detach" }
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text after attaching `name -> path`, asserting it was newly added.
    fn attached(text: &str, name: &str, path: &str) -> String {
        let mut doc = parse(text).unwrap();
        assert!(find(&doc, name).unwrap().is_none(), "expected a fresh name");
        insert(&mut doc, name, "path", path).unwrap();
        let out = doc.to_string();
        verify(&out, name, true).unwrap();
        out
    }
    /// The text after detaching `name`, asserting it was present.
    fn detached(text: &str, name: &str) -> String {
        let mut doc = parse(text).unwrap();
        let (key, _) = find(&doc, name).unwrap().expect("expected the name present");
        remove(&mut doc, &key).unwrap();
        let out = doc.to_string();
        verify(&out, name, false).unwrap();
        out
    }
    fn present(text: &str, name: &str) -> bool {
        find(&parse(text).unwrap(), name).unwrap().is_some()
    }
    fn stored_path(text: &str, name: &str) -> Option<String> {
        find(&parse(text).unwrap(), name).unwrap().and_then(|(_, p)| p)
    }

    #[test]
    fn attach_into_empty() {
        assert_eq!(attached("", "foo", "~/db/foo.duckdb"), "[connection.foo]\npath = \"~/db/foo.duckdb\"\n");
    }

    #[test]
    fn attach_preserves_comments_and_prior_sections() {
        let before = "\
# my databases
[connection.prod] # through the tunnel
url = \"http://localhost:9495\"

[connection.medlabs]
path = \"~/med.duckdb\"
";
        let after = attached(before, "warehouse", "~/wh.duckdb");
        assert!(after.contains("# my databases"), "the comment must survive");
        assert!(after.contains("[connection.prod] # through the tunnel"), "the remote must survive");
        assert!(after.contains("[connection.medlabs]"), "the prior berth must survive");
        assert!(after.contains("[connection.warehouse]") && after.contains("~/wh.duckdb"));
    }

    #[test]
    fn attach_idempotent_by_name_is_detected() {
        let text = "[connection.foo]\npath = \"~/foo.duckdb\"\n";
        let (key, path) = find(&parse(text).unwrap(), "foo").unwrap().unwrap();
        assert_eq!(key, "foo");
        assert_eq!(path.as_deref(), Some("~/foo.duckdb"));
    }

    #[test]
    fn section_key_is_matched_normalized() {
        let text = "[connection.MedLabs]\npath = \"~/m.duckdb\"\n";
        assert!(present(text, "medlabs"), "MedLabs answers medlabs");
        assert_eq!(detached(text, "medlabs"), "");
    }

    #[test]
    fn detach_removes_only_its_section() {
        let text = "\
[connection.foo]
path = \"~/foo.duckdb\"

[connection.bar] # kept
url = \"https://x\"
";
        let after = detached(text, "foo");
        assert!(after.contains("[connection.bar] # kept"));
        assert!(!after.contains("[connection.foo]") && !after.contains("foo.duckdb"));
    }

    #[test]
    fn detach_the_last_connection_drops_the_parent_table() {
        // No bare `[connection]` header may be left behind.
        let after = detached("[connection.only]\npath = \"~/only.duckdb\"\n", "only");
        assert!(!after.contains("[connection"), "no lingering connection header");
    }

    #[test]
    fn an_inline_connection_is_edited_like_a_section() {
        let text = "connection.foo = { path = \"~/foo.duckdb\" }\n";
        assert!(present(text, "foo"));
        assert!(!present(&detached(text, "foo"), "foo"), "the inline entry is removed");
    }

    #[test]
    fn bare_connection_table_with_inline_members_edits() {
        let text = "[connection]\nfoo = { path = \"~/foo.duckdb\" }\nbar = { path = \"~/bar.duckdb\" }\n";
        let after = detached(text, "foo");
        assert!(!present(&after, "foo"));
        assert!(present(&after, "bar"), "the sibling survives");
    }

    /// An entry is found by the file it names, never by a stem it shares:
    /// detaching `~/b/data.duckdb` must not take `~/a/data.duckdb`'s entry.
    #[test]
    fn an_entry_is_filed_by_its_canonical_path() {
        let root = std::env::temp_dir().join(format!("hb-filed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a")).unwrap();
        fs::create_dir_all(root.join("b")).unwrap();
        let root = root.canonicalize().unwrap();
        let text = format!("[connection.Data]\npath = \"{}\"\n", root.join("a/data.duckdb").display());
        let doc = parse(&text).unwrap();
        assert_eq!(filed_as(&doc, &root.join("a/data.duckdb")).unwrap().as_deref(), Some("Data"));
        assert_eq!(filed_as(&doc, &root.join("b/data.duckdb")).unwrap(), None);
        // Another spelling of the same file is the same file.
        assert_eq!(filed_as(&doc, &paths::canonical_db(&root.join("b/../a/data.duckdb")).unwrap()).unwrap().as_deref(), Some("Data"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_stem_beyond_the_name_law_is_cut_not_refused() {
        let long = format!("/x/{}.duckdb", "Q".repeat(200));
        assert_eq!(name_of(Path::new(&long)).unwrap(), "q".repeat(64));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let odd = Path::new(std::ffi::OsStr::from_bytes(b"/x/caf\xe9.duckdb"));
            assert_eq!(name_of(odd).unwrap(), "caf-");
        }
    }

    #[test]
    fn an_edit_that_leaves_an_invalid_config_is_refused() {
        let e = verify("[connection.a]\npath = \"/a.duckdb\"\npth = 1\n", "a", true).unwrap_err();
        assert!(e.contains("left unchanged") && e.contains("pth"), "{e}");
    }

    #[test]
    fn a_quote_in_a_path_round_trips() {
        let weird = "~/od\"d.duckdb";
        let text = attached("", "q", weird);
        assert_eq!(stored_path(&text, "q").as_deref(), Some(weird));
    }

    #[test]
    fn invalid_toml_is_rejected_not_edited() {
        assert!(parse("[connection.foo\npath =").is_err());
    }

    #[test]
    fn remote_addition_is_named_and_transport_explicit() {
        let mut doc = parse("# mine\n").unwrap();
        insert(&mut doc, "prod", "url", "http://foo.bar.com:9494").unwrap();
        let text = doc.to_string();
        assert!(text.contains("# mine"));
        assert!(text.contains("[connection.prod]"));
        assert!(text.contains("url = \"http://foo.bar.com:9494\""));
        verify(&text, "prod", true).unwrap();
    }
}
