//! Native Tokio-backed environment capabilities and atomic filesystem writes.

use std::env::current_dir;
use std::env::var_os;
use std::env::vars_os;
use std::ffi::OsString;
use std::future::Future;
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::io::IsTerminal as _;
use std::io::stderr as standard_error;
use std::path::Path;
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use time::OffsetDateTime;
use tokio::fs::OpenOptions;
use tokio::fs::metadata;
use tokio::fs::read;
use tokio::fs::remove_file;
use tokio::fs::rename;
use tokio::io::AsyncWriteExt as _;
use tokio::io::Stderr;
use tokio::io::Stdin;
use tokio::io::Stdout;
use tokio::io::stderr as tokio_standard_error;
use tokio::io::stdin;
use tokio::io::stdout;
use tokio::runtime::Handle;

use super::ConcurrentEnvironment;
use super::Environment;
use super::EnvironmentError;
use crate::config::CONFIG_FILE_NAMES;

/// Sequence used to avoid collisions between in-process atomic writes.
static TEMPORARY_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Native Tokio-backed Taplo execution environment.
#[derive(Clone, Debug)]
pub struct NativeEnvironment {
  /// Runtime used for concurrent task spawning.
  handle: Handle,
}

impl NativeEnvironment {
  /// Capture the current Tokio runtime.
  ///
  /// # Errors
  ///
  /// Returns [`EnvironmentError::RuntimeUnavailable`] when no runtime is
  /// active on the current thread.
  #[allow(
    clippy::single_call_fn,
    reason = "the public native constructor captures the ambient Tokio runtime for CLI and host integrations"
  )]
  pub fn new() -> Result<Self, EnvironmentError> {
    Handle::try_current()
      .map(Self::from_handle)
      .map_err(|error| EnvironmentError::RuntimeUnavailable {
        message: error.to_string(),
      })
  }

  /// Construct an environment from an explicit Tokio runtime handle.
  #[must_use]
  #[allow(
    clippy::single_call_fn,
    reason = "the explicit-handle constructor is the public injection boundary for embedders that already own a Tokio runtime"
  )]
  pub const fn from_handle(handle: Handle) -> Self {
    Self {
      handle,
    }
  }
}

impl Environment for NativeEnvironment {
  type Stdin = Stdin;
  type Stdout = Stdout;
  type Stderr = Stderr;

  fn now(&self) -> Result<OffsetDateTime, EnvironmentError> {
    Ok(OffsetDateTime::now_utc())
  }

  fn env_var(&self, name: &str) -> Result<Option<String>, EnvironmentError> {
    var_os(name)
      .map(|environment_value| {
        environment_value
          .into_string()
          .map_err(|_invalid_os_string| EnvironmentError::InvalidEnvironmentUnicode)
      })
      .transpose()
  }

  fn env_vars(&self) -> Result<Vec<(String, String)>, EnvironmentError> {
    vars_os()
      .map(|(environment_name, environment_value)| {
        let name = environment_name
          .into_string()
          .map_err(|_invalid_os_string| EnvironmentError::InvalidEnvironmentUnicode)?;
        let contents = environment_value
          .into_string()
          .map_err(|_invalid_os_string| EnvironmentError::InvalidEnvironmentUnicode)?;
        Ok((name, contents))
      })
      .collect()
  }

  fn atty_stderr(&self) -> Result<bool, EnvironmentError> {
    Ok(standard_error().is_terminal())
  }

  fn stdin(&self) -> Self::Stdin {
    stdin()
  }

  fn stdout(&self) -> Self::Stdout {
    stdout()
  }

  fn stderr(&self) -> Self::Stderr {
    tokio_standard_error()
  }

  fn glob_files(&self, pattern: &str) -> Result<Vec<PathBuf>, EnvironmentError> {
    let paths = glob::glob_with(pattern, glob::MatchOptions {
      case_sensitive: true,
      ..glob::MatchOptions::default()
    })
    .map_err(|source| EnvironmentError::GlobPattern {
      pattern: pattern.to_owned(),
      source,
    })?;
    paths
      .map(|path| {
        path.map_err(|source| EnvironmentError::GlobEntry {
          pattern: pattern.to_owned(),
          source,
        })
      })
      .collect()
  }

  crate::implement_file_path_environment!();

  fn cwd(&self) -> Result<Option<PathBuf>, EnvironmentError> {
    current_dir()
      .map(Some)
      .map_err(|source| EnvironmentError::io("current_directory", ".", source))
  }
}

crate::implement_local_environment! {
  for NativeEnvironment {
    spawn |_environment, future| {
      drop(future);
      Err(EnvironmentError::LocalExecutorUnavailable)
    }
    read |_environment, path| {
      read(path)
        .await
        .map_err(|source| EnvironmentError::io("read_file", path, source))
    }
    write |_environment, path, bytes| {
      atomic_write(path, bytes).await
    }
    find_config |_environment, from| {
      find_config_file(from).await
    }
  }
}

impl ConcurrentEnvironment for NativeEnvironment {
  fn spawn<F>(&self, future: F)
  where
    F: Future<Output = ()> + Send + 'static,
  {
    drop(self.handle.spawn(future));
  }

  async fn read_file_concurrent(&self, path: PathBuf) -> Result<Vec<u8>, EnvironmentError> {
    read(&path)
      .await
      .map_err(|source| EnvironmentError::io("read_file", path, source))
  }

  async fn write_file_concurrent(&self, path: PathBuf, bytes: Vec<u8>) -> Result<(), EnvironmentError> {
    atomic_write(&path, &bytes).await
  }

  async fn find_config_file_concurrent(&self, from: PathBuf) -> Result<Option<PathBuf>, EnvironmentError> {
    find_config_file(&from).await
  }
}

/// Atomically replace one host file through a same-directory temporary file.
async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), EnvironmentError> {
  let file_name = path
    .file_name()
    .ok_or_else(|| EnvironmentError::io("create_atomic_write_path", path, IoError::from(ErrorKind::InvalidInput)))?;
  let sequence = TEMPORARY_FILE_SEQUENCE
    .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| current.checked_add(1))
    .map_err(|_current| EnvironmentError::AtomicWriteSequenceExhausted)?;
  let mut temporary_name = OsString::from(".");
  temporary_name.push(file_name);
  temporary_name.push(format!(".taplo-{}-{sequence}.tmp", id()));
  let temporary_path = path.with_file_name(temporary_name);

  let mut file = OpenOptions::new()
    .create_new(true)
    .write(true)
    .open(&temporary_path)
    .await
    .map_err(|source| EnvironmentError::io("create_atomic_write", &temporary_path, source))?;

  let write_result = async {
    file
      .write_all(bytes)
      .await
      .map_err(|source| EnvironmentError::io("write_atomic_file", &temporary_path, source))?;
    file
      .flush()
      .await
      .map_err(|source| EnvironmentError::io("flush_atomic_file", &temporary_path, source))?;
    file
      .sync_all()
      .await
      .map_err(|source| EnvironmentError::io("sync_atomic_file", &temporary_path, source))
  }
  .await;
  drop(file);

  if let Err(error) = write_result {
    drop(remove_file(&temporary_path).await);
    return Err(error);
  }

  if let Err(source) = rename(&temporary_path, path).await {
    drop(remove_file(&temporary_path).await);
    return Err(EnvironmentError::io("replace_file", path, source));
  }
  Ok(())
}

/// Search parent directories for the nearest supported configuration file.
async fn find_config_file(from: &Path) -> Result<Option<PathBuf>, EnvironmentError> {
  let mut directory = from;
  loop {
    for name in CONFIG_FILE_NAMES {
      let candidate = directory.join(name);
      match metadata(&candidate).await {
        Ok(metadata) if metadata.is_file() => return Ok(Some(candidate)),
        Ok(_) => {}
        Err(source) if source.kind() == ErrorKind::NotFound => {}
        Err(source) => {
          return Err(EnvironmentError::io("inspect_config_candidate", candidate, source));
        }
      }
    }
    let Some(parent) = directory.parent() else {
      return Ok(None);
    };
    directory = parent;
  }
}

#[cfg(test)]
mod tests {
  use std::fmt::Debug;
  use std::fs;
  use std::io;
  use std::io::ErrorKind;
  use std::io::IsTerminal as _;
  use std::io::stderr;
  #[cfg(unix)]
  use std::os::unix::fs::symlink;
  use std::path::Path;
  #[cfg(not(unix))]
  use std::path::PathBuf;
  use std::process::id;
  use std::sync::Arc;
  use std::sync::atomic::AtomicBool;
  use std::sync::atomic::Ordering;

  use strict_test_support::TempDir;
  use strict_test_support::ensure_that;
  use tokio::runtime::Builder;
  use tokio::runtime::Runtime;
  use tokio::task::yield_now;
  use url::Url;

  use super::NativeEnvironment;
  use super::atomic_write;
  use crate::environment::ConcurrentEnvironment as _;
  use crate::environment::Environment as _;
  use crate::environment::EnvironmentError;
  use crate::environment::LocalEnvironment as _;

  /// Native filesystem setup, discovery, and cleanup of a rejected metadata candidate.
  #[cfg(not(unix))]
  type MetadataFailure = (PathBuf, io::Result<()>, Result<Option<PathBuf>, EnvironmentError>, io::Result<()>);

  /// Construct the current-thread executor required by native host operations.
  fn test_runtime() -> io::Result<Runtime> {
    Builder::new_current_thread().enable_all().build()
  }

  #[test]
  fn runtime_capture_and_process_facts_preserve_both_capability_polarities() -> Result<(), impl Debug> {
    let outside_runtime = NativeEnvironment::new();
    let runtime = test_runtime();
    let environment = runtime
      .as_ref()
      .ok()
      .map(|executor| executor.block_on(async { NativeEnvironment::new() }));
    let observations = environment
      .as_ref()
      .and_then(|result| result.as_ref().ok())
      .zip(runtime.as_ref().ok())
      .map(|(host, executor)| {
        let now = host.now();
        let path = host.env_var("PATH");
        let variables = host.env_vars();
        let absent = host.env_var(&format!("TAPLO_TEST_ABSENT_{}", id()));
        let terminal = host.atty_stderr();
        let local = host.spawn_local(async {});
        let completed = Arc::new(AtomicBool::new(false));
        let task_observation = Arc::clone(&completed);
        host.spawn(async move {
          task_observation.store(true, Ordering::Release);
        });
        executor.block_on(yield_now());
        (now, path, variables, absent, terminal, local, completed.load(Ordering::Acquire))
      });
    ensure_that(
      (outside_runtime, runtime, environment, observations),
      "native runtime capture and process facts must retain both capability polarities",
      |actual| {
        let Some(ref facts) = actual.3 else {
          return false;
        };
        let Ok(Some(ref process_path)) = facts.1 else {
          return false;
        };
        matches!(&actual.0, Err(EnvironmentError::RuntimeUnavailable { message }) if !message.is_empty())
          && facts
            .0
            .as_ref()
            .is_ok_and(|instant| *instant > time::OffsetDateTime::UNIX_EPOCH)
          && !process_path.is_empty()
          && facts.2.as_ref().is_ok_and(|variables| {
            variables
              .iter()
              .any(|variable| variable.0 == "PATH" && &variable.1 == process_path)
          })
          && matches!(&facts.3, Ok(None))
          && facts.4.as_ref().is_ok_and(|terminal| *terminal == stderr().is_terminal())
          && matches!(&facts.5, Err(EnvironmentError::LocalExecutorUnavailable))
          && facts.6
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn native_path_capabilities_round_trip_and_reject_incompatible_inputs() -> Result<(), impl Debug> {
    let runtime = test_runtime();
    let remote_url = Url::parse("https://example.invalid/schema.json");
    let observed = runtime.as_ref().ok().map(|executor| {
      let environment = NativeEnvironment::from_handle(executor.handle().clone());
      let cwd = environment.cwd();
      let normalized = environment.cwd_normalized();
      let file_url = cwd
        .as_ref()
        .ok()
        .and_then(Option::as_ref)
        .map(|directory| environment.to_file_url(directory));
      let roundtrip = file_url
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .and_then(Option::as_ref)
        .map(|url| (environment.to_file_path(url), environment.to_file_path_normalized(url)));
      let remote = remote_url.as_ref().ok().map(|url| environment.to_file_path(url));
      let relative = environment.to_file_url(Path::new("relative.toml"));
      let absolute = cwd
        .as_ref()
        .ok()
        .and_then(Option::as_ref)
        .map(|directory| environment.is_absolute(directory));
      let relative_classification = environment.is_absolute(Path::new("relative.toml"));
      (
        cwd, normalized, file_url, roundtrip, remote, relative, absolute, relative_classification,
      )
    });
    ensure_that(
      (runtime, remote_url, observed),
      "native paths must round-trip while relative paths and non-file URLs remain unrepresentable",
      |actual| {
        let Some(ref paths) = actual.2 else {
          return false;
        };
        let Ok(Some(ref directory)) = paths.0 else {
          return false;
        };
        let Some(ref roundtrip) = paths.3 else {
          return false;
        };
        directory.is_absolute()
          && roundtrip.0.as_ref().is_ok_and(|path| path.as_ref() == Some(directory))
          && roundtrip
            .1
            .as_ref()
            .is_ok_and(|path| path.as_ref().is_some_and(|value| value.is_absolute()))
          && paths
            .1
            .as_ref()
            .is_ok_and(|path| path.as_ref().is_some_and(|value| value.is_absolute()))
          && matches!(&paths.4, Some(Ok(None)))
          && matches!(&paths.5, Ok(None))
          && matches!(&paths.6, Some(Ok(true)))
          && matches!(&paths.7, Ok(false))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn native_file_operations_replace_atomically_across_execution_models() -> Result<(), impl Debug> {
    let fixture = TempDir::new("taplo-native-io");
    let runtime = test_runtime();
    let observed = fixture.as_ref().ok().zip(runtime.as_ref().ok()).map(|(directory, executor)| {
      let environment = NativeEnvironment::from_handle(executor.handle().clone());
      let document = directory.child("document.toml");
      let local_write = executor.block_on(environment.write_file(&document, b"value = 1\n"));
      let local_read = executor.block_on(environment.read_file(&document));
      let concurrent_write = executor.block_on(environment.write_file_concurrent(document.clone(), b"value = 2\n".to_vec()));
      let concurrent_read = executor.block_on(environment.read_file_concurrent(document.clone()));
      let committed = fs::read(&document);
      (document, local_write, local_read, concurrent_write, concurrent_read, committed)
    });
    ensure_that(
      (fixture, runtime, observed),
      "local and concurrent native writes must commit complete atomic replacements",
      |actual| {
        let Some(ref io) = actual.2 else {
          return false;
        };
        io.1.is_ok()
          && io.2.as_ref().is_ok_and(|bytes| bytes == b"value = 1\n")
          && io.3.is_ok()
          && io.4.as_ref().is_ok_and(|bytes| bytes == b"value = 2\n")
          && io.5.as_ref().is_ok_and(|bytes| bytes == b"value = 2\n")
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn missing_native_reads_preserve_operation_path_and_kind_in_both_execution_models() -> Result<(), impl Debug> {
    let fixture = TempDir::new("taplo-native-missing");
    let runtime = test_runtime();
    let observed = fixture.as_ref().ok().zip(runtime.as_ref().ok()).map(|(directory, executor)| {
      let environment = NativeEnvironment::from_handle(executor.handle().clone());
      let missing = directory.child("missing.toml");
      let local = executor.block_on(environment.read_file(&missing));
      let concurrent = executor.block_on(environment.read_file_concurrent(missing.clone()));
      (missing, [local, concurrent])
    });
    ensure_that(
      (fixture, runtime, observed),
      "missing native reads must retain their operation, path, and native I/O source",
      |actual| {
        let Some(ref io) = actual.2 else {
          return false;
        };
        io.1.iter().all(|result| {
          matches!(result,
        Err(EnvironmentError::Io { operation: "read_file", path, source }) if path == &io.0 && source.kind() == ErrorKind::NotFound)
        })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn atomic_writes_reject_invalid_targets_and_remove_temporary_files() -> Result<(), impl Debug> {
    let fixture = TempDir::new("taplo-native-atomic-write");
    let runtime = test_runtime();
    let observed = fixture.as_ref().ok().zip(runtime.as_ref().ok()).map(|(directory, executor)| {
      let ownerless = executor.block_on(atomic_write(Path::new("/"), b"invalid"));
      let missing_parent = directory.child("absent").join("document.toml");
      let absent = executor.block_on(atomic_write(&missing_parent, b"invalid"));
      let target = directory.child("directory.toml");
      let created = fs::create_dir_all(&target);
      let replacement = executor.block_on(atomic_write(&target, b"invalid"));
      let entries = fs::read_dir(directory.path()).and_then(Iterator::collect::<Result<Vec<_>, _>>);
      (ownerless, missing_parent, absent, target, created, replacement, entries)
    });
    ensure_that((fixture, runtime, observed), "atomic writes must retain invalid-target failures and remove failed-replacement temporary files", |actual| {
      let Some(ref io) = actual.2 else { return false; };
        matches!(&io.0, Err(EnvironmentError::Io { operation: "create_atomic_write_path", source, .. }) if source.kind() == ErrorKind::InvalidInput)
          && matches!(&io.2, Err(EnvironmentError::Io { operation: "create_atomic_write", .. }))
          && io.4.is_ok()
          && matches!(&io.5, Err(EnvironmentError::Io { operation: "replace_file", path, .. }) if path == &io.3)
          && io.6.as_ref().is_ok_and(|entries| entries.len() == 1)

    }).map(drop).map_err(Box::new)
  }

  #[test]
  fn native_glob_and_configuration_discovery_select_nearest_files_and_recover() -> Result<(), impl Debug> {
    let fixture = TempDir::new("taplo-native-discovery");
    let runtime = test_runtime();
    let observed = fixture.as_ref().ok().zip(runtime.as_ref().ok()).map(|(directory, executor)| {
      let project = directory.child("project");
      let nested = project.join("nested");
      let directories = [fs::create_dir_all(&nested), fs::create_dir_all(project.join("taplo.toml"))];
      let root_config = directory.child(".taplo.toml");
      let seeded = fs::write(&root_config, b"include = [\"**/*.toml\"]\n");
      let environment = NativeEnvironment::from_handle(executor.handle().clone());
      let discoveries = [
        executor.block_on(environment.find_config_file(&nested)),
        executor.block_on(environment.find_config_file_concurrent(nested.clone())),
      ];
      let removed = fs::remove_file(&root_config);
      let absent = executor.block_on(environment.find_config_file(&nested));
      #[cfg(unix)]
      let metadata_failures = {
        let candidate = nested.join(".taplo.toml");
        let linked = symlink(&candidate, &candidate);
        let result = executor.block_on(environment.find_config_file(&nested));
        let unlinked = fs::remove_file(&candidate);
        Vec::from([(candidate, linked, result, unlinked)])
      };
      #[cfg(not(unix))]
      let metadata_failures = Vec::<MetadataFailure>::new();
      let matched = directory.child("matched.toml");
      let ignored = directory.child("ignored.txt");
      let files = [fs::write(&matched, b"value = 1\n"), fs::write(ignored, b"ignored\n")];
      let pattern = directory.child("*.toml");
      let globbed = pattern.to_str().map(|value| environment.glob_files_normalized(value));
      let invalid = environment.glob_files("[");
      (
        directories, root_config, seeded, discoveries, removed, absent, metadata_failures, matched, files, pattern, globbed, invalid,
      )
    });
    ensure_that(
      (fixture, runtime, observed),
      "configuration discovery must select nearest files, recover from removal, retain metadata failures, and preserve glob selection",
      |actual| {
        let Some(ref io) = actual.2 else {
          return false;
        };
        let (
          ref directories,
          ref root_config,
          ref seeded,
          ref discoveries,
          ref removed,
          ref absent,
          ref metadata_failures,
          ref matched,
          ref files,
          ref _pattern,
          ref globbed,
          ref invalid,
        ) = *io;
        directories.iter().all(Result::is_ok)
          && seeded.is_ok()
          && discoveries
            .iter()
            .all(|result| result.as_ref().is_ok_and(|path| path.as_ref() == Some(root_config)))
          && removed.is_ok()
          && matches!(absent, Ok(None))
          && metadata_failures.iter().all(|failure| {
            failure.1.is_ok()
              && failure.3.is_ok()
              && matches!(&failure.2, Err(EnvironmentError::Io { operation: "inspect_config_candidate", path, .. }) if path == &failure.0)
          })
          && files.iter().all(Result::is_ok)
          && globbed
            .as_ref()
            .is_some_and(|result| result.as_ref().is_ok_and(|paths| paths.as_slice() == [matched.clone()]))
          && matches!(invalid, Err(EnvironmentError::GlobPattern { pattern, .. }) if pattern == "[")
      },
    )
    .map(drop)
    .map_err(Box::new)
  }
}
