//! Shared configuration, environment, schema, conversion, and utility services.

#![forbid(unsafe_code)]

use std::collections::HashMap as StandardHashMap;
use std::collections::hash_map::RandomState;

use indexmap::IndexMap as OrderedMap;
use lru::LruCache as LeastRecentlyUsedCache;
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::RwLock as TokioRwLock;

/// Taplo configuration loading and file-rule policy.
pub mod config;
/// TOML and JSON conversion helpers.
pub mod convert;
/// Local and concurrent host capability contracts.
pub mod environment;
/// Shared logging initialization.
pub mod log;
#[cfg(feature = "schema")]
/// Schema interpretation, association, cache, and transport services.
pub mod schema;
/// Shared path, glob, and parsing utilities.
pub mod util;

#[cfg(test)]
/// Adapter from the shared deterministic test host into this crate's trait identity.
pub mod test_support;

/// Workspace hash map using the standard randomized hasher.
pub type HashMap<K, V> = StandardHashMap<K, V, RandomState>;
/// Insertion-ordered workspace map using the standard randomized hasher.
pub type IndexMap<K, V> = OrderedMap<K, V, RandomState>;

/// Asynchronous mutual-exclusion lock shared by local and concurrent services.
pub type AsyncMutex<T> = TokioMutex<T>;
/// Asynchronous reader-writer lock shared by local and concurrent services.
pub type AsyncRwLock<T> = TokioRwLock<T>;

/// Least-recently-used cache using the standard randomized hasher.
pub type LruCache<K, V> = LeastRecentlyUsedCache<K, V, RandomState>;
