## Runtime families

Use `create_concurrent_server` and `create_concurrent_world` for native multi-threaded execution. Use `create_local_server` and `create_local_world` for WebAssembly or another current-thread executor. The two families share protocol and state-transition behavior but deliberately expose different ownership and future bounds.
