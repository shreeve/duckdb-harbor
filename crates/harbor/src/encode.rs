//! Wire formatters: the pure half of the NDJSON envelope's JSON.
//!
//! Stateless and engine-free — no FFI, no handles, no duckdb types. The
//! JSON-safe integer rule, temporal formatting, varint decimals,
//! bit/uuid/base64, string escaping, and the keyword table used to quote
//! identifiers in type strings. The engine-facing emission (schema lines,
//! cell values read from vector views) lives in src/v2/encode.rs and calls
//! down into these; the wire bytes are pinned by the v2_encode suite.

/// Render an identifier the way DuckDB does inside a type string: bare when it
/// is a simple lowercase identifier and not a keyword, double-quoted
/// otherwise, with embedded quotes doubled.
pub(crate) fn quote_identifier(name: &str) -> String {
    let simple = !name.is_empty()
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if simple && KEYWORDS.binary_search(&name).is_err() {
        return name.to_string();
    }
    format!("\"{}\"", name.replace('"', "\"\""))
}

// ---------------------------------------------------------------------------
// Scalar emission — the JSON-safe rules every encoder shares.
// ---------------------------------------------------------------------------

/// IEEE-754 doubles hold integers exactly only up to 2^53 - 1. Anything
/// wider goes out quoted; a JavaScript client parsing a bare
/// 9007199254740993 gets 9007199254740992 and never finds out.
const JSON_SAFE: i128 = 9_007_199_254_740_991;

pub(crate) fn push_int(out: &mut String, i: i128) {
    // unsigned_abs, not abs: `i128::MIN` has no positive counterpart, so
    // `abs()` overflows there. In release that wraps back to `i128::MIN`, which
    // compares under the threshold, and the HUGEINT minimum went out as a bare
    // JSON number — the exact silent-reprecision failure this function exists
    // to prevent, on a value any `SELECT (-170141183460469231731687303715884105728)::HUGEINT`
    // produces. In debug it panicked instead.
    if i.unsigned_abs() <= JSON_SAFE as u128 {
        out.push_str(&i.to_string());
    } else {
        push_json_string(out, &i.to_string());
    }
}

pub(crate) fn push_float(out: &mut String, f: f64) {
    // JSON has no NaN or Infinity, but null is not the answer: it is
    // indistinguishable from SQL NULL, so a client cannot tell a missing value
    // from a division that overflowed. The names go out as strings instead.
    if f.is_nan() {
        return push_json_string(out, "NaN");
    }
    if f.is_infinite() {
        return push_json_string(out, if f > 0.0 { "Infinity" } else { "-Infinity" });
    }
    // Rust's Display never switches to exponent notation for large magnitudes,
    // so f64::MAX would go out as 309 digits. Switch at 1e21, which is where
    // JavaScript's own number formatting switches, so the text a client reads
    // is the text it would have produced itself.
    if f != 0.0 && f.abs() >= 1e21 {
        push_exponent(out, &format!("{f:e}"));
    } else {
        out.push_str(&f.to_string());
    }
}

/// A FLOAT, formatted as the f32 it is.
///
/// This used to widen to f64 first and format that, which is lossless but not
/// faithful: `0.1::FLOAT` went out as 0.10000000149011612 — the same number,
/// but not the text DuckDB writes, not the text an f32-aware client writes,
/// and visibly *less* precise than the DOUBLE column holding the same literal
/// right beside it. `f32::to_string` gives the shortest text that round-trips
/// back to the same f32, which is what every other numeric type here does.
pub(crate) fn push_float32(out: &mut String, f: f32) {
    if f.is_nan() {
        return push_json_string(out, "NaN");
    }
    if f.is_infinite() {
        return push_json_string(out, if f > 0.0 { "Infinity" } else { "-Infinity" });
    }
    // Same 1e21 switch as f64, and it is reachable: f32::MAX is ~3.4e38.
    if f != 0.0 && f.abs() >= 1e21 {
        push_exponent(out, &format!("{f:e}"));
    } else {
        out.push_str(&f.to_string());
    }
}

/// Rust writes `1e21`; JSON and JavaScript write `1e+21`. Only a positive
/// exponent is missing its sign.
fn push_exponent(out: &mut String, formatted: &str) {
    match formatted.split_once('e') {
        Some((mantissa, exponent)) if !exponent.starts_with('-') => {
            out.push_str(mantissa);
            out.push_str("e+");
            out.push_str(exponent);
        }
        _ => out.push_str(formatted),
    }
}

pub(crate) fn push_json_string(out: &mut String, s: &str) {
    // serde_json owns the escaping rules, including the ones that are easy to
    // get wrong (control characters, lone surrogates).
    let encoded = match serde_json::to_string(s) {
        Ok(encoded) => encoded,
        Err(_) => return out.push_str("\"\""),
    };
    // One rule serde_json correctly does not apply, because it is about the
    // container rather than the value: U+2028 LINE SEPARATOR and U+2029
    // PARAGRAPH SEPARATOR are legal inside a JSON string, but this is a
    // newline-delimited format and they are line terminators to every
    // Unicode-aware line splitter. Left raw, one row is read as two — and the
    // half that is left over is not valid JSON, so a client sees a parse error
    // whose cause is nowhere near where it happened. Escaping them costs a
    // scan that almost always finds nothing.
    if !encoded.contains('\u{2028}') && !encoded.contains('\u{2029}') {
        return out.push_str(&encoded);
    }
    for ch in encoded.chars() {
        match ch {
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            other => out.push(other),
        }
    }
}

// ---------------------------------------------------------------------------
// Scalar formatting
// ---------------------------------------------------------------------------

/// Days since 1970-01-01 to a civil date, by Howard Hinnant's
/// `civil_from_days`. Correct for the proleptic Gregorian calendar over the
/// whole int32 range, which is more than DATE can hold.
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub(crate) fn fmt_date(days: i32) -> String {
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// HH:MM:SS, with a fraction only when there is one. Six digits unless the
/// value carries sub-microsecond precision.
pub(crate) fn fmt_time(nanos: i128) -> String {
    let (h, min, s, frac) = split_time(nanos.rem_euclid(86_400_000_000_000));
    let mut out = format!("{h:02}:{min:02}:{s:02}");
    push_fraction(&mut out, frac);
    out
}

pub(crate) fn split_time(nanos_in_day: i128) -> (i64, i64, i64, i64) {
    let total_s = (nanos_in_day / 1_000_000_000) as i64;
    let frac = (nanos_in_day % 1_000_000_000) as i64;
    (total_s / 3_600, (total_s % 3_600) / 60, total_s % 60, frac)
}

pub(crate) fn push_fraction(out: &mut String, nanos: i64) {
    if nanos == 0 {
        return;
    }
    // Six digits for microsecond precision, nine when the value actually
    // carries nanoseconds. Trailing zeros come off either way: a TIMESTAMP_MS
    // of .123 should read as .123, not .123000.
    let mut digits = if nanos % 1_000 == 0 {
        format!("{:06}", nanos / 1_000)
    } else {
        format!("{nanos:09}")
    };
    while digits.ends_with('0') {
        digits.pop();
    }
    out.push('.');
    out.push_str(&digits);
}

/// DuckDB stores BIGNUM (formerly VARINT) as a three-byte header followed by
/// the magnitude, most significant byte first. Without this the value goes out
/// base64-encoded — DuckDB's private storage layout, leaked onto the wire,
/// where no client could read it and nothing would say it was wrong.
///
/// The header's top bit is the sign: 1 positive, 0 negative. Its remaining 23
/// bits are the number of magnitude bytes. For negative values *both* the
/// length field and the magnitude are stored one's-complemented, which is what
/// makes the raw bytes sort correctly as unsigned — and what makes a decoder
/// that only complements the magnitude quietly wrong about the length.
///
/// Returns `None` if the bytes are not a well-formed BIGNUM, so the caller can
/// fall back rather than emit a confidently wrong number.
pub(crate) fn varint_to_decimal(bytes: &[u8]) -> Option<String> {
    const HEADER: usize = 3;
    if bytes.len() < HEADER {
        return None;
    }
    let positive = bytes[0] & 0x80 != 0;
    let raw = (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]);
    let declared = if positive { raw & 0x7f_ffff } else { !raw & 0x7f_ffff };
    let data = &bytes[HEADER..];
    if declared as usize != data.len() {
        return None;
    }

    let mut magnitude: Vec<u8> =
        if positive { data.to_vec() } else { data.iter().map(|b| !b).collect() };

    // Long division by 10^9, most significant byte first, taking nine decimal
    // digits per pass. Each quotient digit is `(rem << 8 | byte) / 10^9` with
    // `rem < 10^9`, so it is always under 256 and a byte can hold it.
    let first = magnitude.iter().position(|&b| b != 0).unwrap_or(magnitude.len());
    magnitude.drain(..first);
    if magnitude.is_empty() {
        return Some("0".into());
    }
    let mut groups: Vec<u32> = Vec::new();
    while !magnitude.is_empty() {
        let mut rem: u64 = 0;
        let mut quotient: Vec<u8> = Vec::with_capacity(magnitude.len());
        for &b in &magnitude {
            let cur = (rem << 8) | u64::from(b);
            quotient.push((cur / 1_000_000_000) as u8);
            rem = cur % 1_000_000_000;
        }
        groups.push(rem as u32);
        let nz = quotient.iter().position(|&b| b != 0).unwrap_or(quotient.len());
        magnitude = quotient[nz..].to_vec();
    }

    let mut out = String::with_capacity(groups.len() * 9 + 1);
    if !positive {
        out.push('-');
    }
    // The most significant group carries no leading zeros; every later one is
    // padded to the full nine digits it was divided out as.
    out.push_str(&groups.pop().unwrap_or(0).to_string());
    while let Some(g) = groups.pop() {
        out.push_str(&format!("{g:09}"));
    }
    Some(out)
}

/// DuckDB stores BIT as a leading pad-count byte followed by the bits, most
/// significant first. Without this a bit string goes out base64-encoded, which
/// is not wrong so much as unusable.
pub(crate) fn bit_string(bytes: &[u8]) -> String {
    let Some((&padding, data)) = bytes.split_first() else {
        return String::new();
    };
    let skip = padding as usize;
    let mut out = String::with_capacity(data.len() * 8);
    for (i, byte) in data.iter().enumerate() {
        for bit in (0..8).rev() {
            if i * 8 + (7 - bit) >= skip {
                out.push(if byte >> bit & 1 == 1 { '1' } else { '0' });
            }
        }
    }
    out
}

/// DuckDB stores UUID as a HUGEINT with the high bit flipped, so that the
/// integer ordering matches the textual ordering.
pub(crate) fn uuid_to_string(v: i128) -> String {
    let bits = (v as u128) ^ (1u128 << 127);
    let b = bits.to_be_bytes();
    let hex = |r: &[u8]| r.iter().map(|x| format!("{x:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        hex(&b[0..4]),
        hex(&b[4..6]),
        hex(&b[6..8]),
        hex(&b[8..10]),
        hex(&b[10..16])
    )
}

pub(crate) fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

// ==========================================================================
//
// DuckDB's reserved words, generated
//
// ==========================================================================


// Generated from `SELECT keyword_name FROM duckdb_keywords()` on DuckDB v1.5.5.
//
// DuckDB quotes an identifier in a type string when it is any keyword at all,
// reserved or not — which is why a STRUCT field called `name` comes back as
// STRUCT("name" VARCHAR). Reproducing that keeps the duckdbType string valid
// SQL, so a client can paste it back into a CREATE TABLE.

/// Sorted, lowercase. Binary-searched, so it must stay sorted.
pub(crate) static KEYWORDS: &[&str] = &[
    "abort", "absolute", "access", "action", "add", "admin", "after", "aggregate", "all",
    "also", "alter", "always", "analyse", "analyze", "and", "anti", "any", "array", "as",
    "asc", "asof", "assertion", "assignment", "asymmetric", "at", "attach", "attribute",
    "authorization", "backward", "before", "begin", "between", "bigint", "binary", "bit",
    "boolean", "both", "by", "cache", "call", "called", "cascade", "cascaded", "case", "cast",
    "catalog", "centuries", "century", "chain", "char", "character", "characteristics",
    "check", "checkpoint", "class", "close", "cluster", "coalesce", "collate", "collation",
    "column", "columns", "comment", "comments", "commit", "committed", "compression",
    "concurrently", "configuration", "conflict", "connection", "constraint", "constraints",
    "content", "continue", "conversion", "copy", "cost", "create", "cross", "csv", "cube",
    "current", "cursor", "cycle", "data", "database", "day", "days", "deallocate", "dec",
    "decade", "decades", "decimal", "declare", "default", "defaults", "deferrable", "deferred",
    "definer", "delete", "delimiter", "delimiters", "depends", "desc", "describe", "detach",
    "dictionary", "disable", "discard", "distinct", "do", "document", "domain", "double",
    "drop", "each", "else", "enable", "encoding", "encrypted", "end", "enum", "error",
    "escape", "event", "except", "exclude", "excluding", "exclusive", "execute", "exists",
    "explain", "export", "export_state", "extension", "extensions", "external", "extract",
    "false", "family", "fetch", "filter", "first", "float", "following", "for", "force",
    "foreign", "forward", "freeze", "from", "full", "function", "functions", "generated",
    "glob", "global", "grant", "granted", "group", "grouping", "grouping_id", "groups",
    "handler", "having", "header", "hold", "hour", "hours", "identity", "if", "ignore",
    "ilike", "immediate", "immutable", "implicit", "import", "in", "include", "including",
    "increment", "index", "indexes", "inherit", "inherits", "initially", "inline", "inner",
    "inout", "input", "insensitive", "insert", "install", "instead", "int", "integer",
    "intersect", "interval", "into", "invoker", "is", "isnull", "isolation", "join", "json",
    "key", "label", "lambda", "language", "large", "last", "lateral", "leading", "leakproof",
    "left", "level", "like", "limit", "listen", "load", "local", "location", "lock", "locked",
    "logged", "macro", "map", "mapping", "match", "matched", "materialized", "maxvalue",
    "merge", "method", "microsecond", "microseconds", "millennia", "millennium", "millisecond",
    "milliseconds", "minute", "minutes", "minvalue", "mode", "month", "months", "move", "name",
    "names", "national", "natural", "nchar", "new", "next", "no", "none", "not", "nothing",
    "notify", "notnull", "nowait", "null", "nullif", "nulls", "numeric", "object", "of", "off",
    "offset", "oids", "old", "on", "only", "operator", "option", "options", "or", "order",
    "ordinality", "others", "out", "outer", "over", "overlaps", "overlay", "overriding",
    "owned", "owner", "parallel", "parser", "partial", "partition", "partitioned", "passing",
    "password", "percent", "persistent", "pivot", "pivot_longer", "pivot_wider", "placing",
    "plans", "policy", "position", "positional", "pragma", "preceding", "precision", "prepare",
    "prepared", "preserve", "primary", "prior", "privileges", "procedural", "procedure",
    "program", "publication", "qualify", "quarter", "quarters", "quote", "range", "read",
    "real", "reassign", "recheck", "recursive", "ref", "references", "referencing", "refresh",
    "reindex", "relative", "release", "rename", "repeatable", "replace", "replica", "reset",
    "respect", "restart", "restrict", "returning", "returns", "revoke", "right", "role",
    "rollback", "rollup", "row", "rows", "rule", "sample", "savepoint", "schema", "schemas",
    "scope", "scroll", "search", "second", "seconds", "secret", "security", "select", "semi",
    "sequence", "sequences", "serializable", "server", "session", "set", "setof", "sets",
    "share", "show", "similar", "simple", "skip", "smallint", "snapshot", "some", "sorted",
    "source", "sql", "stable", "standalone", "start", "statement", "statistics", "stdin",
    "stdout", "storage", "stored", "strict", "strip", "struct", "subscription", "substring",
    "summarize", "symmetric", "sysid", "system", "table", "tables", "tablesample",
    "tablespace", "target", "temp", "template", "temporary", "text", "then", "ties", "time",
    "timestamp", "to", "trailing", "transaction", "transform", "treat", "trigger", "trim",
    "true", "truncate", "trusted", "try_cast", "type", "types", "unbounded", "uncommitted",
    "unencrypted", "union", "unique", "unknown", "unlisten", "unlogged", "unpack", "unpivot",
    "until", "update", "use", "user", "using", "vacuum", "valid", "validate", "validator",
    "value", "values", "varchar", "variable", "variadic", "varying", "verbose", "version",
    "view", "views", "virtual", "volatile", "week", "weeks", "when", "where", "whitespace",
    "window", "with", "within", "without", "work", "wrapper", "write", "xml", "xmlattributes",
    "xmlconcat", "xmlelement", "xmlexists", "xmlforest", "xmlnamespaces", "xmlparse", "xmlpi",
    "xmlroot", "xmlserialize", "xmltable", "year", "years", "yes", "zone",
];
