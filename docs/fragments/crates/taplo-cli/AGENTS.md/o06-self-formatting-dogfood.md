## Self-formatting (dogfood)

The CLI formats this repo's own TOML per the root `taplo.toml`. `just x taplo-self-format` runs the repository-owned dogfood check through the guarded extension surface; successful formatting must remain idempotent.
