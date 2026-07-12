# taplo-common

Shared machinery for the taplo tools: the `taplo.toml` config model, JSON-Schema association/caching/validation, and the `Environment` abstraction that lets the same code run natively and in WASM. Depends on `taplo` (with the `schema` feature); consumed by `taplo-lsp`, `taplo-cli`, and `taplo-wasm`.
