//! Host capability contracts split by local and concurrent execution models.

use std::future::Future;
use std::io::Error as IoError;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;

use thiserror::Error;
use time::OffsetDateTime;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use url::ParseError as UrlParseError;
use url::Url;

use crate::util::Normalize;

#[cfg(not(target_family = "wasm"))]
pub mod native;

/// A typed failure at an execution-environment boundary.
#[derive(Debug, Error)]
pub enum EnvironmentError {
  /// A filesystem operation failed.
  #[error("environment operation `{operation}` failed for `{path}`")]
  Io {
    /// Stable operation name.
    operation: &'static str,
    /// Path involved in the operation.
    path:      PathBuf,
    /// Underlying I/O failure.
    #[source]
    source:    IoError,
  },
  /// A glob expression is invalid.
  #[error("invalid glob pattern `{pattern}`")]
  GlobPattern {
    /// Rejected glob source.
    pattern: String,
    /// Underlying parser failure.
    #[source]
    source:  glob::PatternError,
  },
  /// Traversing one matched glob entry failed.
  #[error("failed to traverse a path matched by `{pattern}`")]
  GlobEntry {
    /// Glob source being traversed.
    pattern: String,
    /// Underlying traversal failure.
    #[source]
    source:  glob::GlobError,
  },
  /// A host callback is missing.
  #[error("required host callback `{name}` is missing")]
  MissingCallback {
    /// Missing callback name.
    name: &'static str,
  },
  /// A host callback property exists but is not callable.
  #[error("host property `{name}` is not a function")]
  InvalidCallback {
    /// Invalid callback name.
    name: &'static str,
  },
  /// Reading or invoking a host callback threw an exception.
  #[error("host callback `{name}` failed: {message}")]
  Callback {
    /// Callback name.
    name:    &'static str,
    /// Stable rendered host failure.
    message: String,
  },
  /// A host callback returned a value of the wrong type.
  #[error("host callback `{name}` returned {actual}; expected {expected}")]
  InvalidReturnType {
    /// Callback name.
    name:     &'static str,
    /// Expected value description.
    expected: &'static str,
    /// Actual value description.
    actual:   String,
  },
  /// A host timestamp is malformed or outside the supported range.
  #[error("host callback `{name}` returned an invalid timestamp: {message}")]
  InvalidTimestamp {
    /// Callback name.
    name:    &'static str,
    /// Timestamp parser failure.
    message: String,
  },
  /// An environment key or value is not valid Unicode.
  #[error("environment entry is not valid Unicode")]
  InvalidEnvironmentUnicode,
  /// A host path cannot be represented by the string-only JavaScript path model.
  #[error("path `{path}` is not valid Unicode")]
  InvalidPathUnicode {
    /// Rejected host path.
    path: PathBuf,
  },
  /// A host callback returned a malformed URL string.
  #[error("host callback `{name}` returned invalid URL `{input}`")]
  InvalidUrl {
    /// Callback name.
    name:   &'static str,
    /// Rejected URL text.
    input:  String,
    /// URL parser failure.
    #[source]
    source: UrlParseError,
  },
  /// The native atomic-write temporary-name sequence is exhausted.
  #[error("the native atomic-write temporary-name sequence is exhausted")]
  AtomicWriteSequenceExhausted,
  /// No Tokio runtime is active at the construction boundary.
  #[error("a Tokio runtime is required: {message}")]
  RuntimeUnavailable {
    /// Runtime lookup failure.
    message: String,
  },
  /// The active host cannot schedule a non-`Send` task.
  #[error("a local executor is not available in this environment")]
  LocalExecutorUnavailable,
  /// The process-wide tracing subscriber could not be installed.
  #[error("logging initialization failed: {message}")]
  LoggingInitialization {
    /// Subscriber installation failure.
    message: String,
  },
}

impl EnvironmentError {
  /// Construct a filesystem error with stable operation context.
  #[must_use]
  pub fn io(operation: &'static str, path: impl Into<PathBuf>, source: IoError) -> Self {
    Self::Io {
      operation,
      path: path.into(),
      source,
    }
  }
}

/// Synchronous host facts and stream constructors shared by all execution models.
pub trait Environment: Clone + 'static {
  /// Standard-input stream type.
  type Stdin: AsyncRead + Unpin + 'static;
  /// Standard-output stream type.
  type Stdout: AsyncWrite + Unpin + 'static;
  /// Standard-error stream type.
  type Stderr: AsyncWrite + Unpin + 'static;

  /// Return the current host time.
  ///
  /// # Errors
  ///
  /// Returns [`EnvironmentError`] when the host clock callback fails or returns
  /// a malformed timestamp.
  fn now(&self) -> Result<OffsetDateTime, EnvironmentError>;

  /// Return one environment variable.
  ///
  /// # Errors
  ///
  /// Returns [`EnvironmentError`] when the host callback fails or returns an
  /// invalid value.
  fn env_var(&self, name: &str) -> Result<Option<String>, EnvironmentError>;

  /// Return all visible environment variables.
  ///
  /// # Errors
  ///
  /// Returns [`EnvironmentError`] when a host value is malformed.
  fn env_vars(&self) -> Result<Vec<(String, String)>, EnvironmentError>;

  /// Return whether standard error is attached to a terminal.
  ///
  /// # Errors
  ///
  /// Returns [`EnvironmentError`] when the host callback fails or does not
  /// return a Boolean.
  fn atty_stderr(&self) -> Result<bool, EnvironmentError>;

  /// Construct standard input.
  fn stdin(&self) -> Self::Stdin;

  /// Construct standard output.
  fn stdout(&self) -> Self::Stdout;

  /// Construct standard error.
  fn stderr(&self) -> Self::Stderr;

  /// Resolve one filesystem glob.
  ///
  /// # Errors
  ///
  /// Returns [`EnvironmentError`] for an invalid glob or failed traversal.
  fn glob_files(&self, pattern: &str) -> Result<Vec<PathBuf>, EnvironmentError>;

  /// Convert a URL into a host file path when supported.
  ///
  /// # Errors
  ///
  /// Returns [`EnvironmentError`] when the host callback fails or returns an
  /// invalid type.
  fn to_file_path(&self, url: &Url) -> Result<Option<PathBuf>, EnvironmentError>;

  /// Convert a host file path into a URL when supported.
  ///
  /// # Errors
  ///
  /// Returns [`EnvironmentError`] when the host callback fails or returns an
  /// invalid URL.
  fn to_file_url(&self, path: &Path) -> Result<Option<Url>, EnvironmentError>;

  /// Return whether a path is absolute in the host's path model.
  ///
  /// # Errors
  ///
  /// Returns [`EnvironmentError`] when the host callback fails or returns an
  /// invalid type.
  fn is_absolute(&self, path: &Path) -> Result<bool, EnvironmentError>;

  /// Return the absolute current working directory when available.
  ///
  /// # Errors
  ///
  /// Returns [`EnvironmentError`] when the host callback or filesystem query
  /// fails.
  fn cwd(&self) -> Result<Option<PathBuf>, EnvironmentError>;

  /// Resolve one glob and normalize every returned path.
  ///
  /// # Errors
  ///
  /// Returns the same failures as [`Self::glob_files`].
  fn glob_files_normalized(&self, pattern: &str) -> Result<Vec<PathBuf>, EnvironmentError> {
    Ok(self.glob_files(pattern)?.into_iter().map(Normalize::normalize).collect())
  }

  /// Return the normalized current working directory when available.
  ///
  /// # Errors
  ///
  /// Returns the same failures as [`Self::cwd`].
  fn cwd_normalized(&self) -> Result<Option<PathBuf>, EnvironmentError> {
    Ok(self.cwd()?.map(Normalize::normalize))
  }

  /// Convert a URL into a normalized host file path when supported.
  ///
  /// # Errors
  ///
  /// Returns the same failures as [`Self::to_file_path`].
  fn to_file_path_normalized(&self, url: &Url) -> Result<Option<PathBuf>, EnvironmentError> {
    Ok(self.to_file_path(url)?.map(Normalize::normalize))
  }
}

/// A local host-operation future that may retain current-thread state.
pub type LocalEnvironmentFuture<'operation, Output> = Pin<Box<dyn Future<Output = Output> + 'operation>>;

/// Local, potentially non-`Send` asynchronous host capabilities.
pub trait LocalEnvironment: Environment {
  /// Spawn one task on the local executor.
  ///
  /// # Errors
  ///
  /// Returns [`EnvironmentError::LocalExecutorUnavailable`] when the active
  /// host does not provide a local task executor.
  fn spawn_local<F>(&self, future: F) -> Result<(), EnvironmentError>
  where
    F: Future<Output = ()> + 'static;

  /// Read one file.
  ///
  /// # Errors
  ///
  /// Returns [`EnvironmentError`] when the host read fails or returns malformed
  /// bytes.
  fn read_file<'operation>(
    &'operation self,
    path: &'operation Path,
  ) -> LocalEnvironmentFuture<'operation, Result<Vec<u8>, EnvironmentError>>;

  /// Atomically replace or create one file according to the host contract.
  ///
  /// # Errors
  ///
  /// Returns [`EnvironmentError`] when the host write fails.
  fn write_file<'operation>(
    &'operation self,
    path: &'operation Path,
    bytes: &'operation [u8],
  ) -> LocalEnvironmentFuture<'operation, Result<(), EnvironmentError>>;

  /// Find the nearest supported configuration file.
  ///
  /// # Errors
  ///
  /// Returns [`EnvironmentError`] when the host search fails.
  fn find_config_file<'operation>(
    &'operation self,
    from: &'operation Path,
  ) -> LocalEnvironmentFuture<'operation, Result<Option<PathBuf>, EnvironmentError>>;

  /// Find and normalize the nearest supported configuration file.
  ///
  /// # Errors
  ///
  /// Returns the same failures as [`Self::find_config_file`].
  fn find_config_file_normalized<'operation>(
    &'operation self,
    from: &'operation Path,
  ) -> LocalEnvironmentFuture<'operation, Result<Option<PathBuf>, EnvironmentError>> {
    Box::pin(async move { Ok(self.find_config_file(from).await?.map(Normalize::normalize)) })
  }
}

/// Implement [`LocalEnvironment`] while retaining one canonical method and future contract.
///
/// Host adapters supply only their capability bodies and choose local binding names. The macro
/// owns the borrowed operation lifetimes and boxes each asynchronous operation exactly once.
#[macro_export]
macro_rules! implement_local_environment {
  (
    for $environment:ty {
      spawn |$spawn_environment:ident, $future:ident| $spawn:block
      read |$read_environment:ident, $path:ident| $read:block
      write |$write_environment:ident, $write_path:ident, $bytes:ident| $write:block
      find_config |$find_environment:ident, $from:ident| $find:block
    }
  ) => {
    impl $crate::environment::LocalEnvironment for $environment {
      fn spawn_local<Spawned>(
        &self,
        $future: Spawned,
      ) -> Result<(), $crate::environment::EnvironmentError>
      where
        Spawned: std::future::Future<Output = ()> + 'static,
      {
        let $spawn_environment = self;
        $spawn
      }

      fn read_file<'operation>(
        &'operation self,
        $path: &'operation std::path::Path,
      ) -> $crate::environment::LocalEnvironmentFuture<
        'operation,
        Result<Vec<u8>, $crate::environment::EnvironmentError>,
      > {
        let $read_environment = self;
        std::boxed::Box::pin(async move $read)
      }

      fn write_file<'operation>(
        &'operation self,
        $write_path: &'operation std::path::Path,
        $bytes: &'operation [u8],
      ) -> $crate::environment::LocalEnvironmentFuture<
        'operation,
        Result<(), $crate::environment::EnvironmentError>,
      > {
        let $write_environment = self;
        std::boxed::Box::pin(async move $write)
      }

      fn find_config_file<'operation>(
        &'operation self,
        $from: &'operation std::path::Path,
      ) -> $crate::environment::LocalEnvironmentFuture<
        'operation,
        Result<Option<std::path::PathBuf>, $crate::environment::EnvironmentError>,
      > {
        let $find_environment = self;
        std::boxed::Box::pin(async move $find)
      }
    }
  };
}

/// Implement the platform-neutral file URL and absolute-path environment capabilities.
///
/// Hosts with ordinary `url`/path semantics can invoke this inside their [`Environment`]
/// implementation while retaining their own current-directory, glob, and I/O behavior.
#[macro_export]
macro_rules! implement_file_path_environment {
  () => {
    fn to_file_path(&self, url: &url::Url) -> Result<Option<std::path::PathBuf>, $crate::environment::EnvironmentError> {
      Ok(url.to_file_path().ok())
    }

    fn to_file_url(&self, path: &std::path::Path) -> Result<Option<url::Url>, $crate::environment::EnvironmentError> {
      Ok(url::Url::from_file_path(path).ok())
    }

    fn is_absolute(&self, path: &std::path::Path) -> Result<bool, $crate::environment::EnvironmentError> {
      Ok(path.is_absolute())
    }
  };
}

/// Thread-safe asynchronous host capabilities for native concurrent execution.
pub trait ConcurrentEnvironment: Environment + Send + Sync {
  /// Spawn one `Send` task on the concurrent executor.
  fn spawn<F>(&self, future: F)
  where
    F: Future<Output = ()> + Send + 'static;

  /// Read one owned path on a concurrent executor.
  ///
  /// # Errors
  ///
  /// Returns [`EnvironmentError`] when the host read fails.
  fn read_file_concurrent(&self, path: PathBuf) -> impl Future<Output = Result<Vec<u8>, EnvironmentError>> + Send;

  /// Write one owned path and byte buffer on a concurrent executor.
  ///
  /// # Errors
  ///
  /// Returns [`EnvironmentError`] when the host write fails.
  fn write_file_concurrent(&self, path: PathBuf, bytes: Vec<u8>) -> impl Future<Output = Result<(), EnvironmentError>> + Send;

  /// Find the nearest configuration file on a concurrent executor.
  ///
  /// # Errors
  ///
  /// Returns [`EnvironmentError`] when the host search fails.
  fn find_config_file_concurrent(&self, from: PathBuf) -> impl Future<Output = Result<Option<PathBuf>, EnvironmentError>> + Send;
}
