## Entry points

`lib.rs` exposes four explicit constructors:

- `create_concurrent_server::<E>() -> ConcurrentServer<ConcurrentWorld<E>>` and `create_concurrent_world(env, http)` — construct the native multi-threaded server and `Arc`-owned world for a `ConcurrentEnvironment`.
- `create_local_server::<E>() -> LocalServer<LocalWorld<E>>` and `create_local_world(env, http)` — construct the current-thread server and `Rc`-owned world for a `LocalEnvironment`.

There are no `Server`, `ServerBuilder`, or ambiguous world aliases. Construction that initializes schema services is fallible and returns `WorldError`.
