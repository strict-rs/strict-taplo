## toml-test conformance harness

The `toml-test` subcommand reads TOML on stdin and emits toml-test's tagged JSON, driving the external decoder-conformance suite. Build the decoder with `cargo build --bin taplo --no-default-features --features "rustls-tls,toml-test"`. The repo's CI runs it against `toml-test`; consult `.github/workflows/ci.yaml` for the `-toml` version and skip-list before changing parser behavior.
