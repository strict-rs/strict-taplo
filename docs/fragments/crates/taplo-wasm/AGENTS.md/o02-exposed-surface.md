## Exposed surface

`lib.rs` defines the `#[wasm_bindgen]` entry points:

- `initialize()` — installs the panic hook.
- `format(env, toml, options, config)` / `lint(env, toml, config)` — fallible one-shot format and lint over a document; lint without an associated schema returns an empty diagnostic result.
- `to_json(toml)` / `from_json(json)` — DOM ↔ JSON conversion.
- `run_cli(env, args)` — runs the full `taplo-cli` command surface (gated on the `cli` feature).
- `create_lsp(env, lsp_interface)` — validates the JS environment and constructs a `LocalServer`/`LocalWorld` pair wired to the JS transport (gated on the `lsp` feature). Initialization failures return `JsError`.
