## Features

- `default = ["completions", "lint", "lsp", "rustls-tls", "toml-test"]`.
- Dependency graph between features: `lsp` requires `lint`; `toml-test` requires `lint`; `lint` pulls in `reqwest` plus `taplo-common`'s schema support. `completions` gates `clap_complete`.
- `rustls-tls` vs `native-tls` — the two TLS backends; **swap, don't stack**.

Read exact versions from `Cargo.toml`.
