use anyhow::anyhow;
use arc_swap::ArcSwap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha1::{Digest, Sha1};
use std::{num::NonZeroUsize, path::Path, path::PathBuf, sync::Arc, time::Duration};
use time::OffsetDateTime;
use url::Url;

use crate::{environment::Environment, LruCache};

pub const DEFAULT_LRU_CACHE_EXPIRATION_TIME: Duration = Duration::from_mins(1);
pub const DEFAULT_CACHE_EXPIRATION_TIME: Duration = Duration::from_mins(10);

/// Expiration durations for the in-memory and disk-backed schema stores.
#[derive(Clone, Copy)]
struct ExpirationPolicy {
    /// Lifetime of entries in the process-local LRU.
    memory: Duration,
    /// Lifetime of serialized entries in the disk cache.
    disk: Duration,
}

#[derive(Clone)]
pub struct Cache<E: Environment> {
    env: E,
    expiration_policy: Arc<ArcSwap<ExpirationPolicy>>,
    lru_expires_by: Arc<Mutex<OffsetDateTime>>,
    schemas: Arc<Mutex<LruCache<Url, Arc<Value>>>>,
    disk_root: Arc<ArcSwap<Option<PathBuf>>>,
}

impl<E: Environment> Cache<E> {
    pub fn new(env: E) -> Self {
        Self {
            expiration_policy: Arc::new(ArcSwap::new(Arc::new(ExpirationPolicy {
                memory: DEFAULT_LRU_CACHE_EXPIRATION_TIME,
                disk: DEFAULT_CACHE_EXPIRATION_TIME,
            }))),
            lru_expires_by: Arc::new(Mutex::new(expiration_deadline(
                env.now(),
                DEFAULT_LRU_CACHE_EXPIRATION_TIME,
            ))),
            env,
            schemas: Arc::new(Mutex::new(LruCache::with_hasher(
                NonZeroUsize::new(10).unwrap_or(NonZeroUsize::MIN),
                ahash::RandomState::new(),
            ))),
            disk_root: Default::default(),
        }
    }

    pub fn get_schema(&self, url: &Url) -> Option<Arc<Value>> {
        self.schemas.lock().get(url).cloned()
    }

    /// A `Send + Sync` handle to the in-memory schema store, independent of the `Environment`.
    ///
    /// Used by the schema validator's synchronous `Retrieve` implementation so it can read cached
    /// schemas without holding the (possibly `!Send`) environment.
    pub(crate) fn memory_store(&self) -> Arc<Mutex<LruCache<Url, Arc<Value>>>> {
        self.schemas.clone()
    }

    pub fn contains_schema(&self, url: &Url) -> bool {
        self.schemas.lock().contains(url)
    }

    pub fn set_cache_path(&self, path: Option<PathBuf>) {
        self.disk_root.swap(Arc::new(path));
    }

    pub async fn load(
        &self,
        value_url: &Url,
        include_expired: bool,
    ) -> Result<Arc<Value>, anyhow::Error> {
        let now = self.env.now();

        // We invalidate the in-memory cache at a regular interval.
        if self.lru_expired() {
            self.schemas.lock().clear();
        }

        if let Some(s) = self.schemas.lock().get(value_url) {
            return Ok(s.clone());
        }

        match &**self.disk_root.load() {
            Some(disk_root) => {
                let file_name = cache_hash(value_url);
                let p = disk_root.join(file_name);
                let schema: CachedJson = serde_json::from_slice(&self.env.read_file(&p).await?)?;

                if !include_expired && schema.expires_by < now {
                    return Err(anyhow!("document expired"));
                }

                let s = Arc::new(schema.value);
                self.schemas.lock().put(value_url.clone(), s.clone());
                Ok(s)
            }
            None => Err(anyhow!("cache path not set")),
        }
    }

    pub async fn store(&self, url: Url, value: Arc<Value>) -> Result<(), anyhow::Error> {
        self.schemas.lock().put(url.clone(), value.clone());
        self.save(url, value).await
    }

    pub async fn save(&self, url: Url, value: Arc<Value>) -> Result<(), anyhow::Error> {
        let disk_root = self.disk_root.load_full();
        match disk_root.as_ref() {
            Some(disk_root) => self.save_at(disk_root, url, value).await,
            None => Err(anyhow!("cache path not set")),
        }
    }

    /// Persist a value when disk caching is configured and otherwise skip it normally.
    pub(crate) async fn save_if_configured(
        &self,
        url: Url,
        value: Arc<Value>,
    ) -> Result<(), anyhow::Error> {
        let disk_root = self.disk_root.load_full();
        match disk_root.as_ref() {
            Some(disk_root) => self.save_at(disk_root, url, value).await,
            None => Ok(()),
        }
    }

    /// Serialize and persist one cache entry beneath the captured disk root.
    async fn save_at(
        &self,
        disk_root: &Path,
        url: Url,
        value: Arc<Value>,
    ) -> Result<(), anyhow::Error> {
        let expires_by = expiration_deadline(self.env.now(), self.expiration_policy.load().disk);
        let file_name = cache_hash(&url);
        let path = disk_root.join(file_name);
        let bytes = serde_json::to_vec(&CachedJson {
            expires_by,
            url,
            value: (*value).clone(),
        })?;
        self.env.write_file(&path, &bytes).await
    }

    pub fn set_expiration_times(&self, mem: Duration, disk: Duration) {
        self.expiration_policy.store(Arc::new(ExpirationPolicy {
            memory: mem,
            disk,
        }));
    }

    /// Reports whether the LRU cache is expired, and also resets
    /// the expiration timer in that case.
    pub fn lru_expired(&self) -> bool {
        let now = self.env.now();
        let expires_by = *self.lru_expires_by.lock();
        let expired = expires_by < now;
        if expired {
            *(self.lru_expires_by.lock()) =
                expiration_deadline(now, self.expiration_policy.load().memory);
        }
        expired
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedJson {
    pub expires_by: OffsetDateTime,
    pub url: Url,
    pub value: Value,
}

fn cache_hash(url: &Url) -> String {
    let mut hasher = Sha1::new();
    hasher.update(url.as_str().as_bytes());
    hex::encode(&hasher.finalize()[..])
}

/// Add a standard duration to a timestamp while saturating at the representable boundary.
fn expiration_deadline(now: OffsetDateTime, duration: Duration) -> OffsetDateTime {
    let duration = time::Duration::try_from(duration).unwrap_or(time::Duration::MAX);
    now.saturating_add(duration)
}

#[cfg(test)]
mod tests {
    use super::{
        cache_hash, Cache, CachedJson, DEFAULT_CACHE_EXPIRATION_TIME,
        DEFAULT_LRU_CACHE_EXPIRATION_TIME,
    };
    use crate::{
        environment::Environment,
        test_support::{ensure_anyhow, TestEnvironment},
    };
    use serde_json::json;
    use std::{path::PathBuf, sync::Arc};
    use strict_test_support::{
        ensure, ensure_contains, ensure_eq, ensure_ok, ensure_some, TestFailure,
    };
    use time::OffsetDateTime;
    use url::Url;

    fn schema_url() -> Result<Url, TestFailure> {
        ensure_ok(
            Url::parse("https://example.com/schema.json"),
            "the cache fixture URL must parse",
        )
    }

    #[test]
    fn optional_and_required_disk_persistence_have_distinct_contracts(
    ) -> Result<(), TestFailure> {
        futures::executor::block_on(async {
            let environment = TestEnvironment::default();
            let cache = Cache::new(environment.clone());
            let url = schema_url()?;
            let value = Arc::new(json!({ "type": "object" }));

            let required = cache.save(url.clone(), value.clone()).await;
            ensure(
                required.is_err(),
                "public save must remain fallible without a disk root",
            )?;
            let required_error = ensure_some(required.err(), "the save error must exist")?;
            ensure_contains(
                &required_error.to_string(),
                "cache path not set",
                "the established missing-cache-path diagnostic must be retained",
            )?;

            ensure_anyhow(
                cache
                    .save_if_configured(url.clone(), value.clone())
                    .await,
                "optional persistence must skip an absent disk root",
            )?;
            ensure_eq(
                &environment.writes().len(),
                &0,
                "skipped optional persistence must not write",
            )?;

            cache.set_cache_path(Some(PathBuf::from("/cache")));
            ensure_anyhow(
                cache
                    .save_if_configured(url.clone(), value.clone())
                    .await,
                "configured optional persistence must write",
            )?;
            let expected_path = PathBuf::from("/cache").join(cache_hash(&url));
            ensure(
                environment.writes() == [expected_path.clone()],
                "configured persistence must use the URL-derived cache entry path",
            )?;
            let bytes = ensure_anyhow(
                environment.read_file(&expected_path).await,
                "the written cache entry must be readable",
            )?;
            let cached = ensure_ok(
                serde_json::from_slice::<CachedJson>(&bytes),
                "the written cache entry must be valid cached JSON",
            )?;
            ensure_eq(&cached.url.as_str(), &url.as_str(), "cached URL")?;
            ensure(
                cached.value == *value,
                "the configured write must preserve the schema value",
            )?;

            environment.set_write_failure(true);
            let failed = cache.save_if_configured(url, value).await;
            ensure(
                failed.is_err(),
                "an actual configured write failure must be returned",
            )
        })
    }

    #[test]
    fn default_policy_and_memory_expiration_have_both_polarities() -> Result<(), TestFailure> {
        futures::executor::block_on(async {
            let environment = TestEnvironment::default();
            let cache = Cache::new(environment.clone());
            let policy = cache.expiration_policy.load();
            ensure(
                policy.memory == DEFAULT_LRU_CACHE_EXPIRATION_TIME,
                "memory cache default must be one minute",
            )?;
            ensure(
                policy.disk == DEFAULT_CACHE_EXPIRATION_TIME,
                "disk cache default must be ten minutes",
            )?;

            let url = schema_url()?;
            drop(cache.store(url.clone(), Arc::new(json!({ "const": 1 }))).await);
            environment.set_now(OffsetDateTime::UNIX_EPOCH.saturating_add(time::Duration::seconds(30)));
            ensure(
                !cache.lru_expired(),
                "memory entries must remain live before the deadline",
            )?;
            ensure(
                cache.contains_schema(&url),
                "a pre-deadline expiration check must retain the entry",
            )?;

            environment.set_now(OffsetDateTime::UNIX_EPOCH.saturating_add(time::Duration::minutes(2)));
            let load_result = cache.load(&url, false).await;
            ensure(
                load_result.is_err(),
                "an expired memory entry with no disk fallback must not load",
            )?;
            ensure(
                !cache.contains_schema(&url),
                "crossing the memory deadline must clear the LRU entry",
            )
        })
    }
}
