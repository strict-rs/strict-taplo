## Layout

- `config.rs` — the `taplo.toml` model (`Config`), including `[[rule]]` blocks that scope formatter options to file globs and keys, plus `Config::prepare` and the schema/format-scope resolution the tools call.
- `environment.rs` + `environment/native.rs` — `Environment` owns synchronous host facts and streams, `LocalEnvironment` owns local asynchronous operations and `spawn_local`, and `ConcurrentEnvironment` owns `Send` futures and native spawning. `NativeEnvironment` exposes native capabilities; the local WASM implementation lives in `taplo-wasm`.
- `schema/transport.rs` — `LocalSchemaTransport`, `ConcurrentSchemaTransport`, and `OfflineSchemaTransport` keep transport capability separate from schema interpretation. Native TLS features never leak into the browser transport.
- `schema/` — JSON Schema interpretation (compiled with the `schema` feature): `associations.rs` maps documents to schemas, `cache.rs` persists atomic cache entries, `ext.rs` owns Taplo schema extensions, and `mod.rs` performs iterative reference/composition traversal with deterministic cycle handling.
- `convert.rs`, `log.rs`, `util.rs` — typed TOML/JSON conversion, tracing setup, and shared utilities. Reusable boundaries return domain errors rather than `anyhow`.
