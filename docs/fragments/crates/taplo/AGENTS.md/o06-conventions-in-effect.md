## Conventions in effect

`lib.rs` exposes `HashMap`/`HashSet` aliases backed by the standard library's randomized `RandomState` and re-exports `rowan`. The workspace consumes the maintained strict forks of Logos and Rowan; do not replace the Logos lexer with a local scanner or duplicate TOML implementation. Read the edition, MSRV, and lint posture from the workspace and crate `Cargo.toml` rather than assuming them.

[`logos`]: https://github.com/strict-rs/strict-logos
[`rowan`]: https://github.com/strict-rs/strict-rowan
