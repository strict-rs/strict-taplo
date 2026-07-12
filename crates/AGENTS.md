<!-- Do not edit; generated file. -->

# Workspace Crates Guide

## Read This First

This file adds members-directory guidance on top of the repository-level `AGENTS.md`. Use the root file for global workflow, lint, formatting, testing, and commit rules.

## Purpose of this tree

This directory is the home for the workspace's Rust crates (the template default location is `crates/`; repositories with a different declared layout render this guide at their declared members directory). Optimize for clear boundaries, explicit assumptions, and tests that describe supported behavior.

Do not use this tree to bypass workspace policy. Every crate compiles under the same production-grade constraints from the first commit.

## Adding a crate

- Prefer a library crate for domain logic, algorithms, parsers, engines, protocol models, and reusable services. Add a binary crate only when the request needs an executable entry point, CLI, daemon, or demo runner.
- Name crates for the domain or capability they provide, not for temporary status. Avoid names like `tmp` or `scratch`; a good crate name stays accurate as code matures.
- Keep each crate narrow. If two ideas have different data models, dependencies, or release paths, put them in separate crates and compose them through explicit path dependencies.
- Start every crate with crate-level docs that state what the crate does, what is intentionally out of scope, and its main invariants or assumptions.
- In `Cargo.toml`, inherit workspace metadata, set `publish.workspace = true`, and include `[lints] workspace = true`. Declare dependencies in the root `[workspace.dependencies]` first, then opt into them from each crate with `{name}.workspace = true` under `[dependencies]`, `[dev-dependencies]`, or `[build-dependencies]`.

## Crate shape

- Put the durable core behind small, documented public types and functions. Everything else should stay private or `pub(crate)` until another crate genuinely needs it.
- Keep side effects at the edges. Prefer pure functions for parsing, planning, validation, scoring, transforms, and state transitions; put filesystem, network, environment, clock, and process access behind narrow boundary functions or traits.
- Model uncertainty explicitly. Use enums, result variants, configuration structs, and capability flags for incomplete behavior rather than hidden globals, panics, magic strings, or comments that only explain what the code does not do yet.
- Keep assumptions close to executable evidence. When a crate depends on a simplifying assumption, encode that assumption in tests, fixture names, type names, or module docs so future agents can tell whether it still holds.
- Avoid broad abstraction early. Add traits, generic layers, and feature flags only when at least two concrete call sites need the split or the boundary is part of the public contract.

## Module layout

- Use flat Rust module files such as `parser.rs`, `model.rs`, `engine.rs`, or `runner.rs`; do not create `foo/mod.rs`.
- Keep module names domain-specific once the shape is known. Generic names are acceptable for a first tiny crate, but replace them when they start hiding meaning.
- Keep test fixtures inside the owning crate, such as `tests/fixtures/` for integration fixtures or a small local helper module beside the tested code. Do not create shared fixture infrastructure until more than one crate needs it.
- If a crate exposes a binary and a library, make the binary a thin adapter over the library. The library should own behavior; `main.rs` should own argument/environment translation and exit-code mapping.

## Public API and errors

- Treat every `pub` item as a future compatibility promise. Document it, test it, and keep it as small as the current caller needs.
- Prefer concrete types and typed errors over stringly APIs. If a crate needs to report multiple failure modes, add a crate-specific error enum early and let tests pin its important behavior.
- Use `Option` only for genuine absence. Use `Result` for operations that can fail, and use domain enums when callers need to know which non-error state occurred.
- Keep conversion boundaries explicit with `From` and `TryFrom` implementations when they clarify ownership or validation. Do not add blanket conversions that make invalid states easy to construct.

## Testing

- Every crate should include at least one small test that demonstrates core behavior and one test that captures an important edge case or rejected input.
- Put unit tests inside the same source file as the code they cover. Use integration tests when exercising public crate behavior, binary behavior, fixture-driven flows, or cross-module contracts.
- Tests are panic-free like the rest of the workspace: `assert!`/`assert_eq!`/`assert_ne!`/`debug_assert*`, `unwrap`, `expect`, and `panic!` are all denied in tests too. Write `#[test] fn name() -> Result<(), strict_test_support::TestFailure>` and report failure by returning `Err` via the shared `strict-test-support` `ensure*` helpers; its `src/lib.rs` module docs are the source of truth for the vocabulary (the `ensure*` family and the self-cleaning `TempDir` fixture). Add `strict-test-support.workspace = true` under `[dev-dependencies]` (it is declared once in the root `[workspace.dependencies]`). `strict-test-support` is the one sanctioned piece of shared test infrastructure; keep all other fixtures crate-local.
- Prefer tiny hand-written fixtures. Large copied fixtures are acceptable only when the exact external shape matters; in that case, document where the fixture came from and why it is representative.
- Expand tests before widening the public API or adding downstream callers.

## Maintenance checklist

- The crate name, crate docs, and public API describe the implementation.
- Simplifying assumptions are either removed, made configurable, or documented as supported constraints.
- Public functions have examples where examples would help users discover the API.
- Error variants are actionable enough for callers and tests cover the important failure modes.
- Dependencies are intentional, licensed under the workspace policy, declared in the root `[workspace.dependencies]`, and opted into by crates that use them.
- README or root documentation is updated if the crate becomes a supported workspace surface.
