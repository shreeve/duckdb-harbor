//! A multi-pass backup must retain its snapshot when another caller commits
//! between export and re-export. Exercise the real HTTP session helper.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
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
    if harbor::engine::engine().is_err() {
        return;
    }
    let base = if cfg!(unix) {
        PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    let dir = base.join(format!("hb-snapshot-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let child = Command::new(env!("CARGO_BIN_EXE_harbor"))
        .args([
            dir.join("source.duckdb").to_str().unwrap(),
            "start",
            "--port",
            &address.port().to_string(),
            "--workers",
            "1",
            "--statement-timeout",
            "1s",
        ])
        .env("HARBOR_HOME", dir.join("home"))
        .env("HARBOR_POOL_SIZE", "2")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut server = Server { child, dir };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(mut stream) = TcpStream::connect(address) {
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            stream
                .write_all(b"GET /ready HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .unwrap();
            let mut response = String::new();
            if stream.read_to_string(&mut response).is_ok() && response.starts_with("HTTP/1.1 200")
            {
                break;
            }
        }
        assert!(
            server.child.try_wait().unwrap().is_none(),
            "server exited during startup"
        );
        assert!(Instant::now() < deadline, "server never became ready");
        std::thread::sleep(Duration::from_millis(25));
    }
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
