//! Regression test for the bounded body drain (the DoS fix carried in this
//! crate): dropping a request whose declared Content-Length is huge must NOT
//! allocate anywhere near the declared size. Lives in its own file so the
//! measuring global allocator sees only this test.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{Read, Write};
use std::net::Shutdown;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

// (deliberately no shared support module: this binary must stay
// allocation-quiet apart from the code under test)
fn new_one_server_one_client() -> (justhttp::Server, std::net::TcpStream) {
    let server = justhttp::Server::http("0.0.0.0:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let client = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    (server, client)
}

struct MaxAlloc;
static LARGEST: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for MaxAlloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        LARGEST.fetch_max(l.size(), Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static A: MaxAlloc = MaxAlloc;

#[test]
fn big_declared_body_dropped_unread() {
    let (server, mut client) = new_one_server_one_client();

    // declare 1 GiB, send 5 bytes
    write!(
        client,
        "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 1073741824\r\n\r\nhello"
    )
    .unwrap();

    let rq = server.recv().unwrap();

    // half-close so the drain sees EOF instead of blocking for the rest
    client.shutdown(Shutdown::Write).unwrap();

    LARGEST.store(0, Relaxed);
    drop(rq); // auto-500 + body drain happen here, on this thread

    let largest = LARGEST.load(Relaxed);
    assert!(
        largest < 1 << 20,
        "dropping an unread 1 GiB-declared body allocated {largest} bytes; \
         the drain must stay bounded"
    );

    // the drop-path 500 still reaches the client
    let mut resp = String::new();
    client.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 500"), "got: {resp:.60}");
}

/// A version this server does not speak must be refused WITHOUT stranding the
/// connection.
///
/// The 505 path used to take a second writer from the sink while the request
/// still held the first. A sequential writer blocks on its predecessor's
/// release before its first byte, and that predecessor could only drop after
/// the write returned — so the thread parked forever, holding its descriptors,
/// in a channel wait no socket timeout covers. One unauthenticated
/// `GET / HTTP/2.0` leaked a thread and three descriptors permanently, and a
/// client merely attempting HTTP/2 tripped it by accident.
#[test]
fn an_unsupported_version_is_refused_without_stranding_the_thread() {
    use std::sync::mpsc;
    use std::time::Duration;

    let (server, mut client) = new_one_server_one_client();
    write!(client, "GET / HTTP/2.0\r\nHost: x\r\n\r\n").unwrap();

    // The server side runs on its own thread: if the 505 path deadlocks, this
    // never reports, which is the failure being pinned.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // No request is yielded — the version check answers and ends the
        // connection — so this returns only once that path completes.
        let got = server.recv_timeout(Duration::from_secs(5));
        tx.send(got.map(|o| o.is_some())).ok();
    });

    let mut resp = String::new();
    client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    client.read_to_string(&mut resp).ok();

    assert!(
        resp.starts_with("HTTP/1.1 505"),
        "a 505 must be answered as HTTP/1.1, not as the version being refused; got: {resp:.60}"
    );
    assert!(
        rx.recv_timeout(Duration::from_secs(10)).is_ok(),
        "the connection thread never came back — the 505 path stranded it"
    );
}
