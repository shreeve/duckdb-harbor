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
//! is quoted on the way out. An empty string is left bare: once a null has a
//! name of its own an empty field can only be the empty string, and quotes on
//! every row of every file would be noise.
//!
//! With ONE exception, and it is a row of data rather than a matter of taste.
//! A one-column table holding an empty string writes an empty LINE, and every
//! CSV reader skips those — the row does not come back and nothing says so.
//! `FORCE_QUOTE` is the only lever DuckDB offers, and it takes a column list
//! rather than a predicate, so it is spent per FILE: a table whose export
//! contains a blank record is written again with every value quoted, and no
//! other file pays for it.
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

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use harbor::repl::{scan::{scan, Kind}, split_statements};

/// The writer's dialect. Backup and restore must agree on it, so it is said
/// once. `\t` is the two-character spelling DuckDB reads as a tab.
const DIALECT: &str = "FORMAT csv, DELIMITER '\\t', NULLSTR 'NULL'";

/// The same dialect with every value quoted, for the one file at a time that
/// needs it — see [`has_blank_record`]. Quoting is the only lever DuckDB
/// offers here (`FORCE_QUOTE` takes a column list, not a predicate), so it is
/// spent where a row would otherwise vanish and nowhere else.
const QUOTED: &str = "FORMAT csv, DELIMITER '\\t', NULLSTR 'NULL', FORCE_QUOTE *";

/// What the tables are written as.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    /// Tab-separated: greppable, diffable, editable. The default, because the
    /// artifact outliving its engine is most of the point.
    Tsv,
    /// Binary columnar output, with the compatibility checks below.
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
    harbor::repl::with_snapshot(&target, |execute| {
        let sql = format!("EXPORT DATABASE {} ({})", quote(dir), format.options());
        execute(&sql)?;
        let again = patch_loader(dir, format, strict)?;
        for statement in &again.statements {
            execute(statement)?;
        }
        for stale in &again.replaced {
            fs::remove_file(stale).map_err(|e| format!("{}: {e}", stale.display()))?;
        }
        for note in &again.notes {
            eprintln!("harbor: {note}");
        }
        Ok(())
    })
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
    /// The `COPY <table> TO ...` that fixes a table the first export got
    /// wrong — a different format, or the same one with quotes.
    statements: Vec<String>,
    /// Files a different format supersedes, removed once it is written.
    /// A requote overwrites its own file and adds nothing here.
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
    let schema = schema_types(&read(&dir.join("schema.sql"))?);
    let mut again = Reformat { statements: vec![], replaced: vec![], notes: vec![] };

    let mut lines: Vec<String> = Vec::new();
    for original in split_statements(&before) {
        let (table, _, name) = copy_parts(&original)
            .ok_or_else(|| format!("unrecognized backup COPY statement: {original}"))?;
        let file = Path::new(&name).file_name()
            .ok_or_else(|| format!("invalid backup path: {name}"))?;
        let line = format!("COPY {table} FROM {} ({})", quote(Path::new(file)), format.loader());
        let swap = reformat(dir, &line, &schema, format)?;
        match swap {
            Some(TableRewrite { loader, statement, stale, note }) if !strict => {
                lines.push(loader);
                again.statements.push(statement);
                again.replaced.push(stale);
                again.notes.push(note);
            }
            // --strict: the answer to "text cannot hold this" is an error
            // rather than a change of format, however well announced.
            Some(TableRewrite { note, .. }) => {
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
            // No format change wanted. But a value can still be written in a
            // way the reader will not give back, and that is not a matter of
            // format — so it is checked here, on the file itself.
            None => {
                if format == Format::Tsv
                    && let Some((statement, note)) = requote(dir, &line)?
                {
                    again.statements.push(statement);
                    again.notes.push(note);
                }
                lines.push(line.to_string());
            }
        }
    }

    let after = lines.iter().map(|s| format!("{};\n", s.trim_end_matches(';'))).collect::<String>();
    fs::write(&path, after).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(again)
}

struct TableRewrite {
    loader: String,
    statement: String,
    stale: PathBuf,
    note: String,
}

/// Is this `COPY` line loading a table the chosen format cannot hold? If so,
/// the line that loads it from the OTHER format, the statement that writes
/// that file, the file it supersedes, and the word to say about it.
fn reformat(
    dir: &Path,
    line: &str,
    schema: &HashMap<String, String>,
    format: Format,
) -> Result<Option<TableRewrite>, String> {
    let (table, _, path) = copy_parts(line).ok_or("invalid backup COPY path")?;
    let types = schema.get(table).ok_or_else(|| format!("no schema for backup table {table}"))?;
    let Some((_, why)) = format.cannot_hold().iter().find(|(ty, _)| types.contains(ty)) else {
        return Ok(None);
    };
    if let Some((_, why)) = format.other().cannot_hold().iter().find(|(ty, _)| types.contains(ty)) {
        return Err(format!("{table} cannot round-trip in either backup format: {why}"));
    }

    // The export filename need not match the table's SQL identifier.
    let name = path.strip_suffix(&format!(".{}", format.extension()))
        .ok_or("unexpected backup file extension")?;
    let instead = format.other();

    Ok(Some(TableRewrite {
        loader: format!(
            "COPY {table} FROM {} ({})",
            quote(Path::new(&format!("{name}.{}", instead.extension()))),
            instead.loader()
        ),
        statement: format!(
            "COPY {table} TO {} ({})",
            quote(&dir.join(format!("{name}.{}", instead.extension()))),
            instead.options()
        ),
        stale: dir.join(format!("{name}.{}", format.extension())),
        note: format!("{table} is {}, not {} — {why}", instead.name(), format.name()),
    }))
}

/// Does this table's export hold a record the reader would throw away? If so,
/// the statement that writes the file again with every value quoted, and the
/// word to say about the one file that will not look like the others.
///
/// The condition is exact because it is the failure itself rather than a
/// proxy for it: a blank line is what a CSV reader skips, so a file without
/// one cannot lose a row and pays nothing.
fn requote(dir: &Path, line: &str) -> Result<Option<(String, String)>, String> {
    let Some((table, _, name)) = copy_parts(line) else {
        return Err(format!("unrecognized backup COPY statement: {line}"));
    };
    let file = dir.join(name);
    let input = fs::File::open(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    if !has_blank_record(BufReader::new(input))
        .map_err(|e| format!("{}: {e}", file.display()))?
    {
        return Ok(None);
    }
    Ok(Some((
        format!("COPY {table} TO {} ({QUOTED})", quote(&file)),
        format!(
            "{table} is quoted throughout — it holds an empty string in a single \
             column, which unquoted is an empty line, which a reader skips"
        ),
    )))
}

/// Is any RECORD in this csv an empty line?
///
/// Records, not lines: a value holding a newline spans several physical
/// lines, and a blank one INSIDE the quotes is part of the value and comes
/// back fine. Only a blank line between records is lost, so the quotes have
/// to be walked — which is the same thing the reader does.
fn has_blank_record(mut input: impl BufRead) -> std::io::Result<bool> {
    let (mut quoted, mut at_record_start) = (false, true);
    loop {
        let chunk = input.fill_buf()?;
        if chunk.is_empty() {
            return Ok(false);
        }
        for &ch in chunk {
            match ch {
                b'"' => {
                    quoted = !quoted;
                    at_record_start = false;
                }
                b'\n' if !quoted => {
                    if at_record_start {
                        return Ok(true);
                    }
                    at_record_start = true;
                }
                _ if !quoted => at_record_start = false,
                _ => {}
            }
        }
        let n = chunk.len();
        input.consume(n);
    }
}

/// The identifier after `CREATE TABLE`, spelled the way `load.sql` spells it
/// too. A quoted name can hold anything, `(` included, so the quotes are
/// walked rather than searched past.
fn table_of(create: &str) -> Option<&str> {
    let rest = create.strip_prefix("CREATE TABLE ")?;
    for span in scan(rest) {
        if span.kind == Kind::Code
            && let Some(end) = rest[span.start..span.end].find('(')
        {
            return Some(rest[..span.start + end].trim_end());
        }
    }
    None
}

/// Generated SQL still permits arbitrary quoted identifiers and string paths.
/// Use the same scanner as the REPL, including escaped quotes and newlines.
fn copy_parts(sql: &str) -> Option<(&str, std::ops::Range<usize>, String)> {
    for span in scan(sql) {
        if span.kind == Kind::Str && sql.as_bytes()[span.start] == b'\'' && span.terminated {
            let table = sql[..span.start].trim_end().strip_prefix("COPY ")?
                .strip_suffix(" FROM")?.trim_end();
            let path = sql[span.start + 1..span.end - 1].replace("''", "'");
            return Some((table, span.start..span.end, path));
        }
    }
    None
}

fn unquoted_code(sql: &str) -> String {
    scan(sql).iter().map(|span| {
        if span.kind == Kind::Code { &sql[span.start..span.end] } else { " " }
    }).collect()
}

/// Index the schema once, rather than rescanning every CREATE for each table.
fn schema_types(sql: &str) -> HashMap<String, String> {
    split_statements(sql).iter().filter_map(|create| {
        Some((table_of(create)?.to_string(), unquoted_code(create)))
    }).collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_records_are_scanned_across_buffer_boundaries() {
        for (text, expected) in [
            ("x\n\n", true), ("x\n\"\"\n", false), ("x\n\"line\n\nend\"\n", false),
            ("x\n\"a\"\"b\"\n\n", true), ("\n", true), ("x\nvalue\n", false),
        ] {
            for capacity in 1..8 {
                let input = BufReader::with_capacity(capacity, text.as_bytes());
                assert_eq!(has_blank_record(input).unwrap(), expected, "{text:?}, {capacity}");
            }
        }
    }

    #[test]
    fn generated_sql_preserves_quoted_schemas_and_paths() {
        let table = "\"odd schema\".\"a(\"\"b\n'c\"";
        assert_eq!(table_of(&format!("CREATE TABLE {table}(v INTEGER);")), Some(table));
        let sql = format!("COPY {table} FROM '/tmp/it''s here/a.csv' (FORMAT 'csv');");
        let (got, span, path) = copy_parts(&sql).unwrap();
        assert_eq!(got, table);
        assert_eq!(path, "/tmp/it's here/a.csv");
        assert_eq!(&sql[span], "'/tmp/it''s here/a.csv'");
        assert_eq!(split_statements(&sql).len(), 1);
    }

    #[test]
    fn incompatible_formats_fail_instead_of_losing_values() {
        let schema = schema_types("CREATE TABLE t(u UNION(x INTEGER), z TIME WITH TIME ZONE)");
        for (format, path) in [(Format::Tsv, "t.csv"), (Format::Parquet, "t.parquet")] {
            assert!(reformat(Path::new("/tmp"), &format!("COPY t FROM '{path}'"), &schema, format).is_err());
        }
        let schema = schema_types("CREATE TABLE t(\"VARIANT\" VARCHAR DEFAULT 'UNION(x INT)')");
        assert!(reformat(Path::new("/tmp"), "COPY t FROM 't.csv'", &schema, Format::Tsv).unwrap().is_none());
    }
}
