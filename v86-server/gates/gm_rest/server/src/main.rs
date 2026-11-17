// v86-server REST gate: the HTTP engine every v86 route runs on.
// Clients reach it at http://<computer-ip>:4000/v1/ping. No logic lives here:
// add features to ../core, then expose them with a route in `handle` below.
//
// Built for speed, from the socket up (no framework, no async runtime):
//   http.rs     reads requests and writes replies as plain bytes
//   session.rs  one connection's conversation: keep-alive, pipelining, bodies
//   reactor.rs  one event loop per CPU thread, each owning its connections
//   pool.rs     threads for slow work, so one slow route never stalls the rest
// A request is read into its thread's one buffer, answered into another and sent
// with a single write: no locks and no memory allocation on the way.

mod http;
mod pool;
mod reactor;
mod session;

use std::io::Write;
use std::net::{SocketAddr, TcpListener};

use v86_core::{DemoOrders, Pings};
use socket2::{Domain, Protocol, Socket, Type};

use crate::http::{Kind, Status};
use crate::session::{Reply, Request};

// How many connections may wait to be accepted when thousands arrive at once.
// Windows quietly cuts a plain number down to 200, which is what bounced the rest
// (they retried after ~3 s). It only gives a deep queue when asked with a negative
// number: see `listen`.
const BACKLOG: i32 = 8192;

// /v1/slow?ms=50 → 50, capped. A stand-in for slow work (a partner call, a heavy
// calculation) that must run on the pool, off the event loops.
fn slow_ms(query: Option<&str>) -> u64 {
    let mut ms = 50;
    for pair in query.unwrap_or("").split('&') {
        if let Some(("ms", Ok(v))) = pair.split_once('=').map(|(k, v)| (k, v.parse::<u64>())) {
            ms = v.min(2000);
        }
    }
    ms
}

// One counter for the whole program. A `static` needs no shared pointer, so
// requests on different cores don't fight over a reference count.
static PINGS: Pings = Pings::new();
static DEMO_ORDERS: DemoOrders = DemoOrders::new();

// Every route. `body` arrives empty: write the reply into it, then say how to send it.
// Paths are bytes exactly as the client sent them, hence the b"...".
fn handle(req: &Request, body: &mut Vec<u8>) -> Reply {
    match req.path {
        b"/v1/ping" => {
            // Written once, straight into the reply buffer: no String, no serializer.
            PINGS.ping_json(body);
            Reply::Now(Status::OK, Kind::Json)
        }
        // Demo only: a pretend exchange reply (no API key, no network) for wiring screens.
        b"/v1/demo/order" => {
            DEMO_ORDERS.place_json(body);
            Reply::Now(Status::OK, Kind::Json)
        }
        b"/v1/slow" => {
            let ms = slow_ms(req.query.and_then(|q| std::str::from_utf8(q).ok()));
            // Runs on the work pool so it never stalls /v1/ping. The pool has one thread
            // per CPU thread: more running at once would only take turns on the same cores.
            Reply::Later(Box::new(move |body| {
                std::thread::sleep(std::time::Duration::from_millis(ms));
                write!(body, r#"{{"slept_ms":{ms}}}"#).expect("writing to a Vec cannot fail");
                (Status::OK, Kind::Json)
            }))
        }
        // For the request monitor: the count, without adding one.
        b"/stats" => {
            write!(body, r#"{{"visits":{}}}"#, PINGS.count()).expect("writing to a Vec cannot fail");
            Reply::Now(Status::OK, Kind::Json)
        }
        // Dev only: speed tests start the count from 0. Remove before going public.
        b"/reset" => {
            PINGS.reset();
            body.extend_from_slice(br#"{"visits":0}"#);
            Reply::Now(Status::OK, Kind::Json)
        }
        b"/health" => {
            body.extend_from_slice(b"ok");
            Reply::Now(Status::OK, Kind::Plain)
        }
        _ => {
            body.extend_from_slice(b"not found");
            Reply::Now(Status::NOT_FOUND, Kind::Plain)
        }
    }
}

// Opens the listening socket by hand, so the queue of waiting connections can be deep.
fn listen(port: u16) -> std::io::Result<TcpListener> {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    // On Linux and macOS this lets a restarted server take its port back at once. On
    // Windows the same switch would let a second server share the port and steal
    // connections, so there it stays off and a second copy fails to start instead.
    #[cfg(unix)]
    socket.set_reuse_address(true)?;
    // 0.0.0.0 so the phone on the same Wi-Fi can reach it, not just this computer.
    socket.bind(&SocketAddr::from(([0, 0, 0, 0], port)).into())?;
    // A negative number is Windows' way to ask for exactly this many (SOMAXCONN_HINT).
    socket.listen(if cfg!(windows) { -BACKLOG } else { BACKLOG })?;
    Ok(socket.into())
}

fn env<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok()?.parse().ok()
}

fn main() {
    // A bug must stop the server loudly. Release builds already abort on a panic;
    // this makes debug builds do the same instead of quietly losing one thread.
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        std::process::abort();
    }));

    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let port: u16 = env("PORT").unwrap_or(4000);
    let config = reactor::Config {
        // THREADS=2 limits the server's cores, e.g. when a load tester shares this computer.
        threads: env("THREADS").filter(|&n| n > 0).unwrap_or(cores),
        work_threads: cores,
        // A request that takes longer than this to arrive is answered with 408.
        request_ms: env("REQUEST_MS").unwrap_or(10_000),
        // A connection that stays silent this long is closed.
        idle_ms: env("IDLE_MS").unwrap_or(75_000),
        router: handle,
    };
    let listener = listen(port).unwrap_or_else(|e| {
        eprintln!("[v86-server] cannot listen on port {port}: {e} (is another copy running?)");
        std::process::exit(1);
    });
    // PORT=0 lets the system pick a free port (the tests do this), so print the real one.
    let port = listener.local_addr().map_or(port, |addr| addr.port());
    let _ = writeln!(std::io::stdout(), "[v86-server] listening on http://0.0.0.0:{port}/v1/ping");

    let served = reactor::serve(listener, config, |server| {
        // Ctrl+C once: stop accepting, finish what is in flight, exit. Twice: exit now.
        let _ = ctrlc::set_handler(move || {
            if server.shutdown() {
                std::process::exit(130);
            }
        });
    });
    if let Err(e) = served {
        eprintln!("[v86-server] stopped: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_ms_has_a_default_and_a_limit() {
        assert_eq!(slow_ms(None), 50);
        assert_eq!(slow_ms(Some("ms=7")), 7);
        assert_eq!(slow_ms(Some("x=1&ms=999999")), 2000);
        assert_eq!(slow_ms(Some("ms=abc")), 50);
    }

    #[test]
    fn routes_answer_what_clients_expect() {
        let ask = |path: &'static [u8]| {
            let mut body = Vec::new();
            let reply = handle(&Request { method: b"GET", path, query: None }, &mut body);
            (matches!(reply, Reply::Now(Status::OK, Kind::Json)), String::from_utf8(body).unwrap())
        };
        assert!(ask(b"/v1/demo/order").1.starts_with(r#"{"demo":true,"#));
        assert_eq!(ask(b"/reset"), (true, r#"{"visits":0}"#.to_string()));
        let (json, ping) = ask(b"/v1/ping");
        assert!(json && ping.starts_with(r#"{"ok":true,"count":1,"served_at":"#), "{ping}");
        assert!(ask(b"/stats").1.starts_with(r#"{"visits":"#));
        assert_eq!(ask(b"/health"), (false, "ok".to_string()));
        assert_eq!(ask(b"/nope"), (false, "not found".to_string()));
        assert!(matches!(handle(&Request { method: b"GET", path: b"/v1/slow", query: None }, &mut Vec::new()), Reply::Later(_)));
    }
}
