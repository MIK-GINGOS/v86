// Black-box tests: start the real v86_server program on a free port and talk to it over
// TCP byte for byte, the way a phone, a load tester or a broken client would.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const HELLO: &str = "GET /v1/ping HTTP/1.1\r\n\r\n";

struct Server {
    child: Child,
    port: u16,
    /// Held open so the server always has somewhere to print.
    _stdout: BufReader<ChildStdout>,
}

impl Server {
    /// Starts the server on a port the system picks, with two I/O threads unless told otherwise.
    fn start(settings: &[(&str, &str)]) -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_v86_server"))
            .envs([("PORT", "0"), ("THREADS", "2")])
            .envs(settings.iter().copied())
            .stdout(Stdio::piped())
            .spawn()
            .expect("the server starts");
        let mut stdout = BufReader::new(child.stdout.take().expect("piped"));
        let mut line = String::new();
        stdout.read_line(&mut line).expect("a first line");
        let port = line.trim().trim_end_matches("/v1/ping").rsplit(':').next().and_then(|port| port.parse().ok());
        Server { child, port: port.unwrap_or_else(|| panic!("no port in {line:?}")), _stdout: stdout }
    }

    fn connect(&self) -> Client {
        let stream = TcpStream::connect(("127.0.0.1", self.port)).expect("connects");
        stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        stream.set_nodelay(true).unwrap();
        Client { stream: BufReader::new(stream) }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Client {
    stream: BufReader<TcpStream>,
}

struct Response {
    /// The status line and headers, exactly as sent.
    head: String,
    body: String,
}

impl Client {
    fn send(&mut self, bytes: impl AsRef<[u8]>) -> &mut Client {
        self.stream.get_mut().write_all(bytes.as_ref()).expect("the server takes the bytes");
        self
    }

    /// Reads one response: the head up to its blank line, then `content-length` bytes of
    /// body (none when it answers a HEAD).
    fn read(&mut self, head_only: bool) -> Response {
        let mut head = String::new();
        while !head.ends_with("\r\n\r\n") {
            assert!(self.stream.read_line(&mut head).expect("a response in time") > 0, "closed after {head:?}");
        }
        let length = head.lines().find_map(|line| line.strip_prefix("content-length: ")).map_or(0, |n| n.parse().unwrap());
        let mut body = vec![0; if head_only { 0 } else { length }];
        self.stream.read_exact(&mut body).expect("the whole body");
        Response { head, body: String::from_utf8(body).unwrap() }
    }

    fn get(&mut self, path: &str) -> Response {
        self.send(format!("GET {path} HTTP/1.1\r\nHost: test\r\n\r\n")).read(false)
    }

    /// True if the server closes the connection without sending anything more.
    fn closed(&mut self) -> bool {
        matches!(self.stream.read(&mut [0; 64]), Ok(0))
    }
}

/// The status line and header names of a response, in order.
fn outline(response: &Response) -> Vec<&str> {
    response.head.trim_end().lines().map(|line| line.split(": ").next().unwrap()).collect()
}

#[test]
fn every_route_answers_as_expected() {
    let server = Server::start(&[]);
    let mut client = server.connect();
    let first = client.get("/v1/ping");
    assert_eq!(outline(&first), ["HTTP/1.1 200 OK", "content-type", "content-length", "date"]);
    assert!(first.head.contains("content-type: application/json\r\ncontent-length: 44\r\ndate: ") && first.head.ends_with(" GMT\r\n\r\n"));
    assert!(first.body.starts_with(r#"{"ok":true,"count":1,"served_at":17"#), "{}", first.body);
    assert!(client.get("/v1/ping").body.contains(r#""count":2,"#));
    assert_eq!(client.get("/stats").body, r#"{"visits":2}"#);
    assert_eq!(client.get("/reset").body, r#"{"visits":0}"#);
    assert_eq!(client.get("/stats").body, r#"{"visits":0}"#);
    let health = client.get("/health");
    assert_eq!((outline(&health), health.body.as_str()), (vec!["HTTP/1.1 200 OK", "content-length", "date"], "ok"));
    let missing = client.get("/nope?x=1");
    assert_eq!((outline(&missing), missing.body.as_str()), (vec!["HTTP/1.1 404 Not Found", "content-length", "date"], "not found"));
    // As before, the path alone picks the route: any method, any query, even a full URL.
    let odd = client.send("POST http://phone.local:4000/v1/ping?x=1#top HTTP/1.1\r\nContent-Length: 4\r\n\r\nbody").read(false);
    assert!(odd.body.contains(r#""count":1,"#), "{}", odd.body);
    let slow = client.get("/v1/slow?ms=5");
    assert_eq!(outline(&slow), ["HTTP/1.1 200 OK", "content-type", "content-length", "date"]);
    assert_eq!(slow.body, r#"{"slept_ms":5}"#);
}

#[test]
fn pipelined_requests_are_answered_in_order() {
    let server = Server::start(&[]);
    let mut client = server.connect();
    client.send(HELLO.repeat(5_000));
    for visit in 1..=5_000 {
        let reply = client.read(false);
        assert!(reply.body.contains(&format!(r#""count":{visit},"#)), "reply {visit} was {}", reply.body);
    }
    assert_eq!(server.connect().get("/stats").body, r#"{"visits":5000}"#);
}

#[test]
fn connections_close_exactly_when_they_should() {
    let server = Server::start(&[]);
    // HTTP/1.1 stays open until the client asks to close.
    let mut client = server.connect();
    assert!(!client.get("/health").head.contains("connection:"));
    let last = client.send("GET /health HTTP/1.1\r\nConnection: close\r\n\r\n").read(false);
    assert!(last.head.contains("\r\nconnection: close\r\n") && last.body == "ok" && client.closed());
    // HTTP/1.0 closes unless the client asks to stay.
    let mut old = server.connect();
    let kept = old.send("GET /health HTTP/1.0\r\nConnection: keep-alive\r\n\r\n").read(false);
    assert!(kept.head.starts_with("HTTP/1.1 200 OK\r\n") && kept.head.contains("\r\nconnection: keep-alive\r\n"));
    let gone = old.send("GET /health HTTP/1.0\r\n\r\n").read(false);
    assert!(gone.head.contains("\r\nconnection: close\r\n") && old.closed());
    // A client that finishes sending before it reads still gets its answer.
    let mut half = server.connect();
    half.send("GET /health HTTP/1.1\r\n\r\n").stream.get_ref().shutdown(Shutdown::Write).unwrap();
    assert!(half.read(false).body == "ok" && half.closed());
}

#[test]
fn head_requests_and_request_bodies_keep_the_connection_in_step() {
    let server = Server::start(&[]);
    let mut client = server.connect();
    assert!(client.send("HEAD /health HTTP/1.1\r\n\r\n").read(true).head.contains("content-length: 2\r\n"));
    // Bodies are skipped, however they are framed, and the next request is found behind them.
    client.send("POST /health HTTP/1.1\r\nContent-Length: 11\r\n\r\nhello worldGET /health HTTP/1.1\r\n\r\n");
    assert_eq!((client.read(false).body, client.read(false).body), ("ok".to_string(), "ok".to_string()));
    client.send("POST /health HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6;x=y\r\n world\r\n0\r\nT: 1\r\n\r\nGET /nope HTTP/1.1\r\n\r\n");
    assert_eq!((client.read(false).body, client.read(false).body), ("ok".to_string(), "not found".to_string()));
    // A client that waits for permission before sending its body gets it.
    client.send("POST /health HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 5\r\n\r\n");
    assert_eq!(client.read(false).head, "HTTP/1.1 100 Continue\r\n\r\n");
    assert_eq!(client.send("hello").read(false).body, "ok");
    assert_eq!(client.get("/health").body, "ok");
}

#[test]
fn a_request_arriving_one_byte_at_a_time_is_still_answered() {
    let server = Server::start(&[]);
    let mut client = server.connect();
    for byte in b"GET /v1/ping HTTP/1.1\r\nHost: slow\r\n\r\nGET /health HTTP/1.1\r\n\r\n" {
        client.send([*byte]);
        thread::sleep(Duration::from_millis(1));
    }
    assert!(client.read(false).body.contains(r#""count":1,"#));
    assert_eq!(client.read(false).body, "ok");
}

#[test]
fn broken_requests_get_an_error_and_a_closed_connection() {
    let server = Server::start(&[]);
    for (request, status) in [
        ("GET /v1/ping\r\n\r\n", "400 Bad Request"),
        ("GET /v1/ping HTTP/1.1\r\nBad Header\r\n\r\n", "400 Bad Request"),
        ("GET /v1/ping HTTP/2.0\r\n\r\n", "505 HTTP Version Not Supported"),
        ("POST /v1/ping HTTP/1.1\r\nContent-Length: 99999999\r\n\r\n", "413 Payload Too Large"),
        ("POST /v1/ping HTTP/1.1\r\nTransfer-Encoding: gzip\r\n\r\n", "501 Not Implemented"),
    ] {
        let mut client = server.connect();
        // More bytes follow the bad request. The error must still arrive whole.
        let reply = client.send(request).send("junk the server will never read").read(false);
        assert!(reply.head.starts_with(&format!("HTTP/1.1 {status}\r\n")), "{request:?} got {}", reply.head);
        assert!(reply.head.contains("\r\nconnection: close\r\n") && client.closed(), "{request:?}");
    }
    let mut flood = server.connect();
    let reply = flood.send(format!("GET / HTTP/1.1\r\nX: {}", "y".repeat(20_000))).read(false);
    assert!(reply.head.starts_with("HTTP/1.1 431 ") && flood.closed());
    // None of it counted as a visit, and the server is still in good health.
    assert_eq!(server.connect().get("/stats").body, r#"{"visits":0}"#);
}

#[test]
fn slow_work_never_stalls_other_routes_even_on_one_thread() {
    let server = Server::start(&[("THREADS", "1")]);
    let (mut worker, mut phone) = (server.connect(), server.connect());
    let started = Instant::now();
    // A slow request, with a /v1/ping queued right behind it on the same connection.
    worker.send(format!("GET /v1/slow?ms=400 HTTP/1.1\r\n\r\n{HELLO}"));
    let quick = phone.get("/health");
    let health_took = started.elapsed();
    let work = worker.read(false);
    let work_took = started.elapsed();
    assert_eq!(quick.body, "ok");
    assert_eq!(work.body, r#"{"slept_ms":400}"#);
    assert!(health_took < work_took, "/health took {health_took:?} while /v1/slow took {work_took:?}");
    // Replies on one connection keep their order: /v1/ping waited for /v1/slow.
    assert!(worker.read(false).body.contains(r#""count":1,"#));
}

#[test]
fn slow_and_silent_clients_are_timed_out_but_busy_ones_are_not() {
    let server = Server::start(&[("REQUEST_MS", "300"), ("IDLE_MS", "700")]);
    let (mut slow, mut silent, mut busy) = (server.connect(), server.connect(), server.connect());
    slow.send("GET /v1/ping HTTP/1.1\r\nHost: never-finished");
    // Regular requests keep the busy connection open for longer than the idle limit.
    for _ in 0..6 {
        assert_eq!(busy.get("/health").body, "ok");
        thread::sleep(Duration::from_millis(200));
    }
    let reply = slow.read(false);
    assert!(reply.head.starts_with("HTTP/1.1 408 Request Timeout\r\n") && reply.body == "request timeout" && slow.closed());
    assert!(silent.closed(), "a silent connection is closed without a word");
    assert_eq!(busy.get("/health").body, "ok");
}

#[test]
fn a_client_that_stops_reading_is_waited_for_and_loses_nothing() {
    let server = Server::start(&[]);
    let mut client = server.connect();
    let total = 100_000;
    let mut sender = client.stream.get_ref().try_clone().unwrap();
    let writer = thread::spawn(move || sender.write_all(HELLO.repeat(total).as_bytes()).unwrap());
    // Let replies pile up until the server has to stop and wait for us.
    thread::sleep(Duration::from_millis(500));
    let (mut seen, mut matched, mut buf) = (0, 0, vec![0u8; 65_536]);
    while seen < total {
        let n = client.stream.read(&mut buf).expect("replies keep coming");
        assert!(n > 0, "closed after {seen} replies");
        // Count replies by the blank line that ends each head (the JSON never has one).
        for byte in &buf[..n] {
            matched = match (matched, byte) {
                (0 | 2, b'\r') | (1 | 3, b'\n') => matched + 1,
                (_, b'\r') => 1,
                _ => 0,
            };
            if matched == 4 {
                (seen, matched) = (seen + 1, 0);
            }
        }
    }
    writer.join().unwrap();
    assert_eq!(server.connect().get("/stats").body, format!(r#"{{"visits":{total}}}"#));
}

#[test]
fn a_thousand_connections_at_once_are_all_answered() {
    let server = Server::start(&[]);
    let mut clients: Vec<Client> = (0..1_000).map(|_| server.connect()).collect();
    for client in &mut clients {
        client.send(HELLO);
    }
    for client in &mut clients {
        assert!(client.read(false).head.starts_with("HTTP/1.1 200 OK\r\n"));
    }
    assert_eq!(clients[0].get("/stats").body, r#"{"visits":1000}"#);
}
