## The pipeline (read these modules together)

The crate is organized around a **lossless syntax tree**: every byte, comment, and whitespace span of the input is preserved so the formatter can round-trip and edit in place.

- `parser/` — `parser::parse(src) -> Parse`. A [`logos`] lexer (`syntax.rs` defines `SyntaxKind`) feeds a parser that builds a [`rowan`] green tree; `parser/macros.rs` holds the parser combinators. Parsing is **error-tolerant**: a tree is always produced, with syntax errors collected in `Parse::errors`.
- `syntax.rs` — the `SyntaxKind` token/node enum and the rowan language definition shared by parser, DOM, and formatter.
- `dom/` — `Parse::into_dom()` (`dom/from_syntax.rs`) wraps the tree into a `dom::Node` for data-oriented access: dotted-path indexing (`dom/index.rs`), serde (`dom/serde.rs`), TOML re-emission (`dom/to_toml.rs`), in-place edits (`dom/rewrite.rs`), and node types (`dom/node.rs`, `dom/node/nodes.rs`). `Node::validate()` reports **semantic** errors (duplicate keys, conflicting tables) as `dom::Error` (`dom/error.rs`).
- `formatter/` — `format`, `format_syntax`, `format_green`, `format_with_scopes`, and `format_with_path_scopes` rewrite the tree under a `formatter::Options`. `format_with_path_scopes` applies per-glob/per-key overrides — the mechanism behind `taplo.toml` `[[rule]]` scoping.
