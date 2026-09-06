//! How harbor (and ducktable's fleet view) look.
//!
//! Three rules hold the whole module together:
//!
//! 1. **A pipe gets data, a terminal gets a picture.** Box-drawing and color
//!    appear only when stdout is a tty. Piped output is tab-separated and
//!    byte-stable, so `harbor | awk` keeps working forever.
//! 2. **`NO_COLOR` wins.** It is the de facto standard and it costs one line.
//! 3. **Nothing from disk is ever printed raw.** A database path containing
//!    an escape sequence must not be able to repaint the terminal or forge a
//!    table row, so every untrusted string goes through [`sanitize`] before
//!    it is measured or padded.

use std::io::IsTerminal;
use unicode_width::UnicodeWidthStr;

// ---------------------------------------------------------------------------
// color
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Tone {
    #[default]
    Plain,
    Dim,
    Bold,
    Green,
    Yellow,
    Red,
    Cyan,
}

impl Tone {
    fn code(self) -> &'static str {
        match self {
            Tone::Plain => "",
            Tone::Dim => "\x1b[2m",
            Tone::Bold => "\x1b[1m",
            Tone::Green => "\x1b[32m",
            Tone::Yellow => "\x1b[33m",
            Tone::Red => "\x1b[31m",
            Tone::Cyan => "\x1b[36m",
        }
    }
}

/// The terminal's reading of a severity level. A GUI writes its own.
impl From<crate::state::Level> for Tone {
    fn from(l: crate::state::Level) -> Tone {
        use crate::state::Level;
        match l {
            Level::Idle => Tone::Dim,
            Level::Good => Tone::Green,
            Level::Warn => Tone::Yellow,
            Level::Bad => Tone::Red,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Style {
    pub color: bool,
    pub boxed: bool,
}

impl Style {
    /// What stdout deserves right now.
    ///
    /// `NO_COLOR` (present and non-empty) turns color off whatever else says;
    /// `CLICOLOR_FORCE` turns it on even through a pipe, which is what CI
    /// systems that render ANSI in their log viewer need.
    pub fn stdout() -> Self {
        let tty = std::io::stdout().is_terminal();
        Style { color: color_allowed(tty), boxed: tty }
    }

    /// Bytes, for a pipe or a file. Never colored, never boxed.
    pub fn plain() -> Self {
        Style { color: false, boxed: false }
    }

    /// `--color auto|always|never`, or `[defaults] color`.
    pub fn with_choice(self, choice: Option<&str>) -> Self {
        match choice {
            Some("always") => Style { color: true, ..self },
            Some("never") => Style { color: false, ..self },
            _ => self,
        }
    }

    pub fn paint(&self, tone: Tone, s: &str) -> String {
        if !self.color || tone == Tone::Plain {
            return s.to_string();
        }
        format!("{}{s}\x1b[0m", tone.code())
    }
}

fn color_allowed(tty: bool) -> bool {
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    // TERM=dumb means a terminal that cannot, not one that would rather not.
    if std::env::var("TERM").is_ok_and(|t| t == "dumb") {
        return false;
    }
    tty
}

// ---------------------------------------------------------------------------
// safety and width
// ---------------------------------------------------------------------------

/// Strip anything that could move the cursor, change the color, or forge a
/// border. Replaces rather than deletes, so a hostile name still occupies
/// space and stays visible instead of quietly vanishing.
fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c == '\u{1b}' || c.is_control() { '\u{fffd}' } else { c }).collect()
}

/// Terminal cells, not bytes and not chars.
fn cells(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

fn pad(s: &str, to: usize, right: bool) -> String {
    let w = cells(s);
    let gap = to.saturating_sub(w);
    match right {
        true => format!("{}{s}", " ".repeat(gap)),
        false => format!("{s}{}", " ".repeat(gap)),
    }
}


// ---------------------------------------------------------------------------
// box drawing
// ---------------------------------------------------------------------------

struct Charset {
    pub tl: &'static str,
    pub tm: &'static str,
    pub tr: &'static str,
    pub ml: &'static str,
    pub mm: &'static str,
    pub mr: &'static str,
    pub bl: &'static str,
    pub bm: &'static str,
    pub br: &'static str,
    pub h: &'static str,
    pub v: &'static str,
}

/// Rounded corners. The straight-cornered set stays in the repl's
/// `render.rs` for duckbox, which deliberately mirrors the DuckDB shell.
const ROUNDED: Charset = Charset {
    tl: "╭",
    tm: "┬",
    tr: "╮",
    ml: "├",
    mm: "┼",
    mr: "┤",
    bl: "╰",
    bm: "┴",
    br: "╯",
    h: "─",
    v: "│",
};

// ---------------------------------------------------------------------------
// table
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Cell {
    pub text: String,
    pub tone: Tone,
    pub right: bool,
}

impl Cell {
    pub fn new(text: impl AsRef<str>) -> Self {
        Cell { text: sanitize(text.as_ref()), tone: Tone::Plain, right: false }
    }
    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }
    pub fn right(mut self) -> Self {
        self.right = true;
        self
    }
}

/// A table, plus advisory lines that hang off a row.
///
/// The notes are the point: a `stale` row that cannot say *why* it is stale,
/// or what to run about it, sends the reader to the docs — and the moment
/// they need the answer is the only moment they will read anything.
#[derive(Default)]
pub struct Table {
    caption: Option<String>,
    head: Vec<String>,
    rows: Vec<Vec<Cell>>,
    notes: Vec<(usize, Tone, String)>,
}

impl Table {
    pub fn new<S: AsRef<str>>(head: impl IntoIterator<Item = S>) -> Self {
        Table {
            head: head.into_iter().map(|h| sanitize(h.as_ref())).collect(),
            ..Default::default()
        }
    }

    /// Give the table a title. On a terminal it becomes a small tab joined
    /// to the table's top border; through a pipe it is a plain line before
    /// the TSV header.
    pub fn caption(&mut self, text: impl AsRef<str>) -> &mut Self {
        self.caption = Some(sanitize(text.as_ref()));
        self
    }

    pub fn row(&mut self, cells: impl IntoIterator<Item = Cell>) -> &mut Self {
        self.rows.push(cells.into_iter().collect());
        self
    }

    /// Attach an advisory to the row most recently added.
    pub fn note(&mut self, tone: Tone, text: impl AsRef<str>) -> &mut Self {
        let at = self.rows.len().saturating_sub(1);
        self.notes.push((at, tone, sanitize(text.as_ref())));
        self
    }


    pub fn render(&self, st: &Style) -> String {
        match st.boxed {
            true => self.render_boxed(st),
            false => self.render_plain(),
        }
    }

    /// Plain caption, then tab-separated data; no notes or color. What a pipe wants.
    fn render_plain(&self) -> String {
        let mut out = String::new();
        if let Some(caption) = &self.caption {
            out.push_str(caption);
            out.push('\n');
        }
        if !self.head.is_empty() {
            out.push_str(&self.head.join("\t"));
            out.push('\n');
        }
        for r in &self.rows {
            let line: Vec<&str> = r.iter().map(|c| c.text.as_str()).collect();
            out.push_str(&line.join("\t"));
            out.push('\n');
        }
        out
    }

    fn widths(&self) -> Vec<usize> {
        let n = self.head.len().max(self.rows.iter().map(|r| r.len()).max().unwrap_or(0));
        let mut w = vec![0usize; n];
        for (i, h) in self.head.iter().enumerate() {
            w[i] = w[i].max(cells(h));
        }
        for r in &self.rows {
            for (i, c) in r.iter().enumerate() {
                w[i] = w[i].max(cells(&c.text));
            }
        }
        // Keep the caption's right edge inside the first cell rather than
        // colliding with its first column joint. Usually DATABASE is already
        // much wider; this matters for an empty fleet.
        if let (Some(caption), Some(first)) = (&self.caption, w.first_mut()) {
            *first = (*first).max(cells(caption) + 1);
        }
        w
    }

    fn render_boxed(&self, st: &Style) -> String {
        let c = &ROUNDED;
        let w = self.widths();
        if w.is_empty() {
            return String::new();
        }
        let seg = |i: usize| c.h.repeat(w[i] + 2);
        let rule = |l: &str, m: &str, r: &str| {
            let mid: Vec<String> = (0..w.len()).map(seg).collect();
            format!("{l}{}{r}", mid.join(m))
        };

        let mut out = String::new();
        if let Some(caption) = &self.caption {
            let caption_width = cells(caption);
            out.push_str(&format!("{}{}{}\n", c.tl, c.h.repeat(caption_width + 2), c.tr));
            out.push_str(&format!(
                "{} {} {}\n",
                c.v,
                st.paint(Tone::Bold, caption),
                c.v
            ));

            // The caption's lower border is also the table's top border.
            // Because widths() keeps its endpoint inside column zero, the
            // join is a T pointing up, followed later by the normal column
            // joints pointing down.
            let mut top: Vec<char> = rule(c.tl, c.tm, c.tr).chars().collect();
            top[0] = c.ml.chars().next().unwrap();
            top[caption_width + 3] = c.bm.chars().next().unwrap();
            out.extend(top);
            out.push('\n');
        } else {
            out.push_str(&rule(c.tl, c.tm, c.tr));
            out.push('\n');
        }

        if !self.head.is_empty() {
            let hs: Vec<String> = (0..w.len())
                .map(|i| {
                    let h = self.head.get(i).map(String::as_str).unwrap_or("");
                    format!(" {} ", pad(h, w[i], false))
                })
                .map(|s| st.paint(Tone::Bold, &s))
                .collect();
            out.push_str(&format!("{}{}{}\n", c.v, hs.join(c.v), c.v));
            out.push_str(&rule(c.ml, c.mm, c.mr));
            out.push('\n');
        }

        for (ri, r) in self.rows.iter().enumerate() {
            let cs: Vec<String> = (0..w.len())
                .map(|i| match r.get(i) {
                    Some(cell) => {
                        let body = pad(&cell.text, w[i], cell.right);
                        format!(" {} ", st.paint(cell.tone, &body))
                    }
                    None => " ".repeat(w[i] + 2),
                })
                .collect();
            out.push_str(&format!("{}{}{}", c.v, cs.join(c.v), c.v));

            // A footnote marker hangs OUTSIDE the right edge, so the grid is
            // never interrupted: a note drawn as a full-width row inside the
            // box broke every column rule beneath it. The note itself prints
            // below the table under the same superscript.
            let marks: Vec<String> = self
                .notes
                .iter()
                .enumerate()
                .filter(|(_, (at, _, _))| *at == ri)
                .map(|(k, (_, tone, _))| st.paint(*tone, &superscript(k + 1)))
                .collect();
            if !marks.is_empty() {
                out.push(' ');
                out.push_str(&marks.join(" "));
            }
            out.push('\n');
        }

        out.push_str(&rule(c.bl, c.bm, c.br));
        out.push('\n');
        for (k, (_, tone, text)) in self.notes.iter().enumerate() {
            out.push_str(&st.paint(*tone, &format!("{} {text}", superscript(k + 1))));
            out.push('\n');
        }
        out
    }
}

/// 1 → "¹", 12 → "¹²" — the footnote marks beside a table's right edge.
fn superscript(n: usize) -> String {
    const DIGITS: [char; 10] = ['⁰', '¹', '²', '³', '⁴', '⁵', '⁶', '⁷', '⁸', '⁹'];
    n.to_string().chars().map(|d| DIGITS[d as usize - '0' as usize]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed() -> Style {
        Style { color: false, boxed: true }
    }

    #[test]
    fn a_pipe_gets_tsv_with_no_escapes() {
        let mut t = Table::new(["NAME", "STATE"]);
        t.row([Cell::new("medlabs"), Cell::new("running").tone(Tone::Green)]);
        t.note(Tone::Yellow, "config changed — harbor stop medlabs && harbor start medlabs");
        let out = t.render(&Style::plain());
        assert_eq!(out, "NAME\tSTATE\nmedlabs\trunning\n");
        assert!(!out.contains('\u{1b}'));
    }

    #[test]
    fn rounded_corners_and_square_joints() {
        let mut t = Table::new(["A", "B"]);
        t.row([Cell::new("1"), Cell::new("2")]);
        let out = t.render(&boxed());
        assert!(out.starts_with("╭─"), "{out}");
        assert!(out.contains('┬') && out.contains('┼') && out.contains('┴'));
        assert!(out.trim_end().ends_with('╯'), "{out}");
    }

    #[test]
    fn a_caption_is_a_tab_joined_to_the_top_border() {
        let mut t = Table::new(["DATABASE", "VERSION"]);
        t.caption("fleet");
        t.row([Cell::new("~/labs.duckdb"), Cell::new("old")]);
        let out = t.render(&boxed());
        let mut lines = out.lines();
        assert_eq!(lines.next(), Some("╭───────╮"));
        assert_eq!(lines.next(), Some("│ fleet │"));
        assert_eq!(lines.next(), Some("├───────┴───────┬─────────╮"));
    }

    #[test]
    fn a_caption_stays_a_plain_line_in_tsv() {
        let mut t = Table::new(["DATABASE", "VERSION"]);
        t.caption("fleet");
        t.row([Cell::new("db.duckdb"), Cell::new("old")]);
        assert_eq!(
            t.render(&Style::plain()),
            "fleet\nDATABASE\tVERSION\ndb.duckdb\told\n"
        );
    }

    #[test]
    fn every_line_of_the_box_is_the_same_width() {
        let mut t = Table::new(["NAME", "DATABASE"]);
        t.row([Cell::new("medlabs"), Cell::new("~/Data/medlabs.duckdb")]);
        t.note(Tone::Yellow, "no setup file");
        t.row([Cell::new("x"), Cell::new("~/y.duckdb")]);
        let out = t.render(&boxed());
        // The box: lines that start with a border glyph, measured to the
        // closing glyph. A footnote marker hangs outside the right edge and
        // does not count against the grid.
        let widths: Vec<usize> = out
            .lines()
            .filter(|l| l.starts_with(['╭', '│', '├', '╰']))
            .map(|l| {
                let end = l.rfind(['╮', '│', '┤', '╯']).unwrap();
                cells(&l[..end + 3]) // the border glyphs are 3 bytes each
            })
            .collect();
        assert_eq!(widths.len(), 6, "box lines went missing:\n{out}");
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "ragged: {widths:?}\n{out}");
    }

    #[test]
    fn a_note_is_a_footnote_not_a_row() {
        let mut t = Table::new(["NAME", "STATE"]);
        t.row([Cell::new("medlabs"), Cell::new("unmanaged")]);
        t.note(Tone::Yellow, "not in your config");
        t.row([Cell::new("labs"), Cell::new("stopped")]);
        t.note(Tone::Yellow, "held but no longer configured");
        let out = t.render(&boxed());
        // The marker sits after the row's right edge; the text sits below the
        // box under the same superscript, so the grid is never interrupted.
        assert!(out.lines().any(|l| l.ends_with("│ ¹")), "no marker:\n{out}");
        assert!(out.lines().any(|l| l.ends_with("│ ²")), "no second marker:\n{out}");
        let bottom = out.lines().position(|l| l.starts_with('╰')).unwrap();
        let below: Vec<&str> = out.lines().skip(bottom + 1).collect();
        assert_eq!(below, ["¹ not in your config", "² held but no longer configured"], "{out}");
        assert!(!out.contains('!'), "the old inline note style survived:\n{out}");
    }

    #[test]
    fn wide_characters_do_not_shear_the_columns() {
        // Counting bytes or chars here is exactly how a table goes ragged.
        let mut t = Table::new(["NAME"]);
        t.row([Cell::new("日本語")]);
        t.row([Cell::new("abcdef")]);
        let out = t.render(&boxed());
        let widths: Vec<usize> = out.lines().map(cells).collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "ragged: {widths:?}\n{out}");
    }

    #[test]
    fn a_hostile_name_cannot_repaint_the_terminal() {
        let mut t = Table::new(["NAME"]);
        t.row([Cell::new("\u{1b}[31mred\u{1b}[0m│fake")]);
        let out = t.render(&boxed());
        assert!(!out.contains('\u{1b}'), "escape survived: {out:?}");
        // The bars that remain are the table's own, one per side.
        let bars = out.lines().nth(3).unwrap().matches('│').count();
        assert_eq!(bars, 3, "a forged bar got through: {out}");
    }

    #[test]
    fn no_color_beats_everything() {
        let st = Style { color: false, boxed: true };
        assert_eq!(st.paint(Tone::Red, "x"), "x");
    }

}
