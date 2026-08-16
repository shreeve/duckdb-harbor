//! Minimal HTTP/1.1 client over Unix or TCP streams.
//!
//! One request per connection (`Connection: close`), blocking reads, chunked
//! and Content-Length bodies. This is deliberately the whole client: harbor
//! speaks plain HTTP/1.1 via tiny_http, and pilot's traffic is one request at
//! a time, so an async stack would be pure weight.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    let status = {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        line.split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("bad status line: {line:?}")))?
    };

    let mut chunked = false;
    let mut content_length: Option<u64> = None;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
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
struct ChunkedReader<R: BufRead> {
    inner: R,
    remaining: u64, // bytes left in the current chunk
    done: bool,
}

impl<R: BufRead> ChunkedReader<R> {
    fn new(inner: R) -> Self {
        Self { inner, remaining: 0, done: false }
    }

    fn next_chunk(&mut self) -> io::Result<()> {
        let mut line = String::new();
        self.inner.read_line(&mut line)?;
        let size_part = line.trim().split(';').next().unwrap_or("").trim();
        let size = u64::from_str_radix(size_part, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("bad chunk size: {line:?}")))?;
        if size == 0 {
            // consume trailing CRLF (and any trailers) until blank line
            loop {
                let mut t = String::new();
                if self.inner.read_line(&mut t)? == 0 || t.trim_end().is_empty() {
                    break;
                }
            }
            self.done = true;
        }
        self.remaining = size;
        Ok(())
    }

    fn finish_chunk(&mut self) -> io::Result<()> {
        let mut crlf = [0u8; 2];
        self.inner.read_exact(&mut crlf)
    }
}

impl<R: BufRead> Read for ChunkedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done {
            return Ok(0);
        }
        if self.remaining == 0 {
            self.next_chunk()?;
            if self.done {
                return Ok(0);
            }
        }
        let want = buf.len().min(self.remaining as usize);
        let n = self.inner.read(&mut buf[..want])?;
        self.remaining -= n as u64;
        if self.remaining == 0 {
            self.finish_chunk()?;
        }
        Ok(n)
    }
}

/// The harbor registry directory: $HARBOR_HOME, else ~/.harbor.
pub fn harbor_home() -> PathBuf {
    if let Ok(h) = std::env::var("HARBOR_HOME") {
        return PathBuf::from(h);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home).join(".harbor")
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
}
