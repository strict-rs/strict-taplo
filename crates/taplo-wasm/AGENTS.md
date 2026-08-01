<!-- Do not edit; generated file. -->

# taplo-wasm

`wasm32` bindings that bundle the CLI and local language server for browser/Node consumers. `publish = false`, `[lib] crate-type = ["cdylib"]` — this crate only targets `wasm32` and never asserts thread safety for JavaScript-owned values.

## Exposed surface

`lib.rs` defines the `#[wasm_bindgen]` entry points:

- `initialize()` — installs the panic hook.
- `format(env, toml, options, config)` / `lint(env, toml, config)` — fallible one-shot format and lint over a document; lint without an associated schema returns an empty diagnostic result.
- `to_json(toml)` / `from_json(json)` — DOM ↔ JSON conversion.
- `run_cli(env, args)` — runs the full `taplo-cli` command surface (gated on the `cli` feature).
- `create_lsp(env, lsp_interface)` — validates the JS environment and constructs a `LocalServer`/`LocalWorld` pair wired to the JS transport (gated on the `lsp` feature). Initialization failures return `JsError`.

## Layout

- `environment.rs` — `WasmEnvironment`, a fallibly constructed `Environment + LocalEnvironment` backed by validated JS callbacks. Missing functions, thrown/rejected calls, invalid return types, malformed timestamps, and serialization failures remain typed.
- `lsp.rs` — `TaploWasmLsp` and `WasmLspInterface`, bridging the local `taplo-lsp` runtime to JavaScript without unsafe `Send`/`Sync` declarations.

## Features

`default = ["cli", "lsp"]`; `cli` pulls `taplo-cli`, `lsp` pulls `taplo-lsp`. `taplo-common` uses `schema` and browser-compatible `reqwest` transport without native `rustls-tls` or `native-tls`. Read exact versions from `Cargo.toml`.

## Building

The declared workspace toolchain installs `wasm32-unknown-unknown`. Use `just x wasm-matrix` for no-default, CLI-only, LSP-only, and combined feature checks. `just x js-build` performs an immutable JavaScript install, builds every wrapper package, and then exercises successful formatting/linting/local-LSP construction plus missing, thrown, rejected, malformed, and wrong-type JavaScript boundaries through `js/tests/wasm-boundaries.cjs`. Shared interpretation remains in platform-agnostic crates; keep browser/Node-specific capability glue here.
