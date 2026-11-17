// HTTP/1.1 on the wire: the request-head parser, the chunked-body skipper, the
// response writer and the `date` formatter. Every function here is pure: bytes in,
// bytes or a verdict out. No sockets, no clock, no memory allocation, nothing that
// can block.

/// Longest request head (request line, headers and blank line) the server accepts.
pub const MAX_HEAD: usize = 16 * 1024;
/// Most header lines in one request.
const MAX_HEADERS: usize = 100;
/// Longest method name. Real ones stop at seven letters.
const MAX_METHOD: usize = 16;

/// A status line, ready to copy to the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status(&'static str);

impl Status {
    pub const OK: Status = Status("HTTP/1.1 200 OK\r\n");
    pub const BAD_REQUEST: Status = Status("HTTP/1.1 400 Bad Request\r\n");
    pub const NOT_FOUND: Status = Status("HTTP/1.1 404 Not Found\r\n");
    pub const REQUEST_TIMEOUT: Status = Status("HTTP/1.1 408 Request Timeout\r\n");
    pub const PAYLOAD_TOO_LARGE: Status = Status("HTTP/1.1 413 Payload Too Large\r\n");
    pub const URI_TOO_LONG: Status = Status("HTTP/1.1 414 URI Too Long\r\n");
    pub const HEADERS_TOO_LARGE: Status = Status("HTTP/1.1 431 Request Header Fields Too Large\r\n");
    pub const NOT_IMPLEMENTED: Status = Status("HTTP/1.1 501 Not Implemented\r\n");
    pub const VERSION_NOT_SUPPORTED: Status = Status("HTTP/1.1 505 HTTP Version Not Supported\r\n");
}

/// What kind of body a response carries. JSON is labelled; plain text is sent bare.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Json,
    Plain,
}

/// What a response tells the client about the connection's future.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// It stays open. HTTP/1.1 assumes that, so no header is needed.
    Open,
    /// It stays open for an HTTP/1.0 client that asked: `connection: keep-alive`.
    KeepAlive,
    /// It closes after this response: `connection: close`.
    Close,
}

/// Why a request was refused before it reached a route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    BadRequest,
    Timeout,
    PayloadTooLarge,
    UriTooLong,
    HeadersTooLarge,
    NotImplemented,
    VersionNotSupported,
}

impl Reject {
    /// The status line and plain-text body that tell the client what went wrong.
    pub fn reply(self) -> (Status, &'static str) {
        match self {
            Reject::BadRequest => (Status::BAD_REQUEST, "bad request"),
            Reject::Timeout => (Status::REQUEST_TIMEOUT, "request timeout"),
            Reject::PayloadTooLarge => (Status::PAYLOAD_TOO_LARGE, "payload too large"),
            Reject::UriTooLong => (Status::URI_TOO_LONG, "uri too long"),
            Reject::HeadersTooLarge => (Status::HEADERS_TOO_LARGE, "request header fields too large"),
            Reject::NotImplemented => (Status::NOT_IMPLEMENTED, "not implemented"),
            Reject::VersionNotSupported => (Status::VERSION_NOT_SUPPORTED, "http version not supported"),
        }
    }
}

/// How the length of a request body is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    None,
    Length(u64),
    Chunked,
}

/// One parsed request head. The slices borrow from the buffer that was parsed.
#[derive(Debug, PartialEq, Eq)]
pub struct Head<'a> {
    pub method: &'a [u8],
    /// The target up to `?`, exactly as sent (not percent-decoded).
    pub path: &'a [u8],
    /// Everything after the first `?`, if there was one.
    pub query: Option<&'a [u8]>,
    /// Bytes this head occupies, blank line included.
    pub len: usize,
    pub body: Framing,
    /// Whether the connection stays open after the response.
    pub keep_alive: bool,
    /// False for HTTP/1.0 clients.
    pub http11: bool,
    /// The client is waiting for `100 Continue` before it sends its body.
    pub expect_continue: bool,
}

/// Bytes allowed in a method or header name (RFC 9110 `tchar`).
static TOKEN: [bool; 256] = {
    let mut table = [false; 256];
    let mut byte = 0;
    while byte < 256 {
        table[byte] = (byte as u8).is_ascii_alphanumeric() || matches!(byte as u8, b'!' | b'#'..=b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~');
        byte += 1;
    }
    table
};

/// Index of the first byte at or after `i` that is below `LIMIT` or is DEL (0x7f), or
/// `buf.len()` if there is none. Checks eight bytes per step, so long URLs and header
/// values cost little. The bit tricks can flag a byte wrongly only above a true hit,
/// so the lowest flagged byte is always exact.
#[inline]
fn scan<const LIMIT: u8>(buf: &[u8], mut i: usize) -> usize {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGH: u64 = 0x8080_8080_8080_8080;
    while let Some(chunk) = buf.get(i..i + 8) {
        let word = u64::from_le_bytes(chunk.try_into().expect("eight bytes"));
        let low = word.wrapping_sub(ONES * LIMIT as u64) & !word;
        let del = word ^ (ONES * 0x7f);
        let hit = (low | (del.wrapping_sub(ONES) & !del)) & HIGH;
        if hit != 0 {
            return i + hit.trailing_zeros() as usize / 8;
        }
        i += 8;
    }
    while i < buf.len() && buf[i] >= LIMIT && buf[i] != 0x7f {
        i += 1;
    }
    i
}

/// Parses one request head from the front of `buf`.
///
/// `Ok(None)` means the head is not all here yet. The parser is strict where leniency has
/// let request smuggling through elsewhere: lines must end in CRLF, a header name must
/// touch its colon, folded lines are refused, and a body may be framed in only one way.
pub fn parse_head(buf: &[u8]) -> Result<Option<Head<'_>>, Reject> {
    // Request line: METHOD SP target SP HTTP/1.x CRLF
    let mut i = 0;
    while i < buf.len() && TOKEN[buf[i] as usize] {
        i += 1;
    }
    if i > MAX_METHOD {
        return Err(Reject::BadRequest);
    }
    if i == buf.len() {
        return Ok(None);
    }
    if i == 0 || buf[i] != b' ' {
        return Err(Reject::BadRequest);
    }
    let method = &buf[..i];
    let start = i + 1;
    let end = scan::<0x21>(buf, start);
    if end == buf.len() {
        return Ok(None);
    }
    if end == start || buf[end] != b' ' {
        return Err(Reject::BadRequest);
    }
    let (path, query) = split_target(&buf[start..end]);
    let Some(version) = buf.get(end + 1..end + 11) else { return Ok(None) };
    let http11 = match version {
        // Any HTTP/1.x above 1.0 is to be treated as 1.1.
        [b'H', b'T', b'T', b'P', b'/', b'1', b'.', minor @ b'0'..=b'9', b'\r', b'\n'] => *minor != b'0',
        [b'H', b'T', b'T', b'P', b'/', b'0'..=b'9', b'.', b'0'..=b'9', b'\r', b'\n'] => {
            return Err(Reject::VersionNotSupported)
        }
        _ => return Err(Reject::BadRequest),
    };

    // Header lines, up to the blank one. Only four of them change what the server does.
    i = end + 11;
    let (mut length, mut chunked, mut close, mut keep, mut expect) = (None, false, false, false, false);
    let mut lines = 0;
    loop {
        match (buf.get(i), buf.get(i + 1)) {
            (None, _) | (Some(b'\r'), None) => return Ok(None),
            (Some(b'\r'), Some(b'\n')) => break,
            (Some(b'\r'), Some(_)) => return Err(Reject::BadRequest),
            _ => {}
        }
        lines += 1;
        if lines > MAX_HEADERS {
            return Err(Reject::HeadersTooLarge);
        }
        let name_start = i;
        while i < buf.len() && TOKEN[buf[i] as usize] {
            i += 1;
        }
        if i == buf.len() {
            return Ok(None);
        }
        if i == name_start || buf[i] != b':' {
            return Err(Reject::BadRequest);
        }
        let name = &buf[name_start..i];
        i += 1;
        while i < buf.len() && matches!(buf[i], b' ' | b'\t') {
            i += 1;
        }
        let value_start = i;
        loop {
            i = scan::<0x20>(buf, i);
            match buf.get(i) {
                None => return Ok(None),
                Some(b'\t') => i += 1,
                Some(_) => break,
            }
        }
        match (buf[i], buf.get(i + 1)) {
            (b'\r', None) => return Ok(None),
            (b'\r', Some(b'\n')) => {}
            _ => return Err(Reject::BadRequest),
        }
        let value = buf[value_start..i].trim_ascii_end();
        i += 2;
        match name.len() {
            6 if name.eq_ignore_ascii_case(b"expect") => expect |= value.eq_ignore_ascii_case(b"100-continue"),
            10 if name.eq_ignore_ascii_case(b"connection") => {
                for option in value.split(|&byte| byte == b',') {
                    close |= option.trim_ascii().eq_ignore_ascii_case(b"close");
                    keep |= option.trim_ascii().eq_ignore_ascii_case(b"keep-alive");
                }
            }
            14 if name.eq_ignore_ascii_case(b"content-length") => {
                if length.is_some() {
                    return Err(Reject::BadRequest);
                }
                length = Some(parse_decimal(value).ok_or(Reject::BadRequest)?);
            }
            17 if name.eq_ignore_ascii_case(b"transfer-encoding") => {
                if chunked || !value.eq_ignore_ascii_case(b"chunked") {
                    return Err(Reject::NotImplemented);
                }
                chunked = true;
            }
            _ => {}
        }
    }
    let body = match (chunked, length) {
        // Two framings at once, or chunking from a client too old to have it: nobody can
        // say where this request ends, so it is not safe to guess.
        (true, Some(_)) => return Err(Reject::BadRequest),
        (true, None) if !http11 => return Err(Reject::BadRequest),
        (true, None) => Framing::Chunked,
        (false, None) | (false, Some(0)) => Framing::None,
        (false, Some(n)) => Framing::Length(n),
    };
    let keep_alive = if http11 { !close } else { keep && !close };
    Ok(Some(Head { method, path, query, len: i + 2, body, keep_alive, http11, expect_continue: expect && http11 }))
}

/// Splits a request target into path and query the way a URI parser does: the scheme and
/// host of an absolute target (`http://host/path`) are dropped, and so is any `#fragment`.
fn split_target(mut target: &[u8]) -> (&[u8], Option<&[u8]>) {
    if let Some(hash) = target.iter().position(|&byte| byte == b'#') {
        target = &target[..hash];
    }
    if target.first() != Some(&b'/')
        && let Some(scheme) = target.windows(3).position(|window| window == b"://")
    {
        let rest = &target[scheme + 3..];
        let host = rest.iter().position(|&byte| byte == b'/' || byte == b'?').unwrap_or(rest.len());
        target = &rest[host..];
    }
    let (path, query) = match target.iter().position(|&byte| byte == b'?') {
        Some(mark) => (&target[..mark], Some(&target[mark + 1..])),
        None => (target, None),
    };
    (if path.is_empty() { b"/".as_slice() } else { path }, query)
}

/// A `content-length` value: digits only, no sign, no spaces, and it must fit.
fn parse_decimal(digits: &[u8]) -> Option<u64> {
    if digits.is_empty() {
        return None;
    }
    digits.iter().try_fold(0u64, |total, &digit| {
        if !digit.is_ascii_digit() {
            return None;
        }
        total.checked_mul(10)?.checked_add((digit - b'0') as u64)
    })
}

/// Where the skipper is inside a chunked request body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chunk {
    /// Reading a chunk size in hex: the value so far, and how many digits.
    Size(u64, u8),
    /// Skipping a chunk extension (`;name=value`) up to the CR.
    Ext(u64),
    /// CR seen after the size line: LF must follow.
    SizeLf(u64),
    /// Skipping chunk data: this many bytes left.
    Data(u64),
    /// Chunk data is done: CR, then LF, must follow.
    DataCr,
    DataLf,
    /// In the trailer section after the last chunk; true while the current line is empty.
    Trailer(bool),
    /// CR seen in the trailer section: LF must follow, and a blank line ends the body.
    TrailerLf(bool),
}

impl Chunk {
    pub const START: Chunk = Chunk::Size(0, 0);
}

/// Walks through a chunked body without keeping any of it. Returns how many bytes of `buf`
/// belonged to the body and whether it ended; `None` means the framing is broken.
pub fn skip_chunked(state: &mut Chunk, buf: &[u8]) -> Option<(usize, bool)> {
    let mut i = 0;
    while i < buf.len() {
        let byte = buf[i];
        *state = match *state {
            Chunk::Size(size, digits) => match (byte as char).to_digit(16) {
                Some(digit) if digits < 16 => Chunk::Size(size << 4 | digit as u64, digits + 1),
                None if digits > 0 && byte == b'\r' => Chunk::SizeLf(size),
                None if digits > 0 && matches!(byte, b';' | b' ' | b'\t') => Chunk::Ext(size),
                _ => return None,
            },
            Chunk::Ext(size) if byte == b'\r' => Chunk::SizeLf(size),
            Chunk::Ext(size) if byte != b'\n' => Chunk::Ext(size),
            Chunk::SizeLf(0) if byte == b'\n' => Chunk::Trailer(true),
            Chunk::SizeLf(size) if byte == b'\n' => Chunk::Data(size),
            Chunk::Data(left) => {
                let take = left.min((buf.len() - i) as u64);
                i += take as usize;
                *state = if take == left { Chunk::DataCr } else { Chunk::Data(left - take) };
                continue;
            }
            Chunk::DataCr if byte == b'\r' => Chunk::DataLf,
            Chunk::DataLf if byte == b'\n' => Chunk::START,
            Chunk::Trailer(blank) if byte == b'\r' => Chunk::TrailerLf(blank),
            Chunk::Trailer(_) if byte != b'\n' => Chunk::Trailer(false),
            Chunk::TrailerLf(true) if byte == b'\n' => return Some((i + 1, true)),
            Chunk::TrailerLf(false) if byte == b'\n' => Chunk::Trailer(true),
            _ => return None,
        };
        i += 1;
    }
    Some((i, false))
}

/// Formats seconds since the Unix epoch as an HTTP date: `Mon, 16 Nov 2026 08:05:09 GMT`.
pub fn http_date(unix: u64) -> [u8; 29] {
    const DAYS: &[u8; 21] = b"SunMonTueWedThuFriSat";
    const MONTHS: &[u8; 36] = b"JanFebMarAprMayJunJulAugSepOctNovDec";
    let (days, secs) = (unix / 86_400, unix % 86_400);
    // Days to a calendar date (Howard Hinnant's algorithm): years start in March, so the
    // leap day comes last and every 400-year era has the same shape.
    let shifted = days + 719_468;
    let (era, day_of_era) = (shifted / 146_097, shifted % 146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let march_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * march_month + 2) / 5 + 1;
    let month = if march_month < 10 { march_month + 3 } else { march_month - 9 };
    let year = year_of_era + era * 400 + (month <= 2) as u64;
    let weekday = (days + 4) % 7; // day zero was a Thursday

    let mut out = *b"Thu, 01 Jan 1970 00:00:00 GMT";
    let mut two = |at: usize, value: u64| {
        out[at] = b'0' + (value / 10 % 10) as u8;
        out[at + 1] = b'0' + (value % 10) as u8;
    };
    two(5, day);
    two(12, year / 100);
    two(14, year);
    two(17, secs / 3_600);
    two(20, secs / 60 % 60);
    two(23, secs % 60);
    out[..3].copy_from_slice(&DAYS[weekday as usize * 3..][..3]);
    out[8..11].copy_from_slice(&MONTHS[(month as usize - 1) * 3..][..3]);
    out
}

/// Appends one complete response to `out`. Header names, their order and their spelling
/// are what hyper writes, so clients see the same bytes the previous server sent.
pub fn write_response(out: &mut Vec<u8>, status: Status, kind: Kind, next: Next, date: &[u8; 29], body: &[u8], head_only: bool) {
    out.reserve(192 + body.len());
    out.extend_from_slice(status.0.as_bytes());
    if kind == Kind::Json {
        out.extend_from_slice(b"content-type: application/json\r\n");
    }
    out.extend_from_slice(b"content-length: ");
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    let mut rest = body.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (rest % 10) as u8;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    out.extend_from_slice(&digits[at..]);
    out.extend_from_slice(match next {
        Next::Open => b"\r\ndate: ".as_slice(),
        Next::KeepAlive => b"\r\nconnection: keep-alive\r\ndate: ",
        Next::Close => b"\r\nconnection: close\r\ndate: ",
    });
    out.extend_from_slice(date);
    out.extend_from_slice(b"\r\n\r\n");
    if !head_only {
        out.extend_from_slice(body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(raw: &str) -> Head<'_> {
        parse_head(raw.as_bytes()).expect("valid").expect("complete")
    }

    #[test]
    fn parses_a_plain_get() {
        let raw = "GET /hello?bodies=3&steps=4 HTTP/1.1\r\nHost: phone.local:4000\r\nAccept: */*\r\n\r\nNEXT";
        let h = head(raw);
        assert_eq!((h.method, h.path, h.query), (&b"GET"[..], &b"/hello"[..], Some(&b"bodies=3&steps=4"[..])));
        assert_eq!((h.len, h.body), (raw.len() - 4, Framing::None));
        assert!(h.keep_alive && h.http11 && !h.expect_continue);
    }

    #[test]
    fn every_strict_prefix_is_incomplete_never_an_error() {
        let raw = b"POST /work?x=1 HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\nConnection: keep-alive, Upgrade\r\nX-Tab:\ta\tb \r\n\r\n";
        for cut in 0..raw.len() {
            assert_eq!(parse_head(&raw[..cut]), Ok(None), "a prefix of {cut} bytes");
        }
        assert_eq!(parse_head(raw).unwrap().unwrap().body, Framing::Length(5));
    }

    #[test]
    fn splits_targets_like_a_uri_parser() {
        let split = |target: &str| {
            let raw = format!("GET {target} HTTP/1.1\r\n\r\n");
            let h = parse_head(raw.as_bytes()).unwrap().unwrap();
            let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
            (text(h.path), h.query.map(text))
        };
        let some = |query: &str| Some(query.to_string());
        assert_eq!(split("/a/b"), ("/a/b".into(), None));
        assert_eq!(split("/a?"), ("/a".into(), some("")));
        assert_eq!(split("/a?x=1?y#frag"), ("/a".into(), some("x=1?y")));
        assert_eq!(split("/a#frag?x"), ("/a".into(), None));
        assert_eq!(split("http://host:4000/hello?x=1"), ("/hello".into(), some("x=1")));
        assert_eq!(split("HTTPS://host"), ("/".into(), None));
        assert_eq!(split("http://host?x=1"), ("/".into(), some("x=1")));
        assert_eq!(split("*"), ("*".into(), None));
        assert_eq!(split("/go?to=http://x/y"), ("/go".into(), some("to=http://x/y")));
    }

    #[test]
    fn decides_whether_the_connection_stays_open() {
        let keep = |raw: &str| head(raw).keep_alive;
        assert!(keep("GET / HTTP/1.1\r\n\r\n"));
        assert!(!keep("GET / HTTP/1.1\r\nConnection: close\r\n\r\n"));
        assert!(!keep("GET / HTTP/1.1\r\nconnection:  Keep-Alive ,\tCLOSE  \r\n\r\n"));
        assert!(keep("GET / HTTP/1.1\r\nConnection: upgrade\r\nX-Closer: close\r\n\r\n"));
        assert!(!keep("GET / HTTP/1.0\r\n\r\n"));
        assert!(keep("GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n"));
        assert!(!keep("GET / HTTP/1.0\r\nConnection: keep-alive\r\nConnection: close\r\n\r\n"));
        assert!(keep("GET / HTTP/1.7\r\n\r\n") && head("GET / HTTP/1.7\r\n\r\n").http11);
        assert!(!head("GET / HTTP/1.0\r\n\r\n").http11);
    }

    #[test]
    fn reads_body_framing_and_expectations() {
        let framing = |headers: &str| head(&format!("POST / HTTP/1.1\r\n{headers}\r\n")).body;
        assert_eq!(framing(""), Framing::None);
        assert_eq!(framing("Content-Length: 0\r\n"), Framing::None);
        assert_eq!(framing("CONTENT-LENGTH:\t0042 \r\n"), Framing::Length(42));
        assert_eq!(framing("Content-Length: 18446744073709551615\r\n"), Framing::Length(u64::MAX));
        assert_eq!(framing("transfer-encoding: Chunked\r\n"), Framing::Chunked);
        assert!(head("PUT / HTTP/1.1\r\nExpect: 100-Continue\r\nContent-Length: 1\r\n\r\n").expect_continue);
        assert!(!head("PUT / HTTP/1.0\r\nExpect: 100-continue\r\nContent-Length: 1\r\n\r\n").expect_continue);
        assert!(!head("PUT / HTTP/1.1\r\nExpect: something-else\r\n\r\n").expect_continue);
    }

    #[test]
    fn refuses_malformed_and_ambiguous_heads() {
        let long_method = format!("{} / HTTP/1.1\r\n\r\n", "M".repeat(MAX_METHOD + 1));
        let many_headers = format!("GET / HTTP/1.1\r\n{}\r\n", "X: y\r\n".repeat(MAX_HEADERS + 1));
        let cases = [
            ("GET /\r\n\r\n", Reject::BadRequest),                      // HTTP/0.9
            ("GET  / HTTP/1.1\r\n\r\n", Reject::BadRequest),            // empty target
            ("GET /a b HTTP/1.1\r\n\r\n", Reject::BadRequest),          // space in target
            ("GET /a\x7fb HTTP/1.1\r\n\r\n", Reject::BadRequest),       // DEL in target
            ("G@T / HTTP/1.1\r\n\r\n", Reject::BadRequest),             // not a token
            (" GET / HTTP/1.1\r\n\r\n", Reject::BadRequest),            // leading space
            ("\x16\x03\x01\x02\x00\x01\x00\x01", Reject::BadRequest),   // a TLS hello
            (long_method.as_str(), Reject::BadRequest),
            ("GET / HTTP/1.1\n\n", Reject::BadRequest),                 // bare LF line ends
            ("GET / http/1.1\r\n\r\n", Reject::BadRequest),
            ("GET / HTTP/1.1 \r\n\r\n", Reject::BadRequest),
            ("GET / HTTP/2.0\r\n\r\n", Reject::VersionNotSupported),
            ("GET / HTTP/0.9\r\n\r\n", Reject::VersionNotSupported),
            ("GET / HTTP/1.1\r\nHost : a\r\n\r\n", Reject::BadRequest), // space before colon
            ("GET / HTTP/1.1\r\nA: b\r\n folded\r\n\r\n", Reject::BadRequest),
            ("GET / HTTP/1.1\r\n: nameless\r\n\r\n", Reject::BadRequest),
            ("GET / HTTP/1.1\r\nNoColon\r\n\r\n", Reject::BadRequest),
            ("GET / HTTP/1.1\r\nA: b\nC: d\r\n\r\n", Reject::BadRequest),
            ("GET / HTTP/1.1\r\nA: b\rC: d\r\n\r\n", Reject::BadRequest),
            ("GET / HTTP/1.1\r\nA: b\0c\r\n\r\n", Reject::BadRequest),
            ("GET / HTTP/1.1\r\nA: b\r\n\rX", Reject::BadRequest),
            ("GET / HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\n", Reject::BadRequest),
            ("GET / HTTP/1.1\r\nContent-Length: +5\r\n\r\n", Reject::BadRequest),
            ("GET / HTTP/1.1\r\nContent-Length: 5, 5\r\n\r\n", Reject::BadRequest),
            ("GET / HTTP/1.1\r\nContent-Length:\r\n\r\n", Reject::BadRequest),
            ("GET / HTTP/1.1\r\nContent-Length: 18446744073709551616\r\n\r\n", Reject::BadRequest),
            ("POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n", Reject::BadRequest),
            ("POST / HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n", Reject::BadRequest),
            ("POST / HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\n\r\n", Reject::NotImplemented),
            ("POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n", Reject::NotImplemented),
            (many_headers.as_str(), Reject::HeadersTooLarge),
        ];
        for (raw, why) in cases {
            assert_eq!(parse_head(raw.as_bytes()), Err(why), "{raw:?}");
        }
    }

    #[test]
    fn swar_scan_agrees_with_a_plain_loop() {
        let plain = |buf: &[u8], from: usize, limit: u8| {
            (from..buf.len()).find(|&i| buf[i] < limit || buf[i] == 0x7f).unwrap_or(buf.len())
        };
        // One odd byte anywhere in buffers of every length around the eight-byte step.
        for len in 0..=25 {
            for at in 0..len {
                for byte in 0..=255u8 {
                    let mut buf = vec![b'a'; len];
                    buf[at] = byte;
                    for from in [0, at / 2, at] {
                        assert_eq!(scan::<0x20>(&buf, from), plain(&buf, from, 0x20), "{byte:#x} at {at} of {len}");
                        assert_eq!(scan::<0x21>(&buf, from), plain(&buf, from, 0x21), "{byte:#x} at {at} of {len}");
                    }
                }
            }
        }
        // Two odd bytes in one word: a borrow from the first must not hide or invent the second.
        let edges = [0x00, 0x1f, 0x20, 0x21, 0x7e, 0x7f, 0x80, 0xff];
        for (first, second) in (0..16).flat_map(|a| (a + 1..16).map(move |b| (a, b))) {
            for (x, y) in edges.iter().flat_map(|x| edges.iter().map(move |y| (*x, *y))) {
                let mut buf = [b'a'; 16];
                (buf[first], buf[second]) = (x, y);
                assert_eq!(scan::<0x20>(&buf, 0), plain(&buf, 0, 0x20), "{buf:x?}");
                assert_eq!(scan::<0x21>(&buf, 0), plain(&buf, 0, 0x21), "{buf:x?}");
            }
        }
    }

    #[test]
    fn skips_chunked_bodies_however_they_are_split() {
        let body = b"4\r\nWiki\r\n5;ext=\"v\"\r\npedia\r\nE \r\n in\r\n\r\nchunks.\r\n0\r\nTrailer: yes\r\n\r\n";
        let mut whole = body.to_vec();
        whole.extend_from_slice(b"GET /next");
        let mut state = Chunk::START;
        assert_eq!(skip_chunked(&mut state, &whole), Some((body.len(), true)));
        // The same body one byte at a time ends on the same byte.
        let mut state = Chunk::START;
        for (index, byte) in body.iter().enumerate() {
            assert_eq!(skip_chunked(&mut state, &[*byte]), Some((1, index + 1 == body.len())), "byte {index}");
        }
        // The shortest body there is, the biggest chunk there is, and framing that is broken.
        let fresh = |body: &str| skip_chunked(&mut { Chunk::START }, body.as_bytes());
        assert_eq!(fresh("0\r\n\r\n"), Some((5, true)));
        assert_eq!(fresh("ffffffffffffffff\r\nxx"), Some((20, false)));
        for broken in ["zz\r\n", "\r\n", "4\nWiki", "4\r\nWikiXX", "4\r\nWiki\rX", "11111111111111111\r\n", "0\r\nA: b\n", "0\r\n\rX"] {
            assert_eq!(fresh(broken), None, "{broken:?}");
        }
    }

    #[test]
    fn formats_http_dates() {
        let date = |unix| String::from_utf8(http_date(unix).to_vec()).unwrap();
        assert_eq!(date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(date(784_111_777), "Sun, 06 Nov 1994 08:49:37 GMT");
        assert_eq!(date(951_782_400), "Tue, 29 Feb 2000 00:00:00 GMT");
        assert_eq!(date(1_794_816_309), "Mon, 16 Nov 2026 08:05:09 GMT");
        assert_eq!(date(1_798_761_599), "Thu, 31 Dec 2026 23:59:59 GMT");
        assert_eq!(date(1_798_761_600), "Fri, 01 Jan 2027 00:00:00 GMT");
        assert_eq!(date(4_102_444_800), "Fri, 01 Jan 2100 00:00:00 GMT");
    }

    #[test]
    fn writes_responses_byte_for_byte() {
        let date = http_date(784_111_777);
        let render = |status, kind, next, body: &str, head_only| {
            let mut out = b"earlier".to_vec();
            write_response(&mut out, status, kind, next, &date, body.as_bytes(), head_only);
            String::from_utf8(out).unwrap()
        };
        assert_eq!(
            render(Status::OK, Kind::Json, Next::Open, "{\"visits\":0}", false),
            "earlierHTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 12\r\ndate: Sun, 06 Nov 1994 08:49:37 GMT\r\n\r\n{\"visits\":0}"
        );
        assert_eq!(
            render(Status::NOT_FOUND, Kind::Plain, Next::Close, "not found", false),
            "earlierHTTP/1.1 404 Not Found\r\ncontent-length: 9\r\nconnection: close\r\ndate: Sun, 06 Nov 1994 08:49:37 GMT\r\n\r\nnot found"
        );
        assert_eq!(
            render(Status::OK, Kind::Plain, Next::KeepAlive, "ok", true),
            "earlierHTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: keep-alive\r\ndate: Sun, 06 Nov 1994 08:49:37 GMT\r\n\r\n"
        );
        assert!(render(Status::OK, Kind::Plain, Next::Open, "", false).contains("content-length: 0\r\n"));
    }
}
