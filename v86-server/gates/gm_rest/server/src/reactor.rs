// The I/O engine. One event loop per thread, and each loop owns its connections
// outright, so the request path takes no locks and allocates nothing: a request is
// read into the thread's one hot buffer, parsed in place, answered into a second
// buffer and sent with a single write. Only what cannot finish in one go (a request
// split across packets, a client slow to read) parks bytes on its connection.

use std::io::{self, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token, Waker};

use crate::http::{http_date, Kind, Status};
use crate::pool::{Pool, Task};
use crate::session::{Cx, Job, Router, Session, Stop};

/// What the engine is told at startup.
#[derive(Clone, Copy)]
pub struct Config {
    /// I/O threads, each running its own event loop.
    pub threads: usize,
    /// Threads for heavy jobs: at most this many run at once.
    pub work_threads: usize,
    /// A request must arrive in full within this many milliseconds of its first byte.
    pub request_ms: u64,
    /// A connection with nothing going on is closed after this many milliseconds.
    pub idle_ms: u64,
    /// The function that answers requests.
    pub router: Router,
}

const WAKER: Token = Token(usize::MAX);
/// Size of the one read buffer each thread shares between all its connections.
const READ_BUF: usize = 64 * 1024;
/// Reads one connection may do before the others get their turn.
const TURN: u32 = 16;
/// How long a peer may keep sending after we have said goodbye, in milliseconds.
const LINGER_MS: u64 = 2_000;
/// How long requests in flight get to finish after a shutdown request.
const GRACE_MS: u64 = 5_000;
/// Parked-byte buffers bigger than this are freed once empty instead of kept.
const SPARE: usize = 16 * 1024;

/// On epoll and kqueue, a read that comes back short has emptied the socket and the
/// next byte raises a fresh event, so the usual extra "read until it says no" is skipped.
const SHORT_READ_DRAINS: bool = cfg!(any(
    target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios",
    target_os = "freebsd", target_os = "dragonfly", target_os = "netbsd", target_os = "openbsd",
));
/// Windows makes no such promise, but asking the poller to watch the socket again is
/// free until the next poll, which still beats a wasted read.
const REARM_AFTER_SHORT_READ: bool = cfg!(windows);

/// What other threads hand to an event loop.
pub enum Mail {
    /// A connection the acceptor just took in.
    Conn(TcpStream),
    /// A finished job: the response for connection `slot`, if `id` still lives there.
    Done { slot: usize, id: u64, status: Status, kind: Kind, body: Vec<u8> },
}

/// An event loop's inbox. Any thread may post. The loop is woken only when the box
/// goes from empty to non-empty, so a burst of mail costs one wake-up, not one each.
pub struct Mailbox {
    mail: Mutex<Vec<Mail>>,
    waker: Waker,
}

impl Mailbox {
    pub fn post(&self, mail: Mail) {
        let mut queue = self.mail.lock().unwrap();
        let was_empty = queue.is_empty();
        queue.push(mail);
        drop(queue);
        if was_empty {
            let _ = self.waker.wake();
        }
    }
}

/// Lets another thread ask the server to stop.
pub struct Handle {
    stop: Arc<AtomicBool>,
    acceptor: Arc<Waker>,
    mailboxes: Vec<Arc<Mailbox>>,
}

impl Handle {
    /// Stop accepting, let requests in flight finish, then return from `serve`.
    /// Returns true if a shutdown had already been asked for.
    pub fn shutdown(&self) -> bool {
        let already = self.stop.swap(true, Ordering::SeqCst);
        let _ = self.acceptor.wake();
        for mailbox in &self.mailboxes {
            let _ = mailbox.waker.wake();
        }
        already
    }
}

/// Runs the server on `listener` until `Handle::shutdown` is called. This thread only
/// accepts connections and deals them out in turn; the I/O threads do everything else.
pub fn serve(listener: std::net::TcpListener, config: Config, ready: impl FnOnce(Handle)) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let mut listener = TcpListener::from_std(listener);
    let mut poll = Poll::new()?;
    poll.registry().register(&mut listener, Token(0), Interest::READABLE)?;
    let acceptor = Arc::new(Waker::new(poll.registry(), WAKER)?);
    let stop = Arc::new(AtomicBool::new(false));
    let pool = Pool::start(config.work_threads)?;
    let (mut mailboxes, mut threads) = (Vec::new(), Vec::new());
    for n in 0..config.threads.max(1) {
        let worker = Worker::new(config, stop.clone(), pool.clone())?;
        mailboxes.push(worker.mailbox.clone());
        threads.push(thread::Builder::new().name(format!("io-{n}")).spawn(move || worker.run())?);
    }
    ready(Handle { stop: stop.clone(), acceptor, mailboxes: mailboxes.clone() });

    let mut events = Events::with_capacity(16);
    let mut turn = 0;
    while !stop.load(Ordering::Relaxed) {
        match poll.poll(&mut events, None) {
            Err(e) if e.kind() != ErrorKind::Interrupted => return Err(e),
            _ => {}
        }
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    // Replies are complete when written: never hold them back to batch (Nagle).
                    let _ = stream.set_nodelay(true);
                    mailboxes[turn % mailboxes.len()].post(Mail::Conn(stream));
                    turn += 1;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                // The client gave up between connecting and being accepted: nothing lost.
                Err(e) if matches!(e.kind(), ErrorKind::Interrupted | ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset) => {}
                // Out of sockets or memory: give connections time to close, don't spin.
                Err(_) if stop.load(Ordering::Relaxed) => break,
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
    }
    drop(listener);
    for thread in threads {
        let _ = thread.join();
    }
    Ok(())
}

/// What a connection is in the middle of. Its deadline counts from when this began.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase { Idle, Request, Body, Parked, Sending, Lingering }

struct Conn {
    stream: TcpStream,
    session: Session,
    /// Unique within its event loop, so a late job result cannot land on a reused slot.
    id: u64,
    /// Bytes read but not yet consumed by the session. Normally empty.
    rbuf: Vec<u8>,
    /// Bytes the socket would not take yet, and how many of them are sent. Normally empty.
    wbuf: Vec<u8>,
    wpos: usize,
    phase: Phase,
    /// `session.served()` when the deadline was last set.
    served: u64,
    /// Clock time at which the sweep acts on this connection.
    deadline: u64,
    /// The socket may hold bytes we have not read.
    readable: bool,
    /// The peer hung up or the socket failed: read to the end, don't trust short reads.
    hung_up: bool,
    /// The peer will send no more.
    eof: bool,
    /// `rbuf` was parsed and needs more bytes before it is worth parsing again.
    starved: bool,
    /// The socket is full: wait for a writable event before writing again.
    blocked: bool,
    /// Registered for writable events as well as readable ones.
    wants_write: bool,
    /// The poller must be asked to watch the socket again (Windows, after a short read).
    rearm: bool,
    /// Close once `wbuf` is sent.
    closing: bool,
    /// Our side is shut. Reading goes on only so the peer can finish.
    lingering: bool,
    /// Already on the resume list.
    queued: bool,
}

/// One event loop and everything it owns.
struct Worker {
    config: Config,
    stop: Arc<AtomicBool>,
    pool: Arc<Pool>,
    poll: Poll,
    mailbox: Arc<Mailbox>,
    conns: Vec<Option<Conn>>,
    free: Vec<usize>,
    live: usize,
    next_id: u64,
    /// The one buffer every connection on this thread reads into.
    inbuf: Vec<u8>,
    /// Replies for the connection in hand, on their way to its socket.
    out: Vec<u8>,
    /// Where a route writes its body.
    body: Vec<u8>,
    /// Mail taken out of the box (swapped, so both keep their capacity).
    mail: Vec<Mail>,
    /// Connections that used up their turn with work still to do.
    resume: Vec<usize>,
    /// Milliseconds since this loop started, read once per turn of the loop.
    started: Instant,
    now: u64,
    /// The `date` header value, and the clock time at which it goes stale.
    date: [u8; 29],
    date_until: u64,
    sweep_every: u64,
    next_sweep: u64,
    /// Set at shutdown: the clock time at which to give up on busy connections.
    quit_at: Option<u64>,
}

impl Worker {
    fn new(config: Config, stop: Arc<AtomicBool>, pool: Arc<Pool>) -> io::Result<Worker> {
        let poll = Poll::new()?;
        let waker = Waker::new(poll.registry(), WAKER)?;
        Ok(Worker {
            config,
            stop,
            pool,
            poll,
            mailbox: Arc::new(Mailbox { mail: Mutex::new(Vec::new()), waker }),
            conns: Vec::new(),
            free: Vec::new(),
            live: 0,
            next_id: 0,
            inbuf: vec![0; READ_BUF],
            out: Vec::with_capacity(READ_BUF),
            body: Vec::with_capacity(256),
            mail: Vec::new(),
            resume: Vec::new(),
            started: Instant::now(),
            now: 0,
            date: http_date(0),
            date_until: 0,
            sweep_every: (config.request_ms.min(config.idle_ms) / 4).clamp(10, 1_000),
            next_sweep: 0,
            quit_at: None,
        })
    }

    fn run(mut self) {
        let mut events = Events::with_capacity(1024);
        let mut turn = Vec::new();
        loop {
            let wait = if !self.resume.is_empty() {
                Some(Duration::ZERO)
            } else if self.live == 0 && self.quit_at.is_none() {
                None // nothing can time out: sleep until mail arrives
            } else {
                Some(Duration::from_millis(self.next_sweep.saturating_sub(self.now).clamp(1, self.sweep_every)))
            };
            if let Err(e) = self.poll.poll(&mut events, wait) {
                assert!(e.kind() == ErrorKind::Interrupted, "poll failed: {e}");
            }
            // One look at the clock per turn; every connection handled this turn shares it.
            self.now = self.started.elapsed().as_millis() as u64;
            if self.now >= self.date_until {
                let wall = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
                self.date = http_date(wall.as_secs());
                self.date_until = self.now + 1_000 - wall.subsec_millis() as u64;
            }
            for event in &events {
                let Token(slot) = event.token();
                let Some(conn) = self.conns.get_mut(slot).and_then(Option::as_mut) else { continue };
                let failed = event.is_error();
                conn.readable |= failed || event.is_readable() || event.is_read_closed();
                conn.hung_up |= failed || event.is_read_closed();
                conn.blocked &= !(failed || event.is_writable() || event.is_write_closed());
                self.drive(slot);
            }
            std::mem::swap(&mut turn, &mut self.resume);
            for slot in turn.drain(..) {
                self.drive(slot);
            }
            // New connections last, so a slot freed this turn is not reused while events
            // for its previous owner may still be in this batch.
            self.take_mail();
            if self.now >= self.next_sweep {
                self.sweep();
            }
            if self.stop.load(Ordering::Relaxed) && self.wind_down() {
                return;
            }
        }
    }

    fn take_mail(&mut self) {
        std::mem::swap(&mut *self.mailbox.mail.lock().unwrap(), &mut self.mail);
        let mut mail = std::mem::take(&mut self.mail);
        for item in mail.drain(..) {
            match item {
                Mail::Conn(stream) => self.adopt(stream),
                Mail::Done { slot, id, status, kind, body } => self.complete(slot, id, status, kind, &body),
            }
        }
        self.mail = mail;
    }

    /// Takes over a new connection. Its first request has usually arrived already, so it
    /// is read at once instead of waiting for the poller to say so.
    fn adopt(&mut self, mut stream: TcpStream) {
        if self.quit_at.is_some() {
            return; // shutting down: dropping the stream closes it
        }
        let slot = self.free.pop().unwrap_or_else(|| {
            self.conns.push(None);
            self.conns.len() - 1
        });
        if self.poll.registry().register(&mut stream, Token(slot), Interest::READABLE).is_err() {
            return self.free.push(slot);
        }
        self.next_id += 1;
        self.live += 1;
        self.conns[slot] = Some(Conn {
            stream, session: Session::default(), id: self.next_id, rbuf: Vec::new(), wbuf: Vec::new(), wpos: 0,
            phase: Phase::Idle, served: 0, deadline: self.now + self.config.idle_ms,
            readable: true, hung_up: false, eof: false, starved: false, blocked: false,
            wants_write: false, rearm: false, closing: false, lingering: false, queued: false,
        });
        self.drive(slot);
    }

    /// Moves one connection as far as it can go without waiting, then leaves it either
    /// closed or registered for exactly the event it needs next.
    fn drive(&mut self, slot: usize) {
        if self.advance(slot) {
            self.settle(slot);
        } else {
            self.close(slot);
        }
    }

    /// Returns false once the connection is finished and should be closed.
    fn advance(&mut self, slot: usize) -> bool {
        let Worker { config, pool, mailbox, conns, inbuf, out, body, resume, date, quit_at, .. } = self;
        let Some(conn) = conns.get_mut(slot).and_then(Option::as_mut) else { return true };
        conn.queued = false;
        let id = conn.id;
        let mut offload = |job: Job| pool.submit(Task { mailbox: mailbox.clone(), slot, id, job });
        let mut cx = Cx { out, body, date, router: config.router, draining: quit_at.is_some(), offload: &mut offload };
        let mut reads = TURN;
        loop {
            // 1. Unsent bytes go first: nothing new is produced while the peer isn't reading.
            if !conn.wbuf.is_empty() {
                if conn.blocked {
                    return true;
                }
                match write_some(&mut conn.stream, &conn.wbuf[conn.wpos..]) {
                    Ok((sent, blocked)) => (conn.wpos, conn.blocked) = (conn.wpos + sent, blocked),
                    Err(_) => return false,
                }
                if conn.blocked {
                    return true;
                }
                conn.wbuf.clear();
                conn.wpos = 0;
            }
            // 2. Everything is on the wire. If that was the last reply, hang up properly.
            if conn.closing {
                if conn.eof {
                    return false;
                }
                if !conn.lingering {
                    // Nothing unread and nothing more coming: a plain close is safe.
                    if !conn.readable && conn.rbuf.is_empty() && conn.session.closed_cleanly() {
                        return false;
                    }
                    // Otherwise closing now would reset the connection, which can destroy
                    // the reply just sent. Close our side only and let the peer finish.
                    let _ = conn.stream.shutdown(Shutdown::Write);
                    conn.lingering = true;
                    conn.rbuf = Vec::new();
                }
                while conn.readable {
                    if reads == 0 {
                        return requeue(conn, resume, slot);
                    }
                    reads -= 1;
                    if matches!(read_some(conn, inbuf), Got::Broken) || conn.eof {
                        return false;
                    }
                }
                return true;
            }
            if conn.session.is_parked() {
                return true;
            }
            // 3. Parse what is already here; failing that, read; failing that, wait.
            let stop = if !conn.rbuf.is_empty() && !conn.starved {
                let input = std::mem::take(&mut conn.rbuf);
                let (used, stop) = conn.session.feed(&input, &mut cx);
                conn.rbuf = input;
                conn.rbuf.drain(..used);
                stop
            } else if conn.readable {
                if reads == 0 {
                    return requeue(conn, resume, slot);
                }
                reads -= 1;
                match read_some(conn, inbuf) {
                    // The usual case: parse straight out of the shared buffer, and park
                    // only the unfinished tail (if any) on the connection.
                    Got::Data(n) if conn.rbuf.is_empty() => {
                        let (used, stop) = conn.session.feed(&inbuf[..n], &mut cx);
                        conn.rbuf.extend_from_slice(&inbuf[used..n]);
                        stop
                    }
                    Got::Data(n) => {
                        conn.rbuf.extend_from_slice(&inbuf[..n]);
                        conn.starved = false;
                        continue;
                    }
                    Got::Nothing => continue,
                    Got::Broken => return false,
                }
            } else {
                // Nothing to parse, nothing to read. If the peer is done, so are we.
                return !conn.eof;
            };
            conn.starved = stop == Stop::NeedMore;
            conn.closing |= stop == Stop::Close;
            if push(conn, cx.out).is_err() {
                return false;
            }
        }
    }

    /// After a turn: tell the poller what this connection waits for, and set its deadline.
    fn settle(&mut self, slot: usize) {
        let Some(conn) = self.conns.get_mut(slot).and_then(Option::as_mut) else { return };
        let unsent = !conn.wbuf.is_empty();
        if unsent != conn.wants_write || conn.rearm {
            let interest = if unsent { Interest::READABLE | Interest::WRITABLE } else { Interest::READABLE };
            if self.poll.registry().reregister(&mut conn.stream, Token(slot), interest).is_err() {
                return self.close(slot);
            }
            (conn.wants_write, conn.rearm) = (unsent, false);
        }
        if conn.rbuf.is_empty() && conn.rbuf.capacity() > SPARE {
            conn.rbuf = Vec::new();
        }
        if !unsent && conn.wbuf.capacity() > SPARE {
            conn.wbuf = Vec::new();
        }
        let phase = if conn.lingering {
            Phase::Lingering
        } else if unsent {
            Phase::Sending
        } else if conn.session.is_parked() {
            Phase::Parked
        } else if conn.session.in_body() {
            Phase::Body
        } else if conn.rbuf.is_empty() {
            Phase::Idle
        } else {
            Phase::Request
        };
        // The deadline counts from the start of a phase, not from the last byte, so a
        // client cannot hold a connection open by trickling. Each answered request
        // starts the count again.
        if phase != conn.phase || conn.session.served() != conn.served {
            (conn.phase, conn.served) = (phase, conn.session.served());
            conn.deadline = match phase {
                Phase::Parked => u64::MAX,
                Phase::Request => self.now + self.config.request_ms,
                Phase::Lingering => self.now + LINGER_MS,
                Phase::Idle | Phase::Body | Phase::Sending => self.now + self.config.idle_ms,
            };
        }
    }

    fn close(&mut self, slot: usize) {
        if let Some(mut conn) = self.conns[slot].take() {
            let _ = self.poll.registry().deregister(&mut conn.stream);
            self.free.push(slot);
            self.live -= 1;
        }
    }

    /// A job is done: send its response, then carry on with whatever the client sent since.
    fn complete(&mut self, slot: usize, id: u64, status: Status, kind: Kind, body: &[u8]) {
        let conn = self.conns.get_mut(slot).and_then(Option::as_mut).filter(|conn| conn.id == id);
        let Some(conn) = conn else { return }; // the client left before its job finished
        conn.closing |= conn.session.finish(status, kind, body, &mut self.out, &self.date, self.quit_at.is_some());
        conn.starved = false;
        if push(conn, &mut self.out).is_err() {
            return self.close(slot);
        }
        self.drive(slot);
    }

    /// Acts on every connection whose time is up. Runs about once a second.
    fn sweep(&mut self) {
        self.next_sweep = self.now + self.sweep_every;
        for slot in 0..self.conns.len() {
            let Some(conn) = self.conns[slot].as_mut().filter(|conn| conn.deadline <= self.now) else { continue };
            if conn.phase != Phase::Request {
                self.close(slot);
                continue;
            }
            // Half a request and then silence: say why before hanging up.
            conn.session.time_out(&mut self.out, &self.date);
            conn.closing = true;
            conn.rbuf.clear();
            match push(conn, &mut self.out) {
                Ok(()) => self.drive(slot),
                Err(_) => self.close(slot),
            }
        }
    }

    /// Shutdown: idle connections close at once, busy ones get a few seconds to finish.
    /// Returns true when this loop may exit.
    fn wind_down(&mut self) -> bool {
        if self.quit_at.is_none() {
            self.quit_at = Some(self.now + GRACE_MS);
            self.sweep_every = self.sweep_every.min(50);
            for slot in 0..self.conns.len() {
                if self.conns[slot].as_ref().is_some_and(|conn| conn.phase == Phase::Idle) {
                    self.close(slot);
                }
            }
        }
        self.live == 0 || self.quit_at.is_some_and(|at| self.now >= at)
    }
}

/// Puts a connection that used up its turn at the back of the line.
fn requeue(conn: &mut Conn, resume: &mut Vec<usize>, slot: usize) -> bool {
    conn.queued = true;
    resume.push(slot);
    true
}

/// What a read brought. `Nothing`: socket empty for now, or the peer finished (`eof` set).
enum Got { Data(usize), Nothing, Broken }

/// One read, plus the bookkeeping that says whether another would find anything.
fn read_some(conn: &mut Conn, buf: &mut [u8]) -> Got {
    loop {
        return match conn.stream.read(buf) {
            Ok(0) => {
                (conn.eof, conn.readable) = (true, false);
                Got::Nothing
            }
            Ok(n) => {
                // A short read emptied the socket. Unless the peer hung up (that news came
                // with this event and will not come again), the next byte raises a new one.
                if n < buf.len() && !conn.hung_up && (SHORT_READ_DRAINS || REARM_AFTER_SHORT_READ) {
                    (conn.readable, conn.rearm) = (false, REARM_AFTER_SHORT_READ);
                }
                Got::Data(n)
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                conn.readable = false;
                Got::Nothing
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => Got::Broken,
        };
    }
}

/// Writes until everything is sent or the socket is full. Returns the bytes sent and
/// whether the socket filled up.
fn write_some(stream: &mut TcpStream, mut bytes: &[u8]) -> io::Result<(usize, bool)> {
    let mut sent = 0;
    while !bytes.is_empty() {
        match stream.write(bytes) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(n) => (sent, bytes) = (sent + n, &bytes[n..]),
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok((sent, true)),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok((sent, false))
}

/// Sends `out` now if nothing is queued ahead of it; whatever the socket will not take
/// waits in `wbuf`. Either way `out` comes back empty, ready for the next connection.
fn push(conn: &mut Conn, out: &mut Vec<u8>) -> io::Result<()> {
    let sent = if conn.wbuf.is_empty() {
        write_some(&mut conn.stream, out).map(|(sent, blocked)| {
            conn.blocked = blocked;
            sent
        })
    } else {
        Ok(0)
    };
    if let Ok(sent) = sent {
        conn.wbuf.extend_from_slice(&out[sent..]);
    }
    out.clear();
    sent.map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Reply, Request};
    use std::net::TcpStream as Client;

    fn router(_: &Request, body: &mut Vec<u8>) -> Reply {
        body.extend_from_slice(b"hi");
        Reply::Now(Status::OK, Kind::Plain)
    }

    #[test]
    fn shutdown_finishes_what_is_in_flight_then_returns() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let config = Config { threads: 2, work_threads: 1, request_ms: 5_000, idle_ms: 5_000, router };
        let (ready, started) = std::sync::mpsc::channel();
        let server = thread::spawn(move || serve(listener, config, |handle| ready.send(handle).unwrap()));
        let handle = started.recv().unwrap();
        let (mut idle, mut busy) = (Client::connect(address).unwrap(), Client::connect(address).unwrap());
        busy.write_all(b"GET / HTTP/1.1\r\nHost: half-sent").unwrap();
        idle.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        let mut reply = [0; 256];
        assert!(idle.read(&mut reply).unwrap() > 0);
        assert!(!handle.shutdown());
        // The idle connection closes at once; the half-sent request is still answered.
        assert_eq!(idle.read(&mut reply).unwrap(), 0);
        busy.write_all(b"\r\n\r\n").unwrap();
        let mut answer = String::new();
        busy.read_to_string(&mut answer).unwrap();
        assert!(answer.starts_with("HTTP/1.1 200 OK\r\n") && answer.contains("\r\nconnection: close\r\n") && answer.ends_with("hi"));
        assert!(server.join().unwrap().is_ok() && handle.shutdown());
    }
}
