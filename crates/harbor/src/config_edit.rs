//! The one writer config.toml has.
//!
//! The file is the operator's — comments, ordering, and formatting are their
//! prose — so every edit goes through `toml_edit`'s document model and never
//! a struct round-trip, which would silently erase a hand-written `token-cmd`
//! the server-side schema carries but never reads.
//!
//! Every write passes the same trust gate reads do: a file (or config root)
//! writable by others refuses the edit outright, and the edited text must
//! survive the shared schema check before a byte lands — an edit may never
//! leave behind a file `load` will refuse, because a refused config takes
//! every bare name down with it.

use std::path::PathBuf;

use harbor_common::perms;

/// Read the document, or start an empty one.
fn open_document() -> Result<(PathBuf, toml_edit::DocumentMut), String> {
    let file = harbor_common::config_file()?;
    if file.exists() && perms::exposed(&file) {
        return Err(format!(
            "{} is writable by others or not yours (chmod go-w it) — refusing to edit",
            file.display()
        ));
    }
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    let doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| format!("{} is not valid TOML: {e}", file.display()))?;
    Ok((file, doc))
}

/// Schema-check, then write. The check runs on the exact bytes about to
/// land. Creating the config root here is the deliberate exception to
/// serve's "a server should not conjure a config directory": an edit is the
/// operator writing desired state, and desired state needs somewhere to live.
fn commit(file: &PathBuf, doc: &toml_edit::DocumentMut) -> Result<(), String> {
    let text = doc.to_string();
    harbor_common::config::parse(&text)
        .map_err(|e| format!("refusing an edit the config schema rejects: {e}"))?;
    let root = harbor_common::config_root()?;
    if !root.exists() {
        perms::create_dir_private(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    }
    perms::write_private(file, &text).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(())
}

/// `[connection.<name>] path = "<db>"` — the promotion. Refuses to touch an
/// existing entry: overwriting a name is a decision the operator makes in
/// the file, where the rest of that entry is visible.
pub fn add_entry(name: &str, db: &str) -> Result<PathBuf, String> {
    let (file, mut doc) = open_document()?;
    let connections = doc["connection"].or_insert(toml_edit::Item::Table(Default::default()));
    if let Some(t) = connections.as_table_mut() {
        t.set_implicit(true);
        if t.contains_key(name) {
            return Err(format!(
                "[connection.{name}] already exists in {} — edit it there",
                file.display()
            ));
        }
        let mut entry = toml_edit::Table::new();
        entry["path"] = toml_edit::value(db);
        t.insert(name, toml_edit::Item::Table(entry));
    } else {
        return Err(format!("`connection` is not a table in {}", file.display()));
    }
    commit(&file, &doc)?;
    Ok(file)
}

/// Drop `[connection.<name>]`, reporting whether it was there to drop.
pub fn remove_entry(name: &str) -> Result<bool, String> {
    let (file, mut doc) = open_document()?;
    let removed = doc
        .get_mut("connection")
        .and_then(toml_edit::Item::as_table_mut)
        .map(|t| t.remove(name).is_some())
        .unwrap_or(false);
    if removed {
        commit(&file, &doc)?;
    }
    Ok(removed)
}

/// Set (or, with None, remove) one key on an existing entry.
pub fn set_entry_key(
    name: &str,
    key: &str,
    value: Option<toml_edit::Value>,
) -> Result<(), String> {
    let (file, mut doc) = open_document()?;
    let entry = doc
        .get_mut("connection")
        .and_then(toml_edit::Item::as_table_mut)
        .and_then(|t| t.get_mut(name))
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or_else(|| format!("no [connection.{name}] in {}", file.display()))?;
    match value {
        Some(v) => {
            entry[key] = toml_edit::Item::Value(v);
        }
        None => {
            entry.remove(key);
        }
    }
    commit(&file, &doc)
}
