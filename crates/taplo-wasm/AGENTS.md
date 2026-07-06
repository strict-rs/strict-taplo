# taplo-wasm

`wasm32` bindings that bundle the CLI and language server for browser/Node consumers. `publish = false`, `[lib] crate-type = ["cdylib"]` — this crate only targets `wasm32`.

## Exposed surface

`lib.rs` defines the `#[wasm_bindgen]` entry points:

- `initialize()` — installs the panic hook.
- `format(env, toml, options, config)` / `lint(env, toml, config)` — one-shot format and lint over a document.
- `to_json(toml)` / `from_json(json)` — DOM ↔ JSON conversion.
- `run_cli(env, args)` — runs the full `taplo-cli` command surface (gated on the `cli` feature).
- `create_lsp(env, lsp_interface)` — constructs a `taplo-lsp` server/world pair wired to a JS transport (gated on the `lsp` feature).

## Layout

- `environment.rs` — `WasmEnvironment`, the `taplo_common::environment::Environment` implementation backed by a JS-provided interface (filesystem, clock, stdio, HTTP).
- `lsp.rs` — `TaploWasmLsp` and `WasmLspInterface`, bridging `taplo-lsp` messages to JS.

## Features

`default = ["cli", "lsp"]`; `cli` pulls `taplo-cli`, `lsp` pulls `taplo-lsp`. `taplo-common` is brought in with `rustls-tls`/`schema`/`reqwest`. Read exact versions from `Cargo.toml`.

## Building

Check with `cargo check --target wasm32-unknown-unknown` (add the target via `rustup target add wasm32-unknown-unknown` first). Because this is the WASM half of the `Environment` abstraction, all shared logic lives in the platform-agnostic crates — keep browser/Node-specific glue here.
