set shell := ["bash", "-eu", "-o", "pipefail", "-c"]
# Local Windows developers get PowerShell (always present) instead of just's
# default `sh`; workflow recipes stay as plain `cargo xtask ...` one-liners, so
# the same lines run under both shells.
set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]

# `just` is the canonical entry point. xtask refuses a direct
# `cargo xtask <sub>` invocation unless this env var is set (or `CI` is).
# See `xtask/src/main.rs::guard_invocation`.
export XTASK_VIA_JUST := "1"

# This justfile is the canonical repo entry point and command index for
# developers, AI agents, and local automation. Apart from the default listing
# recipe, recipes stay as thin dispatchers into the composed `cargo xtask`
# runner.
#
# Reusable workflow behavior lives in the owning external `strict-xtask-*`
# crates. Local `xtask/src/` is composition glue plus the `just x` extension
# seam, so automation remains compiled, linted, and tested as Rust instead of
# growing inside this file.
#
# Most repositories should add their own automation through the local extension
# registry and run it as `just x <name>`. Do not add repo-specific commands to
# the shared `strict-xtask-*` crates.
#
# Only reusable template-wide workflows belong in `strict-xtask-*`. When you are
# deliberately maintaining that shared surface, change the owning crate first,
# then expose the composed command with a one-line recipe here.
#
# `just --list` renders the contiguous comment immediately above each recipe.
# Keep recipe-adjacent comments to one short user-facing help sentence; put
# maintainer notes above a blank line so they do not leak into the command list.

# List available repository commands.
default:
    @just --list --unsorted

# Bootstrap toolchains, targets, cargo tools, and the local pre-commit hook.
init:
    cargo xtask init

# Scaffold a workspace crate with `just new <name>` or `just new <name> --bin`.
new *args:
    cargo xtask new {{args}}

# The everyday code commands forward all args verbatim and always pass the
# directory you ran `just` from as `--from`. With no args they run the whole
# workspace; an optional leading PATH scopes to the crate that owns it (resolved
# against `--from`); anything after `--` is forwarded to the underlying cargo
# tool. Examples: `just lint`, `just lint src`, `just lint src -- --fix`.

# Format the workspace or owning crate with nightly rustfmt.
fmt *args:
    cargo xtask fmt --from "{{invocation_directory()}}" {{args}}

# Type-check the workspace or owning crate.
check *args:
    cargo xtask check --from "{{invocation_directory()}}" {{args}}

# Run strict Clippy on the workspace or owning crate.
lint *args:
    cargo xtask lint --from "{{invocation_directory()}}" {{args}}

# Reject forbidden `#[allow(...)]` and `#[expect(...)]` attributes.
lint-attrs:
    cargo xtask lint-attrs

# Run nextest unit and integration tests.
test *args:
    cargo xtask test --from "{{invocation_directory()}}" {{args}}

# Run doctests with `cargo test --doc`.
test-doc *args:
    cargo xtask test-doc --from "{{invocation_directory()}}" {{args}}

# Run nextest and doctests together.
test-all *args:
    cargo xtask test-all --from "{{invocation_directory()}}" {{args}}

# Generate and gate cargo-llvm-cov coverage reports.
coverage *args:
    cargo xtask coverage {{args}}

# Run union coverage across the each-feature matrix.
coverage-per-feature:
    cargo xtask coverage-per-feature

# Build docs for the workspace or owning crate.
doc *args:
    cargo xtask doc --from "{{invocation_directory()}}" {{args}}

# Run cargo-audit and cargo-deny supply-chain checks.
audit:
    cargo xtask audit

# Detect unused dependencies with cargo-machete and cargo-udeps.
deadcode:
    cargo xtask deadcode

# Run the cargo-hack feature-matrix gate.
hack:
    cargo xtask hack

# Run the full local CI mirror in CI order.
ci:
    cargo xtask ci

# Run the pre-commit gate and stage generated metrics and badges.
precommit:
    cargo xtask precommit

# Rebuild and reinstall the worktree-private pre-commit hook.
precommit-refresh:
    cargo xtask precommit-refresh

# Upgrade manifest dependency requirements and refresh the lockfile.
upgrade:
    cargo xtask upgrade

# Refresh transitive lockfile dependencies without changing manifests.
update:
    cargo xtask update

# Report outdated workspace dependencies.
outdated:
    cargo xtask outdated

# Clone the latest template snapshot and apply declared config/dependency lanes.
# A successful URL run stores `rust-template.url`; later runs reuse it.

# Apply a template snapshot without merging template history.
update-template *args:
    cargo xtask update-template {{args}}

# Regenerate `template-lint.toml` and `template-clippy.toml`.
gen-lint-template *args:
    cargo xtask gen-lint-template {{args}}

# Move legacy AGENTS.md fragments into target filename directories.
migrate-agent-guidance-fragments:
    cargo xtask migrate-agent-guidance-fragments

# Regenerate generated Markdown docs from configured fragments.
gen-agent-guidance *args:
    cargo xtask gen-agent-guidance {{args}}

# Render live repo structure, command, gate, and generated-doc facts.
repo-overview *args:
    cargo xtask repo-overview {{args}}

# Render agent helper reports for gates and staged changes.
agent *args:
    cargo xtask agent {{args}}

# Run cyclomatic, duplication, and tokei code-quality checks.
cq:
    cargo xtask cq

# Rerun the test suite with SNAPSHOTS=overwrite so `ensure_snapshot` rewrites
# stale snapshot files in place; review the result with `git diff`.

# Refresh committed snapshot files.
snap-update:
    cargo xtask snap-update

# Rerun the test suite with STRICT_TEST_SEED=random so
# `proptest::strict::ensure_property` draws fresh entropy instead of its fixed
# seed; pin any counterexample as a named unit test. Replay a specific seed with
# `STRICT_TEST_SEED=<n> just test`.

# Rerun property tests with a random strict-test seed.
fuzz:
    cargo xtask fuzz

# Mutation testing via cargo-mutants (the anti-filler gate): rewrites the source
# one mutation at a time and reruns the suite; a surviving mutant is code a test
# reached without checking, i.e. a missing assertion.
#
# The runner first lists candidates and adds exact cfg-inactive excludes for
# source that cannot compile under the effective target/features/Cargo cfg
# configuration, then mutates the workspace; scope a PR with
# `just mutants -- --in-diff <file>` or shard a big run with
# `just mutants -- --shard 1/4`.

# Run the default cargo-mutants gate with cfg-aware excludes.
mutants *args:
    cargo xtask mutants {{args}}

# Distributed mutation testing via the shared strict-xtask-cargo command. It
# reuses the same cfg-aware preflight as `just mutants`, then passes the
# generated excludes into the fleet planner for the invoking consumer repo.

# Plan distributed mutation work with the shared fleet runner.
mutant-fleet *args:
    cargo xtask mutant-fleet {{args}}

# Run the active-code mutation matrix across feature configurations.
comprehensive-mutants *args:
    cargo xtask comprehensive-mutants {{args}}

# Regenerate the committed README badges under badges/ from tracked gate
# outputs (cov/summary.txt, cov/tokei.json) and policy files.

# Regenerate committed README badges from tracked gate outputs.
badges:
    cargo xtask badges

# Cross-compile release artifacts via cargo zigbuild (zig as the cross
# linker) for the default target set; pass --target <TRIPLE> (repeatable,
# glibc-suffix form accepted) to build a different set.

# Cross-compile release artifacts via cargo-zigbuild.
cross *args:
    cargo xtask cross {{args}}

# Run a consumer-registered extension command (registry: xtask/src/extensions.rs).
# `--from` is always forwarded; the handler reads it via `CommandContext::invocation_dir()`.
# Pass an extension's own dashed flags after `--`:  just x release-notes -- --since v1.2

# Run a consumer-registered extension command.
x *args:
    cargo xtask x --from "{{invocation_directory()}}" {{args}}

# Print the bpaf-rendered xtask command help.
help:
    cargo xtask help
# Split existing Markdown doc(s) into consumer fragments and regenerate them
# in place: `just migrate-md [PATH]`. PATH may be a file or a directory
# (recursive); with no PATH it self-heals legacy fragment layouts instead.
migrate-md *args:
    cargo xtask migrate-md --from "{{invocation_directory()}}" {{args}}

# Regenerate generated Markdown docs from configured fragments.
gen-md *args:
    cargo xtask gen-md {{args}}

