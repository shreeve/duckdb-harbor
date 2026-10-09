//! What a statement is, read from its text the way the engine reads it.
//!
//! A server deciding whether a statement may run outside a session, a server
//! reporting whether a session holds a transaction, and a client deciding
//! which statements belong on a session all ask the same question: which
//! keyword will the engine act on? They must give one answer, and it must be
//! the engine's, so the reading lives here, once, measured against the
//! engine: the spaces it skips, the comments it skips, what it takes for a
//! bare word, and the `EXPLAIN` that runs what it explains.

use crate::scan::space_len;

/// Advance `i` past whitespace and SQL comments: `--` to end of line, nested
/// `/* */`. The one skipper every reader of a statement's keywords shares.
pub fn skip_trivia(b: &[u8], i: &mut usize) {
    loop {
        while let n @ 1.. = space_len(&b[*i..]) {
            *i += n;
        }
        if b[*i..].starts_with(b"--") {
            // CR ends the comment too — see ensure_single_statement. The same
            // one-byte gap defeated the fleet-safety fence from the other
            // side: `SET --\r memory_limit='1TB'` looked like a bare `SET`
            // with a trailing comment here, so `fenced_setting` never saw the
            // key, while the engine set it. memory_limit is process-global,
            // so that is every neighbor berth's ceiling raised by one caller
            // — measured going from 1.8 GiB to 931.3 GiB.
            *i = b[*i..]
                .iter()
                .position(|&c| c == b'\n' || c == b'\r')
                .map_or(b.len(), |p| *i + p + 1);
        } else if b[*i..].starts_with(b"/*") {
            let mut depth = 1;
            *i += 2;
            while *i < b.len() && depth > 0 {
                if b[*i..].starts_with(b"/*") {
                    depth += 1;
                    *i += 2;
                } else if b[*i..].starts_with(b"*/") {
                    depth -= 1;
                    *i += 2;
                } else {
                    *i += 1;
                }
            }
        } else {
            break;
        }
    }
}

/// A bare keyword after trivia, uppercased: a run of what the engine takes
/// for an unquoted identifier (ASCII letters and digits, `_`, `$`, and
/// anything past ASCII that is not a space). A quoted identifier is a name
/// and never a keyword, so it reads as nothing, and so does punctuation;
/// `COMMIT$x` and `"BEGIN"` are table names to the engine and to this.
pub fn bare_word(b: &[u8], i: &mut usize) -> String {
    skip_trivia(b, i);
    let start = *i;
    while *i < b.len()
        && space_len(&b[*i..]) == 0
        && (b[*i].is_ascii_alphanumeric() || matches!(b[*i], b'_' | b'$') || b[*i] >= 0x80)
    {
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
        let mut depth = 0usize;
        while i < b.len() {
            match b[i] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                _ => {
                    let at = i;
                    analyze |= analyzes(&bare_word(b, &mut i));
                    if i > at {
                        continue;
                    }
                }
            }
            i += 1;
        }
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
            ("\"COMMIT\"", ""),
            ("COMMIT$x", "COMMIT$X"),
            ("(COMMIT)", ""),
            ("", ""),
        ] {
            assert_eq!(acting_keyword(sql), word, "{sql:?}");
        }
    }
}
