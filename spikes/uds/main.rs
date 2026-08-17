// Phase 0 spike #2: tiny_http over a Unix socket.
//
// PASS requires all three, in order:
//   1. Server::http_unix binds and serves a request
//   2. a second request works (keep-alive loop intact)
//   3. unblock() makes a blocked recv() return, promptly
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn get(path: &std::path::Path, target: &str) -> String {
    let mut s = UnixStream::connect(path).expect("connect");
    write!(s, "GET {target} HTTP/1.1\r\nHost: harbor\r\nConnection: close\r\n\r\n").unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    buf
}

fn main() {
    let path = std::env::temp_dir().join(format!("harbor-spike-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);

    let server = Arc::new(tiny_http::Server::http_unix(&path).expect("bind unix socket"));
    println!("1. bound {}", path.display());

    let srv = server.clone();
    let serving = std::thread::spawn(move || {
        let mut served = 0u32;
        while let Ok(req) = srv.recv() {
            served += 1;
            req.respond(tiny_http::Response::from_string("{\"status\":\"ready\"}")).unwrap();
        }
        served // recv() returned Err => unblocked
    });

    for i in 0..2 {
        let resp = get(&path, "/ready");
        assert!(resp.starts_with("HTTP/1.1 200"), "request {i} failed:\n{resp}");
    }
    println!("2. served two sequential requests");

    let t0 = Instant::now();
    server.unblock();
    let served = serving.join().expect("serving thread");
    let dt = t0.elapsed();
    assert_eq!(served, 2);
    assert!(dt < Duration::from_secs(2), "unblock took {dt:?}");
    println!("3. unblock() released recv() in {dt:?}");

    let _ = std::fs::remove_file(&path);
    println!("UDS SPIKE PASS");
}
