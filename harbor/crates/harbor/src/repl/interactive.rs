//! The harbor REPL: reedline with statement-aware
//! multi-line editing. A buffer is submitted when it ends with `;` outside
//! any string or comment — the same rule the duckdb shell uses — or when it
//! is a dot-command. History persists at ~/.local/state/harbor/history.

use reedline::{
    ColumnarMenu, DefaultHinter, Emacs, FileBackedHistory, KeyCode, KeyModifiers, MenuBuilder,
    Keybindings, Prompt, PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus, Reedline,
    ReedlineEvent, ReedlineMenu, Signal, ValidationResult, Validator, Vi,
    default_emacs_keybindings, default_vi_insert_keybindings, default_vi_normal_keybindings,
    default_vi_visual_keybindings,
};
use std::borrow::Cow;

use crate::repl::complete::SqlCompleter;
use crate::repl::render::{Mode, RenderOpts};
use wire::scan::{Kind, scan};
use crate::repl::http::{Anchor, Transport};
use crate::repl::{Outcome, Transaction};

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
/// non-whitespace code byte is `;` — judged by the shared scanner (`wire::scan`),
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

/// One step of a script: a statement, or a dot command.
#[derive(Debug, PartialEq)]
pub enum Step {
    Sql(String),
    /// The command's line without its dot: `mode csv`.
    Dot(String),
}

/// A script as the duckdb shell reads one: statements split as the REPL
/// splits a buffer, and a line that begins with `.` where a statement would
/// begin is a dot command, the rest of its line its argument. Blank lines and
/// `--` comment lines before it go with it.
pub fn script(text: &str) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut at = 0;
    while at < text.len() {
        if let Some(dot) = dot_ahead(text, at) {
            let end = line_end(text, dot);
            steps.push(Step::Dot(text[dot..end].trim().trim_start_matches('.').to_string()));
            at = end;
            continue;
        }
        // The statements up to the first terminator a dot command follows.
        // A dot command can hold what the scanner would read as an open
        // string (`.read o'brien.sql`), so what follows one is scanned anew.
        let rest = &text[at..];
        let mut cut = rest.len();
        'find: for sp in scan(rest).iter().filter(|sp| sp.kind == Kind::Code) {
            for (off, &c) in rest.as_bytes()[sp.start..sp.end].iter().enumerate() {
                if c == b';' && dot_ahead(rest, sp.start + off + 1).is_some() {
                    cut = sp.start + off + 1;
                    break 'find;
                }
            }
        }
        steps.extend(split_statements(&rest[..cut]).into_iter().map(Step::Sql));
        at += cut;
    }
    steps
}

/// Where the line holding byte `at` ends, past its newline.
fn line_end(text: &str, at: usize) -> usize {
    text[at..].find('\n').map_or(text.len(), |p| at + p + 1)
}

/// The start of the dot command's line when one comes next after `at`, past
/// the rest of the line `at` is on and any lines with nothing for the engine.
fn dot_ahead(text: &str, mut at: usize) -> Option<usize> {
    let mut fresh = at == 0 || text[..at].ends_with('\n');
    while at < text.len() {
        let end = line_end(text, at);
        let line = text[at..end].trim();
        if fresh && line.starts_with('.') {
            return Some(at);
        }
        if !(line.is_empty() || line.starts_with("--")) {
            return None;
        }
        (at, fresh) = (end, true);
    }
    None
}

/// Run a script's steps in order, the dot commands as the prompt runs them,
/// stopping at the first statement that does not finish. `.quit` ends it.
pub fn run_steps(steps: Vec<Step>, opts: &mut RenderOpts, transaction: &mut Transaction) -> Outcome {
    for step in steps {
        let outcome = match step {
            Step::Sql(sql) => transaction.run(&sql, opts),
            Step::Dot(cmd) => match dot_command(&cmd, opts, transaction) {
                DotResult::Handled(outcome) => outcome,
                DotResult::Quit => return Outcome::Done,
                DotResult::Open(_) | DotResult::Keymode(_) => {
                    eprintln!("harbor: .{cmd} works at the prompt, not in a script");
                    Outcome::Failed
                }
            },
        };
        if outcome != Outcome::Done {
            return outcome;
        }
    }
    Outcome::Done
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
    // Tab accepts: the completion picked in an open panel, or else the
    // inline gray suggestion, which is a history hint.
    kb.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::MenuAccept,
            ReedlineEvent::HistoryHintComplete,
        ]),
    );

    // Up and Down mean history wherever history is: on a recalled line,
    // edited or not, until it is emptied. The live line at the bottom is the
    // one place Down has nothing to do — nothing newer sits below the
    // present — and there it opens the completion panel, which sits below
    // the prompt where Down points. With the panel open, Down moves through
    // it. Reedline's Down reports itself inapplicable when it moved nothing,
    // which is what lets the fallback fire.
    kb.add_binding(
        KeyModifiers::NONE,
        KeyCode::Down,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::MenuDown,
            ReedlineEvent::Down,
            ReedlineEvent::Menu("completion_menu".to_string()),
        ]),
    );

    // Ctrl-Space opens the panel anywhere, history or not, the way an editor
    // does; open, it steps to the next entry. Terminals disagree on how they
    // send the chord — a control-modified space, or the NUL it maps to — so
    // both spellings are bound.
    for (modifiers, code) in [
        (KeyModifiers::CONTROL, KeyCode::Char(' ')),
        (KeyModifiers::CONTROL, KeyCode::Null),
        (KeyModifiers::NONE, KeyCode::Null),
    ] {
        kb.add_binding(
            modifiers,
            code,
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::MenuNext,
                ReedlineEvent::Menu("completion_menu".to_string()),
            ]),
        );
    }

    // Right moves within an open panel and accepts nothing there. With no
    // panel it accepts the history hint, which shows only at the end of the
    // line, and anywhere else it moves the cursor.
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
        Box::new(Vi::new(insert, default_vi_normal_keybindings(), default_vi_visual_keybindings()))
    } else {
        let mut kb = default_emacs_keybindings();
        bind_completion_keys(&mut kb);
        Box::new(Emacs::new(kb))
    };
    // The menu closes at the end of the word it was opened for: any character
    // that cannot extend an identifier or a qualified name ends it, so a
    // stale menu never intercepts a later Enter. `_` and `.` keep it open.
    let menu = ColumnarMenu::default().with_name("completion_menu").with_word_chars("_.");
    // The history holds whatever was typed, a `CREATE SECRET` among it, so
    // its directory is the user's alone before the file is made in it.
    let history = harbor_common::history_file()
        .ok()
        .filter(|p| p.parent().is_none_or(|dir| harbor_common::perms::ensure_private_dir(dir).is_ok()));
    Reedline::create()
        .with_validator(Box::new(SqlValidator))
        .with_highlighter(Box::new(crate::repl::highlight::SqlHighlighter))
        .with_hinter(Box::new(DefaultHinter::default()))
        .with_completer(Box::new(completer.clone()))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
        .with_edit_mode(edit_mode)
        .with_history({
            // A read-only or unwritable HARBOR_HOME (restrictive perms, a path
            // component that is a file) must not end the REPL at startup:
            // reedline's with_file creates the parent and can fail. History is
            // then kept in memory, and the REPL says so.
            let history: Box<dyn reedline::History> =
                match history.map(|p| FileBackedHistory::with_file(1000, p)) {
                    Some(Ok(h)) => Box::new(h),
                    _ => {
                        eprintln!("harbor: history file unavailable; using in-memory history");
                        Box::new(FileBackedHistory::default())
                    }
                };
            history
        })
}

pub fn run(
    transport: &Transport,
    name: &str,
    mut opts: RenderOpts,
    mut _anchor: Option<Anchor>,
) -> std::process::ExitCode {
    let mut vi = false;
    // One completer for the session: its catalog cache loads lazily on the
    // first Tab and survives editor rebuilds (.keymode), refreshing on .open.
    let completer = SqlCompleter::new(transport.clone());
    let mut line_editor = make_editor(&completer, vi);
    // No greeting: the prompt appearing IS the connection confirmed, and
    // its name says to what. Discovery lives in .help; fanfare helps no one.
    let mut prompt = BerthPrompt { name: name.to_string() };
    // One transaction for the prompt and for `.read`: a file may open what
    // the next line typed commits.
    let mut transaction = Transaction::new(transport, true);

    loop {
        match line_editor.read_line(&prompt) {
            Ok(Signal::Success(buf)) => {
                let stmt = buf.trim();
                if stmt.is_empty() {
                    continue;
                }
                // A fresh submission starts with a clean cancel flag. The
                // SIGINT handler sets CANCEL whenever the repl is in cooked mode,
                // which includes the pager: a Ctrl-C aimed at `less` (or an
                // external `kill -INT`) would otherwise linger and skip the
                // next statement, typed or read by a dot command. Clearing
                // here, once, drops that staleness while preserving the
                // intra-buffer skip below (a Ctrl-C during `a; b; c` still
                // aborts b and c — those checks are inside the loops, with no
                // read_line between them).
                crate::repl::CANCEL.store(false, std::sync::atomic::Ordering::Relaxed);
                if let Some(cmd) = stmt.strip_prefix('.') {
                    match dot_command(cmd, &mut opts, &mut transaction) {
                        DotResult::Quit => return std::process::ExitCode::SUCCESS,
                        DotResult::Handled(_) => continue,
                        DotResult::Open(target) => {
                            match crate::repl::resolve(&target, &[]) {
                                Ok((t, name)) => {
                                    // Moor at the new server before letting
                                    // the old mooring go: the switch must
                                    // never be the moment both lifetimes hit
                                    // zero clients.
                                    let moored = crate::repl::http::hold(&t).ok();
                                    // A transaction belongs to the server it
                                    // was opened on, and ends with the visit:
                                    // released while that server is still
                                    // held up, as at exit.
                                    transaction = Transaction::new(&t, true);
                                    _anchor = moored;
                                    completer.reconnect(t);
                                    // The prompt changing name announces the switch.
                                    prompt = BerthPrompt { name };
                                }
                                Err(e) => eprintln!("harbor: {e}"),
                            }
                            continue;
                        }
                        DotResult::Keymode(v) => {
                            vi = v;
                            line_editor = make_editor(&completer, vi);
                            eprintln!("harbor: {} keybindings", if vi { "vi" } else { "emacs" });
                            continue;
                        }
                    }
                }
                // One statement per request is the protocol's rule; the
                // trailing terminator is ours to strip. Multi-statement
                // buffers split at terminators outside strings/comments,
                // and stop at the first failure or Ctrl-C.
                for stmt in split_statements(stmt) {
                    if transaction.run(&stmt, &opts) != Outcome::Done {
                        break;
                    }
                }
            }
            Ok(Signal::CtrlC) => continue, // clear the line, keep the REPL
            Ok(Signal::CtrlD) => return std::process::ExitCode::SUCCESS,
            Ok(_) => continue, // other signals (resize, etc.): nothing to do
            Err(e) => {
                eprintln!("harbor: editor error: {e}");
                return std::process::ExitCode::FAILURE;
            }
        }
    }
}

enum DotResult {
    Quit,
    /// Done here, with what became of it: a script stops on anything but
    /// `Done`, as it does after a statement.
    Handled(Outcome),
    Open(String),
    Keymode(bool),
}

/// How deep `.read` nests: a file that reads itself ends here, said, rather
/// than when the stack runs out.
const READ_DEPTH: usize = 32;

fn dot_command(cmd: &str, opts: &mut RenderOpts, transaction: &mut Transaction) -> DotResult {
    // The argument is the rest of the line, so a path may hold a space.
    let (name, arg) = cmd.trim().split_once(char::is_whitespace).unwrap_or((cmd.trim(), ""));
    let arg = Some(arg.trim()).filter(|a| !a.is_empty());
    // A mistake ends a script as a failed statement does: said, and the rest
    // left unrun. At the prompt it is said, and the prompt goes on.
    let failed = |why: String| {
        eprintln!("harbor: {why}");
        DotResult::Handled(Outcome::Failed)
    };
    let done = DotResult::Handled(Outcome::Done);
    // Short aliases (.q .exit .db .h) dispatch here but stay out of
    // DOT_COMMANDS on purpose: help and completion teach the long names.
    match name {
        "quit" | "exit" | "q" => DotResult::Quit,
        "open" => match arg {
            Some(t) => DotResult::Open(t.to_string()),
            None => failed(".open <name|path|url>".into()),
        },
        "keymode" => match arg {
            Some("vi") => DotResult::Keymode(true),
            Some("emacs") => DotResult::Keymode(false),
            _ => failed(".keymode vi|emacs".into()),
        },
        "theme" => match arg {
            None => {
                let (name, _) = crate::repl::theme::describe();
                println!("theme: {name} ({})", crate::repl::theme::NAMES.join(" "));
                done
            }
            Some(name) if crate::repl::theme::set_theme(name) => {
                eprintln!("harbor: theme {name}");
                done
            }
            Some(name) => failed(format!("unknown theme {name:?} ({})", crate::repl::theme::NAMES.join(" "))),
        },
        "appearance" => {
            use crate::repl::theme::Appearance::{Dark, Light};
            match arg {
                None => {
                    let (_, a) = crate::repl::theme::describe();
                    println!("appearance: {}", if a == Light { "light" } else { "dark" });
                }
                Some("light") => crate::repl::theme::set_appearance(Light),
                Some("dark") => crate::repl::theme::set_appearance(Dark),
                Some("auto") => crate::repl::theme::set_appearance(crate::repl::theme::detect_appearance()),
                Some(other) => return failed(format!(".appearance auto|light|dark (got {other:?})")),
            }
            done
        }
        "read" => {
            use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
            static DEPTH: AtomicUsize = AtomicUsize::new(0);
            let Some(f) = arg else { return failed(".read <file.sql>".into()) };
            if DEPTH.load(Relaxed) >= READ_DEPTH {
                return failed(format!(".read {f}: files read inside each other more than {READ_DEPTH} deep"));
            }
            match std::fs::read_to_string(harbor_common::paths::expand(f)) {
                // A failure or a Ctrl-C ends the file, and the script it is in.
                Ok(text) => {
                    DEPTH.fetch_add(1, Relaxed);
                    let outcome = run_steps(script(&text), opts, transaction);
                    DEPTH.fetch_sub(1, Relaxed);
                    DotResult::Handled(outcome)
                }
                Err(e) => failed(format!(".read {f}: {e}")),
            }
        }
        "databases" | "db" => {
            // The same list bare `harbor` prints — one reconcile, not a
            // second one that agrees most of the time.
            let _ = crate::repl::list_main();
            done
        }
        "mode" => match arg.map(|m| (m, Mode::parse(m))) {
            None => {
                println!("mode: {} (duckbox duckboxy markdown csv json jsonlines line list trash)", opts.mode.name());
                done
            }
            Some((_, Some(m))) => {
                opts.mode = m;
                done
            }
            Some((m, None)) => failed(format!("unknown mode {m:?}")),
        },
        "maxrows" => match arg.map(|n| (n, n.parse::<usize>())) {
            None => {
                println!("maxrows: {}", opts.max_rows);
                done
            }
            Some((_, Ok(n))) if n > 0 => {
                opts.max_rows = n;
                done
            }
            Some((n, _)) => failed(format!(".maxrows takes a count above zero, not {n:?}")),
        },
        "nullvalue" => {
            match arg {
                Some(s) => opts.null = s.to_string(),
                None => println!("nullvalue: {:?}", opts.null),
            }
            done
        }
        "timer" => match arg {
            Some("on") => {
                opts.timer = true;
                done
            }
            Some("off") => {
                opts.timer = false;
                done
            }
            None => {
                println!("timer: {}", if opts.timer { "on" } else { "off" });
                done
            }
            Some(other) => failed(format!(".timer on|off (got {other:?})")),
        },
        "tables" => DotResult::Handled(transaction.run("SHOW TABLES", opts)),
        "schema" => {
            let sql = match arg {
                Some(t) => format!(
                    "SELECT sql FROM duckdb_tables() WHERE table_name = '{}'",
                    t.replace('\'', "''")
                ),
                None => "SELECT sql FROM duckdb_tables()".to_string(),
            };
            DotResult::Handled(transaction.run(&sql, &RenderOpts { mode: Mode::List, ..opts.clone() }))
        }
        "help" | "h" => {
            for (name, args, what) in DOT_COMMANDS {
                println!("  {:<18} {what}", format!(".{name} {args}"));
            }
            println!("  statements end with ;   Ctrl-C clears the line");
            println!("  Up/Down walk history; Down on the live line, or Ctrl-Space anywhere, lists completions; Tab accepts one");
            done
        }
        other => failed(format!("no such command .{other} (.help lists them)")),
    }
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
                ReedlineEvent::Down,
                ReedlineEvent::Menu("completion_menu".to_string()),
            ]))
        );
        for (modifiers, code) in [
            (KeyModifiers::CONTROL, KeyCode::Char(' ')),
            (KeyModifiers::CONTROL, KeyCode::Null),
            (KeyModifiers::NONE, KeyCode::Null),
        ] {
            assert_eq!(
                kb.find_binding(modifiers, code),
                Some(ReedlineEvent::UntilFound(vec![
                    ReedlineEvent::MenuNext,
                    ReedlineEvent::Menu("completion_menu".to_string()),
                ]))
            );
        }
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
    fn a_script_runs_dot_commands_where_a_statement_would_begin() {
        use super::{Step, script};
        let sql = |s: &str| Step::Sql(s.to_string());
        let dot = |s: &str| Step::Dot(s.to_string());
        assert_eq!(
            script(".mode csv\nselect 1;\n  .nullvalue (none)\n-- note\n.timer on\nselect 2; select 3"),
            vec![dot("mode csv"), sql("select 1"), dot("nullvalue (none)"), dot("timer on"), sql("select 2"), sql("select 3")]
        );
        // Mid-statement, or after code on its line, a dot is SQL's.
        assert_eq!(script("select\n.5;"), vec![sql("select\n.5")]);
        assert_eq!(script("select 1; .mode csv"), vec![sql("select 1"), sql(".mode csv")]);
        // A dot line inside a string is the string's.
        assert_eq!(script("select '\n.mode csv\n';"), vec![sql("select '\n.mode csv\n'")]);
        // An argument the scanner would read as an open string ends with its line.
        assert_eq!(script(".read o'brien.sql\nselect 1;"), vec![dot("read o'brien.sql"), sql("select 1")]);
        assert_eq!(script(""), vec![]);
        assert_eq!(script("-- only a comment\n"), vec![]);
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
