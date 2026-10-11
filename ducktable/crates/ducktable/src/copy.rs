//! Copying a grid's selection as text: what is copied (`span`) and how it
//! is written (`render`). Pure: the grid hands over the cells it shows, and
//! gets back the clipboard's text. The values are the text Harbor sent,
//! whole, never the cell's ellipsized drawing; a staged cell is copied as
//! it is staged, since the copy is of what the grid shows.

use crate::util::{qident, qlit};
use serde_json::Value;
use std::collections::BTreeSet;

/// The four ways a selection is written. ⌘C is TSV, what spreadsheets
/// paste; the Edit menu has the other three.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Format {
    #[default]
    Tsv,
    Csv,
    Markdown,
    SqlInsert,
}

impl Format {
    /// The format as the status line names it.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Format::Tsv => "TSV",
            Format::Csv => "CSV",
            Format::Markdown => "Markdown",
            Format::SqlInsert => "SQL",
        }
    }
}

/// One copied cell, as the grid shows it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Cell<'a> {
    Text(&'a str),
    Null,
    /// A new row's untouched cell, which the INSERT that commits it omits
    /// so the engine supplies its default.
    Default,
}

/// What a copy takes from the grid's selection, in display positions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Span {
    Nothing,
    /// One cell: the ring's, as (row, schema column).
    Cell(usize, usize),
    /// Whole rows, every visible column, in display order.
    Rows(Vec<usize>),
}

/// The one rule for what ⌘C copies (docs/DESIGN.md, "Copy and export"):
///
/// - `#` (the whole page selected) copies every row on the page.
/// - Two or more selected rows copy those rows.
/// - One row selected from its number in the gutter copies that row.
/// - Otherwise the ring's cell is copied alone, and with no ring, the one
///   selected row.
///
/// Rows are copied under a header line of column names; a single cell is
/// its value alone.
pub(crate) fn span(
    selected: &BTreeSet<usize>,
    all: bool,
    whole_rows: bool,
    ring: Option<(usize, usize)>,
    page_rows: usize,
) -> Span {
    let on_page = |rows: Vec<usize>| {
        let rows: Vec<usize> = rows.into_iter().filter(|&r| r < page_rows).collect();
        if rows.is_empty() { Span::Nothing } else { Span::Rows(rows) }
    };
    if all {
        return on_page((0..page_rows).collect());
    }
    if selected.len() > 1 || (selected.len() == 1 && whole_rows) {
        return on_page(selected.iter().copied().collect());
    }
    match ring {
        Some((row, col)) if row < page_rows && (selected.is_empty() || selected.contains(&row)) => {
            Span::Cell(row, col)
        }
        _ => on_page(selected.iter().copied().collect()),
    }
}

/// The copied cells: one column per visible column copied, each with its
/// name and its type as the wire describes it.
pub(crate) struct Sheet<'a> {
    pub(crate) columns: Vec<(&'a str, &'a wire::Column)>,
    pub(crate) rows: Vec<Vec<Cell<'a>>>,
    /// A header line of column names: rows are copied under one, a single
    /// cell alone. Markdown always has one, since a table needs it, and an
    /// INSERT always names its columns.
    pub(crate) header: bool,
}

/// The selection written as `format`. `table` is the INSERT's target, a
/// quoted name.
pub(crate) fn render(format: Format, sheet: &Sheet<'_>, table: &str) -> String {
    match format {
        Format::Tsv => delimited(sheet, "\t", tsv_field),
        Format::Csv => delimited(sheet, ",", csv_field),
        Format::Markdown => markdown(sheet),
        Format::SqlInsert => insert(sheet, table),
    }
}

fn delimited(sheet: &Sheet<'_>, sep: &str, field: fn(Cell<'_>) -> String) -> String {
    let mut lines = Vec::with_capacity(sheet.rows.len() + 1);
    if sheet.header {
        lines.push(sheet.columns.iter().map(|(name, _)| field(Cell::Text(name))).collect::<Vec<_>>().join(sep));
    }
    for row in &sheet.rows {
        lines.push(row.iter().map(|&cell| field(cell)).collect::<Vec<_>>().join(sep));
    }
    lines.join("\n")
}

/// A TSV field, as spreadsheets read one from the clipboard: NULL, the
/// default and the empty string are all an empty field, and a value with a
/// tab, a line break or a quote is quoted, its quotes doubled.
fn tsv_field(cell: Cell<'_>) -> String {
    match cell {
        Cell::Text(text) if text.contains(['\t', '\n', '\r', '"']) => format!("\"{}\"", text.replace('"', "\"\"")),
        Cell::Text(text) => text.to_string(),
        Cell::Null | Cell::Default => String::new(),
    }
}

/// A CSV field (RFC 4180), as DuckDB writes one: NULL is an empty field and
/// the empty string `""`, so the two stay apart.
fn csv_field(cell: Cell<'_>) -> String {
    match cell {
        Cell::Text(text) if text.is_empty() || text.contains([',', '\n', '\r', '"']) => {
            format!("\"{}\"", text.replace('"', "\"\""))
        }
        Cell::Text(text) => text.to_string(),
        Cell::Null | Cell::Default => String::new(),
    }
}

/// A Markdown table. A pipe is escaped, a line break becomes `<br>`, and
/// NULL and a new row's default are spelled out, since an empty cell is
/// the empty string.
fn markdown(sheet: &Sheet<'_>) -> String {
    let field = |cell: Cell<'_>| match cell {
        Cell::Text(text) => text.replace('|', "\\|").replace("\r\n", "<br>").replace(['\n', '\r'], "<br>"),
        Cell::Null => "NULL".to_string(),
        Cell::Default => "DEFAULT".to_string(),
    };
    let line = |cells: Vec<String>| format!("| {} |", cells.join(" | "));
    let mut lines = vec![
        line(sheet.columns.iter().map(|(name, _)| field(Cell::Text(name))).collect()),
        line(sheet.columns.iter().map(|_| "---".to_string()).collect()),
    ];
    for row in &sheet.rows {
        lines.push(line(row.iter().map(|&cell| field(cell)).collect()));
    }
    lines.join("\n")
}

/// One INSERT for the whole selection, a row of VALUES per row.
fn insert(sheet: &Sheet<'_>, table: &str) -> String {
    let names: Vec<String> = sheet.columns.iter().map(|(name, _)| qident(name)).collect();
    let rows: Vec<String> = sheet
        .rows
        .iter()
        .map(|row| {
            let values: Vec<String> =
                row.iter().zip(&sheet.columns).map(|(&cell, (_, col))| literal(cell, col)).collect();
            format!("  ({})", values.join(", "))
        })
        .collect();
    format!("INSERT INTO {table} ({}) VALUES\n{};", names.join(", "), rows.join(",\n"))
}

/// What a column holds, as far as writing a literal goes.
#[derive(Debug, PartialEq)]
enum Kind {
    List,
    Struct,
    Map,
    Union,
    /// A `VARIANT` or a `JSON`: JSON text, read back through `::JSON`.
    Json,
    /// Base64 on the wire, bytes in the engine.
    Blob,
    /// `{"months", "days", "micros"}` on the wire.
    Interval,
    Boolean,
    Number,
    /// Everything the engine reads from its own text: VARCHAR, the dates
    /// and times, UUID, BIT, ENUM, and the types Harbor sends as a cast.
    Text,
}

fn kind(col: &wire::Column) -> Kind {
    let ty = col.duckdb_type.as_str();
    if col.child.is_some() {
        Kind::List
    } else if col.fields.is_some() {
        Kind::Struct
    } else if col.key_type.is_some() {
        Kind::Map
    } else if col.members.is_some() {
        Kind::Union
    } else if col.encoding.as_deref() == Some("json") || ty.eq_ignore_ascii_case("JSON") {
        Kind::Json
    } else if ty.eq_ignore_ascii_case("BLOB") {
        Kind::Blob
    } else if ty.eq_ignore_ascii_case("INTERVAL") {
        Kind::Interval
    } else if ty.eq_ignore_ascii_case("BOOLEAN") {
        Kind::Boolean
    } else if crate::edits::is_numeric_type(ty) {
        Kind::Number
    } else {
        Kind::Text
    }
}

/// The SQL literal for one cell, written so that an INSERT into a column of
/// `col`'s type stores the value the cell shows: measured to round-trip
/// every type DuckTable renders, nested ones included. A cell's text that
/// is not the wire's spelling (a staged nested value typed in DuckDB's own
/// form, `{a=1}`) is written as a string for the engine to cast, as the
/// editor binds it.
pub(crate) fn literal(cell: Cell<'_>, col: &wire::Column) -> String {
    let text = match cell {
        Cell::Text(text) => text,
        Cell::Null => return "NULL".to_string(),
        Cell::Default => return "DEFAULT".to_string(),
    };
    match kind(col) {
        Kind::Json => format!("{}::JSON", qlit(text)),
        Kind::Blob => format!("from_base64({})", qlit(text)),
        Kind::Boolean => match text {
            "true" => "TRUE".to_string(),
            "false" => "FALSE".to_string(),
            _ => qlit(text),
        },
        Kind::Number => number(text, col),
        Kind::Text => qlit(text),
        Kind::List | Kind::Struct | Kind::Map | Kind::Union | Kind::Interval => {
            match serde_json::from_str::<Value>(text) {
                Ok(value) => nested(&value, col).unwrap_or_else(|| qlit(text)),
                Err(_) => qlit(text),
            }
        }
    }
}

/// A number bare when its text is a numeric literal the engine reads
/// exactly: an integer up to 18 digits (wider ones the parser makes a
/// DOUBLE), a decimal, or one with an exponent. Anything else, NaN and
/// Infinity among them, is a string cast to the column's type, which keeps
/// a list that mixes the two of one type.
fn number(text: &str, col: &wire::Column) -> String {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let (mantissa, exponent) = match digits.find(['e', 'E']) {
        Some(at) => (&digits[..at], Some(&digits[at + 1..])),
        None => (digits, None),
    };
    let (whole, fraction) = match mantissa.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (mantissa, None),
    };
    let all_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let exponent_ok = exponent.is_none_or(|e| all_digits(e.strip_prefix(['+', '-']).unwrap_or(e)));
    let plain = all_digits(whole)
        && fraction.is_none_or(all_digits)
        && exponent_ok
        && (fraction.is_some() || exponent.is_some() || whole.len() <= 18);
    if plain { text.to_string() } else { format!("{}::{}", qlit(text), col.duckdb_type) }
}

/// The literal for a value inside a nested cell, walked with its type. None
/// when the value is not the shape its type has on the wire.
fn nested(value: &Value, col: &wire::Column) -> Option<String> {
    if value.is_null() {
        return Some("NULL".to_string());
    }
    let scalar = |value: &Value| match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    Some(match kind(col) {
        Kind::List => {
            let child = col.child.as_deref()?;
            let items: Option<Vec<String>> = value.as_array()?.iter().map(|v| nested(v, child)).collect();
            format!("[{}]", items?.join(", "))
        }
        Kind::Struct => {
            let object = value.as_object()?;
            let fields: Option<Vec<String>> = col
                .fields
                .as_deref()?
                .iter()
                .map(|field| {
                    let name = field.name.as_deref()?;
                    Some(format!("{}: {}", qlit(name), nested(object.get(name).unwrap_or(&Value::Null), field)?))
                })
                .collect();
            format!("{{{}}}", fields?.join(", "))
        }
        Kind::Map => {
            let (key, val) = (col.key_type.as_deref()?, col.value_type.as_deref()?);
            let pairs: Option<Vec<String>> = value
                .as_array()?
                .iter()
                .map(|pair| match pair.as_array()?.as_slice() {
                    [k, v] => Some(format!("{}: {}", nested(k, key)?, nested(v, val)?)),
                    _ => None,
                })
                .collect();
            format!("MAP {{{}}}", pairs?.join(", "))
        }
        Kind::Union => {
            // Each member cast to the whole union, so a list that holds two
            // members has one element type.
            let tag = value.get("tag")?.as_str()?;
            let member = col.members.as_deref()?.iter().find(|m| m.name.as_deref() == Some(tag))?;
            let inner = nested(value.get("value").unwrap_or(&Value::Null), member)?;
            format!("union_value({} := {inner})::{}", qident(tag), col.duckdb_type)
        }
        Kind::Interval => {
            let part = |name: &str| -> Option<String> {
                let text = scalar(value.get(name)?);
                let digits = text.strip_prefix('-').unwrap_or(&text);
                (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then_some(text)
            };
            format!(
                "INTERVAL '{} months {} days {} microseconds'",
                part("months")?,
                part("days")?,
                part("micros")?
            )
        }
        Kind::Json => format!("{}::JSON", qlit(&scalar(value))),
        Kind::Blob => format!("from_base64({})", qlit(value.as_str()?)),
        Kind::Boolean => match value {
            Value::Bool(true) => "TRUE".to_string(),
            Value::Bool(false) => "FALSE".to_string(),
            other => qlit(&scalar(other)),
        },
        Kind::Number => number(&scalar(value), col),
        Kind::Text => qlit(&scalar(value)),
    })
}

#[cfg(test)]
mod tests {
    use super::{literal, render, span, Cell, Format, Sheet, Span};
    use std::collections::BTreeSet;

    fn col(json: &str) -> wire::Column {
        serde_json::from_str(json).unwrap()
    }

    fn ty(duckdb_type: &str) -> wire::Column {
        wire::Column { duckdb_type: duckdb_type.into(), lossless: true, ..Default::default() }
    }

    #[test]
    fn the_selection_decides_between_a_cell_and_whole_rows() {
        let set = |rows: &[usize]| rows.iter().copied().collect::<BTreeSet<_>>();
        // A clicked cell: its row selected and the ring on it.
        assert_eq!(span(&set(&[3]), false, false, Some((3, 2)), 10), Span::Cell(3, 2));
        // The same row chosen from its number is the whole row.
        assert_eq!(span(&set(&[3]), false, true, Some((3, 2)), 10), Span::Rows(vec![3]));
        // Two rows, by ⌘- or ⇧-click, are rows wherever the ring sits.
        assert_eq!(span(&set(&[5, 2]), false, false, Some((5, 0)), 10), Span::Rows(vec![2, 5]));
        // `#` is the page, whatever else is selected.
        assert_eq!(span(&set(&[1]), true, false, Some((1, 0)), 3), Span::Rows(vec![0, 1, 2]));
        // A row with no ring is the row; nothing selected is nothing.
        assert_eq!(span(&set(&[4]), false, false, None, 10), Span::Rows(vec![4]));
        assert_eq!(span(&set(&[]), false, false, None, 10), Span::Nothing);
        assert_eq!(span(&set(&[]), true, false, None, 0), Span::Nothing);
        // A ring with nothing selected is still its cell; one on a row the
        // page no longer has is nothing.
        assert_eq!(span(&set(&[]), false, false, Some((0, 1)), 10), Span::Cell(0, 1));
        assert_eq!(span(&set(&[]), false, false, Some((12, 1)), 10), Span::Nothing);
    }

    fn sheet<'a>(columns: &'a [(&'a str, wire::Column)], rows: Vec<Vec<Cell<'a>>>, header: bool) -> Sheet<'a> {
        Sheet { columns: columns.iter().map(|(n, c)| (*n, c)).collect(), rows, header }
    }

    #[test]
    fn tsv_is_what_a_spreadsheet_pastes() {
        let columns = [("id", ty("INTEGER")), ("note", ty("VARCHAR"))];
        let rows = vec![
            vec![Cell::Text("1"), Cell::Text("plain")],
            vec![Cell::Text("2"), Cell::Text("tab\there, \"quoted\"\nnext line")],
            vec![Cell::Null, Cell::Text("")],
        ];
        assert_eq!(
            render(Format::Tsv, &sheet(&columns, rows, true), "t"),
            "id\tnote\n1\tplain\n2\t\"tab\there, \"\"quoted\"\"\nnext line\"\n\t"
        );
        // A single cell is its value alone: no header, no line break.
        let one = sheet(&columns[1..], vec![vec![Cell::Text("x,y")]], false);
        assert_eq!(render(Format::Tsv, &one, "t"), "x,y");
    }

    #[test]
    fn csv_keeps_null_and_the_empty_string_apart() {
        let columns = [("a", ty("VARCHAR")), ("b, c", ty("VARCHAR"))];
        let rows = vec![vec![Cell::Null, Cell::Text("")], vec![Cell::Text("say \"hi\""), Cell::Default]];
        assert_eq!(
            render(Format::Csv, &sheet(&columns, rows, true), "t"),
            "a,\"b, c\"\n,\"\"\n\"say \"\"hi\"\"\","
        );
    }

    #[test]
    fn markdown_escapes_pipes_and_line_breaks_and_names_null() {
        let columns = [("a|b", ty("VARCHAR")), ("n", ty("INTEGER"))];
        let rows = vec![vec![Cell::Text("x|y\r\nz"), Cell::Null], vec![Cell::Text(""), Cell::Default]];
        assert_eq!(
            render(Format::Markdown, &sheet(&columns, rows, false), "t"),
            "| a\\|b | n |\n| --- | --- |\n| x\\|y<br>z | NULL |\n|  | DEFAULT |"
        );
    }

    #[test]
    fn an_insert_names_its_columns_and_quotes_every_literal() {
        let columns = [("id", ty("INTEGER")), ("it's \"q\"", ty("VARCHAR"))];
        let rows = vec![vec![Cell::Text("1"), Cell::Text("O'Brien")], vec![Cell::Default, Cell::Null]];
        assert_eq!(
            render(Format::SqlInsert, &sheet(&columns, rows, true), "\"main\".\"t\""),
            "INSERT INTO \"main\".\"t\" (\"id\", \"it's \"\"q\"\"\") VALUES\n  (1, 'O''Brien'),\n  (DEFAULT, NULL);"
        );
    }

    #[test]
    fn numbers_are_bare_only_where_the_parser_reads_them_exactly() {
        let int = ty("BIGINT");
        assert_eq!(literal(Cell::Text("-42"), &int), "-42");
        assert_eq!(literal(Cell::Text("123456789012345678"), &int), "123456789012345678");
        // Nineteen digits and more the parser would make a DOUBLE.
        let huge = ty("UHUGEINT");
        assert_eq!(
            literal(Cell::Text("340282366920938463463374607431768211455"), &huge),
            "'340282366920938463463374607431768211455'::UHUGEINT"
        );
        let dec = ty("DECIMAL(10,2)");
        assert_eq!(literal(Cell::Text("123.45"), &dec), "123.45");
        let double = ty("DOUBLE");
        assert_eq!(literal(Cell::Text("1e300"), &double), "1e300");
        assert_eq!(literal(Cell::Text("1.5E-7"), &double), "1.5E-7");
        assert_eq!(literal(Cell::Text("NaN"), &double), "'NaN'::DOUBLE");
        assert_eq!(literal(Cell::Text("-Infinity"), &ty("FLOAT")), "'-Infinity'::FLOAT");
        // Text a number cell was staged with, padding and all, is the
        // engine's to cast.
        assert_eq!(literal(Cell::Text(" 5"), &int), "' 5'::BIGINT");
        assert_eq!(literal(Cell::Text("1."), &double), "'1.'::DOUBLE");
    }

    #[test]
    fn scalars_are_written_the_way_the_engine_reads_them_back() {
        assert_eq!(literal(Cell::Text("true"), &ty("BOOLEAN")), "TRUE");
        assert_eq!(literal(Cell::Text("qgBi"), &ty("BLOB")), "from_base64('qgBi')");
        assert_eq!(literal(Cell::Text("{\"a\":[1,2]}"), &ty("JSON")), "'{\"a\":[1,2]}'::JSON");
        let variant = col(r#"{"duckdbType":"VARIANT","lossless":false,"encoding":"json"}"#);
        assert_eq!(literal(Cell::Text("\"Morel\""), &variant), "'\"Morel\"'::JSON");
        assert_eq!(literal(Cell::Text("2024-01-02T03:04:05Z"), &ty("TIMESTAMP WITH TIME ZONE")), "'2024-01-02T03:04:05Z'");
        assert_eq!(literal(Cell::Text("a\\b\nc"), &ty("VARCHAR")), "'a\\b\nc'");
        let interval = ty("INTERVAL");
        assert_eq!(
            literal(Cell::Text(r#"{"months":-1,"days":2,"micros":"3000000"}"#), &interval),
            "INTERVAL '-1 months 2 days 3000000 microseconds'"
        );
        // An interval staged in DuckDB's own words is the engine's to read.
        assert_eq!(literal(Cell::Text("3 days"), &interval), "'3 days'");
    }

    #[test]
    fn nested_values_are_walked_with_their_types() {
        let list = col(r#"{"duckdbType":"VARCHAR[]","lossless":true,"child":{"duckdbType":"VARCHAR","lossless":true}}"#);
        assert_eq!(literal(Cell::Text(r#"["a\"b","c\nd",null]"#), &list), "['a\"b', 'c\nd', NULL]");
        let doubles = col(r#"{"duckdbType":"DOUBLE[]","lossless":true,"child":{"duckdbType":"DOUBLE","lossless":true}}"#);
        assert_eq!(literal(Cell::Text(r#"[1.5,"NaN"]"#), &doubles), "[1.5, 'NaN'::DOUBLE]");
        let mixed = col(
            r#"{"duckdbType":"STRUCT(d DATE, bl BLOB)","lossless":true,"fields":[
                {"name":"d","duckdbType":"DATE","lossless":true},
                {"name":"bl","duckdbType":"BLOB","lossless":true}]}"#,
        );
        assert_eq!(
            literal(Cell::Text(r#"{"d":"2024-01-01","bl":"/w=="}"#), &mixed),
            "{'d': '2024-01-01', 'bl': from_base64('/w==')}"
        );
        let map = col(
            r#"{"duckdbType":"MAP(VARCHAR, INTEGER)","lossless":true,"keyType":{"duckdbType":"VARCHAR","lossless":true},
                "valueType":{"duckdbType":"INTEGER","lossless":true},"encoding":"pairs"}"#,
        );
        assert_eq!(literal(Cell::Text(r#"[["k",1],["j",2]]"#), &map), "MAP {'k': 1, 'j': 2}");
        assert_eq!(literal(Cell::Text("[]"), &map), "MAP {}");
        let union = col(
            r#"{"duckdbType":"UNION(n INTEGER, s VARCHAR)","lossless":true,"members":[
                {"name":"n","duckdbType":"INTEGER","lossless":true},{"name":"s","duckdbType":"VARCHAR","lossless":true}]}"#,
        );
        assert_eq!(
            literal(Cell::Text(r#"{"tag":"n","value":7}"#), &union),
            "union_value(\"n\" := 7)::UNION(n INTEGER, s VARCHAR)"
        );
        let intervals = col(r#"{"duckdbType":"INTERVAL[]","lossless":true,"child":{"duckdbType":"INTERVAL","lossless":true}}"#);
        assert_eq!(
            literal(Cell::Text(r#"[{"months":0,"days":1,"micros":"0"}]"#), &intervals),
            "[INTERVAL '0 months 1 days 0 microseconds']"
        );
        // A VARIANT inside a container crosses as its JSON text.
        let variants = col(
            r#"{"duckdbType":"STRUCT(v VARIANT)","lossless":false,"fields":[
                {"name":"v","duckdbType":"VARIANT","lossless":false,"encoding":"json"}]}"#,
        );
        assert_eq!(literal(Cell::Text(r#"{"v":"{\"a\":1}"}"#), &variants), "{'v': '{\"a\":1}'::JSON}");
        // Text a nested cell was staged with in DuckDB's own form is cast.
        assert_eq!(literal(Cell::Text("{k=1}"), &map), "'{k=1}'");
        // A value of another shape than its type is too.
        assert_eq!(literal(Cell::Text("{\"a\":1}"), &list), "'{\"a\":1}'");
    }

    /// Every type the grid shows, copied as SQL INSERT from the text the
    /// grid holds and run, stores the row it was copied from. Live: needs
    /// `HARBOR_LIVE_DB`, a scratch database file, as the probes in
    /// harbor-client's `tests/live.rs` do. The tables live in an attached
    /// in-memory database, which takes a VARIANT whatever the file's
    /// storage version.
    #[test]
    #[ignore]
    fn an_insert_copied_from_the_grid_stores_the_rows_it_shows() {
        let Some(db) = std::env::var_os("HARBOR_LIVE_DB") else {
            println!("set HARBOR_LIVE_DB to a scratch database file; skipping");
            return;
        };
        let conn = harbor_client::fleet::connect_path(std::path::Path::new(&db)).expect("connect");
        let run = |sql: &str| harbor_client::query(&conn, sql).unwrap_or_else(|e| panic!("{e}\n{sql}"));
        run("ATTACH IF NOT EXISTS ':memory:' AS _dt_copy");
        run(r#"CREATE OR REPLACE TABLE _dt_copy.t AS SELECT * FROM (VALUES
            (1::TINYINT, 170141183460469231731687303715884105727::HUGEINT, 340282366920938463463374607431768211455::UHUGEINT,
             1.1::FLOAT, 'nan'::DOUBLE, '-inf'::DOUBLE, 0.30000000000000004::DOUBLE, 12345678901234567890.123456789::DECIMAL(38,9),
             true, 'it''s "x"' || chr(10) || 'tab' || chr(9) || 'back\slash', '\xAA\x00b'::BLOB, DATE '2024-01-02',
             TIME '12:34:56.789', TIMESTAMP '2024-01-02 03:04:05.123456', TIMESTAMPTZ '2024-01-02 03:04:05+00',
             TIMETZ '12:00:00+05', INTERVAL '-1 month 2 days 3.000001 seconds', '4f9b5a0e-3b0a-4b8e-9f5e-1c2d3e4f5a6b'::UUID,
             '101'::BIT, [1, 2, NULL]::INTEGER[], ['a"b', 'c' || chr(10) || 'd'], [1, 2, 3]::INTEGER[3],
             {'a': 1, 'b': 'x'}, MAP {'k': 1, 'j': 2}, union_value(n := 7)::UNION(n INTEGER, s VARCHAR),
             '{"a":[1,2]}'::JSON, '{"a":{"b":1}}'::JSON::VARIANT, 'x'::ENUM('x', 'y'), NULL::INTEGER,
             [DATE '2024-01-01'], {'d': DATE '2024-01-01', 'bl': '\xFF'::BLOB}, [INTERVAL '1 day'],
             [union_value(n := 1)::UNION(n INTEGER, s VARCHAR), union_value(s := 'two')::UNION(n INTEGER, s VARCHAR)],
             ['1.5'::DOUBLE, 'nan'::DOUBLE], {'v': '"s"'::JSON::VARIANT}, []::INTEGER[], MAP {}::MAP(VARCHAR, INTEGER),
             '"Morel"'::JSON::VARIANT, ''),
            (NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
             NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
             NULL, NULL, NULL, NULL)
            ) v(ti, hi, uh, f, dn, di, d, dec, b, s, bl, dt, tm, ts, tstz, ttz, iv, u, bt, li, ls, arr, st, mp, un,
                js, va, en, nl, ldate, stmix, liv, lun, ldn, sv, le, me, vs, es)"#);
        run("CREATE OR REPLACE TABLE _dt_copy.t2 AS SELECT * FROM _dt_copy.t WHERE false");

        let page = run("SELECT * FROM _dt_copy.t");
        // The grid's own text: a string as it came, anything else its JSON.
        let texts: Vec<Vec<Option<String>>> = page
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|v| match v {
                        serde_json::Value::Null => None,
                        serde_json::Value::String(s) => Some(s.clone()),
                        other => Some(other.to_string()),
                    })
                    .collect()
            })
            .collect();
        let names: Vec<String> = page.columns.iter().map(|c| c.name.clone().unwrap_or_default()).collect();
        let sheet = Sheet {
            columns: names.iter().map(String::as_str).zip(page.columns.iter()).collect(),
            rows: texts
                .iter()
                .map(|row| row.iter().map(|t| t.as_deref().map_or(Cell::Null, Cell::Text)).collect())
                .collect(),
            header: true,
        };
        let insert = render(Format::SqlInsert, &sheet, "_dt_copy.t2");
        run(&insert);
        // A MAP and a VARIANT have no equality EXCEPT can use; they are
        // compared through their text.
        let cmp = "SELECT * EXCLUDE (mp, va, sv, me, vs), mp::VARCHAR, va::JSON::VARCHAR, sv::VARCHAR, me::VARCHAR, \
                   vs::JSON::VARCHAR";
        let differs = run(&format!(
            "SELECT count(*) FROM (({cmp} FROM _dt_copy.t EXCEPT ALL {cmp} FROM _dt_copy.t2) UNION ALL \
             ({cmp} FROM _dt_copy.t2 EXCEPT ALL {cmp} FROM _dt_copy.t))"
        ));
        assert_eq!(crate::sql::count_of(&differs), Some(0), "{insert}");
        // The copy reads back as the same text the grid showed.
        let again = run("SELECT * FROM _dt_copy.t2");
        assert_eq!(again.rows, page.rows, "{insert}");
        run("DETACH _dt_copy");
    }
}
