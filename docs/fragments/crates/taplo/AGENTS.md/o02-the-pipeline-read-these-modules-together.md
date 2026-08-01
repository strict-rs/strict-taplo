## The pipeline (read these modules together)

The crate is organized around a **lossless syntax tree**: every byte, comment, and whitespace span of the input is preserved so the formatter can round-trip and edit in place.

- `parser/` — `parser::parse(src) -> Result<Parse, ParseFailure>`. A private [`logos`] `LexToken` lexer feeds an iterative parser and a fallible [`rowan`] builder. Rowan construction failures use `ParseFailure`; recoverable TOML syntax problems remain ordered `Parse::diagnostics()`.
- `syntax.rs` — the public `SyntaxKind` raw-kind newtype and total Rowan conversion shared by parser, DOM, and formatter. Its documented constants retain the historical numeric assignments, and unknown raw values remain representable.
- `dom/` — `Parse::into_dom()` performs mutable semantic construction and then freezes the result into immutable `Arc`-backed nodes with source anchors. Use `Node::get`, `get_key`, `get_index`, and `path`; missing lookup returns `None`, while malformed source is represented by `Node::Invalid`. `Node::validate()` reports semantic `dom::Diagnostic` values.
- `formatter/` — source entry points `format` and `format_with_path_scopes` return `Result<String, FormatError>` because they parse. `format_syntax` and `format_green` remain infallible over already-built trees.
- `dom/rewrite.rs` — source-preserving edits retain exact paths, fragments, pending patches, atomic overlap checks, and stable trivia attachment. `RewriteError::Parse` distinguishes Rowan construction failure from recoverable syntax and semantic diagnostics.
