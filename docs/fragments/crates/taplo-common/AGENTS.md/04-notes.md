## Notes

`lib.rs` exports `HashMap`/`IndexMap` (over `ahash`), `AsyncMutex`/`AsyncRwLock` (over `tokio::sync`), and `LruCache` aliases used across the tools. The crate currently sets `#![warn(clippy::pedantic)]` with a list of `#![allow(..)]`s — upstream taplo posture that the strict-rs conversion has not yet replaced; read the current lint state from `lib.rs` before assuming it.
