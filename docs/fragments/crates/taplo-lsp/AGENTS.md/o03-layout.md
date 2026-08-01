## Layout

- `handlers/` — one module per request (e.g. `completion.rs`, `hover.rs`, `formatting.rs`, `rename.rs`, `semantic_tokens.rs`, `document_symbols.rs`, `folding_ranges.rs`, `links.rs`, `initialize.rs`, `schema.rs`, `documents.rs`, `configuration.rs`, `workspaces.rs`, `conversion.rs`). `handlers.rs` re-exports them.
- `world.rs` — mutable service state, immutable document/config/schema snapshots, checked revisions, and the `LocalWorld`/`ConcurrentWorld` ownership aliases.
- `runtime.rs` — macro owner that generates the bound-specific `local` and `concurrent` runtime families and registers the same handlers on `LocalServer` and `ConcurrentServer`.
- `lsp_ext/` — non-standard protocol extensions plus the Taplo-owned modern document-symbol request marker, registered alongside the standard handlers in both server families.
- `config.rs`, `diagnostics.rs`, `query.rs` — LSP-side config model, diagnostic publishing, and syntax-tree position queries.
