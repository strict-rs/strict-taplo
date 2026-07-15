## Notes

`getrandom` is configured with `wasm-bindgen`/`js` features, so the crate compiles for `wasm32`. Handlers are `async` (built on `futures`/`async-trait`); a `CancelToken` obtained from the server lets a long-running handler observe client cancellation.
