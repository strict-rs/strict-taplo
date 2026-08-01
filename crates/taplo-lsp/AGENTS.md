<!-- Do not edit; generated file. -->

# taplo-lsp

The TOML language server. Shared handlers and immutable world snapshots are exposed through two honest runtime families: a thread-safe concurrent native server and a current-thread local server for WASM. Both use the same message classification, lifecycle, mutation-ordering, cancellation, and typed service logic without requiring JavaScript values to be `Send` or `Sync`.

## Entry points

`lib.rs` exposes four explicit constructors:

- `create_concurrent_server::<E>() -> ConcurrentServer<ConcurrentWorld<E>>` and `create_concurrent_world(env, http)` — construct the native multi-threaded server and `Arc`-owned world for a `ConcurrentEnvironment`.
- `create_local_server::<E>() -> LocalServer<LocalWorld<E>>` and `create_local_world(env, http)` — construct the current-thread server and `Rc`-owned world for a `LocalEnvironment`.

There are no `Server`, `ServerBuilder`, or ambiguous world aliases. Construction that initializes schema services is fallible and returns `WorldError`.

## Layout

- `handlers/` — one module per request (e.g. `completion.rs`, `hover.rs`, `formatting.rs`, `rename.rs`, `semantic_tokens.rs`, `document_symbols.rs`, `folding_ranges.rs`, `links.rs`, `initialize.rs`, `schema.rs`, `documents.rs`, `configuration.rs`, `workspaces.rs`, `conversion.rs`). `handlers.rs` re-exports them.
- `world.rs` — mutable service state, immutable document/config/schema snapshots, checked revisions, and the `LocalWorld`/`ConcurrentWorld` ownership aliases.
- `runtime.rs` — macro owner that generates the bound-specific `local` and `concurrent` runtime families and registers the same handlers on `LocalServer` and `ConcurrentServer`.
- `lsp_ext/` — non-standard protocol extensions plus the Taplo-owned modern document-symbol request marker, registered alongside the standard handlers in both server families.
- `config.rs`, `diagnostics.rs`, `query.rs` — LSP-side config model, diagnostic publishing, and syntax-tree position queries.

## Adding or changing a handler

Implement runtime-neutral behavior in `handlers/<name>.rs`, re-export it from `handlers.rs`, and register thin adapters in both runtime families. State-changing notifications belong on the ordered mutation lane. Requests must capture immutable snapshots after the preceding mutation barrier and suppress generation-dependent output when the captured revision is stale. Custom messages additionally need their type declared under `lsp_ext/`.

## Features and crate type

- `default = rustls-tls`; `native-tls` is the alternative — both forward to `taplo-common`'s TLS features.
- `[lib] crate-type = ["cdylib", "rlib"]` — usable as a Rust dependency and as a C-ABI dynamic library.

Read exact versions from `Cargo.toml`.
