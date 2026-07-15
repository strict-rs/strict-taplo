## Layout

- `bin/taplo.rs` — the binary entry point; `[[bin]] name = "taplo"`.
- `lib.rs` — the `Taplo<E>` type (config loading, file collection) and `execute` dispatch that the binary and the WASM bindings both drive.
- `args.rs` — the `clap` derive argument model (`TaploArgs`, subcommand enums, `GeneralArgs`).
- `commands/` — one module per subcommand: `format.rs`, `lint.rs`, `config.rs`, `queries.rs`, `lsp.rs`, `toml_test.rs`; `commands/mod.rs` dispatches.
- `printing.rs` — diagnostic/output rendering (`codespan-reporting`, colored diffs).
