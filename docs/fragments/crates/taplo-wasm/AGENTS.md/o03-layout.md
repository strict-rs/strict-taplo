## Layout

- `environment.rs` — `WasmEnvironment`, a fallibly constructed `Environment + LocalEnvironment` backed by validated JS callbacks. Missing functions, thrown/rejected calls, invalid return types, malformed timestamps, and serialization failures remain typed.
- `lsp.rs` — `TaploWasmLsp` and `WasmLspInterface`, bridging the local `taplo-lsp` runtime to JavaScript without unsafe `Send`/`Sync` declarations.
