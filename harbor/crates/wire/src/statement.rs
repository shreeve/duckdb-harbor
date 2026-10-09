//! What a statement is, read from its text the way the engine reads it.
//!
//! A server deciding whether a statement may run outside a session, a server
//! reporting whether a session holds a transaction, and a client deciding
//! which statements belong on a session all ask the same question: which
//! keyword will the engine act on? They must give one answer, and it must be
//! the engine's, so the reading lives here, once, measured against the
//! engine: the spaces it skips, the comments it skips, what it takes for a
//! bare word, and the `EXPLAIN` that runs what it explains.

use crate::scan::{Kind, comment_at, is_word_byte, scan, space_len};

/// Advance `i` past whitespace and SQL comments, read by [`crate::scan`]. The
/// one skipper every reader of a statement's keywords shares.
pub fn skip_trivia(b: &[u8], i: &mut usize) {
    loop {
        while let n @ 1.. = space_len(&b[*i..]) {
            *i += n;
        }
        match comment_at(b, *i) {
            Some(comment) => *i = comment.end,
            None => break,
        }
    }
}

/// A bare keyword after trivia, uppercased: a run of what the engine keeps
/// in an unquoted word (anything but ASCII punctuation and the spaces, with
/// `_` and `$` let in; see [`crate::scan`]). A quoted identifier is a name
/// and never a keyword, so it reads as nothing, and so does punctuation;
/// `COMMIT$x` and `"BEGIN"` are table names to the engine and to this.
pub fn bare_word(b: &[u8], i: &mut usize) -> String {
    skip_trivia(b, i);
    let start = *i;
    while *i < b.len() && space_len(&b[*i..]) == 0 && is_word_byte(b[*i]) {
        *i += 1;
    }
    String::from_utf8_lossy(&b[start..*i]).to_ascii_uppercase()
}

/// The keyword the engine acts on: a statement's first, or the first of the
/// statement an analyzed `EXPLAIN` wraps, since analyzing a statement runs
/// it. `EXPLAIN ANALYZE COMMIT` commits. The engine takes ANALYZE as the
/// word after EXPLAIN or as an option in the list that follows, whatever
/// value the option is given, and takes a list after the word too; an
/// `EXPLAIN` without it only plans.
pub fn acting_keyword(sql: &str) -> String {
    let b = sql.as_bytes();
    let mut i = 0;
    let word = bare_word(b, &mut i);
    if word != "EXPLAIN" {
        return word;
    }
    let analyzes = |w: &str| matches!(w, "ANALYZE" | "ANALYSE");
    let at = i;
    let mut analyze = analyzes(&bare_word(b, &mut i));
    if !analyze {
        i = at;
    }
    skip_trivia(b, &mut i);
    if b.get(i) == Some(&b'(') {
        // The list is read in its code alone: a parenthesis or a word in an
        // option's quoted value is part of the value. `(ANALYZE 'x)')` is
        // one option, and the statement after it runs.
        let list = &sql[i..];
        let mut end = list.len();
        let mut depth = 0usize;
        'list: for span in scan(list).into_iter().filter(|s| s.kind == Kind::Code) {
            let code = &list.as_bytes()[..span.end];
            let mut j = span.start;
            while j < span.end {
                match code[j] {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = j + 1;
                            break 'list;
                        }
                    }
                    _ => {
                        let at = j;
                        analyze |= analyzes(&bare_word(code, &mut j));
                        if j > at {
                            continue;
                        }
                    }
                }
                j += 1;
            }
        }
        i += end;
    }
    if analyze { bare_word(b, &mut i) } else { word }
}

/// What a statement does to the surrounding transaction, when that is knowable
/// from its first word: `Some(true)` opens one, `Some(false)` ends one, `None`
/// leaves it as it was. Used to report whether a lease is holding a
/// transaction open, which is the thing an operator most needs to see.
pub fn transaction_effect(sql: &str) -> Option<bool> {
    match acting_keyword(sql).as_str() {
        "BEGIN" | "START" => Some(true),
        "COMMIT" | "END" | "ROLLBACK" | "ABORT" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spaces are the engine's list and no wider: a space it does not
    /// skip is part of a word to it, and the word is then no keyword.
    #[test]
    fn a_keyword_is_read_past_the_spaces_the_engine_skips_and_no_others() {
        for space in ["\u{a0}", "\u{2000}", "\u{200b}", "\u{202f}", "\u{205f}", "\u{2060}", "\u{3000}", "\u{feff}", "\u{b}", "\t\r\n"] {
            assert_eq!(acting_keyword(&format!("{space}COMMIT")), "COMMIT", "{space:?}");
            assert_eq!(transaction_effect(&format!("COMMIT{space}")), Some(false), "{space:?}");
        }
        for not_a_space in ["\u{85}", "\u{1680}", "\u{2028}", "\u{2029}"] {
            assert_eq!(transaction_effect(&format!("{not_a_space}COMMIT")), None, "{not_a_space:?}");
        }
    }

    #[test]
    fn the_acting_keyword_is_the_one_the_engine_runs() {
        for (sql, word) in [
            ("begin", "BEGIN"),
            ("/* a */ -- b\n START TRANSACTION", "START"),
            ("EXPLAIN ANALYZE COMMIT", "COMMIT"),
            ("EXPLAIN (ANALYZE, FORMAT JSON) ROLLBACK", "ROLLBACK"),
            ("EXPLAIN ANALYZE (FORMAT JSON) COMMIT", "COMMIT"),
            ("EXPLAIN COMMIT", "EXPLAIN"),
            ("EXPLAIN (FORMAT JSON) ANALYZE COMMIT", "EXPLAIN"),
            ("EXPLAIN (ANALYZE 'x)') COMMIT", "COMMIT"),
            ("EXPLAIN (ANALYZE, FORMAT \"a)\") COMMIT", "COMMIT"),
            ("EXPLAIN (ANALYZE $t$)$t$, FORMAT JSON) COMMIT", "COMMIT"),
            ("EXPLAIN (ANALYZE /* ) */) COMMIT", "COMMIT"),
            ("EXPLAIN (FORMAT 'analyze') COMMIT", "EXPLAIN"),
            ("EXPLAIN (FORMAT /* analyze */ JSON) COMMIT", "EXPLAIN"),
            ("EXPLAIN (ANALYZE 'x)'", ""),
            ("-- c\rCOMMIT", "COMMIT"),
            ("\"COMMIT\"", ""),
            ("COMMIT$x", "COMMIT$X"),
            ("(COMMIT)", ""),
            ("", ""),
        ] {
            assert_eq!(acting_keyword(sql), word, "{sql:?}");
        }
    }
}
