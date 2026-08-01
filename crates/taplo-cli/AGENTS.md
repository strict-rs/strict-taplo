<!-- Do not edit; generated file. -->

# taplo-cli

The `taplo` binary and the typed library boundary behind it. Depends on `taplo`, `taplo-common`, and (optionally) `taplo-lsp`; native execution uses `ConcurrentEnvironment` and the concurrent LSP server, while the reusable non-LSP command logic remains available to the local WASM adapter.

## Layout

- `bin/taplo.rs` — the binary entry point; `[[bin]] name = "taplo"`.
- `lib.rs` — the `Taplo<E>` type (config loading, file collection) and `execute` dispatch that the binary and the WASM bindings both drive.
- `args.rs` — the `clap` derive argument model (`TaploArgs`, subcommand enums, `GeneralArgs`).
- `commands/` — one module per subcommand: `format.rs`, `lint.rs`, `config.rs`, `queries.rs`, `lsp.rs`, `toml_test.rs`; `commands/mod.rs` dispatches.
- `printing.rs` — diagnostic/output rendering (`codespan-reporting`, colored diffs).

## Adding a subcommand

Add the variant to the arg model in `args.rs`, add a `commands/<name>.rs` module, and wire the dispatch in `commands/mod.rs`.

## Features

- `default = ["completions", "lint", "lsp", "rustls-tls", "toml-test"]`.
- Dependency graph between features: `lsp` requires `lint`; `toml-test` requires `lint`; `lint` pulls in `reqwest` plus `taplo-common`'s schema support. `completions` gates `clap_complete`.
- `rustls-tls` vs `native-tls` — the two TLS backends; **swap, don't stack**.

Read exact versions from `Cargo.toml`.

## toml-test conformance harness

The `toml-test` subcommand reads TOML on stdin and emits `toml-test`'s tagged JSON. `just x toml-conformance` owns the checksum-verified pinned runner and requires the TOML 1.1 suite to pass without a permanent skip list. A newly incompatible upstream case is a stop-and-review decision, not a silent exclusion.

## Self-formatting (dogfood)

The CLI formats this repo's own TOML per the root `taplo.toml`. `just x taplo-self-format` runs the repository-owned dogfood check through the guarded extension surface; successful formatting must remain idempotent.
