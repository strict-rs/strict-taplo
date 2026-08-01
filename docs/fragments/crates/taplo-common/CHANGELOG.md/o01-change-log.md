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
