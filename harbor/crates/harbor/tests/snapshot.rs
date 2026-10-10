//! A multi-pass backup must retain its snapshot when another caller commits
//! between export and re-export. Exercise the real HTTP session helper.
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    dir: PathBuf,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn export_passes_share_a_snapshot_and_failures_rollback() {
    // Skipped, and saying so, only where no engine is promised.
    if let Err(e) = harbor::engine::engine() {
        if ["HARBOR_LIBDUCKDB", "CI"].iter().any(|v| std::env::var_os(v).is_some()) {
            panic!("no engine: {e}");
        }
        eprintln!("snapshot: skipped — {e}");
        return;
    }
    let base = if cfg!(unix) {
        PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    let dir = base.join(format!("hb-snapshot-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    // Port 0: the server takes whatever port is free and says which once it
    // serves, so no other process can take it in between.
    let child = Command::new(env!("CARGO_BIN_EXE_harbor"))
        .args([
            dir.join("source.duckdb").to_str().unwrap(),
            "start",
            "--port",
            "0",
            "--workers",
            "1",
            "--statement-timeout",
            "1s",
        ])
        .env("HARBOR_HOME", dir.join("home"))
        .env("HARBOR_POOL_SIZE", "2")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut server = Server { child, dir };
    let mut said = BufReader::new(server.child.stderr.take().unwrap());
    let mut line = String::new();
    let address = loop {
        line.clear();
        assert!(said.read_line(&mut line).unwrap() > 0, "server exited during startup");
        // `harbor X: serving <db> on 127.0.0.1:<port>[ + <socket>] (duckdb …)`
        if let Some((_, serving)) = line.split_once(": serving ")
            && let Some((_, on)) = serving.split_once(" on ")
            && let Some(address) = on.split_whitespace().next()
        {
            break address.to_string();
        }
    };
    // Whatever else it says goes nowhere, so a full pipe never stalls it.
    std::thread::spawn(move || std::io::copy(&mut said, &mut std::io::sink()));
    let target = format!("http://{address}");
    let quiet = |sql: &[&str]| harbor::repl::exec_quiet(&target, sql, &[]).unwrap();
    quiet(&["CREATE TABLE t(x INTEGER)", "INSERT INTO t VALUES (1)"]);
    let path = |name: &str| {
        format!(
            "'{}'",
            server
                .dir
                .join(name)
                .display()
                .to_string()
                .replace('\'', "''")
        )
    };
    harbor::repl::with_snapshot(&target, |execute| {
        execute(&format!("EXPORT DATABASE {} (FORMAT CSV)", path("export")))?;
        quiet(&["INSERT INTO t VALUES (2)"]);
        execute(&format!(
            "COPY t TO {} (FORMAT CSV, HEADER true)",
            path("again.csv")
        ))
    })
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(server.dir.join("again.csv")).unwrap(),
        "x\n1\n"
    );
    assert_eq!(
        std::fs::read_to_string(server.dir.join("export/t.csv")).unwrap(),
        "x\n1\n"
    );

    let started = Instant::now();
    let timed_out = harbor::repl::with_snapshot(&target, |execute| {
        execute("SELECT sum(sin(i)) FROM range(1000000000000) t(i)")
    });
    assert!(
        timed_out.is_err(),
        "backup must respect the operator statement timeout"
    );
    assert!(started.elapsed() < Duration::from_secs(5));

    let failed: Result<(), String> = harbor::repl::with_snapshot(&target, |execute| {
        execute("INSERT INTO t VALUES (3)")?;
        Err("simulated file inspection failure".into())
    });
    assert!(failed.is_err());
    quiet(&[&format!(
        "COPY (SELECT x FROM t ORDER BY x) TO {} (FORMAT CSV, HEADER true)",
        path("current.csv")
    )]);
    assert_eq!(
        std::fs::read_to_string(server.dir.join("current.csv")).unwrap(),
        "x\n1\n2\n"
    );
}
