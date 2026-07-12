## Layout

- `handlers/` — one module per request (e.g. `completion.rs`, `hover.rs`, `formatting.rs`, `rename.rs`, `semantic_tokens.rs`, `document_symbols.rs`, `folding_ranges.rs`, `links.rs`, `initialize.rs`, `schema.rs`, `documents.rs`, `configuration.rs`, `workspaces.rs`, `conversion.rs`). `handlers.rs` re-exports them.
- `world.rs` — `World`/`WorldState` server state.
- `lsp_ext/` — non-standard protocol extensions: `request.rs` (`ConvertToJson`, `ConvertToToml`, `ListSchemas`, `AssociatedSchema`) and `notification.rs` (`AssociateSchema`), registered alongside the standard handlers in `create_server`.
- `config.rs`, `diagnostics.rs`, `query.rs` — LSP-side config model, diagnostic publishing, and syntax-tree position queries.
