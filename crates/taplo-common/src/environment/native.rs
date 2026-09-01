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
  use std::fs;
  use std::io::ErrorKind;
  use std::io::IsTerminal as _;
  use std::io::stderr;
  #[cfg(unix)]
  use std::os::unix::fs::symlink;
  use std::path::Path;
  use std::path::PathBuf;
  use std::process::id;
  use std::sync::Arc;
  use std::sync::atomic::AtomicBool;
  use std::sync::atomic::Ordering;

  use strict_test_support::TempDir;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
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

  /// Construct the current-thread executor required by native host operations.
  fn test_runtime() -> Result<Runtime, TestFailure> {
    ensure_ok(
      Builder::new_current_thread().enable_all().build(),
      "the native-environment test runtime must initialize",
    )
  }

  /// Project one typed I/O failure into its comparable operation, path, and kind facts.
  fn io_failure_facts(failure: EnvironmentError) -> Option<(&'static str, PathBuf, ErrorKind)> {
    if let EnvironmentError::Io {
      operation,
      path,
      source,
    } = failure
    {
      Some((operation, path, source.kind()))
    } else {
      None
    }
  }

  #[test]
  fn runtime_capture_and_process_facts_preserve_both_capability_polarities() -> Result<(), TestFailure> {
    ensure(
      matches!(
        NativeEnvironment::new(),
        Err(EnvironmentError::RuntimeUnavailable {
          message
        }) if !message.is_empty()
      ),
      "native environment construction must reject a thread without an entered Tokio runtime",
    )?;

    let runtime = test_runtime()?;
    let environment = ensure_ok(
      runtime.block_on(async { NativeEnvironment::new() }),
      "native environment construction must capture an entered Tokio runtime",
    )?;
    ensure(
      ensure_ok(environment.now(), "the native clock must be readable")? > time::OffsetDateTime::UNIX_EPOCH,
      "the native clock must report a post-epoch UTC instant",
    )?;
    let path = ensure_some(
      ensure_ok(environment.env_var("PATH"), "the native process environment must be readable")?,
      "the native test process must expose PATH",
    )?;
    ensure(!path.is_empty(), "the native PATH value must not be empty")?;
    let variables = ensure_ok(environment.env_vars(), "all native process environment entries must be readable")?;
    ensure(
      variables
        .iter()
        .any(|variable| (variable.0.as_str(), variable.1.as_str()) == ("PATH", path.as_str())),
      "bulk environment enumeration must retain the individually observed PATH entry",
    )?;
    let absent_name = format!("TAPLO_TEST_ABSENT_{}", id());
    ensure(
      ensure_ok(
        environment.env_var(&absent_name),
        "an absent native environment entry must remain readable",
      )?
      .is_none(),
      "an unconfigured native environment entry must remain absent",
    )?;
    ensure_eq(
      &ensure_ok(environment.atty_stderr(), "native terminal detection must succeed")?,
      &stderr().is_terminal(),
      "native terminal detection must reflect the actual standard-error handle",
    )?;

    ensure(
      matches!(environment.spawn_local(async {}), Err(EnvironmentError::LocalExecutorUnavailable)),
      "the concurrent native host must reject local-only task spawning",
    )?;
    let task_completed = Arc::new(AtomicBool::new(false));
    let task_observation = Arc::clone(&task_completed);
    environment.spawn(async move {
      task_observation.store(true, Ordering::Release);
    });
    runtime.block_on(yield_now());
    ensure(
      task_completed.load(Ordering::Acquire),
      "the captured native runtime handle must execute spawned concurrent tasks",
    )
  }

  #[test]
  fn native_path_capabilities_round_trip_and_reject_incompatible_inputs() -> Result<(), TestFailure> {
    let runtime = test_runtime()?;
    let environment = NativeEnvironment::from_handle(runtime.handle().clone());
    let working_directory = ensure_some(
      ensure_ok(environment.cwd(), "the native current directory must be readable")?,
      "the native host must expose a current directory",
    )?;
    ensure(working_directory.is_absolute(), "the native current directory must be absolute")?;
    ensure(
      ensure_some(
        ensure_ok(
          environment.cwd_normalized(),
          "the normalized native current directory must be readable",
        )?,
        "the normalized native host must retain its current directory",
      )?
      .is_absolute(),
      "normalization must preserve current-directory absoluteness",
    )?;

    let file_url = ensure_some(
      ensure_ok(
        environment.to_file_url(&working_directory),
        "an absolute native path must convert to a file URL",
      )?,
      "an absolute native path must have a file-URL representation",
    )?;
    ensure(
      ensure_ok(environment.to_file_path(&file_url), "a native file URL must convert back to a path")? == Some(working_directory.clone()),
      "native path and file-URL conversion must round-trip",
    )?;
    ensure(
      ensure_ok(
        environment.to_file_path_normalized(&file_url),
        "a native file URL must convert to a normalized path",
      )?
      .is_some_and(|path| path.is_absolute()),
      "normalized native file-URL conversion must retain an absolute path",
    )?;
    let remote_url = ensure_ok(
      Url::parse("https://example.invalid/schema.json"),
      "the non-file URL fixture must parse",
    )?;
    ensure(
      ensure_ok(
        environment.to_file_path(&remote_url),
        "a non-file URL must be handled without a host callback failure",
      )?
      .is_none(),
      "a non-file URL must not fabricate a native filesystem path",
    )?;
    ensure(
      ensure_ok(
        environment.to_file_url(Path::new("relative.toml")),
        "a relative path must be handled without a host callback failure",
      )?
      .is_none(),
      "a relative path must not fabricate an absolute file URL",
    )?;
    ensure(
      ensure_ok(
        environment.is_absolute(&working_directory),
        "absolute-path classification must succeed",
      )?,
      "the native path classifier must accept the current directory",
    )?;
    ensure(
      !ensure_ok(
        environment.is_absolute(Path::new("relative.toml")),
        "relative-path classification must succeed",
      )?,
      "the native path classifier must reject a relative path",
    )
  }

  #[test]
  fn native_file_operations_replace_atomically_across_execution_models() -> Result<(), TestFailure> {
    let fixture = TempDir::new("taplo-native-io")?;
    let runtime = test_runtime()?;
    let environment = NativeEnvironment::from_handle(runtime.handle().clone());
    let document = fixture.child("document.toml");

    ensure_ok(
      runtime.block_on(environment.write_file(&document, b"value = 1\n")),
      "the local native writer must create a file atomically",
    )?;
    ensure(
      ensure_ok(
        runtime.block_on(environment.read_file(&document)),
        "the local native reader must read the created file",
      )? == b"value = 1\n",
      "the local native reader must preserve exact bytes",
    )?;
    ensure_ok(
      runtime.block_on(environment.write_file_concurrent(document.clone(), b"value = 2\n".to_vec())),
      "the concurrent native writer must replace the file atomically",
    )?;
    ensure(
      ensure_ok(
        runtime.block_on(environment.read_file_concurrent(document.clone())),
        "the concurrent native reader must read the replacement",
      )? == b"value = 2\n",
      "the concurrent native reader must observe the complete replacement",
    )?;
    ensure(
      ensure_ok(fs::read(&document), "the atomically replaced host file must remain readable")? == b"value = 2\n",
      "atomic replacement must commit the exact requested bytes",
    )
  }

  #[test]
  fn missing_native_reads_preserve_operation_path_and_kind_in_both_execution_models() -> Result<(), TestFailure> {
    let fixture = TempDir::new("taplo-native-missing")?;
    let runtime = test_runtime()?;
    let environment = NativeEnvironment::from_handle(runtime.handle().clone());
    let missing = fixture.child("missing.toml");

    let local_failure = ensure_some(
      runtime.block_on(environment.read_file(&missing)).err(),
      "a missing local file must return a typed failure",
    )?;
    ensure(
      io_failure_facts(local_failure) == Some(("read_file", missing.clone(), ErrorKind::NotFound)),
      "a missing local file must retain its operation, path, and typed I/O source",
    )?;
    let concurrent_failure = ensure_some(
      runtime.block_on(environment.read_file_concurrent(missing.clone())).err(),
      "a missing concurrent file must return a typed failure",
    )?;
    ensure(
      io_failure_facts(concurrent_failure) == Some(("read_file", missing, ErrorKind::NotFound)),
      "a missing concurrent file must retain its operation, path, and typed I/O source",
    )
  }

  #[test]
  fn atomic_writes_reject_invalid_targets_and_remove_temporary_files() -> Result<(), TestFailure> {
    let fixture = TempDir::new("taplo-native-atomic-write")?;
    let runtime = test_runtime()?;

    ensure(
      matches!(
        runtime.block_on(atomic_write(Path::new("/"), b"invalid")),
        Err(EnvironmentError::Io {
          operation: "create_atomic_write_path",
          source,
          ..
        }) if source.kind() == ErrorKind::InvalidInput
      ),
      "an ownerless path must be rejected before an atomic temporary file is created",
    )?;
    let missing_parent = fixture.child("absent").join("document.toml");
    ensure(
      matches!(
        runtime.block_on(atomic_write(&missing_parent, b"invalid")),
        Err(EnvironmentError::Io {
          operation: "create_atomic_write",
          ..
        })
      ),
      "an absent parent directory must retain the atomic-create failure boundary",
    )?;

    let directory_target = fixture.child("directory.toml");
    ensure_ok(
      fs::create_dir_all(&directory_target),
      "the atomic-replacement directory target must be created",
    )?;
    ensure(
      matches!(
        runtime.block_on(atomic_write(&directory_target, b"invalid")),
        Err(EnvironmentError::Io {
          operation: "replace_file",
          path,
          ..
        }) if path == directory_target
      ),
      "a directory replacement target must retain the typed rename boundary",
    )?;
    let fixture_entries = ensure_ok(
      fs::read_dir(fixture.path()).and_then(Iterator::collect::<Result<Vec<_>, _>>),
      "the atomic-write fixture directory must remain enumerable",
    )?;
    ensure_eq(
      &fixture_entries.len(),
      &1_usize,
      "failed atomic replacement must remove its temporary file",
    )
  }

  #[test]
  fn native_glob_and_configuration_discovery_select_nearest_files_and_recover() -> Result<(), TestFailure> {
    let fixture = TempDir::new("taplo-native-discovery")?;
    let project = fixture.child("project");
    let nested = project.join("nested");
    ensure_ok(
      fs::create_dir_all(&nested),
      "the nested configuration search fixture must be created",
    )?;
    ensure_ok(
      fs::create_dir_all(project.join("taplo.toml")),
      "a directory-shaped candidate must be created",
    )?;
    let root_config = fixture.child(".taplo.toml");
    ensure_ok(
      fs::write(&root_config, b"include = [\"**/*.toml\"]\n"),
      "the root configuration fixture must be written",
    )?;

    let runtime = test_runtime()?;
    let environment = NativeEnvironment::from_handle(runtime.handle().clone());
    ensure(
      ensure_ok(
        runtime.block_on(environment.find_config_file(&nested)),
        "local configuration discovery must complete",
      )? == Some(root_config.clone()),
      "local configuration discovery must skip non-files and select the nearest supported file",
    )?;
    ensure(
      ensure_ok(
        runtime.block_on(environment.find_config_file_concurrent(nested.clone())),
        "concurrent configuration discovery must complete",
      )? == Some(root_config.clone()),
      "concurrent configuration discovery must share nearest-file semantics",
    )?;

    ensure_ok(fs::remove_file(&root_config), "the root configuration fixture must be removable")?;
    ensure(
      ensure_ok(
        runtime.block_on(environment.find_config_file(&nested)),
        "configuration discovery must complete after removal",
      )?
      .is_none(),
      "configuration discovery must report absence after the selected file is removed",
    )?;

    #[cfg(unix)]
    {
      let looping_candidate = nested.join(".taplo.toml");
      ensure_ok(
        symlink(&looping_candidate, &looping_candidate),
        "the metadata-failure symlink fixture must be created",
      )?;
      ensure(
        matches!(
          runtime.block_on(environment.find_config_file(&nested)),
          Err(EnvironmentError::Io {
            operation: "inspect_config_candidate",
            path,
            ..
          }) if path == looping_candidate
        ),
        "configuration discovery must retain unexpected metadata failures",
      )?;
      ensure_ok(
        fs::remove_file(looping_candidate),
        "the metadata-failure symlink fixture must be removable",
      )?;
    };

    let matched = fixture.child("matched.toml");
    let ignored = fixture.child("ignored.txt");
    ensure_ok(fs::write(&matched, b"value = 1\n"), "the matched glob fixture must be written")?;
    ensure_ok(fs::write(ignored, b"ignored\n"), "the ignored glob fixture must be written")?;
    let pattern_path = fixture.child("*.toml");
    let pattern = ensure_some(pattern_path.to_str(), "the temporary glob fixture path must be valid Unicode")?;
    ensure(
      ensure_ok(
        environment.glob_files_normalized(pattern),
        "the native glob must enumerate matching paths",
      )? == [matched],
      "the native glob must include only matching regular paths",
    )?;
    ensure(
      matches!(
        environment.glob_files("["),
        Err(EnvironmentError::GlobPattern {
          pattern: rejected,
          ..
        }) if rejected == "["
      ),
      "an invalid native glob must retain its rejected expression and typed parser source",
    )
  }
}
