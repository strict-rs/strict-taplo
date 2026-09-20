//! Memory and optional disk caching for JSON Schema documents.

use std::collections::hash_map::RandomState;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::fmt::Result as FmtResult;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use futures::future::LocalBoxFuture;
use parking_lot::Mutex;
use parking_lot::RwLock;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha1::Digest as _;
use sha1::Sha1;
use thiserror::Error;
use time::OffsetDateTime;
use url::Url;

use super::SharedSchemaStore;
use super::transport::ConcurrentTransport;
use super::transport::SchemaTransport;
use super::transport::TransportError;
use crate::LruCache;

/// Default lifetime of process-local schema entries.
pub const DEFAULT_LRU_CACHE_EXPIRATION_TIME: Duration = Duration::from_mins(1);
/// Default lifetime of serialized disk-cache entries.
pub const DEFAULT_CACHE_EXPIRATION_TIME: Duration = Duration::from_mins(10);

/// A typed schema-cache failure.
#[derive(Debug, Error)]
pub enum CacheError {
  /// A transport or host operation failed.
  #[error(transparent)]
  Transport(#[from] TransportError),
  /// A disk-cache operation was requested without a configured root.
  #[error("schema cache path is not configured")]
  PathNotConfigured,
  /// A serialized cache entry has expired.
  #[error("schema cache entry for `{url}` expired at {expires_by}")]
  Expired {
    /// Expired schema URL.
    url:        Box<Url>,
    /// Stored expiration deadline.
    expires_by: OffsetDateTime,
  },
  /// A serialized cache entry could not be decoded.
  #[error("schema cache entry `{path}` is invalid")]
  Decode {
    /// Invalid cache file.
    path:   Box<PathBuf>,
    /// Underlying JSON decoder failure.
    #[source]
    source: serde_json::Error,
  },
  /// A serialized cache entry does not belong to its URL-derived filename.
  #[error("schema cache entry `{path}` belongs to `{actual}`, not requested URL `{expected}`")]
  UrlMismatch {
    /// Cache file selected for the requested URL.
    path:     Box<PathBuf>,
    /// URL requested by the caller.
    expected: Box<Url>,
    /// URL serialized inside the cache entry.
    actual:   Box<Url>,
  },
  /// A cache entry could not be serialized.
  #[error("schema cache entry for `{url}` could not be serialized")]
  Encode {
    /// Schema URL being persisted.
    url:    Box<Url>,
    /// Underlying JSON encoder failure.
    #[source]
    source: serde_json::Error,
  },
}

/// Expiration durations for the in-memory and disk-backed schema stores.
#[derive(Clone, Copy)]
struct ExpirationPolicy {
  /// Lifetime of entries in the process-local LRU.
  memory: Duration,
  /// Lifetime of serialized entries in the disk cache.
  disk:   Duration,
}

/// One immutable schema document shared by cache consumers.
type CachedSchema = Arc<Value>;

/// Generate one execution-model-specific schema-cache operation family.
macro_rules! cache_operations {
  (
    $load:ident,
    $store:ident,
    ($save:ident, $save_if_configured:ident, $save_at:ident);
    $future:ident,
    $box_with:ident,
    { $($bounds:tt)* }
  ) => {
    /// Load one schema from memory or the serialized disk cache.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError`] when the cache path is absent, the entry cannot be
    /// read or decoded, or the entry is expired and `include_expired` is false.
    pub fn $load<'cache>(
      &'cache self,
      schema_url: &'cache Url,
      include_expired: bool,
    ) -> $future<'cache, Result<Arc<Value>, CacheError>>
    $($bounds)*
    {
      async move {
        let now = self.transport.now()?;

        if self.lru_expired()? {
          self.schemas.lock().clear();
        }

        let memory_schema = self.schemas.lock().get(schema_url).map(Arc::clone);
        if let Some(schema) = memory_schema {
          return Ok(schema);
        }

        let disk_root = self.disk_root.read().clone().ok_or(CacheError::PathNotConfigured)?;
        let path = disk_root.join(cache_hash(schema_url));
        let bytes = self.transport.read_bytes(path.clone()).await?;
        let stored_schema: CachedJson = serde_json::from_slice(&bytes).map_err(|source| CacheError::Decode {
          path: Box::new(path.clone()),
          source,
        })?;
        if stored_schema.url != *schema_url {
          return Err(CacheError::UrlMismatch {
            path: Box::new(path),
            expected: Box::new(schema_url.clone()),
            actual: Box::new(stored_schema.url),
          });
        }

        if !include_expired && stored_schema.expires_by <= now {
          return Err(CacheError::Expired {
            url:        Box::new(schema_url.clone()),
            expires_by: stored_schema.expires_by,
          });
        }

        let schema = Arc::new(stored_schema.value);
        drop(self.schemas.lock().put(schema_url.clone(), Arc::clone(&schema)));
        Ok(schema)
      }
      .$box_with()
    }

    /// Store one schema in memory and persist it when disk caching is configured.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError`] when configured persistence fails.
    pub fn $store(&self, url: Url, schema: Arc<Value>) -> $future<'_, Result<(), CacheError>>
    $($bounds)*
    {
      async move {
        self.insert_memory(url.clone(), Arc::clone(&schema));
        self.$save_if_configured(url, schema).await
      }
      .$box_with()
    }

    /// Require serialized persistence of one schema.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError`] when no disk root is configured or persistence
    /// fails.
    pub fn $save(&self, url: Url, schema: Arc<Value>) -> $future<'_, Result<(), CacheError>>
    $($bounds)*
    {
      async move {
        let disk_root = self.disk_root.read().clone().ok_or(CacheError::PathNotConfigured)?;
        self.$save_at(disk_root, url, schema).await
      }
      .$box_with()
    }

    /// Persist a schema when disk caching is configured.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError`] only when configured persistence fails.
    pub(crate) fn $save_if_configured(&self, url: Url, schema: Arc<Value>) -> $future<'_, Result<(), CacheError>>
    $($bounds)*
    {
      async move {
        let configured_root = self.disk_root.read().clone();
        match configured_root {
          Some(configured_root) => self.$save_at(configured_root, url, schema).await,
          None => Ok(()),
        }
      }
      .$box_with()
    }

    /// Serialize and atomically persist one cache entry.
    fn $save_at(&self, disk_root: PathBuf, url: Url, schema: Arc<Value>) -> $future<'_, Result<(), CacheError>>
    $($bounds)*
    {
      async move {
        let expires_by = expiration_deadline(self.transport.now()?, self.expiration_policy.read().disk);
        let path = disk_root.join(cache_hash(&url));
        let bytes = serde_json::to_vec(&CachedJson {
          expires_by,
          url: url.clone(),
          value: (*schema).clone(),
        })
        .map_err(|source| CacheError::Encode {
          url: Box::new(url),
          source,
        })?;
        self.transport.write_bytes(path, bytes).await?;
        Ok(())
      }
      .$box_with()
    }
  };
}

/// Shared schema cache using one execution-model-specific transport.
#[derive(Clone)]
pub struct Cache<T: SchemaTransport> {
  /// Transport used for host clock and disk I/O.
  transport:         T,
  /// Current expiration durations.
  expiration_policy: Arc<RwLock<ExpirationPolicy>>,
  /// Next process-local cache invalidation deadline.
  lru_expires_by:    Arc<Mutex<OffsetDateTime>>,
  /// Process-local schema entries.
  schemas:           SharedSchemaStore,
  /// Optional serialized-cache root.
  disk_root:         Arc<RwLock<Option<PathBuf>>>,
}

impl<T: SchemaTransport> Debug for Cache<T> {
  /// Render the cache's configuration and size without its stored schema documents.
  ///
  /// State another thread is currently using is reported as `None` instead of being waited
  /// for, so formatting never blocks.
  fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
    let disk_root = self.disk_root.try_read();
    let cached_schemas = self.schemas.try_lock().map(|schemas| schemas.len());
    f.debug_struct("Cache")
      .field("disk_root", &disk_root.as_deref())
      .field("cached_schemas", &cached_schemas)
      .finish_non_exhaustive()
  }
}

impl<T: SchemaTransport> Cache<T> {
  /// Construct an empty cache.
  ///
  /// # Errors
  ///
  /// Returns [`CacheError`] when the transport cannot provide the initial clock
  /// value.
  #[allow(
    clippy::single_call_fn,
    reason = "the public cache constructor validates the transport clock before establishing memory and disk expiration state"
  )]
  pub fn new(transport: T) -> Result<Self, CacheError> {
    let now = transport.now()?;
    Ok(Self {
      expiration_policy: Arc::new(RwLock::new(ExpirationPolicy {
        memory: DEFAULT_LRU_CACHE_EXPIRATION_TIME,
        disk:   DEFAULT_CACHE_EXPIRATION_TIME,
      })),
      lru_expires_by: Arc::new(Mutex::new(expiration_deadline(now, DEFAULT_LRU_CACHE_EXPIRATION_TIME))),
      transport,
      schemas: Arc::new(Mutex::new(LruCache::with_hasher(
        NonZeroUsize::new(10).unwrap_or(NonZeroUsize::MIN),
        RandomState::new(),
      ))),
      disk_root: Arc::default(),
    })
  }

  /// Return one process-local schema entry.
  #[must_use]
  pub fn get_schema(&self, url: &Url) -> Option<CachedSchema> {
    self.schemas.lock().get(url).cloned()
  }

  /// Insert one process-local schema entry without requiring disk persistence.
  pub fn insert_memory(&self, url: Url, schema: CachedSchema) {
    drop(self.schemas.lock().put(url, schema));
  }

  /// Return the shared process-local store used by synchronous validator retrieval.
  pub(crate) fn memory_store(&self) -> SharedSchemaStore {
    Arc::clone(&self.schemas)
  }

  /// Return whether one URL is present in the process-local store.
  #[must_use]
  pub fn contains_schema(&self, url: &Url) -> bool {
    self.schemas.lock().contains(url)
  }

  /// Configure or disable serialized disk caching.
  pub fn set_cache_path(&self, path: Option<PathBuf>) {
    *self.disk_root.write() = path;
  }

  schema_execution_families!(
    cache_operations;
    (load, load_concurrent),
    (store, store_concurrent),
    (
      (save, save_if_configured, save_at),
      (
        save_concurrent,
        save_if_configured_concurrent,
        save_at_concurrent
      )
    ),
  );

  /// Replace memory and disk expiration durations from the current host time.
  ///
  /// # Errors
  ///
  /// Returns [`CacheError`] when the transport cannot provide the current time.
  pub fn set_expiration_times(&self, memory: Duration, disk: Duration) -> Result<(), CacheError> {
    let now = self.transport.now()?;
    *self.expiration_policy.write() = ExpirationPolicy {
      memory,
      disk,
    };
    *self.lru_expires_by.lock() = expiration_deadline(now, memory);
    Ok(())
  }

  /// Report whether the process-local LRU expired and advance its deadline.
  ///
  /// # Errors
  ///
  /// Returns [`CacheError`] when the host clock is unavailable.
  pub fn lru_expired(&self) -> Result<bool, CacheError> {
    let now = self.transport.now()?;
    let memory_expiration = self.expiration_policy.read().memory;
    let mut expires_by = self.lru_expires_by.lock();
    let expired = *expires_by <= now;
    if expired {
      *expires_by = expiration_deadline(now, memory_expiration);
    }
    drop(expires_by);
    Ok(expired)
  }
}

/// Serialized disk-cache entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedJson {
  /// Expiration deadline.
  pub expires_by: OffsetDateTime,
  /// Original schema URL.
  pub url:        Url,
  /// Cached schema value.
  pub value:      Value,
}

/// Return the stable filename for one schema URL.
fn cache_hash(url: &Url) -> String {
  let mut hasher = Sha1::new();
  hasher.update(url.as_str().as_bytes());
  hex::encode(hasher.finalize())
}

/// Add a standard duration while saturating at the representable boundary.
fn expiration_deadline(now: OffsetDateTime, duration: Duration) -> OffsetDateTime {
  let cache_duration = time::Duration::try_from(duration).unwrap_or(time::Duration::MAX);
  now.saturating_add(cache_duration)
}

#[cfg(test)]
mod tests {
  use std::fmt::Debug;
  use std::path::PathBuf;
  use std::sync::Arc;
  use std::time::Duration;

  use futures::executor::block_on;
  use serde_json::json;
  use strict_test_support::ensure_that;
  use time::OffsetDateTime;
  use url::ParseError;
  use url::Url;

  use super::Cache;
  use super::CacheError;
  use super::CachedJson;
  use super::cache_hash;
  use crate::environment::LocalEnvironment as _;
  use crate::schema::transport::OfflineSchemaTransport;
  use crate::test_support::TestEnvironment;

  /// Parse the shared cache fixture URL without erasing its native failure.
  fn schema_url() -> Result<Url, ParseError> {
    Url::parse("https://example.com/schema.json")
  }

  #[test]
  fn optional_and_required_disk_persistence_have_distinct_contracts() -> Result<(), impl Debug> {
    let environment = TestEnvironment::default();
    let cache = Cache::new(OfflineSchemaTransport::new(environment.clone()));
    let url = schema_url();
    let schema = Arc::new(json!({ "type": "object" }));
    let observed = block_on(async {
      let (store, address) = cache.as_ref().ok().zip(url.as_ref().ok())?;
      let required = store.save(address.clone(), Arc::clone(&schema)).await;
      let optional = store.save_if_configured(address.clone(), Arc::clone(&schema)).await;
      let skipped_writes = environment.writes();
      store.set_cache_path(Some(PathBuf::from("/cache")));
      let configured = store.save_if_configured(address.clone(), Arc::clone(&schema)).await;
      let writes = environment.writes();
      let path = PathBuf::from("/cache").join(cache_hash(address));
      let bytes = environment.read_file(&path).await;
      let decoded = bytes
        .as_ref()
        .ok()
        .map(|contents| serde_json::from_slice::<CachedJson>(contents));
      environment.set_write_failure(true);
      let failed = store.save_if_configured(address.clone(), Arc::clone(&schema)).await;
      Some((required, optional, skipped_writes, configured, writes, path, bytes, decoded, failed))
    });
    ensure_that(
      (environment, cache, url, schema, observed),
      "required, optional, configured, and failed persistence must preserve their distinct contracts",
      |actual| {
        let Some(ref io) = actual.4 else {
          return false;
        };
        matches!(&io.0, Err(error @ CacheError::PathNotConfigured) if error.to_string().contains("not configured"))
          && io.1.is_ok()
          && io.2.is_empty()
          && io.3.is_ok()
          && io.4.as_slice() == [io.5.clone()]
          && io.7.as_ref().is_some_and(|result| {
            result
              .as_ref()
              .is_ok_and(|cached| actual.2.as_ref().is_ok_and(|address| &cached.url == address) && cached.value == *actual.3)
          })
          && io.8.is_err()
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn expiration_policy_invalidates_memory_and_disk_entries_at_the_deadline() -> Result<(), impl Debug> {
    let environment = TestEnvironment::default();
    let transport = OfflineSchemaTransport::new(environment.clone());
    let cache = Cache::new(transport.clone());
    let url = schema_url();
    let schema = Arc::new(json!({ "type": "string" }));
    let deadline = OffsetDateTime::UNIX_EPOCH.checked_add(time::Duration::seconds(1));
    let observed = block_on(async {
      let (store, address) = cache.as_ref().ok().zip(url.as_ref().ok())?;
      let policy = store.set_expiration_times(Duration::from_secs(1), Duration::from_secs(1));
      store.set_cache_path(Some(PathBuf::from("/cache")));
      let stored = store.store(address.clone(), Arc::clone(&schema)).await;
      let present = store.contains_schema(address);
      if let Some(instant) = deadline {
        environment.set_now(instant);
      }
      let expired_memory = store.load(address, false).await;
      let retained = store.contains_schema(address);
      let reader = Cache::new(transport);
      let disk = if let Ok(ref fresh) = reader {
        fresh.set_cache_path(Some(PathBuf::from("/cache")));
        Some((fresh.load(address, false).await, fresh.load(address, true).await))
      } else {
        None
      };
      Some((policy, stored, present, expired_memory, retained, reader, disk))
    });
    ensure_that(
      (environment, cache, url, schema, deadline, observed),
      "memory and disk entries must expire exactly at the deadline while explicit stale reads retain the schema",
      |actual| {
        let Some(ref loads) = actual.5 else {
          return false;
        };
        actual.4.is_some()
          && loads.0.is_ok()
          && loads.1.is_ok()
          && loads.2
          && matches!(&loads.3, Err(CacheError::Expired { .. }))
          && !loads.4
          && loads.6.as_ref().is_some_and(|disk| {
            matches!(&disk.0, Err(CacheError::Expired { .. })) && disk.1.as_ref().is_ok_and(|stale| **stale == *actual.3)
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn disk_entries_must_match_their_url_derived_identity() -> Result<(), impl Debug> {
    let environment = TestEnvironment::default();
    let cache = Cache::new(OfflineSchemaTransport::new(environment.clone()));
    let expected = schema_url();
    let other = Url::parse("https://example.com/other.json");
    let observed = block_on(async {
      let ((store, requested), stored_url) = cache.as_ref().ok().zip(expected.as_ref().ok()).zip(other.as_ref().ok())?;
      store.set_cache_path(Some(PathBuf::from("/cache")));
      let path = PathBuf::from("/cache").join(cache_hash(requested));
      let bytes = serde_json::to_vec(&CachedJson {
        expires_by: OffsetDateTime::UNIX_EPOCH.saturating_add(time::Duration::hours(1)),
        url:        stored_url.clone(),
        value:      json!({ "type": "number" }),
      });
      if let Ok(ref contents) = bytes {
        environment.insert_file(path.clone(), contents.clone());
      }
      let loaded = store.load(requested, false).await;
      Some((path, bytes, loaded))
    });
    ensure_that(
      (environment, cache, expected, other, observed),
      "cache filenames must reject data serialized for a different URL while retaining both identities",
      |actual| {
        let Some(ref disk) = actual.4 else {
          return false;
        };
        disk.1.is_ok()
          && matches!(&disk.2,
          Err(CacheError::UrlMismatch { path, expected: required, actual: different }) if path.as_ref() == &disk.0
            && actual.2.as_ref().is_ok_and(|requested| required.as_ref() == requested)
            && actual.3.as_ref().is_ok_and(|alternative| different.as_ref() == alternative))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }
}
