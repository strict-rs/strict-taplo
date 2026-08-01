set shell := ["bash", "-eu", "-o", "pipefail", "-c"]
# Local Windows developers get PowerShell (always present) instead of just's
# default `sh`; installed `template` workflows remain plain invocations
# under both shells.
set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]

# `just x` is the canonical entry point for the consumer-compiled extension
# runner. The installed `template` binary permits direct invocation.
export STASK_VIA_JUST := "1"

# This justfile is the canonical repository entry point and command index.
# Standard recipes are thin dispatchers into the installed `template` binary.
# Local `stask/src/` compiles only the repository-specific `just x` extension
# seam, so extensions cannot collide with or mirror standard commands.
#
# Reusable workflow behavior belongs in its owning `template-rs` crate.
# Repository-specific automation belongs in the local extension registry.
#
# `just --list` renders the contiguous comment immediately above each recipe.
# Keep recipe-adjacent comments to one short user-facing help sentence.

# List available repository commands.
default:
    @just --list --unsorted

# Bootstrap toolchains, targets, cargo tools, and the local pre-commit hook.
init:
    template init

# Scaffold a workspace crate with `just new <name>` or `just new <name> --bin`.
new *args:
    template new {{args}}

# The everyday code commands forward all args verbatim and always pass the
# directory you ran `just` from as `--from`. With no args they run the whole
# workspace; an optional leading PATH scopes to the crate that owns it (resolved
# against `--from`); anything after `--` is forwarded to the underlying cargo
# tool. Examples: `just lint`, `just lint src`, `just lint src -- --fix`.

# Format the workspace or owning crate with nightly rustfmt.
fmt *args:
    template fmt --from "{{invocation_directory()}}" {{args}}

# Type-check the workspace or owning crate.
check *args:
    template check --from "{{invocation_directory()}}" {{args}}

# Run strict Clippy on the workspace or owning crate.
lint *args:
    template lint --from "{{invocation_directory()}}" {{args}}

# Reject forbidden `#[allow(...)]` and `#[expect(...)]` attributes.
lint-attrs:
    template lint-attrs

# Run nextest unit and integration tests.
test *args:
    template test --from "{{invocation_directory()}}" {{args}}

# Run doctests with `cargo test --doc`.
test-doc *args:
    template test-doc --from "{{invocation_directory()}}" {{args}}

# Run nextest and doctests together.
test-all *args:
    template test-all --from "{{invocation_directory()}}" {{args}}

# Generate and gate cargo-llvm-cov coverage reports.
coverage *args:
    template coverage {{args}}

# Run union coverage across the each-feature matrix.
coverage-per-feature:
    template coverage-per-feature

# Build docs for the workspace or owning crate.
doc *args:
    template doc --from "{{invocation_directory()}}" {{args}}

# Run cargo-audit and cargo-deny supply-chain checks.
audit:
    template audit

# Detect unused dependencies with cargo-machete and cargo-udeps.
deadcode:
    template deadcode

# Run the cargo-hack feature-matrix gate.
hack:
    template hack

# Run the full local CI mirror in CI order.
ci:
    template ci

# Run the pre-commit gate and stage generated metrics and badges.
precommit:
    template precommit

# Rebuild and reinstall the worktree-private pre-commit hook.
precommit-refresh:
    template precommit-refresh

# Upgrade manifest dependency requirements and refresh the lockfile.
upgrade:
    template upgrade

# Refresh transitive lockfile dependencies without changing manifests.
update:
    template update

# Report outdated workspace dependencies.
outdated:
    template outdated

# Apply the configured or explicitly selected template branch without merging
# template history; the sidecar remains the sole steady-state source authority.
update-template *args:
    template update-template {{args}}

# Regenerate `template-lint.toml` and `template-clippy.toml`.
gen-lint-template *args:
    template gen-lint-template {{args}}

# Split existing Markdown doc(s) into consumer fragments and regenerate them
# in place: `just migrate-md [PATH]`. PATH may be a file or a directory
# (recursive); with no PATH it self-heals legacy fragment layouts instead.
migrate-md *args:
    template agent migrate --from "{{invocation_directory()}}" {{args}}

# Regenerate generated Markdown docs from configured fragments.
gen-md:
    template agent generate

# Render live repo structure, command, gate, and generated-doc facts.
repo-overview *args:
    template repo-overview {{args}}

# Render agent helper reports for gates and staged changes.
agent *args:
    template agent {{args}}

# Run cyclomatic, duplication, and tokei code-quality checks.
cq:
    template cq

# Rerun the test suite with SNAPSHOTS=overwrite so `ensure_snapshot` rewrites
# stale snapshot files in place; review the result with `git diff`.

# Refresh committed snapshot files.
snap-update:
    template snap-update

# Rerun the test suite with STRICT_TEST_SEED=random so
# `proptest::strict::ensure_property` draws fresh entropy instead of its fixed
# seed; pin any counterexample as a named unit test. Replay a specific seed with
# `STRICT_TEST_SEED=<n> just test`.

# Rerun property tests with a random strict-test seed.
fuzz:
    template fuzz

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
    template mutants {{args}}

# Distributed mutation testing via the installed Cargo command surface. It
# reuses the same cfg-aware preflight as `just mutants`, then passes the
# generated excludes into the fleet planner for the invoking consumer repo.

# Plan distributed mutation work with the shared fleet runner.
mutant-fleet *args:
    template mutant-fleet {{args}}

# Run the active-code mutation matrix across feature configurations.
comprehensive-mutants *args:
    template comprehensive-mutants {{args}}

# Regenerate the committed README badges under badges/ from transient
# measurement reports and tracked policy files.

# Regenerate committed README badges from current measurements and policy.
badges:
    template badges

# Cross-compile release artifacts via cargo zigbuild (zig as the cross
# linker) for the default target set; pass --target <TRIPLE> (repeatable,
# glibc-suffix form accepted) to build a different set.

# Cross-compile release artifacts via cargo-zigbuild.
cross *args:
    template cross {{args}}

# Run a consumer-registered extension command (registry: stask/src/extensions.rs).
# `--from` is always forwarded; the handler reads it via `CommandContext::invocation_dir()`.
# Pass an extension's own dashed flags after `--`:  just x release-notes -- --since v1.2

# Run a consumer-registered extension command.
x *args:
    cargo stask x --from "{{invocation_directory()}}" {{args}}

# Print the installed template command help.
help:
    template help
