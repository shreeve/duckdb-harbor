//! One response: status, headers, and a body streamed from any `Read` —
//! chunked when the length is unknown, with the connection-management
//! headers owned here so a handler cannot lie about them.

use crate::http::{Header, HttpVersion, StatusCode};
use httpdate::HttpDate;
use std::cmp::Ordering;

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
    chunked_threshold: Option<usize>,
}

/// Transfer encoding to use when sending the message.
/// Note that only *supported* encodings are listed here.
#[derive(Copy, Clone)]
enum TransferEncoding {
    Identity,
    Chunked,
}

impl FromStr for TransferEncoding {
    type Err = ();

    fn from_str(input: &str) -> Result<TransferEncoding, ()> {
        if input.eq_ignore_ascii_case("identity") {
            Ok(TransferEncoding::Identity)
        } else if input.eq_ignore_ascii_case("chunked") {
            Ok(TransferEncoding::Chunked)
        } else {
            Err(())
        }
    }
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

fn choose_transfer_encoding(
    status_code: StatusCode,
    request_headers: &[Header],
    http_version: &HttpVersion,
    entity_length: Option<usize>,
    chunked_threshold: usize,
) -> TransferEncoding {
    // HTTP 1.0 doesn't support other encoding
    if *http_version <= (1, 0) {
        return TransferEncoding::Identity;
    }

    // Per section 3.3.1 of RFC7230:
    // A server MUST NOT send a Transfer-Encoding header field in any response with a status code
    // of 1xx (Informational) or 204 (No Content).
    if status_code.0 < 200 || status_code.0 == 204 {
        return TransferEncoding::Identity;
    }

    // parsing the request's TE header
    let user_request = request_headers
        .iter()
        // finding TE
        .find(|h| h.field.equiv("TE"))
        // getting the corresponding TransferEncoding
        .and_then(|header| {
            // getting list of requested elements
            let mut parse = parse_header_value(header.value.as_str());

            // sorting elements by most priority
            parse.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));

            // trying to parse each requested encoding
            for value in parse.iter() {
                // q=0 are ignored
                if value.1 <= 0.0 {
                    continue;
                }

                if let Ok(te) = TransferEncoding::from_str(value.0) {
                    return Some(te);
                }
            }

            // encoding not found
            None
        });

    // An unknown length leaves chunked as the only framing that can both start
    // before the body is complete and delimit it on a reusable connection. The
    // client does not get a vote here, and that is the point: honoring
    // `TE: identity` on an unknown-length response sends control of this
    // server's memory to the caller. `raw_print` would have to read the whole
    // body to discover its length, so a client could turn any streamed result
    // into an allocation of that result's size by adding one header —
    // measured at +316 MB of RSS on a six-million-row query, and neatly
    // sidestepping the ceiling the one-shot JSON shape enforces for exactly
    // this reason. `TE` is a hint about what the client can decode, never a
    // licence to pick the server's buffering strategy. (RFC 7230 dropped
    // `identity` from `TE` altogether.)
    if entity_length.is_none() {
        return TransferEncoding::Chunked;
    }

    if let Some(user_request) = user_request {
        return user_request;
    }

    // if the Content-Length is too big, using chunks writer
    if entity_length.is_none_or(|val| val >= chunked_threshold) {
        return TransferEncoding::Chunked;
    }

    // Identity by default
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
            chunked_threshold: None,
        };

        for h in headers {
            response.add_header(h)
        }

        response
    }

    /// Set a threshold for `Content-Length` where we chose chunked
    /// transfer. Notice that chunked transfer might happen regardless of
    /// this threshold, for instance when the request headers indicate
    /// it is wanted or when there is no `Content-Length`.
    #[must_use]
    pub fn with_chunked_threshold(mut self, length: usize) -> Response<R> {
        self.chunked_threshold = Some(length);
        self
    }

    /// The current `Content-Length` threshold for switching over to
    /// chunked transfer. The default is 32768 bytes. Notice that
    /// chunked transfer is mutually exclusive with sending a
    /// `Content-Length` header as per the HTTP spec.
    pub fn chunked_threshold(&self) -> usize {
        self.chunked_threshold.unwrap_or(32768)
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

    /// Returns the same response, but with different data.
    #[must_use]
    pub fn with_data<S>(self, reader: S, data_length: Option<usize>) -> Response<S>
    where
        S: Read,
    {
        Response {
            reader,
            headers: self.headers,
            status_code: self.status_code,
            data_length,
            chunked_threshold: self.chunked_threshold,
        }
    }

    /// Prints the HTTP response to a writer.
    ///
    /// This function is the one used to send the response to the client's socket.
    /// Therefore you shouldn't expect anything pretty-printed or even readable.
    ///
    /// The HTTP version and headers passed as arguments are used to
    ///  decide which features (most notably, encoding) to use.
    ///
    /// Note: does not flush the writer.
    pub fn raw_print<W: Write>(
        self,
        mut writer: W,
        http_version: HttpVersion,
        request_headers: &[Header],
        do_not_send_body: bool,
    ) -> IoResult<()> {
        let transfer_encoding = Some(choose_transfer_encoding(
            self.status_code,
            request_headers,
            &http_version,
            self.data_length,
            self.chunked_threshold(),
        ));

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
            Some(TransferEncoding::Chunked) => {
                head.extend_from_slice(b"Transfer-Encoding: chunked\r\n");
            }

            // No Content-Length when the length is unknown: the close is the
            // delimiter (see above).
            Some(TransferEncoding::Identity) => {
                if let Some(length) = data_length {
                    write!(head, "Content-Length: {length}\r\n")?;
                }
            }

            _ => (),
        };
        head.extend_from_slice(b"\r\n");
        writer.write_all(&head)?;

        // sending the body
        if !do_not_send_body {
            match transfer_encoding {
                Some(TransferEncoding::Chunked) => {
                    use chunked_transfer::Encoder;

                    let mut writer = Encoder::new(writer);
                    io::copy(&mut reader, &mut writer)?;
                }

                // An unknown length is a stream: copy it. A known length of
                // zero has nothing to copy.
                Some(TransferEncoding::Identity) if data_length != Some(0) => {
                    io::copy(&mut reader, &mut writer)?;
                }

                _ => (),
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

/// Parses the value of a header.
/// Suitable for `Accept-*`, `TE`, etc.
///
/// For example with `text/plain, image/png; q=1.5` this function would
/// return `[ ("text/plain", 1.0), ("image/png", 1.5) ]`
fn parse_header_value(input: &str) -> Vec<(&str, f32)> {
    input
        .split(',')
        .filter_map(|elem| {
            let mut params = elem.split(';');

            let t = params.next()?;

            let mut value = 1.0_f32;

            for p in params {
                if p.trim_start().starts_with("q=") {
                    if let Ok(val) = f32::from_str(p.trim_start()[2..].trim()) {
                        value = val;
                        break;
                    }
                }
            }

            Some((t.trim(), value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    #[allow(clippy::float_cmp)]
    fn test_parse_header() {
        let result = super::parse_header_value("text/html, text/plain; q=1.5 , image/png ; q=2.0");

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].0, "text/html");
        assert_eq!(result[0].1, 1.0);
        assert_eq!(result[1].0, "text/plain");
        assert_eq!(result[1].1, 1.5);
        assert_eq!(result[2].0, "image/png");
        assert_eq!(result[2].1, 2.0);
    }

    #[test]
    fn chunked_threshold() {
        let resp = crate::Response::from_string("test".to_string());
        assert_eq!(resp.chunked_threshold(), 32768);
        assert_eq!(resp.with_chunked_threshold(42).chunked_threshold(), 42);
    }
}
