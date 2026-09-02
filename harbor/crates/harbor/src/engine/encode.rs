//! v2 chunk encoder: DuckDB vector views → the NDJSON envelope's JSON.
//!
//! The same wire bytes as src/encode.rs, produced without duckdb-rs or
//! arrow. Types are read once per result into an owned `Type` tree
//! (logical_type introspection), then every chunk is walked through borrowed
//! vector views: no per-row value materialization, no arrow arrays, no
//! decoder panics. The pure formatters — dates, decimals, BIGNUM, BIT, UUID,
//! base64, the JSON-safe integer rule — live in src/encode.rs, which the
//! flip kept as the engine-free formatter home; this module owns everything
//! that touches a vector view.

use super::ffi;
use super::{Error, str_view};
use crate::encode::{
    civil_from_days, digit_pair, push_base64, push_bit_string, push_date, push_float,
    push_float32, push_fraction, push_i64_raw, push_int, push_int_pad, push_json_string,
    push_time, push_tz_offset, push_u128_raw, push_u64_raw, push_uuid, quote_identifier,
    split_time,
    varint_to_decimal,
};

// ---------------------------------------------------------------------------
// The type tree: everything the encoders consult, read once per result.
// ---------------------------------------------------------------------------

/// An owned description of one column type, deep. Built from logical_type
/// introspection so no handle outlives the result that produced it.
pub struct Type {
    pub id: ffi::LOGICAL_TYPE_ID,
    /// The engine's name for the type when it differs from the canonical
    /// name of the id — an extension or user-defined alias such as JSON.
    pub alias: Option<String>,
    /// DECIMAL only: (width, scale).
    pub decimal: (u8, u8),
    /// ARRAY only: the fixed element count.
    pub array_len: u64,
    /// ENUM only: the dictionary, in index order.
    pub enum_values: Vec<String>,
    /// LIST/ARRAY: one unnamed child. MAP: key, value. STRUCT/TUPLE: fields.
    /// UNION: members. Empty elsewhere.
    pub children: Vec<(String, Type)>,
}

/// The canonical names of the type ids this encoder knows, exactly as
/// logical_type_get_name spells them. A name outside this set is an alias.
fn is_canonical_name(name: &str) -> bool {
    matches!(
        name,
        "BOOLEAN" | "TINYINT" | "SMALLINT" | "INTEGER" | "BIGINT" | "HUGEINT" | "UHUGEINT"
            | "UTINYINT" | "USMALLINT" | "UINTEGER" | "UBIGINT" | "FLOAT" | "DOUBLE" | "VARCHAR"
            | "BLOB" | "BIT" | "UUID" | "DATE" | "TIME" | "TIME WITH TIME ZONE" | "TIME_NS"
            | "TIMESTAMP" | "TIMESTAMP_S" | "TIMESTAMP_MS" | "TIMESTAMP_NS"
            | "TIMESTAMP WITH TIME ZONE" | "TIMESTAMPTZ_NS" | "INTERVAL"
            | "DECIMAL" | "LIST" | "ARRAY" | "MAP" | "STRUCT" | "TUPLE" | "UNION" | "ENUM"
            | "NULL" | "\"NULL\"" | "SQLNULL" | "GEOMETRY" | "VARIANT" | "BIGNUM" | "ANY"
            | "INVALID" | "UNKNOWN" | "ROW"
    )
}

impl Type {
    /// Read a borrowed logical_type handle into an owned tree.
    pub fn of(api: &ffi::Api, lt: ffi::logical_type_handle) -> Result<Type, Error> {
        let mut id: ffi::LOGICAL_TYPE_ID = 0;
        call!(api, logical_type_get_id(lt, &mut id));

        let mut name_view = ffi::identifier_t { ptr: std::ptr::null(), len: 0 };
        call!(api, logical_type_get_name(lt, &mut name_view));
        let name = unsafe { str_view(&name_view) }.to_owned();
        let alias = (!is_canonical_name(&name)).then_some(name);

        let mut ty = Type { id, alias, decimal: (0, 0), array_len: 0, enum_values: Vec::new(), children: Vec::new() };

        let mut count: ffi::idx_t = 0;
        call!(api, logical_type_get_param_count(lt, &mut count));

        use ffi::*;
        match id {
            LOGICAL_TYPE_ID_DECIMAL => {
                ty.decimal = (param_u8(api, lt, 0)?, param_u8(api, lt, 1)?);
            }
            LOGICAL_TYPE_ID_LIST => {
                ty.children.push((String::new(), param_type(api, lt, 0)?.1));
            }
            LOGICAL_TYPE_ID_ARRAY => {
                ty.children.push((String::new(), param_type(api, lt, 0)?.1));
                ty.array_len = param_u64(api, lt, 1)?;
            }
            LOGICAL_TYPE_ID_MAP => {
                ty.children.push((String::new(), param_type(api, lt, 0)?.1));
                ty.children.push((String::new(), param_type(api, lt, 1)?.1));
            }
            LOGICAL_TYPE_ID_STRUCT | LOGICAL_TYPE_ID_TUPLE | LOGICAL_TYPE_ID_UNION => {
                for i in 0..count {
                    ty.children.push(param_type(api, lt, i)?);
                }
            }
            LOGICAL_TYPE_ID_ENUM => {
                for i in 0..count {
                    ty.enum_values.push(param_string(api, lt, i)?);
                }
            }
            _ => {}
        }
        Ok(ty)
    }
}

/// One (name, value) parameter where the value is a child TYPE.
fn param_type(api: &ffi::Api, lt: ffi::logical_type_handle, i: ffi::idx_t) -> Result<(String, Type), Error> {
    let (name, mut value) = param(api, lt, i)?;
    let mut child: ffi::logical_type_handle = std::ptr::null_mut();
    let unwrapped = (|| -> Result<Type, Error> {
        call!(api, value_get_type(value, &mut child));
        let ty = Type::of(api, child);
        if let Some(d) = api.logical_type_destroy {
            unsafe { d(&mut child) };
        }
        ty
    })();
    destroy_value(api, &mut value);
    Ok((name, unwrapped?))
}

fn param_u8(api: &ffi::Api, lt: ffi::logical_type_handle, i: ffi::idx_t) -> Result<u8, Error> {
    let (_, mut value) = param(api, lt, i)?;
    let mut out: u8 = 0;
    let r = (|| -> Result<(), Error> { call!(api, value_get_utinyint(value, &mut out)); Ok(()) })();
    destroy_value(api, &mut value);
    r.map(|_| out)
}

fn param_u64(api: &ffi::Api, lt: ffi::logical_type_handle, i: ffi::idx_t) -> Result<u64, Error> {
    let (_, mut value) = param(api, lt, i)?;
    let mut out: i64 = 0;
    let r = (|| -> Result<(), Error> { call!(api, value_get_bigint(value, &mut out)); Ok(()) })();
    destroy_value(api, &mut value);
    r.map(|_| out as u64)
}

fn param_string(api: &ffi::Api, lt: ffi::logical_type_handle, i: ffi::idx_t) -> Result<String, Error> {
    let (_, mut value) = param(api, lt, i)?;
    let mut out = ffi::str_t { ptr: std::ptr::null(), len: 0 };
    let r = (|| -> Result<(), Error> { call!(api, value_get_varchar(value, &mut out)); Ok(()) })();
    let s = r.map(|_| unsafe { str_view(&out) }.to_owned());
    destroy_value(api, &mut value);
    s
}

fn param(api: &ffi::Api, lt: ffi::logical_type_handle, i: ffi::idx_t) -> Result<(String, ffi::value_handle), Error> {
    let mut name = ffi::identifier_t { ptr: std::ptr::null(), len: 0 };
    let mut value: ffi::value_handle = std::ptr::null_mut();
    call!(api, logical_type_get_param(lt, i, &mut name, &mut value));
    Ok((unsafe { str_view(&name) }.to_owned(), value))
}

fn destroy_value(api: &ffi::Api, value: &mut ffi::value_handle) {
    if let Some(d) = api.value_destroy {
        unsafe { d(value) };
    }
}

/// The columns of a result: names and owned type trees.
pub fn result_columns(api: &ffi::Api, result: ffi::result_handle) -> Result<Vec<(String, Type)>, Error> {
    let mut schema: ffi::schema_handle = std::ptr::null_mut();
    call!(api, result_get_schema(result, &mut schema));
    let columns = (|| -> Result<Vec<(String, Type)>, Error> {
        let mut count: ffi::idx_t = 0;
        call!(api, schema_get_count(schema, &mut count));
        let mut columns = Vec::with_capacity(count as usize);
        for i in 0..count {
            let mut name = ffi::identifier_t { ptr: std::ptr::null(), len: 0 };
            let mut lt: ffi::logical_type_handle = std::ptr::null_mut();
            call!(api, schema_get_field(schema, i, &mut name, &mut lt));
            columns.push((unsafe { str_view(&name) }.to_owned(), Type::of(api, lt)?));
        }
        Ok(columns)
    })();
    if let Some(d) = api.schema_destroy {
        let mut s = schema;
        unsafe { d(&mut s) };
    }
    columns
}

// ---------------------------------------------------------------------------
// Schema emission — byte-identical to src/encode.rs emit_column_schema.
// ---------------------------------------------------------------------------

pub fn emit_column_schema(out: &mut String, name: Option<&str>, ty: &Type) {
    emit_schema(out, name, ty)
}

fn emit_schema(out: &mut String, name: Option<&str>, ty: &Type) {
    use ffi::*;
    out.push('{');
    if let Some(n) = name.filter(|n| !n.is_empty()) {
        out.push_str(r#""name":"#);
        push_json_string(out, n);
        out.push(',');
    }
    out.push_str(r#""duckdbType":"#);
    push_json_string(out, &type_name(ty));

    match ty.id {
        LOGICAL_TYPE_ID_DECIMAL => {
            out.push_str(r#","lossless":true,"decimal":{"width":"#);
            out.push_str(&ty.decimal.0.to_string());
            out.push_str(r#","scale":"#);
            out.push_str(&ty.decimal.1.to_string());
            out.push('}');
        }
        LOGICAL_TYPE_ID_LIST => {
            out.push_str(r#","lossless":true,"child":"#);
            emit_schema(out, None, &ty.children[0].1);
        }
        LOGICAL_TYPE_ID_ARRAY => {
            out.push_str(r#","lossless":true,"arrayLength":"#);
            out.push_str(&ty.array_len.to_string());
            out.push_str(r#","child":"#);
            emit_schema(out, None, &ty.children[0].1);
        }
        LOGICAL_TYPE_ID_STRUCT | LOGICAL_TYPE_ID_TUPLE => {
            out.push_str(r#","lossless":true,"fields":["#);
            for (i, (n, child)) in ty.children.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                emit_schema(out, Some(n), child);
            }
            out.push(']');
        }
        LOGICAL_TYPE_ID_MAP => {
            out.push_str(r#","lossless":true,"keyType":"#);
            emit_schema(out, None, &ty.children[0].1);
            out.push_str(r#","valueType":"#);
            emit_schema(out, None, &ty.children[1].1);
            out.push_str(r#","encoding":"pairs""#);
        }
        LOGICAL_TYPE_ID_UNION => {
            // The v2 vector interface keeps the tag reachable inside
            // containers too, so since 0.22 nothing is dropped anywhere —
            // v1 could only recover the tag at the top of a column.
            out.push_str(r#","lossless":true,"members":["#);
            for (i, (n, child)) in ty.children.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                emit_schema(out, Some(n), child);
            }
            out.push(']');
        }
        LOGICAL_TYPE_ID_ENUM => {
            out.push_str(r#","lossless":true,"values":["#);
            for (i, value) in ty.enum_values.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_json_string(out, value);
            }
            out.push(']');
        }
        id if is_lossless(id) => out.push_str(r#","lossless":true"#),
        _ => out.push_str(r#","lossless":false,"encoding":"varchar-cast""#),
    }
    out.push('}');
}

fn is_lossless(id: ffi::LOGICAL_TYPE_ID) -> bool {
    use ffi::*;
    matches!(
        id,
        LOGICAL_TYPE_ID_BOOLEAN
            | LOGICAL_TYPE_ID_TINYINT
            | LOGICAL_TYPE_ID_SMALLINT
            | LOGICAL_TYPE_ID_INTEGER
            | LOGICAL_TYPE_ID_BIGINT
            | LOGICAL_TYPE_ID_HUGEINT
            | LOGICAL_TYPE_ID_UHUGEINT
            | LOGICAL_TYPE_ID_UTINYINT
            | LOGICAL_TYPE_ID_USMALLINT
            | LOGICAL_TYPE_ID_UINTEGER
            | LOGICAL_TYPE_ID_UBIGINT
            | LOGICAL_TYPE_ID_FLOAT
            | LOGICAL_TYPE_ID_DOUBLE
            | LOGICAL_TYPE_ID_VARCHAR
            | LOGICAL_TYPE_ID_UUID
            | LOGICAL_TYPE_ID_DATE
            | LOGICAL_TYPE_ID_TIME
            | LOGICAL_TYPE_ID_TIME_TZ
            | LOGICAL_TYPE_ID_TIMESTAMP
            | LOGICAL_TYPE_ID_TIMESTAMP_SEC
            | LOGICAL_TYPE_ID_TIMESTAMP_MS
            | LOGICAL_TYPE_ID_TIMESTAMP_NS
            | LOGICAL_TYPE_ID_TIMESTAMP_TZ
            | LOGICAL_TYPE_ID_INTERVAL
            | LOGICAL_TYPE_ID_BLOB
            | LOGICAL_TYPE_ID_BIT
            | LOGICAL_TYPE_ID_BIGNUM
            | LOGICAL_TYPE_ID_ENUM
            | LOGICAL_TYPE_ID_SQLNULL
            // New under v2: v1 refused TIME_NS (its client had no decoder)
            // and predates TIMESTAMP_NS WITH TIME ZONE. Both encode exactly
            // here — nanoseconds carry into a nine-digit fraction.
            | LOGICAL_TYPE_ID_TIME_NS
            | LOGICAL_TYPE_ID_TIMESTAMP_TZ_NS
    )
}

pub fn type_name(ty: &Type) -> String {
    use ffi::*;
    if let Some(alias) = &ty.alias {
        return alias.clone();
    }
    match ty.id {
        LOGICAL_TYPE_ID_BOOLEAN => "BOOLEAN".into(),
        LOGICAL_TYPE_ID_TINYINT => "TINYINT".into(),
        LOGICAL_TYPE_ID_SMALLINT => "SMALLINT".into(),
        LOGICAL_TYPE_ID_INTEGER => "INTEGER".into(),
        LOGICAL_TYPE_ID_BIGINT => "BIGINT".into(),
        LOGICAL_TYPE_ID_HUGEINT => "HUGEINT".into(),
        LOGICAL_TYPE_ID_UHUGEINT => "UHUGEINT".into(),
        LOGICAL_TYPE_ID_UTINYINT => "UTINYINT".into(),
        LOGICAL_TYPE_ID_USMALLINT => "USMALLINT".into(),
        LOGICAL_TYPE_ID_UINTEGER => "UINTEGER".into(),
        LOGICAL_TYPE_ID_UBIGINT => "UBIGINT".into(),
        LOGICAL_TYPE_ID_FLOAT => "FLOAT".into(),
        LOGICAL_TYPE_ID_DOUBLE => "DOUBLE".into(),
        LOGICAL_TYPE_ID_VARCHAR => "VARCHAR".into(),
        LOGICAL_TYPE_ID_BLOB => "BLOB".into(),
        LOGICAL_TYPE_ID_BIT => "BIT".into(),
        LOGICAL_TYPE_ID_UUID => "UUID".into(),
        LOGICAL_TYPE_ID_DATE => "DATE".into(),
        LOGICAL_TYPE_ID_TIME => "TIME".into(),
        LOGICAL_TYPE_ID_TIME_TZ => "TIME WITH TIME ZONE".into(),
        LOGICAL_TYPE_ID_TIME_NS => "TIME_NS".into(),
        LOGICAL_TYPE_ID_TIMESTAMP => "TIMESTAMP".into(),
        LOGICAL_TYPE_ID_TIMESTAMP_SEC => "TIMESTAMP_S".into(),
        LOGICAL_TYPE_ID_TIMESTAMP_MS => "TIMESTAMP_MS".into(),
        LOGICAL_TYPE_ID_TIMESTAMP_NS => "TIMESTAMP_NS".into(),
        LOGICAL_TYPE_ID_TIMESTAMP_TZ => "TIMESTAMP WITH TIME ZONE".into(),
        LOGICAL_TYPE_ID_TIMESTAMP_TZ_NS => "TIMESTAMPTZ_NS".into(),
        LOGICAL_TYPE_ID_INTERVAL => "INTERVAL".into(),
        LOGICAL_TYPE_ID_DECIMAL => format!("DECIMAL({},{})", ty.decimal.0, ty.decimal.1),
        LOGICAL_TYPE_ID_LIST => format!("{}[]", type_name(&ty.children[0].1)),
        LOGICAL_TYPE_ID_ARRAY => format!("{}[{}]", type_name(&ty.children[0].1), ty.array_len),
        LOGICAL_TYPE_ID_ENUM => {
            let values: Vec<String> =
                ty.enum_values.iter().map(|v| format!("'{}'", v.replace('\'', "''"))).collect();
            format!("ENUM({})", values.join(", "))
        }
        LOGICAL_TYPE_ID_STRUCT => {
            let fields: Vec<String> = ty
                .children
                .iter()
                .map(|(n, c)| format!("{} {}", quote_identifier(n), type_name(c)))
                .collect();
            format!("STRUCT({})", fields.join(", "))
        }
        LOGICAL_TYPE_ID_TUPLE => {
            let members: Vec<String> = ty.children.iter().map(|(_, c)| type_name(c)).collect();
            format!("TUPLE({})", members.join(", "))
        }
        LOGICAL_TYPE_ID_MAP => {
            format!("MAP({}, {})", type_name(&ty.children[0].1), type_name(&ty.children[1].1))
        }
        LOGICAL_TYPE_ID_UNION => {
            let members: Vec<String> = ty
                .children
                .iter()
                .map(|(n, c)| format!("{} {}", quote_identifier(n), type_name(c)))
                .collect();
            format!("UNION({})", members.join(", "))
        }
        LOGICAL_TYPE_ID_SQLNULL => "\"NULL\"".into(),
        LOGICAL_TYPE_ID_GEOMETRY => "GEOMETRY".into(),
        LOGICAL_TYPE_ID_VARIANT => "VARIANT".into(),
        LOGICAL_TYPE_ID_BIGNUM => "BIGNUM".into(),
        _ => "UNKNOWN".into(),
    }
}

// ---------------------------------------------------------------------------
// The reader tree: borrowed views over one chunk, built once per chunk.
// ---------------------------------------------------------------------------

/// One vector's view plus its children, all borrowed from the owning chunk.
/// Valid until that chunk is destroyed.
pub struct Reader {
    view: ffi::vector_view_t,
    constant: bool,
    children: Vec<Reader>,
    /// Kept for the single-cell value bridge (VARIANT, GEOMETRY).
    vector: ffi::vector_handle,
}

impl Reader {
    /// Build the reader for one column vector of a chunk, recursively.
    pub fn of(api: &ffi::Api, vector: ffi::vector_handle) -> Result<Reader, Error> {
        let mut vtype: ffi::VECTOR_TYPE = 0;
        call!(api, vector_get_vector_type(vector, &mut vtype));
        // FSST / SEQUENCE / SHREDDED have no committed view; materialize.
        if vtype == ffi::VECTOR_TYPE_OTHER {
            call!(api, vector_flatten(vector));
            vtype = ffi::VECTOR_TYPE_FLAT;
        }
        let mut view = ffi::vector_view_t {
            data: std::ptr::null(),
            validity: std::ptr::null(),
            sel: std::ptr::null(),
            count: 0,
        };
        call!(api, vector_get_view(vector, &mut view));
        // Children AFTER the parent's view: reading a DICTIONARY view may
        // flatten its underlying child in place, and any earlier borrow into
        // that child would be dangling.
        let mut n: ffi::idx_t = 0;
        call!(api, vector_get_child_count(vector, &mut n));
        let mut children = Vec::with_capacity(n as usize);
        for i in 0..n {
            let mut child: ffi::vector_handle = std::ptr::null_mut();
            call!(api, vector_get_child(vector, i, &mut child));
            children.push(Reader::of(api, child)?);
        }
        Ok(Reader { view, constant: vtype == ffi::VECTOR_TYPE_CONSTANT, children, vector })
    }

    /// The physical index behind a logical row: through the selection vector
    /// when there is one, pinned to 0 for a constant.
    fn phys(&self, row: usize) -> usize {
        if self.constant {
            return 0;
        }
        if self.view.sel.is_null() {
            return row;
        }
        (unsafe { *self.view.sel.add(row) }) as usize
    }

    fn is_valid(&self, phys: usize) -> bool {
        if self.view.validity.is_null() {
            return true;
        }
        (unsafe { *self.view.validity.add(phys / 64) }) >> (phys % 64) & 1 == 1
    }

    /// Read a fixed-width cell at a physical index.
    unsafe fn get<T: Copy>(&self, phys: usize) -> T {
        unsafe { *(self.view.data as *const T).add(phys) }
    }
}

// ---------------------------------------------------------------------------
// Cell emission — byte-identical to src/encode.rs emit_tagged/emit_value.
// ---------------------------------------------------------------------------

/// Emit one column's cell at `row` of the chunk the reader was built from.
pub fn emit_cell(
    out: &mut String,
    api: &ffi::Api,
    r: &Reader,
    ty: &Type,
    row: usize,
) -> Result<(), Error> {
    emit(out, api, r, ty, row)
}

fn emit(
    out: &mut String,
    api: &ffi::Api,
    r: &Reader,
    ty: &Type,
    row: usize,
) -> Result<(), Error> {
    use ffi::*;

    // SQLNULL columns hold nothing but NULL and expose no storage.
    if ty.id == LOGICAL_TYPE_ID_SQLNULL {
        out.push_str("null");
        return Ok(());
    }

    let phys = r.phys(row);
    if !r.is_valid(phys) {
        out.push_str("null");
        return Ok(());
    }

    unsafe {
        match ty.id {
            LOGICAL_TYPE_ID_BOOLEAN => {
                out.push_str(if r.get::<u8>(phys) != 0 { "true" } else { "false" })
            }
            LOGICAL_TYPE_ID_TINYINT => push_i64_raw(out, r.get::<i8>(phys) as i64),
            LOGICAL_TYPE_ID_SMALLINT => push_i64_raw(out, r.get::<i16>(phys) as i64),
            LOGICAL_TYPE_ID_INTEGER => push_i64_raw(out, r.get::<i32>(phys) as i64),
            LOGICAL_TYPE_ID_UTINYINT => push_u64_raw(out, r.get::<u8>(phys) as u64),
            LOGICAL_TYPE_ID_USMALLINT => push_u64_raw(out, r.get::<u16>(phys) as u64),
            LOGICAL_TYPE_ID_UINTEGER => push_u64_raw(out, r.get::<u32>(phys) as u64),
            LOGICAL_TYPE_ID_BIGINT => push_int(out, r.get::<i64>(phys) as i128),
            LOGICAL_TYPE_ID_UBIGINT => push_int(out, r.get::<u64>(phys) as i128),
            LOGICAL_TYPE_ID_HUGEINT => push_int(out, hugeint(r.get(phys))),
            LOGICAL_TYPE_ID_UHUGEINT => {
                let h: ffi::uhugeint_t = r.get(phys);
                let v = (h.upper as u128) << 64 | h.lower as u128;
                // Same JSON-safe rule as push_int, on the unsigned side.
                if v <= 9_007_199_254_740_991u128 {
                    push_u128_raw(out, v);
                } else {
                    out.push('"');
                    push_u128_raw(out, v);
                    out.push('"');
                }
            }
            LOGICAL_TYPE_ID_FLOAT => push_float32(out, r.get::<f32>(phys)),
            LOGICAL_TYPE_ID_DOUBLE => push_float(out, r.get::<f64>(phys)),
            LOGICAL_TYPE_ID_DECIMAL => {
                let v: i128 = match ty.decimal.0 {
                    ..=4 => r.get::<i16>(phys) as i128,
                    ..=9 => r.get::<i32>(phys) as i128,
                    ..=18 => r.get::<i64>(phys) as i128,
                    _ => hugeint(r.get(phys)),
                };
                // Sign, digits, and a dot need no JSON escaping.
                out.push('"');
                push_decimal(out, v, ty.decimal.1);
                out.push('"');
            }
            LOGICAL_TYPE_ID_VARCHAR => {
                push_json_string(out, &String::from_utf8_lossy(bytes(r, phys)))
            }
            // BLOB/BIT/BIGNUM/UUID and the temporal types below all render
            // into JSON-safe alphabets (base64, digits, hex, 0/1, ISO
            // punctuation), so the quotes are the whole encoding — no escape
            // scan, no intermediate String.
            LOGICAL_TYPE_ID_BLOB => {
                out.push('"');
                push_base64(out, bytes(r, phys));
                out.push('"');
            }
            LOGICAL_TYPE_ID_BIT => {
                out.push('"');
                push_bit_string(out, bytes(r, phys));
                out.push('"');
            }
            LOGICAL_TYPE_ID_BIGNUM => {
                let b = bytes(r, phys);
                match varint_to_decimal(b) {
                    Some(digits) => match digits.parse::<i128>() {
                        Ok(v) => push_int(out, v),
                        Err(_) => {
                            out.push('"');
                            out.push_str(&digits);
                            out.push('"');
                        }
                    },
                    None => {
                        out.push('"');
                        push_base64(out, b);
                        out.push('"');
                    }
                }
            }
            LOGICAL_TYPE_ID_UUID => {
                out.push('"');
                push_uuid(out, hugeint(r.get(phys)));
                out.push('"');
            }
            LOGICAL_TYPE_ID_DATE => {
                out.push('"');
                push_date(out, r.get::<i32>(phys));
                out.push('"');
            }
            LOGICAL_TYPE_ID_TIME => {
                out.push('"');
                push_time(out, r.get::<i64>(phys) as i128 * 1_000);
                out.push('"');
            }
            LOGICAL_TYPE_ID_TIME_NS => {
                out.push('"');
                push_time(out, r.get::<i64>(phys) as i128);
                out.push('"');
            }
            // The stored value packs microseconds-since-midnight above a
            // 24-bit UTC offset in seconds, biased and reverse-ordered so
            // +14:00 sorts before UTC. Since 0.22 both survive to the wire:
            // the local clock, then the offset PostgreSQL-style.
            LOGICAL_TYPE_ID_TIME_TZ => {
                let packed: u64 = r.get(phys);
                out.push('"');
                push_time(out, (packed >> 24) as i128 * 1_000);
                const MAX_OFFSET: i32 = 16 * 60 * 60 - 1; // ±15:59:59
                push_tz_offset(out, MAX_OFFSET - (packed & 0xFF_FFFF) as i32);
                out.push('"');
            }
            LOGICAL_TYPE_ID_TIMESTAMP_SEC => {
                out.push('"');
                push_ts(out, r.get::<i64>(phys) as i128 * 1_000_000_000, true, false);
                out.push('"');
            }
            LOGICAL_TYPE_ID_TIMESTAMP_MS => {
                out.push('"');
                push_ts(out, r.get::<i64>(phys) as i128 * 1_000_000, false, false);
                out.push('"');
            }
            LOGICAL_TYPE_ID_TIMESTAMP => {
                out.push('"');
                push_ts(out, r.get::<i64>(phys) as i128 * 1_000, false, false);
                out.push('"');
            }
            LOGICAL_TYPE_ID_TIMESTAMP_NS => {
                out.push('"');
                push_ts(out, r.get::<i64>(phys) as i128, false, false);
                out.push('"');
            }
            LOGICAL_TYPE_ID_TIMESTAMP_TZ => {
                out.push('"');
                push_ts(out, r.get::<i64>(phys) as i128 * 1_000, false, true);
                out.push('"');
            }
            LOGICAL_TYPE_ID_TIMESTAMP_TZ_NS => {
                out.push('"');
                push_ts(out, r.get::<i64>(phys) as i128, false, true);
                out.push('"');
            }
            LOGICAL_TYPE_ID_INTERVAL => {
                let iv: ffi::interval_t = r.get(phys);
                out.push_str(r#"{"months":"#);
                push_i64_raw(out, iv.months as i64);
                out.push_str(r#","days":"#);
                push_i64_raw(out, iv.days as i64);
                out.push_str(r#","micros":""#);
                push_i64_raw(out, iv.micros);
                out.push_str("\"}");
            }
            LOGICAL_TYPE_ID_ENUM => {
                let idx = match ty.enum_values.len() {
                    ..=255 => r.get::<u8>(phys) as usize,
                    ..=65535 => r.get::<u16>(phys) as usize,
                    _ => r.get::<u32>(phys) as usize,
                };
                push_json_string(out, ty.enum_values.get(idx).map(|s| s.as_str()).unwrap_or(""));
            }
            LOGICAL_TYPE_ID_LIST => {
                let entry: ffi::list_entry_t = r.get(phys);
                out.push('[');
                for j in 0..entry.length {
                    if j > 0 {
                        out.push(',');
                    }
                    emit(out, api, &r.children[0], &ty.children[0].1, (entry.offset + j) as usize)?;
                }
                out.push(']');
            }
            LOGICAL_TYPE_ID_ARRAY => {
                out.push('[');
                for j in 0..ty.array_len {
                    if j > 0 {
                        out.push(',');
                    }
                    emit(out, api, &r.children[0], &ty.children[0].1, phys * ty.array_len as usize + j as usize)?;
                }
                out.push(']');
            }
            LOGICAL_TYPE_ID_STRUCT => {
                out.push('{');
                for (i, (name, child_ty)) in ty.children.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    push_json_string(out, name);
                    out.push(':');
                    emit(out, api, &r.children[i], child_ty, phys)?;
                }
                out.push('}');
            }
            LOGICAL_TYPE_ID_TUPLE => {
                // No field names — an object would collide on the empty key.
                // Order is a tuple's identity; a JSON array carries exactly it.
                out.push('[');
                for (i, (_, child_ty)) in ty.children.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    emit(out, api, &r.children[i], child_ty, phys)?;
                }
                out.push(']');
            }
            LOGICAL_TYPE_ID_MAP => {
                // Pairs, with the key and value types carried through — the
                // same lossless shape as v1, straight off the flattened key
                // and value children.
                let entry: ffi::list_entry_t = r.get(phys);
                out.push('[');
                for j in 0..entry.length {
                    if j > 0 {
                        out.push(',');
                    }
                    out.push('[');
                    emit(out, api, &r.children[0], &ty.children[0].1, (entry.offset + j) as usize)?;
                    out.push(',');
                    emit(out, api, &r.children[1], &ty.children[1].1, (entry.offset + j) as usize)?;
                    out.push(']');
                }
                out.push(']');
            }
            LOGICAL_TYPE_ID_UNION => {
                // children[0] is the tag, children[1..] the members. The
                // tagged object goes out at every depth since 0.22 — v1
                // could only tag at the top of a column.
                let tag = {
                    let t = &r.children[0];
                    let p = t.phys(phys);
                    t.get::<u8>(p) as usize
                };
                let Some((name, member_ty)) = ty.children.get(tag) else {
                    out.push_str("null");
                    return Ok(());
                };
                out.push_str(r#"{"tag":"#);
                push_json_string(out, name);
                out.push_str(r#","value":"#);
                emit(out, api, &r.children[1 + tag], member_ty, phys)?;
                out.push('}');
            }
            // No committed view layout — the single-cell value bridge is the
            // committed way in, and the payload goes out as the engine's text
            // rendering, exactly what the schema's "varchar-cast" promises.
            // (v1 emitted base64 of storage bytes under the same lossless:false
            // label — a payload nothing could decode; text is strictly better.)
            LOGICAL_TYPE_ID_GEOMETRY | LOGICAL_TYPE_ID_VARIANT => {
                let mut value: ffi::value_handle = std::ptr::null_mut();
                call!(api, vector_get_value(r.vector, row as ffi::idx_t, &mut value));
                let text = value_text(api, value);
                destroy_value(api, &mut value);
                match text {
                    Some(s) => push_json_string(out, &s),
                    None => out.push_str("null"),
                }
            }
            // A type this build has never seen: the schema line already said
            // lossless:false, so the payload stays honest and empty.
            _ => out.push_str("null"),
        }
    }
    Ok(())
}

fn hugeint(h: ffi::hugeint_t) -> i128 {
    (h.upper as i128) << 64 | h.lower as i128
}

/// The payload of a 16-byte bytes cell at a physical index.
unsafe fn bytes<'a>(r: &'a Reader, phys: usize) -> &'a [u8] {
    unsafe {
        let cell = &*(r.view.data as *const ffi::bytes_t).add(phys);
        super::bytes_view(cell)
    }
}

/// rust_decimal's Display, reproduced: sign, integer digits, and exactly
/// `scale` fractional digits when scale is nonzero.
fn push_decimal(out: &mut String, v: i128, scale: u8) {
    if v < 0 {
        out.push('-');
    }
    let abs = v.unsigned_abs();
    if scale == 0 {
        return push_u128_raw(out, abs);
    }
    let p = 10u128.pow(scale as u32);
    push_u128_raw(out, abs / p);
    out.push('.');
    // The fraction, zero-padded to exactly `scale` digits.
    let mut buf = [b'0'; 39];
    let mut x = abs % p;
    let mut i = buf.len();
    while x > 0 {
        i -= 1;
        buf[i] = b'0' + (x % 10) as u8;
        x /= 10;
    }
    let start = buf.len() - scale as usize;
    // Safety: the slice holds only ASCII digits.
    out.push_str(unsafe { std::str::from_utf8_unchecked(&buf[start..]) });
}

/// src/encode.rs fmt_timestamp, minus its duckdb-rs TimeUnit parameter:
/// `seconds_only` suppresses the fraction (TIMESTAMP_S), `zulu` appends the
/// timezone marker.
fn push_ts(out: &mut String, nanos: i128, seconds_only: bool, zulu: bool) {
    const DAY: i64 = 86_400_000_000_000;
    // Micros × 1000 can spill past i64 — hence the i128 signature — but
    // almost never does, and 128-bit div_euclid costs an order of magnitude
    // more than the 64-bit one.
    let (days, rest) = match i64::try_from(nanos) {
        Ok(n) => (n.div_euclid(DAY), n.rem_euclid(DAY)),
        Err(_) => (nanos.div_euclid(DAY as i128) as i64, nanos.rem_euclid(DAY as i128) as i64),
    };
    let (y, m, d) = civil_from_days(days);
    let (h, min, s, frac) = split_time(rest);
    if (0..=9999).contains(&y) {
        // The common era: one 19-byte write instead of eleven small ones.
        let mut b = [0u8; 19];
        b[0..2].copy_from_slice(&digit_pair(y as usize / 100));
        b[2..4].copy_from_slice(&digit_pair(y as usize % 100));
        b[4] = b'-';
        b[5..7].copy_from_slice(&digit_pair(m as usize));
        b[7] = b'-';
        b[8..10].copy_from_slice(&digit_pair(d as usize));
        b[10] = b'T';
        b[11..13].copy_from_slice(&digit_pair(h as usize));
        b[13] = b':';
        b[14..16].copy_from_slice(&digit_pair(min as usize));
        b[16] = b':';
        b[17..19].copy_from_slice(&digit_pair(s as usize));
        // Safety: the buffer holds only ASCII digits and punctuation.
        out.push_str(unsafe { std::str::from_utf8_unchecked(&b) });
    } else {
        push_int_pad(out, y, 4);
        out.push('-');
        push_int_pad(out, m as i64, 2);
        out.push('-');
        push_int_pad(out, d as i64, 2);
        out.push('T');
        push_int_pad(out, h, 2);
        out.push(':');
        push_int_pad(out, min, 2);
        out.push(':');
        push_int_pad(out, s, 2);
    }
    if !seconds_only {
        push_fraction(out, frac);
    }
    if zulu {
        out.push('Z');
    }
}

/// The engine's text rendering of a value, via the sized two-call protocol.
fn value_text(api: &ffi::Api, value: ffi::value_handle) -> Option<String> {
    let to_string = api.value_to_string?;
    let mut len: ffi::idx_t = 0;
    let mut err: ffi::error_info_handle = std::ptr::null_mut();
    let code = unsafe { to_string(value, std::ptr::null_mut(), 0, &mut len, &mut err) };
    if code != ffi::ERROR_NONE {
        let _ = Error::take(api, code, err);
        return None;
    }
    let mut buf = vec![0u8; len as usize + 1];
    let mut err: ffi::error_info_handle = std::ptr::null_mut();
    let code = unsafe {
        to_string(value, buf.as_mut_ptr() as *mut _, buf.len() as ffi::idx_t, &mut len, &mut err)
    };
    if code != ffi::ERROR_NONE {
        let _ = Error::take(api, code, err);
        return None;
    }
    buf.truncate(len as usize);
    String::from_utf8(buf).ok()
}
