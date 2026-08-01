<!-- Do not edit; generated file. -->

# taplo-lsp-async

Generic async LSP server scaffolding — JSON-RPC message plumbing, kind-separated request/notification routing, lifecycle transitions, cancellation, mutation ordering, and native framing — with **no dependency on Taplo**. `taplo-lsp` consumes both honest execution families: `LocalServer` for current-thread/non-`Send` hosts and `ConcurrentServer` for native multi-threaded hosts.

## Layout

- `lib.rs` — common JSON-RPC classification, lifecycle state, local/concurrent cancellation states, and the ordered mutation barrier.
- `runtime.rs` — the `LocalServer`/`LocalContext` and `ConcurrentServer`/`ConcurrentContext` families, their private kind-separated handler adapters and registries, response handling, deferred output, and request-versus-mutation scheduling.
- `rpc.rs` — JSON-RPC 2.0 message types (requests, responses, notifications, errors).
- `util.rs` — checked UTF-8/UTF-16/UTF-32 source-position mapping using line/checkpoint tables and protocol-native LSP coordinates.
- `listen.rs` — checked native message framing, task/error propagation, and private stdio/TCP transport implementations behind feature-gated inherent `ConcurrentServer` entry points.

## Features

- `tokio-stdio` — stdio transport (pulls `tokio` with `io-std`/`io-util`/`macros`/`rt`).
- `tokio-tcp` — TCP transport (pulls `tokio` with `io-util`/`macros`/`net`/`rt`).

`default` enables neither, so the bare crate is transport-agnostic and the `tokio` dependency is optional. Consumers select a transport via features (e.g. `taplo-cli` enables both). Read exact versions from `Cargo.toml`.

## Notes

`getrandom` selects its JavaScript backend only on `wasm32`, so the transport-agnostic local server remains available to the WebAssembly binding. Do not introduce `async-trait`, unsafe thread-safety assertions, or a compatibility `Server` alias: local handlers intentionally retain local futures and `Rc`, while concurrent handlers require `Send` futures and `Arc`.

Register state-changing notifications with `on_mutation_notification`; requests then await every earlier mutation without serializing independent request work. Transport shutdown aborts outstanding native handler tasks, while per-request cancellation remains a JSON-RPC response path and preserves duplicate-ID state until the active request is finished.

Writers implement the execution family’s message-writer trait with `MessageWriterError`, preserving native or host I/O separately from a closed serialized output channel. Raw stdio, TCP, and framing failures remain `ServerError::Io`; do not convert typed channel failures into reconstructed `io::Error` values.
