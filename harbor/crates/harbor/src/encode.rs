//! Wire encoders: DuckDB values and types → the NDJSON envelope's JSON.
//!
//! Pure and stateless — no server state, no statics, no locks. Everything a
//! result row or schema needs to become bytes on the wire lives here: value
//! and type emission, the JSON-safe integer rule, temporal formatting,
//! varint decimals, bit/uuid/base64, and the keyword table used to quote
//! identifiers in type strings. Split out of lib.rs so the server file is
//! about serving, and these encoders (pinned by tests against captured wire
//! bytes) read as one subsystem.

use duckdb::{
    ffi,
    core::{LogicalTypeHandle, LogicalTypeId},
    types::{TimeUnit, Value, ValueRef},
};

// duckdb-rs keeps the raw `duckdb_logical_type` private, and two details are
// reachable only through the C API: an ARRAY's length and an ENUM's value
// list. `LogicalTypeHandle` is a single-field newtype around that pointer, so
// a copy of its bytes is the pointer. The assertion turns a layout change in
// duckdb-rs into a compile error instead of a crash at runtime.
const _: () = assert!(
    std::mem::size_of::<LogicalTypeHandle>() == std::mem::size_of::<ffi::duckdb_logical_type>()
);

/// Borrow the handle's pointer. The handle keeps ownership; the result must
/// not outlive it and must not be destroyed.
pub(crate) fn raw_type(ty: &LogicalTypeHandle) -> ffi::duckdb_logical_type {
    unsafe { std::mem::transmute_copy(ty) }
}

pub(crate) fn array_size(ty: &LogicalTypeHandle) -> u64 {
    unsafe { ffi::duckdb_array_type_array_size(raw_type(ty)) }
}

pub(crate) fn enum_values(ty: &LogicalTypeHandle) -> Vec<String> {
    unsafe {
        let handle = raw_type(ty);
        let count = ffi::duckdb_enum_dictionary_size(handle) as usize;
        (0..count)
            .map(|i| {
                let ptr = ffi::duckdb_enum_dictionary_value(handle, i as u64);
                if ptr.is_null() {
                    return String::new();
                }
                let value = std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned();
                ffi::duckdb_free(ptr as *mut std::ffi::c_void);
                value
            })
            .collect()
    }
}

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

// Schema emission
// ---------------------------------------------------------------------------

/// Emit one column's schema. `nested` marks a type that sits inside a
/// container (a list element, a struct field, a map key or value, a union
/// member) rather than being a column of the result itself. It changes one
/// verdict — see the `Union` arm — and nothing else.
pub(crate) fn emit_column_schema(out: &mut String, name: Option<&str>, ty: &LogicalTypeHandle) {
    emit_schema(out, name, ty, false)
}

/// A container's element schema: same shape, but flagged as nested.
fn emit_child_schema(out: &mut String, name: Option<&str>, ty: &LogicalTypeHandle) {
    emit_schema(out, name, ty, true)
}

fn emit_schema(out: &mut String, name: Option<&str>, ty: &LogicalTypeHandle, nested: bool) {
    out.push('{');
    if let Some(n) = name.filter(|n| !n.is_empty()) {
        out.push_str(r#""name":"#);
        push_json_string(out, n);
        out.push(',');
    }
    out.push_str(r#""duckdbType":"#);
    push_json_string(out, &type_name(ty));

    let id = ty.try_id().unwrap_or(LogicalTypeId::Unsupported);
    match id {
        LogicalTypeId::Decimal => {
            out.push_str(r#","lossless":true,"decimal":{"width":"#);
            out.push_str(&ty.decimal_width().to_string());
            out.push_str(r#","scale":"#);
            out.push_str(&ty.decimal_scale().to_string());
            out.push('}');
        }
        LogicalTypeId::List => {
            out.push_str(r#","lossless":true,"child":"#);
            emit_child_schema(out, None, &ty.child(0));
        }
        LogicalTypeId::Array => {
            out.push_str(r#","lossless":true,"arrayLength":"#);
            out.push_str(&array_size(ty).to_string());
            out.push_str(r#","child":"#);
            emit_child_schema(out, None, &ty.child(0));
        }
        LogicalTypeId::Struct => {
            out.push_str(r#","lossless":true,"fields":["#);
            for i in 0..ty.num_children() {
                if i > 0 {
                    out.push(',');
                }
                emit_child_schema(out, Some(&ty.child_name(i)), &ty.child(i));
            }
            out.push(']');
        }
        LogicalTypeId::Map => {
            // A SQL MAP has no JSON counterpart — its keys need not be strings
            // — so values go out as pairs and the encoding says so.
            out.push_str(r#","lossless":true,"keyType":"#);
            emit_child_schema(out, None, &ty.child(0));
            out.push_str(r#","valueType":"#);
            emit_child_schema(out, None, &ty.child(1));
            out.push_str(r#","encoding":"pairs""#);
        }
        LogicalTypeId::Union => {
            // A UNION's value is only meaningful with its tag: `union_value(a
            // := 2)` and `union_value(b := 2)` are different values with the
            // same payload. At the top of a column the tag is recoverable —
            // `union_tag` reads it off the arrow array through the row's
            // ValueRef — and the value goes out as {"tag":…,"value":…}.
            //
            // Nested, it is not. Inside a container harbor holds a decoded
            // `Value::Union(Box<Value>)`, which carries the payload and
            // nothing else, so the tag is already gone by the time this
            // encoder sees it and the member type cannot be chosen either.
            // Both `[union_value(a := 2)]` and `[union_value(b := 2)]`
            // therefore emit `[2]`.
            //
            // Saying so is the point. This used to claim lossless:true beside
            // a members list, which told a client the payload carried
            // something it does not. Same contract as TimeTZ below: name the
            // loss rather than let a client discover it.
            match nested {
                true => out.push_str(r#","lossless":false,"encoding":"union-tag-dropped","members":["#),
                false => out.push_str(r#","lossless":true,"members":["#),
            }
            for i in 0..ty.num_children() {
                if i > 0 {
                    out.push(',');
                }
                emit_child_schema(out, Some(&ty.child_name(i)), &ty.child(i));
            }
            out.push(']');
        }
        LogicalTypeId::Enum => {
            out.push_str(r#","lossless":true,"values":["#);
            for (i, value) in enum_values(ty).iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_json_string(out, value);
            }
            out.push(']');
        }
        // TIME WITH TIME ZONE is the one type harbor cannot carry
        // losslessly: duckdb-rs decodes it to a local time and drops the UTC
        // offset before harbor ever sees the value, so the offset cannot be
        // recovered. Saying so is better than emitting a time that silently
        // means something else.
        LogicalTypeId::TimeTZ => out.push_str(r#","lossless":false,"encoding":"time-offset-dropped""#),
        _ if is_lossless(id) => out.push_str(r#","lossless":true"#),
        // User-defined and extension types round-trip as text. Saying so
        // explicitly is better than silently handing back a string that
        // looks like a native value.
        _ => out.push_str(r#","lossless":false,"encoding":"varchar-cast""#),
    }
    out.push('}');
}

pub(crate) fn is_lossless(id: LogicalTypeId) -> bool {
    use LogicalTypeId::*;
    matches!(
        id,
        Boolean
            | Tinyint
            | Smallint
            | Integer
            | Bigint
            | Hugeint
            | UHugeint
            | UTinyint
            | USmallint
            | UInteger
            | UBigint
            | Float
            | Double
            | Varchar
            | Uuid
            | Date
            | Time
            | Timestamp
            | TimestampS
            | TimestampMs
            | TimestampNs
            | TimestampTZ
            | Interval
            | Blob
            | Bit
            // Lossless because it goes out as its decimal digits — a string
            // when it exceeds what a double holds, so no precision is lost on
            // the way through a JSON parser.
            | Bignum
            | Enum
            | SqlNull
    )
}

pub(crate) fn type_name(ty: &LogicalTypeHandle) -> String {
    use LogicalTypeId::*;
    // An alias is the user's own name for the type (JSON, for one); it is
    // more informative than the storage type underneath it.
    if let Some(alias) = ty.get_alias()
        && !alias.is_empty()
    {
        return alias;
    }
    match ty.try_id().unwrap_or(Unsupported) {
        Boolean => "BOOLEAN".into(),
        Tinyint => "TINYINT".into(),
        Smallint => "SMALLINT".into(),
        Integer => "INTEGER".into(),
        Bigint => "BIGINT".into(),
        Hugeint => "HUGEINT".into(),
        UHugeint => "UHUGEINT".into(),
        UTinyint => "UTINYINT".into(),
        USmallint => "USMALLINT".into(),
        UInteger => "UINTEGER".into(),
        UBigint => "UBIGINT".into(),
        Float => "FLOAT".into(),
        Double => "DOUBLE".into(),
        Varchar | StringLiteral => "VARCHAR".into(),
        Blob => "BLOB".into(),
        Bit => "BIT".into(),
        Uuid => "UUID".into(),
        Date => "DATE".into(),
        Time => "TIME".into(),
        TimeTZ => "TIME WITH TIME ZONE".into(),
        TimeNs => "TIME_NS".into(),
        Timestamp => "TIMESTAMP".into(),
        TimestampS => "TIMESTAMP_S".into(),
        TimestampMs => "TIMESTAMP_MS".into(),
        TimestampNs => "TIMESTAMP_NS".into(),
        TimestampTZ => "TIMESTAMP WITH TIME ZONE".into(),
        Interval => "INTERVAL".into(),
        Decimal => format!("DECIMAL({},{})", ty.decimal_width(), ty.decimal_scale()),
        List => format!("{}[]", type_name(&ty.child(0))),
        Array => format!("{}[{}]", type_name(&ty.child(0)), array_size(ty)),
        Enum => {
            let values: Vec<String> =
                enum_values(ty).iter().map(|v| format!("'{}'", v.replace('\'', "''"))).collect();
            format!("ENUM({})", values.join(", "))
        }
        Struct => {
            let fields: Vec<String> = (0..ty.num_children())
                .map(|i| format!("{} {}", quote_identifier(&ty.child_name(i)), type_name(&ty.child(i))))
                .collect();
            format!("STRUCT({})", fields.join(", "))
        }
        Map => format!("MAP({}, {})", type_name(&ty.child(0)), type_name(&ty.child(1))),
        Union => {
            let members: Vec<String> = (0..ty.num_children())
                .map(|i| format!("{} {}", quote_identifier(&ty.child_name(i)), type_name(&ty.child(i))))
                .collect();
            format!("UNION({})", members.join(", "))
        }
        SqlNull => "\"NULL\"".into(),
        Geometry => "GEOMETRY".into(),
        Variant => "VARIANT".into(),
        Bignum => "BIGNUM".into(),
        _ => "UNKNOWN".into(),
    }
}

// ---------------------------------------------------------------------------
// Value emission
//
// Dispatch is on the decoded value rather than the column type, because the
// value already carries what it needs — DECIMAL brings its width and scale,
// TIMESTAMP brings its unit. The column type is consulted only where the
// value is genuinely ambiguous: UUID and TIMESTAMP WITH TIME ZONE share a
// representation with plain integers and naive timestamps.
// ---------------------------------------------------------------------------

/// IEEE-754 doubles hold integers exactly only up to 2^53 - 1. Anything
/// wider goes out quoted; a JavaScript client parsing a bare
/// 9007199254740993 gets 9007199254740992 and never finds out.
const JSON_SAFE: i128 = 9_007_199_254_740_991;

/// The name of the member a UNION value actually holds, if this is one.
pub(crate) fn union_tag(v: &ValueRef<'_>) -> Option<String> {
    use duckdb::arrow::{array::{Array, UnionArray}, datatypes::DataType};
    let ValueRef::Union(column, idx) = v else {
        return None;
    };
    let union = column.as_any().downcast_ref::<UnionArray>()?;
    let DataType::Union(fields, _) = column.data_type() else {
        return None;
    };
    let type_id = union.type_id(*idx);
    fields.iter().find(|(id, _)| *id == type_id).map(|(_, field)| field.name().clone())
}

/// A UNION goes out as {"tag": member, "value": ...}; everything else is just
/// its value.
pub(crate) fn emit_tagged(out: &mut String, tag: Option<String>, v: &Value, ty: Option<&LogicalTypeHandle>) {
    match (tag, v) {
        (Some(name), Value::Union(inner)) => {
            let member = ty.and_then(|t| {
                (0..t.num_children()).find(|i| t.child_name(*i) == name).map(|i| t.child(i))
            });
            out.push_str(r#"{"tag":"#);
            push_json_string(out, &name);
            out.push_str(r#","value":"#);
            emit_value(out, inner, member.as_ref());
            out.push('}');
        }
        (_, value) => emit_value(out, value, ty),
    }
}

pub(crate) fn emit_value(out: &mut String, v: &Value, ty: Option<&LogicalTypeHandle>) {
    let id = ty.and_then(|t| t.try_id().ok());
    match v {
        Value::Null => out.push_str("null"),
        Value::Boolean(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::TinyInt(i) => out.push_str(&i.to_string()),
        Value::SmallInt(i) => out.push_str(&i.to_string()),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::UTinyInt(i) => out.push_str(&i.to_string()),
        Value::USmallInt(i) => out.push_str(&i.to_string()),
        Value::UInt(i) => out.push_str(&i.to_string()),
        Value::BigInt(i) => push_int(out, *i as i128),
        Value::UBigInt(i) => push_int(out, *i as i128),
        Value::HugeInt(i) => {
            if id == Some(LogicalTypeId::Uuid) {
                push_json_string(out, &uuid_to_string(*i));
            } else {
                push_int(out, *i)
            }
        }
        Value::UHugeInt(i) => {
            if *i <= JSON_SAFE as u128 {
                out.push_str(&i.to_string());
            } else {
                push_json_string(out, &i.to_string());
            }
        }
        Value::Float(f) => push_float32(out, *f),
        Value::Double(f) => push_float(out, *f),
        Value::Decimal(d) => push_json_string(out, &d.to_string()),
        Value::Text(s) | Value::Enum(s) => push_json_string(out, s),
        Value::Blob(b) if id == Some(LogicalTypeId::Bit) => push_json_string(out, &bit_string(b)),
        // Same JSON-safe rule as every other integer: bare when a double holds
        // it exactly, quoted past that. A BIGNUM is arbitrary precision, so it
        // is usually quoted — but a small one should not look different from
        // the same value in a BIGINT column.
        Value::Blob(b) if id == Some(LogicalTypeId::Bignum) => match varint_to_decimal(b) {
            Some(digits) => match digits.parse::<i128>() {
                Ok(v) => push_int(out, v),
                Err(_) => push_json_string(out, &digits),
            },
            None => push_json_string(out, &base64(b)),
        },
        Value::Blob(b) | Value::Geometry(b) => push_json_string(out, &base64(b)),
        Value::Date32(d) => push_json_string(out, &fmt_date(*d)),
        Value::Time64(unit, v) => push_json_string(out, &fmt_time(to_nanos(*unit, *v))),
        Value::Timestamp(unit, v) => {
            let mut s = fmt_timestamp(to_nanos(*unit, *v), *unit);
            if id == Some(LogicalTypeId::TimestampTZ) {
                s.push('Z');
            }
            push_json_string(out, &s);
        }
        Value::Interval { months, days, nanos } => {
            // micros as a string: it is an int64 and JSON numbers are not.
            out.push_str(&format!(
                r#"{{"months":{},"days":{},"micros":"{}"}}"#,
                months,
                days,
                nanos / 1_000
            ));
        }
        Value::List(items) | Value::Array(items) => {
            let child = ty.map(|t| t.child(0));
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                emit_value(out, item, child.as_ref());
            }
            out.push(']');
        }
        Value::Struct(fields) => {
            out.push('{');
            for (i, (k, val)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_json_string(out, k);
                out.push(':');
                emit_value(out, val, ty.map(|t| t.child(i)).as_ref());
            }
            out.push('}');
        }
        Value::Map(entries) => {
            // A SQL MAP has no JSON counterpart: its keys need not be
            // strings. Pairs keep it lossless; the schema line says so with
            // "encoding":"pairs".
            //
            // The key and value types travel with the pair, exactly as LIST
            // passes child(0) and STRUCT passes child(i). They used to be
            // dropped, and the values that need a type to be written correctly
            // were then written wrongly *while the schema line above described
            // them accurately*: a BIT went out as base64 of DuckDB's private
            // storage ("Bf0=" for "101"), a BIGNUM likewise, and a TIMESTAMPTZ
            // lost its Z and read as a naive local time — all of them still
            // labelled "lossless":true.
            let (key_ty, value_ty) = match ty {
                Some(t) => (Some(t.child(0)), Some(t.child(1))),
                None => (None, None),
            };
            out.push('[');
            for (i, (k, val)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('[');
                emit_value(out, k, key_ty.as_ref());
                out.push(',');
                emit_value(out, val, value_ty.as_ref());
                out.push(']');
            }
            out.push(']');
        }
        Value::Union(inner) => emit_value(out, inner, None),
        // `Value` is #[non_exhaustive]: a later DuckDB can add a variant this
        // build has never seen. The schema line already flags such a column
        // lossless:false, so a client knows not to trust the payload.
        _ => out.push_str("null"),
    }
}

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

pub(crate) fn to_nanos(unit: TimeUnit, v: i64) -> i128 {
    let v = v as i128;
    match unit {
        TimeUnit::Second => v * 1_000_000_000,
        TimeUnit::Millisecond => v * 1_000_000,
        TimeUnit::Microsecond => v * 1_000,
        TimeUnit::Nanosecond => v,
    }
}

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

pub(crate) fn fmt_timestamp(nanos: i128, unit: TimeUnit) -> String {
    let day = 86_400_000_000_000i128;
    let days = nanos.div_euclid(day);
    let rest = nanos.rem_euclid(day);
    let (y, m, d) = civil_from_days(days as i64);
    let (h, min, s, frac) = split_time(rest);
    let mut out = format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}");
    // TIMESTAMP_S has no fractional part by definition; emitting one would
    // invent precision the column does not have.
    if unit != TimeUnit::Second {
        push_fraction(&mut out, frac);
    }
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
