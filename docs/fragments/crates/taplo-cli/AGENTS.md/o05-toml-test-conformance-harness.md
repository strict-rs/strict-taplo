## toml-test conformance harness

The `toml-test` subcommand reads TOML on stdin and emits `toml-test`'s tagged JSON. `just x toml-conformance` owns the checksum-verified pinned runner and requires the TOML 1.1 suite to pass without a permanent skip list. A newly incompatible upstream case is a stop-and-review decision, not a silent exclusion.
