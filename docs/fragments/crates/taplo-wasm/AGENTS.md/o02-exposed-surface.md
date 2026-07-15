## Exposed surface

`lib.rs` defines the `#[wasm_bindgen]` entry points:

- `initialize()` — installs the panic hook.
- `format(env, toml, options, config)` / `lint(env, toml, config)` — one-shot format and lint over a document.
- `to_json(toml)` / `from_json(json)` — DOM ↔ JSON conversion.
- `run_cli(env, args)` — runs the full `taplo-cli` command surface (gated on the `cli` feature).
- `create_lsp(env, lsp_interface)` — constructs a `taplo-lsp` server/world pair wired to a JS transport (gated on the `lsp` feature).
