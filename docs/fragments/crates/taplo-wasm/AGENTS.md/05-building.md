## Building

Check with `cargo check --target wasm32-unknown-unknown` (add the target via `rustup target add wasm32-unknown-unknown` first). Because this is the WASM half of the `Environment` abstraction, all shared logic lives in the platform-agnostic crates — keep browser/Node-specific glue here.
