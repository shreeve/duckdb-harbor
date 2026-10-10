//! The client side of Harbor's HTTP, for the harbor CLI and for DuckTable.
//!
//! One request per connection (`Connection: close`), blocking reads,
//! chunked and Content-Length bodies, over a unix socket or TCP. Harbor
//! speaks plain HTTP/1.1 and each client sends one request at a time, so an
//! async stack would be pure weight. Beside the transport live the pieces
//! both clients build on it: a failure that says whether the request left,
//! sessions and the touch that keeps one alive, the anchor that keeps a
//! summoned server present, the summon itself, and finding and stopping the
//! servers on this machine.

mod chunked;
#[cfg(unix)]
mod local;
mod session;
#[cfg(unix)]
mod summon;

#[cfg(unix)]
pub use local::{Found, discover, shutdown};
pub use session::{keep_alive, session_open, session_release, session_renew, session_touch};
#[cfg(unix)]
pub use summon::summon;

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;
use wire::Event;
use wire::endpoint::Route;

/// Where a Harbor server answers.
#[derive(Clone, Debug, PartialEq)]
pub enum Transport {
    #[cfg(unix)]
    Unix(PathBuf),
    Tcp(String), // host:port
}

pub struct Response {
    pub status: u16,
    pub body: Box<dyn BufRead + Send>,
}

impl Response {
    pub fn body_string(mut self) -> io::Result<String> {
        let mut s = String::new();
        self.body.read_to_string(&mut s)?;
        Ok(s)
    }
}

/// Why a request came to nothing. What the caller knows afterwards differs
/// between the three, which matters most for a `COMMIT`.
#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    /// Harbor answered with an error of its own: the engine's verdict
    /// (`sql_error`), or a refusal such as `no_such_session`. The request
    /// did not take effect.
    Refused { code: String, message: String },
    /// The request never reached Harbor whole: the connection could not be
    /// made, or the request could not be written. It did nothing.
    Unsent(String),
    /// The request was sent and no verdict arrived: its answer did not come,
    /// or could not be read to the end. It may have taken effect.
    Unanswered(String),
}

impl Failure {
    /// The session named in the request is gone: released, or reclaimed by
    /// the server at its idle timeout or its deadline, with its transaction
    /// rolled back.
    pub fn session_gone(&self) -> bool {
        matches!(self, Failure::Refused { code, .. } if code == wire::code::NO_SUCH_SESSION)
    }

    /// Harbor's verdict on an answer that is not a 2xx: its error document,
    /// or, for a body that is not one (a proxy's page), no verdict at all.
    pub fn of(status: u16, body: &str) -> Self {
        match Event::parse(body.trim()) {
            Ok(Event::Error { code, message }) => Failure::Refused { code, message },
            _ => Failure::Unanswered(format!("HTTP {status}")),
        }
    }
}

impl From<io::Error> for Failure {
    /// A request that failed on the way: unsent when it never left whole,
    /// unanswered otherwise.
    fn from(e: io::Error) -> Self {
        match was_not_sent(&e) {
            true => Failure::Unsent(e.to_string()),
            false => Failure::Unanswered(e.to_string()),
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Refused { code, message } => write!(f, "{code}: {message}"),
            Failure::Unsent(message) | Failure::Unanswered(message) => f.write_str(message),
        }
    }
}

/// A request whose answer is one small JSON document: sent, checked for a
/// 2xx, and decoded.
pub fn call<T: serde::de::DeserializeOwned>(
    transport: &Transport,
    route: &Route,
    body: Option<&str>,
    patience: Duration,
) -> Result<T, Failure> {
    let response = request(transport, route, body, Some(patience))?;
    let status = response.status;
    let text = response.body_string()?;
    if !(200..300).contains(&status) {
        return Err(Failure::of(status, &text));
    }
    serde_json::from_str(text.trim()).map_err(|e| Failure::Unanswered(format!("{route}: {e}")))
}

/// `GET /ready`, 200 or not.
pub fn ready(transport: &Transport) -> bool {
    matches!(
        request(transport, &wire::endpoint::READY, None, Some(Duration::from_secs(2))),
        Ok(r) if r.status == 200
    )
}

/// A thread that takes a step at intervals until it is dropped or a step
/// returns `None`. Each step names the wait before the next. Dropping it
/// stops the thread and waits for it, so nothing it does outlives it.
pub struct Beat {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

pub fn beat(
    name: &str,
    first: Duration,
    mut step: impl FnMut() -> Option<Duration> + Send + 'static,
) -> io::Result<Beat> {
    let (stop, stopped) = mpsc::channel::<()>();
    let thread = std::thread::Builder::new().name(name.to_string()).spawn(move || {
        let mut wait = first;
        while let Err(mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(wait) {
            match step() {
                Some(next) => wait = next,
                None => break,
            }
        }
    })?;
    Ok(Beat { stop: Some(stop), thread: Some(thread) })
}

impl Drop for Beat {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The mooring: one quiet connection, held open while a client is.
///
/// A summoned server's lifetime is its client count, so what keeps it
/// ashore while a person thinks between statements is presence. Presence
/// has to be spoken: a connection that has never sent a request is
/// reclaimed at the server's first-request timeout, and its silence is
/// indistinguishable from an anonymous caller sitting on a descriptor. So
/// the anchor asks `/ready` once, which buys the connection the long idle
/// clock, and renews inside it. Dropping the anchor is the goodbye.
pub type Anchor = Beat;

/// Inside the server's 300 s idle clock, with enough margin left that a
/// renewal can fail and be retried before the mooring is at risk.
const RENEW: Duration = Duration::from_secs(240);
const RETRY: Duration = Duration::from_secs(5);

pub fn hold(transport: &Transport) -> io::Result<Anchor> {
    let mut held = moor(transport)?;
    let transport = transport.clone();
    beat("harbor-anchor", RENEW, move || match moor(&transport) {
        // Moor the new one before letting the old one go: the swap must
        // never be the moment the count touches zero.
        Ok(next) => {
            drop(std::mem::replace(&mut held, next));
            Some(RENEW)
        }
        // Keep the connection already held and try again well inside what
        // is left of the margin.
        Err(_) => Some(RETRY),
    })
}

/// One `/ready` on a connection that stays open afterwards. The body is
/// never read: the answer's arrival is what marks the connection as having
/// spoken.
fn moor(transport: &Transport) -> io::Result<Response> {
    let response = request_inner(transport, &wire::endpoint::READY, None, Some(RETRY), None, true)?;
    if response.status != 200 {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("harbor readiness returned HTTP {}", response.status),
        ));
    }
    Ok(response)
}

/// The most a response head may hold: per line, and lines.
const HEAD_LINE: u64 = 8 * 1024;
const HEAD_LINES: usize = 100;

trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

/// The route carries its own verb, so a caller cannot pair `GET` with a path
/// harbor only answers to `POST`, the mistake that reads as a 404. `timeout`
/// bounds each read of the answer; a write, and a TCP connect, get at least
/// five seconds.
pub fn request(
    transport: &Transport,
    route: &Route,
    body: Option<&str>,
    timeout: Option<Duration>,
) -> io::Result<Response> {
    request_inner(transport, route, body, timeout, None, false)
}

/// Like `request`, but built for long-running `/sql` streams: the socket gets
/// a short read timeout so the caller's read loop ticks (and can notice a
/// Ctrl-C), while status and header reads here retry through those ticks.
/// A callback error aborts the request, including before any headers arrive.
pub fn request_streaming(
    transport: &Transport,
    route: &Route,
    body: Option<&str>,
    on_tick: &dyn Fn() -> io::Result<()>,
) -> io::Result<Response> {
    request_inner(transport, route, body, Some(Duration::from_millis(250)), Some(on_tick), false)
}

/// Marks an error raised before the whole request was on the wire: the
/// connection could not be made, or the request could not be written. The
/// server acts on a request only once it has all of it, so such a request
/// did nothing.
#[derive(Debug)]
struct NotSent(io::Error);

impl std::fmt::Display for NotSent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for NotSent {}

fn not_sent(e: io::Error) -> io::Error {
    io::Error::new(e.kind(), NotSent(e))
}

/// Whether a `request` error means the request never reached the server
/// whole, so it had no effect there. Any other error came while waiting for
/// or reading the answer, and says nothing about what the server did.
pub fn was_not_sent(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<NotSent>())
}

fn request_inner(
    transport: &Transport,
    route: &Route,
    body: Option<&str>,
    timeout: Option<Duration>,
    on_tick: Option<&dyn Fn() -> io::Result<()>>,
    keep_alive: bool,
) -> io::Result<Response> {
    // A server that stopped reading must not hold a write forever, and a
    // short read tick is no measure of how long a write may take.
    let write_timeout = timeout.map(|t| t.max(Duration::from_secs(5)));
    let (mut stream, host): (Box<dyn Stream>, String) = match transport {
        #[cfg(unix)]
        Transport::Unix(p) => {
            let s = UnixStream::connect(p).map_err(not_sent)?;
            s.set_read_timeout(timeout).map_err(not_sent)?;
            s.set_write_timeout(write_timeout).map_err(not_sent)?;
            (Box::new(s), "harbor".to_string())
        }
        Transport::Tcp(addr) => {
            let s = match write_timeout {
                Some(patience) => {
                    let mut result = Err(io::Error::new(io::ErrorKind::AddrNotAvailable, "no server address"));
                    for address in addr.to_socket_addrs().map_err(not_sent)? {
                        result = TcpStream::connect_timeout(&address, patience);
                        if result.is_ok() {
                            break;
                        }
                    }
                    result.map_err(not_sent)?
                }
                None => TcpStream::connect(addr).map_err(not_sent)?,
            };
            s.set_read_timeout(timeout).map_err(not_sent)?;
            s.set_write_timeout(write_timeout).map_err(not_sent)?;
            (Box::new(s), addr.clone())
        }
    };

    let connection = if keep_alive { "keep-alive" } else { "close" };
    let mut req = format!("{route} HTTP/1.1\r\nHost: {host}\r\nConnection: {connection}\r\n");
    req.push_str(&format!("Accept: {}\r\n", wire::CONTENT_NDJSON));
    if let Some(b) = body {
        req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).map_err(not_sent)?;
    if let Some(b) = body {
        stream.write_all(b.as_bytes()).map_err(not_sent)?;
    }
    stream.flush().map_err(not_sent)?;

    let mut reader = BufReader::new(stream);
    // Headers may not arrive until the statement completes (the server
    // responds once execution starts producing), so the wait happens here,
    // which is why the tick callback fires here: it is how a Ctrl-C reaches
    // a query that has not sent a byte yet. A line is read only so far, and
    // so many of them: what answers on a port that is not Harbor's must not
    // grow the head without end.
    let read_line = |reader: &mut BufReader<Box<dyn Stream>>, line: &mut String| -> io::Result<usize> {
        loop {
            if let Some(f) = on_tick {
                f()?;
            }
            let room = HEAD_LINE.saturating_sub(line.len() as u64);
            match reader.by_ref().take(room).read_line(line) {
                Err(e) if on_tick.is_some() && matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                Ok(_) if line.len() as u64 >= HEAD_LINE && !line.ends_with('\n') => {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "a response header line runs past 8 KiB"));
                }
                other => return other,
            }
        }
    };
    let status = {
        let mut line = String::new();
        read_line(&mut reader, &mut line)?;
        line.split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("bad status line: {line:?}")))?
    };

    let mut chunked = false;
    let mut content_length: Option<u64> = None;
    for count in 0.. {
        let mut line = String::new();
        read_line(&mut reader, &mut line)?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if count == HEAD_LINES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a response has more than 100 header lines"));
        }
        if let Some((k, v)) = line.split_once(':') {
            let v = v.trim();
            match k.to_ascii_lowercase().as_str() {
                "transfer-encoding" if v.eq_ignore_ascii_case("chunked") => chunked = true,
                "content-length" => content_length = v.parse().ok(),
                _ => {}
            }
        }
    }

    let body: Box<dyn BufRead + Send> = if chunked {
        Box::new(BufReader::new(chunked::ChunkedReader::new(reader)))
    } else if let Some(n) = content_length {
        Box::new(BufReader::new(reader.take(n)))
    } else {
        Box::new(reader) // read to EOF (Connection: close)
    };
    Ok(Response { status, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read one request whole, and answer it with `reply` (or hang up when
    /// it is empty). Returns the request's first line.
    pub(crate) fn answer(stream: &mut TcpStream, reply: &str) -> String {
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];
        while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
            head.push(byte[0]);
        }
        let head = String::from_utf8(head).unwrap();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .map_or(0, |n| n.parse().unwrap());
        stream.read_exact(&mut vec![0; length]).unwrap();
        stream.write_all(reply.as_bytes()).unwrap();
        head.lines().next().unwrap_or_default().to_string()
    }

    /// A server on a fresh port that answers one request with `reply`.
    pub(crate) fn one_shot(reply: &'static str) -> (Transport, JoinHandle<String>) {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || answer(&mut listener.accept().unwrap().0, reply));
        (Transport::Tcp(addr.to_string()), server)
    }

    #[test]
    fn a_request_that_cannot_connect_was_not_sent() {
        // Nothing listens on a socket path that does not exist, nor on a
        // port just closed: the request never left.
        #[cfg(unix)]
        {
            let nowhere = Transport::Unix(std::env::temp_dir().join("harbor-http-no-such.sock"));
            let e = request(&nowhere, &wire::endpoint::READY, None, None).err().unwrap();
            assert!(was_not_sent(&e), "{e}");
            assert!(matches!(Failure::from(e), Failure::Unsent(_)));
        }
        let port = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap();
        for timeout in [None, Some(Duration::from_secs(1))] {
            let e = request(&Transport::Tcp(port.to_string()), &wire::endpoint::READY, None, timeout)
                .err()
                .unwrap();
            assert!(was_not_sent(&e), "{e}");
        }

        // A server that takes the request and hangs up without a word: the
        // request was sent, and what became of it is unknown.
        let (transport, server) = one_shot("");
        let e = request(&transport, &wire::endpoint::READY, None, None).err().unwrap();
        assert!(!was_not_sent(&e), "{e}");
        assert!(matches!(Failure::from(e), Failure::Unanswered(_)));
        server.join().unwrap();
    }

    #[test]
    fn an_answer_that_is_not_harbors_is_no_verdict() {
        let refused = Failure::of(404, r#"{"type":"error","code":"no_such_session","message":"gone"}"#);
        assert!(refused.session_gone());
        assert_eq!(refused.to_string(), "no_such_session: gone");
        assert_eq!(Failure::of(502, "<html>"), Failure::Unanswered("HTTP 502".into()));
        assert!(!Failure::Unsent("x".into()).session_gone());
    }

    #[test]
    fn a_head_that_runs_on_is_refused() {
        let long: &'static str = format!("HTTP/1.1 200 OK\r\nX: {}\r\n\r\n", "a".repeat(9000)).leak();
        let many: &'static str = format!("HTTP/1.1 200 OK\r\n{}\r\n", "X: 1\r\n".repeat(101)).leak();
        for reply in [long, many] {
            let (transport, server) = one_shot(reply);
            let e = request(&transport, &wire::endpoint::READY, None, None).err().unwrap();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
            server.join().unwrap();
        }
        let (transport, server) = one_shot("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        assert_eq!(request(&transport, &wire::endpoint::READY, None, None).unwrap().body_string().unwrap(), "ok");
        server.join().unwrap();
    }

    #[test]
    fn a_tick_error_abandons_a_request_still_waiting_for_its_headers() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || listener.accept().map(|(stream, _)| stream));
        let ticks = std::cell::Cell::new(0);
        let tick = || {
            ticks.set(ticks.get() + 1);
            match ticks.get() > 2 {
                true => Err(io::Error::other("interrupted")),
                false => Ok(()),
            }
        };
        let e = request_streaming(&Transport::Tcp(addr.to_string()), &wire::endpoint::READY, None, &tick)
            .err()
            .unwrap();
        assert_eq!(e.to_string(), "interrupted");
        assert_eq!(ticks.get(), 3);
        drop(server.join().unwrap());
    }

    #[test]
    fn a_beat_steps_until_told_to_stop_and_stops_when_dropped() {
        let (seen, steps) = mpsc::channel();
        let mut left = 3;
        let finite = beat("test-beat", Duration::ZERO, move || {
            seen.send(()).unwrap();
            left -= 1;
            (left > 0).then_some(Duration::ZERO)
        })
        .unwrap();
        assert_eq!(steps.iter().count(), 3);
        drop(finite);
        let endless = beat("test-beat", Duration::from_secs(3600), || Some(Duration::from_secs(3600))).unwrap();
        let begun = std::time::Instant::now();
        drop(endless);
        assert!(begun.elapsed() < Duration::from_secs(5));
    }
}
