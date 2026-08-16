//! The pilot REPL (PLAN.md Phase 2): reedline with statement-aware
//! multi-line editing. A buffer is submitted when it ends with `;` outside
//! any string or comment — the same rule the duckdb shell uses — or when it
//! is a dot-command. History persists at ~/.harbor/history.

use reedline::{
    ColumnarMenu, DefaultHinter, Emacs, FileBackedHistory, KeyCode, KeyModifiers, MenuBuilder,
    Prompt, PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus, Reedline,
    ReedlineEvent, ReedlineMenu, Signal, ValidationResult, Validator, default_emacs_keybindings,
};
use std::borrow::Cow;

use crate::render::{Mode, RenderOpts};
use crate::{Conn, run_sql};

struct BerthPrompt {
    name: String,
}

impl Prompt for BerthPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Owned(format!("{} ", self.name))
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
/// non-whitespace byte is `;` outside strings and comments. This scanner is
/// the seed of `sqllex` (tier 1): it already speaks single/double quotes with
/// `''`-style escapes, dollar-quoting, `--` and nested `/* */` comments.
pub fn statement_complete(buf: &str) -> bool {
    let t = buf.trim();
    if t.is_empty() || t.starts_with('.') {
        return true;
    }
    let b = t.as_bytes();
    let mut i = 0;
    let mut last_code_byte = 0u8;
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' => {
                let q = b[i];
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        if i + 1 < b.len() && b[i + 1] == q {
                            i += 2; // '' or "" escape
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                if i >= b.len() {
                    return false; // unterminated literal
                }
                last_code_byte = q;
            }
            b'$' => {
                // dollar-quote: $tag$ ... $tag$
                let start = i;
                let mut j = i + 1;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                    j += 1;
                }
                if j < b.len() && b[j] == b'$' {
                    let tag = &t[start..=j];
                    match t[j + 1..].find(tag) {
                        Some(pos) => {
                            i = j + 1 + pos + tag.len() - 1;
                            last_code_byte = b'$';
                        }
                        None => return false, // unterminated
                    }
                } else {
                    last_code_byte = b'$';
                }
            }
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
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
                if depth > 0 {
                    return false; // unterminated comment
                }
                continue;
            }
            c => {
                if !c.is_ascii_whitespace() {
                    last_code_byte = c;
                }
            }
        }
        i += 1;
    }
    last_code_byte == b';'
}

pub fn run(conn: &Conn, name: &str) -> std::process::ExitCode {
    let history = crate::http::harbor_home().join("history");
    // Tab opens the completion menu, then cycles it (PLAN.md lanes A/B/C).
    let mut keybindings = default_emacs_keybindings();
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".to_string()),
            ReedlineEvent::MenuNext,
        ]),
    );
    let menu = ColumnarMenu::default().with_name("completion_menu");
    let mut line_editor = Reedline::create()
        .with_validator(Box::new(SqlValidator))
        .with_highlighter(Box::new(crate::highlight::SqlHighlighter))
        .with_hinter(Box::new(DefaultHinter::default()))
        .with_completer(Box::new(crate::complete::SqlCompleter::new(conn.clone())))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
        .with_edit_mode(Box::new(Emacs::new(keybindings)))
        .with_history(Box::new(
            FileBackedHistory::with_file(1000, history).expect("history file"),
        ));
    let prompt = BerthPrompt { name: name.to_string() };
    let mut opts = RenderOpts::default();
    eprintln!("pilot: connected to {name} (.help for help, .quit to leave)");

    loop {
        match line_editor.read_line(&prompt) {
            Ok(Signal::Success(buf)) => {
                let stmt = buf.trim();
                if stmt.is_empty() {
                    continue;
                }
                if let Some(cmd) = stmt.strip_prefix('.') {
                    match dot_command(cmd, conn, &mut opts) {
                        DotResult::Quit => return std::process::ExitCode::SUCCESS,
                        DotResult::Handled => continue,
                    }
                }
                // One statement per request is the protocol's rule; the
                // trailing terminator is ours to strip.
                let stmt = stmt.trim_end_matches(';').trim();
                if !stmt.is_empty() {
                    let _ = run_sql(conn, stmt, &opts);
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
}

fn dot_command(cmd: &str, conn: &Conn, opts: &mut RenderOpts) -> DotResult {
    let mut parts = cmd.split_whitespace();
    match parts.next().unwrap_or("") {
        "quit" | "exit" | "q" => return DotResult::Quit,
        "databases" | "db" => {
            let _ = crate::list_fleet();
        }
        "mode" => match parts.next() {
            None => println!("mode: {} (duckbox markdown csv json jsonlines line list trash)", opts.mode.name()),
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
            print!(concat!(
                "  .mode [m]         duckbox | markdown | csv | json | jsonlines | line | list | trash\n",
                "  .maxrows [n]      boxed-mode display cap (head … tail elision past it)\n",
                "  .nullvalue [s]    how NULL renders\n",
                "  .timer on|off     server + wall time per statement\n",
                "  .tables           SHOW TABLES\n",
                "  .schema [t]       CREATE statements, one table or all\n",
                "  .databases        the live fleet\n",
                "  .help  .quit      this text; leave (Ctrl-D too)\n",
                "  statements end with ;   Ctrl-C clears the line\n",
            ));
        }
        other => {
            eprintln!("pilot: no such command .{other} (.help lists them)");
        }
    }
    DotResult::Handled
}

#[cfg(test)]
mod tests {
    use super::statement_complete;

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
    }
}
