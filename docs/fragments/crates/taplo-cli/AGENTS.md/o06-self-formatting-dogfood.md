## Self-formatting (dogfood)

The CLI formats this repo's own TOML per the root `taplo.toml`, and CI asserts `taplo fmt --check` produces no diff. After changing formatter output, run `cargo run -- fmt` and commit the reformatted TOML — formatting is expected to be idempotent.
