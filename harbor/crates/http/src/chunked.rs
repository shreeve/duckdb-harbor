//! The HTTP/1.1 chunked body decoder.

use std::io::{self, BufRead, Read};

/// Decodes an HTTP/1.1 chunked body from the inner reader.
///
/// Resumable by design: a streaming socket ticks every 250 ms with
/// WouldBlock, and a tick can land mid-frame — mid-size-line, mid-payload,
/// mid-CRLF. Every partial (the accumulating `line`, the `remaining` count,
/// the state itself) lives on self, so an interrupted read picks up exactly
/// where it stopped instead of corrupting the framing.
pub(crate) struct ChunkedReader<R: BufRead> {
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
    pub(crate) fn new(inner: R) -> Self {
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
    use std::io::{BufReader, Cursor};

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
