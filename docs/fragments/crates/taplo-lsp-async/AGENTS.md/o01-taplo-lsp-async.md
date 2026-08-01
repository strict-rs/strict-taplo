# taplo-lsp-async

Generic async LSP server scaffolding — JSON-RPC message plumbing, kind-separated request/notification routing, lifecycle transitions, cancellation, mutation ordering, and native framing — with **no dependency on Taplo**. `taplo-lsp` consumes both honest execution families: `LocalServer` for current-thread/non-`Send` hosts and `ConcurrentServer` for native multi-threaded hosts.
