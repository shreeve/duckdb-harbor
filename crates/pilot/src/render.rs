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
}

impl Default for RenderOpts {
    fn default() -> Self {
        Self { mode: Mode::Duckbox, max_rows: 40, null: "NULL".into(), timer: false }
    }
}

impl RenderOpts {
    /// Defaults, then the config file's [defaults] on top. Flags and
    /// dot-commands override the result — the documented precedence.
    pub fn with_defaults(d: &crate::config::Defaults) -> Self {
        let mut o = Self::default();
        if let Some(m) = d.mode.as_deref().and_then(Mode::parse) {
            o.mode = m;
        }
        if let Some(t) = d.timer {
            o.timer = t;
        }
        if let Some(n) = d.maxrows {
            o.max_rows = n;
        }
        if let Some(nv) = &d.nullvalue {
            o.null = nv.clone();
        }
        o
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
    /// life (`pilot … | head`), anything else is reported at `end`.
    broken: Option<std::io::ErrorKind>,
    columns: Vec<String>,
    types: Vec<String>,
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
        match self.opts.mode {
            Mode::Csv => {
                let hdr = self.columns.iter().map(|c| csv_cell(c)).collect::<Vec<_>>().join(",");
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
                let line = values.iter().map(|v| csv_cell(&self.render(v))).collect::<Vec<_>>().join(",");
                self.emit(format_args!("{line}\n"));
            }
            Mode::JsonLines => {
                let obj = json_row(&self.columns, &values);
                self.emit(format_args!("{obj}\n"));
            }
            Mode::Json => {
                let obj = json_row(&self.columns, &values);
                let sep = if self.emitted_first_json { "," } else { "" };
                self.emitted_first_json = true;
                self.emit(format_args!("{sep}\n{obj}"));
            }
            Mode::Line => {
                let w = self.columns.iter().map(|c| display_width(c)).max().unwrap_or(0);
                let lines: Vec<String> = self
                    .columns
                    .iter()
                    .zip(values.iter())
                    .map(|(c, v)| {
                        let pad = " ".repeat(w.saturating_sub(display_width(c)));
                        format!("{pad}{c} = {}", self.render(v))
                    })
                    .collect();
                self.emit(format_args!("{}\n\n", lines.join("\n")));
            }
            Mode::List => {
                let line = values.iter().map(|v| self.render(v)).collect::<Vec<_>>().join("|");
                self.emit(format_args!("{line}\n"));
            }
            Mode::Duckbox | Mode::Duckboxy | Mode::Markdown => {
                // boxed_safe: a value with an embedded newline/tab must not
                // shatter the frame; escape it for display only.
                let cells: Vec<String> = values.iter().map(|v| boxed_safe(&self.render(v))).collect();
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
        if self.broken.is_none() {
            if let Err(e) = self.out.flush() {
                self.broken = Some(e.kind());
            }
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
        let color = std::io::stdout().is_terminal() && g.color;
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
fn json_row(columns: &[String], values: &[Value]) -> String {
    let mut s = String::from("{");
    for (i, (c, v)) in columns.iter().zip(values.iter()).enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&serde_json::to_string(c).expect("strings serialize"));
        s.push(':');
        s.push_str(&v.to_string());
    }
    s.push('}');
    s
}

fn csv_cell(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
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

    #[test]
    fn csv_quoting() {
        assert_eq!(csv_cell("plain"), "plain");
        assert_eq!(csv_cell("a,b"), "\"a,b\"");
        assert_eq!(csv_cell("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn json_rows_keep_duplicate_keys() {
        let cols = vec!["a".to_string(), "a".to_string(), "b\"q".to_string()];
        let vals = vec![json!(1), json!(2), json!(null)];
        assert_eq!(json_row(&cols, &vals), r#"{"a":1,"a":2,"b\"q":null}"#);
        assert_eq!(json_row(&[], &[]), "{}");
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
