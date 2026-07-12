## Conventions in effect

`lib.rs` exposes `HashMap`/`HashSet` aliases over `ahash` and re-exports `rowan`. Read the edition, MSRV, and lint posture from the workspace and crate `Cargo.toml` rather than assuming them — this crate still follows upstream taplo conventions where the strict-rs conversion has not yet landed.

[`logos`]: https://github.com/maciejhirsz/logos
[`rowan`]: https://github.com/rust-analyzer/rowan
