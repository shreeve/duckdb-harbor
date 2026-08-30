//! The pilot REPL: reedline with statement-aware
//! multi-line editing. A buffer is submitted when it ends with `;` outside
//! any string or comment — the same rule the duckdb shell uses — or when it
//! is a dot-command. History persists at ~/.local/state/harbor/history.

use reedline::{
    ColumnarMenu, DefaultHinter, Emacs, FileBackedHistory, KeyCode, KeyModifiers, MenuBuilder,
    Keybindings, Prompt, PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus, Reedline,
    ReedlineEvent, ReedlineMenu, Signal, ValidationResult, Validator, Vi,
    default_emacs_keybindings, default_vi_insert_keybindings, default_vi_normal_keybindings,
};
use std::borrow::Cow;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::complete::SqlCompleter;
use crate::render::{Mode, RenderOpts};
use crate::scan::{Kind, scan};
use crate::{Conn, Outcome, run_sql};

/// Keep an idle-exit berth alive while Reedline is blocked at the prompt.
/// This is intentionally an activity pulse rather than a SQL session: a REPL
/// that is merely waiting for input must not reserve one of Harbor's scarce
/// transaction connections. The channel makes Drop prompt even when the next
/// pulse is many seconds away; a vanished Pilot leaves no server-side state.
struct ReplKeepalive {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ReplKeepalive {
    fn new(conn: Conn) -> Self {
        // Pulse synchronously first. Besides closing the race with an idle
        // deadline that was already near, this also keeps older Harbor builds
        // alive: their authenticated 404 is still a countable request.
        pulse(&conn);

        let Some(period) = keepalive_period(&conn) else {
            return Self { stop: None, thread: None };
        };
        let (tx, rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            while matches!(rx.recv_timeout(period), Err(mpsc::RecvTimeoutError::Timeout)) {
                pulse(&conn);
            }
        });
        Self { stop: Some(tx), thread: Some(thread) }
    }
}

impl Drop for ReplKeepalive {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn pulse(conn: &Conn) {
    if let Ok(resp) = crate::http::request(
        &conn.transport,
        &wire::endpoint::KEEPALIVE,
        conn.token.as_deref(),
        None,
        Some(Duration::from_secs(2)),
    ) {
        // Complete the exchange before closing the socket. The body is tiny;
        // its contents do not matter because activity was counted at dispatch.
        let _ = resp.body_string();
    }
}

/// New Harbor builds publish the configured idle window in /info. Pulse at a
/// third of it, capped so long windows do not make recovery from scheduling
/// stalls fragile. A missing field means an older Harbor; its normal 90s
/// summon window is safely covered by the compatibility fallback.
fn keepalive_period(conn: &Conn) -> Option<Duration> {
    let resp = match crate::http::request(
        &conn.transport,
        &wire::endpoint::INFO,
        conn.token.as_deref(),
        None,
        Some(Duration::from_secs(2)),
    ) {
        Ok(resp) => resp,
        Err(_) => return Some(Duration::from_secs(10)),
    };
    if resp.status != 200 {
        return Some(Duration::from_secs(10));
    }
    let Ok(text) = resp.body_string() else { return Some(Duration::from_secs(10)) };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Some(Duration::from_secs(10));
    };
    match doc.get("idleExitMs") {
        Some(serde_json::Value::Null) => None,
        Some(v) => {
            let Some(ms) = v.as_u64() else { return Some(Duration::from_secs(10)) };
            Some(Duration::from_millis((ms / 3).clamp(1, 10_000)))
        }
        None => Some(Duration::from_secs(10)),
    }
}

struct BerthPrompt {
    name: String,
}

impl Prompt for BerthPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        // No trailing space: the "> " indicator abuts the berth name, so the
        // prompt reads `chk> `, not `chk > `.
        Cow::Borrowed(self.name.as_str())
    }
    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_indicator(&self, _: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("> ")
    }
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("  … ")
    }
    fn render_prompt_history_search_indicator(&self, s: PromptHistorySearch) -> Cow<'_, str> {
        let tag = match s.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "failing ",
        };
        Cow::Owned(format!("({tag}search: {}) ", s.term))
    }
}

struct SqlValidator;

impl Validator for SqlValidator {
    fn validate(&self, line: &str) -> ValidationResult {
        if statement_complete(line) {
            ValidationResult::Complete
        } else {
            ValidationResult::Incomplete
        }
    }
}

/// Complete = a dot-command, an empty line, or a buffer whose last
/// non-whitespace code byte is `;` — judged by the shared scanner (scan.rs),
/// so the validator, splitter, and highlighter can never disagree.
pub fn statement_complete(buf: &str) -> bool {
    let t = buf.trim();
    if t.is_empty() || t.starts_with('.') {
        return true;
    }
    let mut last = 0u8;
    for sp in scan(t) {
        if !sp.terminated {
            return false;
        }
        match sp.kind {
            Kind::Code => {
                for &c in &t.as_bytes()[sp.start..sp.end] {
                    if !c.is_ascii_whitespace() {
                        last = c;
                    }
                }
            }
            // A trailing literal is content, not a terminator (any non-`;`
            // byte does as the marker).
            Kind::Str | Kind::Dollar => last = b'x',
            Kind::LineComment | Kind::BlockComment => {}
        }
    }
    last == b';'
}

/// Split a buffer at `;` terminators in code spans — same scanner, applied
/// cutwise, so `.read` scripts and `a; b;` buffers obey the validator's rules.
/// Segments with no code (a trailing `-- comment`, a stray `;`) are dropped:
/// the server takes statements, not commentary.
pub fn split_statements(buf: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut push = |stmt: &str| {
        if has_code(stmt) {
            out.push(stmt.to_string());
        }
    };
    for sp in scan(buf) {
        if sp.kind != Kind::Code {
            continue;
        }
        for (off, &c) in buf.as_bytes()[sp.start..sp.end].iter().enumerate() {
            if c == b';' {
                push(buf[start..sp.start + off].trim());
                start = sp.start + off + 1;
            }
        }
    }
    push(buf[start..].trim());
    out
}

/// Anything besides whitespace and comments?
fn has_code(s: &str) -> bool {
    scan(s).iter().any(|sp| match sp.kind {
        Kind::Code => s[sp.start..sp.end].bytes().any(|b| !b.is_ascii_whitespace()),
        Kind::Str | Kind::Dollar => true,
        Kind::LineComment | Kind::BlockComment => false,
    })
}

/// The one list of dot-commands: dispatch validates against it, `.help`
/// prints it, and the completer's lane A suggests from it.
pub const DOT_COMMANDS: &[(&str, &str, &str)] = &[
    ("mode", "[m]", "duckbox | duckboxy | markdown | csv | json | jsonlines | line | list | trash"),
    ("maxrows", "[n]", "boxed-mode display cap (head … tail elision past it)"),
    ("nullvalue", "[s]", "how NULL renders"),
    ("timer", "on|off", "server + wall time per statement"),
    ("tables", "", "SHOW TABLES"),
    ("schema", "[t]", "CREATE statements, one table or all"),
    ("databases", "", "the live fleet"),
    ("open", "<target>", "switch database (name, path, url)"),
    ("read", "<file.sql>", "run a script, statement by statement"),
    ("keymode", "vi|emacs", "keybindings (default emacs)"),
    ("theme", "[name]", "syntax colors: duck | mono | vivid"),
    ("appearance", "auto|light|dark", "light/dark palette (auto detects)"),
    ("help", "", "this text"),
    ("quit", "", "leave (Ctrl-D too)"),
];

fn bind_completion_keys(kb: &mut Keybindings) {
    // The inline gray suggestion is a history hint. Right accepts it in
    // Reedline's defaults, which makes ordinary cursor movement surprising;
    // Tab is the conventional and more reachable acceptance key.
    kb.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::MenuAccept,
            ReedlineEvent::HistoryHintComplete,
        ]),
    );

    // The panel is visually below the prompt, so Down opens it and subsequent
    // Down presses move through it. If no menu is installed, retain Reedline's
    // normal next-line/history behavior as the final fallback.
    kb.add_binding(
        KeyModifiers::NONE,
        KeyCode::Down,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::MenuDown,
            ReedlineEvent::Menu("completion_menu".to_string()),
            ReedlineEvent::Down,
        ]),
    );

    // Right navigates an open panel first. With no panel, it accepts the inline
    // history suggestion when one exists, then falls back to cursor movement.
    kb.add_binding(
        KeyModifiers::NONE,
        KeyCode::Right,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::MenuRight,
            ReedlineEvent::HistoryHintComplete,
            ReedlineEvent::Right,
        ]),
    );
}

fn make_editor(completer: &SqlCompleter, vi: bool) -> Reedline {
    let edit_mode: Box<dyn reedline::EditMode> = if vi {
        let mut insert = default_vi_insert_keybindings();
        bind_completion_keys(&mut insert);
        Box::new(Vi::new(insert, default_vi_normal_keybindings()))
    } else {
        let mut kb = default_emacs_keybindings();
        bind_completion_keys(&mut kb);
        Box::new(Emacs::new(kb))
    };
    let menu = ColumnarMenu::default().with_name("completion_menu");
    let history = crate::config::history_file();
    Reedline::create()
        .with_validator(Box::new(SqlValidator))
        .with_highlighter(Box::new(crate::highlight::SqlHighlighter))
        .with_hinter(Box::new(DefaultHinter::default()))
        .with_completer(Box::new(completer.clone()))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
        .with_edit_mode(edit_mode)
        .with_history({
            // A read-only or unwritable HARBOR_HOME (restrictive perms, a path
            // component that is a file) must not crash the REPL on startup:
            // reedline's with_file creates the parent and can fail. Fall back to
            // in-memory history and say so, rather than .expect() panicking with
            // a backtrace before the prompt is even drawn.
            let history: Box<dyn reedline::History> =
                match history.map(|p| FileBackedHistory::with_file(1000, p)) {
                    Some(Ok(h)) => Box::new(h),
                    _ => {
                        eprintln!("pilot: history file unavailable; using in-memory history");
                        Box::new(FileBackedHistory::default())
                    }
                };
            history
        })
}

pub fn run(conn: &Conn, name: &str, mut opts: RenderOpts) -> std::process::ExitCode {
    let mut conn = conn.clone();
    let mut _keepalive = ReplKeepalive::new(conn.clone());
    let mut vi = false;
    // One completer for the session: its catalog cache loads lazily on the
    // first Tab and survives editor rebuilds (.keymode), refreshing on .open.
    let completer = SqlCompleter::new(conn.clone());
    let mut line_editor = make_editor(&completer, vi);
    // No greeting: the prompt appearing IS the connection confirmed, and
    // its name says to what. Discovery lives in .help; fanfare helps no one.
    let mut prompt = BerthPrompt { name: name.to_string() };

    loop {
        match line_editor.read_line(&prompt) {
            Ok(Signal::Success(buf)) => {
                let stmt = buf.trim();
                if stmt.is_empty() {
                    continue;
                }
                if let Some(cmd) = stmt.strip_prefix('.') {
                    match dot_command(cmd, &conn, &mut opts) {
                        DotResult::Quit => return std::process::ExitCode::SUCCESS,
                        DotResult::Handled => continue,
                        DotResult::Open(target) => {
                            // A fresh config read on purpose: .open should
                            // see entries added since the session began.
                            let (cfg, cfg_err) = crate::config::load();
                            match crate::resolve(&cfg, &target, None, cfg_err.as_ref()) {
                                Ok(c) => {
                                    conn = c;
                                    _keepalive = ReplKeepalive::new(conn.clone());
                                    completer.reconnect(conn.clone());
                                    // The prompt changing name announces the switch.
                                    prompt = BerthPrompt { name: crate::prompt_name(&target) };
                                }
                                Err(e) => eprintln!("pilot: {e}"),
                            }
                            continue;
                        }
                        DotResult::Keymode(v) => {
                            vi = v;
                            line_editor = make_editor(&completer, vi);
                            eprintln!("pilot: {} keybindings", if vi { "vi" } else { "emacs" });
                            continue;
                        }
                    }
                }
                // A fresh submission starts with a clean cancel flag. The
                // SIGINT handler sets CANCEL whenever pilot is in cooked mode,
                // which includes the pager: a Ctrl-C aimed at `less` (or an
                // external `kill -INT`) would otherwise linger and skip the
                // next statement the user types. Clearing here, once, drops
                // that staleness while preserving the intra-buffer skip below
                // (a Ctrl-C during `a; b; c` still aborts b and c — those
                // checks are inside this loop, with no read_line between them).
                crate::CANCEL.store(false, std::sync::atomic::Ordering::Relaxed);
                // One statement per request is the protocol's rule; the
                // trailing terminator is ours to strip. Multi-statement
                // buffers split at terminators outside strings/comments,
                // and stop at the first failure or Ctrl-C.
                for stmt in split_statements(stmt) {
                    if run_sql(&conn, &stmt, &opts) != Outcome::Done {
                        break;
                    }
                }
            }
            Ok(Signal::CtrlC) => continue, // clear the line, keep the REPL
            Ok(Signal::CtrlD) => return std::process::ExitCode::SUCCESS,
            Ok(_) => continue, // other signals (resize, etc.): nothing to do
            Err(e) => {
                eprintln!("pilot: editor error: {e}");
                return std::process::ExitCode::FAILURE;
            }
        }
    }
}

enum DotResult {
    Quit,
    Handled,
    Open(String),
    Keymode(bool),
}

fn dot_command(cmd: &str, conn: &Conn, opts: &mut RenderOpts) -> DotResult {
    let mut parts = cmd.split_whitespace();
    // Short aliases (.q .exit .db .h) dispatch here but stay out of
    // DOT_COMMANDS on purpose: help and completion teach the long names.
    match parts.next().unwrap_or("") {
        "quit" | "exit" | "q" => return DotResult::Quit,
        "open" => match parts.next() {
            Some(t) => return DotResult::Open(t.to_string()),
            None => eprintln!("pilot: .open <name|path|url>"),
        },
        "keymode" => match parts.next() {
            Some("vi") => return DotResult::Keymode(true),
            Some("emacs") => return DotResult::Keymode(false),
            _ => eprintln!("pilot: .keymode vi|emacs"),
        },
        "theme" => match parts.next() {
            None => {
                let (name, _) = crate::theme::describe();
                println!("theme: {name} ({})", crate::theme::NAMES.join(" "));
            }
            Some(name) if crate::theme::set_theme(name) => eprintln!("pilot: theme {name}"),
            Some(name) => {
                eprintln!("pilot: unknown theme {name:?} ({})", crate::theme::NAMES.join(" "))
            }
        },
        "appearance" => {
            use crate::theme::Appearance::{Dark, Light};
            match parts.next() {
                None => {
                    let (_, a) = crate::theme::describe();
                    println!("appearance: {}", if a == Light { "light" } else { "dark" });
                }
                Some("light") => crate::theme::set_appearance(Light),
                Some("dark") => crate::theme::set_appearance(Dark),
                Some("auto") => crate::theme::set_appearance(crate::theme::detect_appearance()),
                Some(other) => eprintln!("pilot: .appearance auto|light|dark (got {other:?})"),
            }
        }
        "read" => match parts.next() {
            Some(f) => match std::fs::read_to_string(crate::config::expand(f)) {
                Ok(text) => {
                    for stmt in split_statements(&text) {
                        if run_sql(conn, &stmt, opts) != Outcome::Done {
                            break; // a failure or a Ctrl-C aborts the script
                        }
                    }
                }
                Err(e) => eprintln!("pilot: .read {f}: {e}"),
            },
            None => eprintln!("pilot: .read <file.sql>"),
        },
        "databases" | "db" => {
            // Reloaded, not captured at startup: an edit made in another window
            // is exactly what someone typing `.databases` wants to see.
            let _ = crate::show_fleet(&crate::config::load().0);
        }
        "mode" => match parts.next() {
            None => println!("mode: {} (duckbox duckboxy markdown csv json jsonlines line list trash)", opts.mode.name()),
            Some(m) => match Mode::parse(m) {
                Some(m) => opts.mode = m,
                None => eprintln!("pilot: unknown mode {m:?}"),
            },
        },
        "maxrows" => match parts.next().and_then(|n| n.parse::<usize>().ok()) {
            Some(n) if n > 0 => opts.max_rows = n,
            _ => println!("maxrows: {}", opts.max_rows),
        },
        "nullvalue" => match parts.next() {
            Some(s) => opts.null = s.to_string(),
            None => println!("nullvalue: {:?}", opts.null),
        },
        "timer" => match parts.next() {
            Some("on") => opts.timer = true,
            Some("off") => opts.timer = false,
            _ => println!("timer: {}", if opts.timer { "on" } else { "off" }),
        },
        "tables" => {
            let _ = run_sql(conn, "SHOW TABLES", opts);
        }
        "schema" => {
            let sql = match parts.next() {
                Some(t) => format!(
                    "SELECT sql FROM duckdb_tables() WHERE table_name = '{}'",
                    t.replace('\'', "''")
                ),
                None => "SELECT sql FROM duckdb_tables()".to_string(),
            };
            let _ = run_sql(conn, &sql, &RenderOpts { mode: Mode::List, ..opts.clone() });
        }
        "help" | "h" => {
            for (name, args, what) in DOT_COMMANDS {
                println!("  {:<18} {what}", format!(".{name} {args}"));
            }
            println!("  statements end with ;   Ctrl-C clears the line");
        }
        other => {
            eprintln!("pilot: no such command .{other} (.help lists them)");
        }
    }
    DotResult::Handled
}

#[cfg(test)]
mod tests {
    use super::{bind_completion_keys, split_statements, statement_complete};
    use reedline::{KeyCode, KeyModifiers, ReedlineEvent, default_emacs_keybindings};

    #[test]
    fn completion_keys_follow_the_visual_model() {
        let mut kb = default_emacs_keybindings();
        bind_completion_keys(&mut kb);

        assert_eq!(
            kb.find_binding(KeyModifiers::NONE, KeyCode::Tab),
            Some(ReedlineEvent::UntilFound(vec![
                ReedlineEvent::MenuAccept,
                ReedlineEvent::HistoryHintComplete,
            ]))
        );
        assert_eq!(
            kb.find_binding(KeyModifiers::NONE, KeyCode::Down),
            Some(ReedlineEvent::UntilFound(vec![
                ReedlineEvent::MenuDown,
                ReedlineEvent::Menu("completion_menu".to_string()),
                ReedlineEvent::Down,
            ]))
        );
        assert_eq!(
            kb.find_binding(KeyModifiers::NONE, KeyCode::Right),
            Some(ReedlineEvent::UntilFound(vec![
                ReedlineEvent::MenuRight,
                ReedlineEvent::HistoryHintComplete,
                ReedlineEvent::Right,
            ]))
        );
    }

    #[test]
    fn splitter_respects_quoting() {
        assert_eq!(split_statements("select 1; select 2;"), vec!["select 1", "select 2"]);
        assert_eq!(split_statements("select ';'; select 2"), vec!["select ';'", "select 2"]);
        assert_eq!(split_statements("select $$a;b$$"), vec!["select $$a;b$$"]);
        assert_eq!(
            split_statements("-- c;\nselect 1 /* ; */; select \"a;b\""),
            vec!["-- c;\nselect 1 /* ; */", "select \"a;b\""]
        );
        assert!(split_statements("  ;  ; ").is_empty());
        // multi-byte chars around terminators never split mid-char
        assert_eq!(split_statements("select 'あ'; select “x”"), vec!["select 'あ'", "select “x”"]);
        // comment-only segments are not statements (the server would error)
        assert_eq!(split_statements("SELECT 1; -- done"), vec!["SELECT 1"]);
        assert_eq!(
            split_statements("SELECT 1;\n-- gap\n;\nSELECT 2;"),
            vec!["SELECT 1", "SELECT 2"]
        );
        assert_eq!(split_statements("/* all\ncomment */"), Vec::<String>::new());
    }

    #[test]
    fn terminator_rules() {
        assert!(statement_complete("SELECT 1;"));
        assert!(statement_complete("SELECT 1 ; "));
        assert!(!statement_complete("SELECT 1"));
        assert!(!statement_complete("SELECT ';' "));
        assert!(statement_complete("SELECT ';';"));
        assert!(!statement_complete("SELECT 'unterminated"));
        assert!(!statement_complete("SELECT 1 -- comment;"));
        assert!(statement_complete("SELECT 1; -- trailing comment"));
        assert!(!statement_complete("SELECT /* ; */ 1"));
        assert!(statement_complete("SELECT /* nested /* ; */ */ 1;"));
        assert!(!statement_complete("SELECT $$ ; $$"));
        assert!(statement_complete("SELECT $$ ; $$;"));
        assert!(statement_complete("SELECT $tag$ ; $tag$;"));
        assert!(!statement_complete("SELECT $tag$ ; "));
        assert!(statement_complete(".quit"));
        assert!(statement_complete(""));
        assert!(statement_complete("SELECT 'it''s'; "));
        // a$b$c is an identifier, not an open dollar-quote
        assert!(statement_complete("SELECT a$b$c;"));
    }
}
