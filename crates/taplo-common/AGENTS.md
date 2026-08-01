<!-- Do not edit; generated file. -->

# taplo-common

Shared machinery for the Taplo tools: the `taplo.toml` config model, typed TOML/JSON conversion, JSON Schema interpretation/association/caching, and execution-model-specific environment and schema-transport capabilities. Depends on `taplo` (with the `schema` feature); consumed by `taplo-lsp`, `taplo-cli`, and `taplo-wasm`.

## Layout

- `config.rs` — the `taplo.toml` model (`Config`), including `[[rule]]` blocks that scope formatter options to file globs and keys, plus `Config::prepare` and the schema/format-scope resolution the tools call.
- `environment.rs` + `environment/native.rs` — `Environment` owns synchronous host facts and streams, `LocalEnvironment` owns local asynchronous operations and `spawn_local`, and `ConcurrentEnvironment` owns `Send` futures and native spawning. `NativeEnvironment` exposes native capabilities; the local WASM implementation lives in `taplo-wasm`.
- `schema/transport.rs` — `LocalSchemaTransport`, `ConcurrentSchemaTransport`, and `OfflineSchemaTransport` keep transport capability separate from schema interpretation. Native TLS features never leak into the browser transport.
- `schema/` — JSON Schema interpretation (compiled with the `schema` feature): `associations.rs` maps documents to schemas, `cache.rs` persists atomic cache entries, `ext.rs` owns Taplo schema extensions, and `mod.rs` performs iterative reference/composition traversal with deterministic cycle handling.
- `convert.rs`, `log.rs`, `util.rs` — typed TOML/JSON conversion, tracing setup, and shared utilities. Reusable boundaries return domain errors rather than `anyhow`.

## Features

- `rustls-tls` / `native-tls` — the two TLS backends for `reqwest`; **swap, don't stack**.
- `reqwest` — gates the optional `reqwest` dependency (HTTP schema fetching).
- `schema` — compiles the `schema` module.

`tokio` is target-gated: native builds pull `fs`/`io-std`/`rt`/`time`; `wasm32` builds pull a trimmed set. Browser/WASM consumers enable `reqwest` without either native TLS feature and use the browser-compatible client. Read exact versions and feature wiring from `Cargo.toml`.

## Notes

`lib.rs` exports `HashMap`/`IndexMap`/`LruCache` aliases backed by the standard library's randomized `RandomState`, plus `AsyncMutex`/`AsyncRwLock` aliases over `tokio::sync`. Read the current lint state from `lib.rs` before assuming it.
