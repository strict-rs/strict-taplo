<!-- Do not edit; generated file. -->

# taplo

The core library: a lossless TOML lexer, parser, syntax tree, DOM, and formatter. Synchronous and free of I/O — every other crate in the workspace builds on it.

## The pipeline (read these modules together)

The crate is organized around a **lossless syntax tree**: every byte, comment, and whitespace span of the input is preserved so the formatter can round-trip and edit in place.

- `parser/` — `parser::parse(src) -> Parse`. A [`logos`] lexer (`syntax.rs` defines `SyntaxKind`) feeds a parser that builds a [`rowan`] green tree; `parser/macros.rs` holds the parser combinators. Parsing is **error-tolerant**: a tree is always produced, with syntax errors collected in `Parse::errors`.
- `syntax.rs` — the `SyntaxKind` token/node enum and the rowan language definition shared by parser, DOM, and formatter.
- `dom/` — `Parse::into_dom()` (`dom/from_syntax.rs`) wraps the tree into a `dom::Node` for data-oriented access: dotted-path indexing (`dom/index.rs`), serde (`dom/serde.rs`), TOML re-emission (`dom/to_toml.rs`), in-place edits (`dom/rewrite.rs`), and node types (`dom/node.rs`, `dom/node/nodes.rs`). `Node::validate()` reports **semantic** errors (duplicate keys, conflicting tables) as `dom::Error` (`dom/error.rs`).
- `formatter/` — `format`, `format_syntax`, `format_green`, `format_with_scopes`, and `format_with_path_scopes` rewrite the tree under a `formatter::Options`. `format_with_path_scopes` applies per-glob/per-key overrides — the mechanism behind `taplo.toml` `[[rule]]` scoping.

## Two independent error channels

Syntax errors (`Parse::errors`) and DOM/semantic errors (`Node::validate()` / `Node::errors()`) are **separate** — a document can parse cleanly yet fail validation, and a DOM is still built when syntax errors are present. Any code that "checks for errors" must consult both.

## Features

- `serde` (default) — serde (de)serialization of DOM nodes (`dom/serde.rs`).
- `schema` — JSON-Schema generation (`schemars`) for the formatter `Options`.

Read exact versions and the full dependency set from `Cargo.toml`.

## Tests, fixtures, benches

- Hand-written tests live in `src/tests/` (`formatter.rs`, plus inline cases in `mod.rs`), gated behind `#[cfg(test)]` in `lib.rs`.
- `src/tests/generated/invalid.rs` is a **generated** file (included via `mod generated { mod invalid; }`). Do not hand-edit it — change the fixtures under the repo's `test-data/` or the `test-gen` generator (see `util/test-gen`) and regenerate.
- Benches (`benches/taplo.rs`, `benches/profile.rs`) use `harness = false` with `criterion` (and `pprof` on unix); `examples/parse.rs` is a minimal parse demo.
- Run one test: `cargo test -p taplo <name>`.

## Conventions in effect

`lib.rs` exposes `HashMap`/`HashSet` aliases over `ahash` and re-exports `rowan`. Read the edition, MSRV, and lint posture from the workspace and crate `Cargo.toml` rather than assuming them — this crate still follows upstream taplo conventions where the strict-rs conversion has not yet landed.

[`logos`]: https://github.com/maciejhirsz/logos
[`rowan`]: https://github.com/rust-analyzer/rowan
