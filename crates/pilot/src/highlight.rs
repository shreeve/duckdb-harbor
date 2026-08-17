//! Live syntax highlighting (PLAN.md Phase 2, tier 1): a lexical pass, no
//! grammar. Colors follow the duckdb shell's defaults so muscle memory
//! transfers: keywords green, literals yellow, comments dim, unterminated
//! literals red. String/comment/dollar-quote boundaries come from the shared
//! scanner (scan.rs) — the same spans the validator and splitter obey.

use crate::keywords::KEYWORDS;
use crate::scan::{Kind, scan};
use nu_ansi_term::{Color, Style};
use reedline::{Highlighter, StyledText};

pub struct SqlHighlighter;

#[derive(Clone, Copy, PartialEq)]
enum Class {
    Plain,
    Keyword,
    Literal, // strings + numbers
    Comment,
    Error, // unterminated string/comment
}

fn style(c: Class) -> Style {
    match c {
        Class::Plain => Style::new(),
        Class::Keyword => Style::new().fg(Color::Green),
        Class::Literal => Style::new().fg(Color::Yellow),
        Class::Comment => Style::new().fg(Color::DarkGray),
        Class::Error => Style::new().fg(Color::Red),
    }
}

fn push(out: &mut StyledText, c: Class, s: &str) {
    if !s.is_empty() {
        out.push((style(c), s.to_string()));
    }
}

impl Highlighter for SqlHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut out = StyledText::new();
        for sp in scan(line) {
            let text = &line[sp.start..sp.end];
            match sp.kind {
                Kind::Str | Kind::Dollar => {
                    push(&mut out, if sp.terminated { Class::Literal } else { Class::Error }, text);
                }
                Kind::LineComment => push(&mut out, Class::Comment, text),
                Kind::BlockComment => {
                    push(&mut out, if sp.terminated { Class::Comment } else { Class::Error }, text);
                }
                Kind::Code => highlight_code(&mut out, text),
            }
        }
        out
    }
}

/// Words and numbers inside a code span. Char-wise, so multi-byte characters
/// (identifiers, pasted punctuation) never split a slice mid-char.
fn highlight_code(out: &mut StyledText, text: &str) {
    let mut i = 0;
    while i < text.len() {
        let start = i;
        let c = text[i..].chars().next().expect("i is a char boundary");
        if c.is_alphabetic() || c == '_' {
            while i < text.len() {
                let ch = text[i..].chars().next().expect("char boundary");
                if ch.is_alphanumeric() || ch == '_' {
                    i += ch.len_utf8();
                } else {
                    break;
                }
            }
            let word = &text[start..i];
            let class = if KEYWORDS.binary_search(&word.to_ascii_uppercase().as_str()).is_ok() {
                Class::Keyword
            } else {
                Class::Plain
            };
            push(out, class, word);
        } else if c.is_ascii_digit() {
            while i < text.len() {
                let ch = text[i..].chars().next().expect("char boundary");
                if ch.is_alphanumeric() || ch == '.' || ch == '_' {
                    i += ch.len_utf8();
                } else {
                    break;
                }
            }
            push(out, Class::Literal, &text[start..i]);
        } else {
            // A run of punctuation/whitespace up to the next word or number.
            while i < text.len() {
                let ch = text[i..].chars().next().expect("char boundary");
                if ch.is_alphabetic() || ch.is_ascii_digit() || ch == '_' {
                    break;
                }
                i += ch.len_utf8();
            }
            push(out, Class::Plain, &text[start..i]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(line: &str) -> Vec<(Option<Color>, String)> {
        SqlHighlighter
            .highlight(line, 0)
            .buffer
            .into_iter()
            .map(|(st, s)| (st.foreground, s))
            .collect()
    }

    #[test]
    fn classes_color_correctly() {
        let s = spans("SELECT 'hi' FROM t WHERE n > 42 -- note");
        let find = |txt: &str| s.iter().find(|(_, t)| t == txt).map(|(c, _)| *c).unwrap();
        assert_eq!(find("SELECT"), Some(Color::Green), "keyword");
        assert_eq!(find("FROM"), Some(Color::Green), "keyword");
        assert_eq!(find("'hi'"), Some(Color::Yellow), "string");
        assert_eq!(find("42"), Some(Color::Yellow), "number");
        assert_eq!(find("-- note"), Some(Color::DarkGray), "comment");
        assert_eq!(find("t"), None, "identifier stays plain");
        // unterminated string goes red
        let s = spans("SELECT 'oops");
        assert_eq!(s.last().unwrap().0, Some(Color::Red));
        // dollar-quotes are literals (the old scanner missed these)
        let s = spans("SELECT $t$ hi $t$;");
        assert_eq!(s.iter().find(|(_, t)| t == "$t$ hi $t$").unwrap().0, Some(Color::Yellow));
        // reconstruction: spans concatenate back to the source line
        let s = spans("SELECT /* x */ 1;");
        let joined: String = s.into_iter().map(|(_, t)| t).collect();
        assert_eq!(joined, "SELECT /* x */ 1;");
    }

    #[test]
    fn multibyte_never_panics_and_reconstructs() {
        // These inputs panicked the old byte-sliced highlighter.
        for line in ["SELECT tあ", "SELECT “x”", "SELECT ‘x’", "sélect café", "a€b"] {
            let joined: String = spans(line).into_iter().map(|(_, t)| t).collect();
            assert_eq!(joined, line);
        }
    }
}
