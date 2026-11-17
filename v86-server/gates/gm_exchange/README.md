# gm_exchange: one order on the Binance testnet

Same order as the old Python script (SELL 1.0000000 BTCUSDT at 0.20), in Rust.

- `src/lib.rs`  builds and signs the order. No network. Money is whole numbers, never floats.
- `src/main.rs` sends it (needs `--features send`).

Testnet only: the URL is a constant in `lib.rs`. The key file is read, used and never printed.

    cargo test                                  # offline: payload and signature checks
    cargo build --features send
    set V86_BINANCE_API_KEY=...                 # testnet key
    set V86_BINANCE_KEY_PATH=C:\path\key.pem    # Ed25519 PKCS#8 PEM
    cargo run --features send

Later: the key moves behind `gates/gm_signer` so this gate never sees it.
