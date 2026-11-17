// place_order: sends the sample order to the Binance TESTNET. Build with --features send.
//
//   V86_BINANCE_API_KEY   the testnet API key
//   V86_BINANCE_KEY_PATH  path to your Ed25519 private key (PKCS#8 PEM)
//
// The key text is read, used to sign, and never printed.

use std::time::{SystemTime, UNIX_EPOCH};

use gm_exchange::{Fixed, LimitOrder, Side, TESTNET_ORDER_URL, TimeInForce, load_key, signed_body};

fn main() {
    if let Err(message) = run() {
        eprintln!("[gm_exchange] {message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let api_key = std::env::var("V86_BINANCE_API_KEY").map_err(|_| "set V86_BINANCE_API_KEY")?;
    let key_path = std::env::var("V86_BINANCE_KEY_PATH").map_err(|_| "set V86_BINANCE_KEY_PATH")?;
    let pem = std::fs::read_to_string(&key_path).map_err(|e| format!("cannot read {key_path}: {e}"))?;
    let key = load_key(&pem).map_err(|_| "that file is not an Ed25519 PKCS#8 PEM key")?;

    // The same order as the original script: SELL 1.0000000 BTCUSDT at 0.20.
    let order = LimitOrder {
        symbol: "BTCUSDT".into(),
        side: Side::Sell,
        time_in_force: TimeInForce::Gtc,
        quantity: Fixed::new(10_000_000, 7),
        price: Fixed::new(20, 2),
    };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?;
    let body = signed_body(&order, now.as_millis() as u64, &key).map_err(|e| e.to_string())?;

    let reply = ureq::post(TESTNET_ORDER_URL)
        .set("X-MBX-APIKEY", &api_key)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&body);
    match reply {
        Ok(r) => println!("{}", r.into_string().map_err(|e| e.to_string())?),
        // Binance explains refusals in the body, so show it.
        Err(ureq::Error::Status(code, r)) => println!("HTTP {code}: {}", r.into_string().unwrap_or_default()),
        Err(e) => return Err(e.to_string()),
    }
    Ok(())
}
