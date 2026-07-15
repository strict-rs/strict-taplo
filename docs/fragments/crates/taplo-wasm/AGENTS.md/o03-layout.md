## Layout

- `environment.rs` — `WasmEnvironment`, the `taplo_common::environment::Environment` implementation backed by a JS-provided interface (filesystem, clock, stdio, HTTP).
- `lsp.rs` — `TaploWasmLsp` and `WasmLspInterface`, bridging `taplo-lsp` messages to JS.
