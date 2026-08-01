# taplo-lsp

The TOML language server. Shared handlers and immutable world snapshots are exposed through two honest runtime families: a thread-safe concurrent native server and a current-thread local server for WASM. Both use the same message classification, lifecycle, mutation-ordering, cancellation, and typed service logic without requiring JavaScript values to be `Send` or `Sync`.
