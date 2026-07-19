## Notes

`lib.rs` exports `HashMap`/`IndexMap`/`LruCache` aliases backed by the standard library's randomized `RandomState`, plus `AsyncMutex`/`AsyncRwLock` aliases over `tokio::sync`. Read the current lint state from `lib.rs` before assuming it.
