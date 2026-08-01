## Notes

`getrandom` selects its JavaScript backend only on `wasm32`, so the transport-agnostic local server remains available to the WebAssembly binding. Do not introduce `async-trait`, unsafe thread-safety assertions, or a compatibility `Server` alias: local handlers intentionally retain local futures and `Rc`, while concurrent handlers require `Send` futures and `Arc`.

Register state-changing notifications with `on_mutation_notification`; requests then await every earlier mutation without serializing independent request work. Transport shutdown aborts outstanding native handler tasks, while per-request cancellation remains a JSON-RPC response path and preserves duplicate-ID state until the active request is finished.

Writers implement the execution family’s message-writer trait with `MessageWriterError`, preserving native or host I/O separately from a closed serialized output channel. Raw stdio, TCP, and framing failures remain `ServerError::Io`; do not convert typed channel failures into reconstructed `io::Error` values.
