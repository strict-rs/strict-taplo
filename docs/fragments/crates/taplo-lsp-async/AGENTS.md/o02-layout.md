## Layout

- `lib.rs` — common JSON-RPC classification, lifecycle state, local/concurrent cancellation states, and the ordered mutation barrier.
- `runtime.rs` — the `LocalServer`/`LocalContext` and `ConcurrentServer`/`ConcurrentContext` families, their private kind-separated handler adapters and registries, response handling, deferred output, and request-versus-mutation scheduling.
- `rpc.rs` — JSON-RPC 2.0 message types (requests, responses, notifications, errors).
- `util.rs` — checked UTF-8/UTF-16/UTF-32 source-position mapping using line/checkpoint tables and protocol-native LSP coordinates.
- `listen.rs` — checked native message framing, task/error propagation, and private stdio/TCP transport implementations behind feature-gated inherent `ConcurrentServer` entry points.
