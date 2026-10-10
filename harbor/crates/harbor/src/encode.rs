//! Wire formatters: the pure half of the NDJSON envelope's JSON.
//!
//! Stateless and engine-free — no FFI, no handles, no duckdb types. The
//! JSON-safe integer rule, temporal formatting, varint decimals,
//! bit/uuid/base64 and string escaping. The engine-facing emission (schema lines,
//! cell values read from vector views) lives in src/engine/encode.rs and calls
//! down into these; the wire bytes are pinned by tests/engine.rs.

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

/// A year outside 0000 to 9999, in ISO 8601's expanded form: a sign and six
/// digits (`-000043` is 44 BC, `+010000`), more for a year that needs them.
/// It is the form JavaScript's `toISOString` writes and the only one its
/// `Date` reads; `-0043` reads there as the year 2043. DuckDB reads the
/// negative form back, and refuses the `+`.
pub(crate) fn push_expanded_year(out: &mut String, y: i64) {
    out.push(if y < 0 { '-' } else { '+' });
    push_int_pad(out, y.abs(), 6);
}

/// An integer with its digits zero-padded to `width`, the sign before them
/// and not counted.
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
    for _ in buf.len() - i..width {
        out.push('0');
    }
    // Safety: the slice holds only ASCII digits.
    out.push_str(unsafe { std::str::from_utf8_unchecked(&buf[i..]) });
}

pub(crate) fn push_int(out: &mut String, i: i128) {
    // unsigned_abs, not abs: `i128::MIN` has no positive counterpart, so
    // `abs()` overflows there, and in release wraps back to `i128::MIN`, which
    // compares under the threshold: the HUGEINT minimum would go out as a
    // bare JSON number, the silent reprecision this function exists to
    // prevent.
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

/// The unsigned mirror of [`push_int`]: the single home for the JSON-safe rule
/// on the u128 side (UBIGINT past i64, UHUGEINT). Anything wider than 2^53 - 1
/// goes out quoted so a JS client never silently reprecisions it.
pub(crate) fn push_uint(out: &mut String, v: u128) {
    if v <= JSON_SAFE as u128 {
        push_u128_raw(out, v);
    } else {
        out.push('"');
        push_u128_raw(out, v);
        out.push('"');
    }
}

/// A DOUBLE or a FLOAT, as the shortest text that round-trips to the same
/// value of its own width. A FLOAT widened to f64 first would read
/// 0.10000000149011612 for `0.1::FLOAT`: the same number, but not the text
/// DuckDB writes, and less precise to the eye than a DOUBLE beside it.
pub(crate) fn push_float<F>(out: &mut String, f: F)
where
    F: Copy + Into<f64> + std::fmt::Display + std::fmt::LowerExp,
{
    // Widening is exact, so it classifies an f32 as well as an f64.
    let wide: f64 = f.into();
    // JSON has no NaN or Infinity, but null is not the answer: it is
    // indistinguishable from SQL NULL, so a client cannot tell a missing value
    // from a division that overflowed. The names go out as strings instead.
    if wide.is_nan() {
        return push_json_string(out, "NaN");
    }
    if wide.is_infinite() {
        return push_json_string(out, if wide > 0.0 { "Infinity" } else { "-Infinity" });
    }
    // Rust's Display never switches to exponent notation for large magnitudes,
    // so f64::MAX would go out as 309 digits (and f32::MAX as 39). Switch at
    // 1e21, where JavaScript's own formatting switches for large magnitudes.
    if wide.abs() >= 1e21 {
        push_exponent(out, &format!("{f:e}"));
    } else {
        // Display, written straight into the buffer: the same shortest
        // round-trip text `to_string` yields, without its String.
        let _ = std::fmt::Write::write_fmt(out, format_args!("{f}"));
    }
}

/// The engine's JSON text for a VARIANT writes a non-finite double as a bare
/// `NaN`, `Infinity` or `-Infinity`, which is not JSON: a client's parser
/// throws on the whole row. Each goes out as the string a DOUBLE column sends
/// ([`push_float`]). Outside a string, JSON text holds no other capital
/// letter, so the scan is skipped for text with no `N` or `I` at all.
pub(crate) fn quote_nonfinite(json: &str) -> std::borrow::Cow<'_, str> {
    let b = json.as_bytes();
    if !b.iter().any(|&c| c == b'N' || c == b'I') {
        return json.into();
    }
    let mut out = String::with_capacity(json.len() + 8);
    let (mut start, mut i, mut in_string) = (0, 0, false);
    while i < b.len() {
        match b[i] {
            b'\\' if in_string => i += 1,
            b'"' => in_string = !in_string,
            b'N' | b'I' | b'-' if !in_string => {
                let word = ["NaN", "Infinity", "-Infinity"].into_iter().find(|w| json[i..].starts_with(w));
                if let Some(word) = word {
                    out.push_str(&json[start..i]);
                    out.push('"');
                    out.push_str(word);
                    out.push('"');
                    i += word.len();
                    start = i;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
    out.push_str(&json[start..]);
    out.into()
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
    // One pass, byte-identical to serde_json::to_string (its escaping rules
    // are reproduced below and pinned by a fuzz-comparison test), plus one
    // rule serde_json correctly does not apply because it is about the
    // container rather than the value: U+2028 LINE SEPARATOR and U+2029
    // PARAGRAPH SEPARATOR are legal inside a JSON string, but this is a
    // newline-delimited format and they are line terminators to every
    // Unicode-aware line splitter. Left raw, one row is read as two — and the
    // half that is left over is not valid JSON, so a client sees a parse
    // error whose cause is nowhere near where it happened. Writing straight
    // into `out` saves a String per cell and the scans over it.
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
        push_expanded_year(out, y);
        out.push('-');
        push_int_pad(out, m as i64, 2);
        out.push('-');
        push_int_pad(out, d as i64, 2);
    }
}

/// ±HH:MM, plus :SS only when the offset has seconds. ISO 8601's shape
/// (DuckDB's own text form drops the minutes when zero, which standard
/// datetime parsers refuse); the :SS case is RFC-less but DuckDB accepts
/// offsets down to ±15:59:59 and dropping seconds would un-round-trip them.
pub(crate) fn push_tz_offset(out: &mut String, seconds: i32) {
    let (sign, s) = match seconds < 0 {
        true => ('-', -seconds as usize),
        false => ('+', seconds as usize),
    };
    out.push(sign);
    let mut b = [0u8; 5];
    b[0..2].copy_from_slice(&digit_pair(s / 3600));
    b[2] = b':';
    b[3..5].copy_from_slice(&digit_pair(s / 60 % 60));
    // Safety: the buffer holds only ASCII digits and a colon.
    out.push_str(unsafe { std::str::from_utf8_unchecked(&b) });
    if s % 60 != 0 {
        out.push(':');
        let sec = digit_pair(s % 60);
        out.push_str(unsafe { std::str::from_utf8_unchecked(&sec) });
    }
}

/// HH:MM:SS, with a fraction only when there is one. Six digits unless the
/// value carries sub-microsecond precision.
pub(crate) fn push_time(out: &mut String, nanos: i128) {
    // The i64 path covers every value the encoders pass (micros × 1000 can
    // exceed i64, hence the i128 signature — but only barely, and rem_euclid
    // on i128 is an order of magnitude slower).
    let day = 86_400_000_000_000;
    // 24:00:00 — the SQL standard's end-of-day — is a legal TIME distinct
    // from 00:00:00, and DuckDB stores and compares it as such. It is also
    // exactly one day, so the wraparound below would fold it onto midnight
    // and break the lossless promise. Print it as itself; only genuinely
    // out-of-range values wrap.
    if nanos == day as i128 {
        out.push_str("24:00:00");
        return;
    }
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

/// DuckDB stores BIGNUM as a three-byte header followed by
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference bytes push_json_string must reproduce: serde_json's
    /// escaping, then a pass that escapes U+2028 and U+2029.
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
    fn nonfinite_words_become_strings_outside_strings_only() {
        for (text, want) in [
            ("42", "42"),
            ("NaN", r#""NaN""#),
            ("-Infinity", r#""-Infinity""#),
            ("[NaN,1,Infinity]", r#"["NaN",1,"Infinity"]"#),
            (r#"{"a":Infinity,"b":-Infinity,"c":NaN}"#, r#"{"a":"Infinity","b":"-Infinity","c":"NaN"}"#),
            (r#"{"NaN":"Infinity","s":"x\"NaN","n":-1}"#, r#"{"NaN":"Infinity","s":"x\"NaN","n":-1}"#),
            (r#"["\\",NaN,"é Infinity"]"#, r#"["\\","NaN","é Infinity"]"#),
        ] {
            assert_eq!(quote_nonfinite(text), want, "for {text}");
            serde_json::from_str::<serde_json::Value>(&quote_nonfinite(text)).expect(text);
        }
    }

    #[test]
    fn int_pad_pads_the_digits_after_the_sign() {
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
            let sign = if v < 0 { "-" } else { "" };
            assert_eq!(out, format!("{sign}{:0w$}", v.unsigned_abs()), "for {v} width {w}");
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

    /// `civil_from_days` backs both DATE formatting and the log timestamp.
    /// Pinned to dates whose answers are known independently: the epoch,
    /// both sides of a leap day, the 1900/2000 century rules, and dates before
    /// the epoch, where the sign correction on the era division matters and a
    /// plain truncating divide is a day out.
    #[test]
    fn converts_days_to_civil_dates() {
        for (days, want) in [
            (0_i64, (1970_i64, 1_u32, 1_u32)),
            (59, (1970, 3, 1)),      // 1970 is not a leap year
            (-1, (1969, 12, 31)),    // before the epoch
            (-719_468, (0, 3, 1)),   // start of the era
            (11_016, (2000, 2, 29)), // 2000 is a leap year: the /400 rule
            (11_017, (2000, 3, 1)),
            (-25_508, (1900, 3, 1)), // 1900 is not: the /100 rule
            (20_677, (2026, 8, 12)),
            (2_932_896, (9999, 12, 31)),
        ] {
            assert_eq!(civil_from_days(days), want, "days={days}");
        }
    }

    /// A year past four digits, or before year 0, goes out in the expanded
    /// form a JavaScript `Date` reads: a sign and six digits.
    #[test]
    fn a_year_outside_four_digits_is_expanded() {
        for (days, want) in [
            (2_932_896, "9999-12-31"),
            (2_932_897, "+010000-01-01"),
            (-719_469, "0000-02-29"),
            (-719_834, "-000001-03-01"),
            (i32::MAX as i64 - 1, "+5881580-07-10"),
        ] {
            let mut out = String::new();
            push_date(&mut out, days as i32);
            assert_eq!(out, want, "days={days}");
        }
    }

    /// The byte strings here are what DuckDB v1.5.5 actually put on the wire
    /// for these values, captured from a running server rather than derived
    /// from the format description — a decoder tested only against its own
    /// author's reading of the spec proves nothing about the encoder.
    #[test]
    fn decodes_bignum_wire_format() {
        fn hex(s: &str) -> Vec<u8> {
            (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
        }
        for (bytes, want) in [
            ("80000100", "0"),
            ("80000101", "1"),
            ("7ffffefe", "-1"),
            ("8000017f", "127"),
            ("7ffffe80", "-127"),
            ("800001ff", "255"),
            ("7ffffe00", "-255"),
            ("80000d018ee90ff6c373e0ee4e3f0ad2", "123456789012345678901234567890"),
            ("7ffff2fe7116f0093c8c1f11b1c0f52d", "-123456789012345678901234567890"),
        ] {
            assert_eq!(varint_to_decimal(&hex(bytes)).as_deref(), Some(want), "for {bytes}");
        }
    }

    /// Malformed input must return None so the caller can fall back, rather
    /// than produce a confidently wrong number from garbage.
    #[test]
    fn rejects_malformed_bignum() {
        // Too short to hold a header at all.
        assert_eq!(varint_to_decimal(&[]), None);
        assert_eq!(varint_to_decimal(&[0x80, 0x00]), None);
        // Header claims four magnitude bytes; only one follows.
        assert_eq!(varint_to_decimal(&[0x80, 0x00, 0x04, 0x01]), None);
    }
}
