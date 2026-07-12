## Adding or changing a handler

Implement it in `handlers/<name>.rs`, re-export it from `handlers.rs`, and register it in `create_server`. Custom (non-LSP-spec) messages additionally need their type declared under `lsp_ext/`.
