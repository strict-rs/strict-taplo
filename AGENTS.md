# AGENTS.md

This file provides guidance to coding agents when working with code in this repository.

## What this repository is

Taplo is a TOML toolkit: a lossless parser, DOM, formatter, and linter, exposed as a Rust library, a `taplo` CLI, a language server, and WASM/Node bindings.

This checkout is a hard fork: `origin` is `git@github.com:strict-rs/strict-taplo.git` and `upstream` is `git@github.com:tamasfe/taplo.git`. Pull upstream changes through the `upstream` remote. Most workspace metadata (`authors`, `homepage`, `repository` in the root `Cargo.toml`) and the `README.md`/`CONTRIBUTING.md` still point at the upstream `tamasfe/taplo` project and its docs/CI badges — that is inherited content, not a bug to "fix."

This fork exists to be **refactored from upstream taplo into the strict-rs ecosystem conventions** (edition 2024, a `just`/`xtask`-driven workflow, the panic-free test vocabulary, and the rest of the strict template family's rules). Where a given piece of that conversion has not landed, the code still follows upstream taplo's conventions. Read the edition, resolver, MSRV, and toolchain actually in effect from the workspace `Cargo.toml` and CI config rather than assuming either baseline.

## Workspace layout

Rust crates live under `crates/` (`members = ["crates/*"]`). Listed bottom-up by dependency direction:

- `lsp-async-stub` — generic async LSP server scaffolding (JSON-RPC over tokio TCP/stdio). Independent of taplo; `taplo-lsp` builds on it.
- `taplo` — the core library: lexer, parser, syntax tree, DOM, and formatter. Synchronous, no I/O. Features: `serde` (default), `schema`.
- `taplo-common` — shared machinery for the tools: the `taplo.toml` config model (`config.rs`), JSON-Schema association/cache/validation (`schema/`, via `jsonschema`), and the `Environment` abstraction (`environment.rs`). TLS features `rustls-tls`/`native-tls`.
- `taplo-lsp` — the language server: one module per LSP request under `src/handlers/`, plus `world.rs` (server state). Built on `lsp-async-stub` + `taplo` + `taplo-common`.
- `taplo-cli` — the `taplo` binary (`bin/taplo.rs`). Subcommands in `src/commands/`. Depends on all of the above.
- `taplo-wasm` — `wasm32` bindings that bundle the CLI and LSP for browser/Node (`publish = false`).

`util/test-gen` is **excluded** from the workspace (`exclude = ["util/test-gen"]`) — a standalone `edition = "2018"` helper with its own `Cargo.lock`. Non-Rust trees: `js/` (yarn workspace of `@taplo/core|cli|lib|lsp`), `editors/vscode` (the "Even Better TOML" extension over `taplo-lsp`), `site/` (VitePress docs deployed to taplo.tamasfe.dev), `docker/` + `docker-bake.hcl` + `pyproject.toml` (Alpine/musl CLI binaries and maturin Python wheels).

## Core architecture (the `taplo` crate)

The library is built around a **lossless syntax tree** — every byte, comment, and whitespace of the input is preserved so the formatter can round-trip and edit in place. The pipeline spans several modules; read them together:

1. **Lex + parse** — `parser::parse(src) -> Parse`. A [`logos`] lexer produces `syntax::SyntaxKind` tokens; the parser builds a [`rowan`] green tree. `Parse` carries the tree plus a `Vec<parser::Error>` of syntax errors. Parsing is error-tolerant: a tree is produced even with syntax errors.
2. **DOM** — `Parse::into_dom()` (see `dom/from_syntax.rs`) wraps the syntax tree into a `dom::Node` tree for data-oriented access: dotted-path indexing (`dom/index.rs`), `serde` (de)serialization (`dom/serde.rs`), and TOML re-emission (`dom/to_toml.rs`). `Node::validate()` reports **semantic** errors (duplicate keys, conflicting tables) as `dom::Error`. Crucially, DOM/semantic errors are **separate** from parser syntax errors — check both.
3. **Format** — `formatter::{format, format_syntax, format_green, format_with_scopes, format_with_path_scopes}` rewrite the tree under a `formatter::Options`. `format_with_path_scopes` applies per-glob/per-key overrides — this is how `taplo.toml` `[[rule]]` blocks scope options to specific files and keys.

`taplo-common` layers configuration and schema handling on top: it loads `taplo.toml`, resolves JSON-Schema associations for files, and validates documents. Everything I/O-bound in `taplo-common`/`taplo-lsp`/`taplo-cli` is generic over the `taplo_common::environment::Environment` trait (`environment/native.rs` for native, a WASM impl in `taplo-wasm`), which abstracts the filesystem, clock, stdio, and HTTP so the same code runs natively and in the browser.

## Common commands

Run from the repo root unless noted.

- Build the CLI: `cargo build --bin taplo` (default features: `completions`, `lint`, `lsp`, `rustls-tls`, `toml-test`). Run it: `cargo run -- <args>`, e.g. `cargo run -- fmt --check`.
- Test the core library: `cargo test -p taplo`. Test the CLI: `cargo test -p taplo-cli`. Whole workspace: `cargo test`.
- Run a single test: `cargo test -p taplo <name>`, e.g. `cargo test -p taplo inline_table_with_linebreaks_and_trailing_comma`.
- WASM build check: `cd crates/taplo-wasm && cargo check --target wasm32-unknown-unknown` (needs `rustup target add wasm32-unknown-unknown`).
- MSRV check (matches CI): pin the toolchain to the `rust-version` declared in the workspace `Cargo.toml`, then `cargo check`/`cargo test` on the crates.
- Format Rust: `cargo fmt` (config in `rustfmt.toml`). Note CI does **not** gate on `cargo fmt` or `clippy`; there is no clippy lint gate in this repo.

## TOML conformance and self-formatting (what CI actually enforces)

- **toml-test**: build the decoder with `cargo build --bin taplo --no-default-features --features "rustls-tls,toml-test"`, then run the external [`toml-test`] binary against the `taplo toml-test` subcommand (it reads TOML on stdin, emits toml-test's tagged JSON). CI runs it with `-toml 1.1` and a skip-list of known-failing cases — see `.github/workflows/ci.yaml` for the current skip set before touching parser behavior.
- **Self-formatting (dogfood)**: `taplo` formats its own repo's TOML per the root `taplo.toml`. CI runs `taplo fmt --check` (and asserts `git diff-index --quiet HEAD --` after the core tests), so after any change the CLI formatter must produce **no diff** on the tree — the formatting is expected to be idempotent. If you change formatter output, run `cargo run -- fmt` and commit the reformatted TOML.

## Tests and fixtures

- Hand-written tests live in `crates/taplo/src/tests/` (`formatter.rs`, plus inline cases in `mod.rs`). `test-data/` holds `valid/` and `invalid/` TOML fixtures (with paired JSON/YAML for values), largely inherited from upstream spec test suites.
- `crates/taplo/src/tests/generated/invalid.rs` is a **generated** file included via `mod generated { mod invalid; }`. Regenerate it by running the excluded `util/test-gen` crate over `test-data/` (it emits one `#[test]` per fixture); do not hand-edit the generated file — change the fixtures or the generator and regenerate.

## Feature flags worth knowing

- `taplo-cli`: `rustls-tls` (default) and `native-tls` are the two TLS backends — swap, don't stack. `lint` pulls in `reqwest` + schema support; `lsp` requires `lint`; `toml-test` requires `lint`.
- `taplo`: `serde` (default) enables DOM serde; `schema` enables JSON-Schema generation for formatter `Options`.

[`logos`]: https://github.com/maciejhirsz/logos
[`rowan`]: https://github.com/rust-analyzer/rowan
[`toml-test`]: https://github.com/toml-lang/toml-test

## Commit messages

- Subject: `type(scope): structural imperative description` — conventional-commit style, scope **required**, `!` appended for breaking changes. **Never `chore`** — pick the precise type (`feat`, `fix`, `refactor`, `perf`, `build`, `ci`, `docs`, `test`, `style`, `revert`, …).
- Headline the substance: the subject names the most significant behavior/API change; renames, moves, lockfile bumps, and generated artifacts are fallout, never the headline when real behavior also changed.
- Body: 1–5 sections sized to the commit. Each section starts with a plain-text header line (no `#`, no bold), followed by 3–5 imperative bullets describing structural changes; exactly one blank line between sections; fallout goes in the last section.
- Pass the message via HEREDOC to `git commit -m` so blank lines survive shell quoting.
