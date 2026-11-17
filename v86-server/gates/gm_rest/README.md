# gm_rest: the fast HTTP engine

Taken from the `gm_hello` speed-test server and renamed. The hello greeting and the
physics test are gone; the engine (own reactor, no framework) is unchanged.

- `core/`   plain Rust logic, no networking. Grow the server here.
- `server/` the engine (`http`, `session`, `reactor`, `pool`) and the routes in `src/main.rs`.
- `flood/`  load tester: `cargo run --release` inside `flood/` (server must be running).

Routes now: `/v1/ping`, `/v1/slow?ms=`, `/stats`, `/reset` (dev only), `/health`.

    cargo test                         # unit + wire tests
    cargo build --release && target\release\v86_server.exe     # PORT=4000
    cd flood && cargo run --release -- 4000 5

To add a feature: write it in `core/`, add a route in `server/src/main.rs`.
Slow work (partner calls, heavy maths) returns `Reply::Later` so it runs on the pool.
