//! The justhttp test suite, one property per module:
//!
//! - `basic`     — smoke: one request, one response
//! - `input`     — request parsing, bodies, malformed input
//! - `head`      — request-head bounds and unambiguous framing (hardening)
//! - `network`   — connection lifecycle, pipelining, slow clients
//! - `keepalive` — connection reuse + chunked streaming from a Reader
//! - `buffering` — response backpressure against closed/idle clients
//! - `prompt`    — latency properties: responses leave when they should
//! - `unblock`   — Server::unblock wakes blocked recv()
//! - `unix`      — unix-domain sockets
//! - `first_request` — the first-request idle clock, and that keep-alive is
//!   exempt from it (`#[ignore]`, ~60s each)
//! - `stall`     — the 10s write-timeout backstop (`#[ignore]`, ~35s by
//!   design: `cargo test --test suite -- --ignored`)
//!
//! `tests/drain.rs` stays a separate binary on purpose: it measures
//! allocations with a global allocator, and any other test running in the
//! same process would pollute the measurement.

mod support {

    use std::net::TcpStream;
    use std::thread;
    use std::time::Duration;

    /// Creates a server and a client connected to the server.
    pub fn new_one_server_one_client() -> (justhttp::Server, TcpStream) {
        let server = justhttp::Server::http("0.0.0.0:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        (server, client)
    }

    /// Creates a "hello world" server with a client connected to the server.
    ///
    /// The server will automatically close after 3 seconds.
    pub fn new_client_to_hello_world_server() -> TcpStream {
        let server = justhttp::Server::http("0.0.0.0:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let client = TcpStream::connect(("127.0.0.1", port)).unwrap();

        thread::spawn(move || {
            let mut cycles = 3 * 1000 / 20;

            loop {
                if let Some(rq) = server.try_recv().unwrap() {
                    let response = justhttp::Response::from_string("hello world".to_string());
                    rq.respond(response).unwrap();
                }

                thread::sleep(Duration::from_millis(20));

                cycles -= 1;
                if cycles == 0 {
                    break;
                }
            }
        });

        client
    }

    /// Reads one full HTTP response from the stream: returns (headers, body).
    /// Handles both Content-Length and chunked framing.
    pub fn read_response<R: std::io::Read>(stream: &mut R) -> (String, String) {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        // read headers to CRLFCRLF
        while !buf.ends_with(b"\r\n\r\n") {
            assert_eq!(stream.read(&mut byte).unwrap(), 1, "EOF inside headers");
            buf.push(byte[0]);
        }
        let headers = String::from_utf8(buf).unwrap();
        let lower = headers.to_ascii_lowercase();
        let body = if lower.contains("transfer-encoding: chunked") {
            let mut body = Vec::new();
            loop {
                // chunk size line
                let mut line = Vec::new();
                while !line.ends_with(b"\r\n") {
                    assert_eq!(stream.read(&mut byte).unwrap(), 1);
                    line.push(byte[0]);
                }
                let size_str = String::from_utf8(line).unwrap();
                let size = usize::from_str_radix(size_str.trim(), 16).unwrap();
                let mut chunk = vec![0u8; size + 2]; // chunk + CRLF
                std::io::Read::read_exact(stream, &mut chunk).unwrap();
                if size == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..size]);
            }
            body
        } else if let Some(cl) = lower
            .lines()
            .find(|l| l.starts_with("content-length:"))
            .and_then(|l| l.split(':').nth(1))
            .and_then(|v| v.trim().parse::<usize>().ok())
        {
            let mut body = vec![0u8; cl];
            std::io::Read::read_exact(stream, &mut body).unwrap();
            body
        } else {
            Vec::new()
        };
        (headers, String::from_utf8(body).unwrap())
    }
}

mod basic {
    use super::support;

    use std::io::{Read, Write};

    #[test]
    fn basic_handling() {
        let (server, mut stream) = support::new_one_server_one_client();
        write!(
            stream,
            "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .unwrap();

        let request = server.recv().unwrap();
        assert!(*request.method() == justhttp::Method::Get);
        assert_eq!(request.url(), "/");
        request
            .respond(justhttp::Response::from_string("hello world".to_owned()))
            .unwrap();

        server.try_recv().unwrap();

        let mut content = String::new();
        stream.read_to_string(&mut content).unwrap();
        assert!(content.ends_with("hello world"));
    }
}

mod input {
    use super::support;

    use std::io::{Read, Write};
    use std::net::Shutdown;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn basic_string_input() {
        let (server, client) = support::new_one_server_one_client();

        {
            let mut client = client;
            (write!(client, "GET / HTTP/1.1\r\nHost: localhost\r\nContent-Type: text/plain; charset=utf8\r\nContent-Length: 5\r\n\r\nhello")).unwrap();
        }

        let mut request = server.recv().unwrap();

        let mut output = String::new();
        request.as_reader().read_to_string(&mut output).unwrap();
        assert_eq!(output, "hello");
    }

    #[test]
    fn wrong_content_length() {
        let (server, client) = support::new_one_server_one_client();

        {
            let mut client = client;
            (write!(client, "GET / HTTP/1.1\r\nHost: localhost\r\nContent-Type: text/plain; charset=utf8\r\nContent-Length: 3\r\n\r\nhello")).unwrap();
        }

        let mut request = server.recv().unwrap();

        let mut output = String::new();
        request.as_reader().read_to_string(&mut output).unwrap();
        assert_eq!(output, "hel");
    }

    #[test]
    fn expect_100_continue() {
        let (server, client) = support::new_one_server_one_client();

        let mut client = client;
        (write!(client, "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nExpect: 100-continue\r\nContent-Type: text/plain; charset=utf8\r\nContent-Length: 5\r\n\r\n")).unwrap();
        client.flush().unwrap();

        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let mut request = server.recv().unwrap();
            let mut output = String::new();
            request.as_reader().read_to_string(&mut output).unwrap();
            assert_eq!(output, "hello");
            tx.send(()).unwrap();
        });

        let mut content = vec![0; 12];
        client.read_exact(&mut content).unwrap();
        assert!(&content[9..].starts_with(b"100")); // 100 status code

        (write!(client, "hello")).unwrap();
        client.flush().unwrap();
        client.shutdown(Shutdown::Write).unwrap();

        rx.recv().unwrap();
    }

    #[test]
    fn unsupported_expect_header() {
        let mut client = support::new_client_to_hello_world_server();

        (write!(client, "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nExpect: 189-dummy\r\nContent-Type: text/plain; charset=utf8\r\n\r\n")).unwrap();

        let mut content = String::new();
        client.read_to_string(&mut content).unwrap();
        assert!(&content[9..].starts_with("417")); // 417 status code
    }

    #[test]
    fn invalid_header_name() {
        let mut client = support::new_client_to_hello_world_server();

        // note the space hidden in the Content-Length, which is invalid
        (write!(client, "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: text/plain; charset=utf8\r\nContent-Length : 5\r\n\r\nhello")).unwrap();

        let mut content = String::new();
        client.read_to_string(&mut content).unwrap();
        assert!(&content[9..].starts_with("400 Bad Request")); // 400 status code
    }

    #[test]
    fn custom_content_type_response_header() {
        let (server, mut stream) = support::new_one_server_one_client();
        write!(
            stream,
            "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .unwrap();

        let request = server.recv().unwrap();
        request
            .respond(
                justhttp::Response::from_string("{\"custom\": \"Content-Type\"}").with_header(
                    "Content-Type: application/json"
                        .parse::<justhttp::Header>()
                        .unwrap(),
                ),
            )
            .unwrap();

        let mut content = String::new();
        stream.read_to_string(&mut content).unwrap();

        assert!(content.ends_with("{\"custom\": \"Content-Type\"}"));
        assert_ne!(content.find("Content-Type: application/json"), None);
    }
}

/// The request head is the one part of a request that is parsed before any
/// routing, any authentication, and any application code. Everything it costs
/// is spent on behalf of an anonymous caller, so all of it is bounded, and
/// anything it cannot frame unambiguously is refused rather than guessed at.
/// A failure in this module is a security regression, not a flake.
mod head {
    use super::support;

    use std::io::{Read, Write};

    /// A header line with no end must not be an unbounded allocation. Before
    /// `MAX_LINE` this loop had no ceiling: one socket, one never-terminated
    /// header, and RSS climbed at line speed (30 MB to 1.5 GB in under five
    /// seconds, measured, with no credential presented).
    #[test]
    fn an_endless_header_line_is_refused() {
        let (_server, mut client) = support::new_one_server_one_client();
        write!(client, "GET / HTTP/1.1\r\nHost: localhost\r\nX-Junk: ").unwrap();

        // Well past MAX_LINE, and still no CRLF in sight.
        let blob = "A".repeat(4096);
        let mut sent = 0;
        while sent < 512 * 1024 {
            if client.write_all(blob.as_bytes()).is_err() {
                break; // the server hung up on us, which is the point
            }
            sent += blob.len();
        }

        // 431, and the connection closes: the head cannot be resynchronized.
        let mut content = String::new();
        let _ = client.read_to_string(&mut content);
        assert!(
            content.starts_with("HTTP/1.1 431"),
            "expected 431, got {:?}",
            content.lines().next()
        );
    }

    /// Bounded lines are no defence if there can be any number of them.
    #[test]
    fn too_many_headers_are_refused() {
        let (_server, mut client) = support::new_one_server_one_client();
        write!(client, "GET / HTTP/1.1\r\nHost: localhost\r\n").unwrap();
        for i in 0..4096 {
            if write!(client, "X-Pad-{i}: x\r\n").is_err() {
                break;
            }
        }
        let _ = write!(client, "\r\n");

        let mut content = String::new();
        let _ = client.read_to_string(&mut content);
        assert!(
            content.starts_with("HTTP/1.1 431"),
            "expected 431, got {:?}",
            content.lines().next()
        );
    }

    /// Two Content-Lengths that disagree have two answers, and the gap between
    /// the one this server picks and the one a proxy in front of it picks is a
    /// request-smuggling desync. Refuse instead of choosing.
    #[test]
    fn conflicting_content_lengths_are_refused() {
        let (_server, mut client) = support::new_one_server_one_client();
        write!(
            client,
            "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nContent-Length: 3\r\n\r\nhello"
        )
        .unwrap();

        let mut content = String::new();
        let _ = client.read_to_string(&mut content);
        assert!(
            content.starts_with("HTTP/1.1 400"),
            "expected 400, got {:?}",
            content.lines().next()
        );
    }

    /// Repeating the same value is not ambiguous, so it is not refused.
    #[test]
    fn agreeing_content_lengths_are_served() {
        let (server, mut client) = support::new_one_server_one_client();
        write!(
            client,
            "POST / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\nhello"
        )
        .unwrap();

        let mut request = server.recv().unwrap();
        let mut body = String::new();
        request.as_reader().read_to_string(&mut body).unwrap();
        assert_eq!(body, "hello");
        request
            .respond(justhttp::Response::from_string("ok".to_owned()))
            .unwrap();
    }

    /// A body framed two ways at once is the other half of the same desync.
    #[test]
    fn content_length_with_transfer_encoding_is_refused() {
        let (_server, mut client) = support::new_one_server_one_client();
        write!(
            client,
            "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
        )
        .unwrap();

        let mut content = String::new();
        let _ = client.read_to_string(&mut content);
        assert!(
            content.starts_with("HTTP/1.1 400"),
            "expected 400, got {:?}",
            content.lines().next()
        );
    }

    /// A Content-Length that is not a number used to parse as "no body", so
    /// the bytes the client did send were read as the next request.
    #[test]
    fn unparseable_content_length_is_refused() {
        let (_server, mut client) = support::new_one_server_one_client();
        write!(
            client,
            "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: abc\r\n\r\nhello"
        )
        .unwrap();

        let mut content = String::new();
        let _ = client.read_to_string(&mut content);
        assert!(
            content.starts_with("HTTP/1.1 400"),
            "expected 400, got {:?}",
            content.lines().next()
        );
    }

    /// `TE: identity` must not be able to turn a streamed response into a
    /// buffered one. `raw_print` discovers an unknown length by reading the
    /// whole body, so honoring this header hands control of the server's
    /// memory to the caller — measured at +316 MB of RSS on one query.
    #[test]
    fn te_identity_cannot_unstream_an_unknown_length() {
        let (server, mut client) = support::new_one_server_one_client();
        write!(
            client,
            "GET / HTTP/1.1\r\nHost: localhost\r\nTE: identity\r\nConnection: close\r\n\r\n"
        )
        .unwrap();

        std::thread::spawn(move || {
            let rq = server.recv().unwrap();
            // Unknown length: exactly the shape harbor streams /sql with.
            let body = std::io::Cursor::new(b"streamed".to_vec());
            rq.respond(justhttp::Response::new(200.into(), Vec::new(), body, None))
                .unwrap();
        });

        let (headers, body) = support::read_response(&mut client);
        let lower = headers.to_ascii_lowercase();
        assert!(
            lower.contains("transfer-encoding: chunked"),
            "unknown length must stay chunked, got: {headers}"
        );
        assert!(!lower.contains("content-length:"), "body was buffered: {headers}");
        assert_eq!(body, "streamed");
    }

    /// The HTTP/1.0 half of the same property: no chunked encoding available,
    /// so an unknown length is delimited by the close rather than by reading
    /// the whole body into memory to measure it.
    #[test]
    fn http_1_0_streams_an_unknown_length_to_close() {
        let (server, mut client) = support::new_one_server_one_client();
        write!(client, "GET / HTTP/1.0\r\nHost: localhost\r\n\r\n").unwrap();

        std::thread::spawn(move || {
            let rq = server.recv().unwrap();
            let body = std::io::Cursor::new(b"streamed".to_vec());
            rq.respond(justhttp::Response::new(200.into(), Vec::new(), body, None))
                .unwrap();
        });

        let mut content = String::new();
        client.read_to_string(&mut content).unwrap();
        let lower = content.to_ascii_lowercase();
        assert!(!lower.contains("content-length:"), "body was buffered: {content}");
        assert!(!lower.contains("transfer-encoding:"), "1.0 cannot chunk: {content}");
        assert!(content.ends_with("streamed"), "body truncated: {content}");
    }
}

mod network {
    use super::support;

    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpStream};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn connection_close_header() {
        let mut client = support::new_client_to_hello_world_server();

        (write!(client, "GET / HTTP/1.1\r\nConnection: keep-alive\r\n\r\n")).unwrap();
        thread::sleep(Duration::from_secs(1));

        (write!(client, "GET / HTTP/1.1\r\nConnection: close\r\n\r\n")).unwrap();

        // if the connection was not closed, this will err with timeout
        let mut out = Vec::new();
        client.read_to_end(&mut out).unwrap();
    }

    #[test]
    fn http_1_0_connection_close() {
        let mut client = support::new_client_to_hello_world_server();

        (write!(client, "GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")).unwrap();

        // if the connection was not closed, this will err with timeout
        let mut out = Vec::new();
        client.read_to_end(&mut out).unwrap();
    }

    #[test]
    fn detect_connection_closed() {
        let mut client = support::new_client_to_hello_world_server();

        (write!(client, "GET / HTTP/1.1\r\nConnection: keep-alive\r\n\r\n")).unwrap();
        thread::sleep(Duration::from_secs(1));

        client.shutdown(Shutdown::Write).unwrap();

        // if the connection was not closed, this will err with timeout
        let mut out = Vec::new();
        client.read_to_end(&mut out).unwrap();
    }

    #[test]
    fn trickle() {
        let mut client = support::new_client_to_hello_world_server();

        (write!(client, "G")).unwrap();
        thread::sleep(Duration::from_millis(100));
        (write!(client, "ET /he")).unwrap();
        thread::sleep(Duration::from_millis(100));
        (write!(client, "llo HT")).unwrap();
        thread::sleep(Duration::from_millis(100));
        (write!(client, "TP/1.")).unwrap();
        thread::sleep(Duration::from_millis(100));
        (write!(client, "1\r\nHo")).unwrap();
        thread::sleep(Duration::from_millis(100));
        (write!(client, "st: localho")).unwrap();
        thread::sleep(Duration::from_millis(100));
        (write!(client, "st\r\nConnec")).unwrap();
        thread::sleep(Duration::from_millis(100));
        (write!(client, "tion: close\r")).unwrap();
        thread::sleep(Duration::from_millis(100));
        (write!(client, "\n\r")).unwrap();
        thread::sleep(Duration::from_millis(100));
        (writeln!(client)).unwrap();

        let mut data = String::new();
        client.read_to_string(&mut data).unwrap();
        assert!(data.ends_with("hello world"));
    }

    #[test]
    fn pipelining_test() {
        let mut client = support::new_client_to_hello_world_server();

        (write!(client, "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")).unwrap();
        (write!(client, "GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\n")).unwrap();
        (write!(
            client,
            "GET /world HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        ))
        .unwrap();

        let mut data = String::new();
        client.read_to_string(&mut data).unwrap();
        assert_eq!(data.split("hello world").count(), 4);
    }

    #[test]
    fn crash_500() {
        let server = justhttp::Server::http("0.0.0.0:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();

        thread::spawn(move || {
            server.recv().unwrap();
            // oops, server crash
        });

        (write!(
            client,
            "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        ))
        .unwrap();

        let mut content = String::new();
        client.read_to_string(&mut content).unwrap();
        assert!(&content[9..].starts_with('5')); // 5xx status code
    }

    #[test]
    fn responses_reordered() {
        let (server, mut client) = support::new_one_server_one_client();

        (write!(client, "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")).unwrap();
        (write!(
            client,
            "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        ))
        .unwrap();

        thread::spawn(move || {
            let rq1 = server.recv().unwrap();
            let rq2 = server.recv().unwrap();

            thread::spawn(move || {
                rq2.respond(justhttp::Response::from_string("second request".to_owned()))
                    .unwrap();
            });

            thread::sleep(Duration::from_millis(100));

            thread::spawn(move || {
                rq1.respond(justhttp::Response::from_string("first request".to_owned()))
                    .unwrap();
            });
        });

        let mut content = String::new();
        client.read_to_string(&mut content).unwrap();
        assert!(content.ends_with("second request"));
    }

    #[test]
    fn no_transfer_encoding_on_204() {
        let (server, mut client) = support::new_one_server_one_client();

        (write!(
            client,
            "GET / HTTP/1.1\r\nHost: localhost\r\nTE: chunked\r\nConnection: close\r\n\r\n"
        ))
        .unwrap();

        thread::spawn(move || {
            let rq = server.recv().unwrap();

            let resp = justhttp::Response::empty(justhttp::StatusCode(204));
            rq.respond(resp).unwrap();
        });

        let mut content = String::new();
        client.read_to_string(&mut content).unwrap();

        assert!(content.starts_with("HTTP/1.1 204"));
        assert!(!content.contains("Transfer-Encoding: chunked"));
    }
}

mod keepalive {
    use super::support;

    // Keep-alive reuse and chunked streaming — the serving shape harbor relies
    // on: sequential requests on one connection, each answered with an
    // unknown-length (chunked) response, and a response whose body is produced
    // while the client is already reading.

    use std::io::{Cursor, Read, Write};
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn chunked_reuse() {
        let (server, mut client) = support::new_one_server_one_client();

        std::thread::spawn(move || {
            for i in 0..3 {
                let rq = server.recv().unwrap();
                let body = format!("resp-{i}").into_bytes();
                // unknown length => chunked framing => connection stays reusable
                rq.respond(
                    justhttp::Response::empty(justhttp::StatusCode(200))
                        .with_data(Cursor::new(body), None),
                )
                .unwrap();
            }
        });

        // same socket serves all three requests: keep-alive + chunked framing
        for i in 0..3 {
            write!(client, "GET /{i} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
            let (headers, body) = support::read_response(&mut client);
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("transfer-encoding: chunked"),
                "expected chunked framing, headers were: {headers}"
            );
            assert_eq!(body, format!("resp-{i}"));
        }
    }

    /// A `Read` fed by a channel: `recv()` per chunk, EOF when senders are gone.
    /// This is the executor-to-socket shape a streaming server uses.
    struct ChannelReader {
        rx: mpsc::Receiver<Vec<u8>>,
        pending: Vec<u8>,
    }

    impl Read for ChannelReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pending.is_empty() {
                match self.rx.recv() {
                    Ok(chunk) => self.pending = chunk,
                    Err(_) => return Ok(0), // all senders dropped => EOF
                }
            }
            let n = buf.len().min(self.pending.len());
            buf[..n].copy_from_slice(&self.pending[..n]);
            self.pending.drain(..n);
            Ok(n)
        }
    }

    #[test]
    fn streamed() {
        let (server, mut client) = support::new_one_server_one_client();
        let (tx, rx) = mpsc::channel::<Vec<u8>>();

        std::thread::spawn(move || {
            let rq = server.recv().unwrap();
            let reader = ChannelReader {
                rx,
                pending: Vec::new(),
            };
            rq.respond(
                justhttp::Response::empty(justhttp::StatusCode(200)).with_data(reader, None),
            )
            .unwrap();
        });

        write!(client, "GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();

        // A chunk large enough to overflow both buffering layers (the chunked
        // Encoder's 8 KiB internal buffer and the connection's 1 KiB BufWriter)
        // must reach the client while the body is still open. Smaller writes
        // legitimately wait for more data — that is the buffering contract.
        tx.send(vec![b'e'; 32 * 1024]).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        let mut seen = Vec::new();
        let mut byte = [0u8; 1];
        while !String::from_utf8_lossy(&seen).contains("eeeeeeeeeeeeeeee") {
            assert_eq!(
                client.read(&mut byte).unwrap(),
                1,
                "EOF before the streamed chunk arrived"
            );
            seen.push(byte[0]);
        }

        // closing the channel ends the body; read until the terminating 0-chunk
        drop(tx);
        let mut rest = Vec::new();
        while !rest.ends_with(b"0\r\n\r\n") {
            match client.read(&mut byte) {
                Ok(1) => rest.push(byte[0]),
                _ => break,
            }
        }
        assert!(
            rest.ends_with(b"0\r\n\r\n"),
            "chunked body did not terminate cleanly"
        );
    }
}

mod buffering {
    use super::support;

    use std::io::{Cursor, Read, Write};
    use std::sync::{
        Arc,
        atomic::{
            AtomicUsize,
            Ordering::{AcqRel, Acquire},
        },
    };

    struct MeteredReader<T> {
        inner: T,
        position: Arc<AtomicUsize>,
    }

    impl<T> Read for MeteredReader<T>
    where
        T: Read,
    {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.inner.read(buf) {
                Ok(read) => {
                    self.position.fetch_add(read, AcqRel);
                    Ok(read)
                }
                e => e,
            }
        }
    }

    type Reader = MeteredReader<Cursor<String>>;

    fn big_response_reader() -> Reader {
        let big_body = "ABCDEFGHIJKLMNOPQRSTUVXYZ".repeat(1024 * 1024 * 16);
        MeteredReader {
            inner: Cursor::new(big_body),
            position: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn identity_served(r: &mut Reader) -> justhttp::Response<&mut Reader> {
        let body_len = r.inner.get_ref().len();
        justhttp::Response::empty(200)
            .with_chunked_threshold(usize::MAX)
            .with_data(r, Some(body_len))
    }

    /// Checks that a body-Read:er is not called when the client has disconnected
    #[test]
    fn responding_to_closed_client() {
        let (server, mut stream) = support::new_one_server_one_client();
        write!(
            stream,
            "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .unwrap();

        let request = server.recv().unwrap();

        // Client already disconnected
        drop(stream);

        let mut reader = big_response_reader();
        request
            .respond(identity_served(&mut reader))
            .expect("Successful");

        assert!(reader.position.load(Acquire) < 1024 * 1024);
    }

    /// Checks that a slow client does not cause data to be consumed and buffered from a reader
    #[test]
    fn responding_to_non_consuming_client() {
        let (server, mut stream) = support::new_one_server_one_client();
        write!(
            stream,
            "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .unwrap();

        let request = server.recv().unwrap();

        let mut reader = big_response_reader();
        let position = reader.position.clone();

        // Client still connected, but not reading anything
        std::thread::spawn(move || {
            request
                .respond(identity_served(&mut reader))
                .expect("Successful");
        });

        std::thread::sleep(std::time::Duration::from_millis(100));

        // It seems the client TCP socket can buffer quite a lot, so we need to be permissive
        assert!(position.load(Acquire) < 8 * 1024 * 1024);

        drop(stream);
    }
}

mod prompt {

    use justhttp::{Response, Server};
    use std::io::{Read, Write, copy};
    use std::net::{Shutdown, TcpStream};
    use std::ops::Deref;
    use std::sync::Arc;
    use std::sync::mpsc::channel;
    use std::thread::{sleep, spawn};
    use std::time::Duration;

    /// Stream that produces bytes very slowly
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    struct SlowByteSrc {
        val: u8,
        len: usize,
    }
    impl Read for SlowByteSrc {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            sleep(Duration::from_millis(100));
            let l = self.len.min(buf.len()).min(1000);
            for v in buf[..l].iter_mut() {
                *v = self.val;
            }
            self.len -= l;
            Ok(l)
        }
    }

    /// crude impl of http `Transfer-Encoding: chunked`
    fn encode_chunked(data: &mut dyn Read, output: &mut dyn Write) {
        let mut buf = [0u8; 4096];
        loop {
            let l = data.read(&mut buf).unwrap();
            write!(output, "{:X}\r\n", l).unwrap();
            output.write_all(&buf[..l]).unwrap();
            write!(output, "\r\n").unwrap();
            if l == 0 {
                break;
            }
        }
    }

    mod prompt_pipelining {
        use super::*;

        /// Check that pipelined requests on the same connection are received promptly.
        fn assert_requests_parsed_promptly(
            req_cnt: usize,
            req_body: &'static [u8],
            timeout: Duration,
            req_writer: impl FnOnce(&mut dyn Write) + Send + 'static,
        ) {
            let resp_body = SlowByteSrc {
                val: 42,
                len: 1_000_000,
            }; // very slow response body

            let server = Server::http("0.0.0.0:0").unwrap();
            let mut client = TcpStream::connect(server.server_addr().to_ip().unwrap()).unwrap();
            let (svr_send, svr_rcv) = channel();

            spawn(move || {
                for _ in 0..req_cnt {
                    let mut req = server.recv().unwrap();
                    // read the whole body of the request
                    let mut body = Vec::new();
                    req.as_reader().read_to_end(&mut body).unwrap();
                    assert_eq!(req_body, body.as_slice());
                    // The next pipelined request should now be available for parsing,
                    // while we send the (possibly slow) response in another thread
                    spawn(move || {
                        req.respond(Response::empty(200).with_data(resp_body, Some(resp_body.len)))
                    });
                }
                svr_send.send(()).unwrap();
            });

            spawn(move || req_writer(&mut client));

            // requests must be sent and received quickly (before timeout expires)
            svr_rcv
                .recv_timeout(timeout)
                .expect("Server did not finish reading pipelined requests quickly enough");
        }

        #[test]
        fn empty() {
            assert_requests_parsed_promptly(5, &[], Duration::from_millis(200), move |wr| {
                for _ in 0..5 {
                    write!(wr, "GET / HTTP/1.1\r\n").unwrap();
                    write!(wr, "Connection: keep-alive\r\n\r\n").unwrap();
                }
            });
        }

        #[test]
        fn content_length_short() {
            let body = &[65u8; 100]; // short but not trivial
            assert_requests_parsed_promptly(5, body, Duration::from_millis(200), move |wr| {
                for _ in 0..5 {
                    write!(wr, "GET / HTTP/1.1\r\n").unwrap();
                    write!(wr, "Connection: keep-alive\r\n").unwrap();
                    write!(wr, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
                    wr.write_all(body).unwrap();
                }
            });
        }

        #[test]
        fn content_length_long() {
            let body = &[65u8; 10000]; // long enough that it won't be buffered
            assert_requests_parsed_promptly(5, body, Duration::from_millis(200), move |wr| {
                for _ in 0..5 {
                    write!(wr, "GET / HTTP/1.1\r\n").unwrap();
                    write!(wr, "Connection: keep-alive\r\n").unwrap();
                    write!(wr, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
                    wr.write_all(body).unwrap();
                }
            });
        }

        #[test]
        fn chunked() {
            let body = &[65u8; 10000];
            assert_requests_parsed_promptly(5, body, Duration::from_millis(200), move |wr| {
                for _ in 0..5 {
                    write!(wr, "GET / HTTP/1.1\r\n").unwrap();
                    write!(wr, "Connection: keep-alive\r\n").unwrap();
                    write!(wr, "Transfer-Encoding: chunked\r\n\r\n").unwrap();
                    encode_chunked(&mut &body[..], wr);
                }
            });
        }
    }

    mod prompt_responses {
        use super::*;

        /// Check that response is sent promptly without waiting for full request body.
        fn assert_responds_promptly(
            timeout: Duration,
            req_writer: impl FnOnce(&mut dyn Write) + Send + 'static,
        ) {
            let server = Server::http("0.0.0.0:0").unwrap();
            let client = TcpStream::connect(server.server_addr().to_ip().unwrap()).unwrap();

            spawn(move || {
                loop {
                    // server attempts to respond immediately
                    let req = server.recv().unwrap();
                    req.respond(Response::empty(400)).unwrap();
                }
            });

            let client = Arc::new(client);
            let client_write = Arc::clone(&client);
            // request written (possibly very slowly) in another thread
            spawn(move || req_writer(&mut client_write.deref()));

            // response should arrive quickly (before timeout expires)
            client.set_read_timeout(Some(timeout)).unwrap();
            let resp = client.deref().read(&mut [0u8; 4096]);
            let _ = client.shutdown(Shutdown::Both);
            assert!(resp.is_ok(), "Server response was not sent promptly");
        }

        static SLOW_BODY: SlowByteSrc = SlowByteSrc {
            val: 65,
            len: 1_000_000,
        };

        #[test]
        fn content_length_http11() {
            assert_responds_promptly(Duration::from_millis(200), move |wr| {
                write!(wr, "GET / HTTP/1.1\r\n").unwrap();
                write!(wr, "Content-Length: {}\r\n\r\n", SLOW_BODY.len).unwrap();
                copy(&mut SLOW_BODY.clone(), wr).unwrap();
            });
        }

        #[test]
        fn content_length_http10() {
            assert_responds_promptly(Duration::from_millis(200), move |wr| {
                write!(wr, "GET / HTTP/1.0\r\n").unwrap();
                write!(wr, "Content-Length: {}\r\n\r\n", SLOW_BODY.len).unwrap();
                copy(&mut SLOW_BODY.clone(), wr).unwrap();
            });
        }

        #[test]
        fn expect_continue() {
            assert_responds_promptly(Duration::from_millis(200), move |wr| {
                write!(wr, "GET / HTTP/1.1\r\n").unwrap();
                write!(wr, "Expect: 100 continue\r\n").unwrap();
                write!(wr, "Content-Length: {}\r\n\r\n", SLOW_BODY.len).unwrap();
                copy(&mut SLOW_BODY.clone(), wr).unwrap();
            });
        }

        #[test]
        fn chunked() {
            assert_responds_promptly(Duration::from_millis(200), move |wr| {
                write!(wr, "GET / HTTP/1.1\r\n").unwrap();
                write!(wr, "Transfer-Encoding: chunked\r\n\r\n").unwrap();
                encode_chunked(&mut SLOW_BODY.clone(), wr);
            });
        }
    }
}

/// The first-request clock (~60s each, so `#[ignore]`d by design; run with
/// `cargo test -p justhttp --test suite -- --ignored`). A connection that has
/// never sent a byte is closed; one that has served a request keeps its
/// keep-alive idle forever. Both halves matter: the first bounds an anonymous
/// caller holding sockets, the second is what a REPL at its prompt relies on.
mod first_request {
    use super::support;

    use std::io::{Read, Write};
    use std::time::Instant;

    #[test]
    #[ignore = "~60s by design: exercises the first-request timeout"]
    fn a_connection_that_never_speaks_is_closed() {
        let (_server, mut client) = support::new_one_server_one_client();
        let t0 = Instant::now();
        let mut content = String::new();
        let _ = client.read_to_string(&mut content);
        assert!(
            content.starts_with("HTTP/1.1 408"),
            "expected 408, got {:?}",
            content.lines().next()
        );
        assert!(t0.elapsed().as_secs() >= 55, "closed too early: {:?}", t0.elapsed());
    }

    #[test]
    #[ignore = "~75s by design: proves keep-alive is not on the first-request clock"]
    fn a_served_connection_may_idle_past_the_first_request_timeout() {
        let (server, mut client) = support::new_one_server_one_client();
        std::thread::spawn(move || {
            for rq in server.incoming_requests() {
                let _ = rq.respond(justhttp::Response::from_string("ok".to_owned()));
            }
        });

        let req = "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n";
        write!(client, "{req}").unwrap();
        let (headers, _) = support::read_response(&mut client);
        assert!(headers.starts_with("HTTP/1.1 200"), "first request: {headers}");

        // Well past FIRST_REQUEST_TIMEOUT. This connection has served a
        // request, so it is a keep-alive client and the clock does not apply.
        std::thread::sleep(std::time::Duration::from_secs(75));

        write!(client, "{req}").expect("connection was closed during keep-alive idle");
        let (headers, _) = support::read_response(&mut client);
        assert!(headers.starts_with("HTTP/1.1 200"), "second request: {headers}");
    }
}

mod unblock {

    use std::sync::Arc;
    use std::thread;

    #[test]
    fn unblock_server() {
        let server = justhttp::Server::http("0.0.0.0:0").unwrap();
        let s = Arc::new(server);

        let s1 = s.clone();
        thread::spawn(move || s1.unblock());

        // Without unblock this would hang forever
        for _rq in s.incoming_requests() {}
    }

    #[test]
    fn unblock_threads() {
        let server = justhttp::Server::http("0.0.0.0:0").unwrap();
        let s = Arc::new(server);

        let s1 = s.clone();
        let s2 = s.clone();
        let h1 = thread::spawn(move || for _rq in s1.incoming_requests() {});
        let h2 = thread::spawn(move || for _rq in s2.incoming_requests() {});

        // Graceful shutdown; removing even one of the
        // unblock calls prevents termination
        s.unblock();
        s.unblock();
        h1.join().unwrap();
        h2.join().unwrap();
    }
}

#[cfg(unix)]
mod unix {

    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
        path::PathBuf,
    };

    #[test]
    fn unix_basic_handling() {
        let path = std::env::temp_dir().join(format!("justhttp-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let server = justhttp::Server::http_unix(&path).unwrap();
        let path: PathBuf = server
            .server_addr()
            .to_unix()
            .unwrap()
            .as_pathname()
            .unwrap()
            .into();
        let mut client = UnixStream::connect(path).unwrap();

        write!(
            client,
            "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .unwrap();

        let request = server.recv().unwrap();
        assert!(*request.method() == justhttp::Method::Get);
        assert_eq!(request.url(), "/");
        request
            .respond(justhttp::Response::from_string("hello world".to_owned()))
            .unwrap();

        server.try_recv().unwrap();

        let mut content = String::new();
        client.read_to_string(&mut content).unwrap();
        assert!(content.ends_with("hello world"));
    }
}

mod stall {
    use super::support;

    // Regression test for the response write timeout carried in this crate: a
    // client that stops reading its response must not pin a server thread inside
    // `write` forever. Slow by design (the timeout under test is 10s), so it is
    // `#[ignore]`d: run with `cargo test --test suite -- --ignored`.

    use std::io::{Cursor, Write};
    use std::time::{Duration, Instant};

    #[test]
    #[ignore = "~35s by design: exercises the 10s stalled-reader write timeout"]
    fn stalled_reader_reclaimed() {
        let (server, mut client) = support::new_one_server_one_client();

        write!(client, "GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let rq = server.recv().unwrap();

        // Far beyond any kernel socket buffer; the client never reads a byte.
        let body = vec![b'x'; 64 << 20];
        let len = body.len();
        let t = std::thread::spawn(move || {
            let start = Instant::now();
            let _ = rq.respond(
                justhttp::Response::empty(justhttp::StatusCode(200))
                    .with_data(Cursor::new(body), Some(len)),
            );
            start.elapsed()
        });

        // without the patch this join never returns: the worker is parked in write()
        let elapsed = t.join().unwrap();
        assert!(
            elapsed >= Duration::from_secs(5),
            "returned suspiciously fast ({elapsed:?}) — did the client read after all?"
        );
        assert!(
            elapsed <= Duration::from_secs(40),
            "write-timeout backstop lost: respond took {elapsed:?}"
        );
        drop(client);
    }
}
