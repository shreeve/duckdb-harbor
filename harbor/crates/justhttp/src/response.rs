//! One response: status, headers, and a body streamed from any `Read` —
//! chunked when the length is unknown, with the connection-management
//! headers owned here so a handler cannot lie about them.

use crate::http::{Header, HttpVersion, StatusCode};
use httpdate::HttpDate;

use std::io::Result as IoResult;
use std::io::{self, Cursor, Read, Write};

use std::str::FromStr;
use std::time::SystemTime;

/// Object representing an HTTP response whose purpose is to be given to a `Request`.
///
/// Some headers cannot be changed. Trying to define the value
/// of one of these will have no effect:
///
/// - `Connection`
/// - `Trailer`
/// - `Transfer-Encoding`
/// - `Upgrade`
///
/// Some headers have special behaviors:
///
/// - `Content-Encoding`: If you define this header, the library
///   will assume that the data from the `Read` object has the specified encoding
///   and will just pass-through.
///
/// - `Content-Length`: The length of the data should be set manually
///   using the `Response` object's API. Attempting to set the value of this
///   header will be equivalent to modifying the size of the data but the header
///   itself may not be present in the final result.
///
/// - `Content-Type`: You may only set this header to one value at a time. If you
///   try to set it more than once, the existing value will be overwritten. This
///   behavior differs from the default for most headers, which is to allow them to
///   be set multiple times in the same response.
///
pub struct Response<R> {
    reader: R,
    status_code: StatusCode,
    headers: Vec<Header>,
    data_length: Option<usize>,
}

/// The known body length from which a response is sent chunked rather than
/// with a `Content-Length`.
const CHUNKED_THRESHOLD: usize = 32768;

/// How a response body is framed on the wire.
#[derive(Copy, Clone)]
enum TransferEncoding {
    Identity,
    Chunked,
}

/// Appends a `Date: ...\r\n` line with the current time. The rendered line is
/// cached per thread and reused until the clock's whole-second changes (the
/// header's own resolution); compared with `!=`, so a clock stepped backwards
/// just reformats.
fn write_date_line(out: &mut Vec<u8>) {
    use std::cell::RefCell;
    thread_local! {
        static CACHED: RefCell<(u64, Vec<u8>)> = const { RefCell::new((u64::MAX, Vec::new())) };
    }
    let now = SystemTime::now();
    let secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    CACHED.with(|cell| {
        let mut cached = cell.borrow_mut();
        if cached.0 != secs {
            cached.1.clear();
            let _ = write!(cached.1, "Date: {}\r\n", HttpDate::from(now));
            cached.0 = secs;
        }
        out.extend_from_slice(&cached.1);
    });
}

/// The framing for a response: a function of the version, the status and
/// the body's length, never of anything the client sent.
///
/// An unknown length leaves chunked as the only framing that can both start
/// before the body is complete and delimit it on a reusable connection.
/// Identity framing would have to read the whole body to learn its length,
/// so a header that could choose it would hand control of this server's
/// memory to the caller: a streamed six-million-row result is +316 MB of
/// RSS when buffered. A request's `TE` header is therefore not consulted at
/// all.
fn choose_transfer_encoding(
    status_code: StatusCode,
    http_version: &HttpVersion,
    entity_length: Option<usize>,
) -> TransferEncoding {
    // HTTP/1.0 has no chunked encoding, and RFC 9112 §6.1 forbids a
    // Transfer-Encoding on a 1xx or 204.
    if *http_version <= (1, 0) || status_code.0 < 200 || status_code.0 == 204 {
        return TransferEncoding::Identity;
    }
    if entity_length.is_none_or(|len| len >= CHUNKED_THRESHOLD) {
        return TransferEncoding::Chunked;
    }
    TransferEncoding::Identity
}

impl<R> Response<R>
where
    R: Read,
{
    /// Creates a new Response object.
    pub fn new(
        status_code: StatusCode,
        headers: Vec<Header>,
        data: R,
        data_length: Option<usize>,
    ) -> Response<R> {
        let mut response = Response {
            reader: data,
            status_code,
            headers: Vec::with_capacity(16),
            data_length,
        };

        for h in headers {
            response.add_header(h)
        }

        response
    }

    /// Adds a header to the list.
    /// Does all the checks.
    pub fn add_header<H>(&mut self, header: H)
    where
        H: Into<Header>,
    {
        let header = header.into();

        // ignoring forbidden headers
        if header.field.equiv("Connection")
            || header.field.equiv("Trailer")
            || header.field.equiv("Transfer-Encoding")
            || header.field.equiv("Upgrade")
        {
            return;
        }

        // if the header is Content-Length, setting the data length
        if header.field.equiv("Content-Length") {
            if let Ok(val) = usize::from_str(header.value.as_str()) {
                self.data_length = Some(val)
            }

            return;
        // if the header is Content-Type and it's already set, overwrite it
        } else if header.field.equiv("Content-Type") {
            if let Some(content_type_header) = self
                .headers
                .iter_mut()
                .find(|h| h.field.equiv("Content-Type"))
            {
                content_type_header.value = header.value;
                return;
            }
        }

        self.headers.push(header);
    }

    /// Returns the same response, but with an additional header.
    ///
    /// Some headers cannot be modified and some other have a
    ///  special behavior. See the documentation above.
    #[inline]
    #[must_use]
    pub fn with_header<H>(mut self, header: H) -> Response<R>
    where
        H: Into<Header>,
    {
        self.add_header(header.into());
        self
    }

    /// Returns the same response, but with a different status code.
    #[inline]
    #[must_use]
    pub fn with_status_code<S>(mut self, code: S) -> Response<R>
    where
        S: Into<StatusCode>,
    {
        self.status_code = code.into();
        self
    }

    /// Prints the HTTP response to a writer: the bytes that go to the
    /// client's socket, framed for `http_version`.
    ///
    /// Note: does not flush the writer.
    pub(crate) fn raw_print<W: Write>(
        self,
        mut writer: W,
        http_version: HttpVersion,
        do_not_send_body: bool,
    ) -> IoResult<()> {
        let transfer_encoding = choose_transfer_encoding(
            self.status_code,
            &http_version,
            self.data_length,
        );

        // The whole head — status line through the blank separator — is
        // assembled in one local buffer and sent with a single write, instead
        // of allocating Header objects for Server/Date/Content-Length and
        // pushing each fragment through the (mutex-guarded) writer. Wire
        // order is unchanged: Server, Date, user headers, then TE/CL.
        let mut head = Vec::with_capacity(256);
        write!(
            head,
            "HTTP/{}.{} {} {}\r\n",
            http_version.0,
            http_version.1,
            self.status_code.0,
            self.status_code.default_reason_phrase()
        )?;
        if !self.headers.iter().any(|h| h.field.equiv("Server")) {
            head.extend_from_slice(b"Server: justhttp\r\n");
        }
        if !self.headers.iter().any(|h| h.field.equiv("Date")) {
            write_date_line(&mut head);
        }
        for header in &self.headers {
            head.extend_from_slice(header.field.as_str().as_ref());
            head.extend_from_slice(b": ");
            head.extend_from_slice(header.value.as_str().as_ref());
            head.extend_from_slice(b"\r\n");
        }

        // Identity framing with an unknown length — only reachable for HTTP/1.0
        // clients now, since 1.1 always chunks an unknown length — is delimited
        // by the connection close, which is how HTTP/1.0 has always framed a
        // body of unknown length. `conn.rs` closes after every 1.0 request, so
        // that delimiter is guaranteed to arrive.
        //
        // This used to `read_to_end` the body to discover its length and emit a
        // Content-Length. That kept the connection reusable, which HTTP/1.0
        // barely wants, at the cost of holding the entire response in memory —
        // and harbor streams results with no size limit down this path, so the
        // cost was unbounded and chosen by the caller.
        let mut reader: Box<dyn Read> = Box::new(self.reader);
        let data_length = self.data_length;

        // checking whether to ignore the body of the response
        // status code 1xx, 204 and 304 MUST not include a body
        let do_not_send_body =
            do_not_send_body || matches!(self.status_code.0, 100..=199 | 204 | 304);

        // framing header, then the blank separator, then the single head write
        match transfer_encoding {
            TransferEncoding::Chunked => {
                head.extend_from_slice(b"Transfer-Encoding: chunked\r\n");
            }

            // No Content-Length when the length is unknown: the close is the
            // delimiter (see above).
            TransferEncoding::Identity => {
                if let Some(length) = data_length {
                    write!(head, "Content-Length: {length}\r\n")?;
                }
            }
        };
        head.extend_from_slice(b"\r\n");
        writer.write_all(&head)?;

        // sending the body
        if !do_not_send_body {
            match transfer_encoding {
                TransferEncoding::Chunked => {
                    use chunked_transfer::Encoder;

                    let mut writer = Encoder::new(writer);
                    io::copy(&mut reader, &mut writer)?;
                }

                // An unknown length is a stream: copy it. A known length of
                // zero has nothing to copy.
                TransferEncoding::Identity if data_length != Some(0) => {
                    io::copy(&mut reader, &mut writer)?;
                }

                TransferEncoding::Identity => (),
            }
        }

        Ok(())
    }
}

impl Response<Cursor<Vec<u8>>> {
    /// A 200 response with the string as its body, `Content-Type:
    /// text/plain; charset=UTF-8`, and a known length (identity framing).
    pub fn from_string<S>(data: S) -> Response<Cursor<Vec<u8>>>
    where
        S: Into<String>,
    {
        let data = data.into();
        let data_len = data.len();

        Response::new(
            StatusCode(200),
            vec![
                Header::from_bytes(&b"Content-Type"[..], &b"text/plain; charset=UTF-8"[..])
                    .unwrap(),
            ],
            Cursor::new(data.into_bytes()),
            Some(data_len),
        )
    }
}

impl Response<io::Empty> {
    /// Builds an empty `Response` with the given status code.
    pub fn empty<S>(status_code: S) -> Response<io::Empty>
    where
        S: Into<StatusCode>,
    {
        Response::new(
            status_code.into(),
            Vec::with_capacity(0),
            io::empty(),
            Some(0),
        )
    }
}
