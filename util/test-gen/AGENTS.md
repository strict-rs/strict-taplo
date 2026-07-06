# test-gen

A standalone helper that generates Rust unit tests for the `taplo` crate from TOML fixtures. **Excluded from the workspace** (`exclude = ["util/test-gen"]` in the root `Cargo.toml`): it is `edition = "2018"`, carries its own `Cargo.lock`, and is not built or run by workspace-level `cargo` commands.

## What it does

`src/main.rs` walks an input directory's `valid/` and `invalid/` subtrees and emits one `#[test]` per `.toml` fixture:

- `valid/` fixtures produce tests asserting `parser::parse` yields no syntax errors and the resulting DOM has no semantic errors → `generated/valid.rs`.
- `invalid/` fixtures produce tests asserting at least one syntax **or** semantic error is present → `generated/invalid.rs`.
- Each output file is passed through `rustfmt` after writing.
- The `IGNORED_TESTS` constant lists fixture stems emitted with `#[ignore]`; hyphens in file stems become underscores in test names.

The generated tests reference `crate::parser::parse`, so the output is meant to be dropped into the `taplo` crate under `src/tests/generated/`. Note that `taplo` currently wires in only `invalid.rs` (via `mod generated { mod invalid; }`).

## Running it

Both flags are required:

```
cargo run -- --input <test-data DIR> --output <DIR>
```

(`-i`/`-o` short forms also work.) Output is written under `<output>/generated/`. Point `--input` at the repo's `test-data/` directory to regenerate the taplo fixtures, then copy/commit the regenerated file(s) into `crates/taplo/src/tests/generated/`.

Do not hand-edit the generated files in `taplo` — change the fixtures or this generator and regenerate.
