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
//! the term, so `x.{a,b}::DECIMAL(10,2)` casts both. Items
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
//! leaves the whole statement as it came.
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
/// An error when the expansion would outrun [`BUDGET`].
pub fn expand(sql: &str) -> Result<Cow<'_, str>, String> {
    if !sql.contains('{') {
        return Ok(Cow::Borrowed(sql));
    }
    let mut budget = BUDGET;
    let read = reading(sql);
    let text = Text { src: sql.as_bytes(), read: &read };
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
                b'}' if depth == 0 => return Ok(Cow::Borrowed(sql)),
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
            push_term(&mut out, text.slice(term_start, i), braced, &mut budget)?;
            out.push(text.slice(i, i + 1));
            term_start = i + 1;
            braced = false;
        }
        i += 1;
    }
    if depth != 0 {
        return Ok(Cow::Borrowed(sql));
    }
    push_term(&mut out, text.slice(term_start, text.len()), braced, &mut budget)?;
    if out.src == sql.as_bytes() {
        return Ok(Cow::Borrowed(sql));
    }
    // Every cut falls on an ASCII byte or a whole Unicode space, so the text
    // is UTF-8; and it goes out only if the engine reads it as it was built.
    match String::from_utf8(out.src) {
        Ok(expanded) if reading(&expanded) == out.read => Ok(Cow::Owned(expanded)),
        _ => Ok(Cow::Borrowed(sql)),
    }
}

fn push_term(out: &mut Piece, term: Text, braced: bool, budget: &mut usize) -> Result<(), String> {
    if !braced {
        out.push(term);
        return Ok(());
    }
    for (k, alt) in alternatives(term, budget)?.iter().enumerate() {
        if k > 0 {
            out.push(Text { src: b", ", read: &[CODE, CODE] });
        }
        out.push(alt.text());
    }
    Ok(())
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
        let items = text.items(lbrace + 1, rbrace);
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

    /// The items between `from` and `to`, split at top-level spaces, comments
    /// and commas; a nested group travels whole inside its item, as do a
    /// cast's type parameters.
    fn items(&self, from: usize, to: usize) -> Vec<Text<'a>> {
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
                    items.push(self.slice(start, i));
                }
                start = i + 1;
            }
            i += 1;
        }
        if to > start {
            items.push(self.slice(start, to));
        }
        items
    }
}

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
    /// read another way is not made.
    #[test]
    fn an_expansion_the_engine_would_misread_is_not_made() {
        for sql in [
            // `}E'` begins an E-string and `xE'` does not: read afresh, the
            // string after it would come out as `'a'` and `'b'`.
            r"select {x}E'\'''{a,b}'",
            // `$$` after a brace opens a dollar quote; after a word it does not.
            "select r.{a,b}$$x.{c,d}$$",
            // Two strings would run into one.
            "select {'a' x}'b'",
            // An unterminated string would take in the next alternative.
            "select {a b}'{",
            // The engine's pre-pass misreads `E'\''`, and repeated, the
            // misreading flips: the U+00A0 in the string after it would be
            // stripped where it was kept.
            "select {a,b}.E'\\'', 'x\u{a0}y'",
            // An `E` that begins a word makes the string after it take
            // backslash escapes, whether or not one moves its end: `}'` is a
            // plain string and `E'` is not, `d'` is plain and `e'` is not,
            // `}e'` takes escapes and `xe'` does not.
            r"select {E}'a\tb'",
            r"select t.{d,e}'a\tb'",
            r"select {x,y}e'a\tb'",
        ] {
            assert!(matches!(expand(sql), Ok(Cow::Borrowed(_))), "{sql:?}");
        }
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
        // the `AS` of a `CAST`, which it refuses too.
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
    /// engine should read that.
    fn reference(sql: &str) -> (Vec<u8>, Vec<Tok>) {
        let units = units(sql);
        let unchanged = || (sql.as_bytes().to_vec(), reading_of(&units));
        let mut out: Vec<Unit> = Vec::new();
        let (mut depth, mut term) = (0usize, Vec::new());
        let flush = |out: &mut Vec<Unit>, term: &mut Vec<Unit>| {
            let alts = if term.iter().any(|u| u.0 == Tok::Code(b'{')) { reference_alternatives(term) } else { vec![term.clone()] };
            for (k, alt) in alts.into_iter().enumerate() {
                if k > 0 {
                    out.push((Tok::Code(b','), b",".to_vec()));
                    out.push((Tok::Space, b" ".to_vec()));
                }
                out.extend(alt);
            }
            term.clear();
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
                flush(&mut out, &mut term);
                out.push(unit.clone());
            } else {
                term.push(unit.clone());
            }
            k += 1;
        }
        if depth != 0 {
            return unchanged();
        }
        flush(&mut out, &mut term);
        (out.iter().flat_map(|u| u.1.clone()).collect(), reading_of(&out))
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

    fn reference_alternatives(term: &[Unit]) -> Vec<Vec<Unit>> {
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
                None => return vec![term.to_vec()],
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
                items.push(Vec::new());
            } else {
                item.push(u.clone());
            }
            k += 1;
        }
        items.retain(|item| !item.is_empty());
        if items.is_empty() {
            return vec![term.to_vec()];
        }
        items
            .into_iter()
            .flat_map(|item| reference_alternatives(&[&term[..o], &item[..], &term[c + 1..]].concat()))
            .collect()
    }

    /// What a generated statement is made of: code, groups, the quotes the
    /// engine knows and the ones its pre-pass misreads, comments, and the
    /// spaces, ASCII and Unicode, that it strips and that it keeps.
    const FRAGMENTS: &[&str] = &[
        "select ", "r", ".", "x", "e", "E", "a1", "é", "1", "1e5", ".5", ",", "(", ")", ";", ":", "::", "$", "\\", "_",
        "{", "}", "{a,b}", "{a b}", "r.{a,b}", "{'k': 1}", "{}", "{a,{b,c}}", "{a::INT b}", "{x}",
        "::DECIMAL(10,2)", "::T(", "::S(a {b} c)", "{a::V(1, 2) b}", "::X(E'\\', \"(\")", "2(",
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
            let got = expand(&sql).unwrap();
            let (want, read) = reference(&sql);
            let want = String::from_utf8(want).unwrap();
            let ok = if want == sql {
                got == sql
            } else if got == sql {
                refused += 1;
                reading_of(&units(&want)) != read
            } else {
                expanded += 1;
                got == want && reading_of(&units(&got)) == read
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
