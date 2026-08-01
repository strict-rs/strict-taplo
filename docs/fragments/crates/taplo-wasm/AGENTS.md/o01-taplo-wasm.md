# taplo-wasm

`wasm32` bindings that bundle the CLI and local language server for browser/Node consumers. `publish = false`, `[lib] crate-type = ["cdylib"]` — this crate only targets `wasm32` and never asserts thread safety for JavaScript-owned values.
