// flood: a load tester for v86-server's speed tests, in plain Rust threads and sockets.
// It floods one server with GET /v1/ping for a fixed time, then reports what the SERVER
// counted and checks it against its own count, the way speedtest-parallel.ps1 does
// with autocannon. autocannon tops out around 20k requests/second per tester on this
// laptop, far below what the servers can do, so there the tester is what gets
// measured. Here it is the server.
//   compare\speedtest-native.cmd                 -> Rust (port 4000), 15 s
//   compare\speedtest-native.cmd 4002            -> Bun  (port 4002)
//   compare\speedtest-native.cmd 4000 30 64 10   -> port, seconds, connections, requests per write

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

// One request on its own connection. Returns everything the server sent back.
fn fetch(port: u16, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).ok()?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).ok()?;
    Some(reply)
}

// The number after `"key":` in a JSON reply.
fn number(reply: &str, key: &str) -> Option<u64> {
    let rest = &reply[reply.find(&format!("\"{key}\":"))? + key.len() + 3..];
    rest[..rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len())].parse().ok()
}

// Requests the server has counted so far. Rust has /stats (reads the count without
// adding one); Bun only has /v1/ping.
fn count(port: u16) -> Option<u64> {
    let stats = fetch(port, "/stats").and_then(|reply| number(&reply, "visits"));
    stats.or_else(|| number(&fetch(port, "/v1/ping")?, "count"))
}

// One connection: write `pipe` requests in one go, read until that many replies have
// come back, repeat until told to stop. Every request it sends is answered before it
// leaves, so its count and the server's must agree. None means the connection broke.
fn flood(port: u16, batch: &[u8], pipe: u64, start: &Barrier, stop: &AtomicBool, total: &AtomicU64) -> Option<()> {
    let stream = TcpStream::connect(("127.0.0.1", port));
    start.wait(); // everyone waits here, connected or not, so the flood starts together
    let mut stream = stream.ok()?;
    stream.set_nodelay(true).ok()?;
    let mut buf = vec![0u8; 64 * 1024];
    while !stop.load(Ordering::Relaxed) {
        stream.write_all(batch).ok()?;
        // Every reply's head ends with a blank line and the JSON after it never has
        // one, so counting blank lines counts replies.
        let (mut seen, mut matched) = (0, 0);
        while seen < pipe {
            let n = stream.read(&mut buf).ok().filter(|&n| n > 0)?;
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
        total.fetch_add(seen, Ordering::Relaxed);
    }
    Some(())
}

// 1234567 → "1,234,567"
fn commas(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn main() {
    let arg = |index: usize, default: u64| std::env::args().nth(index).and_then(|text| text.parse().ok()).unwrap_or(default);
    let (port, seconds, connections, pipe) = (arg(1, 4000) as u16, arg(2, 15), arg(3, 64).max(1) as usize, arg(4, 10).max(1));
    let url = format!("http://127.0.0.1:{port}/v1/ping");

    // Start from 0 (Rust only; Bun has no /reset, so it just counts on). Skipped when
    // several floods share the server (tabs.ps1 sets FLOOD_NO_RESET and resets once
    // itself): each reset would wipe the others' count mid-run.
    let reset = std::env::var_os("FLOOD_NO_RESET").is_none();
    if reset && fetch(port, "/reset").is_some_and(|reply| reply.starts_with("HTTP/1.1 200")) {
        println!("Counter reset to 0.");
    }
    let Some(before) = count(port) else {
        eprintln!("Cannot reach {url} (is the server running?)");
        std::process::exit(1);
    };
    println!("Flooding {url} for {seconds} s from {connections} connections ({pipe} requests per write)...");
    let batch = format!("GET /v1/ping HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").repeat(pipe as usize).into_bytes();
    let (start, stop, total) = (Barrier::new(connections + 1), AtomicBool::new(false), AtomicU64::new(0));
    let (secs, broken) = std::thread::scope(|scope| {
        let testers: Vec<_> = (0..connections).map(|_| scope.spawn(|| flood(port, &batch, pipe, &start, &stop, &total))).collect();
        start.wait();
        let clock = Instant::now();
        std::thread::sleep(Duration::from_secs(seconds));
        stop.store(true, Ordering::Relaxed);
        let broken = testers.into_iter().map(|tester| tester.join().unwrap()).filter(Option::is_none).count();
        (clock.elapsed().as_secs_f64(), broken)
    });

    let served = count(port).unwrap_or(before).saturating_sub(before);
    let got = total.load(Ordering::Relaxed);
    println!();
    println!("Server answered : {} requests", commas(served));
    println!("Tester got      : {} replies   (difference {})", commas(got), served as i64 - got as i64);
    if reset {
        println!("                  (the server also counts your phones and, on Bun, this count itself)");
    } else {
        println!("                  (other tabs flood the same server, so the server's number");
        println!("                   includes theirs: compare the grand total in the monitor)");
    }
    if broken > 0 {
        println!("Broken          : {broken} of {connections} connections failed or were closed");
    }
    println!("Time            : {secs:.1} s flooding");
    if served > 0 {
        let speed = served as f64 / secs;
        println!("Speed           : {} requests/second", commas(speed as u64));
        println!("2 million would take {:.1} seconds at this speed", 2_000_000.0 / speed);
    }
}
