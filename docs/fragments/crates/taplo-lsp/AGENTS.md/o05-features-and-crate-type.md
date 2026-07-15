## Features and crate type

- `default = rustls-tls`; `native-tls` is the alternative — both forward to `taplo-common`'s TLS features.
- `[lib] crate-type = ["cdylib", "rlib"]` — usable as a Rust dependency and as a C-ABI dynamic library.

Read exact versions from `Cargo.toml`.
