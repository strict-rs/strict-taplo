# taplo-lsp

The TOML language server. Wires `lsp-async-stub` to `taplo` + `taplo-common`, exposing one handler per LSP request. Generic over `taplo_common::environment::Environment`, so the same server runs natively and in WASM.

## Entry points

`lib.rs` exposes two constructors:

- `create_server::<E>() -> Server<World<E>>` — builds the `lsp-async-stub` server and registers every request/notification handler (initialize, folding ranges, document symbols, formatting, completion, hover, links, semantic tokens, prepare/rename, document + configuration + workspace notifications).
- `create_world::<E>(env) -> World<E>` — constructs the shared server state.

## Layout

- `handlers/` — one module per request (e.g. `completion.rs`, `hover.rs`, `formatting.rs`, `rename.rs`, `semantic_tokens.rs`, `document_symbols.rs`, `folding_ranges.rs`, `links.rs`, `initialize.rs`, `schema.rs`, `documents.rs`, `configuration.rs`, `workspaces.rs`, `conversion.rs`). `handlers.rs` re-exports them.
- `world.rs` — `World`/`WorldState` server state.
- `lsp_ext/` — non-standard protocol extensions: `request.rs` (`ConvertToJson`, `ConvertToToml`, `ListSchemas`, `AssociatedSchema`) and `notification.rs` (`AssociateSchema`), registered alongside the standard handlers in `create_server`.
- `config.rs`, `diagnostics.rs`, `query.rs` — LSP-side config model, diagnostic publishing, and syntax-tree position queries.

## Adding or changing a handler

Implement it in `handlers/<name>.rs`, re-export it from `handlers.rs`, and register it in `create_server`. Custom (non-LSP-spec) messages additionally need their type declared under `lsp_ext/`.

## Features and crate type

- `default = rustls-tls`; `native-tls` is the alternative — both forward to `taplo-common`'s TLS features.
- `[lib] crate-type = ["cdylib", "rlib"]` — usable as a Rust dependency and as a C-ABI dynamic library.

Read exact versions from `Cargo.toml`.
