## Tests, fixtures, benches

- Hand-written tests live in `src/tests/` (`formatter.rs`, `fixtures.rs`, `properties.rs`, plus focused cases in `mod.rs`), gated behind `#[cfg(test)]` in `lib.rs`.
- `fixtures.rs` walks sorted runtime corpora under `test-data/invalid/` and `test-data/valid/`. Every invalid fixture must produce a syntax or semantic diagnostic; every valid fixture must remain clean. There is no generated compiled fixture module or `util/test-gen` owner.
- Strict property tests execute through `proptest::strict::ensure_property` and cover parse/format/reparse stability, semantic path round trips, and non-overlapping rewrite application.
- Benches (`benches/taplo.rs`, `benches/profile.rs`) use `harness = false` with `criterion` (and `pprof` on unix); `examples/parse.rs` is a minimal parse demo.
- Run one test: `cargo test -p taplo <name>`.
