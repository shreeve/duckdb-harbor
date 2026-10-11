//! Completion in the Query editor (docs/QUERY.md, "Completion").
//!
//! The engine answers: DuckDB's own completer, `sql_auto_complete`, asked
//! through Harbor about the caret's statement up to the caret, on a pooled
//! connection and never the view's transaction session. The sidebar's
//! catalog answers what the engine cannot: a qualified name's members, and
//! everything when the server cannot load the completer or keeps not
//! answering in time.
//!
//! The editor's own popup shows the list. Typing a name's character or a
//! `.` opens it, ⌃Space summons it, ↑/↓ move through it, Tab or a click
//! accepts, Escape closes it, and Enter always inserts a newline. The kit is
//! told what was typed through its completion provider, which answers that
//! it will take it from there; everything after that is here, so the
//! debounce, the stale answer and the server that cannot complete are this
//! module's decisions, each a pure function below.

use gpui_kit::component::input::{
    CompletionProvider, EditorState, Enter, Escape, Rope, RopeExt as _,
};
use gpui_kit::{App, AppContext as _, Focusable as _, Modifiers, Subscription, Task, WeakEntity, Window};
use harbor_client::complete::Suggestion;
use harbor_client::{Catalog, Conn, Failure};
use lsp_types::{CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

/// How long typing must pause before the engine is asked: a burst of keys
/// asks once, about the text it ends with.
const DEBOUNCE: Duration = Duration::from_millis(80);
/// How long an answer may take. Over the local socket the engine answers in
/// a millisecond or two, and a pooled connection's first question after the
/// catalog changed takes about a hundred (measured: 97 to 134 ms on a loaded
/// machine). One that has not answered by this is no help to the keystroke
/// that asked, and the catalog answers instead.
const PATIENCE: Duration = Duration::from_millis(300);
/// A first load may fetch the extension.
const LOAD_PATIENCE: Duration = Duration::from_secs(30);
/// Answers missed in a row before the engine is not asked again.
const MISSES: u8 = 3;
/// The longest statement the engine is asked about, in bytes: a pasted dump
/// is the catalog's to complete.
const LONGEST: usize = 64 * 1024;
/// The most suggestions shown.
const MOST: usize = 100;

// ---------------------------------------------------------------------------
// Decisions
// ---------------------------------------------------------------------------

/// Whether what was just typed opens the list: a name's characters, or the
/// `.` that asks for a qualified name's members.
pub(super) fn opens(typed: &str) -> bool {
    typed == "." || (!typed.is_empty() && typed.chars().all(|c| c == '_' || c.is_alphanumeric()))
}

/// A character the engine's tokenizer keeps inside a bare name.
fn name_char(c: char) -> bool {
    c == '_' || c == '$' || !(c.is_ascii_punctuation() || c.is_whitespace())
}

/// Where the caret asks for a completion.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Site {
    /// The caret's statement, from the end of the `;` before it to the `;`
    /// after it or the end of the text.
    pub stmt: std::ops::Range<usize>,
    /// Where the name being completed begins, the `"` that opens a quoted
    /// name included.
    pub word: usize,
    pub caret: usize,
    /// Where an accepted suggestion's replacement ends: the caret, or the
    /// closing quote of the quoted name the caret is in.
    pub end: usize,
    /// The name before a `.` that qualifies this one, unquoted.
    pub qualifier: Option<String>,
}

impl Site {
    /// What is typed of the name.
    pub fn typed<'a>(&self, text: &'a str) -> &'a str {
        &text[self.word..self.caret]
    }

    /// What the engine is asked about: the statement up to the caret.
    pub fn asked<'a>(&self, text: &'a str) -> &'a str {
        &text[self.stmt.start..self.caret]
    }
}

/// Where the caret in `text` asks for a completion, read with the lexer the
/// server uses (`wire::scan`). None inside a string, a dollar quote or a
/// comment, and in a number.
pub(super) fn site(text: &str, caret: usize) -> Option<Site> {
    use wire::scan::Kind;
    if caret > text.len() || !text.is_char_boundary(caret) {
        return None;
    }
    let bytes = text.as_bytes();
    let spans = wire::scan::scan(text);
    // The span that holds the character before the caret.
    let before = caret.checked_sub(1).and_then(|b| spans.iter().rfind(|s| s.start <= b));
    let (word, end) = match before {
        None => (caret, caret),
        Some(s) if s.kind == Kind::Code => {
            let word = text[s.start..caret]
                .char_indices()
                .rev()
                .take_while(|&(_, c)| name_char(c))
                .last()
                .map_or(caret, |(i, _)| s.start + i);
            // A number, or a parameter, is no name.
            if text[word..caret].starts_with(|c: char| c.is_ascii_digit() || c == '$') {
                return None;
            }
            (word, caret)
        }
        // In a quoted name: the replacement takes in its closing quote.
        Some(s) if s.kind == Kind::Str && bytes[s.start] == b'"' && (caret < s.end || !s.terminated) => {
            (s.start, if s.terminated { s.end } else { caret })
        }
        // In a string, a dollar quote or a comment, or past the end of an
        // open one; a line comment runs to the end of its line.
        Some(s) if caret < s.end || !s.terminated || s.kind == Kind::LineComment => return None,
        // Just past a closed literal or comment.
        Some(_) => (caret, caret),
    };
    let qualifier = text[..word].strip_suffix('.').and_then(|head| {
        if head.ends_with('"') {
            let quoted = spans.iter().find(|s| s.end == head.len() && s.kind == Kind::Str && s.terminated)?;
            let inner = text.get(quoted.start + 1..quoted.end - 1)?;
            (bytes[quoted.start] == b'"').then(|| inner.replace("\"\"", "\""))
        } else {
            let name: String = {
                let mut chars: Vec<char> = head.chars().rev().take_while(|&c| name_char(c)).collect();
                chars.reverse();
                chars.into_iter().collect()
            };
            (!name.is_empty() && !name.starts_with(|c: char| c.is_ascii_digit() || c == '$')).then_some(name)
        }
    });
    let semicolons: Vec<usize> = spans
        .iter()
        .filter(|s| s.kind == Kind::Code)
        .flat_map(|s| (s.start..s.end).filter(|&i| bytes[i] == b';'))
        .collect();
    let start = semicolons.iter().rev().find(|&&i| i < word).map_or(0, |&i| i + 1);
    let stop = semicolons.iter().find(|&&i| i >= caret).copied().unwrap_or(text.len());
    Some(Site { stmt: start..stop, word, caret, end, qualifier })
}

/// Whether a site is worth a list: one summoned always is; one typed into
/// needs part of a name, or the `.` after a qualifier.
pub(super) fn worth(site: &Site, text: &str, summoned: bool) -> bool {
    summoned || !site.typed(text).is_empty() || site.qualifier.is_some()
}

/// Whether the engine can be asked.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Engine {
    /// Not asked yet: its completer is loaded on first need.
    Untried,
    Loading,
    /// It answers. `misses` counts the answers in a row that did not come.
    Ready { misses: u8 },
    /// It cannot complete, or kept not answering: the catalog answers for
    /// as long as the view lasts.
    Unavailable,
}

/// Who answers one request for a list.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Plan {
    /// Load the engine's completer; the catalog answers meanwhile.
    Load,
    Ask,
    Catalog,
}

/// Who answers, for a statement of `asked` bytes up to the caret. A
/// qualified name whose `members` the catalog knows is the catalog's: after
/// a bare `table.` the engine names operators, not columns.
pub(super) fn plan(engine: Engine, asked: usize, members: bool) -> Plan {
    match engine {
        _ if members || asked > LONGEST => Plan::Catalog,
        Engine::Untried => Plan::Load,
        Engine::Ready { .. } => Plan::Ask,
        Engine::Loading | Engine::Unavailable => Plan::Catalog,
    }
}

/// The engine after asking the server to load its completer. A request that
/// never left can be tried again; a refusal, or no answer, ends the asking.
pub(super) fn loaded(answer: Result<(), &Failure>) -> Engine {
    match answer {
        Ok(()) => Engine::Ready { misses: 0 },
        Err(Failure::Unsent(_)) => Engine::Untried,
        Err(_) => Engine::Unavailable,
    }
}

/// The engine after an answer to a question. The engine's error about the
/// text is a verdict on the text; one saying it has no `sql_auto_complete`
/// ends the asking, and so does the third answer in a row that did not
/// come.
pub(super) fn answered(engine: Engine, answer: Result<(), &Failure>) -> Engine {
    let misses = match engine {
        Engine::Ready { misses } => misses,
        _ => 0,
    };
    match answer {
        Ok(()) => Engine::Ready { misses: 0 },
        Err(Failure::Refused { code, message }) if code == wire::code::SQL_ERROR => {
            if message.contains("sql_auto_complete") {
                Engine::Unavailable
            } else {
                Engine::Ready { misses }
            }
        }
        Err(_) if misses + 1 >= MISSES => Engine::Unavailable,
        Err(_) => Engine::Ready { misses: misses + 1 },
    }
}

/// What a suggestion names.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Kind {
    Keyword,
    Table,
    Column,
    Function,
    Schema,
    Other,
}

impl Kind {
    /// The kind in the engine's words.
    fn of(engine: &str) -> Kind {
        match engine {
            "keyword" => Kind::Keyword,
            "table" => Kind::Table,
            "column" => Kind::Column,
            "schema" | "catalog" => Kind::Schema,
            k if k.contains("function") || k.contains("macro") => Kind::Function,
            _ => Kind::Other,
        }
    }

    /// The word shown beside a suggestion.
    fn label(self) -> &'static str {
        match self {
            Kind::Keyword => "keyword",
            Kind::Table => "table",
            Kind::Column => "column",
            Kind::Function => "function",
            Kind::Schema => "schema",
            Kind::Other => "",
        }
    }

    fn lsp(self) -> CompletionItemKind {
        match self {
            Kind::Keyword => CompletionItemKind::KEYWORD,
            Kind::Table => CompletionItemKind::STRUCT,
            Kind::Column => CompletionItemKind::FIELD,
            Kind::Function => CompletionItemKind::FUNCTION,
            Kind::Schema => CompletionItemKind::MODULE,
            Kind::Other => CompletionItemKind::TEXT,
        }
    }
}

/// One suggestion: the text that replaces what is typed from `start`, a
/// byte offset in the editor's text.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Item {
    pub text: String,
    pub start: usize,
    pub kind: Kind,
}

/// The engine's suggestions for the text from `base` to the caret, placed in
/// the editor's text.
pub(super) fn from_engine(suggestions: &[Suggestion], base: usize) -> Vec<Item> {
    suggestions
        .iter()
        .map(|s| Item { text: s.text.clone(), start: base + s.start, kind: Kind::of(&s.kind) })
        .collect()
}

/// A name as typed: bare when it is a plain identifier, quoted otherwise.
pub(super) fn quoted(name: &str) -> String {
    let plain = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// A name folded for comparing: unquoted, in lower case.
fn fold(name: &str) -> String {
    name.trim_start_matches('"').trim_end_matches('"').replace("\"\"", "\"").to_lowercase()
}

/// Whether a suggestion continues what is typed, case and quotes aside.
fn continues(item: &Item, text: &str, caret: usize) -> bool {
    text.get(item.start..caret).is_some_and(|typed| fold(&item.text).starts_with(&fold(typed)))
}

/// The table an alias in `stmt` stands for: the name before it, after FROM,
/// JOIN or a comma, with or without AS. Only names, quoted or bare, count,
/// and only in code; a suggestion is all that rides on it.
pub(super) fn alias_target(stmt: &str, alias: &str) -> Option<String> {
    use wire::scan::Kind as Span;
    #[derive(PartialEq)]
    enum Tok {
        Name(String),
        Mark(char),
    }
    let mut toks = Vec::new();
    for span in wire::scan::scan(stmt) {
        let piece = &stmt[span.start..span.end];
        match span.kind {
            Span::Str if piece.starts_with('"') => {
                toks.push(Tok::Name(piece.trim_matches('"').replace("\"\"", "\"")))
            }
            Span::Code => {
                let mut name = String::new();
                for c in piece.chars() {
                    if name_char(c) {
                        name.push(c);
                        continue;
                    }
                    if !name.is_empty() {
                        toks.push(Tok::Name(std::mem::take(&mut name)));
                    }
                    if !c.is_whitespace() {
                        toks.push(Tok::Mark(c));
                    }
                }
                if !name.is_empty() {
                    toks.push(Tok::Name(name));
                }
            }
            _ => {}
        }
    }
    let is = |t: Option<&Tok>, word: &str| matches!(t, Some(Tok::Name(n)) if n.eq_ignore_ascii_case(word));
    for (i, tok) in toks.iter().enumerate() {
        if !matches!(tok, Tok::Name(n) if n.eq_ignore_ascii_case(alias)) {
            continue;
        }
        let mut j = i;
        if j > 0 && is(toks.get(j - 1), "as") {
            j -= 1;
        }
        let Some(Tok::Name(table)) = j.checked_sub(1).and_then(|k| toks.get(k)) else { continue };
        // Past a schema's `name.`, to what stands before the table.
        let mut k = j - 1;
        if k >= 2 && toks[k - 1] == Tok::Mark('.') && matches!(toks[k - 2], Tok::Name(_)) {
            k -= 2;
        }
        let lead = k.checked_sub(1).and_then(|p| toks.get(p));
        if is(lead, "from") || is(lead, "join") || lead == Some(&Tok::Mark(',')) {
            return Some(table.clone());
        }
    }
    None
}

/// The catalog's suggestions for a site: a qualifier's members (a table's
/// or an alias's columns, a schema's tables), or else the tables, schemas
/// and columns whose names continue what is typed.
pub(super) fn from_catalog(catalog: &Catalog, site: &Site, text: &str) -> Vec<Item> {
    let typed = fold(site.typed(text));
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut offer = |name: &str, kind: Kind| {
        let key = name.to_lowercase();
        if key.starts_with(&typed) && seen.insert(key) {
            out.push(Item { text: quoted(name), start: site.word, kind });
        }
    };
    match &site.qualifier {
        Some(q) => {
            let alias = alias_target(&text[site.stmt.clone()], q);
            for table in &catalog.tables {
                let named = table.name.eq_ignore_ascii_case(q)
                    || alias.as_deref().is_some_and(|a| table.name.eq_ignore_ascii_case(a));
                if named {
                    table.columns.iter().for_each(|c| offer(&c.name, Kind::Column));
                }
            }
            for table in catalog.tables.iter().filter(|t| t.schema.eq_ignore_ascii_case(q)) {
                offer(&table.name, Kind::Table);
            }
        }
        None => {
            catalog.tables.iter().for_each(|t| offer(&t.name, Kind::Table));
            catalog.schemas().into_iter().filter(|s| *s != "main").for_each(|s| offer(s, Kind::Schema));
            for table in &catalog.tables {
                table.columns.iter().for_each(|c| offer(&c.name, Kind::Column));
            }
        }
    }
    out.truncate(MOST);
    out
}

/// The list shown: a qualified name's members from the catalog when it
/// knows them; else the engine's suggestions that continue what is typed, in
/// the engine's order, then the catalog's names the engine left out (it
/// answers about twenty for a typed prefix). The engine also offers names
/// that merely contain what is typed (`depends` for `pe`, `(` for `SEL`);
/// those show only when nothing continues it, as the guesses at a typo they
/// are. Without the engine, the catalog's alone. One entry per name.
pub(super) fn merge(engine: Option<Vec<Item>>, catalog: Vec<Item>, site: &Site, text: &str) -> Vec<Item> {
    if site.qualifier.is_some() && !catalog.is_empty() {
        return catalog;
    }
    let engine: Vec<Item> = engine
        .unwrap_or_default()
        .into_iter()
        .filter(|i| i.start >= site.stmt.start && i.start <= site.caret && text.is_char_boundary(i.start))
        .collect();
    let (first, rest): (Vec<Item>, Vec<Item>) = engine.into_iter().partition(|i| continues(i, text, site.caret));
    let guesses = if first.is_empty() && catalog.is_empty() { rest } else { Vec::new() };
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<Item> = first
        .into_iter()
        .chain(catalog)
        .chain(guesses)
        .filter(|i| seen.insert(fold(&i.text)))
        .collect();
    out.truncate(MOST);
    out
}

/// The list last shown, and where.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Shown {
    /// The statement before the name.
    pub head: String,
    pub word: usize,
    pub typed: String,
    pub items: Vec<Item>,
}

impl Shown {
    pub fn of(site: &Site, text: &str, items: Vec<Item>) -> Self {
        Shown {
            head: text[site.stmt.start..site.word].to_string(),
            word: site.word,
            typed: site.typed(text).to_string(),
            items,
        }
    }
}

/// While a name grows, the list shown narrows to what still continues it,
/// at once and with no question asked, so the list stays up between the
/// keystroke and the engine's answer. Only the same name in the same
/// statement narrows.
pub(super) fn narrow(shown: &Shown, site: &Site, text: &str) -> Option<Vec<Item>> {
    let typed = site.typed(text);
    let same = shown.word == site.word
        && shown.head == text[site.stmt.start..site.word]
        && typed.len() > shown.typed.len()
        && fold(typed).starts_with(&fold(&shown.typed));
    same.then(|| shown.items.iter().filter(|i| continues(i, text, site.caret)).cloned().collect())
}

/// How many bytes of `label` match `typed`, letter for letter with case
/// aside: the part of a suggestion the list highlights.
pub(super) fn matched(label: &str, typed: &str) -> usize {
    let mut typed = typed.chars().flat_map(char::to_lowercase);
    let mut len = 0;
    for c in label.chars() {
        let mut lower = c.to_lowercase();
        let fits = lower.all(|l| typed.next() == Some(l));
        if !fits {
            break;
        }
        len += c.len_utf8();
    }
    len
}

/// The question an answer was for. An answer lands only while the question
/// is still the newest asked and the text and caret are still those it
/// asked about.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Asked {
    ticket: u64,
    caret: usize,
    text: u64,
}

impl Asked {
    pub fn of(ticket: u64, text: &str, caret: usize) -> Self {
        use std::hash::{Hash as _, Hasher as _};
        let mut h = std::hash::DefaultHasher::new();
        text.hash(&mut h);
        Asked { ticket, caret, text: h.finish() }
    }
}

/// What a keystroke does to completion.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Key {
    /// Take the selected suggestion (Tab).
    Accept,
    /// Close the list and let the key do its own work (Enter: a newline).
    Dismiss,
    /// Close the list without sending the key through it (⌘Enter runs).
    Close,
    /// Open the list where the caret is (⌃Space).
    Summon,
    Pass,
}

pub(super) fn key(key: &str, m: &Modifiers, open: bool) -> Key {
    let bare = !(m.platform || m.control || m.alt || m.shift || m.function);
    match key {
        "space" if m.control && !(m.platform || m.alt || m.shift) => Key::Summon,
        "tab" if open && bare => Key::Accept,
        "enter" if open && m.platform => Key::Close,
        "enter" if open => Key::Dismiss,
        _ => Key::Pass,
    }
}

// ---------------------------------------------------------------------------
// The editor's side
// ---------------------------------------------------------------------------

/// Completion for one Query view's editor, on its berth's connection.
pub(crate) struct Completer {
    shared: Rc<Shared>,
}

struct Shared {
    conn: Conn,
    editor: WeakEntity<EditorState>,
    /// What the kit last said was typed, and the caret it leaves.
    typed: RefCell<Option<(String, usize)>>,
    /// The newest question, bumped by every change and every summons.
    ticket: Cell<u64>,
    engine: Cell<Engine>,
    shown: RefCell<Option<Shown>>,
    /// The question in flight: replacing it drops the one before.
    asking: RefCell<Task<()>>,
}

/// The kit's completion provider, as a listener: it is told each typed
/// text, records it, and answers that the kit need not ask for a list. The
/// list is presented from here instead (`present_completion_items`), since
/// the kit's own request shows one answer per keystroke, and the list that
/// narrows at once and is then replaced by the engine's answer is two.
struct Typing(Weak<Shared>);

impl CompletionProvider for Typing {
    fn completions(
        &self,
        _: &Rope,
        _: usize,
        _: CompletionContext,
        _: &mut Window,
        _: &mut App,
    ) -> Task<anyhow::Result<CompletionResponse>> {
        Task::ready(Ok(CompletionResponse::Array(Vec::new())))
    }

    fn is_completion_trigger(&self, offset: usize, new_text: &str, _: &mut App) -> bool {
        if let Some(shared) = self.0.upgrade() {
            shared.typed.replace(Some((new_text.to_string(), offset + new_text.len())));
        }
        false
    }
}

impl Completer {
    pub(crate) fn new(conn: Conn, editor: &gpui_kit::Entity<EditorState>, cx: &mut App) -> Self {
        let shared = Rc::new(Shared {
            conn,
            editor: editor.downgrade(),
            typed: RefCell::new(None),
            ticket: Cell::new(0),
            engine: Cell::new(Engine::Untried),
            shown: RefCell::new(None),
            asking: RefCell::new(Task::ready(())),
        });
        let typing = Rc::new(Typing(Rc::downgrade(&shared)));
        editor.update(cx, |e, _| e.lsp_mut().completion_provider = Some(typing));
        Completer { shared }
    }

    /// The keys completion takes before the editor's bindings see them.
    pub(crate) fn keys(&self, cx: &mut App) -> Subscription {
        let shared = Rc::downgrade(&self.shared);
        cx.intercept_keystrokes(move |ev, window, cx| {
            let Some(shared) = shared.upgrade() else { return };
            let Some(editor) = shared.editor.upgrade() else { return };
            let (focused, open) = {
                let e = editor.read(cx);
                (e.focus_handle(cx).is_focused(window), e.completion_menu_state().open)
            };
            if !focused {
                return;
            }
            match key(&ev.keystroke.key, &ev.keystroke.modifiers, open) {
                Key::Accept => {
                    let enter = Box::new(Enter { secondary: false, shift: false });
                    if editor.update(cx, |e, cx| e.route_overlay_action(enter, window, cx)) {
                        cx.stop_propagation();
                    }
                }
                Key::Dismiss => {
                    editor.update(cx, |e, cx| e.route_overlay_action(Box::new(Escape), window, cx));
                }
                Key::Close => editor.update(cx, |e, cx| e.dismiss_completion_overlay(cx)),
                Key::Summon => {
                    ask(&shared, bump(&shared), true, window, cx);
                    cx.stop_propagation();
                }
                Key::Pass => {}
            }
        })
    }

    /// The editor's text changed. Typing a name or a `.` asks for a list;
    /// any change outdates the questions before it.
    pub(crate) fn changed(&self, window: &mut Window, cx: &mut App) {
        let shared = &self.shared;
        let typed = shared.typed.take();
        let ticket = bump(shared);
        let Some(editor) = shared.editor.upgrade() else { return };
        let caret = editor.read(cx).cursor();
        if typed.is_some_and(|(typed, after)| after == caret && opens(&typed)) {
            ask(shared, ticket, false, window, cx);
        }
    }
}

fn bump(shared: &Shared) -> u64 {
    shared.ticket.set(shared.ticket.get() + 1);
    shared.ticket.get()
}

/// Answer the caret: from the catalog at once, or from the engine after the
/// debounce.
fn ask(shared: &Rc<Shared>, ticket: u64, summoned: bool, window: &mut Window, cx: &mut App) {
    let Some(editor) = shared.editor.upgrade() else { return };
    let (text, caret) = {
        let e = editor.read(cx);
        (e.value().to_string(), e.cursor())
    };
    let Some(site) = site(&text, caret).filter(|s| worth(s, &text, summoned)) else { return };
    let catalog = catalog_items(&site, &text, cx);
    if !summoned {
        let narrowed = shared.shown.borrow().as_ref().and_then(|s| narrow(s, &site, &text));
        if let Some(items) = narrowed {
            present(shared, &site, &text, items, window, cx);
        }
    }
    let members = site.qualifier.is_some() && !catalog.is_empty();
    match plan(shared.engine.get(), site.asked(&text).len(), members) {
        Plan::Catalog => {
            let items = merge(None, catalog, &site, &text);
            present(shared, &site, &text, items, window, cx);
        }
        Plan::Load => {
            shared.engine.set(Engine::Loading);
            let items = merge(None, catalog, &site, &text);
            present(shared, &site, &text, items, window, cx);
            let (weak, conn) = (Rc::downgrade(shared), shared.conn.clone());
            window
                .spawn(cx, async move |cx| {
                    let answer = cx
                        .background_spawn(async move { harbor_client::complete::load(&conn, LOAD_PATIENCE) })
                        .await;
                    let Some(shared) = weak.upgrade() else { return };
                    shared.engine.set(loaded(answer.as_ref().map(|_| ())));
                    // The question that waited for the load is asked, if it
                    // is still the newest.
                    if shared.ticket.get() == ticket && matches!(shared.engine.get(), Engine::Ready { .. }) {
                        cx.update(|window, cx| ask(&shared, ticket, summoned, window, cx)).ok();
                    }
                })
                .detach();
        }
        Plan::Ask => {
            let (weak, conn) = (Rc::downgrade(shared), shared.conn.clone());
            let asked = Asked::of(ticket, &text, caret);
            let question = site.asked(&text).to_string();
            let task = window.spawn(cx, async move |cx| {
                if !summoned {
                    cx.background_executor().timer(DEBOUNCE).await;
                }
                if weak.upgrade().is_none_or(|s| s.ticket.get() != ticket) {
                    return;
                }
                let answer = cx
                    .background_spawn(async move { harbor_client::complete::suggest(&conn, &question, PATIENCE) })
                    .await;
                let Some(shared) = weak.upgrade() else { return };
                shared.engine.set(answered(shared.engine.get(), answer.as_ref().map(|_| ())));
                cx.update(|window, cx| {
                    let Some(editor) = shared.editor.upgrade() else { return };
                    let now = {
                        let e = editor.read(cx);
                        Asked::of(shared.ticket.get(), &e.value(), e.cursor())
                    };
                    if now != asked {
                        return;
                    }
                    let engine = answer.ok().map(|s| from_engine(&s, site.stmt.start));
                    let items = merge(engine, catalog, &site, &text);
                    present(&shared, &site, &text, items, window, cx);
                })
                .ok();
            });
            *shared.asking.borrow_mut() = task;
        }
    }
}

/// The sidebar catalog's suggestions: the connected database's, which is
/// this view's.
fn catalog_items(site: &Site, text: &str, cx: &App) -> Vec<Item> {
    let Some(view) = crate::app_view(cx) else { return Vec::new() };
    match &view.read(cx).phase {
        crate::app::Phase::Connected { catalog, .. } => from_catalog(catalog, site, text),
        _ => Vec::new(),
    }
}

/// Show `items` in the editor's popup, an empty list closing it, while the
/// editor has the keyboard.
fn present(shared: &Shared, site: &Site, text: &str, items: Vec<Item>, window: &mut Window, cx: &mut App) {
    let Some(editor) = shared.editor.upgrade() else { return };
    if !editor.read(cx).focus_handle(cx).is_focused(window) {
        return;
    }
    let typed = site.typed(text).to_string();
    let rope: Rope = editor.read(cx).text().clone();
    let end = rope.offset_to_position(site.end);
    let lsp = items
        .iter()
        .map(|item| {
            let range = lsp_types::Range::new(rope.offset_to_position(item.start), end);
            let typed = text.get(item.start..site.caret).unwrap_or_default();
            let kind = item.kind.label();
            CompletionItem {
                label: item.text.clone(),
                kind: Some(item.kind.lsp()),
                detail: (!kind.is_empty()).then(|| kind.to_string()),
                // The kit highlights as many bytes of the label as the
                // filter text holds.
                filter_text: Some(item.text[..matched(&item.text, typed)].to_string()),
                text_edit: Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
                    range,
                    new_text: item.text.clone(),
                })),
                ..Default::default()
            }
        })
        .collect();
    shared.shown.replace(Some(Shown::of(site, text, items)));
    editor.update(cx, |e, cx| e.present_completion_items(site.word, typed, lsp, cx));
}

#[cfg(test)]
mod tests {
    // NOT `use super::*`: the glob would bring gpui's `test` attribute in
    // over the built-in one.
    use super::{
        alias_target, answered, from_catalog, from_engine, key, loaded, matched, merge, narrow,
        opens, plan, quoted, site, worth, Asked, Engine, Item, Key, Kind, Plan, Shown, Site,
    };
    use harbor_client::complete::Suggestion;
    use harbor_client::{Catalog, Failure};

    /// `text` with the caret where `|` stands.
    fn at(marked: &str) -> (String, usize) {
        let caret = marked.find('|').expect("a caret");
        (marked.replacen('|', "", 1), caret)
    }

    fn site_of(marked: &str) -> Option<(Site, String)> {
        let (text, caret) = at(marked);
        site(&text, caret).map(|s| (s, text))
    }

    #[test]
    fn a_name_or_a_dot_opens_the_list_and_nothing_else_does() {
        for typed in ["s", "E", "_", "é", "peo", "."] {
            assert!(opens(typed), "{typed:?}");
        }
        for typed in ["", " ", "\n", ",", "(", "..", "a b", "'", "\"", "*"] {
            assert!(!opens(typed), "{typed:?}");
        }
    }

    #[test]
    fn the_site_is_the_name_before_the_caret_in_its_statement() {
        let (s, text) = site_of("SELECT 1; SELECT * FROM pe|").unwrap();
        assert_eq!((s.typed(&text), s.asked(&text), s.qualifier.as_deref()), ("pe", " SELECT * FROM pe", None));
        assert_eq!((s.stmt, s.end), (9..26, 26));
        // The statement runs on past the caret to its `;`, for aliases
        // declared after it.
        let (s, text) = site_of("SELECT p.na| FROM people p; SELECT 2").unwrap();
        assert_eq!((s.typed(&text), s.qualifier.as_deref()), ("na", Some("p")));
        assert_eq!(&text[s.stmt.clone()], "SELECT p.na FROM people p");
        // Right after a `;`, a statement begins.
        let (s, text) = site_of("SELECT 1;s|").unwrap();
        assert_eq!(s.asked(&text), "s");
        // A `;` inside a string or a comment divides nothing.
        let (s, text) = site_of("SELECT ';' -- ;\n, na|").unwrap();
        assert_eq!(s.stmt.start, 0);
        assert_eq!(s.typed(&text), "na");
        // Multi-byte names are whole, and offsets count bytes.
        let (s, text) = site_of("SELECT é, naïve|").unwrap();
        assert_eq!((s.word, s.typed(&text)), (11, "naïve"));
        // An em dash is part of a name to the engine's tokenizer.
        assert_eq!(site_of("SELECT —peo|").unwrap().0.word, 7);
        // At the start of the text, or after a space, the name is empty.
        assert_eq!(site_of("|").unwrap().0.word, 0);
        assert_eq!(site_of("SELECT |").unwrap().0.word, 7);
    }

    #[test]
    fn no_site_in_a_string_a_comment_or_a_number() {
        for marked in [
            "SELECT 'peo|",
            "SELECT 'pe|o'",
            "SELECT $$pe|$$",
            "SELECT 1 -- pe|",
            "SELECT 1 -- pe|\nFROM t",
            "SELECT /* pe| */ 1",
            "SELECT /* pe|",
            "SELECT 12|",
            "SELECT 1.5|",
            "SELECT $1|",
        ] {
            assert_eq!(site_of(marked), None, "{marked:?}");
        }
        // Just past a closed string or comment, a name begins.
        assert_eq!(site_of("SELECT 'a'|").unwrap().0.word, 10);
        assert_eq!(site_of("SELECT /* x */|").unwrap().0.word, 14);
        // A line comment's line ends, and the next line is code.
        let (s, text) = site_of("SELECT 1 -- c\nFR|").unwrap();
        assert_eq!(s.typed(&text), "FR");
    }

    #[test]
    fn a_quoted_name_is_completed_whole_quotes_and_all() {
        // Open: the replacement runs to the caret.
        let (s, text) = site_of("SELECT * FROM \"Ord|").unwrap();
        assert_eq!((s.typed(&text), s.end), ("\"Ord", text.len()));
        // Closed by the editor's auto-close: it takes in the closing quote.
        let (s, text) = site_of("SELECT * FROM \"Ord|\" WHERE").unwrap();
        assert_eq!((s.typed(&text), s.end), ("\"Ord", 19));
        // Just past a closed quoted name, nothing of it is typed.
        assert_eq!(site_of("SELECT \"a\"|").unwrap().0.word, 10);
    }

    #[test]
    fn a_qualifier_is_the_name_before_the_dot_unquoted() {
        let q = |m: &str| site_of(m).unwrap().0.qualifier;
        assert_eq!(q("SELECT people.|"), Some("people".into()));
        assert_eq!(q("SELECT people.em|"), Some("people".into()));
        assert_eq!(q("SELECT sales.invoices.to|"), Some("invoices".into()));
        assert_eq!(q("SELECT \"Order Items\".q|"), Some("Order Items".into()));
        assert_eq!(q("SELECT \"a\"\"b\".|"), Some("a\"b".into()));
        assert_eq!(q("SELECT x|"), None);
        assert_eq!(q("SELECT (1).|"), None);
        assert_eq!(q("SELECT 'a'.|"), None);
    }

    #[test]
    fn a_typed_site_needs_a_name_or_a_qualifier_and_a_summoned_one_does_not() {
        let w = |m: &str, summoned| {
            let (s, text) = site_of(m).unwrap();
            worth(&s, &text, summoned)
        };
        assert!(w("SELECT na|", false));
        assert!(w("SELECT people.|", false));
        assert!(!w("SELECT |", false));
        assert!(w("SELECT |", true));
    }

    fn refused(message: &str) -> Failure {
        Failure::Refused { code: wire::code::SQL_ERROR.into(), message: message.into() }
    }

    #[test]
    fn the_engine_is_loaded_once_asked_while_it_answers_and_left_when_it_cannot() {
        assert_eq!(plan(Engine::Untried, 10, false), Plan::Load);
        assert_eq!(plan(Engine::Loading, 10, false), Plan::Catalog);
        assert_eq!(plan(Engine::Ready { misses: 2 }, 10, false), Plan::Ask);
        assert_eq!(plan(Engine::Unavailable, 10, false), Plan::Catalog);
        // A qualified name's members, and a pasted dump, are the catalog's.
        assert_eq!(plan(Engine::Ready { misses: 0 }, 10, true), Plan::Catalog);
        assert_eq!(plan(Engine::Ready { misses: 0 }, 64 * 1024 + 1, false), Plan::Catalog);

        assert_eq!(loaded(Ok(())), Engine::Ready { misses: 0 });
        assert_eq!(loaded(Err(&refused("Extension \"autocomplete\" not found"))), Engine::Unavailable);
        assert_eq!(loaded(Err(&Failure::Unanswered("timed out".into()))), Engine::Unavailable);
        assert_eq!(loaded(Err(&Failure::Unsent("refused".into()))), Engine::Untried);
    }

    #[test]
    fn three_missed_answers_in_a_row_end_the_asking_and_an_answer_resets_the_count() {
        let late = Failure::Unanswered("timed out".into());
        let mut engine = Engine::Ready { misses: 0 };
        engine = answered(engine, Err(&late));
        engine = answered(engine, Err(&late));
        assert_eq!(engine, Engine::Ready { misses: 2 });
        assert_eq!(answered(engine, Ok(())), Engine::Ready { misses: 0 });
        assert_eq!(answered(engine, Err(&late)), Engine::Unavailable);
        let busy = Failure::Refused { code: wire::code::UNAVAILABLE.into(), message: "not serving".into() };
        assert_eq!(answered(engine, Err(&busy)), Engine::Unavailable);
        // The engine's verdict on the text is an answer, and counts no miss;
        // one that it has no completer ends the asking.
        assert_eq!(answered(engine, Err(&refused("Parser Error: syntax error"))), Engine::Ready { misses: 2 });
        let missing = refused("Catalog Error: Table Function with name sql_auto_complete does not exist!");
        assert_eq!(answered(Engine::Ready { misses: 0 }, Err(&missing)), Engine::Unavailable);
    }

    fn item(text: &str, start: usize, kind: Kind) -> Item {
        Item { text: text.into(), start, kind }
    }

    fn texts(items: &[Item]) -> Vec<&str> {
        items.iter().map(|i| i.text.as_str()).collect()
    }

    fn catalog() -> Catalog {
        serde_json::from_value(serde_json::json!({
            "tables": [
                {"name": "people", "schema": "main", "columns": [
                    {"name": "id", "type": "INTEGER"},
                    {"name": "name", "type": "VARCHAR"},
                    {"name": "email", "type": "VARCHAR"}]},
                {"name": "Order Items", "schema": "main", "columns": [
                    {"name": "item_id", "type": "INTEGER"},
                    {"name": "qty", "type": "INTEGER"}]},
                {"name": "invoices", "schema": "sales", "columns": [
                    {"name": "invoice_no", "type": "INTEGER"},
                    {"name": "people_id", "type": "INTEGER"}]}
            ]
        }))
        .expect("a catalog")
    }

    fn from_cat(marked: &str) -> Vec<String> {
        let (s, text) = site_of(marked).unwrap();
        from_catalog(&catalog(), &s, &text).into_iter().map(|i| i.text).collect()
    }

    #[test]
    fn the_catalog_offers_tables_schemas_and_columns_that_continue_the_name() {
        assert_eq!(from_cat("SELECT * FROM pe|"), ["people", "people_id"]);
        assert_eq!(from_cat("SELECT * FROM PE|"), ["people", "people_id"]);
        assert_eq!(from_cat("SELECT * FROM \"Ord|"), ["\"Order Items\""]);
        assert_eq!(from_cat("SELECT * FROM sa|"), ["sales"]);
        assert_eq!(from_cat("SELECT i|"), ["invoices", "id", "item_id", "invoice_no"]);
        // `main` is everywhere already.
        assert!(from_cat("SELECT ma|").is_empty());
        // Every name is there for a summons with nothing typed.
        assert_eq!(from_cat("SELECT |").len(), 11);
    }

    #[test]
    fn a_qualifier_offers_its_tables_columns_its_aliases_and_its_schemas_tables() {
        assert_eq!(from_cat("SELECT people.|"), ["id", "name", "email"]);
        assert_eq!(from_cat("SELECT PEOPLE.e|"), ["email"]);
        assert_eq!(from_cat("SELECT \"Order Items\".|"), ["item_id", "qty"]);
        assert_eq!(from_cat("SELECT p.| FROM people p"), ["id", "name", "email"]);
        assert_eq!(from_cat("SELECT o.q| FROM people AS p JOIN \"Order Items\" AS o ON true"), ["qty"]);
        assert_eq!(from_cat("SELECT i.| FROM people p, sales.invoices i"), ["invoice_no", "people_id"]);
        assert_eq!(from_cat("FROM sales.|"), ["invoices"]);
        assert!(from_cat("SELECT nobody.|").is_empty());
    }

    #[test]
    fn an_alias_is_the_name_after_its_table_in_from_or_join() {
        let t = |stmt: &str, alias: &str| alias_target(stmt, alias);
        assert_eq!(t("SELECT p.x FROM people p", "p"), Some("people".into()));
        assert_eq!(t("select P.x from people as p", "p"), Some("people".into()));
        assert_eq!(t("FROM a x JOIN b AS y ON x.id = y.id", "y"), Some("b".into()));
        assert_eq!(t("FROM a x, s.b y", "y"), Some("b".into()));
        assert_eq!(t("FROM \"Order Items\" oi", "oi"), Some("Order Items".into()));
        // Not an alias: a column's, a name in a string or comment.
        assert_eq!(t("SELECT a AS p FROM t", "p"), None);
        assert_eq!(t("SELECT 'FROM people p' -- FROM people p", "p"), None);
        assert_eq!(t("SELECT 1", "p"), None);
    }

    #[test]
    fn names_that_are_not_plain_identifiers_are_quoted() {
        assert_eq!(quoted("people"), "people");
        assert_eq!(quoted("People_2"), "People_2");
        assert_eq!(quoted("Order Items"), "\"Order Items\"");
        assert_eq!(quoted("2fa"), "\"2fa\"");
        assert_eq!(quoted("a\"b"), "\"a\"\"b\"");
        assert_eq!(quoted("naïve"), "\"naïve\"");
    }

    #[test]
    fn the_engine_names_what_continues_the_typed_text_and_the_catalog_fills_in() {
        let (s, text) = site_of("SELECT * FROM pe|").unwrap();
        let engine = from_engine(
            &[
                Suggestion { text: "depends".into(), start: 14, kind: "keyword".into() },
                Suggestion { text: "people".into(), start: 14, kind: "table".into() },
                Suggestion { text: "PERCENT".into(), start: 14, kind: "keyword".into() },
                Suggestion { text: "people".into(), start: 14, kind: "table".into() },
            ],
            0,
        );
        assert_eq!(engine[1], item("people", 14, Kind::Table));
        let catalog = vec![item("people", 14, Kind::Table), item("people_id", 14, Kind::Column)];
        let merged = merge(Some(engine), catalog.clone(), &s, &text);
        assert_eq!(texts(&merged), ["people", "PERCENT", "people_id"]);
        // What merely contains the typed text shows only when nothing
        // continues it.
        let (s, text) = site_of("SELECT * FROM pepl|").unwrap();
        let guesses = vec![item("people", 14, Kind::Table), item("pg_type", 14, Kind::Table)];
        assert_eq!(texts(&merge(Some(guesses.clone()), Vec::new(), &s, &text)), ["people", "pg_type"]);
        let catalog = vec![item("pepl_log", 14, Kind::Table)];
        assert_eq!(texts(&merge(Some(guesses), catalog, &s, &text)), ["pepl_log"]);
        let (s, text) = site_of("SELECT * FROM pe|").unwrap();
        let catalog = vec![item("people", 14, Kind::Table), item("people_id", 14, Kind::Column)];
        // Without the engine, the catalog alone.
        assert_eq!(texts(&merge(None, catalog.clone(), &s, &text)), ["people", "people_id"]);
        // An engine that answered nothing leaves the catalog's.
        assert_eq!(texts(&merge(Some(Vec::new()), catalog, &s, &text)), ["people", "people_id"]);
        // A suggestion placed outside the statement, or past the caret, is
        // no replacement for what is typed.
        let stray = vec![item("x", 3, Kind::Other), item("y", 99, Kind::Other)];
        let (s, text) = site_of("SELECT 1; SELECT y|").unwrap();
        assert!(merge(Some(stray), Vec::new(), &s, &text).is_empty());
        // A quoted and a bare spelling of one name are one entry.
        let (s, text) = site_of("SELECT na|").unwrap();
        let engine = vec![item("\"name\"", 7, Kind::Column), item("name", 7, Kind::Keyword)];
        assert_eq!(texts(&merge(Some(engine), Vec::new(), &s, &text)), ["\"name\""]);
    }

    #[test]
    fn a_qualified_name_known_to_the_catalog_takes_the_catalogs_members_alone() {
        let (s, text) = site_of("SELECT people.na|").unwrap();
        let engine = vec![item("\"name\"", 14, Kind::Column), item("nanosecond", 14, Kind::Function)];
        let catalog = vec![item("name", 14, Kind::Column)];
        assert_eq!(texts(&merge(Some(engine.clone()), catalog, &s, &text)), ["name"]);
        // Unknown to the catalog, the qualifier is the engine's to read.
        assert_eq!(texts(&merge(Some(engine), Vec::new(), &s, &text)), ["\"name\"", "nanosecond"]);
    }

    #[test]
    fn the_list_shown_narrows_while_its_name_grows_in_its_statement() {
        let (s, text) = site_of("SELECT * FROM p|").unwrap();
        let items = vec![item("people", 14, Kind::Table), item("PERCENT", 14, Kind::Keyword), item("pg_type", 14, Kind::Table)];
        let shown = Shown::of(&s, &text, items);
        let n = |marked: &str| {
            let (s, text) = site_of(marked).unwrap();
            narrow(&shown, &s, &text).map(|items| texts(&items).into_iter().map(String::from).collect::<Vec<_>>())
        };
        assert_eq!(n("SELECT * FROM pe|"), Some(vec!["people".into(), "PERCENT".into()]));
        assert_eq!(n("SELECT * FROM PEO|"), Some(vec!["people".into()]));
        assert_eq!(n("SELECT * FROM pz|"), Some(vec![]));
        // Not the same name growing: shorter, elsewhere, or another statement head.
        assert_eq!(n("SELECT * FROM p|"), None);
        assert_eq!(n("SELECT * FROM |"), None);
        assert_eq!(n("SELECT * FROM xe|"), None);
        assert_eq!(n("SELECT * FROM  pe|"), None);
        assert_eq!(n("SELECT 1 FROM pe|"), None);
    }

    #[test]
    fn the_highlight_covers_the_typed_letters_of_the_label() {
        assert_eq!(matched("SELECT", "sel"), 3);
        assert_eq!(matched("\"Order Items\"", "\"ord"), 4);
        assert_eq!(matched("depends", "pe"), 0);
        assert_eq!(matched("Éclair", "é"), 2);
        assert_eq!(matched("people", ""), 0);
        assert_eq!(matched("pe", "people"), 2);
    }

    #[test]
    fn an_answer_lands_only_for_the_newest_question_on_the_text_and_caret_it_asked() {
        let asked = Asked::of(4, "SELECT pe", 9);
        assert_eq!(asked, Asked::of(4, "SELECT pe", 9));
        assert_ne!(asked, Asked::of(5, "SELECT pe", 9));
        assert_ne!(asked, Asked::of(4, "SELECT peo", 9));
        assert_ne!(asked, Asked::of(4, "SELECT pe", 8));
    }

    #[test]
    fn tab_accepts_enter_is_a_newline_and_control_space_summons() {
        use gpui_kit::Modifiers;
        let none = Modifiers::default();
        let cmd = Modifiers { platform: true, ..Default::default() };
        let ctrl = Modifiers { control: true, ..Default::default() };
        let shift = Modifiers { shift: true, ..Default::default() };
        assert_eq!(key("tab", &none, true), Key::Accept);
        assert_eq!(key("tab", &none, false), Key::Pass);
        assert_eq!(key("tab", &shift, true), Key::Pass);
        assert_eq!(key("enter", &none, true), Key::Dismiss);
        assert_eq!(key("enter", &shift, true), Key::Dismiss);
        assert_eq!(key("enter", &cmd, true), Key::Close);
        assert_eq!(key("enter", &none, false), Key::Pass);
        assert_eq!(key("space", &ctrl, false), Key::Summon);
        assert_eq!(key("space", &ctrl, true), Key::Summon);
        assert_eq!(key("space", &none, false), Key::Pass);
        assert_eq!(key("escape", &none, true), Key::Pass);
        assert_eq!(key("down", &none, true), Key::Pass);
    }

    /// The decisions end to end against a live Harbor and its catalog, as
    /// the editor takes them. Ignored by default; it creates and drops a
    /// table, so it runs only against the scratch database `HARBOR_LIVE_DB`
    /// names, under a scratch `HARBOR_HOME`, as harbor-client's live probes
    /// do: `HARBOR_HOME=/tmp/dt-home HARBOR_LIVE_DB=/tmp/scratch.duckdb cargo
    /// test -p ducktable complete -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn a_scratch_database_completes_keywords_tables_and_a_tables_columns() {
        use super::{LOAD_PATIENCE, PATIENCE};
        let Some(db) = std::env::var_os("HARBOR_LIVE_DB") else {
            println!("set HARBOR_LIVE_DB to a scratch database file; skipping");
            return;
        };
        let conn = harbor_client::fleet::connect_path(std::path::Path::new(&db)).expect("connect");
        harbor_client::query(&conn, "CREATE OR REPLACE TABLE _dt_complete_view (id INTEGER, email VARCHAR)")
            .expect("create");
        assert_eq!(loaded(harbor_client::complete::load(&conn, LOAD_PATIENCE).as_ref().map(|_| ())), Engine::Ready { misses: 0 });
        let catalog = harbor_client::catalog(&conn).expect("catalog");
        let list = |marked: &str| -> Vec<String> {
            let (text, caret) = at(marked);
            let site = site(&text, caret).expect("a site");
            let names = from_catalog(&catalog, &site, &text);
            let members = site.qualifier.is_some() && !names.is_empty();
            let engine = match plan(Engine::Ready { misses: 0 }, site.asked(&text).len(), members) {
                Plan::Ask => {
                    let answer = harbor_client::complete::suggest(&conn, site.asked(&text), PATIENCE).expect("an answer");
                    Some(from_engine(&answer, site.stmt.start))
                }
                _ => None,
            };
            let shown: Vec<String> = merge(engine, names, &site, &text).into_iter().map(|i| i.text).collect();
            println!("{marked:?}: {:?}", shown.iter().take(6).collect::<Vec<_>>());
            shown
        };
        assert_eq!(list("SEL|")[0], "SELECT");
        assert_eq!(list("SELECT 1; select * fr|")[0], "from");
        assert_eq!(list("SELECT * FROM _dt_complete_v|")[0], "_dt_complete_view");
        assert_eq!(list("SELECT _dt_complete_view.|"), ["id", "email"]);
        assert_eq!(list("SELECT _dt_complete_view.e|"), ["email"]);
        assert_eq!(list("SELECT v.| FROM _dt_complete_view v"), ["id", "email"]);
        assert_eq!(list("SELECT * FROM _dt_complete_view WHERE em|")[0], "email");
        harbor_client::query(&conn, "DROP TABLE _dt_complete_view").expect("drop");
    }
}
