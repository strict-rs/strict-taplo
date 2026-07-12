<!-- Do not edit; generated file. -->

# taplo-common

Shared machinery for the taplo tools: the `taplo.toml` config model, JSON-Schema association/caching/validation, and the `Environment` abstraction that lets the same code run natively and in WASM. Depends on `taplo` (with the `schema` feature); consumed by `taplo-lsp`, `taplo-cli`, and `taplo-wasm`.

## Layout

- `config.rs` — the `taplo.toml` model (`Config`), including `[[rule]]` blocks that scope formatter options to file globs and keys, plus `Config::prepare` and the schema/format-scope resolution the tools call.
- `environment.rs` + `environment/native.rs` — the `Environment` trait abstracting the filesystem, clock, stdio, and HTTP, with the native implementation. Every I/O-bound path in the tools is generic over `Environment` (the WASM implementation lives in `taplo-wasm`).
- `schema/` — JSON-Schema handling (compiled only with the `schema` feature): `associations.rs` (map documents to schemas), `cache.rs` (fetch/store), `ext.rs` (taplo schema extensions), `mod.rs` (`Schemas`, validation via `jsonschema`).
- `convert.rs`, `log.rs` (`setup_stderr_logging` and friends), `util.rs` — conversion helpers, tracing setup, and shared utilities.

## Features

- `rustls-tls` / `native-tls` — the two TLS backends for `reqwest`; **swap, don't stack**.
- `reqwest` — gates the optional `reqwest` dependency (HTTP schema fetching).
- `schema` — compiles the `schema` module.

`tokio` is target-gated: native builds pull `fs`/`io-std`/`rt`/`time`; `wasm32` builds pull a trimmed set. Read exact versions and feature wiring from `Cargo.toml`.

## Notes

`lib.rs` exports `HashMap`/`IndexMap` (over `ahash`), `AsyncMutex`/`AsyncRwLock` (over `tokio::sync`), and `LruCache` aliases used across the tools. The crate currently sets `#![warn(clippy::pedantic)]` with a list of `#![allow(..)]`s — upstream taplo posture that the strict-rs conversion has not yet replaced; read the current lint state from `lib.rs` before assuming it.
