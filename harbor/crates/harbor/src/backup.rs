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
//! The dialect is one rule: a bare `NULL` is the only bare thing in the file,
//! and every other value is quoted.
//!
//!   NULL        a real null — four bare characters
//!   "NULL"      the STRING "NULL"
//!   ""          an empty string
//!   "anything"  itself
//!
//! Nothing can collide, and nothing is invisible — which matters more than it
//! sounds. Written bare, an empty string is an empty field, and in a
//! one-column table an empty field is an empty LINE; every CSV reader skips
//! those, so the row would not come back and nothing would say so. The reader
//! still takes a bare field as an empty string, so a backup stays editable by
//! hand.
//!
//! Values that contain the separators are QUOTED, not escaped — RFC 4180, the
//! same rule every CSV reader already knows. A value holding a tab, a newline
//! or a quote is wrapped, the character sits inside it literally, and a `"`
//! within doubles to `""`. Nothing is backslash-escaped, so a backslash in
//! the data is only ever a backslash, and the two characters `\t` stay
//! distinct from a tab. The cost is that a value holding a newline spans
//! physical lines: a row is a record, not always a line, so `wc -l` counts
//! lines and not rows.
//!
//! The directory is self-contained: `load.sql` names each file by its own
//! name and nothing more, so the whole thing can be moved, renamed, copied to
//! another machine or committed to a repo and still restore.
//!
//! Two types text cannot hold — `UNION`, which loses its tag, and `VARIANT`,
//! whose contents come back retyped — are written as parquet instead, one
//! file, beside the others. `load.sql` names the format per table, so the
//! directory stays self-describing and the choice is visible in `ls`. Text
//! for what text can carry, parquet only where it must: a database with one
//! variant column keeps every other table greppable.
//!
//! `--format parquet` asks for the whole database in one format, and
//! `--strict` refuses the fallback rather than taking it — for a backup that
//! has to be text, all of it, or not at all. The rule under all three is the
//! same: never write something that will not come back. What varies is only
//! whether the answer to "text cannot hold this" is parquet or an error.
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
///
/// `FORCE_QUOTE *` is not decoration. Left off, an empty string is written as
/// an empty FIELD — and in a one-column table an empty field is an empty
/// LINE, which every CSV reader skips. The row does not come back and nothing
/// says so. Quoting every value costs about 14% on a small database and buys
/// a format with one rule instead of four cases: bare `NULL` is the only bare
/// thing in the file, and everything else is a quoted value.
const DIALECT: &str = "FORMAT csv, DELIMITER '\\t', NULLSTR 'NULL', FORCE_QUOTE *";

/// What the tables are written as.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    /// Tab-separated: greppable, diffable, editable. The default, because the
    /// artifact outliving its engine is most of the point.
    Tsv,
    /// Every table parquet: one format, every type, not readable by eye.
    Parquet,
}

impl Format {
    fn parse(word: &str) -> Result<Format, String> {
        match word {
            "tsv" | "csv" | "text" => Ok(Format::Tsv),
            "parquet" => Ok(Format::Parquet),
            _ => Err(format!("unknown format {word:?} — tsv or parquet")),
        }
    }

    fn options(self) -> &'static str {
        match self {
            Format::Tsv => DIALECT,
            Format::Parquet => "FORMAT parquet",
        }
    }

    /// What `load.sql` says to read it back with.
    fn loader(self) -> &'static str {
        match self {
            Format::Tsv => "FORMAT 'csv', allow_quoted_nulls false, delimiter '\\t', \
                            quote '\"', header 1, nullstr 'NULL'",
            Format::Parquet => "FORMAT 'parquet'",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Format::Tsv => "csv",
            Format::Parquet => "parquet",
        }
    }

    /// The spelling `--format` takes, which is not always the extension.
    fn flag(self) -> &'static str {
        match self {
            Format::Tsv => "tsv",
            Format::Parquet => "parquet",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Format::Tsv => "text",
            Format::Parquet => "parquet",
        }
    }

    fn other(self) -> Format {
        match self {
            Format::Tsv => Format::Parquet,
            Format::Parquet => Format::Tsv,
        }
    }

    /// The types this format cannot hold faithfully, as they are spelled in
    /// the exported `schema.sql`, each with the reason to say out loud.
    ///
    /// Neither format is complete, and the holes are not the same shape.
    /// Text loses a UNION's tag (the restore then refuses) and retypes a
    /// VARIANT's contents (it does not). Parquet normalises a TIMETZ to UTC,
    /// so `12:00:00+02:30` returns as `09:30:00+00` — the same instant,
    /// a different value, and nothing said. Each hole is the other format's
    /// solid ground, which is what makes a mixed directory the answer.
    ///
    /// Not every hole is a TYPE. Parquet also refuses a NEGATIVE interval,
    /// which is a value, invisible in a schema and impossible to route
    /// around from here — and does not need to be, because it fails loudly.
    fn cannot_hold(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Format::Tsv => &[
                ("VARIANT", "a VARIANT's contents come back retyped"),
                ("UNION(", "a UNION loses its tag"),
            ],
            Format::Parquet => &[
                ("TIME WITH TIME ZONE", "parquet normalises a TIMETZ to UTC"),
            ],
        }
    }
}

/// `harbor <db> backup [dir] [--format tsv|parquet] [--strict]`.
pub fn backup(db: &Path, args: &[String]) -> Result<(), String> {
    if !db.exists() {
        return Err(format!("{} does not exist — there is nothing to back up", db.display()));
    }
    let mut dir: Option<String> = None;
    let mut format = Format::Tsv;
    let mut strict = false;
    let mut rest = args.iter();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--format" => match rest.next() {
                Some(v) => format = Format::parse(v)?,
                None => return Err("--format needs a format (tsv or parquet)".into()),
            },
            "--strict" => strict = true,
            _ if a.starts_with('-') => return Err(format!("backup: unexpected option {a}")),
            _ if dir.is_none() => dir = Some(a.clone()),
            _ => return Err(format!("backup: unexpected argument {a}")),
        }
    }
    let dir = match dir {
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

    // A backup that fails takes its half-written directory with it. Anything
    // left behind would stand in the way of the retry that fixes the flag,
    // and would read as a backup while being a fragment of one.
    if let Err(e) = write_backup(db, &dir, format, strict) {
        let _ = fs::remove_dir_all(&dir);
        return Err(e);
    }

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

/// Everything between an empty directory and a finished backup. Split out so
/// that one `?` can undo all of it: no format carries every type, so a failure
/// here is an ordinary outcome rather than a surprise.
fn write_backup(db: &Path, dir: &Path, format: Format, strict: bool) -> Result<(), String> {
    let target = db.display().to_string();
    let sql = format!("EXPORT DATABASE {} ({})", quote(dir), format.options());
    harbor::repl::exec_quiet(&target, &[&sql], &[])?;

    // The loader is rewritten first, because reading it is how the tables
    // text cannot carry are found — and rewriting it is how they are fixed.
    let again = patch_loader(dir, format, strict)?;
    if !again.statements.is_empty() {
        let refs: Vec<&str> = again.statements.iter().map(String::as_str).collect();
        harbor::repl::exec_quiet(&target, &refs, &[])?;
        for stale in &again.replaced {
            fs::remove_file(stale).map_err(|e| format!("{}: {e}", stale.display()))?;
        }
        for note in &again.notes {
            eprintln!("harbor: {note}");
        }
    }
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

/// What a rewritten `load.sql` still needs doing to it.
struct Reformat {
    /// `COPY <table> TO '<dir>/<name>.parquet' (FORMAT parquet)`, one per
    /// table text cannot carry.
    statements: Vec<String>,
    /// The csv files the parquet replaces, removed once it is written.
    replaced: Vec<PathBuf>,
    /// One line each, for the operator: which table, and why.
    notes: Vec<String>,
}

/// Rewrite the generated `load.sql` into one that will still work later, and
/// report the tables that have to leave the text format behind.
///
/// Three edits, each for its own reason.
///
/// The paths come out absolute, because that is what `EXPORT DATABASE` was
/// handed, and an absolute path nails the directory to the machine that
/// wrote it — move it, rename it, `rsync` it, and every `COPY` still points
/// at where it used to be. `IMPORT DATABASE` resolves a relative path
/// against the directory it was given, so the file's own name is the only
/// spelling that is always right.
///
/// duckdb#25501: the COPY that EXPORT generates inherits
/// `allow_quoted_nulls=true` from the CSV reader's Pandas-compatibility
/// default (duckdb#7162), which reads a QUOTED null marker as a null — so
/// `"NULL"`, the one spelling that exists to escape the marker, comes back as
/// a null and the escape has no way to work. The writer is right and the
/// reader is wrong, so the fix goes on the reader. That half comes out once
/// 25501 lands; the rest stays.
///
/// And the two types text cannot hold, which are read out of `schema.sql` —
/// the artifact's own account of itself — and pointed at a parquet file
/// instead. `schema.sql` and `load.sql` spell a table the same way, quotes
/// and all, which is what lets one be looked up in the other.
fn patch_loader(dir: &Path, format: Format, strict: bool) -> Result<Reformat, String> {
    let path = dir.join("load.sql");
    let before = read(&path)?;
    let schema = read(&dir.join("schema.sql"))?;
    let mut again = Reformat { statements: vec![], replaced: vec![], notes: vec![] };

    let mut lines: Vec<String> = Vec::new();
    for line in before
        .replace(&format!("'{}/", dir.display()), "'")
        .replace("FORMAT 'csv'", "FORMAT 'csv', allow_quoted_nulls false")
        .lines()
    {
        let swap = reformat(dir, line, &schema, format);
        match swap {
            Some((swapped, statement, stale, note)) if !strict => {
                lines.push(swapped);
                again.statements.push(statement);
                again.replaced.push(stale);
                again.notes.push(note);
            }
            // --strict: the answer to "text cannot hold this" is an error
            // rather than a change of format, however well announced.
            Some((_, _, _, note)) => {
                let (head, why) = note.split_once(" — ").unwrap_or(("", ""));
                let table = head.split(" is ").next().unwrap_or("");
                return Err(format!(
                    "{table} cannot be written as {} — {why}. Drop --strict to write \
                     that one table as {} beside the rest, or --format {} for all of them",
                    format.name(),
                    format.other().name(),
                    format.other().flag()
                ));
            }
            None => lines.push(line.to_string()),
        }
    }

    let after = lines.join("\n") + "\n";
    if after == before && !before.trim().is_empty() {
        return Err(format!(
            "{} was not in the shape this patch expects — see duckdb#25501",
            path.display()
        ));
    }
    fs::write(&path, after).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(again)
}

/// Is this `COPY` line loading a table the chosen format cannot hold? If so,
/// the line that loads it from the OTHER format, the statement that writes
/// that file, the file it supersedes, and the word to say about it.
fn reformat(
    dir: &Path,
    line: &str,
    schema: &str,
    format: Format,
) -> Option<(String, String, PathBuf, String)> {
    let (table, why) = schema.lines().find_map(|create| {
        let why = format
            .cannot_hold()
            .iter()
            .find(|(spelling, _)| create.contains(spelling))
            .map(|(_, why)| *why)?;
        let table = table_of(create)?;
        line.starts_with(&format!("COPY {table} FROM '"))
            .then_some((table.to_string(), why))
    })?;

    // The file DuckDB chose, which is NOT always the table's name — a space
    // in one becomes an underscore in the other.
    let open = line.find('\'')? + 1;
    let name = line[open..]
        .split('\'')
        .next()?
        .strip_suffix(&format!(".{}", format.extension()))?
        .to_string();
    let instead = format.other();

    Some((
        format!(
            "COPY {table} FROM '{name}.{}' ({});",
            instead.extension(),
            instead.loader()
        ),
        format!(
            "COPY {table} TO {} ({})",
            quote(&dir.join(format!("{name}.{}", instead.extension()))),
            instead.options()
        ),
        dir.join(format!("{name}.{}", format.extension())),
        format!("{table} is {}, not {} — {why}", instead.name(), format.name()),
    ))
}

/// The identifier after `CREATE TABLE`, spelled the way `load.sql` spells it
/// too. A quoted name can hold anything, `(` included, so the quotes are
/// walked rather than searched past.
fn table_of(create: &str) -> Option<&str> {
    let rest = create.strip_prefix("CREATE TABLE ")?;
    if !rest.starts_with('"') {
        return rest.find('(').map(|end| &rest[..end]);
    }
    let bytes = rest.as_bytes();
    let mut i = 1;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            if bytes.get(i + 1) == Some(&b'"') {
                i += 2;
                continue;
            }
            return Some(&rest[..=i]);
        }
        i += 1;
    }
    None
}

fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
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
        // One file per table, whichever format it needed.
        if entry.path().extension().is_some_and(|e| e == "csv" || e == "parquet") {
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
