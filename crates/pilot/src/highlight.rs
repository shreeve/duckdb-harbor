//! Live syntax highlighting (PLAN.md Phase 2, tier 1): a lexical pass, no
//! grammar. Colors follow the duckdb shell's defaults so muscle memory
//! transfers: keywords green, literals yellow, comments dim, unterminated
//! literals red. The scanner classes here mirror repl::statement_complete;
//! both grow into the full sqllex port.

use crate::keywords::KEYWORDS;
use nu_ansi_term::{Color, Style};
use reedline::{Highlighter, StyledText};

pub struct SqlHighlighter;

#[derive(Clone, Copy, PartialEq)]
enum Class {
    Plain,
    Keyword,
    Literal,  // strings + numbers
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

impl Highlighter for SqlHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut out = StyledText::new();
        let b = line.as_bytes();
        let mut i = 0;
        let push = |out: &mut StyledText, c: Class, s: &str| {
            if !s.is_empty() {
                out.push((style(c), s.to_string()));
            }
        };
        while i < b.len() {
            let start = i;
            match b[i] {
                b'\'' | b'"' => {
                    let q = b[i];
                    i += 1;
                    let mut closed = false;
                    while i < b.len() {
                        if b[i] == q {
                            if i + 1 < b.len() && b[i + 1] == q {
                                i += 2;
                                continue;
                            }
                            i += 1;
                            closed = true;
                            break;
                        }
                        i += 1;
                    }
                    push(&mut out, if closed { Class::Literal } else { Class::Error }, &line[start..i]);
                }
                b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                    push(&mut out, Class::Comment, &line[start..]);
                    i = b.len();
                }
                b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
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
                    push(&mut out, if depth == 0 { Class::Comment } else { Class::Error }, &line[start..i]);
                }
                c if c.is_ascii_digit() => {
                    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_') {
                        i += 1;
                    }
                    push(&mut out, Class::Literal, &line[start..i]);
                }
                c if c.is_ascii_alphabetic() || c == b'_' => {
                    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                        i += 1;
                    }
                    let word = &line[start..i];
                    let class = if KEYWORDS.binary_search(&word.to_ascii_uppercase().as_str()).is_ok() {
                        Class::Keyword
                    } else {
                        Class::Plain
                    };
                    push(&mut out, class, word);
                }
                _ => {
                    i += 1;
                    push(&mut out, Class::Plain, &line[start..i]);
                }
            }
        }
        out
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
        // reconstruction: spans concatenate back to the source line
        let s = spans("SELECT /* x */ 1;");
        let joined: String = s.into_iter().map(|(_, t)| t).collect();
        assert_eq!(joined, "SELECT /* x */ 1;");
    }
}
