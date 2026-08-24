use std::io::Error as IoError;
use std::io::Result as IoResult;
use std::io::{BufReader, BufWriter, ErrorKind, Read};

use std::net::SocketAddr;
use std::str::FromStr;

use crate::Request;
use crate::http::{HttpVersion, Method};
use crate::stream::RefinedTcpStream;
use sequential::{SequentialReader, SequentialReaderBuilder, SequentialWriterBuilder};

/// A ClientConnection is an object that will store a socket to a client
/// and return Request objects.
pub struct ClientConnection {
    // address of the client
    remote_addr: IoResult<Option<SocketAddr>>,

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
}

/// Error that can happen when reading a request.
#[derive(Debug)]
enum ReadError {
    WrongRequestLine,
    WrongHeader(HttpVersion),
    /// the client sent an unrecognized `Expect` header
    ExpectationFailed(HttpVersion),
    ReadIoError(IoError),
}

impl ClientConnection {
    /// Creates a new `ClientConnection` that takes ownership of the `TcpStream`.
    pub fn new(
        write_socket: RefinedTcpStream,
        mut read_socket: RefinedTcpStream,
    ) -> ClientConnection {
        let remote_addr = read_socket.peer_addr();

        let mut source = SequentialReaderBuilder::new(BufReader::with_capacity(1024, read_socket));
        let first_header = source.next().unwrap();

        ClientConnection {
            source,
            // 8192 matches the chunked encoder's internal buffer, so headers +
            // a full chunk + the terminator coalesce into a single write().
            sink: SequentialWriterBuilder::new(BufWriter::with_capacity(8192, write_socket)),
            remote_addr,
            next_header_source: first_header,
            no_more_requests: false,
        }
    }

    /// Reads the next line from self.next_header_source.
    ///
    /// Reads until `CRLF` is reached. The next read will start
    ///  at the first byte of the new line.
    // byte-at-a-time on purpose: the source is already a BufReader, and this
    // exact framing (CRLF only, Interrupted retried by Bytes) is semantics.
    #[allow(clippy::unbuffered_bytes)]
    fn read_next_line<'b>(&mut self, buf: &'b mut Vec<u8>) -> IoResult<&'b str> {
        buf.clear();
        let mut prev_byte_was_cr = false;

        loop {
            let byte = self.next_header_source.by_ref().bytes().next();

            let byte = match byte {
                Some(b) => b?,
                None => return Err(IoError::new(ErrorKind::ConnectionAborted, "Unexpected EOF")),
            };

            if byte == b'\n' && prev_byte_was_cr {
                buf.pop(); // removing the '\r'
                if !buf.is_ascii() {
                    return Err(IoError::new(
                        ErrorKind::InvalidInput,
                        "Header is not in ASCII",
                    ));
                }
                // ASCII was just verified, and ASCII is valid UTF-8.
                return Ok(std::str::from_utf8(buf).unwrap());
            }

            prev_byte_was_cr = byte == b'\r';

            buf.push(byte);
        }
    }

    /// Reads a request from the stream.
    /// Blocks until the header has been read.
    fn read(&mut self) -> Result<Request, ReadError> {
        let (method, path, version, headers) = {
            // one line buffer reused for the request line and every header line
            let mut line_buf = Vec::with_capacity(128);

            // reading the request line
            let (method, path, version) = {
                let line = self
                    .read_next_line(&mut line_buf)
                    .map_err(ReadError::ReadIoError)?;

                parse_request_line(line.trim())?
            };

            // getting all headers
            let headers = {
                let mut headers = Vec::with_capacity(16);
                loop {
                    let line = self
                        .read_next_line(&mut line_buf)
                        .map_err(ReadError::ReadIoError)?;

                    if line.is_empty() {
                        break;
                    };
                    headers.push(match FromStr::from_str(line.trim()) {
                        Ok(h) => h,
                        _ => return Err(ReadError::WrongHeader(version)),
                    });
                }

                headers
            };

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
            data_source,
            writer,
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

        loop {
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
                let writer = self.sink.next().unwrap();
                let response = Response::from_string(
                    "This server only supports HTTP versions 1.0 and 1.1".to_owned(),
                )
                .with_status_code(StatusCode(505));
                response
                    .raw_print(writer, HttpVersion(1, 1), &[], false)
                    .ok();
                continue;
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
                Some(val)
                    if !contains_ignore_case(val, "keep-alive")
                        && *rq.http_version() == HttpVersion(1, 0) =>
                {
                    self.no_more_requests = true
                }
                None if *rq.http_version() == HttpVersion(1, 0) => self.no_more_requests = true,
                _ => (),
            };

            // returning the request
            return Some(rq);
        }
    }
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
