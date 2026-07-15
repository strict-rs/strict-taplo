# taplo-lsp

The TOML language server. Wires `lsp-async-stub` to `taplo` + `taplo-common`, exposing one handler per LSP request. Generic over `taplo_common::environment::Environment`, so the same server runs natively and in WASM.
