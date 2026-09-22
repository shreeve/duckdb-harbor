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
//! between whitespace, commas, parentheses and semicolons. Items in a group
//! are separated by commas; whitespace alone separates them too, so a comma
//! is never required, only clearer. A group nests; two groups in one term
//! multiply, as they do in a shell. The alternatives a term expands
//! to are joined with `, `, which is what a select list, a `FROM` list and an
//! argument list all take.
//!
//! DuckDB's own grammar uses braces in one place, the struct literal
//! `{'a': 1}`, and it always carries a lone `:` at its top level — one that
//! is not the `::` of a cast. Such a group is left exactly as it came, as is
//! an empty one. Strings, quoted identifiers, dollar quotes and comments are
//! never touched: the scanner that splits statements marks them. A brace
//! that never closes leaves the whole statement as it came, and the engine
//! reports the syntax error at it.
//!
//! Each group multiplies, so a short statement can stand for an enormous one:
//! thirty two-item groups in a term are a billion alternatives. Expansion
//! therefore has a budget, [`BUDGET`] bytes of text written or re-read, and a
//! statement that would spend more is refused rather than expanded.

use std::borrow::Cow;

use crate::repl::scan::{Kind, scan};

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
    let text = Text::of(sql);
    let mut out = String::with_capacity(sql.len() + 64);
    let mut term_start = 0;
    let mut depth = 0usize;
    let mut braced = false;
    let mut i = 0;
    while i < sql.len() {
        if let Some(end) = text.opaque_end(i) {
            i = end;
            continue;
        }
        let c = sql.as_bytes()[i];
        match c {
            b'{' => {
                depth += 1;
                braced = true;
            }
            b'}' => {
                if depth == 0 {
                    return Ok(Cow::Borrowed(sql));
                }
                depth -= 1;
            }
            _ if depth == 0 && is_boundary(c) => {
                push_term(&mut out, &sql[term_start..i], braced, &mut budget)?;
                out.push(c as char);
                term_start = i + 1;
                braced = false;
            }
            _ => {}
        }
        i += 1;
    }
    if depth != 0 {
        return Ok(Cow::Borrowed(sql));
    }
    push_term(&mut out, &sql[term_start..], braced, &mut budget)?;
    Ok(if out == sql { Cow::Borrowed(sql) } else { Cow::Owned(out) })
}

fn is_boundary(c: u8) -> bool {
    c.is_ascii_whitespace() || matches!(c, b',' | b'(' | b')' | b';')
}

fn push_term(out: &mut String, term: &str, braced: bool, budget: &mut usize) -> Result<(), String> {
    if !braced {
        out.push_str(term);
        return Ok(());
    }
    let mut first = true;
    for alt in alternatives(term, budget)? {
        if !first {
            out.push_str(", ");
        }
        first = false;
        out.push_str(&alt);
    }
    Ok(())
}

/// Every string a term stands for, in order. A term with no group stands for
/// itself. One group is resolved at a time, depth first, from a stack rather
/// than by recursion: a term can hold thousands of groups, one item each,
/// that multiply nothing and would each be a frame. Each string is charged
/// its length before it is built, since it is built once, scanned once and
/// written at most once: a group of many items behind a long prefix is
/// refused before its alternatives take any memory.
fn alternatives(term: &str, budget: &mut usize) -> Result<Vec<String>, String> {
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
    let mut pending = vec![term.to_string()];
    while let Some(term) = pending.pop() {
        let text = Text::of(&term);
        let Some((lbrace, rbrace)) = text.first_group() else {
            out.push(term);
            continue;
        };
        let items = text.items(lbrace + 1, rbrace);
        if items.is_empty() {
            // `x.{}` stands for nothing; the engine can say so.
            out.push(term);
            continue;
        }
        let (prefix, suffix) = (&term[..lbrace], &term[rbrace + 1..]);
        let each = prefix.len() + suffix.len();
        spend(items.iter().fold(0usize, |sum, item| sum.saturating_add(each + item.len())))?;
        let next: Vec<String> = items.iter().rev().map(|item| format!("{prefix}{item}{suffix}")).collect();
        pending.extend(next);
    }
    Ok(out)
}

/// A piece of SQL with its opaque spans — strings, quoted identifiers, dollar
/// quotes, comments — marked, so a walk over it steps past them whole.
struct Text<'a> {
    src: &'a str,
    /// For a byte that begins an opaque span, the byte after the span.
    ends: Vec<usize>,
}

impl<'a> Text<'a> {
    fn of(src: &'a str) -> Self {
        let mut ends = vec![0; src.len()];
        for sp in scan(src) {
            if sp.kind != Kind::Code {
                // An unterminated string runs to the end of the text, which
                // hides any brace after it — as it hides everything else.
                ends[sp.start] = sp.end.max(sp.start + 1);
            }
        }
        Text { src, ends }
    }

    fn opaque_end(&self, i: usize) -> Option<usize> {
        match self.ends[i] {
            0 => None,
            end => Some(end),
        }
    }

    /// The first group that expands: `(lbrace, rbrace)`. A struct literal and
    /// an empty pair are stepped over, as is anything nested in them.
    fn first_group(&self) -> Option<(usize, usize)> {
        let b = self.src.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if let Some(end) = self.opaque_end(i) {
                i = end;
                continue;
            }
            if b[i] == b'{' {
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
        let b = self.src.as_bytes();
        let mut depth = 0usize;
        let mut i = open;
        while i < b.len() {
            if let Some(end) = self.opaque_end(i) {
                i = end;
                continue;
            }
            match b[i] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        None
    }

    /// A lone `:` at the top level of the group — `{'a': 1}`, never `::`.
    fn is_struct_literal(&self, from: usize, to: usize) -> bool {
        let b = self.src.as_bytes();
        let mut depth = 0usize;
        let mut i = from;
        while i < to {
            if let Some(end) = self.opaque_end(i) {
                i = end;
                continue;
            }
            match b[i] {
                b'{' => depth += 1,
                b'}' => depth = depth.saturating_sub(1),
                b':' if depth == 0 => {
                    let before = i > from && b[i - 1] == b':';
                    let after = i + 1 < to && b[i + 1] == b':';
                    if !before && !after {
                        return true;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        false
    }

    /// The items between `from` and `to`, split at top-level whitespace and
    /// commas; a nested group travels whole inside its item.
    fn items(&self, from: usize, to: usize) -> Vec<&'a str> {
        let b = self.src.as_bytes();
        let mut items = Vec::new();
        let mut depth = 0usize;
        let mut start = from;
        let mut i = from;
        let push = |items: &mut Vec<&'a str>, s: usize, e: usize| {
            if e > s {
                items.push(&self.src[s..e]);
            }
        };
        while i < to {
            if let Some(end) = self.opaque_end(i) {
                i = end;
                continue;
            }
            match b[i] {
                b'{' => depth += 1,
                b'}' => depth = depth.saturating_sub(1),
                c if depth == 0 && (c.is_ascii_whitespace() || c == b',') => {
                    push(&mut items, start, i);
                    start = i + 1;
                }
                _ => {}
            }
            i += 1;
        }
        push(&mut items, start, to);
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
}
