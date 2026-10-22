//! Brace expansion for SQL: the shell's `{a,b}`, applied to a statement.
//!
//! ```sql
//! SELECT id, raw_request.{requisitionNumber, visitDate, patient.{lastName, firstName}}
//!   FROM orders
//! ```
//!
//! reaches the engine as
//!
//! ```sql
//! SELECT id, raw_request.requisitionNumber, raw_request.visitDate,
//!            raw_request.patient.lastName, raw_request.patient.firstName
//!   FROM orders
//! ```
//!
//! A group can sit anywhere in a term — `orders_{2025,2026}`,
//! `{first,last}Name`, `x.{a,b}::VARCHAR` — where a term is a run of text
//! between spaces, comments, commas, parentheses and semicolons, a space
//! being whatever the engine takes for one: the ASCII spaces, the vertical
//! tab among them, and the Unicode spaces it strips before it parses. A
//! cast's type parameters, the `(10,2)` right after `::DECIMAL`, stay in
//! the term, so `x.{a,b}::DECIMAL(10,2)` casts both. A type the engine
//! reads on past a space or comment, as in `::DECIMAL (10,2)`, `::INT []`
//! or `::DOUBLE PRECISION`, would reach only the last alternative, so a
//! term or an item that ends inside one is refused. Items
//! in a group are separated by commas; spaces and comments alone separate
//! them too, so a comma is never required, only clearer. A group nests; two
//! groups in one term multiply, as they do in a shell. The alternatives a
//! term expands to are joined with `, `, which is what a select list, a
//! `FROM` list and an argument list all take.
//!
//! DuckDB's own grammar uses braces in one place, the struct literal
//! `{'a': 1}`, and it always carries a lone `:` at its top level — one that
//! is not the `::` of a cast. Such a group is left exactly as it came, as is
//! an empty one. A brace that never closes leaves the whole statement as it
//! came, and the engine reports the syntax error at it.
//!
//! The statement is read once, as the engine reads it ([`wire::scan`]), and
//! every byte keeps that reading wherever the expansion carries it: only a
//! group in code expands, and a brace in a string, a quoted name, a dollar
//! quote or a comment is text. The engine reads the expanded statement
//! afresh, and a quote its pre-pass misreads can change where it strips a
//! Unicode space; an expansion the engine would read in any way other than
//! byte for byte as the statement it came from (a string's bytes taken for
//! code, two strings run into one, a string that takes backslash escapes in
//! one and not in the other, a space stripped in one and kept in the other)
//! is refused, with what to write instead: sent as written, its braces
//! would reach the engine, which reads them as a struct literal or not at
//! all.
//!
//! Each group multiplies, so a short statement can stand for an enormous one:
//! thirty two-item groups in a term are a billion alternatives. Expansion
//! therefore has a budget, [`BUDGET`] bytes of text written or re-read, and a
//! statement that would spend more is refused rather than expanded.

use std::borrow::Cow;

use wire::scan::{Kind, is_space, plain_spaces, spans};

/// How much text one statement's expansion may write or re-read: twice the
/// 8 MiB a request body may be, so what it produces is never much larger
/// than what a client could send outright. A select list of hundreds of paths
/// spends a few hundred KiB. Reaching the budget takes under a tenth of a
/// second; what the engine then makes of a statement that size is its own
/// cost, the same as if it had arrived unexpanded.
pub const BUDGET: usize = 16 << 20;

/// The expanded statement, or the input itself when there was nothing to do.
/// An error when the expansion would outrun [`BUDGET`], or could not be made
/// as written: one the engine would read another way, or a cast whose type
/// a group's term or item would cut short.
pub fn expand(sql: &str) -> Result<Cow<'_, str>, String> {
    if !sql.contains('{') {
        return Ok(Cow::Borrowed(sql));
    }
    let mut budget = BUDGET;
    let read = reading(sql);
    let text = Text { src: sql.as_bytes(), read: &read };
    if !text.balanced() {
        return Ok(Cow::Borrowed(sql));
    }
    let mut out = Piece::default();
    let mut term_start = 0;
    let mut depth = 0usize;
    let mut braced = false;
    let mut lists = None;
    let mut i = 0;
    while i < text.len() {
        let boundary = match text.class(i) {
            Class::Code => match text.src[i] {
                b'{' => {
                    depth += 1;
                    braced = true;
                    false
                }
                b'}' => {
                    depth -= 1;
                    false
                }
                b'(' if depth == 0 => match text.type_parameters(term_start, i, text.len(), &mut lists) {
                    Some(close) => {
                        i = close + 1;
                        continue;
                    }
                    None => true,
                },
                c => depth == 0 && matches!(c, b',' | b')' | b';'),
            },
            Class::Space => depth == 0,
            Class::Opaque => false,
        };
        if boundary {
            let alts = push_term(&mut out, text.slice(term_start, i), braced, &mut budget)?;
            // A term that stands for several ends here, at a space or
            // comment, and a cast's type the engine reads on past it would
            // reach only the last of them.
            if alts > 1
                && text.class(i) == Class::Space
                && let Some(refusal) = text.cast_beyond(term_start, i, text.len())
            {
                return Err(refusal);
            }
            out.push(text.slice(i, i + 1));
            term_start = i + 1;
            braced = false;
        }
        i += 1;
    }
    push_term(&mut out, text.slice(term_start, text.len()), braced, &mut budget)?;
    if out.src == sql.as_bytes() {
        return Ok(Cow::Borrowed(sql));
    }
    // Every cut falls on an ASCII byte or a whole Unicode space, so the text
    // is UTF-8; and it goes out only if the engine reads it as it was built.
    match String::from_utf8(out.src) {
        Ok(expanded) if reading(&expanded) == out.read => Ok(Cow::Owned(expanded)),
        Ok(expanded) => Err(misread(&out.read, &reading(&expanded)).into()),
        Err(_) => Err(QUOTE.into()),
    }
}

/// Why the engine would read an expansion another way than it was built,
/// from the first byte where the two readings part.
fn misread(built: &[u8], read: &[u8]) -> &'static str {
    match built.iter().zip(read).map(|(a, b)| a ^ b).find(|&d| d != 0) {
        Some(STRIPPED) => UNICODE_SPACE,
        Some(ESCAPES) => STRING_ESCAPES,
        _ => QUOTE,
    }
}

const UNICODE_SPACE: &str = "brace expansion refused: the engine would read a Unicode space in this statement another way \
     once its brace groups expand; write it as a plain space, or write the alternatives out";
const STRING_ESCAPES: &str = "brace expansion refused: a string beside a brace group would gain or lose backslash escapes \
     once the group expands; put a space between the group and the string, or write the alternatives out";
const QUOTE: &str = "brace expansion refused: a string or dollar quote beside a brace group would begin or end elsewhere \
     once the group expands; put a space between the group and the quote, or write the alternatives out";

/// Writes a term's expansion to `out`; returns how many alternatives it
/// stands for.
fn push_term(out: &mut Piece, term: Text, braced: bool, budget: &mut usize) -> Result<usize, String> {
    if !braced {
        out.push(term);
        return Ok(1);
    }
    let alts = alternatives(term, budget)?;
    for (k, alt) in alts.iter().enumerate() {
        if k > 0 {
            out.push(Text { src: b", ", read: &[CODE, CODE] });
        }
        out.push(alt.text());
    }
    Ok(alts.len())
}

/// Every piece of text a term stands for, in order. A term with no group
/// stands for itself. One group is resolved at a time, depth first, from a
/// stack rather than by recursion: a term can hold thousands of groups, one
/// item each, that multiply nothing and would each be a frame. Each piece is
/// charged its length before it is built, since it is built once, scanned
/// once and written at most once: a group of many items behind a long prefix
/// is refused before its alternatives take any memory.
fn alternatives(term: Text, budget: &mut usize) -> Result<Vec<Piece>, String> {
    let mut spend = |cost: usize| {
        *budget = budget.checked_sub(cost).ok_or_else(|| {
            format!(
                "brace expansion refused: this statement would expand past {} MiB",
                BUDGET >> 20
            )
        })?;
        Ok::<(), String>(())
    };
    spend(term.len())?;
    let mut out = Vec::new();
    let mut pending = vec![Piece::of(term)];
    while let Some(piece) = pending.pop() {
        let text = piece.text();
        let Some((lbrace, rbrace)) = text.first_group() else {
            out.push(piece);
            continue;
        };
        let items = text.items(lbrace + 1, rbrace)?;
        if items.is_empty() {
            // `x.{}` stands for nothing; the engine can say so.
            out.push(piece);
            continue;
        }
        let (prefix, suffix) = (text.slice(0, lbrace), text.slice(rbrace + 1, text.len()));
        let each = prefix.len() + suffix.len();
        spend(items.iter().fold(0usize, |sum, item| sum.saturating_add(each + item.len())))?;
        let next: Vec<Piece> = items
            .iter()
            .rev()
            .map(|item| {
                let mut alt = Piece::default();
                for part in [prefix, *item, suffix] {
                    alt.push(part);
                }
                alt
            })
            .collect();
        pending.extend(next);
    }
    Ok(out)
}

// How the engine reads a byte, packed in one: the kind of span it falls in,
// whether it begins a string, quoted name, dollar quote or comment, whether
// the string it begins takes backslash escapes, and whether the engine's
// pre-pass turns it into a space.
const CODE: u8 = 0;
const STR: u8 = 1;
const DOLLAR: u8 = 2;
const LINE_COMMENT: u8 = 3;
const BLOCK_COMMENT: u8 = 4;
const KIND: u8 = 7;
const BEGINS: u8 = 8;
const STRIPPED: u8 = 16;
const ESCAPES: u8 = 32;

/// The engine's reading of each byte of `src`. Two texts with the same bytes
/// that agree here are one statement to the engine.
fn reading(src: &str) -> Vec<u8> {
    let plain = plain_spaces(src.as_bytes());
    let mut read = vec![CODE; src.len()];
    for span in spans(&plain) {
        let kind = match span.kind {
            Kind::Code => continue,
            Kind::Str => STR,
            Kind::Dollar => DOLLAR,
            Kind::LineComment => LINE_COMMENT,
            Kind::BlockComment => BLOCK_COMMENT,
        };
        read[span.start..span.end].fill(kind);
        read[span.start] |= BEGINS;
        if span.escapes {
            read[span.start] |= ESCAPES;
        }
    }
    for (r, (a, b)) in read.iter_mut().zip(src.bytes().zip(plain.iter())) {
        if a != *b {
            *r |= STRIPPED;
        }
    }
    read
}

/// What a byte is to the expansion: code it reads, a space or comment that
/// separates terms and items, or the inside of a string, quoted name or
/// dollar quote, which travels as it is.
#[derive(Clone, Copy, PartialEq)]
enum Class {
    Code,
    Space,
    Opaque,
}

/// A piece of SQL beside the engine's reading of each of its bytes, taken
/// from the statement it was cut from.
#[derive(Clone, Copy)]
struct Text<'a> {
    src: &'a [u8],
    read: &'a [u8],
}

/// An alternative being built: text and reading, owned.
#[derive(Default)]
struct Piece {
    src: Vec<u8>,
    read: Vec<u8>,
}

impl Piece {
    fn of(text: Text) -> Self {
        Piece { src: text.src.to_vec(), read: text.read.to_vec() }
    }

    fn push(&mut self, text: Text) {
        self.src.extend_from_slice(text.src);
        self.read.extend_from_slice(text.read);
    }

    fn text(&self) -> Text<'_> {
        Text { src: &self.src, read: &self.read }
    }
}

impl<'a> Text<'a> {
    fn len(&self) -> usize {
        self.src.len()
    }

    fn slice(&self, from: usize, to: usize) -> Text<'a> {
        Text { src: &self.src[from..to], read: &self.read[from..to] }
    }

    fn class(&self, i: usize) -> Class {
        match self.read[i] & KIND {
            CODE if self.read[i] & STRIPPED != 0 || is_space(self.src[i]) => Class::Space,
            CODE => Class::Code,
            LINE_COMMENT | BLOCK_COMMENT => Class::Space,
            _ => Class::Opaque,
        }
    }

    /// The byte at `i` when the engine reads it as code and not a space.
    fn code(&self, i: usize) -> Option<u8> {
        (self.class(i) == Class::Code).then(|| self.src[i])
    }

    /// The first group that expands: `(lbrace, rbrace)`. A struct literal and
    /// an empty pair are stepped over, as is anything nested in them.
    fn first_group(&self) -> Option<(usize, usize)> {
        let mut i = 0;
        while i < self.len() {
            if self.code(i) == Some(b'{') {
                let close = self.matching(i)?;
                if !self.is_struct_literal(i + 1, close) {
                    return Some((i, close));
                }
                i = close + 1;
                continue;
            }
            i += 1;
        }
        None
    }

    /// The `}` that closes the `{` at `open`.
    fn matching(&self, open: usize) -> Option<usize> {
        let mut depth = 0usize;
        for i in open..self.len() {
            match self.code(i) {
                Some(b'{') => depth += 1,
                Some(b'}') => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// A lone `:` at the top level of the group — `{'a': 1}`, never `::`.
    fn is_struct_literal(&self, from: usize, to: usize) -> bool {
        let mut depth = 0usize;
        for i in from..to {
            match self.code(i) {
                Some(b'{') => depth += 1,
                Some(b'}') => depth = depth.saturating_sub(1),
                Some(b':') if depth == 0 => {
                    let before = i > from && self.code(i - 1) == Some(b':');
                    let after = i + 1 < to && self.code(i + 1) == Some(b':');
                    if !before && !after {
                        return true;
                    }
                }
                _ => {}
            }
        }
        false
    }

    /// Where a cast's type parameters close, when the `(` at `open` follows
    /// `::` and a type name, with nothing between, inside the text from
    /// `from`: `::DECIMAL(10,2)`, `::STRUCT(a INT)`. They are part of the
    /// word before them, so neither a term nor an item ends inside them. A
    /// list that holds a brace or a `;`, or does not close before `to`, is
    /// not taken. `lists` holds the text's parentheses, paired once when
    /// first needed: a scan forward from each `(` would make thousands of
    /// unclosed ones quadratic.
    fn type_parameters(&self, from: usize, open: usize, to: usize, lists: &mut Option<Vec<(usize, usize)>>) -> Option<usize> {
        let name = (from..open)
            .rev()
            .take_while(|&j| self.code(j).is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_'))
            .count();
        let start = open - name;
        if name == 0
            || self.src[start].is_ascii_digit()
            || start < from + 2
            || self.code(start - 1) != Some(b':')
            || self.code(start - 2) != Some(b':')
        {
            return None;
        }
        let lists = lists.get_or_insert_with(|| self.paired_parentheses(from, to));
        lists.binary_search_by_key(&open, |pair| pair.0).ok().map(|k| lists[k].1)
    }

    /// Each `(` in code between `from` and `to` beside the `)` that closes
    /// it, ordered by the `(`. A brace or a `;` closes no list, and every one
    /// open at it is left unpaired.
    fn paired_parentheses(&self, from: usize, to: usize) -> Vec<(usize, usize)> {
        let (mut open, mut pairs) = (Vec::new(), Vec::new());
        for i in from..to {
            match self.code(i) {
                Some(b'(') => open.push(i),
                Some(b')') => pairs.extend(open.pop().map(|o| (o, i))),
                Some(b'{' | b'}' | b';') => open.clear(),
                _ => {}
            }
        }
        pairs.sort_unstable();
        pairs
    }

    /// Whether every `}` in code closes a `{`, and every `{` is closed.
    fn balanced(&self) -> bool {
        let mut depth = 0usize;
        for i in 0..self.len() {
            match self.code(i) {
                Some(b'{') => depth += 1,
                Some(b'}') if depth == 0 => return false,
                Some(b'}') => depth -= 1,
                _ => {}
            }
        }
        depth == 0
    }

    /// A refusal when the word from `from` to `end` ends in a cast, `end` is
    /// the space or comment after it, and the engine reads the cast's type on
    /// past it, to the first thing after it before `to`: parameters,
    /// `::DECIMAL (10,2)`; array bounds, `::INT []` or `::INT ARRAY`; or the
    /// rest of a type of several words, `::DOUBLE PRECISION`. A brace group's
    /// term or item ends at that space, so its alternatives would split the
    /// type: the first cast to its name alone, or its rest an item of its own.
    fn cast_beyond(&self, from: usize, end: usize, to: usize) -> Option<String> {
        let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
        // Back over the cast: array bounds, `[]` or `[3]`, then parameters
        // the term kept, then the type's name and the `::` before it.
        let mut j = end;
        while j > from && self.code(j - 1) == Some(b']') {
            let digits = (from..j - 1).rev().take_while(|&k| self.code(k).is_some_and(|c| c.is_ascii_digit())).count();
            let open = j - 1 - digits;
            if open == from || self.code(open - 1) != Some(b'[') {
                return None;
            }
            j = open - 1;
        }
        let parameters = j > from && self.code(j - 1) == Some(b')');
        if parameters {
            let mut depth = 0usize;
            loop {
                j -= 1;
                match self.code(j) {
                    Some(b')') => depth += 1,
                    Some(b'(') => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                if j == from {
                    return None;
                }
            }
        }
        let name_len = (from..j).rev().take_while(|&k| self.code(k).is_some_and(word)).count();
        let start = j - name_len;
        if name_len == 0
            || self.src[start].is_ascii_digit()
            || start < from + 2
            || self.code(start - 1) != Some(b':')
            || self.code(start - 2) != Some(b':')
        {
            return None;
        }
        let name = String::from_utf8_lossy(&self.src[start..j]);
        let written = String::from_utf8_lossy(&self.src[start..end]);
        // Forward past spaces and comments to what follows.
        let next = (end..to).find(|&k| self.class(k) != Class::Space)?;
        let (follows, list) = match self.code(next) {
            Some(b'(') if !parameters => ("parameters", "(…)"),
            Some(b'[') => ("array bounds", "[…]"),
            _ => ("", ""),
        };
        if !follows.is_empty() {
            return Some(format!(
                "brace expansion refused: the space or comment after `::{written}` would split its {follows} from it \
                 across a brace group's alternatives; write `::{written}{list}` with nothing between, \
                 or cast each alternative on its own"
            ));
        }
        let after = (next..to).take_while(|&k| self.code(k).is_some_and(word)).count();
        let following = String::from_utf8_lossy(&self.src[next..next + after]);
        let one_word = type_continues(&name, &following)?;
        let fix = match one_word {
            "" => String::new(),
            "[]" => format!("write `::{written}[]`, or "),
            one => format!("write `::{one}`, or "),
        };
        Some(format!(
            "brace expansion refused: the space or comment after `::{written}` would split the type `{written} {following}` \
             across a brace group's alternatives; {fix}cast each alternative on its own"
        ))
    }

    /// The items between `from` and `to`, split at top-level spaces, comments
    /// and commas; a nested group travels whole inside its item, as do a
    /// cast's type parameters. An error when an item ends inside a cast's
    /// type ([`Text::cast_beyond`]).
    fn items(&self, from: usize, to: usize) -> Result<Vec<Text<'a>>, String> {
        let mut items = Vec::new();
        let mut depth = 0usize;
        let mut start = from;
        let mut lists = None;
        let mut i = from;
        while i < to {
            let split = match self.class(i) {
                Class::Code => match self.src[i] {
                    b'{' => {
                        depth += 1;
                        false
                    }
                    b'}' => {
                        depth = depth.saturating_sub(1);
                        false
                    }
                    b'(' if depth == 0 => {
                        if let Some(close) = self.type_parameters(start, i, to, &mut lists) {
                            i = close + 1;
                            continue;
                        }
                        false
                    }
                    c => depth == 0 && c == b',',
                },
                Class::Space => depth == 0,
                Class::Opaque => false,
            };
            if split {
                if i > start {
                    if self.class(i) == Class::Space
                        && let Some(refusal) = self.cast_beyond(start, i, to)
                    {
                        return Err(refusal);
                    }
                    items.push(self.slice(start, i));
                }
                start = i + 1;
            }
            i += 1;
        }
        if to > start {
            items.push(self.slice(start, to));
        }
        Ok(items)
    }
}

/// When `following`, the word after a cast to `name`, is part of that type
/// to the engine: the type's one-word name, `[]` for `ARRAY`, or nothing
/// when it has none. Case aside, as the engine reads them: `ARRAY` after any
/// type, `DOUBLE PRECISION`, `TIMESTAMP` and `TIME` `WITH` or `WITHOUT TIME
/// ZONE`, `CHARACTER`, `CHAR`, `NCHAR` and `BIT VARYING`, `NATIONAL
/// CHARACTER`, and an `INTERVAL`'s unit. Any other word after a type is an
/// alias, or a word the engine refuses there.
fn type_continues(name: &str, following: &str) -> Option<&'static str> {
    let (name, next) = (name.to_ascii_uppercase(), following.to_ascii_uppercase());
    let one_word = match (name.as_str(), next.as_str()) {
        (_, "ARRAY") => "[]",
        ("DOUBLE", "PRECISION") => "DOUBLE",
        ("TIMESTAMP", "WITH") => "TIMESTAMPTZ",
        ("TIME", "WITH") => "TIMETZ",
        ("TIMESTAMP", "WITHOUT") => "TIMESTAMP",
        ("TIME", "WITHOUT") => "TIME",
        ("CHARACTER" | "CHAR" | "NCHAR", "VARYING") | ("NATIONAL", "CHARACTER" | "CHAR") => "VARCHAR",
        ("BIT", "VARYING") => "BIT",
        ("INTERVAL", unit) if INTERVAL_UNITS.contains(&unit) => "",
        _ => return None,
    };
    Some(one_word)
}

/// The units the engine reads after `::INTERVAL` as part of the type.
const INTERVAL_UNITS: &[&str] = &[
    "YEAR", "YEARS", "MONTH", "MONTHS", "DAY", "DAYS", "HOUR", "HOURS", "MINUTE", "MINUTES", "SECOND", "SECONDS",
    "MILLISECOND", "MILLISECONDS", "MICROSECOND", "MICROSECONDS", "WEEK", "WEEKS", "QUARTER", "QUARTERS", "DECADE",
    "DECADES", "CENTURY", "CENTURIES", "MILLENNIUM", "MILLENNIA",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn x(sql: &str) -> String {
        expand(sql).unwrap().into_owned()
    }

    #[test]
    fn a_path_group_expands_to_paths() {
        assert_eq!(x("select raw_request.{a,b} from t"), "select raw_request.a, raw_request.b from t");
        assert_eq!(x("select r.{a b c} from t"), "select r.a, r.b, r.c from t");
        assert_eq!(x("select r.{a, b ,c,} from t"), "select r.a, r.b, r.c from t");
    }

    #[test]
    fn groups_nest_and_span_lines() {
        let sql = "select\n  id,\n  raw_request.{\n    requisitionNumber\n    visitDate\n    patient.{\n      lastName\n      firstName\n    }\n  },\nfrom\n  orders\nwhere\n  raw_request.patient.lastName ilike 'morel'\n;";
        assert_eq!(
            x(sql),
            "select\n  id,\n  raw_request.requisitionNumber, raw_request.visitDate, raw_request.patient.lastName, raw_request.patient.firstName,\nfrom\n  orders\nwhere\n  raw_request.patient.lastName ilike 'morel'\n;"
        );
    }

    #[test]
    fn a_group_can_sit_anywhere_in_a_term() {
        assert_eq!(x("select * from orders_{2025,2026}"), "select * from orders_2025, orders_2026");
        assert_eq!(x("select {first,last}Name from p"), "select firstName, lastName from p");
        assert_eq!(x("select r.{a,b}::VARCHAR from t"), "select r.a::VARCHAR, r.b::VARCHAR from t");
        assert_eq!(x("select {a,b} from t"), "select a, b from t");
        assert_eq!(x("select lower(r.{a,b}) from t"), "select lower(r.a, r.b) from t");
    }

    #[test]
    fn two_groups_in_a_term_multiply() {
        assert_eq!(x("select r.{a,b}.{x,y} from t"), "select r.a.x, r.a.y, r.b.x, r.b.y from t");
        assert_eq!(x("select {a,b}_{1,2} from t"), "select a_1, a_2, b_1, b_2 from t");
    }

    #[test]
    fn a_struct_literal_is_left_alone() {
        for sql in [
            "select {'a': 1}",
            "select {'a': 1, 'b': {'c': 2}}::VARIANT",
            "insert into t values ({'patient': {'firstName': 'Cat'}, 'flag': true})",
            "select MAP {'k': 1}",
            "select {}",
            "select r.{}",
        ] {
            assert!(matches!(expand(sql), Ok(Cow::Borrowed(_))), "{sql}");
        }
        // a struct literal inside a group item travels whole
        assert_eq!(x("select r.{a {'k': 1}} from t"), "select r.a, r.{'k': 1} from t");
        // a cast's `::` is not the marker
        assert_eq!(x("select r.{a::INT b} from t"), "select r.a::INT, r.b from t");
    }

    #[test]
    fn strings_comments_and_quoted_names_are_opaque() {
        for sql in [
            "select '{a,b}'",
            "select $$ x.{a,b} $$",
            "select 1 -- r.{a,b}\n",
            "select /* r.{a,b} */ 1",
            "select \"weird{name}\" from t",
        ] {
            assert!(matches!(expand(sql), Ok(Cow::Borrowed(_))), "{sql}");
        }
        assert_eq!(x("select r.{a \"first-name\"} from t"), "select r.a, r.\"first-name\" from t");
        assert_eq!(x("select \"my col\".{a,b} from t"), "select \"my col\".a, \"my col\".b from t");
        assert_eq!(x("select r.{a,b} from t where s = '{'"), "select r.a, r.b from t where s = '{'");
    }

    /// A space is what the engine takes for one: the vertical tab, and a
    /// Unicode space wherever its pre-pass strips it, end a term and separate
    /// items as an ASCII space does; a comment does too.
    #[test]
    fn a_term_ends_where_the_engine_ends_a_word() {
        assert_eq!(x("select r.{a,b}\u{b}from t"), "select r.a, r.b\u{b}from t");
        assert_eq!(x("select r.{a,b}\u{a0}from t"), "select r.a, r.b\u{a0}from t");
        assert_eq!(x("select r.{a\u{3000}b\u{b}c} from t"), "select r.a, r.b, r.c from t");
        assert_eq!(x("select r.{a,b}-- c\nfrom t"), "select r.a, r.b-- c\nfrom t");
        assert_eq!(x("select r.{a,b}/* c */from t"), "select r.a, r.b/* c */from t");
        assert_eq!(x("select r.{a /* , */ b -- }\n c} from t"), "select r.a, r.b, r.c from t");
        // Where the pre-pass keeps a Unicode space, here inside the quote a
        // `'` in a comment opens for it, its bytes are part of the word, and
        // of the item.
        assert_eq!(x("select /* ' */ r.{a\u{a0}b} from t"), "select /* ' */ r.a\u{a0}b from t");
        // U+1680 is no space to the engine.
        assert_eq!(x("select r.{a,b}\u{1680}x"), "select r.a\u{1680}x, r.b\u{1680}x");
    }

    /// The statement is read once, and every alternative keeps that reading:
    /// a group in a string stays text, and an expansion the engine would
    /// read another way is refused, saying what to write instead.
    #[test]
    fn an_expansion_the_engine_would_misread_is_refused() {
        for (sql, why) in [
            // `}E'` begins an E-string and `xE'` does not: read afresh, the
            // string after it would come out as `'a'` and `'b'`.
            (r"select {x}E'\'''{a,b}'", STRING_ESCAPES),
            // `$$` after a brace opens a dollar quote; after a word it does not.
            ("select r.{a,b}$$x.{c,d}$$", QUOTE),
            // Two strings would run into one.
            ("select {'a' x}'b'", QUOTE),
            // An unterminated string would take in the next alternative.
            ("select {a b}'{", QUOTE),
            // The engine's pre-pass misreads `E'\''`, and repeated, the
            // misreading flips: the U+00A0 in the string after it would be
            // stripped where it was kept.
            ("select {a,b}.E'\\'', 'x\u{a0}y'", UNICODE_SPACE),
            // An `E` that begins a word makes the string after it take
            // backslash escapes, whether or not one moves its end: `}'` is a
            // plain string and `E'` is not, `d'` is plain and `e'` is not,
            // `}e'` takes escapes and `xe'` does not.
            (r"select {E}'a\tb'", STRING_ESCAPES),
            (r"select t.{d,e}'a\tb'", STRING_ESCAPES),
            (r"select {x,y}e'a\tb'", STRING_ESCAPES),
        ] {
            assert_eq!(expand(sql), Err(why.to_string()), "{sql:?}");
        }
        // Each refusal's fix is one the engine reads as written.
        assert_eq!(x(r"select {x} E'\'''{a,b}'"), r"select x E'\'''{a,b}'");
        assert_eq!(x("select r.{a,b} $$x.{c,d}$$"), "select r.a, r.b $$x.{c,d}$$");
        assert_eq!(x("select {'a' x} 'b'"), "select 'a', x 'b'");
        assert_eq!(x("select {a,b}.E'\\'', 'x y'"), "select a.E'\\'', b.E'\\'', 'x y'");
        assert_eq!(x(r"select t.{d,e} 'a\tb'"), r"select t.d, t.e 'a\tb'");
        // A misreading the expansion does not repeat changes nothing.
        assert_eq!(x("select /* ' */ r.{a,b}, 'x\u{a0}y'"), "select /* ' */ r.a, r.b, 'x\u{a0}y'");
        assert_eq!(x("select E'\\'', r.{a,b}, 'x\u{a0}y'"), "select E'\\'', r.a, r.b, 'x\u{a0}y'");
    }

    /// A cast's type parameters belong to the word before them: the
    /// expansion casts every alternative, never only the last.
    #[test]
    fn a_cast_keeps_its_type_parameters() {
        assert_eq!(x("select x.{a,b}::DECIMAL(10,2) from t"), "select x.a::DECIMAL(10,2), x.b::DECIMAL(10,2) from t");
        assert_eq!(x("select x.{a,b}::VARCHAR(20)"), "select x.a::VARCHAR(20), x.b::VARCHAR(20)");
        assert_eq!(
            x("select x.{a,b}::STRUCT(k DECIMAL(4,1), v INT)[] y"),
            "select x.a::STRUCT(k DECIMAL(4,1), v INT)[], x.b::STRUCT(k DECIMAL(4,1), v INT)[] y"
        );
        assert_eq!(x("select r.{a::DECIMAL(10,2), b} from t"), "select r.a::DECIMAL(10,2), r.b from t");
        assert_eq!(x("select round(r.{a,b}::DECIMAL(9,3), 1)"), "select round(r.a::DECIMAL(9,3), r.b::DECIMAL(9,3), 1)");
        // Anything else ends the term as ever: a call's parentheses, a
        // space (here after the `::`, which the engine then refuses), and
        // the `AS` of a `CAST`, which it refuses too. A space between the
        // name and its parameters is refused, as
        // `a_cast_a_group_would_split_is_refused` shows.
        assert_eq!(x("select f(r.{a,b})"), "select f(r.a, r.b)");
        assert_eq!(x("select r.{a,b}:: DECIMAL(10,2)"), "select r.a::, r.b:: DECIMAL(10,2)");
        assert_eq!(x("select cast(x.{a,b} as DECIMAL(10,2))"), "select cast(x.a, x.b as DECIMAL(10,2))");
        // A list that holds a brace, or never closes, is not taken.
        assert_eq!(x("select r.{a,b}::T({c})"), "select r.a::T, r.b::T(c)");
        assert!(matches!(expand("select r.{a,b}::DECIMAL(10"), Ok(Cow::Owned(_))));
        // Lists that never close are paired once, not each scanned to the
        // end: a megabyte of them expands at once, where a scan from each
        // would take minutes.
        let open = format!("select r.{{a,b}} {}", "x::a(".repeat(200_000));
        assert_eq!(x(&open).len(), open.len() + 1);
        let open = format!("select r.{{{}}}", "x::a(".repeat(200_000));
        assert_eq!(x(&open).len(), open.len() - 2);
    }

    /// A type the engine reads on past a space or comment would be split
    /// across the alternatives, the first cast to its name alone: such a
    /// statement is refused, naming the type and the fix.
    #[test]
    fn a_cast_a_group_would_split_is_refused() {
        let refusal = |sql: &str| expand(sql).expect_err(sql);
        let parameters = "brace expansion refused: the space or comment after `::DECIMAL` would split its parameters \
                          from it across a brace group's alternatives; write `::DECIMAL(…)` with nothing between, \
                          or cast each alternative on its own";
        assert_eq!(refusal("select x.{a,b}::DECIMAL (10,2) from t"), parameters);
        assert_eq!(refusal("select x.{a,b}::DECIMAL/* p */(10,2) from t"), parameters);
        assert_eq!(refusal("select x.{a,b}::DECIMAL\u{a0}(10,2) from t"), parameters);
        assert_eq!(
            refusal("select x.{a,b}::INT [] from t"),
            "brace expansion refused: the space or comment after `::INT` would split its array bounds \
             from it across a brace group's alternatives; write `::INT[…]` with nothing between, \
             or cast each alternative on its own"
        );
        assert!(refusal("select x.{a,b}::INT[] [2]").contains("after `::INT[]` would split its array bounds"));
        for (sql, typ, fix) in [
            ("select x.{a,b}::DOUBLE PRECISION from t", "DOUBLE PRECISION", "write `::DOUBLE`, or "),
            ("select x.{a,b}::timestamp with time zone", "timestamp with", "write `::TIMESTAMPTZ`, or "),
            ("select x.{a,b}::TIME WITHOUT TIME ZONE", "TIME WITHOUT", "write `::TIME`, or "),
            ("select x.{a,b}::CHARACTER VARYING(9)", "CHARACTER VARYING", "write `::VARCHAR`, or "),
            ("select x.{a,b}::NATIONAL CHAR", "NATIONAL CHAR", "write `::VARCHAR`, or "),
            ("select x.{a,b}::INT ARRAY", "INT ARRAY", "write `::INT[]`, or "),
            ("select x.{a,b}::DECIMAL(10,2) ARRAY", "DECIMAL(10,2) ARRAY", "write `::DECIMAL(10,2)[]`, or "),
            ("select x.{a,b}::INTERVAL -- c\n YEAR", "INTERVAL YEAR", ""),
            ("select x.{a,b}::INT[3] ARRAY", "INT[3] ARRAY", "write `::INT[3][]`, or "),
            // Inside a group the item ends there, and the type's rest would
            // be an item of its own.
            ("select r.{a::DOUBLE PRECISION, b} from t", "DOUBLE PRECISION", "write `::DOUBLE`, or "),
            ("select r.{a::INTERVAL year} from t", "INTERVAL year", ""),
        ] {
            let got = refusal(sql);
            assert!(got.starts_with("brace expansion refused: the space or comment after `::"), "{sql:?}: {got}");
            assert!(got.ends_with(&format!("split the type `{typ}` across a brace group's alternatives; {fix}cast each alternative on its own")), "{sql:?}: {got}");
        }
        // Written without the space, the type is whole in every alternative;
        // a group of one alternative splits nothing; an alias, or a word the
        // engine reads as no part of the type, ends the term as ever; a
        // struct literal and an unbalanced brace are left as they came.
        assert_eq!(x("select x.{a,b}::INT[] from t"), "select x.a::INT[], x.b::INT[] from t");
        assert_eq!(x("select x.{a}::DOUBLE PRECISION"), "select x.a::DOUBLE PRECISION");
        assert_eq!(x("select x.{a}.{b}::DECIMAL (10,2)"), "select x.a.b::DECIMAL (10,2)");
        assert_eq!(x("select x.{a,b}::INT year from t"), "select x.a::INT, x.b::INT year from t");
        assert_eq!(x("select x.{a,b}::DOUBLE, c"), "select x.a::DOUBLE, x.b::DOUBLE, c");
        assert_eq!(x("select x.{a,b}::DOUBLE\nfrom t"), "select x.a::DOUBLE, x.b::DOUBLE\nfrom t");
        assert_eq!(x("select r.{a::INT array_col}"), "select r.a::INT, r.array_col");
        for sql in [
            "select {'k': 1}::STRUCT (k INT)",
            "select {'k': 1}::DOUBLE PRECISION",
            "select x.{a,b}::DECIMAL (10,2) }",
            "select '{a,b}'::DOUBLE PRECISION",
            "select 1::DECIMAL (10,2)",
        ] {
            assert!(matches!(expand(sql), Ok(Cow::Borrowed(_))), "{sql:?}");
        }
    }

    #[test]
    fn an_unbalanced_brace_leaves_the_statement_as_it_came() {
        for sql in ["select r.{a,b from t", "select r.a} from t", "select r.{a,{b} from t", "select r.{a 'b} from t"] {
            assert!(matches!(expand(sql), Ok(Cow::Borrowed(_))), "{sql}");
        }
    }

    #[test]
    fn an_expansion_past_the_budget_is_refused_not_run() {
        // Thirty groups of two: a billion alternatives from 150 bytes.
        let doubling = format!("select 1 as x{}", "{a,b}".repeat(30));
        assert!(expand(&doubling).unwrap_err().contains("brace expansion refused"));
        // Groups that multiply nothing still cost a scan each, and would
        // each have been a stack frame.
        let chained = format!("select x{} from t", "{a}".repeat(100_000));
        assert!(expand(&chained).is_err());
        // One group of many items behind a long prefix is refused before
        // its alternatives are built: 70 KB that would have asked for 500 MB.
        let wide_prefix = format!("select 1 as {}{{{}}}", "x".repeat(50_000), vec!["a"; 10_000].join(","));
        assert!(expand(&wide_prefix).is_err());
        // Well inside it, a wide select list expands as ever.
        let wide = format!("select r.{{{}}} from t", (0..2000).map(|i| format!("c{i}")).collect::<Vec<_>>().join(","));
        assert_eq!(x(&wide).matches("r.c").count(), 2000);
    }

    #[test]
    fn nothing_to_do_borrows() {
        assert!(matches!(expand("select 1"), Ok(Cow::Borrowed(_))));
        assert!(matches!(expand("select a.b, a.c from t"), Ok(Cow::Borrowed(_))));
    }

    /// A statement as the engine reads it, one unit at a time: a string,
    /// quoted name, dollar quote or comment whole, with whether a string
    /// takes backslash escapes, each other byte alone, and a space as a space
    /// whatever its bytes. Each unit keeps the bytes it was read from.
    #[derive(Clone, PartialEq, Debug)]
    enum Tok {
        Space,
        Code(u8),
        Quoted(Kind, bool, Vec<u8>),
        Comment(Vec<u8>),
    }

    type Unit = (Tok, Vec<u8>);

    fn units(sql: &str) -> Vec<Unit> {
        let plain = plain_spaces(sql.as_bytes());
        let mut out = Vec::new();
        for sp in spans(&plain) {
            let raw = sql.as_bytes()[sp.start..sp.end].to_vec();
            let read = plain[sp.start..sp.end].to_vec();
            match sp.kind {
                Kind::Code => {
                    for (&r, &p) in raw.iter().zip(&read) {
                        out.push((if is_space(p) { Tok::Space } else { Tok::Code(p) }, vec![r]));
                    }
                }
                Kind::LineComment | Kind::BlockComment => out.push((Tok::Comment(read), raw)),
                kind => out.push((Tok::Quoted(kind, sp.escapes, read), raw)),
            }
        }
        out
    }

    /// The reading with each run of spaces taken as one, as the engine takes it.
    fn reading_of(units: &[Unit]) -> Vec<Tok> {
        let mut out: Vec<Tok> = Vec::new();
        for (tok, _) in units {
            if !(*tok == Tok::Space && out.last() == Some(&Tok::Space)) {
                out.push(tok.clone());
            }
        }
        out
    }

    fn separates(tok: &Tok) -> bool {
        matches!(tok, Tok::Space | Tok::Comment(_))
    }

    /// The expansion worked out a second way, by recursion over the units of
    /// the statement's one reading: what it should become, and how the
    /// engine should read that; `None` when a term or an item would end
    /// inside a cast's type.
    fn reference(sql: &str) -> Option<(Vec<u8>, Vec<Tok>)> {
        let units = units(sql);
        let unchanged = || Some((sql.as_bytes().to_vec(), reading_of(&units)));
        let mut out: Vec<Unit> = Vec::new();
        let (mut depth, mut term, mut split) = (0usize, Vec::new(), false);
        // The term's alternatives written out; how many there were, or
        // `None` when an item splits a cast.
        let flush = |out: &mut Vec<Unit>, term: &mut Vec<Unit>| {
            let alts = if term.iter().any(|u| u.0 == Tok::Code(b'{')) { reference_alternatives(term)? } else { vec![term.clone()] };
            let n = alts.len();
            for (k, alt) in alts.into_iter().enumerate() {
                if k > 0 {
                    out.push((Tok::Code(b','), b",".to_vec()));
                    out.push((Tok::Space, b" ".to_vec()));
                }
                out.extend(alt);
            }
            term.clear();
            Some(n)
        };
        let mut k = 0;
        while k < units.len() {
            let unit = &units[k];
            match unit.0 {
                Tok::Code(b'{') => depth += 1,
                Tok::Code(b'}') if depth == 0 => return unchanged(),
                Tok::Code(b'}') => depth -= 1,
                _ => {}
            }
            if let Some(close) = (depth == 0).then(|| reference_type_parameters(&term, &units, k)).flatten() {
                term.extend_from_slice(&units[k..=close]);
                k = close + 1;
                continue;
            }
            let boundary = depth == 0 && (separates(&unit.0) || matches!(unit.0, Tok::Code(b',' | b'(' | b')' | b';')));
            if boundary {
                let cast = separates(&unit.0) && reference_cast_beyond(&term, &units[k..]);
                match flush(&mut out, &mut term) {
                    Some(n) => split |= n > 1 && cast,
                    None => split = true,
                }
                out.push(unit.clone());
            } else {
                term.push(unit.clone());
            }
            k += 1;
        }
        if depth != 0 {
            return unchanged();
        }
        if flush(&mut out, &mut term).is_none() || split {
            return None;
        }
        Some((out.iter().flat_map(|u| u.1.clone()).collect(), reading_of(&out)))
    }

    /// Whether `word` ends in a cast whose type the engine reads on past the
    /// separators that begin `rest`: `::name`, its parameters and array
    /// bounds, then a `(` (when it has no parameters), a `[`, or a word that
    /// continues the type.
    fn reference_cast_beyond(word: &[Unit], rest: &[Unit]) -> bool {
        let code = |u: &Unit| match u.0 {
            Tok::Code(c) => Some(c),
            _ => None,
        };
        let ident = |u: &Unit| code(u).is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_');
        let mut n = word.len();
        while n > 0 && code(&word[n - 1]) == Some(b']') {
            let digits = word[..n - 1].iter().rev().take_while(|u| code(u).is_some_and(|c| c.is_ascii_digit())).count();
            if n < digits + 2 || code(&word[n - 2 - digits]) != Some(b'[') {
                return false;
            }
            n -= digits + 2;
        }
        let parameters = n > 0 && code(&word[n - 1]) == Some(b')');
        if parameters {
            let mut depth = 0;
            loop {
                n -= 1;
                match code(&word[n]) {
                    Some(b')') => depth += 1,
                    Some(b'(') => depth -= 1,
                    _ => {}
                }
                if depth == 0 {
                    break;
                }
                if n == 0 {
                    return false;
                }
            }
        }
        let name: Vec<u8> = word[..n].iter().rev().take_while(|u| ident(u)).filter_map(code).collect();
        let at = n - name.len();
        if name.is_empty() || name.last().unwrap().is_ascii_digit() || at < 2 || word[at - 2..at].iter().any(|u| code(u) != Some(b':')) {
            return false;
        }
        let name: String = name.iter().rev().map(|&c| c as char).collect();
        let Some(next) = rest.iter().position(|u| !separates(&u.0)) else {
            return false;
        };
        match code(&rest[next]) {
            Some(b'(') => !parameters,
            Some(b'[') => true,
            _ => {
                let following: String = rest[next..].iter().take_while(|u| ident(u)).filter_map(code).map(|c| c as char).collect();
                super::type_continues(&name, &following).is_some()
            }
        }
    }

    /// Where the parentheses that open at `units[open]` close, when they
    /// follow `::` and a type name that end `before`, and hold no brace or
    /// `;`: a cast's type parameters, part of the word before them.
    fn reference_type_parameters(before: &[Unit], units: &[Unit], open: usize) -> Option<usize> {
        if units[open].0 != Tok::Code(b'(') {
            return None;
        }
        let word = |u: &Unit| matches!(u.0, Tok::Code(c) if c.is_ascii_alphanumeric() || c == b'_');
        let name = before.iter().rev().take_while(|u| word(u)).count();
        let n = before.len();
        if name == 0 || n < name + 2 || matches!(before[n - name].0, Tok::Code(c) if c.is_ascii_digit()) {
            return None;
        }
        if before[n - name - 2..n - name].iter().any(|u| u.0 != Tok::Code(b':')) {
            return None;
        }
        let mut depth = 0;
        for (j, u) in units.iter().enumerate().skip(open) {
            match u.0 {
                Tok::Code(b'(') => depth += 1,
                Tok::Code(b')') => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(j);
                    }
                }
                Tok::Code(b'{' | b'}' | b';') => return None,
                _ => {}
            }
        }
        None
    }

    fn reference_alternatives(term: &[Unit]) -> Option<Vec<Vec<Unit>>> {
        let close = |open: usize| {
            let mut depth = 0;
            for (k, u) in term.iter().enumerate().skip(open) {
                match u.0 {
                    Tok::Code(b'{') => depth += 1,
                    Tok::Code(b'}') => {
                        depth -= 1;
                        if depth == 0 {
                            return k;
                        }
                    }
                    _ => {}
                }
            }
            unreachable!("a term reaches here balanced")
        };
        let mut open = 0;
        let group = loop {
            match term[open..].iter().position(|u| u.0 == Tok::Code(b'{')) {
                None => return Some(vec![term.to_vec()]),
                Some(k) => {
                    let (o, c) = (open + k, close(open + k));
                    let inner = &term[o + 1..c];
                    let mut depth = 0;
                    let colon = (0..inner.len()).any(|j| {
                        match inner[j].0 {
                            Tok::Code(b'{') => depth += 1,
                            Tok::Code(b'}') => depth -= 1,
                            _ => {}
                        }
                        let colon = |j: usize| inner.get(j).is_some_and(|u| u.0 == Tok::Code(b':'));
                        depth == 0 && colon(j) && !(j > 0 && colon(j - 1)) && !colon(j + 1)
                    });
                    if !colon {
                        break (o, c);
                    }
                    open = c + 1;
                }
            }
        };
        let (o, c) = group;
        let mut items: Vec<Vec<Unit>> = vec![Vec::new()];
        let mut depth = 0;
        let inner = &term[o + 1..c];
        let mut k = 0;
        while k < inner.len() {
            let u = &inner[k];
            match u.0 {
                Tok::Code(b'{') => depth += 1,
                Tok::Code(b'}') => depth -= 1,
                _ => {}
            }
            let item = items.last_mut().unwrap();
            if let Some(close) = (depth == 0).then(|| reference_type_parameters(item, inner, k)).flatten() {
                item.extend_from_slice(&inner[k..=close]);
                k = close + 1;
                continue;
            }
            if depth == 0 && (separates(&u.0) || u.0 == Tok::Code(b',')) {
                if separates(&u.0) && reference_cast_beyond(item, &inner[k..]) {
                    return None;
                }
                items.push(Vec::new());
            } else {
                item.push(u.clone());
            }
            k += 1;
        }
        items.retain(|item| !item.is_empty());
        if items.is_empty() {
            return Some(vec![term.to_vec()]);
        }
        let mut alts = Vec::new();
        for item in items {
            alts.extend(reference_alternatives(&[&term[..o], &item[..], &term[c + 1..]].concat())?);
        }
        Some(alts)
    }

    /// What a generated statement is made of: code, groups, casts, the quotes
    /// the engine knows and the ones its pre-pass misreads, comments, and the
    /// spaces, ASCII and Unicode, that it strips and that it keeps.
    const FRAGMENTS: &[&str] = &[
        "select ", "r", ".", "x", "e", "E", "a1", "é", "1", "1e5", ".5", ",", "(", ")", ";", ":", "::", "$", "\\", "_",
        "{", "}", "{a,b}", "{a b}", "r.{a,b}", "{'k': 1}", "{}", "{a,{b,c}}", "{a::INT b}", "{x}",
        "::DECIMAL(10,2)", "::T(", "::S(a {b} c)", "{a::V(1, 2) b}", "::X(E'\\', \"(\")", "2(",
        "::DOUBLE", " PRECISION", "::INTERVAL", " year", " ARRAY", "::INT[]", "[2]", " [", "]", " (",
        "'", "'{a,b}'", "''", "'\\'", "E'\\''", "e'a\\\\b'", "\"", "\"{a,b}\"", "`", "`{a,b}`", "`it's`",
        "$$", "$$x.{a,b}$$", "$t$", "$t${a,b}$t$", "$t$t$",
        "--", "-- {a,b}\n", "/*", "*/", "/* ' */", "/* {a,b} */", "\n", "\r",
        " ", "\t", "\u{b}", "\u{c}", "\u{a0}", "\u{2000}", "\u{200b}", "\u{3000}", "\u{feff}", "\u{202f}", "\u{1680}", "\u{2028}",
    ];

    /// `n` statements from `seed`, each checked against the reference: an
    /// expansion is the reference's to the byte and reads as the reference
    /// says the engine should read it; a statement left as it came is one
    /// with nothing to expand, or one whose expansion the engine would read
    /// another way, there or in the text around it. Returns how many expanded
    /// and how many were refused.
    fn fuzz(seed: u64, n: usize) -> (usize, usize) {
        let mut state = seed.max(1);
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let (mut expanded, mut refused, mut wrong, mut failures) = (0, 0, 0, Vec::new());
        for _ in 0..n {
            let len = 1 + next() % 24;
            let sql: String = (0..len).map(|_| FRAGMENTS[(next() % FRAGMENTS.len() as u64) as usize]).collect();
            let got = expand(&sql);
            let (ok, want) = match (reference(&sql), &got) {
                // A cast's type a term or an item would split.
                (None, got) => {
                    refused += 1;
                    (got.as_ref().is_err_and(|e| e.contains("would split")), None)
                }
                (Some((want, read)), got) => {
                    let want = String::from_utf8(want).unwrap();
                    let ok = match got {
                        _ if want == sql => *got == Ok(Cow::Borrowed(sql.as_str())),
                        // One the engine would read another way.
                        Err(e) => {
                            refused += 1;
                            [UNICODE_SPACE, STRING_ESCAPES, QUOTE].contains(&e.as_str()) && reading_of(&units(&want)) != read
                        }
                        Ok(got) => {
                            expanded += 1;
                            *got == want && reading_of(&units(got)) == read
                        }
                    };
                    (ok, Some(want))
                }
            };
            if !ok {
                wrong += 1;
                if failures.len() < 10 {
                    failures.push(format!("{sql:?}\n   got  {got:?}\n   want {want:?}"));
                }
            }
        }
        assert!(wrong == 0, "seed {seed}: {wrong} of {n} wrong, among them\n{}", failures.join("\n"));
        (expanded, refused)
    }

    #[test]
    fn a_generated_statement_expands_only_where_the_engine_reads_code() {
        let (expanded, refused) = fuzz(0x5eed_b4ac, 20_000);
        // The fragments are chosen to make trouble, so a refusal is common;
        // both answers are exercised.
        assert!(expanded > 2_000 && refused > 1_000, "{expanded} expanded, {refused} refused");
    }

    /// Harbor's half of `test/scripts/braces.py`, which has the engine
    /// parse what this writes: each NUL-ended statement in the file
    /// `UNBRACE_IN`, expanded into the file `UNBRACE_OUT`, NUL-ended and led
    /// by `X` (expanded), `S` (left as it came) or `R` (refused, and the
    /// refusal). Without the two names it does nothing.
    #[test]
    #[ignore]
    fn expand_a_file() {
        let (Ok(input), Ok(output)) = (std::env::var("UNBRACE_IN"), std::env::var("UNBRACE_OUT")) else {
            return;
        };
        let input = std::fs::read(input).unwrap();
        let mut out = Vec::new();
        for sql in input.strip_suffix(b"\0").unwrap_or(&input).split(|&b| b == 0) {
            let sql = std::str::from_utf8(sql).unwrap();
            match expand(sql) {
                Ok(Cow::Owned(expanded)) => out.extend([b"X", expanded.as_bytes()].concat()),
                Ok(Cow::Borrowed(same)) => out.extend([b"S", same.as_bytes()].concat()),
                Err(refusal) => out.extend([b"R", refusal.as_bytes()].concat()),
            }
            out.push(0);
        }
        std::fs::write(output, out).unwrap();
    }

    /// The long run: `UNBRACE_FUZZ=<count>[:<seed>] cargo test --release -p
    /// harbor unbrace -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn a_long_fuzz() {
        let spec = std::env::var("UNBRACE_FUZZ").unwrap_or_default();
        let (n, seed) = spec.split_once(':').unwrap_or((&spec, "1"));
        let (n, seed) = (n.parse().unwrap_or(1_000_000), seed.parse().unwrap_or(1));
        let (expanded, refused) = fuzz(seed, n);
        println!("unbrace fuzz: {n} statements, seed {seed}: {expanded} expanded, {refused} refused");
    }
}
