## Features

- `rustls-tls` / `native-tls` — the two TLS backends for `reqwest`; **swap, don't stack**.
- `reqwest` — gates the optional `reqwest` dependency (HTTP schema fetching).
- `schema` — compiles the `schema` module.

`tokio` is target-gated: native builds pull `fs`/`io-std`/`rt`/`time`; `wasm32` builds pull a trimmed set. Read exact versions and feature wiring from `Cargo.toml`.
