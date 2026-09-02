//! The membership store: the `[connection.<name>]` sections of config.toml.
//!
//! `attach` and `detach` add or remove one connection, editing the file through
//! toml_edit's DOM so the comments and ordering the operator wrote survive
//! untouched — we mutate one node, we don't reserialize the file. The full
//! schema stays with the `config` reader; here we only ever touch a section's
//! `path`, so we need the editor, not the typed deserializer.
//!
//! Section keys are matched normalized, so `[connection.MedLabs]` answers
//! `medlabs`, and every valid spelling — a standard table, an inline
//! `connection.x = {…}`, a dotted key — is handled the same, because it is the
//! same DOM either way.

use crate::{paths, perms};
use std::path::Path;
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

/// Add `db` to the config under its normalized stem, or confirm it is already
/// there. Returns the name it was filed under. Errors if that name already
/// belongs to a different file.
pub fn attach(db: &Path) -> Result<(String, Attached), String> {
    let name = name_of(db)?;
    let canon = paths::canonical_db(db)?;
    let stored = paths::shorten(&canon);
    let mut doc = parse(&read()?)?;

    if let Some((_, existing)) = find(&doc, &name)? {
        // Same file under the same name is idempotent success; a different file
        // wanting the same name is the collision only its author can resolve.
        if let Some(p) = &existing {
            if paths::canonical_db(&paths::expand(p)).ok() == Some(canon) {
                return Ok((name, Attached::AlreadyThere));
            }
        }
        return Err(match existing {
            Some(p) => format!("'{name}' already names {p} — detach it first, or rename this one"),
            None => format!("'{name}' already exists and is not a local database — remove it by hand first"),
        });
    }

    insert(&mut doc, &name, &stored)?;
    let text = doc.to_string();
    verify(&text, &name, true)?;
    write(&text)?;
    Ok((name, Attached::Added))
}

/// Remove `db` from the config, matched by its normalized stem. Returns whether
/// a section was actually there to remove; a missing file or absent name is a
/// quiet `false`, never an error.
pub fn detach(db: &Path) -> Result<(String, bool), String> {
    let name = name_of(db)?;
    let mut doc = parse(&read()?)?;

    let Some((key, _)) = find(&doc, &name)? else {
        return Ok((name, false));
    };
    remove(&mut doc, &key)?;
    let text = doc.to_string();
    verify(&text, &name, false)?;
    write(&text)?;
    Ok((name, true))
}

/// The name a database files under: its stem, normalized to the registry
/// alphabet — the same derivation every other harbor name goes through.
pub fn name_of(db: &Path) -> Result<String, String> {
    let stem = db
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("not a database path: {}", db.display()))?;
    paths::normalize(stem)
}

fn read() -> Result<String, String> {
    let file = paths::config_file()?;
    Ok(std::fs::read_to_string(&file).unwrap_or_default())
}

/// Write the new config over the old, atomically and privately: a temp file in
/// the same directory, then a rename, so a crash mid-write can never leave a
/// half-config behind.
fn write(text: &str) -> Result<(), String> {
    let root = paths::config_root()?;
    perms::ensure_private_dir(&root)?;
    let file = root.join("config.toml");
    let tmp = root.join("config.toml.tmp");
    perms::write_private(&tmp, text).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &file).map_err(|e| format!("replacing {}: {e}", file.display()))?;
    Ok(())
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

/// Add `[connection.<name>]` with `path = <stored>`. The caller has ruled out a
/// collision, so this only ever creates.
fn insert(doc: &mut DocumentMut, name: &str, stored: &str) -> Result<(), String> {
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
    entry["path"] = value(stored);
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

/// Postcondition, checked before the bytes land: the edited text still parses,
/// and the named section is present (`want`) or gone (`!want`). Cheap insurance
/// that the edit did what we think.
fn verify(text: &str, name: &str, want: bool) -> Result<(), String> {
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
        insert(&mut doc, name, path).unwrap();
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
[defaults]
mode = \"duckbox\"

[connection.medlabs]
path = \"~/med.duckdb\"
";
        let after = attached(before, "warehouse", "~/wh.duckdb");
        assert!(after.contains("# my databases"), "the comment must survive");
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
[defaults]
mode = \"csv\"

[connection.foo]
path = \"~/foo.duckdb\"

[connection.bar]
url = \"https://x\"
";
        let after = detached(text, "foo");
        assert!(after.contains("[defaults]") && after.contains("mode = \"csv\""));
        assert!(after.contains("[connection.bar]"));
        assert!(!after.contains("[connection.foo]") && !after.contains("foo.duckdb"));
    }

    #[test]
    fn detach_the_last_connection_drops_the_parent_table() {
        // No bare `[connection]` header may be left behind.
        let after = detached("[connection.only]\npath = \"~/only.duckdb\"\n", "only");
        assert!(!after.contains("[connection"), "no lingering connection header");
    }

    #[test]
    fn inline_connection_now_edits_instead_of_refusing() {
        // The form the hand-rolled writer used to refuse: toml_edit handles it.
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

    #[test]
    fn defaults_connection_key_is_not_mistaken_for_a_section() {
        // `connection = "medlabs"` inside [defaults] is a key, not a berth.
        let text = "[defaults]\nconnection = \"medlabs\"\n\n[connection.medlabs]\npath = \"~/m.duckdb\"\n";
        let (key, _) = find(&parse(text).unwrap(), "medlabs").unwrap().unwrap();
        assert_eq!(key, "medlabs");
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
}
