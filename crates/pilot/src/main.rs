//! pilot — the Harbor client.
//!
//! Phase-2 seed (PLAN.md): one-shot SQL and the fleet view. Zero-config local
//! (D10a): a bare name resolves to ~/.harbor/<name>.sock and its token file;
//! no config needed. The REPL, config.toml address book, https, and
//! spawn-on-demand grow here next.
//!
//!   pilot                          list live berths
//!   pilot <target> -c "SQL"        run one statement
//!   echo "SQL" | pilot <target>    same, from stdin
//!
//! <target> = berth name | socket path | http://host:port

mod http;
mod repl;

use harbor_protocol::{Event, SqlRequest, endpoint};
use http::Transport;
use std::io::{BufRead, IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

struct Conn {
    transport: Transport,
    token: Option<String>,
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut target: Option<String> = None;
    let mut sql: Option<String> = None;
    let mut token: Option<String> = None;
    let mut json = false;

    while let Some(a) = args.next() {
        match a.as_str() {
            "-c" | "--command" => sql = args.next(),
            "--token" => token = args.next(),
            "--json" => json = true,
            "-h" | "--help" => {
                print!("{HELP}");
                return ExitCode::SUCCESS;
            }
            _ if target.is_none() => target = Some(a),
            _ => return fail(&format!("unexpected argument: {a}")),
        }
    }

    let Some(target) = target else { return list_fleet() };

    let conn = match resolve(&target, token) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };

    let sql = match sql {
        Some(s) => s,
        None => {
            // No -c and a real terminal: the REPL. On a pipe: read stdin.
            if std::io::stdin().is_terminal() {
                return repl::run(&conn, &target);
            }
            let mut s = String::new();
            if std::io::stdin().read_to_string(&mut s).is_err() || s.trim().is_empty() {
                return fail("no SQL given: use -c \"...\" or pipe on stdin");
            }
            s
        }
    };

    run_sql(&conn, sql.trim(), json)
}

const HELP: &str = "\
pilot — the Harbor client

usage:
  pilot                        list live berths in ~/.harbor
  pilot <target> -c \"SQL\"      run one statement
  echo \"SQL\" | pilot <target>  same, from stdin

target:
  name                         a live berth: ~/.harbor/<name>.sock (+ <name>.token)
  path/to.sock                 a harbor unix socket
  http://host:port             a harbor TCP listener

options:
  --token <t>                  bearer token (else HARBOR_TOKEN, else <name>.token)
  --json                       one JSON object per row instead of a table
";

/// Zero-config resolution (D9/D10a order, sans config file for now):
/// live berth name → registry socket; *.sock path → socket; http:// → TCP.
fn resolve(target: &str, flag_token: Option<String>) -> Result<Conn, String> {
    let env_token = std::env::var("HARBOR_TOKEN").ok();

    if let Some(rest) = target.strip_prefix("http://") {
        let (addr, extra) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
        if !extra.is_empty() {
            return Err("path-prefixed URLs arrive with the config address book; use host:port".into());
        }
        let addr = if addr.contains(':') { addr.to_string() } else { format!("{addr}:9495") };
        return Ok(Conn { transport: Transport::Tcp(addr), token: flag_token.or(env_token) });
    }
    if target.starts_with("https://") {
        return Err("https targets land in Phase 2 (ureq); front a socket with Caddy and use http:// or a name".into());
    }
    if target.ends_with(".duckdb") {
        return Err("spawn-on-demand (PLAN.md D9) needs `harbor serve` — coming with the harbor binary".into());
    }

    let sock: PathBuf;
    let mut file_token = None;
    if target.contains('/') || target.ends_with(".sock") {
        sock = PathBuf::from(target);
    } else {
        let home = http::harbor_home();
        sock = home.join(format!("{target}.sock"));
        if !sock.exists() {
            return Err(format!(
                "no live berth named {target:?} ({} not found){}",
                sock.display(),
                fleet_hint(&home)
            ));
        }
        file_token = std::fs::read_to_string(home.join(format!("{target}.token")))
            .ok()
            .map(|t| t.trim().to_string());
    }
    Ok(Conn { transport: Transport::Unix(sock), token: flag_token.or(env_token).or(file_token) })
}

fn fleet_hint(home: &std::path::Path) -> String {
    let names: Vec<String> = berth_sockets(home)
        .iter()
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .collect();
    if names.is_empty() { String::new() } else { format!("; live berths: {}", names.join(", ")) }
}

fn berth_sockets(home: &std::path::Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(home)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "sock"))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Bare `pilot`: the fleet view. /ready is unauthenticated by design, so this
/// needs no tokens; config-file remotes join this view in Phase 2.
fn list_fleet() -> ExitCode {
    let home = http::harbor_home();
    let socks = berth_sockets(&home);
    if socks.is_empty() {
        println!("no live berths in {} (start one: harbor add <db>)", home.display());
        return ExitCode::SUCCESS;
    }
    println!("{:<20} {:<8} SOCKET", "BERTH", "STATE");
    for sock in socks {
        let name = sock.file_stem().unwrap_or_default().to_string_lossy();
        let state = match http::request(
            &Transport::Unix(sock.clone()),
            "GET",
            endpoint::READY,
            None,
            None,
            Some(Duration::from_secs(2)),
        ) {
            Ok(r) if r.status == 200 => "ready",
            Ok(_) => "unready",
            Err(_) => "dead",
        };
        println!("{name:<20} {state:<8} {}", sock.display());
    }
    ExitCode::SUCCESS
}

fn run_sql(conn: &Conn, sql: &str, json: bool) -> ExitCode {
    let body = serde_json::to_string(&SqlRequest { sql: sql.to_string(), ..Default::default() })
        .expect("request serializes");
    let resp = match http::request(&conn.transport, "POST", endpoint::SQL, conn.token.as_deref(), Some(&body), None) {
        Ok(r) => r,
        Err(e) => return fail(&format!("cannot reach harbor: {e}")),
    };

    // Non-2xx: the body is one Event::Error document.
    if resp.status >= 300 {
        let status = resp.status;
        let text = resp.body_string().unwrap_or_default();
        return match Event::parse(text.trim()) {
            Ok(Event::Error { code, message }) => fail(&format!("harbor error ({code}): {message}")),
            _ => fail(&format!("HTTP {status} from harbor: {text}")),
        };
    }

    // Stream the envelope. json mode emits rows as they arrive (O(1) memory);
    // table mode buffers — the duckbox head/tail policy replaces that in Phase 2.
    let mut columns: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<serde_json::Value>> = Vec::new();
    let mut body = resp.body;
    let mut line = String::new();
    loop {
        line.clear();
        match body.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => return fail(&format!("stream died: {e}")),
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let event = match Event::parse(trimmed) {
            Ok(ev) => ev,
            Err(e) => return fail(&format!("bad envelope line ({e}): {trimmed}")),
        };
        match event {
            Event::Schema { columns: cols } => {
                columns = cols
                    .iter()
                    .enumerate()
                    .map(|(i, c)| c.name.clone().unwrap_or_else(|| format!("col{i}")))
                    .collect();
            }
            Event::Row { values } => {
                if json {
                    let obj: serde_json::Map<String, serde_json::Value> =
                        columns.iter().cloned().zip(values).collect();
                    println!("{}", serde_json::Value::Object(obj));
                } else {
                    rows.push(values);
                }
            }
            Event::End { row_count, time_ms } => {
                if !json {
                    print_table(&columns, &rows);
                    eprintln!("{row_count} rows ({time_ms} ms)");
                }
                return ExitCode::SUCCESS;
            }
            Event::Error { code, message } => {
                return fail(&format!("harbor error ({code}): {message}"));
            }
        }
    }
    fail("stream ended without an end event")
}

/// Plain aligned table — the honest placeholder until duckbox (Phase 2).
fn print_table(columns: &[String], rows: &[Vec<serde_json::Value>]) {
    let render = |v: &serde_json::Value| -> String {
        match v {
            serde_json::Value::Null => "NULL".to_string(),
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    };
    let cells: Vec<Vec<String>> = rows.iter().map(|r| r.iter().map(render).collect()).collect();
    let mut widths: Vec<usize> = columns.iter().map(|c| c.chars().count()).collect();
    for row in &cells {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
    }
    let line = |cells: &[String]| {
        let padded: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{:<w$}", c, w = widths.get(i).copied().unwrap_or(0)))
            .collect();
        println!("{}", padded.join("  "));
    };
    line(&columns.to_vec());
    line(&widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>());
    for row in cells {
        line(&row);
    }
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("pilot: {msg}");
    ExitCode::FAILURE
}
