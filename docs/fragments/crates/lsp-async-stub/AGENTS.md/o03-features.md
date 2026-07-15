## Features

- `tokio-stdio` — stdio transport (pulls `tokio` with `io-std`/`io-util`/`macros`/`rt`).
- `tokio-tcp` — TCP transport (pulls `tokio` with `io-util`/`macros`/`net`/`rt`).

`default` enables neither, so the bare crate is transport-agnostic and the `tokio` dependency is optional. Consumers select a transport via features (e.g. `taplo-cli` enables both). Read exact versions from `Cargo.toml`.
