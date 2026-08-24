//! Minimal HTTP/1.1 client over Unix or TCP streams.
//!
//! One request per connection (`Connection: close`), blocking reads, chunked
//! and Content-Length bodies. This is deliberately the whole client: harbor
//! speaks plain HTTP/1.1 via justhttp, and pilot's traffic is one request at
//! a time, so an async stack would be pure weight.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone)]
pub enum Transport {
    Unix(PathBuf),
    Tcp(String), // host:port
}

pub struct Response {
    pub status: u16,
    pub body: Box<dyn BufRead>,
}

impl Response {
    pub fn body_string(mut self) -> io::Result<String> {
        let mut s = String::new();
        self.body.read_to_string(&mut s)?;
        Ok(s)
    }
}

trait Stream: Read + Write {}
impl<T: Read + Write> Stream for T {}

pub fn request(
    transport: &Transport,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
    timeout: Option<Duration>,
) -> io::Result<Response> {
    request_inner(transport, method, path, token, body, timeout, None)
}

/// Like `request`, but built for long-running /sql streams: the socket gets a
/// short read timeout so the caller's read loop ticks (and can notice a
/// Ctrl-C), while status and header reads here retry through those ticks.
pub fn request_streaming(
    transport: &Transport,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
    on_tick: &dyn Fn(),
) -> io::Result<Response> {
    request_inner(transport, method, path, token, body, Some(Duration::from_millis(250)), Some(on_tick))
}

fn request_inner(
    transport: &Transport,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
    timeout: Option<Duration>,
    on_tick: Option<&dyn Fn()>,
) -> io::Result<Response> {
    let (stream, host): (Box<dyn Stream>, String) = match transport {
        Transport::Unix(p) => {
            let s = UnixStream::connect(p)?;
            s.set_read_timeout(timeout)?;
            (Box::new(s), "harbor".to_string())
        }
        Transport::Tcp(addr) => {
            let s = TcpStream::connect(addr)?;
            s.set_read_timeout(timeout)?;
            (Box::new(s), addr.clone())
        }
    };
    let mut stream = stream;

    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    req.push_str("Accept: application/x-ndjson\r\n");
    if let Some(t) = token {
        req.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    if let Some(b) = body {
        req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            b.len()
        ));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes())?;
    if let Some(b) = body {
        stream.write_all(b.as_bytes())?;
    }
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    // Headers may not arrive until the statement completes (the server
    // responds once execution starts producing), so the wait happens HERE —
    // which is why the tick callback fires here: it is how a Ctrl-C reaches
    // a query that has not sent a byte yet.
    let read_line = |reader: &mut BufReader<Box<dyn Stream>>, line: &mut String| -> io::Result<usize> {
        loop {
            match reader.read_line(line) {
                Err(e) if on_tick.is_some() && matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                    if let Some(f) = on_tick {
                        f();
                    }
                    continue;
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
    loop {
        let mut line = String::new();
        read_line(&mut reader, &mut line)?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
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

    let body: Box<dyn BufRead> = if chunked {
        Box::new(BufReader::new(ChunkedReader::new(reader)))
    } else if let Some(n) = content_length {
        Box::new(BufReader::new(reader.take(n)))
    } else {
        Box::new(reader) // read to EOF (Connection: close)
    };
    Ok(Response { status, body })
}

/// Decodes an HTTP/1.1 chunked body from the inner reader.
///
/// Resumable by design: request_streaming sockets tick every 250ms with
/// WouldBlock, and a tick can land mid-frame — mid-size-line, mid-payload,
/// mid-CRLF. Every partial (the accumulating `line`, the `remaining` count,
/// the state itself) lives on self, so an interrupted read picks up exactly
/// where it stopped instead of corrupting the framing.
struct ChunkedReader<R: BufRead> {
    inner: R,
    state: ChunkState,
    line: Vec<u8>,  // partial size/CRLF/trailer line, kept across WouldBlock
    remaining: u64, // payload bytes left in the current chunk
}

#[derive(PartialEq)]
enum ChunkState {
    Size,    // reading "1a3\r\n"
    Data,    // reading `remaining` payload bytes
    DataEnd, // reading the CRLF after the payload
    Trailers, // after the 0-chunk: lines until a blank one
    Done,
}

impl<R: BufRead> ChunkedReader<R> {
    fn new(inner: R) -> Self {
        Self { inner, state: ChunkState::Size, line: Vec::new(), remaining: 0 }
    }

    /// Append to self.line until `\n` (kept) or EOF. WouldBlock propagates
    /// with the partial line intact. Returns whether a full line arrived.
    fn fill_line(&mut self) -> io::Result<bool> {
        loop {
            let avail = self.inner.fill_buf()?;
            if avail.is_empty() {
                return Ok(false); // EOF
            }
            if let Some(pos) = avail.iter().position(|&c| c == b'\n') {
                self.line.extend_from_slice(&avail[..=pos]);
                self.inner.consume(pos + 1);
                return Ok(true);
            }
            let n = avail.len();
            self.line.extend_from_slice(avail);
            self.inner.consume(n);
        }
    }
}

impl<R: BufRead> Read for ChunkedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.state {
                ChunkState::Done => return Ok(0),
                ChunkState::Size => {
                    let eol = self.fill_line()?;
                    if !eol && self.line.is_empty() {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "connection closed between chunks",
                        ));
                    }
                    let text = String::from_utf8_lossy(&self.line).into_owned();
                    self.line.clear();
                    let size_part = text.trim().split(';').next().unwrap_or("").trim();
                    let size = u64::from_str_radix(size_part, 16).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, format!("bad chunk size: {text:?}"))
                    })?;
                    if size == 0 {
                        self.state = ChunkState::Trailers;
                    } else {
                        self.remaining = size;
                        self.state = ChunkState::Data;
                    }
                }
                ChunkState::Data => {
                    let want = buf.len().min(self.remaining as usize);
                    let n = self.inner.read(&mut buf[..want])?;
                    if n == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "connection closed mid-chunk",
                        ));
                    }
                    self.remaining -= n as u64;
                    if self.remaining == 0 {
                        self.state = ChunkState::DataEnd;
                    }
                    return Ok(n);
                }
                ChunkState::DataEnd => {
                    self.fill_line()?; // the CRLF after the payload
                    self.line.clear();
                    self.state = ChunkState::Size;
                }
                ChunkState::Trailers => {
                    let eol = self.fill_line()?;
                    let blank = self.line.iter().all(|&c| c == b'\r' || c == b'\n');
                    self.line.clear();
                    if blank || !eol {
                        self.state = ChunkState::Done;
                        return Ok(0);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn chunked_reassembles_across_chunk_boundaries() {
        // Two NDJSON lines split mid-line across three chunks, then terminator.
        // sizes: 0xb = {"type":"ro ; 0x11 = w","values":[1]}\n ; 0x13 = {"type":"end"}\nxxxx
        let wire = "b\r\n{\"type\":\"ro\r\n11\r\nw\",\"values\":[1]}\n\r\n13\r\n{\"type\":\"end\"}\nxxxx\r\n0\r\n\r\n";
        let mut r = BufReader::new(ChunkedReader::new(Cursor::new(wire.as_bytes())));
        let mut lines = Vec::new();
        loop {
            let mut l = String::new();
            if r.read_line(&mut l).unwrap() == 0 {
                break;
            }
            lines.push(l.trim_end().to_string());
        }
        assert_eq!(lines[0], r#"{"type":"row","values":[1]}"#);
        assert_eq!(lines[1], r#"{"type":"end"}"#);
        assert_eq!(lines[2], "xxxx");
        assert_eq!(lines.len(), 3);
    }

    /// One byte per read, a WouldBlock before every one of them — the worst
    /// case of the 250ms streaming tick landing mid-size-line, mid-payload,
    /// and mid-trailer. The decoder must resume, never desync.
    struct Drip<'a> {
        data: &'a [u8],
        pos: usize,
        ready: bool,
    }

    impl Read for Drip<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !std::mem::replace(&mut self.ready, true) {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "tick"));
            }
            self.ready = false;
            if self.pos >= self.data.len() {
                return Ok(0);
            }
            buf[0] = self.data[self.pos];
            self.pos += 1;
            Ok(1)
        }
    }

    #[test]
    fn chunked_survives_wouldblock_at_every_byte() {
        let wire = "b\r\n{\"type\":\"ro\r\n11\r\nw\",\"values\":[1]}\n\r\n13\r\n{\"type\":\"end\"}\nxxxx\r\n0\r\nx-trailer: 1\r\n\r\n";
        let drip = Drip { data: wire.as_bytes(), pos: 0, ready: false };
        let mut r = ChunkedReader::new(BufReader::new(drip));
        let mut out = Vec::new();
        let mut buf = [0u8; 7];
        loop {
            match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue, // the tick
                Err(e) => panic!("decoder desynced: {e}"),
            }
        }
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"type\":\"row\",\"values\":[1]}\n{\"type\":\"end\"}\nxxxx"
        );
    }

    #[test]
    fn eof_mid_chunk_is_an_error_not_silence() {
        // The server dying mid-payload must not read as a clean end.
        let wire = "b\r\n{\"type";
        let mut r = ChunkedReader::new(BufReader::new(Cursor::new(wire.as_bytes())));
        let mut out = Vec::new();
        let err = r.read_to_end(&mut out).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
