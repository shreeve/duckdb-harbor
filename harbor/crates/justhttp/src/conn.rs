//! One client connection: read requests off the socket in sequence, hand
//! each to the server's queue, and keep the reader honest — line-length
//! ceilings, version checks, and the keep-alive/close decision all live
//! here, below routing and below any authentication.

use std::io::Error as IoError;
use std::io::Result as IoResult;
use std::io::{BufReader, BufWriter, ErrorKind, Read};

use std::net::SocketAddr;
use std::str::FromStr;
use std::time::{Duration, Instant};

use crate::Request;
use crate::http::{HttpVersion, Method, StatusCode};
use crate::stream::{RefinedTcpStream, ShutdownHandle};
use sequential::{SequentialReader, SequentialReaderBuilder, SequentialWriterBuilder};

/// The largest request line or header line we will assemble.
///
/// There was no ceiling here at all, and the buffer grows a byte at a time
/// until CRLF — so an unauthenticated client could open one socket, send
/// `GET / HTTP/1.1\r\nX-Junk: ` and then never stop, and watch the server's
/// RSS climb at line speed (measured: 30 MB to 1.5 GB in under five seconds).
/// The check has to live here, below routing and below any authentication,
/// because the allocation happens before either of them can run.
const MAX_LINE: usize = 8 * 1024;

/// The largest number of header lines in one request. Same reasoning as
/// `MAX_LINE` — bounded lines are no help if there can be a million of them.
const MAX_HEADERS: usize = 128;

/// How long the whole request head (request line plus every header) may take
/// to arrive, measured from its first byte.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a brand-new connection may say nothing at all before it is closed.
///
/// A socket that has never sent a byte has not asked for anything, and letting
/// it wait forever meant an anonymous caller could hold connections — and a
/// thread apiece — for as long as it liked, bounded only by the
/// file-descriptor limit.
const FIRST_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How long an established keep-alive connection may sit between requests.
///
/// This clock used to not exist: serving one request took a connection off the
/// first-request timeout permanently, on the reasoning that a REPL at its
/// prompt or a pooled client between queries is doing nothing wrong. The gap
/// is that the cheapest request on the server is also the unauthenticated one
/// — harbor answers `/ready` without a token on purpose, so a load balancer
/// need not hold a credential — so one anonymous `/ready` bought a connection
/// the right to idle forever. Measured: 120 such connections held 120 threads
/// and 240 descriptors indefinitely, still answering after 100 seconds idle,
/// while 120 that said nothing at all were reclaimed on schedule.
///
/// Five minutes is far longer than any pooled client's own idle timeout (30–90
/// seconds is typical, and the repl sends `Connection: close` outright), so a
/// legitimate client either speaks again well inside it or reconnects without
/// noticing. It does not make holding connections impossible — nothing here
/// caps concurrent connections — but it does mean they have to be paid for
/// again every five minutes instead of being taken once and kept.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// A socket read timeout, as opposed to a real I/O failure. macOS reports
/// `SO_RCVTIMEO` as `WouldBlock` and Linux as `TimedOut`; both mean "nothing
/// arrived in the window", which is a decision point rather than an error.
fn is_read_timeout(e: &IoError) -> bool {
    matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

/// A ClientConnection is an object that will store a socket to a client
/// and return Request objects.
pub struct ClientConnection {
    // address of the client
    remote_addr: IoResult<Option<SocketAddr>>,

    // whether this connection came in over the unix-socket listener; every
    // request it produces is stamped with it (see Request::is_local)
    local: bool,

    // sequence of Readers to the stream, so that the data is not read in
    //  the wrong order
    source: SequentialReaderBuilder<BufReader<RefinedTcpStream>>,

    // sequence of Writers to the stream, to avoid writing response #2 before
    //  response #1
    sink: SequentialWriterBuilder<BufWriter<RefinedTcpStream>>,

    // Reader to read the next header from
    next_header_source: SequentialReader<BufReader<RefinedTcpStream>>,

    // set to true if we know that the previous request is the last one
    no_more_requests: bool,

    // whether this connection has produced a request yet; only before that is
    // it on the FIRST_REQUEST_TIMEOUT clock
    served_a_request: bool,

    // how the bounded body drain ends this connection when it gives up on a
    // body the client is still dribbling (see EqualReader's Drop)
    shutdown: Option<ShutdownHandle>,
}

/// Error that can happen when reading a request.
#[derive(Debug)]
enum ReadError {
    WrongRequestLine,
    WrongHeader(HttpVersion),
    /// A request line or header line over `MAX_LINE`, or more than
    /// `MAX_HEADERS` of them. Carries the status to answer with: 414 for an
    /// oversized request line, 431 for the header block.
    HeadTooLarge(StatusCode),
    /// Content-Length and Transfer-Encoding disagree, or two Content-Lengths
    /// do. Ambiguous framing is a request-smuggling primitive, not a quirk to
    /// resolve by picking one.
    AmbiguousFraming(HttpVersion),
    /// the client sent an unrecognized `Expect` header
    ExpectationFailed(HttpVersion),
    ReadIoError(IoError),
}

impl ClientConnection {
    /// Creates a new `ClientConnection` that takes ownership of the stream.
    /// `local` records which listener accepted it: true for the unix socket,
    /// false for TCP.
    pub fn new(
        write_socket: RefinedTcpStream,
        mut read_socket: RefinedTcpStream,
        local: bool,
    ) -> ClientConnection {
        let remote_addr = read_socket.peer_addr();
        // Taken while the stream is still here, exactly as the interrupt
        // handles are on the harbor side: nothing can reach this socket once
        // it is inside the BufReader.
        let shutdown = read_socket.shutdown_handle().ok();

        let mut source = SequentialReaderBuilder::new(BufReader::with_capacity(1024, read_socket));
        let first_header = source.next().unwrap();

        ClientConnection {
            source,
            // 8192 matches the chunked encoder's internal buffer, so headers +
            // a full chunk + the terminator coalesce into a single write().
            sink: SequentialWriterBuilder::new(BufWriter::with_capacity(8192, write_socket)),
            remote_addr,
            local,
            next_header_source: first_header,
            no_more_requests: false,
            served_a_request: false,
            shutdown,
        }
    }

    /// Reads the next line from self.next_header_source.
    ///
    /// Reads until `CRLF` is reached. The next read will start
    ///  at the first byte of the new line.
    ///
    /// `deadline` is the budget for the whole request head, shared across the
    /// request line and every header line, and started by the head's first
    /// byte — so a client cannot refresh it by dribbling one byte per line.
    /// `None` on entry means nothing of this request has arrived yet, so the
    /// connection is merely idle and waits on the longer connection clocks
    /// (`FIRST_REQUEST_TIMEOUT` or `IDLE_TIMEOUT`) rather than this one.
    /// `too_large` is the status to report if this line runs past `MAX_LINE`.
    // byte-at-a-time on purpose: the source is already a BufReader, and this
    // exact framing (CRLF only, Interrupted retried by Bytes) is semantics.
    #[allow(clippy::unbuffered_bytes)]
    fn read_next_line<'b>(
        &mut self,
        buf: &'b mut Vec<u8>,
        deadline: &mut Option<Instant>,
        too_large: StatusCode,
    ) -> Result<&'b str, ReadError> {
        buf.clear();
        let mut prev_byte_was_cr = false;
        // Nothing of this request has arrived yet, so this is how long the
        // connection may stay quiet: a short clock before it has ever asked
        // for anything, a long one between keep-alive requests.
        let quiet_until = Instant::now()
            + match self.served_a_request {
                true => IDLE_TIMEOUT,
                false => FIRST_REQUEST_TIMEOUT,
            };

        loop {
            let byte = self.next_header_source.by_ref().bytes().next();

            let byte = match byte {
                Some(Ok(b)) => b,
                Some(Err(ref e)) if is_read_timeout(e) => match *deadline {
                    // Idle: this request has not begun. Both the never-spoke
                    // and the between-requests cases are on a clock; only the
                    // length differs.
                    None if Instant::now() >= quiet_until => {
                        return Err(ReadError::ReadIoError(IoError::new(
                            ErrorKind::TimedOut,
                            match self.served_a_request {
                                true => "the connection was idle past the keep-alive timeout",
                                false => "no request on a new connection within the first-request timeout",
                            },
                        )));
                    }
                    None => continue,
                    Some(d) if Instant::now() < d => continue,
                    // Normalized so the 408 arm in `next()` fires on both
                    // platforms, whichever kind the OS reported.
                    Some(_) => {
                        return Err(ReadError::ReadIoError(IoError::new(
                            ErrorKind::TimedOut,
                            "the request head did not arrive within the head timeout",
                        )));
                    }
                },
                Some(Err(e)) => return Err(ReadError::ReadIoError(e)),
                None => {
                    return Err(ReadError::ReadIoError(IoError::new(
                        ErrorKind::ConnectionAborted,
                        "Unexpected EOF",
                    )));
                }
            };

            // The head has started: from here the client is on the clock.
            if deadline.is_none() {
                *deadline = Some(Instant::now() + HEAD_TIMEOUT);
            }

            if byte == b'\n' && prev_byte_was_cr {
                buf.pop(); // removing the '\r'
                if !buf.is_ascii() {
                    return Err(ReadError::ReadIoError(IoError::new(
                        ErrorKind::InvalidInput,
                        "Header is not in ASCII",
                    )));
                }
                // ASCII was just verified, and ASCII is valid UTF-8.
                return Ok(std::str::from_utf8(buf).unwrap());
            }

            prev_byte_was_cr = byte == b'\r';

            buf.push(byte);
            if buf.len() > MAX_LINE {
                return Err(ReadError::HeadTooLarge(too_large));
            }
        }
    }

    /// Reads a request from the stream.
    /// Blocks until the header has been read.
    fn read(&mut self) -> Result<Request, ReadError> {
        let (method, path, version, headers) = {
            // one line buffer reused for the request line and every header line
            let mut line_buf = Vec::with_capacity(128);
            // One budget for the whole head, started by its first byte.
            let mut deadline: Option<Instant> = None;

            // reading the request line
            let (method, path, version) = {
                let line =
                    self.read_next_line(&mut line_buf, &mut deadline, StatusCode(414))?;

                parse_request_line(line.trim())?
            };

            // getting all headers
            let headers = {
                let mut headers = Vec::with_capacity(16);
                loop {
                    let line =
                        self.read_next_line(&mut line_buf, &mut deadline, StatusCode(431))?;

                    if line.is_empty() {
                        break;
                    };
                    if headers.len() >= MAX_HEADERS {
                        return Err(ReadError::HeadTooLarge(StatusCode(431)));
                    }
                    headers.push(match FromStr::from_str(line.trim()) {
                        Ok(h) => h,
                        _ => return Err(ReadError::WrongHeader(version)),
                    });
                }

                headers
            };

            check_framing(&headers).map_err(|()| ReadError::AmbiguousFraming(version))?;

            (method, path, version, headers)
        };

        // building the writer for the request
        let writer = self.sink.next().unwrap();

        // follow-up for next potential request
        let mut data_source = self.source.next().unwrap();
        std::mem::swap(&mut self.next_header_source, &mut data_source);

        // building the next reader
        let request = crate::request::new_request(
            method,
            path,
            version,
            headers,
            *self.remote_addr.as_ref().unwrap(),
            self.local,
            data_source,
            writer,
            self.shutdown.clone(),
        )
        .map_err(|e| {
            use crate::request;
            match e {
                request::RequestCreationError::CreationIoError(e) => ReadError::ReadIoError(e),
                request::RequestCreationError::ExpectationFailed => {
                    ReadError::ExpectationFailed(version)
                }
            }
        })?;

        // return the request
        Ok(request)
    }
}

impl Iterator for ClientConnection {
    type Item = Request;

    /// Blocks until the next Request is available.
    /// Returns None when no new Requests will come from the client.
    fn next(&mut self) -> Option<Request> {
        use crate::{Response, StatusCode};

        // the client sent a "connection: close" header in this previous request
        //  or is using HTTP 1.0, meaning that no new request will come
        if self.no_more_requests {
            return None;
        }

        // Not a loop any more, and deliberately so: every arm below either
        // yields a request or ends the connection. The 505 arm was the one
        // path that used to go round again, and it no longer can — a peer
        // that opened with a version this server cannot speak gets its answer
        // and the connection closes.
        {
            let rq = match self.read() {
                Err(ReadError::WrongRequestLine) => {
                    let writer = self.sink.next().unwrap();
                    let response = Response::empty(StatusCode(400));
                    response
                        .raw_print(writer, HttpVersion(1, 1), &[], false)
                        .ok();
                    return None; // we don't know where the next request would start,
                    // so we have to close
                }

                Err(ReadError::WrongHeader(ver)) => {
                    let writer = self.sink.next().unwrap();
                    let response = Response::empty(StatusCode(400));
                    response.raw_print(writer, ver, &[], false).ok();
                    return None; // we don't know where the next request would start,
                    // so we have to close
                }

                Err(ReadError::ReadIoError(ref err)) if err.kind() == ErrorKind::TimedOut => {
                    // request timeout
                    let writer = self.sink.next().unwrap();
                    let response = Response::empty(StatusCode(408));
                    response
                        .raw_print(writer, HttpVersion(1, 1), &[], false)
                        .ok();
                    return None; // closing the connection
                }

                Err(ReadError::HeadTooLarge(status)) => {
                    let writer = self.sink.next().unwrap();
                    let response = Response::empty(status);
                    response
                        .raw_print(writer, HttpVersion(1, 1), &[], false)
                        .ok();
                    return None; // the head is unbounded from here; close
                }

                Err(ReadError::AmbiguousFraming(ver)) => {
                    let writer = self.sink.next().unwrap();
                    let response = Response::empty(StatusCode(400));
                    response.raw_print(writer, ver, &[], false).ok();
                    return None; // we cannot know where the body ends, so close
                }

                Err(ReadError::ExpectationFailed(ver)) => {
                    let writer = self.sink.next().unwrap();
                    let response = Response::empty(StatusCode(417));
                    response.raw_print(writer, ver, &[], true).ok();
                    return None; // TODO: should be recoverable, but needs handling in case of body
                }

                Err(ReadError::ReadIoError(_)) => return None,

                Ok(rq) => rq,
            };

            // checking HTTP version
            if *rq.http_version() > (1, 1) {
                // Answered through the request's OWN writer, and then the
                // connection ends.
                //
                // This used to take a SECOND writer from the sink while `rq`
                // still held the first, and that deadlocked the thread
                // outright: a sequential writer blocks on its predecessor's
                // release before its first byte (see SequentialWriter::write),
                // and the predecessor was owned by an `rq` that could only drop
                // after the write returned. So the connection thread parked
                // forever, holding its descriptors, in a wait no socket timeout
                // covers because it is a channel and not a read. One
                // unauthenticated `GET / HTTP/2.0` cost a thread and three
                // descriptors permanently, and a client merely *attempting*
                // HTTP/2 — curl --http2, an h2c upgrade probe — triggered it by
                // accident.
                //
                // `return None` rather than `continue` for the same reason RFC
                // 9110 pairs 505 with closing: a peer that opened with a
                // version this server cannot speak has nothing useful to say
                // next on the same connection.
                let response = Response::from_string(
                    "This server only supports HTTP versions 1.0 and 1.1".to_owned(),
                )
                .with_status_code(StatusCode(505));
                let mut rq = rq;
                rq.answer_as_http11();
                rq.respond(response).ok();
                return None;
            }

            // updating the status of the connection
            let connection_header = rq
                .headers()
                .iter()
                .find(|h| h.field.equiv("Connection"))
                .map(|h| h.value.as_str());

            // case-insensitive substring match (NOT token-wise): exactly the
            // lowercase-then-contains behavior this replaced, minus the alloc
            fn contains_ignore_case(hay: &str, needle: &str) -> bool {
                hay.as_bytes()
                    .windows(needle.len())
                    .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
            }

            match connection_header {
                Some(val) if contains_ignore_case(val, "close") => self.no_more_requests = true,
                Some(val) if contains_ignore_case(val, "upgrade") => self.no_more_requests = true,
                // Every HTTP/1.0 request is the last one on its connection,
                // `Connection: keep-alive` or not. 1.0 has no chunked encoding,
                // so a response of unknown length can only be delimited by the
                // close — which response.rs now relies on instead of buffering
                // the whole body in memory to discover its length. Reusing a
                // 1.0 connection and streaming an unknown length are mutually
                // exclusive; this picks the one that cannot be turned into an
                // unbounded allocation by a client that asks for a big result.
                _ if *rq.http_version() == HttpVersion(1, 0) => self.no_more_requests = true,
                _ => (),
            };

            // From here this is a keep-alive connection, off the
            // first-request clock.
            self.served_a_request = true;

            // returning the request
            Some(rq)
        }
    }
}

/// Reject a request whose body length is ambiguous.
///
/// Two `Content-Length` headers that disagree, or a `Content-Length` alongside
/// a `Transfer-Encoding`, do not have a right answer — they have two, and the
/// gap between the one this server picks and the one a proxy in front of it
/// picks is exactly a request-smuggling desync. RFC 9110 §8.6 says to reject,
/// and rejecting costs nothing: no correct client sends either shape.
///
/// A `Content-Length` that is not a number is refused for the same reason. It
/// used to parse as `None` and the request was served as though it had no body
/// at all, leaving the bytes the client did send to be read as the next
/// request on the connection.
fn check_framing(headers: &[crate::http::Header]) -> Result<(), ()> {
    let mut lengths = headers
        .iter()
        .filter(|h| h.field.equiv("Content-Length"))
        .map(|h| h.value.as_str().trim().parse::<usize>());

    let first = match lengths.next() {
        None => return Ok(()),
        Some(Ok(n)) => n,
        Some(Err(_)) => return Err(()),
    };
    for other in lengths {
        if !matches!(other, Ok(n) if n == first) {
            return Err(());
        }
    }
    // A body cannot be framed two ways at once.
    if headers.iter().any(|h| h.field.equiv("Transfer-Encoding")) {
        return Err(());
    }
    Ok(())
}

/// Parses a "HTTP/1.1" string.
fn parse_http_version(version: &str) -> Result<HttpVersion, ReadError> {
    let (major, minor) = match version {
        "HTTP/0.9" => (0, 9),
        "HTTP/1.0" => (1, 0),
        "HTTP/1.1" => (1, 1),
        "HTTP/2.0" => (2, 0),
        "HTTP/3.0" => (3, 0),
        _ => return Err(ReadError::WrongRequestLine),
    };

    Ok(HttpVersion(major, minor))
}

/// Parses the request line of the request.
/// eg. GET / HTTP/1.1
fn parse_request_line(line: &str) -> Result<(Method, String, HttpVersion), ReadError> {
    let mut parts = line.split(' ');

    let method = parts.next().and_then(|w| w.parse().ok());
    let path = parts.next().map(ToOwned::to_owned);
    let version = parts.next().and_then(|w| parse_http_version(w).ok());

    method
        .and_then(|method| Some((method, path?, version?)))
        .ok_or(ReadError::WrongRequestLine)
}

#[cfg(test)]
mod test {
    #[test]
    fn test_parse_request_line() {
        let (method, path, ver) = super::parse_request_line("GET /hello HTTP/1.1").unwrap();

        assert!(method == crate::Method::Get);
        assert!(path == "/hello");
        assert!(ver == crate::http::HttpVersion(1, 1));

        assert!(super::parse_request_line("GET /hello").is_err());
        assert!(super::parse_request_line("qsd qsd qsd").is_err());
    }
}

mod sequential {
    use std::io::Result as IoResult;
    use std::io::{Read, Write};

    use std::sync::mpsc::channel;
    use std::sync::mpsc::{Receiver, Sender};
    use std::sync::{Arc, Mutex};

    use std::mem;

    pub struct SequentialReaderBuilder<R>
    where
        R: Read + Send,
    {
        inner: SequentialReaderBuilderInner<R>,
    }

    enum SequentialReaderBuilderInner<R>
    where
        R: Read + Send,
    {
        First(R),
        NotFirst(Receiver<R>),
    }

    pub struct SequentialReader<R>
    where
        R: Read + Send,
    {
        inner: SequentialReaderInner<R>,
        next: Sender<R>,
    }

    enum SequentialReaderInner<R>
    where
        R: Read + Send,
    {
        MyTurn(R),
        Waiting(Receiver<R>),
        Empty,
    }

    pub struct SequentialWriterBuilder<W>
    where
        W: Write + Send,
    {
        writer: Arc<Mutex<W>>,
        next_trigger: Option<Receiver<()>>,
    }

    pub struct SequentialWriter<W>
    where
        W: Write + Send,
    {
        trigger: Option<Receiver<()>>,
        writer: Arc<Mutex<W>>,
        on_finish: Sender<()>,
    }

    impl<R: Read + Send> SequentialReaderBuilder<R> {
        pub fn new(reader: R) -> SequentialReaderBuilder<R> {
            SequentialReaderBuilder {
                inner: SequentialReaderBuilderInner::First(reader),
            }
        }
    }

    impl<W: Write + Send> SequentialWriterBuilder<W> {
        pub fn new(writer: W) -> SequentialWriterBuilder<W> {
            SequentialWriterBuilder {
                writer: Arc::new(Mutex::new(writer)),
                next_trigger: None,
            }
        }
    }

    impl<R: Read + Send> Iterator for SequentialReaderBuilder<R> {
        type Item = SequentialReader<R>;

        fn next(&mut self) -> Option<SequentialReader<R>> {
            let (tx, rx) = channel();

            let inner = mem::replace(&mut self.inner, SequentialReaderBuilderInner::NotFirst(rx));

            match inner {
                SequentialReaderBuilderInner::First(reader) => Some(SequentialReader {
                    inner: SequentialReaderInner::MyTurn(reader),
                    next: tx,
                }),

                SequentialReaderBuilderInner::NotFirst(previous) => Some(SequentialReader {
                    inner: SequentialReaderInner::Waiting(previous),
                    next: tx,
                }),
            }
        }
    }

    impl<W: Write + Send> Iterator for SequentialWriterBuilder<W> {
        type Item = SequentialWriter<W>;
        fn next(&mut self) -> Option<SequentialWriter<W>> {
            let (tx, rx) = channel();
            let mut next_next_trigger = Some(rx);
            ::std::mem::swap(&mut next_next_trigger, &mut self.next_trigger);

            Some(SequentialWriter {
                trigger: next_next_trigger,
                writer: self.writer.clone(),
                on_finish: tx,
            })
        }
    }

    impl<R: Read + Send> Read for SequentialReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
            let mut reader = match self.inner {
                SequentialReaderInner::MyTurn(ref mut reader) => return reader.read(buf),
                SequentialReaderInner::Waiting(ref mut recv) => recv.recv().unwrap(),
                SequentialReaderInner::Empty => unreachable!(),
            };

            let result = reader.read(buf);
            self.inner = SequentialReaderInner::MyTurn(reader);
            result
        }
    }

    impl<W: Write + Send> Write for SequentialWriter<W> {
        fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
            if let Some(v) = self.trigger.as_mut() {
                v.recv().unwrap()
            }
            self.trigger = None;

            self.writer.lock().unwrap().write(buf)
        }

        fn flush(&mut self) -> IoResult<()> {
            if let Some(v) = self.trigger.as_mut() {
                v.recv().unwrap()
            }
            self.trigger = None;

            self.writer.lock().unwrap().flush()
        }
    }

    impl<R> Drop for SequentialReader<R>
    where
        R: Read + Send,
    {
        fn drop(&mut self) {
            let inner = mem::replace(&mut self.inner, SequentialReaderInner::Empty);

            match inner {
                SequentialReaderInner::MyTurn(reader) => {
                    self.next.send(reader).ok();
                }
                SequentialReaderInner::Waiting(recv) => {
                    let reader = recv.recv().unwrap();
                    self.next.send(reader).ok();
                }
                SequentialReaderInner::Empty => (),
            }
        }
    }

    impl<W> Drop for SequentialWriter<W>
    where
        W: Write + Send,
    {
        fn drop(&mut self) {
            self.on_finish.send(()).ok();
        }
    }
}
