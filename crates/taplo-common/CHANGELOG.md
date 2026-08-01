<!-- Do not edit; generated file. -->

# Change Log

## 0.16.0

### Breaking Changes

- Split host capabilities into `Environment`, `LocalEnvironment`, and `ConcurrentEnvironment`.
- Separate schema interpretation from `LocalSchemaTransport`, `ConcurrentSchemaTransport`, and `OfflineSchemaTransport`.
- Replace reusable `anyhow` boundaries with typed configuration, conversion, schema, cache, association, transport, and environment errors.
- Make TOML-to-JSON conversion reject syntax and semantic diagnostics instead of serializing a partial document.

### Reliability

- Traverse schema references and composition branches with iterative queues and deterministic cycle handling.
- Persist cache writes atomically and report unsupported schemes or unavailable remote transport as typed failures.

## 0.6.0

### Fixes

- Fix incorrect error locations for unexpected entries ([#680](https://github.com/tamasfe/taplo/pull/664))

### Features

- Improve error locations for unexpected properties ([#664](https://github.com/tamasfe/taplo/pull/664))
  - Add `text_ranges` for `NodeValidationError`

## 0.5.2

This is a re-release of 0.5.1

## 0.5.1

### Fixes

- Bump dependencies to support RISC-V platform([#545](https://github.com/tamasfe/taplo/pull/545))
- Do not enable default-tls unconditionally ([#554](https://github.com/tamasfe/taplo/pull/554))

## 0.5.0

### Features

- Support adding custom CA ([#454](https://github.com/tamasfe/taplo/pull/454))

### Fixes

- Do not change the extension of catalog paths ([#426](https://github.com/tamasfe/taplo/pull/426))
- Update json_value_merge to 2.0.0 ([#474](https://github.com/tamasfe/taplo/pull/474))
- Save index to cache as it would be returned ([#524](https://github.com/tamasfe/taplo/pull/524))
