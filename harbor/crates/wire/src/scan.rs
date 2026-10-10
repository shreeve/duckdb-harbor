//! The one SQL lexer: where strings, quoted names, dollar quotes and comments
//! begin and end, read the way the engine's tokenizer reads them.
//!
//! Harbor's brace expansion, the REPL's Enter validator, statement splitter
//! and highlighter, backup's reading of the SQL it generates, and the keyword
//! reader in [`crate::statement`] all take their boundaries from here, so no
//! two of them can disagree about where a literal ends. The rules, each
//! measured against the engine: `''` and `""` escape a quote; a `'` string
//! takes backslash escapes when an `E` begins the word before it; `--` runs
//! to LF or CR; `/* */` nests; `$tag$` quotes, where the tag is empty or an
//! ASCII letter, `_` or non-ASCII byte followed by those and digits, and opens
//! only between words, since `$` continues a word (`a$b$c` is one
//! identifier) and a digit after it makes a parameter (`$1`); a number ends
//! where the engine's tokenizer ends one, so what follows it begins afresh.
//! Words end at the Unicode spaces the engine turns into spaces before it
//! parses, found where its pre-pass finds them ([`plain_spaces`]).
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

/// A space to the engine's tokenizer: the ASCII set with the vertical tab.
pub(crate) fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t'..=b'\r')
}

/// `b` with the Unicode spaces the engine turns into spaces before it parses
/// (U+00A0, U+2000 to U+200B, U+202F, U+205F, U+2060, U+3000 and U+FEFF)
/// turned into ASCII spaces, byte for byte, so every offset still names the
/// same place in the text. Found as the engine's pre-pass finds them
/// (`Parser::StripUnicodeSpaces`), quirks and all: its lexer knows `''` but
/// not an E-string's backslash, opens a quote inside a block comment and a
/// dollar quote inside a word, closes `$t$` on the `$` that opened it when a
/// `t$` follows, and never looks at the last two bytes. Wherever it leaves
/// one, the tokenizer reads its bytes as part of a word, and so does this
/// lexer: a reader that knew fewer spaces than the engine would take a
/// statement behind one for part of a word, and one that knew more would
/// end a word the engine keeps going.
pub(crate) fn plain_spaces(b: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    let lead = |c: &u8| matches!(c, 0xC2 | 0xE2 | 0xE3 | 0xEF);
    if !b.iter().any(lead) {
        return b.into();
    }
    let (n, mut out, mut pos) = (b.len(), b.to_vec(), 0);
    let tag_first = |c: u8| c.is_ascii_alphabetic() || c == b'_' || c >= 0x80;
    'regular: while pos + 2 < n {
        let width = match b[pos..pos + 3] {
            [0xC2, 0xA0, _] => 2,
            [0xE2, 0x80, 0x80..=0x8B | 0xAF] | [0xE2, 0x81, 0x9F | 0xA0] | [0xE3, 0x80, 0x80] | [0xEF, 0xBB, 0xBF] => 3,
            _ => 0,
        };
        out[pos..pos + width].fill(b' ');
        match b[pos] {
            q @ (b'\'' | b'"') => {
                pos += 1;
                while pos + 1 < n {
                    if b[pos] == q && b[pos + 1] != q {
                        pos += 1;
                        continue 'regular;
                    }
                    pos += if b[pos] == q { 2 } else { 1 };
                }
                break;
            }
            b'$' if b[pos + 1] == b'$' || tag_first(b[pos + 1]) => {
                pos += 1;
                let start = pos;
                let tag = loop {
                    if pos + 2 >= n {
                        break 'regular;
                    }
                    match b[pos] {
                        b'$' => break &b[start..pos],
                        c if tag_first(c) || c.is_ascii_digit() => pos += 1,
                        _ => continue 'regular,
                    }
                };
                // The search starts on the `$` that ended the tag, and the
                // scan resumes on the `$` that ends the close.
                while pos + 2 < n {
                    if b[pos] == b'$' && b[pos + 1..].starts_with(tag) && b.get(pos + 1 + tag.len()) == Some(&b'$') {
                        pos += tag.len() + 1;
                        continue 'regular;
                    }
                    pos += 1;
                }
                break;
            }
            b'-' if b[pos + 1] == b'-' => match b[pos..].iter().position(|&c| c == b'\n' || c == b'\r') {
                Some(end) => pos += end,
                None => break,
            },
            _ => pos += 1,
        }
    }
    out.into()
}

/// A byte the engine's tokenizer keeps inside a bare word: anything but ASCII
/// punctuation and the ASCII spaces, with `_` and `$` let in. Non-ASCII bytes
/// are word bytes.
pub(crate) fn is_word_byte(c: u8) -> bool {
    matches!(c, b'_' | b'$') || !(c.is_ascii_punctuation() || is_space(c))
}

/// Where the number that starts at `s` (a digit, or a `.`) ends, as the
/// engine's tokenizer reads one: digits and `.`, a `_` before a digit, an
/// `e` once a digit has been seen and not right after another `e`, and a
/// sign right after the `e`. Whatever it took past its last digit or `.` it
/// gives back, so in `1e5e'x'` the number is `1e5` and `e'x'` an E-string.
fn number_end(b: &[u8], s: usize) -> usize {
    let e = |c: u8| (c | 0x20) == b'e';
    let mut i = s + 1;
    while let Some(&c) = b.get(i) {
        let more = match c {
            b'0'..=b'9' | b'.' => true,
            b'_' => b.get(i + 1).is_some_and(|&d| d.is_ascii_digit() || d == b'.'),
            b'+' | b'-' => e(b[i - 1]),
            c if e(c) => !e(b[i - 1]) && (b[s].is_ascii_digit() || b[i - 1].is_ascii_digit()),
            _ => false,
        };
        if !more {
            break;
        }
        i += 1;
    }
    if i < b.len() {
        while !(b[i - 1].is_ascii_digit() || b[i - 1] == b'.') {
            i -= 1;
        }
    }
    i
}

/// The comment that opens at `i`, if one does.
pub(crate) fn comment_at(b: &[u8], i: usize) -> Option<Span> {
    let (kind, end, terminated) = match b.get(i..i + 2)? {
        b"--" => {
            // CR ends the comment as well as LF, as it does for the engine.
            let end = b[i..].iter().position(|&c| c == b'\n' || c == b'\r').map_or(b.len(), |p| i + p);
            (Kind::LineComment, end, true)
        }
        b"/*" => {
            let (mut j, mut depth) = (i + 2, 1);
            while j < b.len() && depth > 0 {
                match &b[j..] {
                    [b'/', b'*', ..] => (depth, j) = (depth + 1, j + 2),
                    [b'*', b'/', ..] => (depth, j) = (depth - 1, j + 2),
                    _ => j += 1,
                }
            }
            (Kind::BlockComment, j, depth == 0)
        }
        _ => return None,
    };
    Some(Span { start: i, end, kind, terminated })
}

/// The string, quoted name or dollar quote that opens at `i`, if one does.
/// `word` is where the bare word that `i` would continue began: a `$` inside
/// a word opens nothing, and a `'` after an `E` that began one opens a string
/// that takes backslash escapes. The `e` that ends `LIKE'` or `date'` began no
/// word, so the backslash in `LIKE'\'` is data.
fn quote_at(b: &[u8], i: usize, word: Option<usize>) -> Option<Span> {
    let (kind, end, terminated) = match b[i] {
        q @ (b'\'' | b'"') => {
            let escapes = q == b'\'' && i > 0 && word == Some(i - 1) && (b[i - 1] | 0x20) == b'e';
            let mut j = i + 1;
            loop {
                match b.get(j) {
                    None => break (Kind::Str, j, false),
                    Some(b'\\') if escapes && j + 1 < b.len() => j += 2,
                    Some(&c) if c == q && b.get(j + 1) == Some(&q) => j += 2,
                    Some(&c) if c == q => break (Kind::Str, j + 1, true),
                    Some(_) => j += 1,
                }
            }
        }
        b'$' if word.is_none() && !b.get(i + 1).is_some_and(u8::is_ascii_digit) => {
            let tag_byte = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80;
            let close = i + 1 + b[i + 1..].iter().position(|&c| !tag_byte(c))?;
            if b[close] != b'$' {
                return None;
            }
            // The tag holds no `$`, so a failed comparison at one `$` never
            // reaches past the next: the search stays linear in the body.
            let tag = &b[i..=close];
            match (close + 1..b.len()).find(|&j| b[j..].starts_with(tag)) {
                Some(j) => (Kind::Dollar, j + tag.len(), true),
                None => (Kind::Dollar, b.len(), false),
            }
        }
        _ => return None,
    };
    Some(Span { start: i, end, kind, terminated })
}

/// `src` cut into spans that cover it end to end, in order.
pub fn scan(src: &str) -> Vec<Span> {
    spans(&plain_spaces(src.as_bytes()))
}

/// The spans of text whose spaces are already plain ([`plain_spaces`]).
pub(crate) fn spans(b: &[u8]) -> Vec<Span> {
    let (mut spans, mut code, mut i) = (Vec::new(), 0, 0);
    // Where the bare word the scan is in began, if it is in one.
    let mut word = None;
    while i < b.len() {
        if let Some(span) = comment_at(b, i).or_else(|| quote_at(b, i, word)) {
            if i > code {
                spans.push(Span { start: code, end: i, kind: Kind::Code, terminated: true });
            }
            spans.push(span);
            (i, code, word) = (span.end, span.end, None);
            continue;
        }
        // A number begins at a `.` anywhere and at a digit outside a word,
        // and ends no word but the one a `.` ends: what follows it is read
        // afresh, so `1$t$x$t$` is a number and a quote.
        if b[i] == b'.' || (word.is_none() && b[i].is_ascii_digit()) {
            (i, word) = (number_end(b, i), None);
            continue;
        }
        word = match b[i] {
            // A `$` continues a word but begins none: `$1` is a parameter.
            b'$' => word,
            c if is_word_byte(c) => word.or(Some(i)),
            _ => None,
        };
        i += 1;
    }
    if b.len() > code {
        spans.push(Span { start: code, end: b.len(), kind: Kind::Code, terminated: true });
    }
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
        assert!(!kinds("'oops").last().unwrap().2);
        assert!(!kinds("/* oops").last().unwrap().2);
        assert!(!kinds("$$ oops").last().unwrap().2);
    }

    /// A digit after `$` makes a bind parameter, never a tag. What follows
    /// the parameter is read afresh, as the engine reads it: in
    /// `SELECT $1$abc$1$` the quote opens at `$abc$` and never closes.
    #[test]
    fn a_digit_led_tag_is_a_parameter() {
        assert!(scan("SELECT $1$").iter().all(|s| s.kind == Kind::Code && s.terminated));
        assert!(scan("SELECT $1$; SELECT $1$").iter().all(|s| s.kind == Kind::Code));
        assert_eq!(kinds("SELECT $1$abc$1$"), vec![
            (Kind::Code, "SELECT $1", true),
            (Kind::Dollar, "$abc$1$", false),
        ]);
        // a letter- or underscore-led tag opens one, and may hold digits
        assert_eq!(kinds("SELECT $t1$;$t1$")[1], (Kind::Dollar, "$t1$;$t1$", true));
        assert_eq!(kinds("SELECT $_$;$_$")[1], (Kind::Dollar, "$_$;$_$", true));
    }

    #[test]
    fn dollar_inside_identifier_is_not_a_quote() {
        // a$b$c is one identifier to the engine, not `a` + dollar-quote `$b$...`.
        assert!(scan("SELECT a$b$c;").iter().all(|s| s.kind == Kind::Code));
        // ...but a $tag$ after a delimiter still opens one.
        assert_eq!(kinds("SELECT $b$;$b$")[1], (Kind::Dollar, "$b$;$b$", true));
        // A word is whatever the engine keeps in one: a non-ASCII letter or a
        // control byte joins it, so the `;` here is a terminator and the
        // quote after it never closes.
        for src in ["SELECT 1 AS é$t$;$t$", "SELECT 1 AS x\u{1}$t$;$t$", "SELECT a1$t$;$t$"] {
            let k = kinds(src);
            assert_eq!(k[1], (Kind::Dollar, "$t$", false), "{src:?}");
        }
    }

    /// A number or a parameter is not a word, so a tag right after one opens
    /// a quote, as the engine reads `1$t$;$t$`.
    #[test]
    fn a_tag_after_a_number_opens_a_quote() {
        assert_eq!(kinds("SELECT 1$t$;$t$")[1], (Kind::Dollar, "$t$;$t$", true));
        assert_eq!(kinds("SELECT $1$t$;$t$")[1], (Kind::Dollar, "$t$;$t$", true));
    }

    /// The engine takes any non-ASCII byte into a tag: `$é$a;b$é$` is the
    /// string `a;b`, one statement, not two.
    #[test]
    fn a_tag_may_be_non_ascii() {
        assert_eq!(kinds("SELECT $é$a;b$é$ z")[1], (Kind::Dollar, "$é$a;b$é$", true));
        assert_eq!(kinds("SELECT $t\u{2000}$a;b$t\u{2000}$")[1], (Kind::Dollar, "$t\u{2000}$a;b$t\u{2000}$", true));
        assert_eq!(kinds("SELECT $é$a;b$e$")[1], (Kind::Dollar, "$é$a;b$e$", false));
    }

    /// The engine turns its Unicode spaces into spaces before it reads a
    /// word, so one ends the word before it and a tag after it opens a quote.
    #[test]
    fn a_unicode_space_ends_a_word() {
        assert_eq!(kinds("SELECT x\u{a0}$t$a;b$t$")[1], (Kind::Dollar, "$t$a;b$t$", true));
        assert_eq!(kinds("SELECT x\u{3000}$t$a;b$t$")[1], (Kind::Dollar, "$t$a;b$t$", true));
    }

    /// Only where the engine's pre-pass finds one: elsewhere its bytes are
    /// part of a word, and the `$` after it opens nothing. Each case is
    /// measured against the engine, which reads two statements in each.
    #[test]
    fn a_unicode_space_is_one_only_where_the_engine_strips_it() {
        let plain = |s: &str| plain_spaces(s.as_bytes()).into_owned();
        // The pre-pass never looks at the last two bytes.
        assert_eq!(plain("COMMIT\u{a0}"), "COMMIT\u{a0}".as_bytes());
        assert_eq!(plain("COMMIT\u{3000}"), b"COMMIT   ");
        assert_eq!(plain("\u{feff}BEGIN"), b"   BEGIN");
        for src in [
            // A quote inside a block comment opens one for it,
            "SELECT 1 /* ' */, x\u{a0}$t$;SELECT 2;--$t$",
            // a backslash in an E-string escapes nothing,
            "SELECT E'\\'', x\u{a0}$t$;SELECT 2;--$t$",
            // and a dollar quote opens inside a word.
            "SELECT a$b$ AS c, x\u{a0}$t$;SELECT 2;--$t$",
        ] {
            assert!(kinds(src).iter().all(|k| k.0 != Kind::Dollar), "{src:?}: {:?}", kinds(src));
            assert_eq!(plain(src), src.as_bytes(), "{src:?}");
        }
        // A `$t$` closes on the `$` that ended its tag when `t$` follows,
        // and the scan goes on from the closing `$`.
        assert_eq!(plain("$t$t$ x\u{a0}y "), b"$t$t$ x  y ");
    }

    /// A number ends where the engine's tokenizer ends it, an exponent or a
    /// `_` included, so a quote right after it opens, and brace expansion
    /// leaves the string after it alone.
    #[test]
    fn a_number_ends_where_the_engine_ends_it() {
        for src in [r"SELECT 1_0e'\'' , '{a,b}' AS y", r"SELECT 1e5e'\'' , '{a,b}' AS y", r"SELECT .5e'\'' , '{a,b}'"] {
            let k = kinds(src);
            assert_eq!((k[1], k[3]), ((Kind::Str, r"'\''", true), (Kind::Str, "'{a,b}'", true)), "{src:?}");
        }
        for src in ["SELECT 1_0$t$;$t$", "SELECT 1e5$t$;$t$", "SELECT 1e-5$t$;$t$", "SELECT 1.$t$;$t$"] {
            assert_eq!(kinds(src)[1], (Kind::Dollar, "$t$;$t$", true), "{src:?}");
        }
        // What it took past its last digit it gives back: the `e` begins a
        // word, and its quote is an E-string.
        assert_eq!(kinds(r"SELECT 1e+e'\'';'")[1], (Kind::Str, r"'\''", true));
        // Inside a word a digit is the word's: `a1e'\'` is a plain string.
        assert_eq!(kinds(r"SELECT a1e'\'")[1], (Kind::Str, r"'\'", true));
    }

    /// CR ends a `--` comment for the engine, so it ends one here too: the
    /// validator, the splitter, the highlighter and the server's brace
    /// expansion all read these spans.
    #[test]
    fn cr_ends_a_line_comment() {
        assert_eq!(
            kinds("SELECT 1 --c\r; SELECT 2"),
            vec![
                (Kind::Code, "SELECT 1 ", true),
                (Kind::LineComment, "--c", true),
                (Kind::Code, "\r; SELECT 2", true),
            ]
        );
        // CRLF: the comment stops at the CR, and both bytes stay in code.
        assert_eq!(kinds("-- c\r\nSELECT 1")[0], (Kind::LineComment, "-- c", true));
        // Other line breaks do not end one, for the engine or here.
        assert_eq!(kinds("-- c\u{b}; SELECT 2\u{c}; x").len(), 1);
    }

    #[test]
    fn escape_strings() {
        // E'\'' is a complete escape string (the span starts at the quote;
        // the E prefix stays code). Without the prefix, \ is literal.
        assert_eq!(kinds(r"SELECT E'\''"), vec![
            (Kind::Code, "SELECT E", true),
            (Kind::Str, r"'\''", true),
        ]);
        assert_eq!(kinds(r"SELECT e'a\\b' x")[1], (Kind::Str, r"'a\\b'", true));
        // plain strings: backslash is not an escape ('\' is complete)
        assert_eq!(kinds(r"SELECT '\'")[1], (Kind::Str, r"'\'", true));
        // An `e` that ends a word is no prefix: tablE'x', LIKE'\' and the
        // typed literal xe'\' are plain strings.
        for src in [r"tablE'\'", r"SELECT 'a' LIKE'\'", r"SELECT xe'\'"] {
            assert_eq!(kinds(src).last(), Some(&(Kind::Str, r"'\'", true)), "{src:?}");
        }
        // An `E` that begins a word is one wherever the word begins: after a
        // number, a literal, or a Unicode space (each measured against the
        // engine, which reads `';` as the end of the first statement).
        for src in ["SELECT 1e'\\'';'", "SELECT 'a'e'\\'';'", "SELECT\u{a0}e'\\'';'"] {
            assert_eq!(kinds(src).iter().find(|k| k.0 == Kind::Str && k.1 != "'a'"), Some(&(Kind::Str, r"'\''", true)), "{src:?}");
        }
        // unterminated E-string stays open
        assert!(!kinds(r"SELECT E'\''oops' -- ").last().unwrap().2);
    }

    #[test]
    fn multibyte_stays_whole() {
        // Non-ASCII anywhere must never split a span mid-char (slicing safety).
        for src in ["SELECT tあ", "SELECT “x”", "SELECT 'あ;'; -- あ", "あ$b$c", "$é$x$é$", "x\u{a0}$t$"] {
            for s in scan(src) {
                assert!(src.is_char_boundary(s.start) && src.is_char_boundary(s.end), "{src:?}");
            }
        }
    }
}
