# taplo-cli

The `taplo` binary and the typed library boundary behind it. Depends on `taplo`, `taplo-common`, and (optionally) `taplo-lsp`; native execution uses `ConcurrentEnvironment` and the concurrent LSP server, while the reusable non-LSP command logic remains available to the local WASM adapter.
