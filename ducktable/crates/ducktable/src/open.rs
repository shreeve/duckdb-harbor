//! Opening anything (docs/OPEN.md): every way a path reaches DuckTable —
//! Finder and the Dock (`on_open_urls`), a drop on the window, the command
//! line and File → Open… — lands in [`open_paths`], which reads what each
//! path holds (`wire::open`) and opens it: a DuckDB database as itself,
//! anything else DuckDB reads in the workspace. The decisions are the pure
//! functions here; the switch itself is the app's, through the one dialog
//! like any other (`leave_asking`).

use crate::app::{DbKey, Openings};
use crate::sql::Rel;
use gpui_kit::*;
use harbor_client::Catalog;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use wire::open::Kind;

/// The paths a command line names: every argument after the program's
/// own, made absolute against `cwd`, but the ones macOS adds. Finder once
/// passed a process serial number (`-psn_0_12345`), and AppKit reads
/// `-Key value` pairs as defaults (`-NSDocumentRevisionsDebugMode YES`), so
/// an argument that begins with `-` is passed over with the value after it.
pub(crate) fn paths_from_args(args: impl IntoIterator<Item = OsString>, cwd: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut args = args.into_iter().skip(1);
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy();
        if text.starts_with("-psn_") {
            continue;
        }
        if text.starts_with('-') {
            args.next();
            continue;
        }
        out.push(cwd.join(PathBuf::from(arg)));
    }
    out
}

/// The paths of the `file://` URLs macOS hands an app to open (Finder's
/// double-click and Open With, a drop on the Dock icon, `open -a`), each
/// percent-decoded. Any other URL is not a file, and is passed over.
pub(crate) fn paths_from_urls(urls: &[String]) -> Vec<PathBuf> {
    urls.iter()
        .filter_map(|url| url.strip_prefix("file://"))
        // `file://localhost/x` names the same file as `file:///x`.
        .map(|rest| rest.strip_prefix("localhost").unwrap_or(rest))
        .filter(|path| path.starts_with('/'))
        .filter_map(percent_decode)
        .map(|path| match path.len() > 1 {
            // A folder's URL ends in `/`.
            true => PathBuf::from(path.trim_end_matches('/')),
            false => PathBuf::from(path),
        })
        .collect()
}

/// `%XX` escapes decoded, the bytes read as UTF-8 as a file URL encodes
/// them. None for a malformed escape or bytes that are not UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// What a set of paths opens, from what each holds.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Plan {
    /// A DuckDB database, opened as itself.
    pub(crate) database: Option<PathBuf>,
    /// Files that open in the workspace, in the order they came.
    pub(crate) workspace: Vec<(PathBuf, Kind)>,
    /// Why a path does not open, one sentence each.
    pub(crate) refused: Vec<String>,
}

/// What `classified` opens. Everything that opens in the workspace opens
/// there, together. One window shows one database, so a DuckDB database
/// opens only when nothing else in the set does, and only the first:
/// any other is said not to have opened, with why.
pub(crate) fn plan(classified: Vec<(PathBuf, Result<Kind, String>)>) -> Plan {
    let mut out = Plan::default();
    let mut databases = Vec::new();
    for (path, kind) in classified {
        match kind {
            Ok(Kind::Duckdb) => databases.push(path),
            Ok(kind) => out.workspace.push((path, kind)),
            Err(why) => out.refused.push(why),
        }
    }
    let mut databases = databases.into_iter();
    if out.workspace.is_empty() {
        out.database = databases.next();
    }
    for db in databases {
        out.refused.push(format!(
            "{} is a DuckDB database, and opens by itself: one window shows one database at a time.",
            harbor_client::paths::shorten(&db)
        ));
    }
    out
}

/// The relation to show once files have opened in the workspace: the
/// first file's view, or the first table of the first SQLite file's
/// catalog, in the sidebar's order, else its first view. None when nothing
/// opened, or nothing it opened is in `catalog`.
pub(crate) fn first_opened(catalog: &Catalog, opened: &Openings) -> Option<Rel> {
    let first = opened.iter().find_map(|o| o.as_ref().ok())?;
    if !first.attached {
        return catalog.views_in("main").iter().any(|v| v.name == first.name).then(|| Rel::own("main", &first.name));
    }
    let attached = catalog.attached.iter().find(|a| a.name == first.name)?;
    let schemas = attached.schemas();
    let table = schemas.iter().find_map(|s| attached.tables_in(s).first().map(|t| Rel::of(&attached.name, s, &t.name)));
    table.or_else(|| schemas.iter().find_map(|s| attached.views_in(s).first().map(|v| Rel::of(&attached.name, s, &v.name))))
}

/// What the status line says of what opened, and the refusals the alert
/// says, one paragraph each.
pub(crate) fn report(opened: &Openings) -> (Option<String>, Vec<String>) {
    let lines: Vec<&str> = opened.iter().filter_map(|o| o.as_ref().ok()).map(|o| o.line.as_str()).collect();
    let said = match lines.as_slice() {
        [] => None,
        [one] => Some(one.to_string()),
        [first, rest @ ..] => Some(format!("{first}, and {} more", rest.len())),
    };
    let refused = opened.iter().filter_map(|o| o.as_ref().err().cloned()).collect();
    (said, refused)
}

/// How big a file's import may be before Import as Table asks first: the
/// copy reads the whole file into the workspace.
pub(crate) const ASK_ABOVE: u64 = 100_000_000;

/// The question Import as Table asks before it copies a large file, as
/// the message and its detail, or None when it goes ahead unasked.
pub(crate) fn import_question(name: &str, source: &Path, bytes: Option<u64>) -> Option<(String, String)> {
    let bytes = bytes.filter(|b| *b > ASK_ABOVE)?;
    Some((
        format!("Import {name} as a table?"),
        format!(
            "{} is {}. Importing reads all of it and copies it into the workspace, which grows by about as much; \
             the file is left as it is.",
            harbor_client::paths::shorten(source),
            crate::util::human(bytes as f64, "B"),
        ),
    ))
}

/// Open `paths`, however they came: read what each holds off the UI
/// thread, then open what can be opened and say why the rest cannot.
pub(crate) fn open_paths(paths: Vec<PathBuf>, cx: &mut App) {
    if paths.is_empty() {
        return;
    }
    cx.spawn(async move |cx| {
        let (classified, workspace) = cx
            .background_executor()
            .spawn(async move {
                let classified: Vec<_> = paths
                    .into_iter()
                    .map(|path| {
                        let kind = harbor_client::workspace::kind_of(&path);
                        (path, kind)
                    })
                    .collect();
                // The workspace's key, by its canonical path, to tell
                // whether it is the database on screen.
                let workspace = harbor_client::paths::workspace().ok().map(|ws| DbKey::of_file(&ws));
                (classified, workspace)
            })
            .await;
        cx.update(|cx| {
            let plan = plan(classified);
            if !plan.refused.is_empty() {
                alert("DuckTable could not open everything", plan.refused.join("\n\n"), cx);
            }
            if let Some(view) = crate::app_view(cx) {
                view.update(cx, |app, cx| app.open_planned(plan, workspace, cx));
            }
        });
    })
    .detach();
}

/// A warning in the platform's alert, with one button, from anywhere: it
/// waits for the window's own update to finish.
pub(crate) fn alert(title: impl Into<String>, detail: String, cx: &mut App) {
    let title = title.into();
    cx.defer(move |cx| {
        let window = cx.active_window().or_else(|| cx.windows().first().copied());
        if let Some(w) = window {
            w.update(cx, |_, window, cx| {
                let answer = window.prompt(PromptLevel::Warning, &title, Some(&detail), &["OK"], cx);
                cx.spawn(async move |_| answer.await.ok()).detach();
            })
            .ok();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{first_opened, import_question, paths_from_args, paths_from_urls, plan, report, Plan, ASK_ABOVE};
    use crate::sql::Rel;
    use harbor_client::workspace::Opened;
    use harbor_client::Catalog;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use wire::open::Kind;
    use wire::catalog::{Attached, Table, View};
    use wire::open::Reader;

    #[test]
    fn a_command_line_names_paths_but_not_the_process_serial_number() {
        let args = [
            "/Applications/DuckTable.app/Contents/MacOS/ducktable",
            "-psn_0_1234",
            "a.csv",
            "-NSDocumentRevisionsDebugMode",
            "YES",
            "data/",
            "/abs/b.parquet",
        ]
        .map(OsString::from);
        assert_eq!(
            paths_from_args(args, Path::new("/home/me")),
            [PathBuf::from("/home/me/a.csv"), PathBuf::from("/home/me/data/"), PathBuf::from("/abs/b.parquet")]
        );
        assert!(paths_from_args([OsString::from("ducktable")], Path::new("/")).is_empty());
    }

    #[test]
    fn file_urls_are_paths_decoded_and_other_urls_are_not() {
        let urls = [
            "file:///Users/me/Sales%20Q1%20(final).csv",
            "file:///Users/me/lake/",
            "file://localhost/tmp/it's.json",
            "file:///tmp/donn%C3%A9es.xlsx",
            "https://example.com/x.csv",
            "file:///tmp/bad%zz.csv",
        ]
        .map(String::from);
        assert_eq!(
            paths_from_urls(&urls),
            [
                PathBuf::from("/Users/me/Sales Q1 (final).csv"),
                PathBuf::from("/Users/me/lake"),
                PathBuf::from("/tmp/it's.json"),
                PathBuf::from("/tmp/données.xlsx"),
            ]
        );
    }

    #[test]
    fn data_files_open_together_and_a_database_opens_alone() {
        let csv = (PathBuf::from("/x/a.csv"), Ok(Kind::Data(Reader::Csv)));
        let app = (PathBuf::from("/x/app.sqlite"), Ok(Kind::Sqlite));
        let db = (PathBuf::from("/x/one.duckdb"), Ok(Kind::Duckdb));
        let other = (PathBuf::from("/x/two.duckdb"), Ok(Kind::Duckdb));
        let bad = (PathBuf::from("/x/n.bin"), Err("/x/n.bin: not a kind harbor opens".to_string()));

        // A database alone opens as itself.
        assert_eq!(plan(vec![db.clone()]), Plan { database: Some("/x/one.duckdb".into()), ..Default::default() });
        // Data files and SQLite files open in the workspace, all of them.
        let p = plan(vec![csv.clone(), bad.clone(), app.clone()]);
        assert_eq!(p.database, None);
        assert_eq!(
            p.workspace,
            [(PathBuf::from("/x/a.csv"), Kind::Data(Reader::Csv)), (PathBuf::from("/x/app.sqlite"), Kind::Sqlite)]
        );
        assert_eq!(p.refused, ["/x/n.bin: not a kind harbor opens"]);
        // Two databases: the first opens, and the second says why not.
        let p = plan(vec![db.clone(), other]);
        assert_eq!(p.database, Some("/x/one.duckdb".into()));
        assert!(p.refused[0].starts_with("/x/two.duckdb is a DuckDB database, and opens by itself"));
        // A database among data files does not take the window from them.
        let p = plan(vec![db, csv]);
        assert_eq!((p.database, p.workspace.len(), p.refused.len()), (None, 1, 1));
        assert!(plan(vec![bad]).database.is_none());
    }

    fn opened(name: &str, attached: bool) -> Result<Opened, String> {
        Ok(Opened { name: name.into(), attached, made: true, line: format!("{name}: opened") })
    }

    #[test]
    fn what_opened_first_is_shown() {
        let table = |name: &str, schema: &str| Table { name: name.into(), schema: schema.into(), ..Default::default() };
        let view = |name: &str| View { name: name.into(), schema: "main".into(), ..Default::default() };
        let catalog = Catalog {
            views: vec![view("sales"), view("lake")],
            attached: vec![
                Attached { name: "app".into(), tables: vec![table("people", "main"), table("audit", "zz"), table("notes", "main")], ..Default::default() },
                Attached { name: "empty".into(), views: vec![view("v")], ..Default::default() },
            ],
            ..Default::default()
        };
        let failed: Result<Opened, String> = Err("x.csv: no such file".into());
        assert_eq!(first_opened(&catalog, &vec![failed.clone(), opened("lake", false)]), Some(Rel::own("main", "lake")));
        // A SQLite file shows its first table, `main` first, in name order.
        assert_eq!(first_opened(&catalog, &vec![opened("app", true)]), Some(Rel::of("app", "main", "notes")));
        // One with no tables shows its first view.
        assert_eq!(first_opened(&catalog, &vec![opened("empty", true)]), Some(Rel::of("empty", "main", "v")));
        assert_eq!(first_opened(&catalog, &vec![opened("gone", false)]), None);
        assert_eq!(first_opened(&catalog, &vec![failed]), None);
    }

    #[test]
    fn what_opened_is_said_in_a_line_and_what_did_not_in_the_alert() {
        let failed: Result<Opened, String> = Err("x.csv: no such file".into());
        assert_eq!(report(&vec![]), (None, vec![]));
        assert_eq!(report(&vec![opened("a", false)]), (Some("a: opened".into()), vec![]));
        assert_eq!(
            report(&vec![opened("a", false), failed, opened("b", true), opened("c", false)]),
            (Some("a: opened, and 2 more".into()), vec!["x.csv: no such file".to_string()])
        );
    }

    /// The bundle declares a document type for every kind of file DuckTable
    /// opens (`scripts/macos-app.sh`), so Finder hands it each one: owning
    /// DuckDB's, and an alternate to the app that has the common kinds.
    #[test]
    fn the_bundle_declares_every_kind_it_opens() {
        let script = include_str!("../../../scripts/macos-app.sh");
        let declared = |rank: &str, uti: &str| {
            script.lines().any(|l| l.trim_start().starts_with("doc_type \"") && l.contains(&format!(" {rank} ")) && l.split_whitespace().any(|w| w == uti))
        };
        let imported = |uti: &str, ext: &str| {
            script.lines().any(|l| l.trim_start().starts_with(&format!("imported {uti} ")) && l.split_whitespace().any(|w| w == ext))
        };
        const SQLITE: &[u8] = b"SQLite format 3\0";
        // Each extension, the type Finder knows it by (the system's, or one
        // the bundle imports), the rank, and a file of the kind.
        let kinds: [(&str, &str, bool, &str, &[u8]); 16] = [
            ("duckdb", "org.duckdb.database", true, "Owner", b""),
            ("ddb", "org.duckdb.database", true, "Owner", b""),
            ("sqlite", "org.sqlite.database", true, "Alternate", SQLITE),
            ("sqlite3", "org.sqlite.database", true, "Alternate", SQLITE),
            ("db", "org.sqlite.database", true, "Alternate", SQLITE),
            ("csv", "public.comma-separated-values-text", false, "Alternate", b"a,b"),
            ("tsv", "public.tab-separated-values-text", false, "Alternate", b"a\tb"),
            ("json", "public.json", false, "Alternate", b"{}"),
            ("ndjson", "public.ndjson", false, "Alternate", b"{}"),
            ("jsonl", "org.jsonlines.jsonl", true, "Alternate", b"{}"),
            ("xlsx", "org.openxmlformats.spreadsheetml.sheet", false, "Alternate", b"PK\x03\x04"),
            ("geojson", "public.geojson", false, "Alternate", b"{}"),
            ("parquet", "org.apache.parquet", true, "Default", b"PAR1"),
            ("shp", "com.esri.shapefile", true, "Default", b"\x00\x00\x27\x0a"),
            ("gpkg", "org.opengeospatial.geopackage", true, "Default", SQLITE),
            ("folder", "public.folder", false, "Alternate", b""),
        ];
        for (ext, uti, import, rank, head) in kinds {
            assert!(declared(rank, uti), "{ext}: no {rank} document type for {uti}");
            assert!(!import || imported(uti, ext), "{ext}: {uti} is not imported with it");
            let kind = match ext {
                "folder" => wire::open::classify("/x/lake", wire::open::Entry::Dir),
                _ => wire::open::classify(&format!("/x/f.{ext}"), wire::open::Entry::File(head)),
            };
            assert!(kind.is_ok(), "{ext}: {kind:?}");
        }
    }

    #[test]
    fn a_large_import_asks_first_saying_the_size() {
        let source = Path::new("/data/big.csv");
        assert_eq!(import_question("big", source, Some(ASK_ABOVE)), None);
        assert_eq!(import_question("big", source, None), None);
        let (message, detail) = import_question("big", source, Some(2_400_000_000)).unwrap();
        assert_eq!(message, "Import big as a table?");
        assert!(detail.starts_with("/data/big.csv is 2.4GB."), "{detail}");
    }
}
