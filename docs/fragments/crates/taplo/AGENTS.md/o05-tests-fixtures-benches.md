## Tests, fixtures, benches

- Hand-written tests live in `src/tests/` (`formatter.rs`, plus inline cases in `mod.rs`), gated behind `#[cfg(test)]` in `lib.rs`.
- `src/tests/generated/invalid.rs` is a **generated** file (included via `mod generated { mod invalid; }`). Do not hand-edit it — change the fixtures under the repo's `test-data/` or the `test-gen` generator (see `util/test-gen`) and regenerate.
- Benches (`benches/taplo.rs`, `benches/profile.rs`) use `harness = false` with `criterion` (and `pprof` on unix); `examples/parse.rs` is a minimal parse demo.
- Run one test: `cargo test -p taplo <name>`.
