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

/// Every two-digit number, as bytes: "000102...9899". One table lookup
/// replaces two divisions in the digit writers below.
pub(crate) const DIGIT_PAIRS: &[u8; 200] = b"\
0001020304050607080910111213141516171819\
2021222324252627282930313233343536373839\
4041424344454647484950515253545556575859\
6061626364656667686970717273747576777879\
8081828384858687888990919293949596979899";

/// The decimal digits of a u64, written straight into `out` — the same bytes
/// `u64::to_string` produces, without the String it allocates. This is the
/// workhorse under every integer on the wire.
pub(crate) fn push_u64_raw(out: &mut String, mut v: u64) {
    let mut buf = [0u8; 20]; // u64::MAX has 20 digits
    let mut i = buf.len();
    while v >= 100 {
        let pair = ((v % 100) as usize) * 2;
        v /= 100;
        i -= 2;
        buf[i..i + 2].copy_from_slice(&DIGIT_PAIRS[pair..pair + 2]);
    }
    if v >= 10 {
        let pair = (v as usize) * 2;
        i -= 2;
        buf[i..i + 2].copy_from_slice(&DIGIT_PAIRS[pair..pair + 2]);
    } else {
        i -= 1;
        buf[i] = b'0' + v as u8;
    }
    // Safety: the slice holds only ASCII digits.
    out.push_str(unsafe { std::str::from_utf8_unchecked(&buf[i..]) });
}

/// `u128::to_string`'s bytes without its String. The 128-bit divide loop is
/// an order of magnitude slower than the 64-bit one, so anything that fits —
/// which is everything but HUGEINT/UHUGEINT tails — takes the u64 path.
pub(crate) fn push_u128_raw(out: &mut String, v: u128) {
    if let Ok(small) = u64::try_from(v) {
        return push_u64_raw(out, small);
    }
    let mut buf = [0u8; 39]; // u128::MAX has 39 digits
    let mut i = buf.len();
    let mut v = v;
    // Peel 19-digit chunks: at most two 128-bit divisions, the rest u64.
    while v > u64::MAX as u128 {
        let mut chunk = (v % 10_000_000_000_000_000_000) as u64; // 10^19
        v /= 10_000_000_000_000_000_000;
        for _ in 0..19 {
            i -= 1;
            buf[i] = b'0' + (chunk % 10) as u8;
            chunk /= 10;
        }
    }
    let mut v64 = v as u64;
    loop {
        i -= 1;
        buf[i] = b'0' + (v64 % 10) as u8;
        v64 /= 10;
        if v64 == 0 {
            break;
        }
    }
    // Safety: the slice holds only ASCII digits.
    out.push_str(unsafe { std::str::from_utf8_unchecked(&buf[i..]) });
}

/// `i128::to_string`'s bytes, allocation-free: a sign, then the digits.
pub(crate) fn push_i128_raw(out: &mut String, v: i128) {
    if v < 0 {
        out.push('-');
    }
    push_u128_raw(out, v.unsigned_abs());
}

/// `i64::to_string`'s bytes, allocation-free — the whole path stays 64-bit.
pub(crate) fn push_i64_raw(out: &mut String, v: i64) {
    if v < 0 {
        out.push('-');
    }
    push_u64_raw(out, v.unsigned_abs());
}

/// An integer zero-padded to `width` the way `format!("{v:0width$}")` pads:
/// the sign first, then zeros, then the digits, sign counted toward the width.
pub(crate) fn push_int_pad(out: &mut String, v: i64, width: usize) {
    let neg = v < 0;
    if neg {
        out.push('-');
    }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    let mut x = v.unsigned_abs();
    loop {
        i -= 1;
        buf[i] = b'0' + (x % 10) as u8;
        x /= 10;
        if x == 0 {
            break;
        }
    }
    let written = (buf.len() - i) + neg as usize;
    for _ in written..width {
        out.push('0');
    }
    // Safety: the slice holds only ASCII digits.
    out.push_str(unsafe { std::str::from_utf8_unchecked(&buf[i..]) });
}

pub(crate) fn push_int(out: &mut String, i: i128) {
    // unsigned_abs, not abs: `i128::MIN` has no positive counterpart, so
    // `abs()` overflows there. In release that wraps back to `i128::MIN`, which
    // compares under the threshold, and the HUGEINT minimum went out as a bare
    // JSON number — the exact silent-reprecision failure this function exists
    // to prevent, on a value any `SELECT (-170141183460469231731687303715884105728)::HUGEINT`
    // produces. In debug it panicked instead.
    if i.unsigned_abs() <= JSON_SAFE as u128 {
        // Under 2^53 always fits i64; stay on the 64-bit digit writer.
        push_i64_raw(out, i as i64);
    } else {
        // Digits and a sign need no JSON escaping; the quotes are the whole
        // encoding.
        out.push('"');
        push_i128_raw(out, i);
        out.push('"');
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
        // Display, written straight into the buffer: the same shortest
        // round-trip text `to_string` yields, without its String.
        let _ = std::fmt::Write::write_fmt(out, format_args!("{f}"));
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
        let _ = std::fmt::Write::write_fmt(out, format_args!("{f}"));
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
    // One pass, byte-identical to what serde_json::to_string used to produce
    // here (its escaping rules are reproduced below and pinned by a
    // fuzz-comparison test), plus one rule serde_json correctly does not
    // apply because it is about the container rather than the value: U+2028
    // LINE SEPARATOR and U+2029 PARAGRAPH SEPARATOR are legal inside a JSON
    // string, but this is a newline-delimited format and they are line
    // terminators to every Unicode-aware line splitter. Left raw, one row is
    // read as two — and the half that is left over is not valid JSON, so a
    // client sees a parse error whose cause is nowhere near where it
    // happened. Writing straight into `out` drops the String serde_json
    // allocated per cell and the two container scans over it.
    out.push('"');
    let bytes = s.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        // The overwhelmingly common byte: printable, not a quote or
        // backslash, and not the 0xE2 that could open U+2028/U+2029.
        if b >= 0x20 && b != b'"' && b != b'\\' && b != 0xE2 {
            i += 1;
            continue;
        }
        if b == 0xE2 {
            // U+2028 is E2 80 A8, U+2029 is E2 80 A9; every other E2
            // sequence passes through raw.
            if bytes.len() - i >= 3 && bytes[i + 1] == 0x80 && bytes[i + 2] & 0xFE == 0xA8 {
                out.push_str(&s[start..i]);
                out.push_str(if bytes[i + 2] == 0xA8 { "\\u2028" } else { "\\u2029" });
                i += 3;
                start = i;
            } else {
                i += 1;
            }
            continue;
        }
        out.push_str(&s[start..i]);
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x08 => out.push_str("\\b"),
            0x09 => out.push_str("\\t"),
            0x0A => out.push_str("\\n"),
            0x0C => out.push_str("\\f"),
            0x0D => out.push_str("\\r"),
            c => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out.push_str("\\u00");
                out.push(HEX[(c >> 4) as usize] as char);
                out.push(HEX[(c & 0xF) as usize] as char);
            }
        }
        i += 1;
        start = i;
    }
    out.push_str(&s[start..]);
    out.push('"');
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

/// The two digits of a value in 0..=99, from the pair table.
#[inline]
pub(crate) fn digit_pair(v: usize) -> [u8; 2] {
    [DIGIT_PAIRS[v * 2], DIGIT_PAIRS[v * 2 + 1]]
}

pub(crate) fn push_date(out: &mut String, days: i32) {
    let (y, m, d) = civil_from_days(days as i64);
    if (0..=9999).contains(&y) {
        // The common era: one 10-byte write instead of five small ones.
        let mut b = [0u8; 10];
        b[0..2].copy_from_slice(&digit_pair(y as usize / 100));
        b[2..4].copy_from_slice(&digit_pair(y as usize % 100));
        b[4] = b'-';
        b[5..7].copy_from_slice(&digit_pair(m as usize));
        b[7] = b'-';
        b[8..10].copy_from_slice(&digit_pair(d as usize));
        // Safety: the buffer holds only ASCII digits and dashes.
        out.push_str(unsafe { std::str::from_utf8_unchecked(&b) });
    } else {
        push_int_pad(out, y, 4);
        out.push('-');
        push_int_pad(out, m as i64, 2);
        out.push('-');
        push_int_pad(out, d as i64, 2);
    }
}

/// HH:MM:SS, with a fraction only when there is one. Six digits unless the
/// value carries sub-microsecond precision.
pub(crate) fn push_time(out: &mut String, nanos: i128) {
    // The i64 path covers every value the encoders pass (micros × 1000 can
    // exceed i64, hence the i128 signature — but only barely, and rem_euclid
    // on i128 is an order of magnitude slower).
    let day = 86_400_000_000_000;
    let ns = match i64::try_from(nanos) {
        Ok(n) => n.rem_euclid(day),
        Err(_) => nanos.rem_euclid(day as i128) as i64,
    };
    let (h, min, s, frac) = split_time(ns);
    let mut b = [0u8; 8];
    b[0..2].copy_from_slice(&digit_pair(h as usize));
    b[2] = b':';
    b[3..5].copy_from_slice(&digit_pair(min as usize));
    b[5] = b':';
    b[6..8].copy_from_slice(&digit_pair(s as usize));
    // Safety: the buffer holds only ASCII digits and colons.
    out.push_str(unsafe { std::str::from_utf8_unchecked(&b) });
    push_fraction(out, frac);
}

/// Splits nanoseconds-since-midnight (already reduced below one day, so it
/// fits and stays in i64) into hours, minutes, seconds, and the fraction.
pub(crate) fn split_time(nanos_in_day: i64) -> (i64, i64, i64, i64) {
    let total_s = nanos_in_day / 1_000_000_000;
    let frac = nanos_in_day % 1_000_000_000;
    (total_s / 3_600, (total_s % 3_600) / 60, total_s % 60, frac)
}

pub(crate) fn push_fraction(out: &mut String, nanos: i64) {
    if nanos == 0 {
        return;
    }
    // Six digits for microsecond precision, nine when the value actually
    // carries nanoseconds. Trailing zeros come off either way: a TIMESTAMP_MS
    // of .123 should read as .123, not .123000.
    let (mut v, width) = if nanos % 1_000 == 0 {
        ((nanos / 1_000) as u64, 6)
    } else {
        (nanos as u64, 9)
    };
    let mut buf = [b'0'; 9];
    let mut i = width;
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    // The zero-fill already padded the front; trim the back. At least one
    // nonzero digit exists because nanos != 0.
    let mut end = width;
    while buf[end - 1] == b'0' {
        end -= 1;
    }
    out.push('.');
    // Safety: the slice holds only ASCII digits.
    out.push_str(unsafe { std::str::from_utf8_unchecked(&buf[..end]) });
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
pub(crate) fn push_bit_string(out: &mut String, bytes: &[u8]) {
    let Some((&padding, data)) = bytes.split_first() else {
        return;
    };
    let skip = padding as usize;
    out.reserve(data.len() * 8);
    for (i, byte) in data.iter().enumerate() {
        for bit in (0..8).rev() {
            if i * 8 + (7 - bit) >= skip {
                out.push(if byte >> bit & 1 == 1 { '1' } else { '0' });
            }
        }
    }
}

/// DuckDB stores UUID as a HUGEINT with the high bit flipped, so that the
/// integer ordering matches the textual ordering.
pub(crate) fn push_uuid(out: &mut String, v: i128) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bits = (v as u128) ^ (1u128 << 127);
    let b = bits.to_be_bytes();
    for (i, byte) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xF) as usize] as char);
    }
}

pub(crate) fn push_base64(out: &mut String, data: &[u8]) {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    out.reserve(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference bytes push_json_string must reproduce: serde_json's
    /// escaping, then the U+2028/U+2029 post-pass — exactly the two-step
    /// encoding this function replaced.
    fn reference(s: &str) -> String {
        let encoded = serde_json::to_string(s).unwrap();
        let mut out = String::new();
        for ch in encoded.chars() {
            match ch {
                '\u{2028}' => out.push_str("\\u2028"),
                '\u{2029}' => out.push_str("\\u2029"),
                other => out.push(other),
            }
        }
        out
    }

    fn ours(s: &str) -> String {
        let mut out = String::new();
        push_json_string(&mut out, s);
        out
    }

    #[test]
    fn json_string_matches_serde_on_adversarial_inputs() {
        let cases: &[&str] = &[
            "",
            "plain ascii",
            "quote \" backslash \\ done",
            "\\\\\"\"",
            "\u{0}\u{1}\u{2}\u{3}\u{4}\u{5}\u{6}\u{7}\u{8}\u{9}\u{a}\u{b}\u{c}\u{d}\u{e}\u{f}",
            "\u{10}\u{11}\u{12}\u{13}\u{14}\u{15}\u{16}\u{17}\u{18}\u{19}\u{1a}\u{1b}\u{1c}\u{1d}\u{1e}\u{1f}",
            "\u{7f}",           // DEL passes through raw
            "\u{2028}",         // line separator, escaped for NDJSON
            "\u{2029}",         // paragraph separator
            "a\u{2028}b\u{2029}c",
            "\u{2027}\u{202a}", // E2 80 A7 / E2 80 AA — neighbors stay raw
            "\u{2088}\u{20a8}", // other E2-lead chars sharing trailing bytes
            "héllo wörld",
            "日本語テキスト",
            "🦆 emoji \u{10ffff}",
            "mixed \" \u{2028} \\ \u{1} 中 🦆 end",
            "ends with lead-alike \u{2028}",
            "\u{2028}starts",
            "e2 near end \u{e0a8}",
        ];
        for s in cases {
            assert_eq!(ours(s), reference(s), "for {s:?}");
        }
    }

    #[test]
    fn json_string_matches_serde_on_random_inputs() {
        // A cheap deterministic PRNG over a hostile alphabet: escapes,
        // controls, E2-family multibyte chars, plain ASCII.
        let alphabet: Vec<char> = ('\u{0}'..='\u{2f}')
            .chain(['"', '\\', '\u{7f}', '\u{2027}', '\u{2028}', '\u{2029}', '\u{202a}'])
            .chain(['\u{2088}', '\u{20a8}', 'é', '中', '🦆', 'a', 'z', '\u{e0a8}'])
            .collect();
        let mut state = 0x243F6A8885A308D3u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let len = (next() % 24) as usize;
            let s: String =
                (0..len).map(|_| alphabet[next() as usize % alphabet.len()]).collect();
            assert_eq!(ours(&s), reference(&s), "for {s:?}");
        }
    }

    #[test]
    fn int_pad_matches_format() {
        for &(v, w) in &[
            (0i64, 2usize),
            (0, 4),
            (5, 2),
            (5, 4),
            (42, 2),
            (999, 2),
            (1234, 4),
            (12345, 4),
            (-5, 4),
            (-123, 4),
            (-1234, 4),
            (-12345, 4),
            (5877642, 4),
            (-5877641, 4),
            (i64::MAX, 4),
            (i64::MIN, 4),
        ] {
            let mut out = String::new();
            push_int_pad(&mut out, v, w);
            assert_eq!(out, format!("{v:0w$}"), "for {v} width {w}");
        }
    }

    #[test]
    fn raw_ints_match_to_string() {
        for v in [
            0i128,
            1,
            -1,
            9,
            10,
            -10,
            i128::from(i64::MAX),
            i128::from(i64::MIN),
            i128::MAX,
            i128::MIN,
        ] {
            let mut out = String::new();
            push_i128_raw(&mut out, v);
            assert_eq!(out, v.to_string());
        }
        for v in [0u128, 1, 9, 10, u128::from(u64::MAX), u128::MAX] {
            let mut out = String::new();
            push_u128_raw(&mut out, v);
            assert_eq!(out, v.to_string());
        }
    }
}
