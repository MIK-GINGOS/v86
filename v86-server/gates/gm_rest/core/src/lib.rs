// v86_core: the logic itself. It knows nothing about sockets, so the same code
// can be tested in plain unit tests and reused behind any gate.
// Add features as modules here, then expose them with a route in server/src/main.rs.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Counts requests answered by /v1/ping. Safe to share between threads; `const`
/// so a server can keep one in a `static` (no reference counting per request).
#[derive(Debug, Default)]
pub struct Pings {
    count: AtomicU64,
}

impl Pings {
    pub const fn new() -> Self {
        Self { count: AtomicU64::new(0) }
    }

    /// Counts one ping and writes {"ok":true,"count":N,"served_at":T} straight into `out`.
    pub fn ping_json(&self, out: &mut Vec<u8>) {
        let count = self.count.fetch_add(1, Ordering::Relaxed) + 1;
        write!(out, r#"{{"ok":true,"count":{count},"served_at":{}}}"#, now_secs())
            .expect("writing to a Vec cannot fail");
    }

    /// Pings so far, without counting one (for monitoring).
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Starts again from 0 (for speed tests).
    pub fn reset(&self) {
        self.count.store(0, Ordering::Relaxed);
    }
}

/// Demo orders: a pretend exchange reply, so screens can be wired and shown with no API key.
/// Same shape as Binance's order reply (status NEW) but marked "demo": true.
#[derive(Debug, Default)]
pub struct DemoOrders {
    next: AtomicU64,
}

impl DemoOrders {
    pub const fn new() -> Self {
        Self { next: AtomicU64::new(0) }
    }

    pub fn place_json(&self, out: &mut Vec<u8>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        write!(
            out,
            r#"{{"demo":true,"symbol":"BTCUSDT","orderId":{id},"side":"SELL","type":"LIMIT","status":"NEW","price":"0.20","origQty":"1.0000000","executedQty":"0.00000000","transactTime":{ms}}}"#
        )
        .expect("writing to a Vec cannot fail");
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_orders_count_up_and_are_marked_demo() {
        let d = DemoOrders::new();
        let (mut a, mut b) = (Vec::new(), Vec::new());
        d.place_json(&mut a);
        d.place_json(&mut b);
        assert!(String::from_utf8(a).unwrap().starts_with(r#"{"demo":true,"symbol":"BTCUSDT","orderId":1,"#));
        assert!(String::from_utf8(b).unwrap().contains(r#""orderId":2,"#));
    }

    #[test]
    fn counts_each_ping() {
        let p = Pings::new();
        let (mut a, mut b) = (Vec::new(), Vec::new());
        p.ping_json(&mut a);
        p.ping_json(&mut b);
        assert!(String::from_utf8(a).unwrap().starts_with(r#"{"ok":true,"count":1,"served_at":"#));
        assert!(String::from_utf8(b).unwrap().contains(r#""count":2,"#));
        assert_eq!(p.count(), 2);
        p.reset();
        assert_eq!(p.count(), 0);
    }
}
