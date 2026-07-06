# taplo-cli

The `taplo` binary and the library behind it. Depends on `taplo`, `taplo-common`, and (optionally) `taplo-lsp`; generic over `taplo_common::environment::Environment` so the CLI logic is reused by `taplo-wasm`.

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

The `toml-test` subcommand reads TOML on stdin and emits toml-test's tagged JSON, driving the external decoder-conformance suite. Build the decoder with `cargo build --bin taplo --no-default-features --features "rustls-tls,toml-test"`. The repo's CI runs it against `toml-test`; consult `.github/workflows/ci.yaml` for the `-toml` version and skip-list before changing parser behavior.

## Self-formatting (dogfood)

The CLI formats this repo's own TOML per the root `taplo.toml`, and CI asserts `taplo fmt --check` produces no diff. After changing formatter output, run `cargo run -- fmt` and commit the reformatted TOML — formatting is expected to be idempotent.
