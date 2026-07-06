# lsp-async-stub

Generic async LSP server scaffolding — JSON-RPC message plumbing, request/notification routing, and cancellation — with **no dependency on taplo**. `taplo-lsp` is one consumer; the crate is otherwise standalone.

## Layout

- `lib.rs` — the `Server` builder and its handler registration (`on_request::<R, _>(..)`, `on_notification::<N, _>(..)`, terminated by `.build()`), plus the cancellation machinery (`Cancellation`, `CancelToken`). A private `handler` module holds the internal `Handler` trait.
- `rpc.rs` — JSON-RPC 2.0 message types (requests, responses, notifications, errors).
- `util.rs` — shared helpers used by the server and handlers.
- `listen/` — transport loops, compiled only when a `tokio-*` feature is on: `listen/stdio.rs` and `listen/tcp.rs`. The `listen` module itself is `#[cfg]`-gated on those features.

## Features

- `tokio-stdio` — stdio transport (pulls `tokio` with `io-std`/`io-util`/`macros`/`rt`).
- `tokio-tcp` — TCP transport (pulls `tokio` with `io-util`/`macros`/`net`/`rt`).

`default` enables neither, so the bare crate is transport-agnostic and the `tokio` dependency is optional. Consumers select a transport via features (e.g. `taplo-cli` enables both). Read exact versions from `Cargo.toml`.

## Notes

`getrandom` is configured with `wasm-bindgen`/`js` features, so the crate compiles for `wasm32`. Handlers are `async` (built on `futures`/`async-trait`); a `CancelToken` obtained from the server lets a long-running handler observe client cancellation.
