//! The one SQL surface scanner (the seed of sqllex, PLAN.md Phase 4).
//!
//! Everything that must agree on where strings, comments, and dollar-quotes
//! begin and end — the Enter-submits validator, the multi-statement splitter,
//! the highlighter — consumes these spans. One automaton, one set of rules:
//! `''`/`""` escapes, `$tag$` dollar-quoting (not opened mid-identifier, so
//! `a$b$c` stays an identifier), `--` line comments, nested `/* */`.
//!
//! Span boundaries fall only on ASCII delimiter bytes, so slicing the source
//! at them is UTF-8-safe by construction; multi-byte characters always land
//! whole inside a span.

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Code,
    Str,    // 'single' or "double" quoted
    Dollar, // $tag$ ... $tag$
    LineComment,
    BlockComment,
}

#[derive(Clone, Copy, Debug)]
pub struct Span {
    pub start: usize,
    pub end: usize, // exclusive
    pub kind: Kind,
    pub terminated: bool,
}

pub fn scan(src: &str) -> Vec<Span> {
    let b = src.as_bytes();
    let mut spans = Vec::new();
    let mut code_start = 0;
    let mut i = 0;
    // Was the previous byte identifier-ish? Guards the `$` opener: DuckDB
    // allows `$` inside identifiers, so `a$b$c` must not start a quote.
    // Multi-byte chars count as identifier-ish for the same reason.
    let mut prev_ident = false;
    let flush = |spans: &mut Vec<Span>, from: usize, to: usize| {
        if to > from {
            spans.push(Span { start: from, end: to, kind: Kind::Code, terminated: true });
        }
    };
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"') => {
                flush(&mut spans, code_start, i);
                let start = i;
                i += 1;
                let mut terminated = false;
                while i < b.len() {
                    if b[i] == q {
                        if i + 1 < b.len() && b[i + 1] == q {
                            i += 2; // '' or "" escape
                            continue;
                        }
                        i += 1;
                        terminated = true;
                        break;
                    }
                    i += 1;
                }
                spans.push(Span { start, end: i, kind: Kind::Str, terminated });
                code_start = i;
                prev_ident = false;
            }
            b'$' if !prev_ident => {
                let start = i;
                let mut j = i + 1;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                    j += 1;
                }
                if j < b.len() && b[j] == b'$' {
                    flush(&mut spans, code_start, i);
                    let tag = &src[start..=j];
                    let (end, terminated) = match src[j + 1..].find(tag) {
                        Some(pos) => (j + 1 + pos + tag.len(), true),
                        None => (b.len(), false),
                    };
                    spans.push(Span { start, end, kind: Kind::Dollar, terminated });
                    code_start = end;
                    i = end;
                    prev_ident = false;
                } else {
                    // A lone `$` (e.g. a $1 parameter): plain code.
                    i += 1;
                    prev_ident = true;
                }
            }
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                flush(&mut spans, code_start, i);
                let start = i;
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                spans.push(Span { start, end: i, kind: Kind::LineComment, terminated: true });
                code_start = i;
                prev_ident = false;
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                flush(&mut spans, code_start, i);
                let start = i;
                let mut depth = 1;
                i += 2;
                while i < b.len() && depth > 0 {
                    if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                    } else if i + 1 < b.len() && b[i] == b'*' && b[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                spans.push(Span { start, end: i, kind: Kind::BlockComment, terminated: depth == 0 });
                code_start = i;
                prev_ident = false;
            }
            c => {
                prev_ident = c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80;
                i += 1;
            }
        }
    }
    flush(&mut spans, code_start, b.len());
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<(Kind, &str, bool)> {
        scan(src).iter().map(|s| (s.kind, &src[s.start..s.end], s.terminated)).collect()
    }

    #[test]
    fn spans_cover_and_classify() {
        assert_eq!(
            kinds("SELECT 'a''b' -- c\n/* d /* e */ */ $t$;$t$ x"),
            vec![
                (Kind::Code, "SELECT ", true),
                (Kind::Str, "'a''b'", true),
                (Kind::Code, " ", true),
                (Kind::LineComment, "-- c", true),
                (Kind::Code, "\n", true),
                (Kind::BlockComment, "/* d /* e */ */", true),
                (Kind::Code, " ", true),
                (Kind::Dollar, "$t$;$t$", true),
                (Kind::Code, " x", true),
            ]
        );
    }

    #[test]
    fn unterminated_marks() {
        assert_eq!(kinds("'oops").last().unwrap().2, false);
        assert_eq!(kinds("/* oops").last().unwrap().2, false);
        assert_eq!(kinds("$$ oops").last().unwrap().2, false);
    }

    #[test]
    fn dollar_inside_identifier_is_not_a_quote() {
        // a$b$c is one identifier in DuckDB, not `a` + dollar-quote `$b$...`.
        assert!(scan("SELECT a$b$c;").iter().all(|s| s.kind == Kind::Code));
        // ...but a $tag$ after a delimiter still opens one.
        assert_eq!(kinds("SELECT $b$;$b$")[1], (Kind::Dollar, "$b$;$b$", true));
    }

    #[test]
    fn multibyte_stays_whole() {
        // Non-ASCII anywhere must never split a span mid-char (slicing safety).
        for src in ["SELECT tあ", "SELECT “x”", "SELECT 'あ;'; -- あ", "あ$b$c"] {
            for s in scan(src) {
                assert!(src.is_char_boundary(s.start) && src.is_char_boundary(s.end), "{src:?}");
            }
        }
    }
}
