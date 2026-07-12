//! Deterministic local environment used by language-server behavior tests.

use async_trait::async_trait;
use futures::Future;
use parking_lot::RwLock;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use taplo_common::environment::Environment;
use time::OffsetDateTime;
use url::Url;

/// Convert an `anyhow` result into the panic-free test failure vocabulary.
pub(crate) fn ensure_anyhow<T>(
    result: Result<T, anyhow::Error>,
    context: &'static str,
) -> Result<T, strict_test_support::TestFailure> {
    result.map_err(|error| strict_test_support::TestFailure::WasErr {
        context,
        cause: error.to_string(),
    })
}

/// In-memory environment with explicit CWD, file, and discovery observations.
#[derive(Clone)]
pub(crate) struct TestEnvironment {
    /// Optional current working directory.
    cwd: Arc<RwLock<Option<PathBuf>>>,
    /// In-memory file contents.
    files: Arc<RwLock<HashMap<PathBuf, Vec<u8>>>>,
    /// Bases passed to config discovery.
    discovery_bases: Arc<RwLock<Vec<PathBuf>>>,
}

impl Default for TestEnvironment {
    fn default() -> Self {
        Self {
            cwd: Arc::new(RwLock::new(Some(PathBuf::from("/workspace")))),
            files: Default::default(),
            discovery_bases: Default::default(),
        }
    }
}

impl TestEnvironment {
    /// Replace the current working directory.
    pub(crate) fn set_cwd(&self, cwd: Option<PathBuf>) {
        *self.cwd.write() = cwd;
    }

    /// Seed one readable in-memory file.
    pub(crate) fn insert_file(&self, path: impl Into<PathBuf>, bytes: impl Into<Vec<u8>>) {
        self.files.write().insert(path.into(), bytes.into());
    }

    /// Return every base passed to config discovery.
    pub(crate) fn discovery_bases(&self) -> Vec<PathBuf> {
        self.discovery_bases.read().clone()
    }
}

#[async_trait(?Send)]
impl Environment for TestEnvironment {
    type Stdin = tokio::io::Empty;
    type Stdout = tokio::io::Sink;
    type Stderr = tokio::io::Sink;

    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH
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
        self.files
            .read()
            .get(path)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("file not found: {}", path.display()))
    }

    async fn write_file(&self, path: &Path, bytes: &[u8]) -> Result<(), anyhow::Error> {
        self.files.write().insert(path.to_path_buf(), bytes.to_vec());
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
        self.discovery_bases.write().push(from.to_path_buf());
        taplo_common::config::CONFIG_FILE_NAMES
            .iter()
            .map(|name| from.join(name))
            .find(|candidate| self.files.read().contains_key(candidate))
    }
}
