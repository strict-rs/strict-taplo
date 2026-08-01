<!-- Do not edit; generated file. -->

# taplo-lsp-async

This library provides typed JSON-RPC/LSP protocol plumbing for two distinct execution models.

- `LocalServer<W>` and `LocalContext<W>` retain `Rc`, local futures, and non-`Send` worlds for WebAssembly/current-thread hosts.
- `ConcurrentServer<W>` and `ConcurrentContext<W>` use `Arc`, `Send` futures, ordered mutation barriers, and native stdio/TCP transports.

Both families share kind-separated typed handler registration, JSON-RPC classification, initialization/shutdown rules, cancellation, duplicate-ID rejection, outbound requests, deferred output, and checked source-position mapping. The crate has no Taplo dependency; `taplo-lsp` is its primary consumer.
