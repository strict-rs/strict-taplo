## Layout

- `config.rs` — the `taplo.toml` model (`Config`), including `[[rule]]` blocks that scope formatter options to file globs and keys, plus `Config::prepare` and the schema/format-scope resolution the tools call.
- `environment.rs` + `environment/native.rs` — the `Environment` trait abstracting the filesystem, clock, stdio, and HTTP, with the native implementation. Every I/O-bound path in the tools is generic over `Environment` (the WASM implementation lives in `taplo-wasm`).
- `schema/` — JSON-Schema handling (compiled only with the `schema` feature): `associations.rs` (map documents to schemas), `cache.rs` (fetch/store), `ext.rs` (taplo schema extensions), `mod.rs` (`Schemas`, validation via `jsonschema`).
- `convert.rs`, `log.rs` (`setup_stderr_logging` and friends), `util.rs` — conversion helpers, tracing setup, and shared utilities.
