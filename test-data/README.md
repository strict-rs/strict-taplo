# TOML behavior fixtures

The `invalid/` corpus contains TOML source that must produce at least one recoverable parser or semantic DOM diagnostic. The library still constructs a lossless syntax tree for every fixture so editor recovery remains exercised.

The `valid/` corpus contains representative TOML 1.1 documents that must produce neither parser nor semantic DOM diagnostics. It covers scalar forms, strings, dotted keys, tables, inline tables, arrays, arrays of tables, comments, and Unicode.

The `taplo` crate discovers both directories at test runtime, filters for `.toml` files, sorts the paths deterministically, and evaluates every fixture. The corpus is deliberately data-driven; do not regenerate a compiled Rust module from these files.

The `analytics/` and `rewrite/` directories remain owned by their existing specialized behavior suites. Full TOML language conformance is exercised separately by the checksum-pinned `just x toml-conformance` repository extension.
