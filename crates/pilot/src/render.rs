//! Result rendering (PLAN.md Phase 2): duckbox and friends.
//!
//! The policy split is the whole design: BOXED modes (duckbox, markdown)
//! retain O(display) — up to max_rows in the head plus a ring of the last
//! max_rows/2 — count everything, and render after `end`; PIPE modes (csv,
//! json, jsonlines, line, list, trash) emit each row as it arrives with O(1)
//! memory. A 100M-row SELECT costs the boxed client nothing but time.

use harbor_protocol::Column;
use serde_json::Value;
use std::io::{BufWriter, IsTerminal, Stdout, Write};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Duckbox,
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
        !matches!(self, Mode::Duckbox | Mode::Markdown)
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
            columns: Vec::new(),
            types: Vec::new(),
            head: Vec::new(),
            tail: std::collections::VecDeque::new(),
            total: 0,
            emitted_first_json: false,
        }
    }

    pub fn schema(&mut self, cols: &[Column]) {
        self.columns = cols
            .iter()
            .enumerate()
            .map(|(i, c)| c.name.clone().unwrap_or_else(|| format!("col{i}")))
            .collect();
        self.types = cols.iter().map(|c| c.duckdb_type.clone()).collect();
        match self.opts.mode {
            Mode::Csv => {
                let hdr: Vec<String> = self.columns.iter().map(|c| csv_cell(c)).collect();
                let _ = writeln!(self.out, "{}", hdr.join(","));
            }
            Mode::Json => {
                let _ = write!(self.out, "[");
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
                let _ = writeln!(self.out, "{line}");
            }
            Mode::JsonLines => {
                let obj: serde_json::Map<String, Value> =
                    self.columns.iter().cloned().zip(values).collect();
                let _ = writeln!(self.out, "{}", Value::Object(obj));
            }
            Mode::Json => {
                let obj: serde_json::Map<String, Value> =
                    self.columns.iter().cloned().zip(values).collect();
                let sep = if self.emitted_first_json { "," } else { "" };
                self.emitted_first_json = true;
                let _ = write!(self.out, "{sep}\n{}", Value::Object(obj));
            }
            Mode::Line => {
                let w = self.columns.iter().map(|c| c.len()).max().unwrap_or(0);
                let lines: Vec<String> = self
                    .columns
                    .iter()
                    .zip(values.iter())
                    .map(|(c, v)| format!("{c:>w$} = {}", self.render(v)))
                    .collect();
                let _ = writeln!(self.out, "{}\n", lines.join("\n"));
            }
            Mode::List => {
                let line = values.iter().map(|v| self.render(v)).collect::<Vec<_>>().join("|");
                let _ = writeln!(self.out, "{line}");
            }
            Mode::Duckbox | Mode::Markdown => {
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

    pub fn end(mut self, row_count: u64, time_ms: u64, wall_ms: u128) {
        match self.opts.mode {
            Mode::Json => {
                let _ = writeln!(self.out, "\n]");
            }
            Mode::Duckbox => self.boxed(row_count, glyphs_duckbox()),
            Mode::Markdown => self.boxed(row_count, glyphs_markdown()),
            _ => {}
        }
        let _ = self.out.flush();
        if self.opts.timer {
            eprintln!("Run Time: server {time_ms} ms, wall {wall_ms} ms");
        } else if !self.opts.mode.is_streaming() {
            eprintln!("{row_count} rows ({time_ms} ms)");
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

        // Column widths from what will be shown, capped; values truncate.
        const MAX_COL: usize = 40;
        let ncols = self.columns.len();
        let widths: Vec<usize> = (0..ncols)
            .map(|i| {
                let mut w = display_width(&self.columns[i]);
                if g.type_row {
                    w = w.max(display_width(&self.types[i]));
                }
                for r in top.iter().chain(bottom.iter()) {
                    w = w.max(display_width(r.get(i).map(String::as_str).unwrap_or("")));
                }
                w.min(MAX_COL)
            })
            .collect();

        // Terminal fit: prune middle columns, marking with a "…" column.
        let term_w = terminal_width();
        let fits = |ws: &[usize], n_shown: usize, pruned: bool| {
            let sep = 3; // " │ " per gap, borders
            ws.iter().take(n_shown).sum::<usize>()
                + (n_shown + pruned as usize).saturating_sub(1) * sep
                + 4
                + if pruned { 3 } else { 0 }
                <= term_w
        };
        let mut left = ncols; // columns shown from the left before the … column
        let mut right = 0; // columns shown from the right
        if !fits(&widths, ncols, false) && ncols > 2 {
            left = 1;
            right = 1;
            loop {
                let mut ws: Vec<usize> = widths[..left].to_vec();
                ws.extend_from_slice(&widths[ncols - right..]);
                if !fits(&ws, ws.len(), true) {
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
        let pruned = left < ncols;
        let idx: Vec<Option<usize>> = if pruned {
            let mut v: Vec<Option<usize>> = (0..left).map(Some).collect();
            v.push(None); // the … column
            v.extend((ncols - right..ncols).map(Some));
            v
        } else {
            (0..ncols).map(Some).collect()
        };
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
            let types = self.types.clone();
            cells_line(&|i| types[i].clone(), false, &mut out);
        }
        rule(g.ml, g.mm, g.mr, &mut out);
        for r in top {
            let r = r.clone();
            cells_line(&|i| r.get(i).cloned().unwrap_or_default(), true, &mut out);
        }
        if spilled {
            cells_line(&|_i| "·".to_string(), false, &mut out);
            for r in bottom {
                let r = r.clone();
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
                note.push_str(&format!(", {ncols} columns ({} shown)", left + right));
            }
            out.push_str(&dim(&note));
            out.push('\n');
        }
        deliver(out);
    }
}

/// Boxed output taller than the terminal pages through $PAGER (default
/// `less -SRFX`: -S no-wrap since widths already fit, -F quit-if-one-screen).
/// Streaming modes never page — they already left the building.
fn deliver(out: String) {
    let term_h = match crossterm::terminal::size() {
        Ok((_, h)) if h > 0 => h as usize,
        _ => 40, // unknown height (odd pty): guess, don't page everything
    };
    let tall = out.lines().count() + 2 > term_h;
    if tall && std::io::stdout().is_terminal() {
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
    fn truncate_marks_elision() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 4), "abc");
    }
}
