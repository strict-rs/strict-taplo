# Change Log

## 0.16.0

### Breaking Changes

- Make `parser::parse` return `Result<Parse, ParseFailure>` and expose recoverable syntax problems through `Parse::diagnostics()`.
- Replace the public Logos-derived token enum with a total `SyntaxKind` raw-kind newtype and a private Logos lexer token type.
- Freeze the DOM into immutable `Send + Sync` nodes, replace generic indexing with explicit optional queries, and rename semantic errors to `dom::Diagnostic`.
- Make source formatter entry points and DOM TOML rendering return typed `FormatError` and `RenderError` values.
- Add `RewriteError::Parse`, remove the obsolete rewrite error alias, and retain atomic source-preserving edit behavior.

### Reliability

- Parse arrays and inline tables with an explicit heap frame stack instead of recursive parser calls.
- Preserve malformed scalar source as typed `Node::Invalid` values rather than fabricating decoded fallbacks.
- Preflight DOM rendering so malformed nodes retain their invalid reason and semantically conflicting trees fail before output is written.
- Propagate every fallible Rowan builder operation and preserve unknown raw syntax kinds without assertions, casts, or unsafe conversion.
