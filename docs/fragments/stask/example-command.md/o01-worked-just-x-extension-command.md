# Worked `just x` Extension Command

The template ships with an intentionally empty repository-specific extension registry. Standard workflows remain owned by the installed `template` binary; `just x` is the only consumer-compiled command seam.

This example adds `just x release-notes -- --since <REF>` without changing the `justfile`, the installed catalog, or any reusable workflow crate.
