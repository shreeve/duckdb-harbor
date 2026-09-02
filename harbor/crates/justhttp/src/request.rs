//! One request: parsed line and headers, a body readable exactly once
//! under byte and time budgets, and `respond()` — the single door a
//! response leaves through. The drop path answers 500 and drains the
//! unread body, bounded, so an abandoned request cannot strand the
//! connection or the thread.

use std::io::Error as IoError;
use std::io::{self, Cursor, ErrorKind, Read, Write};

use std::net::SocketAddr;
use std::str::FromStr;
use std::time::{Duration, Instant};

use crate::Response;
use crate::http::{Header, HttpVersion, Method, StatusCode};
use crate::stream::ShutdownHandle;
use budgeted_reader::BudgetedReader;
use chunked_transfer::Decoder;
use equal_reader::EqualReader;
use fused_reader::FusedReader;

/// How long a request body may take to arrive, start to finish.
///
/// `take(MAX_BODY)` bounds how many BYTES a handler will read; nothing bounded
/// how LONG it would wait for them. A client dribbling a byte every few
/// seconds stayed under the per-read socket timeout forever, so the read never
/// failed and never finished — and it holds the thread that is serving the
/// request, which on harbor is one of a handful of workers. Eight such
/// connections took every worker and the berth answered nothing at all,
/// `/ready` included. (The drop-drain had the same shape and is bounded
/// separately; this is the other half — the body a handler actually asked
/// for.)
///
/// Thirty seconds is chosen against what this body IS: one SQL statement and
/// its parameters, where a megabyte is already pathological. Even a maximal
/// 8 MiB body needs only 273 KB/s to make it, and real ones are kilobytes.
/// A minimum-throughput floor would be the stricter instrument — it never
/// punishes a slow-but-honest uploader — but it is more machinery than a
/// statement endpoint can justify.
const BODY_TIMEOUT: Duration = Duration::from_secs(30);

/// Represents an HTTP request made by a client.
///
/// A `Request` object is what is produced by the server, and is what
/// your code must analyse and answer.
///
/// This object implements the `Send` trait, therefore you can dispatch your requests to
/// worker threads.
///
/// # Pipelining
///
/// If a client sends multiple requests in a row (without waiting for the response), then you will
/// get multiple `Request` objects simultaneously. This is called *requests pipelining*.
/// justhttp automatically reorders the responses so that you don't need to worry about the
/// order in which you call `respond`.
///
/// This mechanic is disabled if:
///
///  - The body of a request is large enough (handling pipelining requires storing the
///    body of the request in a buffer ; if the body is too big, justhttp will avoid doing that)
///  - A request sends a `Expect: 100-continue` header (which means that the client waits to
///    know whether its body will be processed before sending it)
///  - A request sends a `Connection: close` header, which indicates that this is the last
///    request that will be received on this connection
///
/// # Automatic cleanup
///
/// If a `Request` object is destroyed without `respond` being called, an empty response
/// with a 500 status code (internal server error) will automatically be
/// sent back to the client.
/// This means that if your code fails during the handling of a request, this "internal server
/// error" response will automatically be sent during the stack unwinding.
pub struct Request {
    // where to read the body from
    data_reader: Option<Box<dyn Read + Send + 'static>>,

    // if this writer is empty, then the request has been answered
    response_writer: Option<Box<dyn Write + Send + 'static>>,

    remote_addr: Option<SocketAddr>,

    method: Method,

    path: String,

    http_version: HttpVersion,

    headers: Vec<Header>,

    body_length: Option<usize>,

    // true if a `100 Continue` response must be sent when `as_reader()` is called
    must_send_continue: bool,
}

/// Error that can happen when building a `Request` object.
#[derive(Debug)]
pub enum RequestCreationError {
    /// The client sent an `Expect` header that was not recognized.
    ExpectationFailed,

    /// Error while reading data from the socket during the creation of the `Request`.
    CreationIoError(IoError),
}

impl From<IoError> for RequestCreationError {
    fn from(err: IoError) -> RequestCreationError {
        RequestCreationError::CreationIoError(err)
    }
}

/// Builds a new request.
///
/// After the request line and headers have been read from the socket, a new `Request` object
/// is built.
///
/// You must pass a `Read` that will allow the `Request` object to read from the incoming data.
/// It is the responsibility of the `Request` to read only the data of the request and not further.
///
/// The `Write` object will be used by the `Request` to write the response.
#[allow(clippy::too_many_arguments)]
pub fn new_request<R, W>(
    method: Method,
    path: String,
    version: HttpVersion,
    headers: Vec<Header>,
    remote_addr: Option<SocketAddr>,
    mut source_data: R,
    writer: W,
    shutdown: Option<ShutdownHandle>,
) -> Result<Request, RequestCreationError>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    // finding the transfer-encoding header
    let transfer_encoding = headers
        .iter()
        .find(|h: &&Header| h.field.equiv("Transfer-Encoding"))
        .map(|h| h.value.clone());

    // finding the content-length header
    let content_length = if transfer_encoding.is_some() {
        // if transfer-encoding is specified, the Content-Length
        // header must be ignored (RFC2616 #4.4)
        None
    } else {
        headers
            .iter()
            .find(|h: &&Header| h.field.equiv("Content-Length"))
            .and_then(|h| FromStr::from_str(h.value.as_str()).ok())
    };

    // true if the client sent a `Expect: 100-continue` header
    let expects_continue = {
        match headers
            .iter()
            .find(|h: &&Header| h.field.equiv("Expect"))
            .map(|h| h.value.as_str())
        {
            None => false,
            Some(v) if v.eq_ignore_ascii_case("100-continue") => true,
            _ => return Err(RequestCreationError::ExpectationFailed),
        }
    };

    // we wrap `source_data` around a reading whose nature depends on the transfer-encoding and
    // content-length headers. (Upstream special-cased `Connection: upgrade` here, handing the
    // raw stream to the request; with the upgrade API gone, upgrade requests get normal body
    // framing and the connection still closes after them — see conn.rs.)
    let reader = if let Some(content_length) = content_length {
        if content_length == 0 {
            Box::new(io::empty()) as Box<dyn Read + Send + 'static>
        } else if content_length <= 1024 && !expects_continue {
            // if the content-length is small enough, we just read everything into a buffer

            let mut buffer = vec![0; content_length];
            let mut offset = 0;
            // On the same clock as every other body, and it has to be: this
            // read happens during request construction, before any handler or
            // route exists to time it out, so a client dribbling into a
            // declared 1024 bytes held this connection's thread for as long as
            // it cared to — measured at ~51 minutes a connection, 60 of them at
            // once, with no credential.
            let deadline = Instant::now() + BODY_TIMEOUT;

            while offset != content_length {
                if Instant::now() >= deadline {
                    let info = "the request body did not arrive within the body timeout";
                    let err = IoError::new(ErrorKind::TimedOut, info);
                    return Err(RequestCreationError::CreationIoError(err));
                }
                let read = source_data.read(&mut buffer[offset..])?;
                if read == 0 {
                    // the socket returned EOF, but we were before the expected content-length
                    // aborting
                    let info = "Connection has been closed before we received enough data";
                    let err = IoError::new(ErrorKind::ConnectionAborted, info);
                    return Err(RequestCreationError::CreationIoError(err));
                }

                offset += read;
            }

            Box::new(Cursor::new(buffer)) as Box<dyn Read + Send + 'static>
        } else {
            let data_reader = EqualReader::new(source_data, content_length, shutdown);
            Box::new(BudgetedReader::new(FusedReader::new(data_reader), BODY_TIMEOUT))
                as Box<dyn Read + Send + 'static>
        }
    } else if transfer_encoding.is_some() {
        // if a transfer-encoding was specified, then "chunked" is ALWAYS applied
        // over the message (RFC2616 #3.6)
        Box::new(BudgetedReader::new(FusedReader::new(Decoder::new(source_data)), BODY_TIMEOUT))
            as Box<dyn Read + Send + 'static>
    } else {
        // if we have neither a Content-Length nor a Transfer-Encoding,
        // assuming that we have no data
        // TODO: could also be multipart/byteranges
        Box::new(io::empty()) as Box<dyn Read + Send + 'static>
    };

    Ok(Request {
        data_reader: Some(reader),
        response_writer: Some(Box::new(writer) as Box<dyn Write + Send + 'static>),
        remote_addr,
        method,
        path,
        http_version: version,
        headers,
        body_length: content_length,
        must_send_continue: expects_continue,
    })
}

impl Request {
    /// Returns the method requested by the client (eg. `GET`, `POST`, etc.).
    #[inline]
    pub fn method(&self) -> &Method {
        &self.method
    }

    /// Returns the resource requested by the client.
    #[inline]
    pub fn url(&self) -> &str {
        &self.path
    }

    /// Returns a list of all headers sent by the client.
    #[inline]
    pub fn headers(&self) -> &[Header] {
        &self.headers
    }

    /// Returns the HTTP version of the request.
    #[inline]
    pub fn http_version(&self) -> &HttpVersion {
        &self.http_version
    }

    /// Answer this request as HTTP/1.1 regardless of what it claimed to be.
    ///
    /// Exactly one caller: the 505 for a version this server does not speak.
    /// A response echoes the request's version, which is right everywhere else
    /// and wrong there — replying `HTTP/2.0 505 HTTP Version Not Supported`
    /// asserts the very version the status code exists to refuse.
    pub(crate) fn answer_as_http11(&mut self) {
        self.http_version = HttpVersion(1, 1);
    }

    /// Returns the length of the body in bytes.
    ///
    /// Returns `None` if the length is unknown.
    #[inline]
    pub fn body_length(&self) -> Option<usize> {
        self.body_length
    }

    /// Returns the address of the client that sent this request.
    ///
    /// The address is always `Some` for TCP listeners, but always `None` for UNIX listeners
    /// (as the remote address of a UNIX client is almost always unnamed).
    ///
    /// Note that this is gathered from the socket. If you receive the request from a proxy,
    /// this function will return the address of the proxy and not the address of the actual
    /// user.
    #[inline]
    pub fn remote_addr(&self) -> Option<&SocketAddr> {
        self.remote_addr.as_ref()
    }

    /// Allows to read the body of the request.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::io::Read;
    /// # let server = justhttp::Server::http("0.0.0.0:0").unwrap();
    /// let mut request = server.recv().unwrap();
    ///
    /// let mut content = String::new();
    /// request.as_reader().read_to_string(&mut content).unwrap();
    /// ```
    ///
    /// If the client sent a `Expect: 100-continue` header with the request, calling this
    ///  function will send back a `100 Continue` response.
    #[inline]
    pub fn as_reader(&mut self) -> &mut dyn Read {
        if self.must_send_continue {
            let msg = Response::empty(StatusCode(100));
            msg.raw_print(
                self.response_writer.as_mut().unwrap().by_ref(),
                self.http_version,
                &self.headers,
                true,
            )
            .ok();
            self.response_writer.as_mut().unwrap().flush().ok();
            self.must_send_continue = false;
        }

        self.data_reader.as_mut().unwrap()
    }

    /// Extract the response `Writer` object from the Request. Dropping the
    /// `Writer` unblocks the next pipelined response on the connection.
    ///
    /// This may only be called once on a single request.
    fn extract_writer_impl(&mut self) -> Box<dyn Write + Send + 'static> {
        assert!(self.response_writer.is_some());
        self.response_writer.take().unwrap()
    }

    /// Sends a response to this request.
    #[inline]
    pub fn respond<R>(mut self, response: Response<R>) -> Result<(), IoError>
    where
        R: Read,
    {
        self.respond_impl(response)
    }

    fn respond_impl<R>(&mut self, response: Response<R>) -> Result<(), IoError>
    where
        R: Read,
    {
        let mut writer = self.extract_writer_impl();

        let do_not_send_body = self.method == Method::Head;

        Self::ignore_client_closing_errors(response.raw_print(
            writer.by_ref(),
            self.http_version,
            &self.headers,
            do_not_send_body,
        ))?;

        Self::ignore_client_closing_errors(writer.flush())
    }

    fn ignore_client_closing_errors(result: io::Result<()>) -> io::Result<()> {
        result.or_else(|err| match err.kind() {
            ErrorKind::BrokenPipe
            | ErrorKind::ConnectionAborted
            | ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionReset => Ok(()),
            _ => Err(err),
        })
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        if self.response_writer.is_some() {
            let response = Response::empty(500);
            let _ = self.respond_impl(response); // ignoring any potential error
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Request;

    #[test]
    fn must_be_send() {
        #![allow(dead_code)]
        fn f<T: Send>(_: &T) {}
        fn bar(rq: &Request) {
            f(rq);
        }
    }
}

mod budgeted_reader {
    use std::io::{Error as IoError, ErrorKind, Read, Result as IoResult};
    use std::time::{Duration, Instant};

    /// Caps how long a body may take to arrive, however slowly it trickles.
    ///
    /// The clock starts on the FIRST read, not when the reader is built, and
    /// that is the part worth being deliberate about: a request can sit in the
    /// queue waiting for a free worker, and a budget started at parse time
    /// would be half spent before anyone tried to read the body — so a busy
    /// berth would begin rejecting uploads that had merely queued. Starting
    /// lazily also gets `Expect: 100-continue` right, where the body does not
    /// begin until the handler asks for it.
    ///
    /// Checked before each read rather than interrupting one in progress: the
    /// socket carries its own per-read timeout, so a single read cannot block
    /// past it, and the two together bound the total.
    pub struct BudgetedReader<R> {
        reader: R,
        budget: Duration,
        deadline: Option<Instant>,
    }

    impl<R: Read> BudgetedReader<R> {
        pub fn new(reader: R, budget: Duration) -> Self {
            Self { reader, budget, deadline: None }
        }
    }

    impl<R: Read> Read for BudgetedReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
            let deadline = *self.deadline.get_or_insert_with(|| Instant::now() + self.budget);
            if Instant::now() >= deadline {
                return Err(IoError::new(
                    ErrorKind::TimedOut,
                    "the request body did not arrive within the body timeout",
                ));
            }
            self.reader.read(buf)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::BudgetedReader;
        use std::io::{ErrorKind, Read};
        use std::time::Duration;

        /// A reader that always has one more byte, forever — a client that
        /// keeps the connection technically alive and never finishes.
        struct Endless;
        impl Read for Endless {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                std::thread::sleep(Duration::from_millis(10));
                buf[0] = b'x';
                Ok(1)
            }
        }

        #[test]
        fn a_body_that_never_ends_is_cut_off() {
            let mut r = BudgetedReader::new(Endless, Duration::from_millis(200));
            let mut sink = Vec::new();
            let err = r.read_to_end(&mut sink).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::TimedOut);
        }

        #[test]
        fn a_body_that_arrives_is_not_disturbed() {
            let mut r = BudgetedReader::new(&b"hello"[..], Duration::from_secs(30));
            let mut s = String::new();
            r.read_to_string(&mut s).unwrap();
            assert_eq!(s, "hello");
        }

        /// The budget must not start until someone actually reads: a request
        /// that queued behind a busy worker has not spent any of it yet.
        #[test]
        fn the_clock_starts_on_the_first_read() {
            let mut r = BudgetedReader::new(&b"hi"[..], Duration::from_millis(300));
            std::thread::sleep(Duration::from_millis(500)); // queued, unread
            let mut s = String::new();
            r.read_to_string(&mut s).unwrap();
            assert_eq!(s, "hi");
        }
    }
}

mod equal_reader {
    use std::io::Read;
    use std::io::Result as IoResult;
    use std::time::{Duration, Instant};

    use crate::stream::ShutdownHandle;

    /// How long the drop-drain may spend discarding a body nobody asked for.
    ///
    /// The buffer was already bounded; the *loop* was not. It follows the
    /// client's declared Content-Length to completion, and the per-read socket
    /// timeout only fires on a peer that has stopped entirely — so a client
    /// dribbling one byte every few seconds kept every read succeeding and the
    /// drain running forever. That drain runs on the thread that handled the
    /// request (the `Request` is dropped when the handler returns), and it runs
    /// *after* the response, so no credential is needed to start one: six of
    /// them took every harbor worker and the berth answered nothing at all,
    /// `/ready` included. Measured: 8 connections at one byte per 3s, and
    /// /ready went from 0.01s to a hard timeout until the drip stopped.
    ///
    /// Two seconds is far more than a body already in flight needs and far
    /// less than a drip can exploit.
    const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

    /// A `Reader` that reads exactly the number of bytes from a sub-reader.
    ///
    /// If the limit is reached, it returns EOF. If the limit is not reached
    /// when the destructor is called, the remaining bytes will be read and
    /// thrown away.
    pub struct EqualReader<R>
    where
        R: Read,
    {
        reader: R,
        size: usize,
        /// How to end the connection when the drain below gives up. See the
        /// note there: an abandoned drain leaves the stream at an unknown
        /// offset, and that is a smuggling primitive, not an untidiness.
        shutdown: Option<ShutdownHandle>,
    }

    impl<R> EqualReader<R>
    where
        R: Read,
    {
        pub fn new(reader: R, size: usize, shutdown: Option<ShutdownHandle>) -> EqualReader<R> {
            EqualReader { reader, size, shutdown }
        }
    }

    impl<R> Read for EqualReader<R>
    where
        R: Read,
    {
        fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
            if self.size == 0 {
                return Ok(0);
            }

            let buf = if buf.len() < self.size {
                buf
            } else {
                &mut buf[..self.size]
            };

            match self.reader.read(buf) {
                Ok(len) => {
                    self.size -= len;
                    Ok(len)
                }
                err @ Err(_) => err,
            }
        }
    }

    impl<R> Drop for EqualReader<R>
    where
        R: Read,
    {
        fn drop(&mut self) {
            // THE BOUNDED DRAIN (one of the hardening behaviors this crate carries,
            // with a regression test in tests/drain.rs): a fixed 64 KiB buffer instead
            // of `vec![0; remaining_to_read]`. The
            // remaining size is the client's *declared* Content-Length minus what
            // was read — attacker-chosen and unbounded — so the upstream code let
            // an unauthenticated request declaring 1 GB and sending 9 bytes cost
            // this process a 1 GB zeroed allocation per connection at drop time,
            // no matter what the server responded. Measured live before the
            // patch: 6 such requests drove RSS from 22 MB to 2.2 GB.
            //
            // AND BOUNDED IN TIME, which the buffer alone was not: the loop
            // followed the declared length to the end, so a client dribbling a
            // byte at a time kept it running indefinitely on the handler's own
            // thread. `DRAIN_TIMEOUT` is the ceiling on how long a body nobody
            // asked for may hold that thread.
            let mut remaining_to_read = self.size;
            let mut buf = [0u8; 65536];
            let deadline = Instant::now() + DRAIN_TIMEOUT;

            while remaining_to_read > 0 {
                if Instant::now() >= deadline {
                    // Out of patience with a body still arriving. The stream is
                    // now at an offset neither side agrees on, and the bytes
                    // still to come would be read as the next request line on
                    // this connection — a request the client never sent and the
                    // server would answer. Ending the connection is the only
                    // safe close: shutting the read side down turns every later
                    // read into EOF, so `ClientConnection::next` stops rather
                    // than parsing whatever arrives next.
                    if let Some(shutdown) = &self.shutdown {
                        shutdown.shutdown_read();
                    }
                    break;
                }
                let want = remaining_to_read.min(buf.len());

                match self.reader.read(&mut buf[..want]) {
                    // an error or EOF ends the drain — a half-closed socket
                    // must not spin here
                    Err(_) | Ok(0) => break,
                    Ok(other) => {
                        remaining_to_read -= other;
                    }
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::EqualReader;
        use std::io::Read;

        #[test]
        fn test_limit() {
            use std::io::Cursor;

            let mut org_reader = Cursor::new("hello world".to_string().into_bytes());

            {
                let mut equal_reader = EqualReader::new(org_reader.by_ref(), 5, None);

                let mut string = String::new();
                equal_reader.read_to_string(&mut string).unwrap();
                assert_eq!(string, "hello");
            }

            let mut string = String::new();
            org_reader.read_to_string(&mut string).unwrap();
            assert_eq!(string, " world");
        }

        #[test]
        fn test_not_enough() {
            use std::io::Cursor;

            let mut org_reader = Cursor::new("hello world".to_string().into_bytes());

            {
                let mut equal_reader = EqualReader::new(org_reader.by_ref(), 5, None);

                let mut vec = [0];
                equal_reader.read_exact(&mut vec).unwrap();
                assert_eq!(vec[0], b'h');
            }

            let mut string = String::new();
            org_reader.read_to_string(&mut string).unwrap();
            assert_eq!(string, " world");
        }
    }
}

mod fused_reader {
    use std::io::{IoSliceMut, Read, Result as IoResult};

    /// Wraps another reader and provides "fused" behavior.
    /// When the underlying reader reaches EOF, it is dropped
    /// and the fused reader becomes an empty stub.
    pub struct FusedReader<R: Read> {
        inner: Option<R>,
    }

    impl<R: Read> FusedReader<R> {
        pub fn new(inner: R) -> Self {
            Self { inner: Some(inner) }
        }
    }

    impl<R: Read> Read for FusedReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
            match &mut self.inner {
                Some(r) => {
                    let l = r.read(buf)?;
                    if l == 0 {
                        self.inner = None;
                    }
                    Ok(l)
                }
                None => Ok(0),
            }
        }

        fn read_vectored(&mut self, bufs: &mut [IoSliceMut<'_>]) -> IoResult<usize> {
            match &mut self.inner {
                Some(r) => {
                    let l = r.read_vectored(bufs)?;
                    if l == 0 {
                        self.inner = None;
                    }
                    Ok(l)
                }
                None => Ok(0),
            }
        }
    }
}
