# Change Log

## 0.16.0

### Breaking Changes

- Return typed `CliError` values from the reusable command boundary and select process exit status only in the binary.
- Run the native LSP command through the concurrent server and the caller-provided native environment.
- Propagate parser, formatter, configuration, conversion, schema, and transport failures without `anyhow`.

### Changes

- Render terminal output with `anstyle` vocabulary.
- Move TOML 1.1 conformance and self-formatting into guarded repository extension commands used by CI.
