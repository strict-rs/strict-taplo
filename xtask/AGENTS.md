<!-- Do not edit; generated file. -->

# xtask Agent Guide

## Read This First

This file is the agent and maintainer guide for the **`xtask`** crate; `README.md` is its overview and usage reference. The global workspace rules - the lint policy (`clippy.toml` plus the `[workspace.lints.*]` blocks of `Cargo.toml`), formatting (`rustfmt.toml`), panic-free testing, and the commit format - still apply here; they are spelled out in the repository-root `AGENTS.md`.

## Role of this crate

`xtask` is the repository automation adapter behind the canonical `just` command surface. Keep it focused on composition, entrypoints, and the project-owned `just x <name>` extension registry. Reusable primitives that should not churn template-consuming repositories belong in the owning `strict-xtask-*` crate (`core`, `cargo`, or `agents-md`), and the template-owned command surface belongs in `rust-template`.

Prefer small, explicit workflows over clever general runners. A new subcommand should make it obvious what external tools it calls, what files it reads or writes, and what failure means.

## Shared-source boundary

`xtask/` composes reusable command crates and project-owned extensions; it is not the place to fork shared workflow behavior. Product repositories should not edit built-in workflow files, template-owned `justfile` recipes, or reusable `strict-xtask-*` crates for repo-specific automation.

- **Reusable behavior stays in its owning crate.** `strict-xtask-core` owns language-agnostic runner behavior, extension parsing, color, output, context, and process helpers. `strict-xtask-cargo` owns Rust/Cargo workflows, `repo-overview`, agent helper reports, and command behavior tests. `strict-xtask-agents-md` owns generated Markdown engine behavior.
- **Repo-specific automation goes through `just x <name>`.** Register project commands through the project-owned extension registry and keep them outside built-in workflow command sets. The generic `just x` recipe is the stable public seam.
- **Template command definitions belong to `rust-template`.** If the template-owned `justfile` or minimal `xtask` composition needs a new reusable command surface, make that change in `rust-template` after the owning `strict-xtask-*` crate provides the behavior.
- **Shared tests follow shared behavior.** Tests for reusable command behavior belong with that behavior, especially in `strict-xtask-cargo-rs` for Rust/Cargo workflows and helper reports. Local extension tests should cover only the local extension contract.

## Adding or changing shared automation

> **Audience: maintainers changing shared automation itself.** Project-specific work should extend through `just x <name>` instead (see "Shared-source boundary" above).

For a new shared Rust/Cargo workflow, update the owning shared crate first, then expose it through the template command surface owned by `rust-template`. Keep argv parsing, process orchestration, reusable helpers, and tests in the owning crate rather than local project extension modules.

Use `strict-xtask-cargo` for Rust/Cargo workflows, `repo-overview`, agent helper reports, and command behavior tests. Use `strict-xtask-agents-md` for generated Markdown engine behavior. Use `strict-xtask-core` for runner, parser, color, command-observation, or extension-router behavior. Do not move reusable behavior into local `xtask` just because a consuming repo needs it.

## Extension mechanism

> **Audience: project extensions.** Maintainers changing `strict-xtask-core::extension` should work from the shared crate's module docs and tests.

The extension seam maps each project command's parsed arguments into a plain-data variant of the project's `extensions::ProjectCommand` enum. Dispatch stays explicit: `ProjectCommand::run` matches the selected variant and calls the owning module's handler.

- Each extension module owns its argument parser, command entry point, and command logic.
- `xtask/src/extensions.rs` owns the project command enum, dispatch `match`, and registry list.
- `xtask/src/lib.rs` only needs the module declaration for the new project-owned command.
- The generic `just x` recipe serves every extension, so the `justfile` stays stable.

The template registry may start empty. `just x` should still exist and report that no project extension commands are registered until a repository adds one.

## Command execution

Use `CommandContext::process()` / `ProcessRunner`, or the matching `Runtime` methods, instead of hand-built shell strings. Pass programs and arguments separately, and choose the appropriate `ToolColor` strategy (`CargoGlobal`, `CargoNextest`, `CargoLlvmCov`, captured-machine output, or no color) so forwarded tools respect the same color policy as `xtask`. Avoid `bash -c` unless the task is intrinsically shell behavior and there is no reasonable Rust or direct-process alternative.

Classify a non-zero exit in the layer that actually executes the process, using that layer's dedicated command-failure variant. Do not relabel a dependency-owned process failure as if the calling workflow had spawned it. Use tolerant execution only for expected optional probes, and emit a clear skip/status line when continuing after a failure.

Keep workflows idempotent. Re-running `just init`, `just gen-lint-template`, `just gen-md`, or similar maintenance commands should either produce the same files or a clear deterministic update. Validate output paths so generated files do not escape the workspace; mirror the existing `coverage --output` and generated-output parsing style for path guards.

## Errors and output

Keep the typed error model. Do not introduce `anyhow`, `std::io::Error::new`, `std::io::Error::other`, or stringly failure plumbing. Each reusable crate defines and owns its own `thiserror::Error` enum; when that crate gains a failure mode, add a dedicated variant with its `#[error("...")]` message as a user-visible contract, preserve underlying causes with `#[from]` / `#[source]`, and add a focused behavior test.

Wrap dependency-owned errors transparently instead of cloning their taxonomy. A workflow error enum may use a transparent `#[from]` variant for an owning dependency, but it must not exhaustively translate, duplicate, rename, or stringify the dependency's variants. Mark public error enums `#[non_exhaustive]` so downstream consumers cannot make lockstep exhaustive matches into a cross-crate coupling mechanism.

Adapt domain errors into the core runner only at command-registration, pre-commit, or equivalent execution boundaries, using the core API that preserves the original error as an `Error::source()`. Never convert a domain error to a message-only fallback merely to satisfy the runner's return type.

All fallible public functions need a `# Errors` doc section. Prefer private helpers for detailed phases so the public function reads as the command's high-level contract.

Write user-visible output through `CommandContext::output()` / `Output`, not `println!` or `eprintln!`. Prefer semantic status roles (`ok`, `info`, `warn`, `error`, `debug`, `probe`, `skip`) over embedding raw ANSI strings. Status lines should be short and stable enough for humans and scripts to recognize.

## Testing expectations

Add tests with the change. The usual layers are:

- Unit tests next to pure helpers in the same source file.
- `cmd.rs` tests using the fake `Runtime` for command order, arguments, and failure behavior.
- `tests/cli.rs` integration tests for the compiled binary, public help, direct-invocation guard behavior, parse validation, and black-box command behavior.

When a command shells out, prefer fake tools in integration fixtures instead of requiring the real external program. Cover at least one failure path for new process orchestration.

**No raw-fd leaks from tests.** A test that drives a real external program through an fd-inheriting runner inherits the test process's stdout/stderr, so a chatty program can leak straight to the console even when the test passes. Put that call in an `#[ignore]`d child test and drive it from a parent through the shared capture helper, asserting on the captured status and stderr.

**Coverage is an obligation on every file you touch (non-negotiable).** Every source you create or edit in this crate must reach **at least 90% on every coverage measure** — regions, functions, lines, and (when the active mode measures them) branches. The floors are a uniform 90% on both the TOTAL row and, under `just coverage --per-file`, every individual file row, so one file below 90% on one measure fails the run. The percentage is necessary but not sufficient; the rules below are what a number cannot check:

- **Pre-existing gaps are in scope.** A file that was already under 90%, or under-covered lines you did not write, is still yours to bring to 90% on every measure once you touch the file — "it was already like that" and "that is not my code" are not exemptions.
- **Both polarities, every behavior.** Pin what *does* happen and what *does not*: fake process runners record the exact command sequence on the success path and workflows short-circuit with typed errors on the failure path; a parser accepts the valid argv and rejects the malformed one at parse time; a `Display` impl renders its present-field form and its absent-field form.
- **No coverage-gaming, no filler.** Every test asserts a real, observable behavioral contract; a test that runs code only to move the percentage or pins an incidental detail is worse than none. Write the test the behavior deserves, then verify with `just coverage --per-file` and read the rows for the files you touched.

Run `just fmt && just check && just test` before handing off. If the change touches docs examples, also run `just test-doc`; if it changes CI/precommit composition or core tooling behavior, prefer `just ci`, which includes the normal per-file coverage gate and the per-feature union coverage gate. When you have added or changed code but are not running the full CI mirror, also run `just coverage --per-file` and confirm every file you touched clears the floors.

## Visibility is the consumer contract (this is a template)

Repos generated from this template call into `xtask`, so `pub` vs `pub(crate)` encodes the *intended consumer surface*, not what this repo happens to exercise. An item being unreferenced outside its module **here** is never grounds to make it `pub(crate)`, delete it, or drop a doc link to it — a consuming repo may be the caller. When a public `//!` / `///` doc links a `pub(crate)` item (`rustdoc::private_intra_doc_links`, deny — fires only during `cargo doc` / `just doc`), make the item genuinely `pub`, along with any type in its signature (else `private_interfaces` / E0446), rather than demoting the link to a code span; public value-returning fns then take `#[must_use]` (`must_use_candidate`, deny). Decide visibility by the intended consumer surface — expose the composable building blocks the module `//!` doc advertises, keep only true sub-helpers `pub(crate)` — and verify with `just doc && just lint`.

## Boundaries

Do not add new lint-silencing attributes. If an existing scoped exception is near the code you are editing, keep it narrow and do not copy it into new files.

Do not hand-edit generated reference files unless the task is specifically to change generated output; update the generator and run the generator instead.

Dependencies are appropriate when they remove real parsing, format, traversal, or platform complexity and pass the workspace supply-chain policy. Add them deliberately at the root `[workspace.dependencies]`, opt the owning crate into them with `.workspace = true`, and document why they beat hand-rolled maintenance code. If the dependency supports reusable xtask primitives, prefer the relevant `strict-xtask-*` crate over adding another direct `xtask` dependency; `xtask` should compose and adapt rather than own reusable implementation by default.
