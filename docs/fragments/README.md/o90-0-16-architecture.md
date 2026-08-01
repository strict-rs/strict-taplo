## The `0.16` architecture

Taplo `0.16` is a coordinated breaking release over the maintained strict Logos and Rowan forks. Logos remains the lexer engine; the parser is fallible and non-recursive, the source-preserving DOM is frozen and `Send + Sync`, and source formatter/render entry points expose typed failures.

The language server has separate runtime contracts: native execution uses `ConcurrentServer` with `Arc` snapshots and `Send` request futures, while WebAssembly uses `LocalServer` with `Rc` ownership and local futures. Both share protocol lifecycle, ordered mutation barriers, immutable generation-checked snapshots, schema interpretation, and diagnostic behavior.

Repository-owned `just x` extensions run TOML 1.1 conformance, the complete WASM feature matrix, JavaScript wrapper builds, and Taplo self-formatting. See [`docs/migrations/0.16.md`](docs/migrations/0.16.md) for the API migration.
