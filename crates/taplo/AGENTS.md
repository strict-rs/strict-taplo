<!-- Do not edit; generated file. -->

# taplo

The core library: a lossless TOML lexer, parser, syntax tree, DOM, and formatter. Synchronous and free of I/O — every other crate in the workspace builds on it.

## The pipeline (read these modules together)

The crate is organized around a **lossless syntax tree**: every byte, comment, and whitespace span of the input is preserved so the formatter can round-trip and edit in place.

- `parser/` — `parser::parse(src) -> Result<Parse, ParseFailure>`. A private [`logos`] `LexToken` lexer feeds an iterative parser and a fallible [`rowan`] builder. Rowan construction failures use `ParseFailure`; recoverable TOML syntax problems remain ordered `Parse::diagnostics()`.
- `syntax.rs` — the public `SyntaxKind` raw-kind newtype and total Rowan conversion shared by parser, DOM, and formatter. Its documented constants retain the historical numeric assignments, and unknown raw values remain representable.
- `dom/` — `Parse::into_dom()` performs mutable semantic construction and then freezes the result into immutable `Arc`-backed nodes with source anchors. Use `Node::get`, `get_key`, `get_index`, and `path`; missing lookup returns `None`, while malformed source is represented by `Node::Invalid`. `Node::validate()` reports semantic `dom::Diagnostic` values.
- `formatter/` — source entry points `format` and `format_with_path_scopes` return `Result<String, FormatError>` because they parse. `format_syntax` and `format_green` remain infallible over already-built trees.
- `dom/rewrite.rs` — source-preserving edits retain exact paths, fragments, pending patches, atomic overlap checks, and stable trivia attachment. `RewriteError::Parse` distinguishes Rowan construction failure from recoverable syntax and semantic diagnostics.

## Two independent error channels

Tree-construction failures (`ParseFailure`), recoverable syntax diagnostics (`Parse::diagnostics()`), and DOM diagnostics (`Node::validate()` / `Node::errors()`) are **separate**. A document can build a tree yet contain syntax diagnostics, can parse cleanly yet fail semantic validation, and can still produce a frozen DOM for editor recovery. Any code that requires valid TOML must consult all applicable channels.

## Features

- `serde` (default) — serde (de)serialization of DOM nodes (`dom/serde.rs`).
- `schema` — JSON-Schema generation (`schemars`) for the formatter `Options`.

Read exact versions and the full dependency set from `Cargo.toml`.

## Tests, fixtures, benches

- Hand-written tests live in `src/tests/` (`formatter.rs`, `fixtures.rs`, `properties.rs`, plus focused cases in `mod.rs`), gated behind `#[cfg(test)]` in `lib.rs`.
- `fixtures.rs` walks sorted runtime corpora under `test-data/invalid/` and `test-data/valid/`. Every invalid fixture must produce a syntax or semantic diagnostic; every valid fixture must remain clean. There is no generated compiled fixture module or `util/test-gen` owner.
- Strict property tests execute through `proptest::strict::ensure_property` and cover parse/format/reparse stability, semantic path round trips, and non-overlapping rewrite application.
- Benches (`benches/taplo.rs`, `benches/profile.rs`) use `harness = false` with `criterion` (and `pprof` on unix); `examples/parse.rs` is a minimal parse demo.
- Run one test: `cargo test -p taplo <name>`.

## Conventions in effect

`lib.rs` exposes `HashMap`/`HashSet` aliases backed by the standard library's randomized `RandomState` and re-exports `rowan`. The workspace consumes the maintained strict forks of Logos and Rowan; do not replace the Logos lexer with a local scanner or duplicate TOML implementation. Read the edition, MSRV, and lint posture from the workspace and crate `Cargo.toml` rather than assuming them.

[`logos`]: https://github.com/strict-rs/strict-logos
[`rowan`]: https://github.com/strict-rs/strict-rowan
