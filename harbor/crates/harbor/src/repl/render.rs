//! Result rendering: duckbox and friends.
//!
//! The policy split is the whole design: BOXED modes (duckbox, markdown)
//! retain O(display) — up to max_rows in the head plus a ring of the last
//! max_rows/2 — count everything, and render after `end`; PIPE modes (csv,
//! json, jsonlines, line, list, trash) emit each row as it arrives with O(1)
//! memory. A 100M-row SELECT costs the boxed client nothing but time.

use wire::Column;
use serde_json::Value;
use std::io::{BufWriter, IsTerminal, Stdout, Write};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Duckbox,
    Duckboxy, // duckbox minus the type row
    Markdown,
    Csv,
    Json,
    JsonLines,
    Line,
    List,
    Trash,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        Some(match s {
            "duckbox" | "box" => Mode::Duckbox,
            "duckboxy" | "boxy" => Mode::Duckboxy,
            "markdown" | "md" => Mode::Markdown,
            "csv" => Mode::Csv,
            "json" => Mode::Json,
            "jsonlines" | "ndjson" => Mode::JsonLines,
            "line" => Mode::Line,
            "list" => Mode::List,
            "trash" => Mode::Trash,
            _ => return None,
        })
    }
    pub fn name(&self) -> &'static str {
        match self {
            Mode::Duckbox => "duckbox",
            Mode::Duckboxy => "duckboxy",
            Mode::Markdown => "markdown",
            Mode::Csv => "csv",
            Mode::Json => "json",
            Mode::JsonLines => "jsonlines",
            Mode::Line => "line",
            Mode::List => "list",
            Mode::Trash => "trash",
        }
    }
    pub fn is_streaming(&self) -> bool {
        !matches!(self, Mode::Duckbox | Mode::Duckboxy | Mode::Markdown)
    }
}

#[derive(Clone)]
pub struct RenderOpts {
    pub mode: Mode,
    pub max_rows: usize,
    pub null: String,
    pub timer: bool,
    /// Whether output is going to a terminal rather than a file or a pipe.
    ///
    /// Only CSV consults this, and only because CSV is the one mode that is
    /// both a data format and unescaped: the display modes may always render a
    /// control character harmless, and JSON always escapes one as `\u001b`,
    /// but doing either to a CSV cell would corrupt the value for the program
    /// on the other end of the pipe — which is what CSV is for. So the rule is
    /// the destination, not the mode: bytes headed for a file stay verbatim,
    /// bytes headed for a terminal are not allowed to drive it.
    pub tty: bool,
}

impl Default for RenderOpts {
    fn default() -> Self {
        Self {
            mode: Mode::Duckbox,
            max_rows: 40,
            null: "NULL".into(),
            timer: false,
            // Detected once here rather than per cell; overridden by the CLI,
            // and false in tests so expectations stay byte-exact.
            tty: std::io::IsTerminal::is_terminal(&std::io::stdout()),
        }
    }
}

/// What the wire put in a column's cells.
///
/// A `Variant` column's schema says `encoding: json`: the engine cast each
/// value to JSON text, minified. A `JsonColumn` is the `JSON` type, whose
/// cast to text is the JSON itself — validated, but exactly as written, so
/// it may be pretty-printed. Both are JSON text, and the json modes splice
/// them in as JSON. Only a VARIANT string sheds its quotes in a display
/// mode: a JSON column is text that is JSON, its quotes are part of the
/// value, and DuckDB's own table shows them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cell {
    Plain,
    JsonColumn,
    Variant,
}

impl Cell {
    fn of(c: &Column) -> Cell {
        if c.encoding.as_deref() == Some("json") {
            Cell::Variant
        } else if c.duckdb_type.eq_ignore_ascii_case("JSON") {
            Cell::JsonColumn
        } else {
            Cell::Plain
        }
    }

    fn is_json_text(self) -> bool {
        self != Cell::Plain
    }
}

/// Streaming renderer: fed one event at a time, finishes on `end`.
///
/// Pipe modes write through one BufWriter — a large export costs pages, not
/// a write(2) per row — flushed at `end`. Boxed modes buffer their layout
/// and hand it to deliver() (pager-aware) instead.
pub struct Renderer<'a> {
    opts: &'a RenderOpts,
    out: BufWriter<Stdout>,
    /// First write failure; once set, output stops. EPIPE here is normal
    /// life (`harbor db … | head`), anything else is reported at `end`.
    broken: Option<std::io::ErrorKind>,
    columns: Vec<String>,
    types: Vec<String>,
    /// Per column, what the wire put in the cell.
    cells: Vec<Cell>,
    head: Vec<Vec<String>>,
    tail: std::collections::VecDeque<Vec<String>>,
    total: u64,
    emitted_first_json: bool,
}

impl<'a> Renderer<'a> {
    pub fn new(opts: &'a RenderOpts) -> Self {
        Self {
            opts,
            out: BufWriter::new(std::io::stdout()),
            broken: None,
            columns: Vec::new(),
            types: Vec::new(),
            cells: Vec::new(),
            head: Vec::new(),
            tail: std::collections::VecDeque::new(),
            total: 0,
            emitted_first_json: false,
        }
    }

    fn emit(&mut self, args: std::fmt::Arguments) {
        if self.broken.is_some() {
            return;
        }
        if let Err(e) = self.out.write_fmt(args) {
            self.broken = Some(e.kind());
        }
    }

    /// Set once a write has failed; the caller should stop feeding rows.
    pub fn failed(&self) -> Option<std::io::ErrorKind> {
        self.broken
    }

    pub fn schema(&mut self, cols: &[Column]) {
        self.columns = cols
            .iter()
            .enumerate()
            .map(|(i, c)| c.name.clone().unwrap_or_else(|| format!("col{i}")))
            .collect();
        // Lowercased for the type row, duckbox-style: quieter under the
        // headers, and how DuckDB's own CLI prints them.
        self.types = cols.iter().map(|c| c.duckdb_type.to_lowercase()).collect();
        self.cells = cols.iter().map(Cell::of).collect();
        match self.opts.mode {
            Mode::Csv => {
                let hdr = self
                    .columns
                    .iter()
                    .map(|c| csv_cell_for(c, self.opts.tty))
                    .collect::<Vec<_>>()
                    .join(",");
                self.emit(format_args!("{hdr}\n"));
            }
            Mode::Json => {
                self.emit(format_args!("["));
            }
            _ => {}
        }
    }

    pub fn row(&mut self, values: Vec<Value>) {
        self.total += 1;
        match self.opts.mode {
            Mode::Trash => {}
            Mode::Csv => {
                let line = values
                    .iter()
                    .map(|v| csv_cell_for(&self.render(v), self.opts.tty))
                    .collect::<Vec<_>>()
                    .join(",");
                self.emit(format_args!("{line}\n"));
            }
            Mode::JsonLines => {
                let obj = json_row(&self.columns, &self.cells, &values);
                self.emit(format_args!("{obj}\n"));
            }
            Mode::Json => {
                let obj = json_row(&self.columns, &self.cells, &values);
                let sep = if self.emitted_first_json { "," } else { "" };
                self.emitted_first_json = true;
                self.emit(format_args!("{sep}\n{obj}"));
            }
            // line and list are display modes, like the boxed ones, so they
            // get the same treatment: a value is shown, never executed. Raw
            // database bytes reaching a terminal means the content chooses the
            // colors, moves the cursor, retitles the window, or drives OSC 52
            // — `SELECT chr(27) || '[2J'` should print an escape, not clear the
            // screen. csv/json deliberately stay raw below: those are
            // interchange formats and escaping there would corrupt the data.
            Mode::Line => {
                let w = self.columns.iter().map(|c| display_width(c)).max().unwrap_or(0);
                let lines: Vec<String> = self
                    .columns
                    .iter()
                    .zip(values.iter())
                    .enumerate()
                    .map(|(i, (c, v))| {
                        let pad = " ".repeat(w.saturating_sub(display_width(c)));
                        format!("{pad}{} = {}", shown_safe(c), shown_safe(&self.shown(i, v)))
                    })
                    .collect();
                self.emit(format_args!("{}\n\n", lines.join("\n")));
            }
            Mode::List => {
                let line = values
                    .iter()
                    .enumerate()
                    .map(|(i, v)| shown_safe(&self.shown(i, v)))
                    .collect::<Vec<_>>()
                    .join("|");
                self.emit(format_args!("{line}\n"));
            }
            Mode::Duckbox | Mode::Duckboxy | Mode::Markdown => {
                // boxed_safe: a value with an embedded newline/tab must not
                // shatter the frame; escape it for display only.
                let cells: Vec<String> =
                    values.iter().enumerate().map(|(i, v)| boxed_safe(&self.shown(i, v))).collect();
                if self.head.len() < self.opts.max_rows {
                    self.head.push(cells);
                } else {
                    if self.tail.len() >= self.opts.max_rows.div_ceil(2).max(1) {
                        self.tail.pop_front();
                    }
                    self.tail.push_back(cells);
                }
            }
        }
    }

    pub fn end(mut self, row_count: u64, time_ms: u64, wall_ms: u128) -> std::io::Result<()> {
        match self.opts.mode {
            Mode::Json => self.emit(format_args!("\n]\n")),
            Mode::Duckbox => self.boxed(row_count, glyphs_duckbox()),
            Mode::Duckboxy => self.boxed(row_count, Glyphs { type_row: false, ..glyphs_duckbox() }),
            Mode::Markdown => self.boxed(row_count, glyphs_markdown()),
            _ => {}
        }
        if self.broken.is_none()
            && let Err(e) = self.out.flush()
        {
            self.broken = Some(e.kind());
        }
        if self.opts.timer {
            eprintln!("Run Time: server {time_ms} ms, wall {wall_ms} ms");
        } else if !self.opts.mode.is_streaming() {
            eprintln!("{row_count} rows ({time_ms} ms)");
        }
        match self.broken {
            None => Ok(()),
            Some(kind) => Err(kind.into()),
        }
    }

    fn render(&self, v: &Value) -> String {
        match v {
            Value::Null => self.opts.null.clone(),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }

    /// A cell as a display mode shows it. A column the wire encodes as JSON
    /// (a VARIANT) holds JSON text in each cell; when that text is a JSON
    /// string, the table shows the string's content — `L2605106156`, not
    /// `"L2605106156"` — the way a VARCHAR column has always shown a string.
    /// A number, a boolean, a null, an object or an array keeps its JSON
    /// text, so 42 and "42" print alike here, as they do from a VARCHAR;
    /// csv and json stay raw and keep them apart. A SQL NULL is still the
    /// NULL marker, and text that is not JSON shows as it came.
    fn shown(&self, i: usize, v: &Value) -> String {
        if self.cells.get(i) == Some(&Cell::Variant)
            && let Value::String(s) = v
            && let Ok(text) = serde_json::from_str::<String>(s)
        {
            return text;
        }
        self.render(v)
    }

    /// The boxed layout. Rows shown: all of head when nothing spilled, else
    /// first half … last half with an elision row, duckdb-style.
    fn boxed(&mut self, total: u64, g: Glyphs) {
        if self.columns.is_empty() {
            return;
        }
        let spilled = !self.tail.is_empty();
        let head: Vec<Vec<String>> = std::mem::take(&mut self.head);
        let tail: Vec<Vec<String>> = std::mem::take(&mut self.tail).into();
        let (top, bottom): (&[Vec<String>], &[Vec<String>]) = if spilled {
            let keep = (self.opts.max_rows / 2).max(1);
            (&head[..keep.min(head.len())], &tail[..])
        } else {
            (&head[..], &[])
        };

        // Column widths from what will be shown: natural when the frame fits
        // the terminal, else the widest columns shrink first (fit_widths, to a
        // floor of MAX_COL); values truncate only to their column's width.
        let ncols = self.columns.len();
        let mut widths: Vec<usize> = (0..ncols)
            .map(|i| {
                let mut w = display_width(&self.columns[i]);
                if g.type_row {
                    w = w.max(display_width(&self.types[i]));
                }
                for r in top.iter().chain(bottom.iter()) {
                    w = w.max(display_width(r.get(i).map(String::as_str).unwrap_or("")));
                }
                w
            })
            .collect();
        let term_w = terminal_width();
        fit_widths(&mut widths, term_w);

        // Terminal fit: prune middle columns, marking with a "…" column.
        let idx = plan_columns(&widths, term_w);
        let pruned = idx.iter().any(Option::is_none);
        let shown_cols = idx.iter().filter(|o| o.is_some()).count();
        let colw = |o: &Option<usize>| o.map_or(1, |i| widths[i]);

        let mut out = String::new();
        // The same say the fleet table gets: NO_COLOR, CLICOLOR_FORCE and
        // TERM=dumb all speak here, not only at a terminal check.
        let color = harbor_common::ui::Style::stdout().color && g.color;
        let dim = |s: &str| if color { format!("\x1b[90m{s}\x1b[0m") } else { s.to_string() };

        let rule = |l: &str, m: &str, r: &str, out: &mut String| {
            if l.is_empty() {
                return;
            }
            out.push_str(l);
            for (k, o) in idx.iter().enumerate() {
                if k > 0 {
                    out.push_str(m);
                }
                out.push_str(&g.h.repeat(colw(o) + 2));
            }
            out.push_str(r);
            out.push('\n');
        };
        let cells_line = |get: &dyn Fn(usize) -> String, styled: bool, out: &mut String| {
            out.push_str(g.v);
            for (k, o) in idx.iter().enumerate() {
                if k > 0 {
                    out.push_str(g.v);
                }
                let (txt, w) = match o {
                    Some(i) => (get(*i), widths[*i]),
                    None => ("…".to_string(), 1),
                };
                let txt = truncate(&txt, w);
                let pad = w.saturating_sub(display_width(&txt));
                let cell = format!(" {txt}{} ", " ".repeat(pad));
                let dimmed = !styled || o.is_none() || txt == self.opts.null;
                out.push_str(&if dimmed { dim(&cell) } else { cell });
            }
            out.push_str(g.v);
            out.push('\n');
        };

        rule(g.tl, g.tm, g.tr, &mut out);
        let cols: Vec<String> = self.columns.iter().map(|c| boxed_safe(c)).collect();
        cells_line(&|i| cols[i].clone(), true, &mut out);
        if g.type_row {
            cells_line(&|i| self.types[i].clone(), false, &mut out);
        }
        rule(g.ml, g.mm, g.mr, &mut out);
        for r in top {
            cells_line(&|i| r.get(i).cloned().unwrap_or_default(), true, &mut out);
        }
        if spilled {
            cells_line(&|_i| "·".to_string(), false, &mut out);
            for r in bottom {
                cells_line(&|i| r.get(i).cloned().unwrap_or_default(), true, &mut out);
            }
        }
        rule(g.bl, g.bm, g.br, &mut out);

        let shown = top.len() + bottom.len();
        if spilled || pruned {
            let mut note = format!("{total} rows");
            if spilled {
                note.push_str(&format!(" ({shown} shown)"));
            }
            if pruned {
                note.push_str(&format!(", {ncols} columns ({shown_cols} shown)"));
            }
            out.push_str(&dim(&note));
            out.push('\n');
        }
        // The frame's rendered width, from geometry rather than measured off
        // `out` (whose dimmed cells carry ANSI codes display_width would count):
        // left border + each shown column padded by 2 + inner separators +
        // right border. plan_columns fits to the terminal but won't prune below
        // two columns, so two wide columns can still overflow — deliver() pages
        // on that so the box doesn't soft-wrap and shatter.
        let frame_width =
            2 + idx.iter().map(|o| colw(o) + 2).sum::<usize>() + idx.len().saturating_sub(1);
        deliver(out, frame_width);
    }
}

/// Boxed output taller OR wider than the terminal pages through $PAGER (default
/// `less -SRFX`: -S no-wrap so a wide box scrolls horizontally instead of
/// shattering, -F quit-if-one-screen). Streaming modes never page.
fn deliver(out: String, width: usize) {
    let (term_w, term_h) = match crossterm::terminal::size() {
        Ok((w, h)) if h > 0 => (w as usize, h as usize),
        _ => (0, 40), // unknown size (odd pty): guess height, don't force width paging
    };
    let tall = out.lines().count() + 2 > term_h;
    // term_w == 0 means the width could not be read — don't guess it into paging.
    let wide = term_w > 0 && width > term_w;
    if (tall || wide) && std::io::stdout().is_terminal() {
        let pager = std::env::var("PAGER").unwrap_or_else(|_| "less -SRFX".into());
        let mut parts = pager.split_whitespace();
        if let Some(cmd) = parts.next() {
            let mut child = std::process::Command::new(cmd)
                .args(parts)
                .stdin(std::process::Stdio::piped())
                .spawn();
            if let Ok(ref mut c) = child {
                if let Some(stdin) = c.stdin.as_mut() {
                    let _ = stdin.write_all(out.as_bytes());
                }
                let _ = c.wait();
                return;
            }
        }
    }
    print!("{out}");
    let _ = std::io::stdout().flush();
}

struct Glyphs {
    tl: &'static str, tm: &'static str, tr: &'static str,
    ml: &'static str, mm: &'static str, mr: &'static str,
    bl: &'static str, bm: &'static str, br: &'static str,
    h: &'static str, v: &'static str,
    type_row: bool,
    color: bool,
}

fn glyphs_duckbox() -> Glyphs {
    Glyphs {
        tl: "┌", tm: "┬", tr: "┐",
        ml: "├", mm: "┼", mr: "┤",
        bl: "└", bm: "┴", br: "┘",
        h: "─", v: "│",
        type_row: true,
        color: true,
    }
}

fn glyphs_markdown() -> Glyphs {
    Glyphs {
        tl: "", tm: "", tr: "",
        ml: "|", mm: "|", mr: "|",
        bl: "", bm: "", br: "",
        h: "-", v: "|",
        type_row: false,
        color: false,
    }
}

fn terminal_width() -> usize {
    crossterm::terminal::size().map(|(w, _)| w as usize).unwrap_or(120)
}

/// The floor a column shrinks to before pruning takes over — and, on a narrow
/// terminal, the effective cap a wide value is cut back to.
const MAX_COL: usize = 40;

/// Fit natural column widths to the terminal. A frame that fits keeps every
/// column at its natural width; one that doesn't shrinks the widest columns
/// first — all ties together, down to the runner-up level — never below
/// MAX_COL. Whatever still cannot fit at the floor is plan_columns's problem.
fn fit_widths(widths: &mut [usize], term_w: usize) {
    const GAP: usize = 3; // " │ " between cells
    const EDGES: usize = 4; // the outer borders and their padding
    let budget = term_w.saturating_sub(EDGES + widths.len().saturating_sub(1) * GAP);
    loop {
        let total: usize = widths.iter().sum();
        if total <= budget {
            return;
        }
        let mx = widths.iter().copied().max().unwrap_or(0);
        if mx <= MAX_COL {
            return;
        }
        let at_max = widths.iter().filter(|&&w| w == mx).count();
        let next = widths.iter().copied().filter(|&w| w < mx).max().unwrap_or(0);
        let target = next.max(MAX_COL).max(mx.saturating_sub((total - budget).div_ceil(at_max)));
        for w in widths.iter_mut() {
            if *w == mx {
                *w = target;
            }
        }
    }
}

/// Which columns a `term_w`-wide terminal shows: every index, or a
/// left…right selection with `None` marking the "…" elision column.
/// Growth alternates sides from 1+1 until the next column would not fit.
fn plan_columns(widths: &[usize], term_w: usize) -> Vec<Option<usize>> {
    const GAP: usize = 3; // " │ " between cells
    const EDGES: usize = 4; // the outer borders and their padding
    const ELLIPSIS: usize = 3; // the "…" column: 1 wide plus its padding
    let ncols = widths.len();
    let fits = |ws: &[usize], pruned: bool| {
        ws.iter().sum::<usize>()
            + (ws.len() + pruned as usize).saturating_sub(1) * GAP
            + EDGES
            + if pruned { ELLIPSIS } else { 0 }
            <= term_w
    };
    let mut left = ncols; // columns shown from the left before the … column
    let mut right = 0; // columns shown from the right
    if !fits(widths, false) && ncols > 2 {
        left = 1;
        right = 1;
        loop {
            let mut ws: Vec<usize> = widths[..left].to_vec();
            ws.extend_from_slice(&widths[ncols - right..]);
            if !fits(&ws, true) {
                if left + right > 2 {
                    if left > right { left -= 1 } else { right -= 1 }
                }
                break;
            }
            if left + right >= ncols {
                left = ncols;
                right = 0;
                break;
            }
            if left <= right { left += 1 } else { right += 1 }
        }
    }
    if left < ncols {
        let mut v: Vec<Option<usize>> = (0..left).map(Some).collect();
        v.push(None); // the … column
        v.extend((ncols - right..ncols).map(Some));
        v
    } else {
        (0..ncols).map(Some).collect()
    }
}

fn display_width(s: &str) -> usize {
    s.width() // terminal cells, so CJK/emoji columns align
}

fn truncate(s: &str, w: usize) -> String {
    if display_width(s) <= w {
        return s.to_string();
    }
    let budget = w.saturating_sub(1); // room for the …
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if used + cw > budget {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

/// Control characters in a value shown at a terminal are the terminal's
/// instructions, not the value's content. `boxed_safe` is this with the frame
/// as its reason; this is the same rule for the unframed display modes, where
/// the reason is only that a value must not be able to drive the terminal.
fn shown_safe(s: &str) -> String {
    boxed_safe(s)
}

/// Control characters would shatter the boxed frame; show them escaped.
fn boxed_safe(s: &str) -> String {
    if !s.chars().any(|c| c.is_control()) {
        return s.to_string();
    }
    s.chars()
        .map(|c| match c {
            '\n' => "\\n".to_string(),
            '\t' => "\\t".to_string(),
            '\r' => "\\r".to_string(),
            c if c.is_control() => '\u{FFFD}'.to_string(),
            c => c.to_string(),
        })
        .collect()
}

/// A row as a JSON object with duplicate column names kept verbatim —
/// `{"a":1,"a":2}` is what `duckdb -json` emits too; syntactically valid,
/// and the consumer's parser picks its own policy. serde_json::Map would
/// silently collapse them, so the object is assembled by hand.
///
/// A top-level column the wire carries as JSON text (a VARIANT or a JSON
/// column) is spliced in as that JSON, so `doc.patient.age` reads as `43`
/// and a document as an object, the way `duckdb -json` emits a JSON
/// column. Quoting the text as a string would be JSON inside JSON, and the
/// reader would have to parse twice. A SQL NULL and a JSON null both come
/// out as `null` — the consumer's `JSON.parse` gives the same value either
/// way; the wire and csv still tell them apart. JSON nested inside a
/// STRUCT, LIST or MAP column stays a string, as the wire holds it.
///
/// The text is checked before it goes in, so a row is always well-formed.
/// What fails the check stays a string: `NaN` and `Infinity`, which the
/// engine's JSON cast writes bare and JSON has no word for; a document
/// nested more than 128 levels deep, serde_json's limit. A wide integer
/// (a HUGEINT put inside a VARIANT) passes and is spliced as a bare number,
/// where the same value in its own column is the envelope's JSON-safe
/// string; that is what `duckdb -json` does, and a consumer wanting the
/// digits reads csv. A JSON column keeps its text as written, so a
/// pretty-printed document carries newlines between its tokens; valid JSON
/// has no raw newline anywhere else, and jsonlines is one record per line,
/// so they become spaces.
fn json_row(columns: &[String], cells: &[Cell], values: &[Value]) -> String {
    let mut s = String::from("{");
    for (i, (c, v)) in columns.iter().zip(values.iter()).enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&serde_json::to_string(c).expect("strings serialize"));
        s.push(':');
        match v {
            Value::String(text)
                if cells.get(i).is_some_and(|c| c.is_json_text())
                    && serde_json::from_str::<serde::de::IgnoredAny>(text).is_ok() =>
            {
                if text.contains(['\n', '\r']) {
                    s.extend(text.chars().map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch }));
                } else {
                    s.push_str(text);
                }
            }
            _ => s.push_str(&v.to_string()),
        }
    }
    s.push('}');
    s
}

/// A CSV cell, quoted per RFC 4180.
///
/// `tty` is the escape gate described on `RenderOpts::tty`: a value carrying
/// `ESC [ 31 m` is a colour instruction the moment it reaches a terminal, and
/// `harbor db --mode csv` at a prompt is a terminal. Piped or redirected, the
/// bytes go out exactly as the database holds them.
fn csv_cell_for(s: &str, tty: bool) -> String {
    let owned;
    let s = match tty && s.chars().any(char::is_control) {
        true => {
            owned = boxed_safe(s);
            owned.as_str()
        }
        false => s,
    };
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
fn csv_cell(s: &str) -> String {
    csv_cell_for(s, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn col(name: &str, ty: &str) -> Column {
        Column { name: Some(name.into()), duckdb_type: ty.into(), lossless: true, ..Default::default() }
    }

    #[test]
    fn head_tail_retention_bounds_memory() {
        let opts = RenderOpts { max_rows: 4, ..Default::default() };
        let mut r = Renderer::new(&opts);
        r.schema(&[col("n", "BIGINT")]);
        for i in 0..1000 {
            r.row(vec![json!(i)]);
        }
        assert_eq!(r.total, 1000);
        assert_eq!(r.head.len(), 4);
        assert!(r.tail.len() <= 2);
        assert_eq!(r.tail.back().unwrap()[0], "999"); // the true tail survived
    }

    /// A value is shown, never executed: an escape sequence in the database
    /// must not reach the terminal from a display mode. csv/json are
    /// interchange and stay raw — escaping there would corrupt the data.
    #[test]
    fn display_modes_neutralize_control_characters() {
        let esc = "\u{1b}[31mRED\u{1b}[0m";
        assert!(!shown_safe(esc).contains('\u{1b}'), "escape survived: {:?}", shown_safe(esc));
        assert!(!boxed_safe(esc).contains('\u{1b}'));
        // Ordinary text is untouched, including multi-byte characters.
        assert_eq!(shown_safe("plain あ"), "plain あ");
        // The common whitespace escapes stay readable rather than becoming
        // replacement characters.
        assert_eq!(shown_safe("a\nb\tc"), "a\\nb\\tc");
    }

    #[test]
    fn csv_quoting() {
        // Piped (tty=false): bytes go out verbatim, because the consumer is a
        // program and an escaped value would be a corrupted one.
        assert_eq!(csv_cell_for("\u{1b}[31mred", false), "\u{1b}[31mred");
        // At a terminal: the same bytes are instructions, so they are defused.
        assert!(!csv_cell_for("\u{1b}[31mred", true).contains('\u{1b}'));
        // Quoting is unaffected by either.
        assert_eq!(csv_cell_for("a,b", true), "\"a,b\"");
        assert_eq!(csv_cell("plain"), "plain");
        assert_eq!(csv_cell("a,b"), "\"a,b\"");
        assert_eq!(csv_cell("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn a_variant_string_shows_its_content_in_display_modes() {
        let opts = RenderOpts { mode: Mode::Duckbox, max_rows: 10, null: "NULL".into(), timer: false, tty: false };
        let mut r = Renderer::new(&opts);
        let variant = Column { encoding: Some("json".into()), duckdb_type: "VARIANT".into(), ..Column::default() };
        let plain = Column { duckdb_type: "VARCHAR".into(), ..Column::default() };
        r.schema(&[variant, plain]);
        // the JSON-encoded column: a string sheds its quotes, nothing else changes
        assert_eq!(r.shown(0, &json!("\"L2605106156\"")), "L2605106156");
        assert_eq!(r.shown(0, &json!("\"42\"")), "42");
        assert_eq!(r.shown(0, &json!("42")), "42");
        assert_eq!(r.shown(0, &json!("true")), "true");
        assert_eq!(r.shown(0, &json!("null")), "null");
        assert_eq!(r.shown(0, &json!("{\"a\":1}")), "{\"a\":1}");
        assert_eq!(r.shown(0, &json!("\"a\\nb\"")), "a\nb");
        assert_eq!(r.shown(0, &Value::Null), "NULL");
        assert_eq!(r.shown(0, &json!("not json")), "not json");
        // a plain column is untouched, quotes and all
        assert_eq!(r.shown(1, &json!("\"quoted\"")), "\"quoted\"");
    }

    #[test]
    fn json_rows_keep_duplicate_keys() {
        let cols = vec!["a".to_string(), "a".to_string(), "b\"q".to_string()];
        let vals = vec![json!(1), json!(2), json!(null)];
        assert_eq!(json_row(&cols, &[Cell::Plain; 3], &vals), r#"{"a":1,"a":2,"b\"q":null}"#);
        assert_eq!(json_row(&[], &[], &[]), "{}");
    }

    #[test]
    fn json_rows_splice_json_text_cells() {
        let cols = vec!["v".to_string(), "s".to_string()];
        let flags = [Cell::Variant, Cell::Plain];
        // a VARIANT string, number, object, array and JSON null go in as JSON
        assert_eq!(json_row(&cols, &flags, &[json!("\"Steve\""), json!("x")]), r#"{"v":"Steve","s":"x"}"#);
        assert_eq!(json_row(&cols, &flags, &[json!("43"), json!("43")]), r#"{"v":43,"s":"43"}"#);
        assert_eq!(json_row(&cols, &flags, &[json!("{\"a\":[1,\"2\",null]}"), json!(null)]), r#"{"v":{"a":[1,"2",null]},"s":null}"#);
        assert_eq!(json_row(&cols, &flags, &[json!("null"), json!("null")]), r#"{"v":null,"s":"null"}"#);
        assert_eq!(json_row(&cols, &flags, &[json!("true"), json!(true)]), r#"{"v":true,"s":true}"#);
        // a SQL NULL is null, as it always was
        assert_eq!(json_row(&cols, &flags, &[Value::Null, Value::Null]), r#"{"v":null,"s":null}"#);
        // text that is not JSON stays a string: the object is never malformed
        assert_eq!(json_row(&cols, &flags, &[json!("not json"), json!("x")]), r#"{"v":"not json","s":"x"}"#);
        assert_eq!(json_row(&cols, &flags, &[json!("1 2"), json!("x")]), r#"{"v":"1 2","s":"x"}"#);
        // a plain column holding JSON-looking text is a string, untouched
        assert_eq!(json_row(&cols, &flags, &[json!("1"), json!("{\"a\":1}")]), r#"{"v":1,"s":"{\"a\":1}"}"#);
        // what the engine's cast writes that JSON cannot say stays a string
        assert_eq!(json_row(&cols, &flags, &[json!("NaN"), json!("x")]), r#"{"v":"NaN","s":"x"}"#);
        assert_eq!(json_row(&cols, &flags, &[json!("-Infinity"), json!("x")]), r#"{"v":"-Infinity","s":"x"}"#);
        // a wide integer and duplicate keys go in as they are
        assert_eq!(json_row(&cols, &flags, &[json!("170141183460469231731687303715884105727"), json!("x")]), r#"{"v":170141183460469231731687303715884105727,"s":"x"}"#);
        assert_eq!(json_row(&cols, &flags, &[json!("{\"a\":1,\"a\":2}"), json!("x")]), r#"{"v":{"a":1,"a":2},"s":"x"}"#);
        // a JSON column is spliced too, and a pretty-printed one stays on one line
        let json = [Cell::JsonColumn, Cell::Plain];
        assert_eq!(json_row(&cols, &json, &[json!("{\n  \"a\": 1\r\n}"), json!("x")]), "{\"v\":{   \"a\": 1  },\"s\":\"x\"}");
        assert_eq!(json_row(&cols, &json, &[json!("\"line\\nbreak\""), json!("x")]), r#"{"v":"line\nbreak","s":"x"}"#);
    }

    #[test]
    fn a_column_is_classed_by_what_the_wire_put_in_it() {
        let opts = RenderOpts { mode: Mode::Json, max_rows: 10, null: "NULL".into(), timer: false, tty: false };
        let mut r = Renderer::new(&opts);
        let variant = Column { encoding: Some("json".into()), duckdb_type: "VARIANT".into(), ..Column::default() };
        let json = Column { duckdb_type: "JSON".into(), ..Column::default() };
        let plain = Column { duckdb_type: "VARCHAR".into(), ..Column::default() };
        r.schema(&[variant, json, plain]);
        assert_eq!(r.cells, vec![Cell::Variant, Cell::JsonColumn, Cell::Plain]);
        // only a VARIANT string sheds its quotes in a display mode; a JSON
        // column's quotes are part of its text, as DuckDB's own table shows them
        assert_eq!(r.shown(0, &json!("\"abc\"")), "abc");
        assert_eq!(r.shown(1, &json!("\"abc\"")), "\"abc\"");
        assert_eq!(r.shown(2, &json!("\"abc\"")), "\"abc\"");
    }

    #[test]
    fn column_plan_fits_the_terminal() {
        // plenty of room: every column, in order
        assert_eq!(plan_columns(&[5, 5, 5], 120), vec![Some(0), Some(1), Some(2)]);
        // too narrow: first … last, the minimal plan
        assert_eq!(plan_columns(&[20, 20, 20, 20], 40), vec![Some(0), None, Some(3)]);
        // room for three of four (all four need 53): left gets the extra
        assert_eq!(plan_columns(&[10, 10, 10, 10], 50), vec![Some(0), Some(1), None, Some(3)]);
        // two columns are never pruned, even overflowing
        assert_eq!(plan_columns(&[100, 100], 40), vec![Some(0), Some(1)]);
        // exact fit: 3 cols of 10 = 30 + 2 gaps (6) + edges (4) = 40
        assert_eq!(plan_columns(&[10, 10, 10], 40), vec![Some(0), Some(1), Some(2)]);
        assert_eq!(plan_columns(&[10, 10, 10], 39), vec![Some(0), None, Some(2)]);
    }

    #[test]
    fn truncate_marks_elision() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 4), "abc");
    }

    #[test]
    fn wide_terminal_keeps_natural_widths() {
        // 138 wide + 2 gaps (6) + edges (4) = 148: fits 200, nothing shrinks
        let mut w = vec![8, 120, 10];
        fit_widths(&mut w, 200);
        assert_eq!(w, vec![8, 120, 10]);
    }

    #[test]
    fn widest_column_shrinks_first() {
        // budget 90: only the 120 gives ground, and only by what's needed
        let mut w = vec![8, 120, 10];
        fit_widths(&mut w, 100);
        assert_eq!(w, vec![8, 72, 10]);
    }

    #[test]
    fn ties_shrink_together_and_stop_at_the_floor() {
        let mut w = vec![60, 60, 60];
        fit_widths(&mut w, 60);
        assert_eq!(w, vec![40, 40, 40]); // pruning takes it from here
    }
}
