//! `harbor <db> backup [dir]` and `harbor <db> restore <dir>` — a database's
//! CONTENTS, in a form that outlives the file that holds them.
//!
//! A harbor-written `.duckdb` is only as portable as the engine that wrote
//! it; copying one is a snapshot, not a backup. These two verbs write and
//! read the durable thing instead: a directory of `schema.sql`, `load.sql`
//! and one tab-separated file per table, which is `EXPORT DATABASE` and
//! `IMPORT DATABASE` underneath with the dialect pinned so the pair agree.
//!
//! TSV rather than parquet, deliberately. Both round-trip exactly and both
//! come to about the same size, so the tie goes to what you can do with the
//! artifact six months from now: grep it, diff two of them, read one in an
//! editor, keep one in a repo. A parquet file answers none of those.
//!
//! The dialect is three values and one escape, which is the whole of it:
//!
//!   NULL        a real null — four bare characters
//!   "NULL"      the STRING "NULL", quoted to escape the marker
//!   <nothing>   an empty string (a written `""` reads the same way)
//!
//! Nothing else can collide, because any value that would read as the marker
//! is quoted on the way out.
//!
//! The directory is self-contained: `load.sql` names each file by its own
//! name and nothing more, so the whole thing can be moved, renamed, copied to
//! another machine or committed to a repo and still restore.
//!
//! Restore writes a NEW file and refuses an existing one, always. A restore
//! that can overwrite is a restore that can be run at the wrong moment and
//! take the very thing it was meant to protect; swapping the file into place
//! is a human's job, and a deliberate one. It is also the only moment block
//! size can be chosen, since DuckDB fixes that when a file is created and
//! offers no ALTER — so `--block-size` belongs here.

use std::fs;
use std::path::{Path, PathBuf};

/// The writer's dialect. Backup and restore must agree on it, so it is said
/// once. `\t` is the two-character spelling DuckDB reads as a tab.
const DIALECT: &str = "FORMAT csv, DELIMITER '\\t', NULLSTR 'NULL'";

/// `harbor <db> backup [dir]`.
pub fn backup(db: &Path, args: &[String]) -> Result<(), String> {
    if !db.exists() {
        return Err(format!("{} does not exist — there is nothing to back up", db.display()));
    }
    let dir = match lone_path("backup", args)? {
        Some(d) => absolute(&d)?,
        None => default_dir(db)?,
    };
    if dir.exists() {
        return Err(format!(
            "{} already exists — a backup never writes into a directory that is there",
            dir.display()
        ));
    }
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }

    let target = db.display().to_string();
    let sql = format!("EXPORT DATABASE {} ({DIALECT})", quote(&dir));
    harbor::repl::exec_quiet(&target, &[&sql], &[])?;
    patch_loader(&dir)?;

    let (tables, bytes) = weigh(&dir)?;
    eprintln!(
        "harbor: backed up {tables} table{} to {} ({})",
        if tables == 1 { "" } else { "s" },
        harbor_common::paths::shorten(&dir),
        size(bytes)
    );
    eprintln!("harbor: restore it with — harbor <new.duckdb> restore {}", dir.display());
    Ok(())
}

/// `harbor <db> restore <dir> [--block-size <s>]`.
pub fn restore(db: &Path, args: &[String]) -> Result<(), String> {
    let mut dir: Option<String> = None;
    let mut block: Option<String> = None;
    let mut rest = args.iter();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--block-size" => {
                let Some(v) = rest.next() else {
                    return Err("--block-size needs a size (16k, 32k, 64k, 128k, 256k)".into());
                };
                harbor::parse_block_size(v)?;
                block = Some(v.clone());
            }
            _ if a.starts_with('-') => return Err(format!("restore: unexpected option {a}")),
            _ if dir.is_none() => dir = Some(a.clone()),
            _ => return Err(format!("restore: unexpected argument {a}")),
        }
    }
    let Some(dir) = dir else {
        return Err("restore from where? — harbor <new.duckdb> restore <dir>".into());
    };
    let dir = absolute(&dir)?;
    if !dir.join("load.sql").exists() {
        return Err(format!("{} has no load.sql — not a backup directory", dir.display()));
    }
    // The law of this verb. A restore is allowed to make a database and
    // nothing else; the WAL is named too, since a stray one would be read
    // as belonging to the file we are about to write.
    if db.exists() {
        return Err(format!(
            "{} already exists — restore only ever makes a NEW database. Restore beside it, \
             then move it into place",
            db.display()
        ));
    }
    let wal = PathBuf::from(format!("{}.wal", db.display()));
    if wal.exists() {
        return Err(format!("{} is in the way — move it aside first", wal.display()));
    }

    let spawn: Vec<String> = match &block {
        Some(v) => vec!["--block-size".into(), v.clone()],
        None => Vec::new(),
    };
    let target = db.display().to_string();
    let sql = format!("IMPORT DATABASE {}", quote(&dir));
    if let Err(e) = harbor::repl::exec_quiet(&target, &[&sql, "CHECKPOINT"], &spawn) {
        // A half-written database is worse than none: it exists, so the next
        // restore refuses, and it opens, so it can be mistaken for the real
        // thing. Take it back out.
        let _ = harbor::repl::shutdown(db);
        let _ = fs::remove_file(db);
        let _ = fs::remove_file(&wal);
        return Err(e);
    }
    // A restored database is a FILE, not a berth — nobody asked for a server
    // on it. Fold the WAL in and let it go. A server that will not go is
    // worth saying and is not a failed restore: the rows are in.
    if let Err(e) = harbor::repl::shutdown(db) {
        eprintln!("harbor: restored, but its server is still up — {e}");
    }

    let bytes = fs::metadata(db).map(|m| m.len()).unwrap_or(0);
    let (tables, _) = weigh(&dir)?;
    eprintln!(
        "harbor: restored {tables} table{} into {} ({})",
        if tables == 1 { "" } else { "s" },
        harbor_common::paths::shorten(db),
        size(bytes)
    );
    // Say what it is NOT, because the mistake is silent: a restored file that
    // nothing serves looks exactly like a database that came back.
    eprintln!("harbor: it is a FILE, not a berth — put it in service by hand:");
    eprintln!("  harbor <berth> stop && mv {} <the database it replaces>", db.display());
    Ok(())
}

/// Two edits to the generated `load.sql`, both about the artifact outliving
/// the moment it was made.
///
/// The paths come out absolute, because that is what `EXPORT DATABASE` was
/// handed, and an absolute path nails the directory to the machine that
/// wrote it — move it, rename it, `rsync` it, and every `COPY` still points
/// at where it used to be. `IMPORT DATABASE` resolves a relative path
/// against the directory it was given, so the file's own name is the only
/// spelling that is always right.
///
/// And duckdb#25501: EXPORT writes `""` for an empty string and a bare field
/// for NULL, then hands you a load.sql that reads BOTH back as NULL, because
/// the COPY it generates inherits `allow_quoted_nulls=true` from the CSV
/// reader's Pandas-compatibility default (duckdb#7162). The writer is right
/// and the reader is wrong, so the fix goes on the reader. That half comes
/// out once 25501 lands; the paths stay.
fn patch_loader(dir: &Path) -> Result<(), String> {
    let path = dir.join("load.sql");
    let before = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let after = before
        .replace(&format!("'{}/", dir.display()), "'")
        .replace("FORMAT 'csv'", "FORMAT 'csv', allow_quoted_nulls false");
    if after == before && !before.trim().is_empty() {
        return Err(format!(
            "{} was not in the shape this patch expects — see duckdb#25501",
            path.display()
        ));
    }
    fs::write(&path, after).map_err(|e| format!("{}: {e}", path.display()))
}

/// One bare word, or none. Options belong to the verbs that have them.
fn lone_path(verb: &str, args: &[String]) -> Result<Option<String>, String> {
    match args {
        [] => Ok(None),
        [one] if !one.starts_with('-') => Ok(Some(one.clone())),
        _ => Err(format!("{verb}: unexpected argument{}: {}",
            if args.len() == 1 { "" } else { "s" }, args.join(" "))),
    }
}

/// `<db>.backups/<stamp>` beside the database. A stamp, not a name: two
/// backups taken the same day are two backups, and the one thing a backup
/// directory must never do is quietly become another backup's grave. The
/// format sorts lexically, which is also chronologically.
fn default_dir(db: &Path) -> Result<PathBuf, String> {
    let db = absolute(&db.display().to_string())?;
    let stem = db.file_stem().map_or_else(|| "db".into(), |s| s.to_string_lossy().into_owned());
    let parent = db.parent().unwrap_or(Path::new(".")).to_path_buf();
    Ok(parent.join(format!("{stem}.backups")).join(stamp()))
}

/// `YYYYMMDDHHMMSS`, UTC. Days-from-civil, run backwards — a fortnight of
/// date arithmetic is not worth a dependency.
fn stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}{:02}{:02}{:02}", rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// How many tables the directory holds, and what it weighs.
fn weigh(dir: &Path) -> Result<(usize, u64), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let (mut tables, mut bytes) = (0, 0);
    for entry in entries.flatten() {
        let meta = entry.metadata().map_err(|e| format!("{}: {e}", dir.display()))?;
        bytes += meta.len();
        if entry.path().extension().is_some_and(|e| e == "csv") {
            tables += 1;
        }
    }
    Ok((tables, bytes))
}

fn size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1}G", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1}M", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{}K", b / (1 << 10)),
        b => format!("{b}B"),
    }
}

/// The server has its own working directory, so a relative path given here
/// would land somewhere else entirely. Absolute, always — without requiring
/// the path to exist, which for a backup directory it must not.
fn absolute(p: &str) -> Result<PathBuf, String> {
    let p = harbor_common::paths::expand(p);
    std::path::absolute(&p).map_err(|e| format!("{}: {e}", p.display()))
}

/// A path as a SQL string literal.
fn quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', "''"))
}
