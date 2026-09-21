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
//! A `VARIANT` column travels as JSON. Its display rendering cannot come
//! back — the number 42 and the string "42" both print as `42`, and the
//! reader hands every cell back as a string — but JSON tells them apart,
//! and a value that entered as JSON returns exactly as it entered: every
//! inner type, every nesting, byte for byte. So the column is cast to JSON
//! on the way out and decoded on the way in. What JSON has no word for — a
//! DATE, a DECIMAL, a BLOB, a TIMESTAMP put inside a variant from SQL —
//! comes back as JSON's nearest type, and the backup says so, once per
//! column, when it happens; `--format parquet` keeps those. The decode is
//! an UPDATE, and `IMPORT DATABASE` takes nothing but COPY, so it lives in
//! `after.sql` beside `load.sql`: a stock `duckdb` importing the directory by
//! hand gets the JSON text and can run the second file itself. `restore`
//! does not need the UPDATE. It runs the directory's statements itself and
//! loads such a table as documents (see [`restore_plan`]), so a CHECK on a
//! VARIANT column is only ever shown the document.
//!
//! One type text cannot hold at all — `UNION`, which loses its tag — is
//! written as parquet instead, one file, beside the others. `load.sql` names
//! the format per table, so the directory stays self-describing and the
//! choice is visible in `ls`. Text for what text can carry, parquet only
//! where it must. A `VARIANT` nested inside another type is beyond both:
//! the JSON route cannot reach it and parquet has no writer for it, so the
//! backup refuses rather than write what will not come back.
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

/// The one-line companion of `load.sql`: what to run after it. Only
/// [`restore`] reads it; DuckDB's own IMPORT never will.
const AFTER: &str = "after.sql";

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
    /// Text loses a UNION's tag (the restore then refuses). Parquet
    /// normalises a TIMETZ to UTC,
    /// so `12:00:00+02:30` returns as `09:30:00+00` — the same instant,
    /// a different value, and nothing said. Each hole is the other format's
    /// solid ground, which is what makes a mixed directory the answer.
    ///
    /// A VARIANT nested inside another type is in BOTH lists, and that is
    /// the point: text retypes it (the JSON route reaches only a plain
    /// column, see [`json_out`]) and parquet has no writer for a variant
    /// below the root, so a table holding one is refused whichever format
    /// was asked for. A plain VARIANT column is in neither: it travels as
    /// JSON under text and as itself under parquet.
    ///
    /// Not every hole is a TYPE. Parquet also refuses a NEGATIVE interval,
    /// which is a value, invisible in a schema and impossible to route
    /// around from here — and does not need to be, because it fails loudly.
    fn cannot_hold(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Format::Tsv => &[
                ("VARIANT", "a VARIANT nested inside another type comes back retyped"),
                ("UNION(", "a UNION loses its tag"),
            ],
            Format::Parquet => &[
                ("VARIANT", "parquet has no writer for a VARIANT nested inside another type"),
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
        let again = patch_loader(dir, format, strict, execute)?;
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
    let after = dir.join(AFTER);
    let plan = restore_plan(
        &dir,
        &read(&dir.join("schema.sql"))?,
        &read(&dir.join("load.sql"))?,
        &if after.exists() { read(&after)? } else { String::new() },
    )?;
    let mut sql: Vec<&str> = plan.iter().map(String::as_str).collect();
    sql.push("CHECKPOINT");
    if let Err(e) = harbor::repl::exec_quiet(&target, &sql, &spawn) {
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

/// Every statement a restore runs, in order: `schema.sql` as written, each
/// table's load, then whatever of `after.sql` the loads have not already done.
///
/// This is `IMPORT DATABASE` spelled out. That statement is the two files run
/// in order, each COPY's file joined to the directory, and nothing more; it
/// is spelled out because one kind of load has to differ. A plain VARIANT
/// column travels as JSON text. A COPY straight into the table lands each
/// cell as a VARIANT *string* for `after.sql` to decode, and a CHECK on the
/// column is tested in between, against the string: `variant_typeof(v) LIKE
/// 'OBJECT%'` refuses every row. So a table `after.sql` would decode is
/// loaded as documents instead. The COPY from `load.sql`, options and all,
/// fills a staging table that differs from the real one only in holding
/// those columns as VARCHAR, so every other column is parsed exactly as it
/// would have been; one INSERT then moves the rows across with the text cast
/// through JSON, and the staging table is dropped. The table never holds a
/// VARIANT string, and its decode is left out of what runs afterwards.
///
/// Which tables those are is the backup's own record: the decode statement
/// [`json_in`] wrote for the table is in `after.sql`. A table without one,
/// such as one written as parquet or a directory with no `after.sql` at
/// all, is loaded by its COPY as written.
fn restore_plan(dir: &Path, schema_sql: &str, load_sql: &str, after_sql: &str) -> Result<Vec<String>, String> {
    let schema = schema_types(schema_sql);
    let mut decode: Vec<String> = split_statements(after_sql).iter().map(|s| uncommented(s).to_string()).collect();
    let stage = stage_name(schema_sql);
    let mut plan = split_statements(schema_sql);
    for line in split_statements(load_sql) {
        let (table, literal, name) = copy_parts(&line)
            .ok_or_else(|| format!("unrecognized backup COPY statement: {line}"))?;
        let file = Path::new(&name).file_name().ok_or_else(|| format!("invalid backup path: {name}"))?;
        let copy_into = |target: &str| {
            format!("COPY {target} FROM {}{}", quote(&dir.join(file)), &line[literal.end..])
        };
        let variants = schema.get(table).map_or(&[][..], |t| &t.variants[..]);
        let decoded = (!variants.is_empty())
            .then(|| json_in(table, variants))
            .and_then(|expected| decode.iter().position(|s| *s == expected));
        match decoded {
            Some(at) => {
                decode.remove(at);
                plan.push(format!("CREATE TABLE {stage} AS SELECT * REPLACE ({}) FROM {table} LIMIT 0", cast_each(variants, "VARCHAR")));
                plan.push(copy_into(&stage));
                plan.push(format!("INSERT INTO {table} SELECT * REPLACE ({}) FROM {stage}", cast_each(variants, "JSON::VARIANT")));
                plan.push(format!("DROP TABLE {stage}"));
            }
            None => plan.push(copy_into(table)),
        }
    }
    plan.extend(decode);
    Ok(plan)
}

/// `"a"::TYPE AS "a", "b"::TYPE AS "b"` — the body of a `SELECT * REPLACE`.
fn cast_each(columns: &[String], to: &str) -> String {
    columns.iter()
        .map(|c| format!("{}::{to} AS {}", ident(c), ident(c)))
        .collect::<Vec<_>>().join(", ")
}

/// A table name the schema does not mention anywhere, for a load to be staged
/// in. Identifiers fold case, so the search does too.
fn stage_name(schema_sql: &str) -> String {
    let taken = schema_sql.to_ascii_lowercase();
    (0..).map(|n| format!("harbor_restore_{n}"))
        .find(|name| !taken.contains(name.as_str()))
        .expect("an unbounded search")
}

/// A statement without the comments written above it.
fn uncommented(statement: &str) -> &str {
    let code = scan(statement).into_iter().find(|span| match span.kind {
        Kind::LineComment | Kind::BlockComment => false,
        Kind::Code => !statement[span.start..span.end].trim().is_empty(),
        _ => true,
    });
    match code {
        Some(span) if span.kind == Kind::Code => statement[span.start..].trim_start(),
        Some(span) => &statement[span.start..],
        None => "",
    }
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
/// And the shapes text cannot hold, which are read out of `schema.sql` —
/// the artifact's own account of itself — and pointed at a parquet file
/// instead. `schema.sql` and `load.sql` spell a table the same way, quotes
/// and all, which is what lets one be looked up in the other.
///
/// A plain VARIANT column is the fourth edit, and the one that runs NOW
/// rather than being handed back: the table's file is written again with
/// the column cast to JSON (see [`json_out`]), because the blank-record
/// check below has to look at the file that will actually be restored, not
/// the one EXPORT wrote. The decode goes to `after.sql`.
fn patch_loader(
    dir: &Path,
    format: Format,
    strict: bool,
    execute: &dyn Fn(&str) -> Result<(), String>,
) -> Result<Reformat, String> {
    let path = dir.join("load.sql");
    let before = read(&path)?;
    let schema = schema_types(&read(&dir.join("schema.sql"))?);
    let mut again = Reformat { statements: vec![], replaced: vec![], notes: vec![] };
    let mut after: Vec<String> = Vec::new();

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
                if format == Format::Tsv {
                    let variants = &schema[table].variants;
                    let source = if variants.is_empty() {
                        table.to_string()
                    } else {
                        let source = json_out(table, variants);
                        execute(&format!("COPY {source} TO {} ({DIALECT})", quote(Path::new(&name))))?;
                        for (column, held) in json_check(execute, dir, table, variants)? {
                            if strict {
                                return Err(format!(
                                    "{table}.{} holds {held} — a VARIANT is written as JSON, which has \
                                     no such type. Drop --strict to write it as JSON anyway, or \
                                     --format parquet to keep it",
                                    ident(&column)
                                ));
                            }
                            again.notes.push(format!(
                                "{table}.{} holds {held} — written as JSON, which has no such type; \
                                 --format parquet keeps it",
                                ident(&column)
                            ));
                        }
                        after.push(json_in(table, variants));
                        source
                    };
                    if let Some((statement, note)) = requote(dir, &line, &source)? {
                        again.statements.push(statement);
                        again.notes.push(note);
                    }
                }
                lines.push(line.to_string());
            }
        }
    }

    let text = lines.iter().map(|s| format!("{};\n", s.trim_end_matches(';'))).collect::<String>();
    fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    if !after.is_empty() {
        let text = format!(
            "-- Run after load.sql: the VARIANT columns above travelled as JSON text, and\n\
             -- IMPORT DATABASE takes nothing but COPY, so their decode is here.\n{}",
            after.iter().map(|s| format!("{s};\n")).collect::<String>()
        );
        let path = dir.join(AFTER);
        fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(again)
}

/// The table as it goes OUT when some of its columns are plain VARIANT: every
/// column as it is, those cast to JSON. `SELECT *` with a REPLACE keeps the
/// column order and never has to spell the others.
fn json_out(table: &str, variants: &[String]) -> String {
    format!("(SELECT * REPLACE ({}) FROM {table})", cast_each(variants, "JSON"))
}

/// The decode that brings those columns back IN. The COPY in `load.sql`
/// hands each cell over as a VARIANT holding a string — the JSON text — and
/// reading that string as JSON gives the value the string described.
fn json_in(table: &str, variants: &[String]) -> String {
    let set = variants.iter()
        .map(|c| format!("{} = {}::VARCHAR::JSON::VARIANT", ident(c), ident(c)))
        .collect::<Vec<_>>().join(", ");
    format!("UPDATE {table} SET {set}")
}

/// Which of a table's plain VARIANT columns hold something JSON cannot carry,
/// and what: `(column, "DATE, DECIMAL")`, one entry per column that would
/// change, none when every cell survives the round trip.
///
/// A cell survives when it equals itself after a trip through JSON, which
/// is the failure itself rather than a proxy for it, and is decided by the
/// engine. What it cannot see is an integer stored from SQL as a narrow
/// type coming back as JSON's wide one — the same value, a different width
/// label — which is what "written as JSON" means and is not reported.
///
/// The engine answers through a file: the backup session runs statements
/// and returns nothing, and a `COPY (…) TO` is the one way it can be asked
/// a question. The probe is removed as soon as it is read.
fn json_check(
    execute: &dyn Fn(&str) -> Result<(), String>,
    dir: &Path,
    table: &str,
    variants: &[String],
) -> Result<Vec<(String, String)>, String> {
    let probe = dir.join(".variant-check");
    let selects = variants.iter().map(|c| {
        let c = ident(c);
        // Ordered, so the note reads the same whichever engine build answers.
        format!(
            "coalesce(string_agg(DISTINCT variant_type({c}), ', ' ORDER BY variant_type({c})) \
             FILTER (WHERE NOT coalesce({c} = {c}::JSON::VARIANT, true)), '')"
        )
    }).collect::<Vec<_>>().join(", ");
    execute(&format!(
        "COPY (SELECT {selects} FROM {table}) TO {} (FORMAT csv, DELIMITER '\\t', HEADER false)",
        quote(&probe)
    ))?;
    let text = read(&probe)?;
    let _ = fs::remove_file(&probe);
    let fields = text.trim_end_matches(['\n', '\r']).split('\t');
    Ok(variants.iter().zip(fields).filter_map(|(column, field)| {
        // The writer quotes an empty string; an inner-type list never needs it.
        let held = field.strip_prefix('"').and_then(|f| f.strip_suffix('"')).unwrap_or(field);
        (!held.is_empty()).then(|| (column.clone(), held.to_string()))
    }).collect())
}

/// A column name as a SQL identifier — always quoted, which is always right.
fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[derive(Debug)]
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
    schema: &HashMap<String, TableSchema>,
    format: Format,
) -> Result<Option<TableRewrite>, String> {
    let (table, _, path) = copy_parts(line).ok_or("invalid backup COPY path")?;
    let types = &schema.get(table).ok_or_else(|| format!("no schema for backup table {table}"))?.types;
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
fn requote(dir: &Path, line: &str, source: &str) -> Result<Option<(String, String)>, String> {
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
        format!("COPY {source} TO {} ({QUOTED})", quote(&file)),
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

/// One table as `schema.sql` declares it, reduced to what decides how it
/// travels.
struct TableSchema {
    /// Every definition except the plain VARIANT columns, as code with the
    /// strings and identifiers blanked: what the chosen format has to hold
    /// on its own, searched for the type names it cannot.
    types: String,
    /// The plain VARIANT columns, unquoted, in declaration order. These are
    /// not the format's problem: they travel as JSON whatever it is.
    variants: Vec<String>,
}

/// Index the schema once, rather than rescanning every CREATE for each table.
fn schema_types(sql: &str) -> HashMap<String, TableSchema> {
    split_statements(sql).iter().filter_map(|create| {
        let table = table_of(create)?.to_string();
        let (mut types, mut variants) = (Vec::new(), Vec::new());
        for (name, code) in definitions(create) {
            match name {
                Some(name) if is_plain_variant(&code) => variants.push(name),
                _ => types.push(code),
            }
        }
        Some((table, TableSchema { types: types.join(", "), variants }))
    }).collect()
}

/// `VARIANT` and nothing else for a type, whatever constraints follow it. A
/// `VARIANT[]` or a `STRUCT(v VARIANT)` is a shape text cannot reach as JSON
/// and stays the format's concern.
fn is_plain_variant(code: &str) -> bool {
    let upper = code.to_ascii_uppercase();
    upper == "VARIANT" || upper.starts_with("VARIANT ")
}

/// The definitions inside a `CREATE TABLE`'s parentheses, each as (column
/// name, the rest as blanked code) — or (None, code) for a constraint clause
/// such as `PRIMARY KEY (a, b)`, which has no column. The parentheses and
/// commas that split them are walked with the scanner, since a quoted name
/// or a default string can hold either, and a type such as `DECIMAL(5, 2)`
/// holds both.
fn definitions(create: &str) -> Vec<(Option<String>, String)> {
    let Some(table) = table_of(create) else { return Vec::new() };
    let head = "CREATE TABLE ".len() + table.len();
    let Some(open) = create[head..].find('(').map(|i| head + i) else { return Vec::new() };
    let body = &create[open + 1..];
    let mut spans_of: Vec<(usize, usize)> = Vec::new();
    let (mut depth, mut start) = (1usize, 0usize);
    'walk: for span in scan(body) {
        if span.kind != Kind::Code {
            continue;
        }
        for (i, &b) in body.as_bytes()[span.start..span.end].iter().enumerate() {
            let at = span.start + i;
            match b {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        spans_of.push((start, at));
                        break 'walk;
                    }
                }
                b',' if depth == 1 => {
                    spans_of.push((start, at));
                    start = at + 1;
                }
                _ => {}
            }
        }
    }
    spans_of.into_iter().filter_map(|(s, e)| {
        let def = body[s..e].trim_start();
        if def.is_empty() {
            return None;
        }
        if let Some(quoted) = def.strip_prefix('"') {
            // A quoted name: up to the first `"` that is not doubled.
            let (mut i, mut end) = (0, None);
            while let Some(j) = quoted[i..].find('"').map(|j| i + j) {
                if quoted.as_bytes().get(j + 1) == Some(&b'"') {
                    i = j + 2;
                } else {
                    end = Some(j);
                    break;
                }
            }
            let end = end?;
            let name = quoted[..end].replace("\"\"", "\"");
            return Some((Some(name), unquoted_code(&quoted[end + 1..]).trim().to_string()));
        }
        let n = def.find(|c: char| c.is_whitespace() || c == '(').unwrap_or(def.len());
        let (word, rest) = def.split_at(n);
        let constraint = matches!(
            word.to_ascii_uppercase().as_str(),
            "CONSTRAINT" | "PRIMARY" | "UNIQUE" | "CHECK" | "FOREIGN"
        );
        let code = unquoted_code(if constraint { def } else { rest }).trim().to_string();
        Some((if constraint { None } else { Some(word.to_string()) }, code))
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
    fn definitions_find_plain_variant_columns_and_nothing_else() {
        let schema = schema_types(
            "CREATE TABLE \"odd name\"(id INTEGER PRIMARY KEY, v VARIANT, \"quoted \"\"col\"\"\" VARIANT NOT NULL, \
             n INTEGER DEFAULT(3), CHECK((n > 0)), d VARCHAR DEFAULT 'a, (b) VARIANT', PRIMARY KEY (id, n));\n\
             CREATE TABLE x.t(s STRUCT(a VARIANT), l VARIANT[], u UNION(num INTEGER, str VARCHAR), plain VARCHAR);\n\
             CREATE TABLE \"select\"(\"VARIANT\" VARCHAR DEFAULT 'UNION(x INT)');",
        );
        let odd = &schema["\"odd name\""];
        assert_eq!(odd.variants, vec!["v".to_string(), "quoted \"col\"".to_string()]);
        assert!(!odd.types.contains("VARIANT"), "{}", odd.types);
        assert!(odd.types.contains("INTEGER PRIMARY KEY"));
        let nested = &schema["x.t"];
        assert!(nested.variants.is_empty());
        assert!(nested.types.contains("STRUCT(a VARIANT)") && nested.types.contains("UNION("));
        let named = &schema["\"select\""];
        assert!(named.variants.is_empty());
        assert!(!named.types.contains("UNION("));
        assert_eq!(
            json_out("t", &odd.variants),
            "(SELECT * REPLACE (\"v\"::JSON AS \"v\", \"quoted \"\"col\"\"\"::JSON AS \"quoted \"\"col\"\"\") FROM t)"
        );
        assert_eq!(
            json_in("t", &["v".to_string()]),
            "UPDATE t SET \"v\" = \"v\"::VARCHAR::JSON::VARIANT"
        );
    }

    #[test]
    fn a_restore_loads_documents_as_documents() {
        let schema = "CREATE TABLE g(id INTEGER, v VARIANT, \"w w\" VARIANT NOT NULL, CHECK((variant_typeof(v) ~~ 'OBJECT%')));\n\
                      CREATE TABLE plain(id INTEGER);\n\
                      CREATE TABLE u(v VARIANT, x UNION(a INTEGER));\n\
                      CREATE VIEW Harbor_Restore_0 AS SELECT 1;\n";
        let load = "COPY g FROM 'g.csv' (FORMAT 'csv', header 1, nullstr 'NULL');\n\
                    COPY plain FROM '/somewhere/else/plain.csv' (FORMAT 'csv');\n\
                    COPY u FROM 'u.parquet' (FORMAT 'parquet');\n";
        let decode = json_in("g", &["v".to_string(), "w w".to_string()]);
        let after = format!("-- Run after load.sql.\n-- Two lines of it.\n{decode};\nUPDATE elsewhere SET x = 1;\n");
        let dir = Path::new("/it's here");
        assert_eq!(restore_plan(dir, schema, load, &after).unwrap(), [
            "CREATE TABLE g(id INTEGER, v VARIANT, \"w w\" VARIANT NOT NULL, CHECK((variant_typeof(v) ~~ 'OBJECT%')))",
            "CREATE TABLE plain(id INTEGER)",
            "CREATE TABLE u(v VARIANT, x UNION(a INTEGER))",
            "CREATE VIEW Harbor_Restore_0 AS SELECT 1",
            "CREATE TABLE harbor_restore_1 AS SELECT * REPLACE (\"v\"::VARCHAR AS \"v\", \"w w\"::VARCHAR AS \"w w\") FROM g LIMIT 0",
            "COPY harbor_restore_1 FROM '/it''s here/g.csv' (FORMAT 'csv', header 1, nullstr 'NULL')",
            "INSERT INTO g SELECT * REPLACE (\"v\"::JSON::VARIANT AS \"v\", \"w w\"::JSON::VARIANT AS \"w w\") FROM harbor_restore_1",
            "DROP TABLE harbor_restore_1",
            "COPY plain FROM '/it''s here/plain.csv' (FORMAT 'csv')",
            "COPY u FROM '/it''s here/u.parquet' (FORMAT 'parquet')",
            "UPDATE elsewhere SET x = 1",
        ]);
        // A directory with no after.sql holds no JSON text: every table is
        // loaded by its COPY.
        let plan = restore_plan(dir, schema, load, "").unwrap();
        assert_eq!(plan[4], "COPY g FROM '/it''s here/g.csv' (FORMAT 'csv', header 1, nullstr 'NULL')");
        assert_eq!(plan.len(), 7);
    }

    #[test]
    fn incompatible_formats_fail_instead_of_losing_values() {
        let schema = schema_types("CREATE TABLE t(u UNION(x INTEGER), z TIME WITH TIME ZONE)");
        for (format, path) in [(Format::Tsv, "t.csv"), (Format::Parquet, "t.parquet")] {
            assert!(reformat(Path::new("/tmp"), &format!("COPY t FROM '{path}'"), &schema, format).is_err());
        }
        let schema = schema_types("CREATE TABLE t(\"VARIANT\" VARCHAR DEFAULT 'UNION(x INT)')");
        assert!(reformat(Path::new("/tmp"), "COPY t FROM 't.csv'", &schema, Format::Tsv).unwrap().is_none());
        // A plain VARIANT column is text's to carry, as JSON, and parquet's as
        // itself; a nested one is beyond both and refused either way.
        let schema = schema_types("CREATE TABLE t(v VARIANT); CREATE TABLE n(s STRUCT(v VARIANT))");
        assert!(reformat(Path::new("/tmp"), "COPY t FROM 't.csv'", &schema, Format::Tsv).unwrap().is_none());
        assert!(reformat(Path::new("/tmp"), "COPY t FROM 't.parquet'", &schema, Format::Parquet).unwrap().is_none());
        for (format, path) in [(Format::Tsv, "n.csv"), (Format::Parquet, "n.parquet")] {
            let err = reformat(Path::new("/tmp"), &format!("COPY n FROM '{path}'"), &schema, format).unwrap_err();
            assert!(err.contains("either backup format"), "{err}");
        }
    }
}
