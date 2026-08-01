<!-- Do not edit; generated file. -->

# Local Extension Guide

## Read This First

The local `stask` is the consumer-compiled repository-specific extension seam behind `just x <name>`. Standard commands belong exclusively to the installed `template` binary and the root `justfile` recipes that call it.

## Role of This Crate

- `src/main.rs` is a thin process entrypoint into the guarded extension runner.
- `src/lib.rs` exposes the repository's local extension composition.
- `src/extensions.rs` registers only commands unique to this repository beneath the single `x` surface.
- The registry may be empty; `just x` remains a controlled surface and reports that no local extensions are registered.
- Keep all standard formatting, lint, test, update, documentation, analysis, and Cargo workflows out of this crate.

## Shared-Source Boundary

Local extension code composes `template-core` and `template-stask`; it does not fork shared workflow behavior.

- Put generic command, runner, process, filesystem, Git, TOML, and output capabilities in `template-core`.
- Put reusable standard workflows in their owning `template-rs` domain crate and expose them through `template-cli`.
- Put repository-specific automation behind `just x <name>` and keep its parser, enum variant, and handler local.
- Do not depend on `template-cli`, `template-cargo`, `template-docs-engine`, `template-schema-engine`, `template-update`, or compile-time guidance content from the local extension crate.
- Test reusable behavior beside its owning implementation; local tests cover only extension composition and dispatch.

## Adding or Changing Automation

For a repository-specific command, define a typed argument parser and handler, lift it with `template_stask::extension_command(...)`, and assemble it through `template_stask::registry(...)`. Keep extension names unique and keep dispatch explicit.

If more than one repository needs the behavior, stop treating it as a local extension. Implement it in the capability-owning `template-rs` crate, add behavioral tests there, and register the finished standard command through `template-cli`.

Never add standard command aliases, delegation, or fallback execution to the local registry. A consumer build must not be required for standard repository maintenance.

## Extension Mechanism

- `template_stask::extension_command(...)` lifts one typed parser into a named nested extension.
- `template_stask::registry(...)` validates unique nested command identities and constructs the top-level `x` command set.
- `template_stask::empty_registry(...)` preserves the controlled `x` surface when the repository has no project command.
- `template_stask::run_with_extensions(...)` rejects installed command sets, validates the extension-only catalog, and executes it through `RunnerMode::Stask`.
- The runner parses `x --from <DIR>`, rebases `CommandContext::invocation_dir()`, splits passthrough tokens at the first bare `--`, and retains the `STASK_VIA_JUST=1` or `CI` direct-invocation guard.

Reject duplicate extension names during registry construction, before parsing or executing a handler. Do not mirror the installed catalog locally to detect collisions; surface classification keeps installed and extension command sets distinct.

## Command Execution

Use `CommandContext` and the generic process, filesystem, and output capabilities exposed by `template-core` instead of building shell strings or direct-printing from handlers. Pass programs and arguments separately, preserve the selected color policy, and keep effect boundaries observable in tests.

Classify non-zero exits in the layer that executes the process. Preserve source errors across the local adapter boundary rather than relabeling a dependency-owned failure. Use tolerant probes only when absence is an explicitly supported state and report the resulting skip clearly.

Keep extensions idempotent where they maintain files. Validate every target path before effects and do not let generated outputs escape the repository boundary.

## Errors and Output

Keep a source-preserving, `#[non_exhaustive]`, `thiserror`-based `XtaskError` and a local `Result<T>` alias when repository extensions need domain failures. Wrap lower-domain errors transparently; do not convert them to strings or ad-hoc `io::Error` values.

Adapt extension errors into the core runner only at command registration. Every public fallible function documents its errors.

Write user-visible output through `CommandContext::output()` and semantic roles such as `ok`, `info`, `warn`, `error`, `debug`, `probe`, and `skip`. Keep output stable and avoid raw ANSI or direct `println!`/`eprintln!` calls.

## Testing Expectations

Local tests return `Result<(), TestFailure>` and validate observable extension behavior:

- the exact top-level `x` surface and empty-registry outcome;
- successful typed argument parsing and handler dispatch;
- malformed arguments and unknown extensions;
- duplicate-name and installed-surface rejection before execution;
- `--from` rebasing and passthrough splitting;
- guarded invocation with and without `XTASK_VIA_JUST=1` or `CI`;
- process, filesystem, and output observations for both success and failure.

Use `template_core::testing` recording capabilities for reusable runner observations. Do not mirror standard command tests here, add source-text assertions, or introduce filler branches for coverage.

## Visibility and Dead Code

Expose only the local composition function and extension types required by the binary or integration tests. Before deleting an apparently unused seam, check the binary, root `just x` recipe, public registry contract, downstream consumer examples, and feature-gated test façade. Remove proven obsolete handlers, parsers, errors, tests, and dependencies together; do not leave ghost extension code behind.

## Boundaries

- Do not add lint-silencing attributes, compatibility command aliases, standard-command fallbacks, broad preludes, `mod.rs`, or `#[path]` wiring.
- Do not hand-edit generated Markdown or generated policy references; update their source inputs and run the owning installed workflow.
- Keep local production dependencies limited to `template-core` and `template-stask` unless a repository-specific extension has a genuine additional domain dependency.
- Add reusable dependencies to the owning shared crate, not the consumer extension adapter.
- Preserve the local `CLAUDE.md` companion as the exact `@./AGENTS.md` bridge when generated guidance manages this target.
