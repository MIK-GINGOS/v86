// gm_exchange: build and sign one Binance order. No network here.
//
// Money is whole numbers (atoms) plus a fixed number of decimals, never a float.
// Keys come from a file the caller names; nothing here prints or logs a key.
// The URL is fixed to the TESTNET on purpose: going live must be a deliberate change.

use std::fmt;

use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey, pkcs8::DecodePrivateKey};

/// Binance Spot testnet. Not the real exchange.
pub const TESTNET_ORDER_URL: &str = "https://testnet.binance.vision/api/v3/order";

/// A non-negative amount as whole atoms: `Fixed::new(20, 2)` is 0.20.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fixed {
    pub atoms: u64,
    pub decimals: u8,
}

impl Fixed {
    pub const fn new(atoms: u64, decimals: u8) -> Self {
        Self { atoms, decimals }
    }
}

impl fmt::Display for Fixed {
    /// 10_000_000 at 7 decimals gives "1.0000000". Integer maths only.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.decimals == 0 {
            return write!(f, "{}", self.atoms);
        }
        let scale = 10u64.pow(u32::from(self.decimals));
        write!(f, "{}.{:0width$}", self.atoms / scale, self.atoms % scale, width = usize::from(self.decimals))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeInForce {
    /// Good till cancelled.
    Gtc,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitOrder {
    pub symbol: String,
    pub side: Side,
    pub time_in_force: TimeInForce,
    pub quantity: Fixed,
    pub price: Fixed,
}

#[derive(Debug, PartialEq, Eq)]
pub enum OrderError {
    BadSymbol,
    ZeroQuantity,
    ZeroPrice,
}

impl fmt::Display for OrderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::BadSymbol => "symbol must be 2 to 20 upper-case letters or digits",
            Self::ZeroQuantity => "quantity must be above zero",
            Self::ZeroPrice => "price must be above zero",
        })
    }
}

impl std::error::Error for OrderError {}

impl LimitOrder {
    pub fn check(&self) -> Result<(), OrderError> {
        let ok = (2..=20).contains(&self.symbol.len())
            && self.symbol.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
        if !ok {
            return Err(OrderError::BadSymbol);
        }
        if self.quantity.atoms == 0 {
            return Err(OrderError::ZeroQuantity);
        }
        if self.price.atoms == 0 {
            return Err(OrderError::ZeroPrice);
        }
        Ok(())
    }

    /// The query string Binance signs, in the order the API documents. The symbol is
    /// checked to be letters and digits only, so nothing needs escaping.
    pub fn payload(&self, timestamp_ms: u64) -> Result<String, OrderError> {
        self.check()?;
        let side = match self.side {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        };
        let tif = match self.time_in_force {
            TimeInForce::Gtc => "GTC",
        };
        Ok(format!(
            "symbol={}&side={side}&type=LIMIT&timeInForce={tif}&quantity={}&price={}&timestamp={timestamp_ms}",
            self.symbol, self.quantity, self.price
        ))
    }
}

/// Loads an Ed25519 private key from the text of a PKCS#8 PEM file.
pub fn load_key(pem: &str) -> Result<SigningKey, ed25519_dalek::pkcs8::Error> {
    SigningKey::from_pkcs8_pem(pem)
}

/// Percent-encodes everything except unreserved characters (base64 has + / =).
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The full form body to POST: the signed payload plus `&signature=`.
pub fn signed_body(order: &LimitOrder, timestamp_ms: u64, key: &SigningKey) -> Result<String, OrderError> {
    let payload = order.payload(timestamp_ms)?;
    let signature = STANDARD.encode(key.sign(payload.as_bytes()).to_bytes());
    Ok(format!("{payload}&signature={}", encode(&signature)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, Verifier};

    // The same order as the original Python script: SELL 1.0000000 BTCUSDT at 0.20.
    fn sample() -> LimitOrder {
        LimitOrder {
            symbol: "BTCUSDT".into(),
            side: Side::Sell,
            time_in_force: TimeInForce::Gtc,
            quantity: Fixed::new(10_000_000, 7),
            price: Fixed::new(20, 2),
        }
    }

    #[test]
    fn money_prints_with_exact_decimals() {
        assert_eq!(Fixed::new(10_000_000, 7).to_string(), "1.0000000");
        assert_eq!(Fixed::new(20, 2).to_string(), "0.20");
        assert_eq!(Fixed::new(5, 2).to_string(), "0.05");
        assert_eq!(Fixed::new(123_456_789, 8).to_string(), "1.23456789");
        assert_eq!(Fixed::new(7, 0).to_string(), "7");
    }

    #[test]
    fn payload_matches_the_original_script() {
        assert_eq!(
            sample().payload(1700000000000).unwrap(),
            "symbol=BTCUSDT&side=SELL&type=LIMIT&timeInForce=GTC&quantity=1.0000000&price=0.20&timestamp=1700000000000"
        );
    }

    #[test]
    fn signature_verifies_with_the_public_key() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let body = signed_body(&sample(), 1700000000000, &key).unwrap();
        let (payload, sig) = body.split_once("&signature=").unwrap();
        let sig = sig.replace("%2B", "+").replace("%2F", "/").replace("%3D", "=");
        let raw = STANDARD.decode(sig).unwrap();
        let sig = Signature::from_slice(&raw).unwrap();
        assert!(key.verifying_key().verify(payload.as_bytes(), &sig).is_ok());
        // Changing the price after signing must break it.
        assert!(key.verifying_key().verify(payload.replace("0.20", "0.21").as_bytes(), &sig).is_err());
    }

    #[test]
    fn signature_is_form_safe() {
        let key = SigningKey::from_bytes(&[1u8; 32]);
        let body = signed_body(&sample(), 1, &key).unwrap();
        let sig = body.split_once("&signature=").unwrap().1;
        assert!(sig.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'%'), "{sig}");
    }

    #[test]
    fn bad_orders_are_refused_before_signing() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let mut o = sample();
        o.symbol = "btc&x=1".into();
        assert_eq!(signed_body(&o, 1, &key), Err(OrderError::BadSymbol));
        let mut o = sample();
        o.quantity = Fixed::new(0, 7);
        assert_eq!(signed_body(&o, 1, &key), Err(OrderError::ZeroQuantity));
        let mut o = sample();
        o.price = Fixed::new(0, 2);
        assert_eq!(signed_body(&o, 1, &key), Err(OrderError::ZeroPrice));
    }

    #[test]
    fn url_is_the_testnet() {
        assert!(TESTNET_ORDER_URL.starts_with("https://testnet.binance.vision/"));
    }
}
