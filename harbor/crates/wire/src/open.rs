//! Opening a file that is not a DuckDB database: what kind it is, and the
//! SQL that opens it in a workspace.
//!
//! A file is known by its first bytes where its kind has a signature, and by
//! its name where it has none. [`classify`] reads both and nothing else: the
//! caller reads the bytes ([`HEAD_LEN`] of them, fewer for a shorter file),
//! so the same rule answers for the `harbor` command line and for DuckTable,
//! and a test can state it without a file. A name whose kind has a signature
//! the bytes do not carry is refused, as is anything else that cannot be
//! told, with what was looked at; nothing is guessed.
//!
//! A database opens as itself, or, for SQLite, as a catalog attached in the
//! workspace ([`attach_sql`]). Everything else DuckDB reads opens as a view
//! in the workspace ([`view_sql`]) that reads the file where it lies, named
//! for the file ([`view_name`], then [`free_name`]). Every path reaches SQL
//! as a string literal and every name as a quoted identifier, and a path
//! handed to one of DuckDB's multi-file readers has its glob characters
//! bracketed, since those readers take their argument as a pattern: `a*.csv`
//! would read `ab.csv` too, and `s[ab].csv` would read `sa.csv` instead.
//!
//! A view is kept in the workspace's own catalog; an attachment is kept in
//! its [`Record`], from which every server that opens the workspace attaches
//! the file again, after loading the extensions its views call into
//! ([`extensions`]).

/// How many leading bytes [`classify`] looks at: SQLite's header string is
/// the longest signature.
pub const HEAD_LEN: usize = 16;

const SQLITE: &[u8; 16] = b"SQLite format 3\0";
const PARQUET: &[u8; 4] = b"PAR1";
const ZIP: &[u8; 4] = b"PK\x03\x04";
const GZIP: &[u8; 2] = b"\x1f\x8b";
/// A shapefile's file code, 9994 big-endian.
const SHAPEFILE: &[u8; 4] = b"\x00\x00\x27\x0a";

/// What a path holds, as far as opening it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A DuckDB database, or a path that becomes one: opened as itself.
    Duckdb,
    /// A SQLite database: attached in the workspace.
    Sqlite,
    /// A file DuckDB reads: a view in the workspace.
    Data(Reader),
}

/// The DuckDB function a data file's view reads it with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reader {
    /// `.csv`, `.tsv`, `.txt`, `.csv.gz`: `read_csv`, dialect detected.
    Csv,
    /// A `.parquet` file: `read_parquet`.
    Parquet,
    /// A folder: `read_parquet` over every `.parquet` file beneath it, its
    /// `key=value` directories read as columns.
    ParquetFolder,
    /// `.json`, `.ndjson`, `.jsonl`: `read_json`, format detected.
    Json,
    /// `.xlsx`: `read_xlsx`, its first sheet. The `excel` extension names
    /// no sheets without reading the workbook, so one view reads the first.
    Xlsx,
    /// `.geojson`, `.shp`, `.gpkg`: `ST_Read`, the first layer. The
    /// `spatial` extension does not load on demand, so the caller loads it
    /// first ([`Reader::extension`]), and so does every server that opens the
    /// workspace ([`extensions`]).
    Spatial,
}

/// What the caller found at a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry<'a> {
    /// Nothing is there.
    Missing,
    /// A directory.
    Dir,
    /// A file, and its first bytes: [`HEAD_LEN`] of them, or all of a
    /// shorter file.
    File(&'a [u8]),
}

/// The kind of what is at `path`, from its name and from `entry`.
///
/// A DuckDB file is known by `DUCK` at byte 8 under any name, and a path
/// ending `.duckdb` or `.ddb` opens as a database whatever it holds, as does
/// an empty file or a missing path with no data file's name, since DuckDB
/// makes a database there. A missing path named as a data file or a SQLite
/// file is refused rather than made into a database. A directory is a
/// folder of Parquet.
pub fn classify(path: &str, entry: Entry) -> Result<Kind, String> {
    let name = file_name(path);
    let ext = extension(name);
    let head = match entry {
        Entry::Dir => return Ok(Kind::Data(Reader::ParquetFolder)),
        Entry::Missing => {
            return match by_name(&ext) {
                Named::Database | Named::Either | Named::Unknown => Ok(Kind::Duckdb),
                Named::Kind(_) | Named::Signed(..) => Err(format!("{path}: no such file")),
            };
        }
        Entry::File(head) => head,
    };
    if head.get(8..12) == Some(b"DUCK") {
        return Ok(Kind::Duckdb);
    }
    // A GeoPackage is a SQLite database, and opens as the layer it holds.
    if ext != "gpkg" && head.starts_with(SQLITE) {
        return Ok(Kind::Sqlite);
    }
    if head.starts_with(PARQUET) {
        return Ok(Kind::Data(Reader::Parquet));
    }
    match by_name(&ext) {
        Named::Database => Ok(Kind::Duckdb),
        Named::Kind(kind) => Ok(kind),
        Named::Signed(kind, signature, _) if head.starts_with(signature) => Ok(kind),
        Named::Signed(_, _, what) => Err(format!("{path}: named .{ext}, but {} — not {what}", begins(head))),
        Named::Either | Named::Unknown if head.is_empty() => Ok(Kind::Duckdb),
        Named::Either => Err(format!(
            "{path}: named .{ext}, but {} — not a DuckDB or SQLite database",
            begins(head)
        )),
        Named::Unknown => Err(format!(
            "{path}: not a kind harbor opens — {}, and {}; it opens DuckDB and SQLite databases, \
             .csv .tsv .txt .csv.gz, .parquet or a folder of them, .json .ndjson .jsonl, .xlsx, \
             and .geojson .shp .gpkg",
            match ext.is_empty() {
                true => "its name has no extension".to_string(),
                false => format!("its name ends .{ext}"),
            },
            begins(head),
        )),
    }
}

/// What a name alone says.
enum Named {
    /// `.duckdb`, `.ddb`.
    Database,
    /// A kind with no signature to check.
    Kind(Kind),
    /// A kind whose files begin with a signature, and what to call the
    /// kind when they do not.
    Signed(Kind, &'static [u8], &'static str),
    /// `.db`: anyone's. DuckDB's and SQLite's are known by their bytes
    /// before the name is read, an empty one becomes DuckDB's, and anything
    /// else in one is refused.
    Either,
    Unknown,
}

fn by_name(ext: &str) -> Named {
    match ext {
        "duckdb" | "ddb" => Named::Database,
        "csv" | "tsv" | "txt" => Named::Kind(Kind::Data(Reader::Csv)),
        "json" | "ndjson" | "jsonl" => Named::Kind(Kind::Data(Reader::Json)),
        "geojson" => Named::Kind(Kind::Data(Reader::Spatial)),
        "csv.gz" => Named::Signed(Kind::Data(Reader::Csv), GZIP, "gzip"),
        "parquet" => Named::Signed(Kind::Data(Reader::Parquet), PARQUET, "Parquet"),
        "xlsx" => Named::Signed(Kind::Data(Reader::Xlsx), ZIP, "an Excel workbook"),
        "shp" => Named::Signed(Kind::Data(Reader::Spatial), SHAPEFILE, "a shapefile"),
        "gpkg" => Named::Signed(Kind::Data(Reader::Spatial), SQLITE, "a GeoPackage"),
        "sqlite" | "sqlite3" => Named::Signed(Kind::Sqlite, SQLITE, "SQLite"),
        "db" => Named::Either,
        _ => Named::Unknown,
    }
}

/// The bytes looked at, for a refusal: as text when they read as text.
fn begins(head: &[u8]) -> String {
    if head.is_empty() {
        return "it is empty".to_string();
    }
    let printable = head.iter().all(|&b| b == b'\t' || b == b'\n' || b == b'\r' || (0x20..0x7f).contains(&b));
    match printable {
        true => format!("it begins {:?}", String::from_utf8_lossy(head)),
        false => {
            let hex: Vec<String> = head.iter().map(|b| format!("{b:02x}")).collect();
            format!("it begins {}", hex.join(" "))
        }
    }
}

/// The last component of a path, on either separator.
fn file_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed)
}

/// The extension, lowercased, with `.csv.gz` kept whole. Lowercased in
/// ASCII alone: every extension matched is ASCII, and the stem is cut from
/// the name by the extension's length in bytes, which full Unicode
/// lowercasing changes (`K`, the Kelvin sign, is `k`; `İ` is two
/// characters).
fn extension(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".csv.gz") && lower.len() > ".csv.gz".len() {
        return "csv.gz".to_string();
    }
    match lower.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_string(),
        _ => String::new(),
    }
}

impl Reader {
    /// The extension the caller loads before the view is made, and before
    /// it is read in a server that has not loaded it: `spatial`, which the
    /// engine does not load on demand. The others load themselves.
    pub fn extension(self) -> Option<&'static str> {
        match self {
            Reader::Spatial => Some("spatial"),
            _ => None,
        }
    }

    /// What a view of this reader shows, for people: "a view over" this.
    pub fn describes(self) -> &'static str {
        match self {
            Reader::Csv | Reader::Parquet | Reader::Json | Reader::Spatial => "",
            Reader::ParquetFolder => "the Parquet files in ",
            Reader::Xlsx => "the first sheet of ",
        }
    }

    /// The DuckDB function that reads it.
    pub fn function(self) -> &'static str {
        match self {
            Reader::Csv => "read_csv",
            Reader::Parquet | Reader::ParquetFolder => "read_parquet",
            Reader::Json => "read_json",
            Reader::Xlsx => "read_xlsx",
            Reader::Spatial => "ST_Read",
        }
    }

    /// The function's first argument for `path`, as a string literal: the
    /// path as a pattern that matches only itself, and for a folder every
    /// `.parquet` file at any depth beneath it. GDAL opens the path as
    /// given, so `ST_Read`'s is the path itself.
    pub fn argument(self, path: &str) -> String {
        match self {
            Reader::Spatial => quote_literal(path),
            Reader::ParquetFolder => {
                let dir = glob_escape(path.trim_end_matches(['/', '\\']));
                quote_literal(&format!("{dir}/**/*.parquet"))
            }
            _ => quote_literal(&glob_escape(path)),
        }
    }

    /// The function call that reads `path`, as the view's SQL holds it.
    pub fn source(self, path: &str) -> String {
        match self {
            Reader::ParquetFolder => format!("read_parquet({}, hive_partitioning = true)", self.argument(path)),
            _ => format!("{}({})", self.function(), self.argument(path)),
        }
    }
}

/// `CREATE VIEW` for a data file: `name` over `path`, which is absolute,
/// since the server that runs this has a working directory of its own.
pub fn view_sql(name: &str, reader: Reader, path: &str) -> String {
    format!("CREATE VIEW {} AS SELECT * FROM {}", quote_ident(name), reader.source(path))
}

/// How long a statement waits on a SQLite file another program has locked,
/// in milliseconds, before it fails with `database is locked`. The engine
/// reads every attached database's catalog for any catalog read, and waits
/// five seconds by default, so one locked file would hold every catalog read
/// of the server that long. A second rides over a writer's commit.
pub const SQLITE_BUSY_MS: u32 = 1000;

/// `ATTACH` for a SQLite file, read-write, as the catalog `name`, waiting
/// [`SQLITE_BUSY_MS`] on a lock.
pub fn attach_sql(name: &str, path: &str) -> String {
    format!(
        "ATTACH {} AS {} (TYPE sqlite, BUSY_TIMEOUT {SQLITE_BUSY_MS})",
        quote_literal(path),
        quote_ident(name)
    )
}

/// The schema of the workspace's record of what is attached in it
/// ([`Record`]). No name [`view_name`] makes can be it, since that trims a
/// leading `_`, so a file's view or catalog never takes it.
pub const RECORD_SCHEMA: &str = "_harbor";

/// The record's table, in [`RECORD_SCHEMA`].
pub const RECORD_TABLE: &str = "attached";

/// The statements that make the record where it is not, in order.
pub const RECORD_DDL: [&str; 2] = [
    "CREATE SCHEMA IF NOT EXISTS _harbor",
    "CREATE TABLE IF NOT EXISTS _harbor.attached \
     (name VARCHAR PRIMARY KEY, path VARCHAR NOT NULL, type VARCHAR NOT NULL, read_only BOOLEAN NOT NULL)",
];

/// A database file attached in the workspace, as the workspace records it
/// so that every server that opens the workspace attaches it again: an
/// `ATTACH` lasts only as long as the server that ran it.
///
/// Whoever attaches a file writes its row ([`Record::insert_sql`]), and the
/// server rewrites the record from what is attached when it stops, so a
/// plain `ATTACH` or `DETACH` from any client is recorded too. A row whose
/// file is gone when the server starts stays, and is reported, until the
/// file is back or the row is deleted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Record {
    /// The catalog's name.
    pub name: String,
    /// The file, absolute.
    pub path: String,
    /// The engine's name for its storage: `sqlite` or `duckdb`.
    #[serde(rename = "type")]
    pub storage: String,
    pub read_only: bool,
}

impl Record {
    /// Whether a database the engine lists is one the workspace records: a
    /// SQLite or DuckDB file. An in-memory database has nothing to attach
    /// again, and another engine's path can be a connection string, which
    /// may hold a password.
    pub fn kept(storage: &str, path: &str) -> bool {
        matches!(storage, "sqlite" | "duckdb") && !path.is_empty() && path != ":memory:"
    }

    /// `ATTACH` for this file, as it was attached, and a SQLite file
    /// waiting [`SQLITE_BUSY_MS`] on a lock.
    pub fn attach_sql(&self) -> String {
        let read_only = if self.read_only { ", READ_ONLY" } else { "" };
        let busy = match self.storage.as_str() {
            "sqlite" => format!(", BUSY_TIMEOUT {SQLITE_BUSY_MS}"),
            _ => String::new(),
        };
        format!(
            "ATTACH {} AS {} (TYPE {}{read_only}{busy})",
            quote_literal(&self.path),
            quote_ident(&self.name),
            quote_ident(&self.storage)
        )
    }

    /// The record's row for this file, written over any row of its name.
    pub fn insert_sql(&self) -> String {
        format!(
            "INSERT OR REPLACE INTO {RECORD_SCHEMA}.{RECORD_TABLE} (name, path, type, read_only) VALUES ({}, {}, {}, {})",
            quote_literal(&self.name),
            quote_literal(&self.path),
            quote_literal(&self.storage),
            self.read_only
        )
    }

    /// The record's row for the catalog `name` deleted, so no server that
    /// opens the workspace attaches its file again.
    pub fn delete_sql(name: &str) -> String {
        format!("DELETE FROM {RECORD_SCHEMA}.{RECORD_TABLE} WHERE name = {}", quote_literal(name))
    }

    /// The names the record holds for files other than `path`. A catalog
    /// for `path` takes none of them, or it would write over the row of a
    /// file its server could not attach.
    pub fn names_sql(path: &str) -> String {
        format!("SELECT name FROM {RECORD_SCHEMA}.{RECORD_TABLE} WHERE path <> {}", quote_literal(path))
    }
}

/// The extensions a statement calls into that the engine does not load on
/// demand: `spatial`, for any `ST_` function. Read from the code only, so a
/// path or a name that spells `st_read(` does not count. The engine loads
/// `excel`, `json`, `parquet` and `sqlite` when a statement needs them.
pub fn extensions(sql: &str) -> Vec<&'static str> {
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80;
    let mut out = Vec::new();
    for span in crate::scan::scan(sql).iter().filter(|s| s.kind == crate::scan::Kind::Code) {
        let code = &sql.as_bytes()[span.start..span.end];
        let mut i = 0;
        while i < code.len() {
            if !word(code[i]) {
                i += 1;
                continue;
            }
            let start = i;
            while i < code.len() && word(code[i]) {
                i += 1;
            }
            let called = code[i..].iter().find(|c| !c.is_ascii_whitespace()) == Some(&b'(');
            let name = &code[start..i];
            if called && name.len() > 3 && name[..3].eq_ignore_ascii_case(b"st_") && !out.contains(&"spatial") {
                out.push("spatial");
            }
        }
    }
    out
}

/// Whether a view's SQL, as the engine renders it in `duckdb_views()`,
/// reads `path` with `reader`: the view a second opening of the same file
/// finds and keeps rather than making another. The engine keeps a string
/// constant as it was written, so the path's literal is found verbatim; the
/// function name before it is matched in any case.
pub fn reads(view_sql: &str, reader: Reader, path: &str) -> bool {
    let argument = reader.argument(path);
    let call = format!("{}(", reader.function()).to_lowercase();
    view_sql.match_indices(&argument).any(|(at, _)| view_sql[..at].to_lowercase().ends_with(&call))
}

/// The name a file's view or catalog takes: the file's name without its
/// extension, every run of anything but letters, digits and `_` made one
/// `_`, trimmed of `_`, and led by `_` when it would begin with a digit.
/// `data` when nothing is left.
pub fn view_name(path: &str) -> String {
    let name = file_name(path);
    let ext = extension(name);
    let stem = match ext.is_empty() {
        true => name,
        false => &name[..name.len() - ext.len() - 1],
    };
    let mut out = String::new();
    for c in stem.chars() {
        match c.is_alphanumeric() || c == '_' {
            true => out.push(c),
            false if !out.ends_with('_') => out.push('_'),
            false => {}
        }
    }
    let out = out.trim_matches('_');
    match out.chars().next() {
        None => "data".to_string(),
        Some(c) if c.is_ascii_digit() => format!("_{out}"),
        Some(_) => out.to_string(),
    }
}

/// `base`, or the first of `base_2`, `base_3`, … that `taken` refuses. A
/// name is taken when a relation or catalog already has it or when it is a
/// reserved word, in any case, since DuckDB's names are case-insensitive;
/// `taken` is asked as the caller's names are spelled.
pub fn free_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_string();
    }
    (2..).map(|n| format!("{base}_{n}")).find(|name| !taken(name)).unwrap()
}

/// A DuckDB identifier, double-quoted with embedded quotes doubled.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A DuckDB string literal, single-quoted with embedded quotes doubled.
pub fn quote_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// A path as a glob pattern that matches only itself: `*`, `?` and `[` each
/// bracketed, the one escape DuckDB's glob takes (a backslash is not one).
pub fn glob_escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            '*' | '?' | '[' => {
                out.push('[');
                out.push(c);
                out.push(']');
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUCK: &[u8] = b"\x8f\x02\xa1\x00\x00\x00\x00\x00DUCK\x40\x00\x00\x00";

    fn file(path: &str, head: &[u8]) -> Result<Kind, String> {
        classify(path, Entry::File(head))
    }

    #[test]
    fn signatures_decide_under_any_name() {
        assert_eq!(file("x.csv", DUCK), Ok(Kind::Duckdb));
        assert_eq!(file("x", DUCK), Ok(Kind::Duckdb));
        assert_eq!(file("app.db", SQLITE), Ok(Kind::Sqlite));
        assert_eq!(file("app", SQLITE), Ok(Kind::Sqlite));
        assert_eq!(file("app.sqlite3", SQLITE), Ok(Kind::Sqlite));
        assert_eq!(file("x.bin", b"PAR1\x15\x00"), Ok(Kind::Data(Reader::Parquet)));
        // A GeoPackage is SQLite inside and a layer to read.
        assert_eq!(file("roads.gpkg", SQLITE), Ok(Kind::Data(Reader::Spatial)));
    }

    #[test]
    fn names_decide_where_there_is_no_signature() {
        for (name, reader) in [
            ("sales.csv", Reader::Csv),
            ("SALES.TSV", Reader::Csv),
            ("log.txt", Reader::Csv),
            ("events.json", Reader::Json),
            ("events.ndjson", Reader::Json),
            ("events.jsonl", Reader::Json),
            ("parks.geojson", Reader::Spatial),
        ] {
            assert_eq!(file(name, b"id,name\n1,a\n"), Ok(Kind::Data(reader)), "{name}");
        }
        assert_eq!(file("a.csv.gz", b"\x1f\x8b\x08\x00"), Ok(Kind::Data(Reader::Csv)));
        assert_eq!(file("book.xlsx", b"PK\x03\x04\x14\x00"), Ok(Kind::Data(Reader::Xlsx)));
        assert_eq!(file("pts.shp", b"\x00\x00\x27\x0a\x00\x00"), Ok(Kind::Data(Reader::Spatial)));
        assert_eq!(classify("data/", Entry::Dir), Ok(Kind::Data(Reader::ParquetFolder)));
        assert_eq!(file("x.duckdb", b"garbage"), Ok(Kind::Duckdb));
        assert_eq!(file("x.ddb", b""), Ok(Kind::Duckdb));
    }

    #[test]
    fn a_name_the_bytes_contradict_is_refused_with_what_was_seen() {
        let e = file("x.parquet", b"id,name\n").unwrap_err();
        assert!(e.contains("named .parquet") && e.contains("\"id,name\\n\"") && e.contains("not Parquet"), "{e}");
        let e = file("app.sqlite", b"\x00\x01\xfe").unwrap_err();
        assert!(e.contains("00 01 fe") && e.contains("not SQLite"), "{e}");
        assert!(file("x.xlsx", b"<html>").is_err());
        assert!(file("x.csv.gz", b"id,name").is_err());
        assert!(file("x.shp", b"abcd").is_err());
        assert!(file("x.gpkg", b"abcd").is_err());
        let e = file("x.db", b"hello").unwrap_err();
        assert!(e.contains("not a DuckDB or SQLite database"), "{e}");
        assert_eq!(file("x.db", b""), Ok(Kind::Duckdb));
    }

    #[test]
    fn what_cannot_be_told_is_refused() {
        let e = file("notes.bin", b"\x7fELF\x02\x01").unwrap_err();
        assert!(e.contains("ends .bin") && e.contains("7f 45 4c 46"), "{e}");
        let e = file("README", b"hello").unwrap_err();
        assert!(e.contains("no extension") && e.contains("\"hello\""), "{e}");
    }

    #[test]
    fn a_missing_path_becomes_a_database_only_when_named_like_one() {
        assert_eq!(classify("new.duckdb", Entry::Missing), Ok(Kind::Duckdb));
        assert_eq!(classify("new", Entry::Missing), Ok(Kind::Duckdb));
        assert_eq!(classify("new.db", Entry::Missing), Ok(Kind::Duckdb));
        assert_eq!(classify("sales.csv", Entry::Missing), Err("sales.csv: no such file".into()));
        assert!(classify("app.sqlite", Entry::Missing).is_err());
        // An empty file is a database DuckDB has yet to write.
        assert_eq!(file("empty", b""), Ok(Kind::Duckdb));
    }

    #[test]
    fn views_and_attachments_quote_everything() {
        assert_eq!(
            view_sql("sales", Reader::Csv, "/data/it's é [2024]*?.csv"),
            "CREATE VIEW \"sales\" AS SELECT * FROM read_csv('/data/it''s é [[]2024][*][?].csv')"
        );
        assert_eq!(
            view_sql("say \"hi\"", Reader::Spatial, "/m y/it's [x].shp"),
            "CREATE VIEW \"say \"\"hi\"\"\" AS SELECT * FROM ST_Read('/m y/it''s [x].shp')"
        );
        assert_eq!(
            view_sql("p", Reader::ParquetFolder, "/lake/it's/"),
            "CREATE VIEW \"p\" AS SELECT * FROM read_parquet('/lake/it''s/**/*.parquet', hive_partitioning = true)"
        );
        assert_eq!(
            attach_sql("app", "/x/o'brien db.sqlite"),
            "ATTACH '/x/o''brien db.sqlite' AS \"app\" (TYPE sqlite, BUSY_TIMEOUT 1000)"
        );
        assert_eq!(Reader::Xlsx.source("/b[1].xlsx"), "read_xlsx('/b[[]1].xlsx')");
        assert_eq!(Reader::Json.source("/j.json"), "read_json('/j.json')");
        assert_eq!(Reader::Parquet.source("/p.parquet"), "read_parquet('/p.parquet')");
    }

    #[test]
    fn a_record_attaches_its_file_again_as_it_was_attached() {
        let app = Record { name: "app".into(), path: "/x/o'brien.sqlite".into(), storage: "sqlite".into(), read_only: false };
        assert_eq!(app.attach_sql(), "ATTACH '/x/o''brien.sqlite' AS \"app\" (TYPE \"sqlite\", BUSY_TIMEOUT 1000)");
        assert_eq!(
            app.insert_sql(),
            "INSERT OR REPLACE INTO _harbor.attached (name, path, type, read_only) VALUES ('app', '/x/o''brien.sqlite', 'sqlite', false)"
        );
        let other = Record { name: "o\"x".into(), path: "/o.duckdb".into(), storage: "duckdb".into(), read_only: true };
        assert_eq!(other.attach_sql(), "ATTACH '/o.duckdb' AS \"o\"\"x\" (TYPE \"duckdb\", READ_ONLY)");
        assert!(Record::kept("sqlite", "/a.sqlite") && Record::kept("duckdb", "/o.duckdb"));
        assert!(!Record::kept("duckdb", "") && !Record::kept("duckdb", ":memory:"));
        assert!(!Record::kept("postgres", "dbname=x password=y"));
        assert_eq!(Record::delete_sql("o'x"), "DELETE FROM _harbor.attached WHERE name = 'o''x'");
        assert_eq!(Record::names_sql("/x/o'brien.sqlite"), "SELECT name FROM _harbor.attached WHERE path <> '/x/o''brien.sqlite'");
        // The record's schema is no name a file's view or catalog takes.
        assert_eq!(view_name("/x/_harbor.sqlite"), "harbor");
    }

    #[test]
    fn a_spatial_function_in_the_code_needs_spatial() {
        assert_eq!(extensions("CREATE VIEW p AS SELECT * FROM st_read('/p.shp');"), ["spatial"]);
        assert_eq!(extensions("SELECT ST_Area (geom), ST_Centroid(geom) FROM t"), ["spatial"]);
        assert!(extensions("CREATE VIEW s AS SELECT * FROM read_csv('/x/st_read(.csv');").is_empty());
        assert!(extensions("SELECT * FROM read_xlsx('/b.xlsx') -- st_read(").is_empty());
        assert!(extensions("SELECT st_x, first_st_y(1), \"st_read\"(1) FROM t").is_empty());
    }

    #[test]
    fn a_view_is_found_by_the_file_it_reads() {
        // As the engine renders them in duckdb_views().
        let csv = "CREATE VIEW sales AS SELECT * FROM read_csv('/x/it''s.csv');";
        assert!(reads(csv, Reader::Csv, "/x/it's.csv"));
        assert!(!reads(csv, Reader::Csv, "/x/it.csv"));
        assert!(!reads(csv, Reader::Json, "/x/it's.csv"));
        let lake = "CREATE VIEW lake AS SELECT * FROM read_parquet('/l/**/*.parquet', (hive_partitioning = true));";
        assert!(reads(lake, Reader::ParquetFolder, "/l"));
        assert!(!reads(lake, Reader::Parquet, "/l"));
        let shp = "CREATE VIEW pts AS SELECT * FROM st_read('/p.shp');";
        assert!(reads(shp, Reader::Spatial, "/p.shp"));
    }

    #[test]
    fn names_are_identifiers_people_can_type() {
        assert_eq!(view_name("/x/sales.csv"), "sales");
        assert_eq!(view_name("/x/Sales Q1 (final).CSV"), "Sales_Q1_final");
        assert_eq!(view_name("/x/events.csv.gz"), "events");
        assert_eq!(view_name("/x/2024-sales.parquet"), "_2024_sales");
        assert_eq!(view_name("/x/données.json"), "données");
        assert_eq!(view_name("/x/lake/"), "lake");
        assert_eq!(view_name("C:\\x\\it's.xlsx"), "it_s");
        assert_eq!(view_name("/x/---.csv"), "data");
        assert_eq!(view_name("/x/.hidden"), "hidden");
        assert_eq!(view_name("/x/noext"), "noext");
        // Names whose lowercase is longer or shorter in bytes.
        assert_eq!(view_name("/x/lake.\u{212a}"), "lake");
        assert_eq!(view_name("/x/\u{e9}.\u{130}"), "\u{e9}");
        assert_eq!(view_name("/x/\u{130}stanbul.CSV"), "\u{130}stanbul");
        assert_eq!(classify("/x/lake.\u{212a}", Entry::Dir), Ok(Kind::Data(Reader::ParquetFolder)));
        assert_eq!(file("/x/\u{e9}.\u{130}", SQLITE), Ok(Kind::Sqlite));
    }

    #[test]
    fn a_taken_name_gets_the_next_number() {
        let taken = ["sales", "sales_2", "order"];
        let is_taken = |n: &str| taken.iter().any(|t| t.eq_ignore_ascii_case(n));
        assert_eq!(free_name("sales", is_taken), "sales_3");
        assert_eq!(free_name("SALES", is_taken), "SALES_3");
        assert_eq!(free_name("order", is_taken), "order_2");
        assert_eq!(free_name("fresh", is_taken), "fresh");
    }
}
