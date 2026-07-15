## Layout

- `lib.rs` — the `Server` builder and its handler registration (`on_request::<R, _>(..)`, `on_notification::<N, _>(..)`, terminated by `.build()`), plus the cancellation machinery (`Cancellation`, `CancelToken`). A private `handler` module holds the internal `Handler` trait.
- `rpc.rs` — JSON-RPC 2.0 message types (requests, responses, notifications, errors).
- `util.rs` — shared helpers used by the server and handlers.
- `listen/` — transport loops, compiled only when a `tokio-*` feature is on: `listen/stdio.rs` and `listen/tcp.rs`. The `listen` module itself is `#[cfg]`-gated on those features.
