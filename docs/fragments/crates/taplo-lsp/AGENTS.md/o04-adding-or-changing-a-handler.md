## Adding or changing a handler

Implement runtime-neutral behavior in `handlers/<name>.rs`, re-export it from `handlers.rs`, and register thin adapters in both runtime families. State-changing notifications belong on the ordered mutation lane. Requests must capture immutable snapshots after the preceding mutation barrier and suppress generation-dependent output when the captured revision is stale. Custom messages additionally need their type declared under `lsp_ext/`.
