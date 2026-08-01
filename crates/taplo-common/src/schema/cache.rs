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
  use std::path::PathBuf;
  use std::sync::Arc;
  use std::time::Duration;

  use futures::executor::block_on;
  use serde_json::json;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_contains;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use taplo_test_support::ensure_result;
  use time::OffsetDateTime;
  use url::Url;

  use super::Cache;
  use super::CachedJson;
  use super::cache_hash;
  use crate::environment::LocalEnvironment as _;
  use crate::schema::transport::OfflineSchemaTransport;
  use crate::test_support::TestEnvironment;
  /// Parse the shared cache fixture URL.
  fn schema_url() -> Result<Url, TestFailure> {
    ensure_ok(Url::parse("https://example.com/schema.json"), "the cache fixture URL must parse")
  }

  #[test]
  fn optional_and_required_disk_persistence_have_distinct_contracts() -> Result<(), TestFailure> {
    block_on(async {
      let environment = TestEnvironment::default();
      let transport = OfflineSchemaTransport::new(environment.clone());
      let cache = ensure_result(Cache::new(transport), "the test cache must initialize")?;
      let url = schema_url()?;
      let schema = Arc::new(json!({ "type": "object" }));

      let required = cache.save(url.clone(), Arc::clone(&schema)).await;
      ensure(required.is_err(), "public save must remain fallible without a disk root")?;
      let required_error = ensure_some(required.err(), "the save error must exist")?;
      ensure_contains(
        &required_error.to_string(),
        "not configured",
        "the missing-cache-path diagnostic must be retained",
      )?;

      ensure_result(
        cache.save_if_configured(url.clone(), Arc::clone(&schema)).await,
        "optional persistence must skip an absent disk root",
      )?;
      ensure_eq(&environment.writes().len(), &0, "skipped optional persistence must not write")?;

      cache.set_cache_path(Some(PathBuf::from("/cache")));
      ensure_result(
        cache.save_if_configured(url.clone(), Arc::clone(&schema)).await,
        "configured optional persistence must write",
      )?;
      let expected_path = PathBuf::from("/cache").join(cache_hash(&url));
      ensure(
        environment.writes() == [expected_path.clone()],
        "configured persistence must use the URL-derived cache entry path",
      )?;
      let bytes = ensure_result(
        environment.read_file(&expected_path).await,
        "the written cache entry must be readable",
      )?;
      let cached = ensure_ok(
        serde_json::from_slice::<CachedJson>(&bytes),
        "the written cache entry must be valid cached JSON",
      )?;
      ensure_eq(&cached.url.as_str(), &url.as_str(), "cached URL")?;
      ensure(cached.value == *schema, "the configured write must preserve the schema value")?;

      environment.set_write_failure(true);
      let failed = cache.save_if_configured(url, schema).await;
      ensure(failed.is_err(), "an actual configured write failure must be returned")
    })
  }

  #[test]
  fn expiration_policy_invalidates_memory_and_disk_entries_at_the_deadline() -> Result<(), TestFailure> {
    block_on(async {
      let environment = TestEnvironment::default();
      let transport = OfflineSchemaTransport::new(environment.clone());
      let cache = ensure_result(Cache::new(transport.clone()), "the expiring cache must initialize")?;
      ensure_result(
        cache.set_expiration_times(Duration::from_secs(1), Duration::from_secs(1)),
        "the short expiration policy must install",
      )?;
      cache.set_cache_path(Some(PathBuf::from("/cache")));
      let url = schema_url()?;
      let schema = Arc::new(json!({ "type": "string" }));
      ensure_result(
        cache.store(url.clone(), Arc::clone(&schema)).await,
        "the expiring cache entry must persist",
      )?;
      ensure(cache.contains_schema(&url), "a newly stored schema must be present in memory")?;

      let deadline = ensure_some(
        OffsetDateTime::UNIX_EPOCH.checked_add(time::Duration::seconds(1)),
        "the cache deadline must be representable",
      )?;
      environment.set_now(deadline);
      let expired_memory = cache.load(&url, false).await;
      ensure(
        matches!(expired_memory, Err(super::CacheError::Expired { .. })),
        "an entry must expire exactly at its configured deadline",
      )?;
      ensure(
        !cache.contains_schema(&url),
        "an expired LRU generation must be cleared before consulting disk",
      )?;

      let reader = ensure_result(Cache::new(transport), "the disk-cache reader must initialize")?;
      reader.set_cache_path(Some(PathBuf::from("/cache")));
      let expired_disk = reader.load(&url, false).await;
      ensure(
        matches!(expired_disk, Err(super::CacheError::Expired { .. })),
        "a fresh reader must reject the serialized entry at its deadline",
      )?;
      let stale = ensure_result(
        reader.load(&url, true).await,
        "an explicit stale fallback must retain an expired serialized entry",
      )?;
      ensure(
        *stale == *schema,
        "the stale fallback must preserve the complete cached schema value",
      )
    })
  }

  #[test]
  fn disk_entries_must_match_their_url_derived_identity() -> Result<(), TestFailure> {
    block_on(async {
      let environment = TestEnvironment::default();
      let transport = OfflineSchemaTransport::new(environment.clone());
      let cache = ensure_result(Cache::new(transport), "the identity-checking cache must initialize")?;
      cache.set_cache_path(Some(PathBuf::from("/cache")));
      let expected = schema_url()?;
      let actual = ensure_ok(Url::parse("https://example.com/other.json"), "the mismatched cache URL must parse")?;
      let path = PathBuf::from("/cache").join(cache_hash(&expected));
      let bytes = ensure_ok(
        serde_json::to_vec(&CachedJson {
          expires_by: OffsetDateTime::UNIX_EPOCH.saturating_add(time::Duration::hours(1)),
          url:        actual.clone(),
          value:      json!({ "type": "number" }),
        }),
        "the mismatched cache fixture must serialize",
      )?;
      environment.insert_file(path.clone(), bytes);

      ensure(
        matches!(
          cache.load(&expected, false).await,
          Err(super::CacheError::UrlMismatch {
            path: error_path,
            expected: error_expected,
            actual: error_actual,
          }) if (
            error_path.as_ref(),
            error_expected.as_ref(),
            error_actual.as_ref(),
          ) == (&path, &expected, &actual)
        ),
        "a cache filename must not authorize data serialized for another schema URL",
      )
    })
  }
}
