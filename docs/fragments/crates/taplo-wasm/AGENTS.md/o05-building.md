## Building

The declared workspace toolchain installs `wasm32-unknown-unknown`. Use `just x wasm-matrix` for no-default, CLI-only, LSP-only, and combined feature checks. `just x js-build` performs an immutable JavaScript install, builds every wrapper package, and then exercises successful formatting/linting/local-LSP construction plus missing, thrown, rejected, malformed, and wrong-type JavaScript boundaries through `js/tests/wasm-boundaries.cjs`. Shared interpretation remains in platform-agnostic crates; keep browser/Node-specific capability glue here.
