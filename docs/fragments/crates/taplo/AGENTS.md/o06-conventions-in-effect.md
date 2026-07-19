## Conventions in effect

`lib.rs` exposes `HashMap`/`HashSet` aliases backed by the standard library's randomized `RandomState` and re-exports `rowan`. Read the edition, MSRV, and lint posture from the workspace and crate `Cargo.toml` rather than assuming them.

[`logos`]: https://github.com/maciejhirsz/logos
[`rowan`]: https://github.com/rust-analyzer/rowan
