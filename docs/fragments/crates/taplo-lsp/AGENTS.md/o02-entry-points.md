## Entry points

`lib.rs` exposes two constructors:

- `create_server::<E>() -> Server<World<E>>` — builds the `lsp-async-stub` server and registers every request/notification handler (initialize, folding ranges, document symbols, formatting, completion, hover, links, semantic tokens, prepare/rename, document + configuration + workspace notifications).
- `create_world::<E>(env) -> World<E>` — constructs the shared server state.
