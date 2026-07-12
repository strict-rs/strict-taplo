//! Deterministic environment support shared by crate-local behavior tests.

use crate::environment::Environment;
use async_trait::async_trait;
use futures::Future;
use parking_lot::RwLock;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use time::OffsetDateTime;
use url::Url;

/// Extract an `anyhow` result into the repository's panic-free test failure vocabulary.
pub(crate) fn ensure_anyhow<T>(
    result: Result<T, anyhow::Error>,
    context: &'static str,
) -> Result<T, strict_test_support::TestFailure> {
    result.map_err(|error| strict_test_support::TestFailure::WasErr {
        context,
        cause: error.to_string(),
    })
}

/// In-memory environment with a controllable clock and injected I/O failures.
#[derive(Clone)]
pub(crate) struct TestEnvironment {
    /// Current test time.
    now: Arc<RwLock<OffsetDateTime>>,
    /// Optional current working directory.
    cwd: Arc<RwLock<Option<PathBuf>>>,
    /// In-memory file contents.
    files: Arc<RwLock<HashMap<PathBuf, Vec<u8>>>>,
    /// Paths written through the environment.
    writes: Arc<RwLock<Vec<PathBuf>>>,
    /// Whether reads should fail.
    fail_reads: Arc<RwLock<bool>>,
    /// Whether writes should fail.
    fail_writes: Arc<RwLock<bool>>,
}

impl Default for TestEnvironment {
    fn default() -> Self {
        Self {
            now: Arc::new(RwLock::new(OffsetDateTime::UNIX_EPOCH)),
            cwd: Arc::new(RwLock::new(Some(PathBuf::from("/workspace")))),
            files: Default::default(),
            writes: Default::default(),
            fail_reads: Default::default(),
            fail_writes: Default::default(),
        }
    }
}

impl TestEnvironment {
    /// Seed one in-memory file.
    pub(crate) fn insert_file(&self, path: impl Into<PathBuf>, bytes: impl Into<Vec<u8>>) {
        self.files.write().insert(path.into(), bytes.into());
    }

    /// Replace the deterministic clock value.
    pub(crate) fn set_now(&self, now: OffsetDateTime) {
        *self.now.write() = now;
    }

    /// Enable or disable injected write failures.
    pub(crate) fn set_write_failure(&self, enabled: bool) {
        *self.fail_writes.write() = enabled;
    }

    /// Return the paths successfully written through the environment.
    pub(crate) fn writes(&self) -> Vec<PathBuf> {
        self.writes.read().clone()
    }
}

#[async_trait(?Send)]
impl Environment for TestEnvironment {
    type Stdin = tokio::io::Empty;
    type Stdout = tokio::io::Sink;
    type Stderr = tokio::io::Sink;

    fn now(&self) -> OffsetDateTime {
        *self.now.read()
    }

    fn spawn<F>(&self, future: F)
    where
        F: Future + Send + 'static,
        F::Output: Send,
    {
        drop(futures::executor::block_on(future));
    }

    fn spawn_local<F>(&self, future: F)
    where
        F: Future + 'static,
    {
        drop(futures::executor::block_on(future));
    }

    fn env_var(&self, _name: &str) -> Option<String> {
        None
    }

    fn env_vars(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    fn atty_stderr(&self) -> bool {
        false
    }

    fn stdin(&self) -> Self::Stdin {
        tokio::io::empty()
    }

    fn stdout(&self) -> Self::Stdout {
        tokio::io::sink()
    }

    fn stderr(&self) -> Self::Stderr {
        tokio::io::sink()
    }

    fn glob_files(&self, _glob: &str) -> Result<Vec<PathBuf>, anyhow::Error> {
        Ok(self.files.read().keys().cloned().collect())
    }

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>, anyhow::Error> {
        if *self.fail_reads.read() {
            return Err(anyhow::anyhow!("injected read failure"));
        }
        self.files
            .read()
            .get(path)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("file not found: {}", path.display()))
    }

    async fn write_file(&self, path: &Path, bytes: &[u8]) -> Result<(), anyhow::Error> {
        if *self.fail_writes.read() {
            return Err(anyhow::anyhow!("injected write failure"));
        }
        self.files.write().insert(path.to_path_buf(), bytes.to_vec());
        self.writes.write().push(path.to_path_buf());
        Ok(())
    }

    fn to_file_path(&self, url: &Url) -> Option<PathBuf> {
        url.to_file_path().ok()
    }

    fn is_absolute(&self, path: &Path) -> bool {
        path.is_absolute()
    }

    fn cwd(&self) -> Option<PathBuf> {
        self.cwd.read().clone()
    }

    async fn find_config_file(&self, from: &Path) -> Option<PathBuf> {
        crate::config::CONFIG_FILE_NAMES
            .iter()
            .map(|name| from.join(name))
            .find(|candidate| self.files.read().contains_key(candidate))
    }
}
