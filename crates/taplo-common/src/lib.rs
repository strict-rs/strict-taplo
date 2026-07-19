#![warn(clippy::pedantic)]
#![deny(clippy::print_stdout, clippy::print_stderr)]
#![allow(
  clippy::single_match,
  clippy::default_trait_access,
  clippy::single_match_else,
  clippy::module_name_repetitions,
  clippy::missing_errors_doc,
  clippy::missing_panics_doc,
  clippy::missing_fields_in_debug,
  clippy::similar_names,
  clippy::too_many_lines,
  clippy::needless_continue
)]

pub mod config;
pub mod convert;
pub mod environment;
pub mod log;
#[cfg(feature = "schema")]
pub mod schema;
pub mod util;

#[cfg(test)]
pub(crate) mod test_support;

pub type HashMap<K, V> = std::collections::HashMap<K, V, std::collections::hash_map::RandomState>;
pub type IndexMap<K, V> = indexmap::IndexMap<K, V, std::collections::hash_map::RandomState>;

pub type AsyncMutex<T> = tokio::sync::Mutex<T>;
pub type AsyncRwLock<T> = tokio::sync::RwLock<T>;

pub type LruCache<K, V> = lru::LruCache<K, V, std::collections::hash_map::RandomState>;
