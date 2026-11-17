// One connection's HTTP conversation as a state machine: request bytes in, response
// bytes out. It never touches a socket, so every protocol rule here is tested with
// plain byte slices.

use crate::http::{parse_head, skip_chunked, write_response, Chunk, Framing, Kind, Next, Reject, Status, MAX_HEAD};

/// Largest request body the server will read past. No route uses a body, so this only has
/// to cover clients that send one anyway.
pub const MAX_BODY: u64 = 1024 * 1024;
/// Once this much output is waiting, stop parsing and let the socket catch up.
pub const HIGH_WATER: usize = 64 * 1024;

/// What a route gets to see of a request: raw bytes, straight from the wire.
pub struct Request<'a> {
    #[allow(dead_code)] // no route needs it yet: as before, the path alone picks the route
    pub method: &'a [u8],
    pub path: &'a [u8],
    pub query: Option<&'a [u8]>,
}

/// What a route decides.
pub enum Reply {
    /// The body buffer holds the answer: send it with this status.
    Now(Status, Kind),
    /// Too slow for an I/O thread: run this on the work pool, then send what it wrote.
    Later(Job),
}

/// Heavy work for the pool. It fills the body buffer and says how to send it.
pub type Job = Box<dyn FnOnce(&mut Vec<u8>) -> (Status, Kind) + Send>;

/// The one function that maps requests to replies (`route` in `main.rs`).
pub type Router = fn(&Request<'_>, &mut Vec<u8>) -> Reply;

/// What `feed` borrows from the thread it runs on.
pub struct Cx<'a> {
    /// Responses are appended here.
    pub out: &'a mut Vec<u8>,
    /// Scratch space a route writes its body into.
    pub body: &'a mut Vec<u8>,
    /// The current `date` header value.
    pub date: &'a [u8; 29],
    pub router: Router,
    /// True once the server is shutting down: answer, then close.
    pub draining: bool,
    /// Where `Reply::Later` jobs go.
    pub offload: &'a mut dyn FnMut(Job),
}

/// Why `feed` returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// Every usable byte was consumed: read more.
    NeedMore,
    /// Plenty of output is waiting: send it, then call again.
    Flush,
    /// A job went to the work pool: call `finish` when it is done.
    Parked,
    /// The last response ends the connection: send it, then hang up.
    Close,
}

/// What the next input bytes are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Input {
    /// A request head.
    Head,
    /// A body of known length, to be skipped: bytes left.
    Body(u64),
    /// A chunked body, to be skipped: where in it, and how many more bytes it may take.
    Chunked(Chunk, u64),
    /// Nothing: the conversation is over. True if it ended exactly at the end of a
    /// request, with no unread body and no further bytes behind it.
    Closed(bool),
}

/// The protocol state of one connection.
pub struct Session {
    input: Input,
    /// While a job is on the pool: how its response must be sent (connection, HEAD or not).
    parked: Option<(Next, bool)>,
    served: u64,
}

impl Default for Session {
    fn default() -> Session {
        Session { input: Input::Head, parked: None, served: 0 }
    }
}

impl Session {
    /// Requests answered so far.
    pub fn served(&self) -> u64 {
        self.served
    }

    /// True while a job is on the work pool. No input is consumed until it returns, so
    /// responses leave in the order their requests arrived.
    pub fn is_parked(&self) -> bool {
        self.parked.is_some()
    }

    /// True while the rest of a request body is still expected.
    pub fn in_body(&self) -> bool {
        matches!(self.input, Input::Body(_) | Input::Chunked(..))
    }

    /// True if the conversation ended with nothing left unread, so the socket can simply
    /// be closed without risking a reset that would destroy the last response.
    pub fn closed_cleanly(&self) -> bool {
        self.input == Input::Closed(true)
    }

    /// Consumes as much of `data` as it can, appending responses to `cx.out`. Returns how
    /// many bytes were used and why it stopped; unused bytes must be offered again later.
    pub fn feed(&mut self, data: &[u8], cx: &mut Cx<'_>) -> (usize, Stop) {
        let mut pos = 0;
        loop {
            if self.parked.is_some() {
                return (pos, Stop::Parked);
            }
            match self.input {
                Input::Closed(_) => return (data.len(), Stop::Close),
                Input::Body(left) => {
                    let take = left.min((data.len() - pos) as u64);
                    pos += take as usize;
                    if take < left {
                        self.input = Input::Body(left - take);
                        return (pos, Stop::NeedMore);
                    }
                    self.input = Input::Head;
                }
                Input::Chunked(mut chunk, budget) => match skip_chunked(&mut chunk, &data[pos..]) {
                    Some((used, true)) => {
                        pos += used;
                        self.input = Input::Head;
                    }
                    Some((used, false)) if used as u64 <= budget => {
                        self.input = Input::Chunked(chunk, budget - used as u64);
                        return (data.len(), Stop::NeedMore);
                    }
                    // Broken framing or an endless body, after the answer already went out:
                    // there is nothing left to say, only to hang up.
                    _ => {
                        self.input = Input::Closed(false);
                        return (data.len(), Stop::Close);
                    }
                },
                Input::Head => {
                    // Blank lines before a request line are legal noise.
                    while data[pos..].starts_with(b"\r\n") {
                        pos += 2;
                    }
                    let rest = &data[pos..];
                    if rest.is_empty() || rest == b"\r" {
                        return (pos, Stop::NeedMore);
                    }
                    if cx.out.len() >= HIGH_WATER {
                        return (pos, Stop::Flush);
                    }
                    let head = match parse_head(rest) {
                        Ok(Some(head)) if head.len <= MAX_HEAD => head,
                        Ok(None) if rest.len() < MAX_HEAD => return (pos, Stop::NeedMore),
                        // Too long. Blame the headers if the request line at least ended.
                        Ok(_) if rest[..MAX_HEAD].contains(&b'\n') => return self.refuse(Reject::HeadersTooLarge, data, cx),
                        Ok(_) => return self.refuse(Reject::UriTooLong, data, cx),
                        Err(reject) => return self.refuse(reject, data, cx),
                    };
                    pos += head.len;
                    let after = match head.body {
                        Framing::None => Input::Head,
                        Framing::Length(n) if n > MAX_BODY => return self.refuse(Reject::PayloadTooLarge, data, cx),
                        Framing::Length(n) => Input::Body(n),
                        Framing::Chunked => Input::Chunked(Chunk::START, MAX_BODY),
                    };
                    if head.expect_continue && after != Input::Head {
                        cx.out.extend_from_slice(b"HTTP/1.1 100 Continue\r\n\r\n");
                    }
                    let next = match (head.keep_alive && !cx.draining, head.http11) {
                        (false, _) => Next::Close,
                        (true, true) => Next::Open,
                        (true, false) => Next::KeepAlive,
                    };
                    let head_only = head.method == b"HEAD";
                    let request = Request { method: head.method, path: head.path, query: head.query };
                    cx.body.clear();
                    self.input = after;
                    let (status, kind) = match (cx.router)(&request, cx.body) {
                        Reply::Now(status, kind) => (status, kind),
                        Reply::Later(job) => {
                            self.parked = Some((next, head_only));
                            (cx.offload)(job);
                            return (pos, Stop::Parked);
                        }
                    };
                    write_response(cx.out, status, kind, next, cx.date, cx.body, head_only);
                    self.served += 1;
                    if next == Next::Close {
                        self.input = Input::Closed(after == Input::Head && pos == data.len());
                        return (data.len(), Stop::Close);
                    }
                }
            }
        }
    }

    /// Writes the response of the parked request, now that its job is done. Returns true
    /// if the connection must close once it is sent.
    pub fn finish(&mut self, status: Status, kind: Kind, body: &[u8], out: &mut Vec<u8>, date: &[u8; 29], draining: bool) -> bool {
        let Some((next, head_only)) = self.parked.take() else { return false };
        let next = if draining { Next::Close } else { next };
        write_response(out, status, kind, next, date, body, head_only);
        self.served += 1;
        if next == Next::Close {
            self.input = Input::Closed(self.input == Input::Head);
        }
        next == Next::Close
    }

    /// The client took too long to finish a request: say so, and end the conversation.
    pub fn time_out(&mut self, out: &mut Vec<u8>, date: &[u8; 29]) {
        let (status, text) = Reject::Timeout.reply();
        write_response(out, status, Kind::Plain, Next::Close, date, text.as_bytes(), false);
        self.input = Input::Closed(false);
    }

    /// Answers a request that cannot be served and ends the conversation: after a framing
    /// error there is no telling where the next request would start.
    fn refuse(&mut self, why: Reject, data: &[u8], cx: &mut Cx<'_>) -> (usize, Stop) {
        let (status, text) = why.reply();
        write_response(cx.out, status, Kind::Plain, Next::Close, cx.date, text.as_bytes(), false);
        self.input = Input::Closed(false);
        (data.len(), Stop::Close)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATE: &[u8; 29] = b"Sun, 06 Nov 1994 08:49:37 GMT";
    const GET_A: &[u8] = b"GET /a HTTP/1.1\r\n\r\n";
    const CLOSE: &str = "connection: close\r\n";

    fn router(request: &Request<'_>, body: &mut Vec<u8>) -> Reply {
        match request.path {
            b"/a" => body.extend_from_slice(b"A"),
            b"/echo" => {
                body.extend_from_slice(request.method);
                body.push(b' ');
                body.extend_from_slice(request.query.unwrap_or(b"-"));
            }
            b"/later" => {
                return Reply::Later(Box::new(|body| {
                    body.extend_from_slice(b"L");
                    (Status::OK, Kind::Json)
                }))
            }
            _ => {
                body.extend_from_slice(b"not found");
                return Reply::Now(Status::NOT_FOUND, Kind::Plain);
            }
        }
        Reply::Now(Status::OK, Kind::Plain)
    }

    /// Plays the reactor: keeps unconsumed bytes, collects output, holds parked jobs.
    #[derive(Default)]
    struct Rig {
        session: Session,
        pending: Vec<u8>,
        wire: Vec<u8>,
        jobs: Vec<Job>,
        draining: bool,
    }

    impl Rig {
        /// Hands the session more bytes, as after a socket read.
        fn deliver(&mut self, bytes: &[u8]) -> Stop {
            self.pending.extend_from_slice(bytes);
            loop {
                let (mut out, mut body) = (Vec::new(), Vec::new());
                let jobs = &mut self.jobs;
                let mut offload = |job| jobs.push(job);
                let mut cx = Cx { out: &mut out, body: &mut body, date: DATE, router, draining: self.draining, offload: &mut offload };
                let (used, stop) = self.session.feed(&self.pending, &mut cx);
                self.pending.drain(..used);
                self.wire.extend_from_slice(&out);
                if stop != Stop::Flush {
                    return stop;
                }
            }
        }

        /// Runs the oldest parked job and finishes its request, as when the pool reports back.
        fn complete(&mut self) -> bool {
            let mut body = Vec::new();
            let (status, kind) = (self.jobs.remove(0))(&mut body);
            self.session.finish(status, kind, &body, &mut self.wire, DATE, self.draining)
        }

        fn wire(&mut self) -> String {
            String::from_utf8(std::mem::take(&mut self.wire)).unwrap()
        }
    }

    /// The exact bytes of one response. `connection` is empty or a whole header line.
    fn reply(status: &str, body: &str, connection: &str) -> String {
        format!("HTTP/1.1 {status}\r\ncontent-length: {}\r\n{connection}date: Sun, 06 Nov 1994 08:49:37 GMT\r\n\r\n{body}", body.len())
    }

    const LATER: &str = "200 OK\r\ncontent-type: application/json";

    #[test]
    fn answers_pipelined_requests_in_order() {
        let mut rig = Rig::default();
        let stop = rig.deliver(b"GET /a HTTP/1.1\r\n\r\nPOST /echo?q=1 HTTP/1.1\r\nHost: x\r\n\r\nGET /nope HTTP/1.1\r\n\r\nGET /a HT");
        assert_eq!(stop, Stop::NeedMore);
        assert_eq!(rig.wire(), reply("200 OK", "A", "") + &reply("200 OK", "POST q=1", "") + &reply("404 Not Found", "not found", ""));
        assert_eq!((rig.pending.as_slice(), rig.session.served()), (&b"GET /a HT"[..], 3));
        assert_eq!(rig.deliver(b"TP/1.1\r\n\r\n"), Stop::NeedMore);
        assert_eq!(rig.wire(), reply("200 OK", "A", ""));
        assert!(rig.pending.is_empty() && !rig.session.in_body() && !rig.session.is_parked());
    }

    #[test]
    fn output_is_the_same_however_the_bytes_arrive() {
        let input: &[u8] = b"\r\nGET /a HTTP/1.1\r\nHost: x\r\n\r\n\
            POST /echo HTTP/1.1\r\nContent-Length: 11\r\n\r\nhello world\r\n\
            POST /echo?c HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n\
            HEAD /a HTTP/1.0\r\nConnection: keep-alive\r\n\r\n\
            GET /later HTTP/1.1\r\n\r\n\
            GET /a HTTP/1.1\r\nConnection: close\r\n\r\n";
        let run = |step: usize| {
            let mut rig = Rig::default();
            let mut stop = Stop::NeedMore;
            for piece in input.chunks(step) {
                stop = rig.deliver(piece);
                while stop == Stop::Parked && !rig.jobs.is_empty() {
                    assert!(!rig.complete());
                    stop = rig.deliver(b"");
                }
            }
            (rig.wire(), stop)
        };
        let expected = reply("200 OK", "A", "")
            + &reply("200 OK", "POST -", "")
            + &reply("200 OK", "POST c", "")
            + reply("200 OK", "A", "connection: keep-alive\r\n").strip_suffix('A').unwrap()
            + &reply(LATER, "L", "")
            + &reply("200 OK", "A", CLOSE);
        for step in [input.len(), 1, 2, 3, 7, 16, 61] {
            assert_eq!(run(step), (expected.clone(), Stop::Close), "delivered {step} bytes at a time");
        }
    }

    #[test]
    fn a_parked_request_holds_the_line_until_its_job_returns() {
        let mut rig = Rig::default();
        assert_eq!(rig.deliver(&[b"GET /later HTTP/1.1\r\n\r\n", GET_A].concat()), Stop::Parked);
        assert_eq!((rig.wire(), rig.jobs.len()), (String::new(), 1));
        // Nothing more is consumed while the job runs.
        assert_eq!(rig.deliver(GET_A), Stop::Parked);
        assert!(rig.session.is_parked() && rig.pending.len() == 2 * GET_A.len());
        assert!(!rig.complete());
        assert_eq!(rig.deliver(b""), Stop::NeedMore);
        assert_eq!(rig.wire(), reply(LATER, "L", "") + &reply("200 OK", "A", "").repeat(2));
        assert_eq!(rig.session.served(), 3);
    }

    #[test]
    fn closes_when_asked_and_ignores_what_follows() {
        let mut rig = Rig::default();
        assert_eq!(rig.deliver(b"GET /a HTTP/1.1\r\nConnection: close\r\n\r\n"), Stop::Close);
        assert!(rig.session.closed_cleanly());
        assert_eq!(rig.wire(), reply("200 OK", "A", CLOSE));
        assert_eq!(rig.deliver(GET_A), Stop::Close);
        assert!(rig.wire().is_empty() && rig.pending.is_empty());

        // HTTP/1.0 closes unless asked to stay. Bytes behind the last request, or a body
        // that was never read, make the close unclean.
        let mut rig = Rig::default();
        assert_eq!(rig.deliver(b"GET /a HTTP/1.0\r\n\r\nGET /a HTTP/1.0\r\n\r\n"), Stop::Close);
        assert!(!rig.session.closed_cleanly());
        assert_eq!(rig.wire(), reply("200 OK", "A", CLOSE));
        let mut rig = Rig::default();
        assert_eq!(rig.deliver(b"POST /a HTTP/1.0\r\nContent-Length: 3\r\n\r\n"), Stop::Close);
        assert!(!rig.session.closed_cleanly());
    }

    #[test]
    fn a_draining_server_closes_after_the_next_answer() {
        let mut rig = Rig { draining: true, ..Rig::default() };
        assert_eq!(rig.deliver(&GET_A.repeat(2)), Stop::Close);
        assert_eq!(rig.wire(), reply("200 OK", "A", CLOSE));

        let mut rig = Rig::default();
        assert_eq!(rig.deliver(b"HEAD /later HTTP/1.1\r\n\r\n"), Stop::Parked);
        rig.draining = true;
        assert!(rig.complete() && rig.session.closed_cleanly());
        assert_eq!(rig.wire(), reply(LATER, "L", CLOSE).strip_suffix('L').unwrap());
    }

    #[test]
    fn sends_100_continue_before_reading_a_body() {
        let mut rig = Rig::default();
        assert_eq!(rig.deliver(b"POST /a HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 4\r\n\r\n"), Stop::NeedMore);
        assert_eq!(rig.wire(), format!("HTTP/1.1 100 Continue\r\n\r\n{}", reply("200 OK", "A", "")));
        assert!(rig.session.in_body());
        assert_eq!(rig.deliver(b"body"), Stop::NeedMore);
        assert!(!rig.session.in_body() && rig.wire().is_empty());
        // With no body there is nothing to wait for, so nothing extra is said.
        rig.deliver(b"GET /a HTTP/1.1\r\nExpect: 100-continue\r\n\r\n");
        assert_eq!(rig.wire(), reply("200 OK", "A", ""));
    }

    #[test]
    fn refuses_bad_input_and_closes() {
        let refusal = |input: &[u8]| {
            let mut rig = Rig::default();
            assert_eq!(rig.deliver(input), Stop::Close);
            assert!(!rig.session.closed_cleanly() && rig.pending.is_empty());
            rig.wire()
        };
        let bad = reply("400 Bad Request", "bad request", CLOSE);
        assert_eq!(refusal(b"GET / HTTP/1.1\r\nBad Header\r\n\r\n"), bad);
        assert_eq!(refusal(b"GET /a HTTP/1.1\r\n\r\nNOPE\r\n\r\n"), reply("200 OK", "A", "") + &bad);
        assert!(refusal(b"GET / HTTP/3.0\r\n\r\n").starts_with("HTTP/1.1 505 "));
        assert!(refusal(b"POST / HTTP/1.1\r\nContent-Length: 1048577\r\n\r\n").starts_with("HTTP/1.1 413 "));
        assert!(refusal(b"POST / HTTP/1.1\r\nTransfer-Encoding: gzip\r\n\r\n").starts_with("HTTP/1.1 501 "));
        // Size limits: an endless request line is 414, endless headers are 431, whether or
        // not the head ever completes.
        let long_line = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(MAX_HEAD));
        assert!(refusal(long_line.as_bytes()).starts_with("HTTP/1.1 414 "));
        assert!(refusal(&long_line.as_bytes()[..MAX_HEAD]).starts_with("HTTP/1.1 414 "));
        let long_headers = format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "y".repeat(MAX_HEAD));
        assert!(refusal(long_headers.as_bytes()).starts_with("HTTP/1.1 431 "));
        assert!(refusal(&long_headers.as_bytes()[..MAX_HEAD]).starts_with("HTTP/1.1 431 "));
        // One byte under the limit is still fine.
        let mut rig = Rig::default();
        let fits = format!("GET /a HTTP/1.1\r\nX: {}\r\n\r\n", "y".repeat(MAX_HEAD - 24));
        assert_eq!((fits.len(), rig.deliver(fits.as_bytes())), (MAX_HEAD, Stop::NeedMore));
        assert_eq!(rig.wire(), reply("200 OK", "A", ""));
    }

    #[test]
    fn a_broken_or_endless_chunked_body_just_hangs_up() {
        let mut rig = Rig::default();
        assert_eq!(rig.deliver(b"POST /a HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nnot-hex\r\n"), Stop::Close);
        assert_eq!(rig.wire(), reply("200 OK", "A", ""));
        assert!(!rig.session.closed_cleanly());

        let mut rig = Rig::default();
        assert_eq!(rig.deliver(b"POST /a HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nfffffffff\r\n"), Stop::NeedMore);
        let stops: Vec<Stop> = (0..17).map(|_| rig.deliver(&[b'x'; 64 * 1024])).collect();
        assert!(stops[..15].iter().all(|stop| *stop == Stop::NeedMore), "{stops:?}");
        assert_eq!(stops[15..], [Stop::Close, Stop::Close]);
    }

    #[test]
    fn pauses_for_the_socket_when_output_piles_up() {
        let input = GET_A.repeat(2_000);
        let one = reply("200 OK", "A", "").len();
        let (mut out, mut body, mut session) = (Vec::new(), Vec::new(), Session::default());
        let mut cx = Cx { out: &mut out, body: &mut body, date: DATE, router, draining: false, offload: &mut |_| {} };
        let (used, stop) = session.feed(&input, &mut cx);
        assert_eq!(stop, Stop::Flush);
        assert!(used < input.len() && used % GET_A.len() == 0);
        assert_eq!(out.len(), used / GET_A.len() * one);
        assert!(out.len() >= HIGH_WATER && out.len() < HIGH_WATER + one);
        let mut rig = Rig::default();
        assert_eq!(rig.deliver(&input), Stop::NeedMore);
        assert_eq!(rig.wire().len(), 2_000 * one);
    }

    #[test]
    fn blank_lines_between_requests_are_noise() {
        let mut rig = Rig::default();
        assert_eq!(rig.deliver(b"\r\n\r\n\r"), Stop::NeedMore);
        assert_eq!(rig.pending, b"\r");
        assert_eq!(rig.deliver(&[b"\n", GET_A].concat()), Stop::NeedMore);
        assert_eq!(rig.wire(), reply("200 OK", "A", ""));
    }

    #[test]
    fn a_request_that_never_finishes_gets_408() {
        let mut rig = Rig::default();
        assert_eq!(rig.deliver(b"GET /a HTTP/1.1\r\nHost: slow"), Stop::NeedMore);
        rig.session.time_out(&mut rig.wire, DATE);
        assert_eq!(rig.wire(), reply("408 Request Timeout", "request timeout", CLOSE));
        assert_eq!(rig.deliver(b"\r\n\r\n"), Stop::Close);
        assert!(rig.wire().is_empty() && !rig.session.closed_cleanly());
    }
}
