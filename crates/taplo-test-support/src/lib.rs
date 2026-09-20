//! Deterministic host capabilities shared by Taplo workspace behavior tests.
//!
//! This private workspace crate owns the in-memory filesystem, controllable
//! clock, configuration discovery, and injected I/O failures used to exercise
//! both local and concurrent environment contracts.

use std::collections::HashMap;
use std::error::Error;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::fmt::Result as FmtResult;
use std::future::Future;
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use futures::executor::block_on;
use futures::task::AtomicWaker;
use parking_lot::RwLock;
use strict_test_support::ResultFailure;
use strict_test_support::ensure_ok;
use time::OffsetDateTime;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::ReadBuf;

/// Kind reported by an I/O failure injected into [`TestEnvironment`].
const INJECTED_IO_FAILURE: ErrorKind = ErrorKind::PermissionDenied;

/// Shared, synchronously mutable fixture state.
type Shared<T> = Arc<RwLock<T>>;

/// In-memory file contents indexed by their host path.
type MemoryFiles = HashMap<PathBuf, Vec<u8>>;

/// Mutable bytes and close state for one interactive standard-input pipe.
#[derive(Debug, Default)]
struct InputPipeState {
  /// Bytes accepted from the fixture-owned writer.
  bytes:  Vec<u8>,
  /// Whether the writer has closed the input stream.
  closed: bool,
}

/// Shared byte-source state paired with its waiting reader.
struct ReadCoordination<State> {
  /// Mutable bytes and source-specific availability state.
  state: Shared<State>,
  /// Reader waiting for bytes or an availability transition.
  waker: Arc<AtomicWaker>,
}

impl<State> Clone for ReadCoordination<State> {
  fn clone(&self) -> Self {
    Self {
      state: Arc::clone(&self.state),
      waker: Arc::clone(&self.waker),
    }
  }
}

impl<State: Debug> Debug for ReadCoordination<State> {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> FmtResult {
    formatter
      .debug_struct("ReadCoordination")
      .field("state", &self.state)
      .finish_non_exhaustive()
  }
}

/// Shared coordination for one interactive standard-input pipe.
type InteractiveInput = ReadCoordination<InputPipeState>;

/// Shared coordination for one captured output reader.
type CapturedOutput = ReadCoordination<Vec<u8>>;

/// Deterministic byte source selected for one standard-channel reader.
#[derive(Clone, Debug)]
enum TestReadSource {
  /// Complete bytes available immediately to every new reader.
  Snapshot(Vec<u8>),
  /// Shared pipe whose writer controls byte and EOF availability.
  Interactive(InteractiveInput),
  /// Shared output capture that waits indefinitely at its current end.
  Captured(CapturedOutput),
}

impl Default for TestReadSource {
  fn default() -> Self {
    Self::Snapshot(Vec::new())
  }
}

/// Copy currently available bytes into one asynchronous read buffer.
fn read_available(bytes: &[u8], position: usize, buffer: &mut ReadBuf<'_>) -> usize {
  let chunk = bytes
    .get(position..)
    .unwrap_or_default()
    .iter()
    .take(buffer.remaining())
    .copied()
    .collect::<Vec<_>>();
  buffer.put_slice(&chunk);
  chunk.len()
}

/// Reader over deterministic snapshot, interactive input, or captured output.
#[derive(Debug)]
pub struct TestInput {
  /// Input source selected when this reader was requested.
  source:   TestReadSource,
  /// Number of bytes already consumed.
  position: usize,
}

/// Reader over bytes captured from one deterministic output channel.
pub type TestOutputReader = TestInput;

impl TestInput {
  /// Connect one fresh reader to a captured output channel.
  fn captured(bytes: &Shared<Vec<u8>>, waker: &Arc<AtomicWaker>) -> Self {
    Self {
      source:   TestReadSource::Captured(CapturedOutput {
        state: Arc::clone(bytes),
        waker: Arc::clone(waker),
      }),
      position: 0,
    }
  }
}

impl AsyncRead for TestInput {
  fn poll_read(self: Pin<&mut Self>, context: &mut Context<'_>, buffer: &mut ReadBuf<'_>) -> Poll<Result<(), IoError>> {
    let input = self.get_mut();
    if buffer.remaining() == 0 {
      return Poll::Ready(Ok(()));
    }
    match input.source {
      TestReadSource::Snapshot(ref bytes) => {
        let read = read_available(bytes, input.position, buffer);
        input.position = input.position.saturating_add(read);
        Poll::Ready(Ok(()))
      }
      TestReadSource::Interactive(ref pipe) => {
        pipe.waker.register(context.waker());
        let state = pipe.state.read();
        let read = read_available(&state.bytes, input.position, buffer);
        let closed = state.closed;
        drop(state);
        input.position = input.position.saturating_add(read);
        if read == 0 && !closed {
          Poll::Pending
        } else {
          Poll::Ready(Ok(()))
        }
      }
      TestReadSource::Captured(ref capture) => {
        capture.waker.register(context.waker());
        let bytes = capture.state.read();
        input.position = input.position.min(bytes.len());
        let read = read_available(&bytes, input.position, buffer);
        drop(bytes);
        input.position = input.position.saturating_add(read);
        if read == 0 { Poll::Pending } else { Poll::Ready(Ok(())) }
      }
    }
  }
}

/// Fixture-owned writer feeding one interactive standard-input reader.
#[derive(Clone, Debug)]
pub struct TestInputWriter {
  /// Shared pipe receiving exact written bytes.
  pipe: InteractiveInput,
}

impl AsyncWrite for TestInputWriter {
  fn poll_write(self: Pin<&mut Self>, _context: &mut Context<'_>, buffer: &[u8]) -> Poll<Result<usize, IoError>> {
    let mut state = self.pipe.state.write();
    if state.closed {
      return Poll::Ready(Err(IoError::from(ErrorKind::BrokenPipe)));
    }
    state.bytes.extend_from_slice(buffer);
    drop(state);
    self.pipe.waker.wake();
    Poll::Ready(Ok(buffer.len()))
  }

  fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Result<(), IoError>> {
    Poll::Ready(Ok(()))
  }

  fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Result<(), IoError>> {
    self.pipe.state.write().closed = true;
    self.pipe.waker.wake();
    Poll::Ready(Ok(()))
  }
}

/// Writer that captures one deterministic standard-output channel.
#[derive(Clone, Debug)]
pub struct TestOutput {
  /// Bytes written to this channel.
  bytes: Shared<Vec<u8>>,
  /// Whether output operations should fail.
  fail:  Shared<bool>,
  /// Reader waiting for newly captured bytes.
  waker: Arc<AtomicWaker>,
}

impl TestOutput {
  /// Connect one output handle to its shared byte, failure, and wake state.
  fn connected(bytes: &Shared<Vec<u8>>, fail: &Shared<bool>, waker: &Arc<AtomicWaker>) -> Self {
    Self {
      bytes: Arc::clone(bytes),
      fail:  Arc::clone(fail),
      waker: Arc::clone(waker),
    }
  }

  /// Return the configured result for an operation without buffered state.
  fn operation_result(&self) -> Result<(), IoError> {
    if *self.fail.read() {
      Err(IoError::from(INJECTED_IO_FAILURE))
    } else {
      Ok(())
    }
  }
}

impl AsyncWrite for TestOutput {
  fn poll_write(self: Pin<&mut Self>, _context: &mut Context<'_>, buffer: &[u8]) -> Poll<Result<usize, IoError>> {
    if let Err(error) = self.operation_result() {
      return Poll::Ready(Err(error));
    }
    self.bytes.write().extend_from_slice(buffer);
    self.waker.wake();
    Poll::Ready(Ok(buffer.len()))
  }

  fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Result<(), IoError>> {
    Poll::Ready(self.operation_result())
  }

  fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Result<(), IoError>> {
    Poll::Ready(self.operation_result())
  }
}

/// Extract a successful result while preserving its concrete error on failure.
///
/// # Errors
///
/// Returns [`ResultFailure`] with the supplied context and original error when
/// `result` contains an error.
pub fn ensure_result<T, E: Error>(result: Result<T, E>, context: &'static str) -> Result<T, ResultFailure<E>> {
  ensure_ok(result, context)
}

/// Drive one fixture-owned future to completion.
pub fn drive<F: Future>(future: F) -> F::Output {
  block_on(future)
}

/// Implement Taplo's base and local environment traits for a tuple adapter.
///
/// The invocation module supplies Taplo's environment types and the standard
/// stream/path vocabulary in scope. Keeping this expansion in the shared
/// fixture owner prevents test crates from copying host adaptation behavior
/// while allowing each test target to implement its own visible trait identity.
#[macro_export]
macro_rules! implement_local_test_environment {
  ($adapter:ident, $config_names:path, $path_implementation:path, $local_implementation:path) => {
    impl Deref for $adapter {
      type Target = $crate::TestEnvironment;

      fn deref(&self) -> &Self::Target {
        &self.0
      }
    }

    impl Environment for $adapter {
      type Stdin = $crate::TestInput;
      type Stdout = $crate::TestOutput;
      type Stderr = $crate::TestOutput;

      fn now(&self) -> Result<OffsetDateTime, EnvironmentError> {
        self.0.now().ok_or(EnvironmentError::MissingCallback {
          name: "now"
        })
      }

      fn env_var(&self, name: &str) -> Result<Option<String>, EnvironmentError> {
        Ok(self.0.env_var(name))
      }

      fn env_vars(&self) -> Result<Vec<(String, String)>, EnvironmentError> {
        Ok(self.0.env_vars())
      }

      fn atty_stderr(&self) -> Result<bool, EnvironmentError> {
        Ok(false)
      }

      fn stdin(&self) -> Self::Stdin {
        self.0.stdin_reader()
      }

      fn stdout(&self) -> Self::Stdout {
        self.0.stdout_writer()
      }

      fn stderr(&self) -> Self::Stderr {
        self.0.stderr_writer()
      }

      fn glob_files(&self, _pattern: &str) -> Result<Vec<PathBuf>, EnvironmentError> {
        Ok(self.0.file_paths())
      }

      $path_implementation!();

      fn cwd(&self) -> Result<Option<PathBuf>, EnvironmentError> {
        Ok(self.0.cwd())
      }
    }

    $local_implementation! {
      for $adapter {
        spawn |_environment, future| {
          $crate::drive(future);
          Ok(())
        }
        read |environment, path| {
          environment
            .0
            .read_file(path)
            .map_err(|source| EnvironmentError::io("read_file", path, source))
        }
        write |environment, path, bytes| {
          environment
            .0
            .write_file(path.to_path_buf(), bytes.to_vec())
            .map_err(|source| EnvironmentError::io("write_file", path, source))
        }
        find_config |environment, from| {
          Ok(environment.0.find_config_file(from, $config_names))
        }
      }
    }
  };
}

/// Implement Taplo's concurrent environment trait for a tuple adapter.
///
/// Invoke this after [`implement_local_test_environment`] when the consumer
/// genuinely exercises Taplo's native concurrent capability family.
#[macro_export]
macro_rules! implement_concurrent_test_environment {
  ($adapter:ident, $config_names:path) => {
    impl ConcurrentEnvironment for $adapter {
      fn spawn<Spawned>(&self, future: Spawned)
      where
        Spawned: Future<Output = ()> + Send + 'static,
      {
        $crate::drive(future);
      }

      fn read_file_concurrent(&self, path: PathBuf) -> impl Future<Output = Result<Vec<u8>, EnvironmentError>> + Send {
        let environment = self.clone();
        async move {
          environment
            .0
            .read_file(&path)
            .map_err(|source| EnvironmentError::io("read_file", path, source))
        }
      }

      fn write_file_concurrent(&self, path: PathBuf, bytes: Vec<u8>) -> impl Future<Output = Result<(), EnvironmentError>> + Send {
        let environment = self.clone();
        async move {
          environment
            .0
            .write_file(path.clone(), bytes)
            .map_err(|source| EnvironmentError::io("write_file", path, source))
        }
      }

      fn find_config_file_concurrent(&self, from: PathBuf) -> impl Future<Output = Result<Option<PathBuf>, EnvironmentError>> + Send {
        let environment = self.clone();
        async move { Ok(environment.0.find_config_file(&from, $config_names)) }
      }
    }
  };
}

/// In-memory host with deterministic observations and injectable capabilities.
#[derive(Clone, Debug)]
pub struct TestEnvironment {
  /// Whether the host clock callback is available.
  clock_available: Shared<bool>,
  /// Current deterministic host time.
  now:             Shared<OffsetDateTime>,
  /// Optional current working directory.
  cwd:             Shared<Option<PathBuf>>,
  /// Deterministic process environment.
  env_vars:        Shared<HashMap<String, String>>,
  /// Standard-input source copied or connected into each requested reader.
  stdin:           Shared<TestReadSource>,
  /// Bytes written to standard output.
  stdout:          Shared<Vec<u8>>,
  /// Reader waiting for standard-output bytes.
  stdout_waker:    Arc<AtomicWaker>,
  /// Bytes written to standard error.
  stderr:          Shared<Vec<u8>>,
  /// Reader waiting for standard-error bytes.
  stderr_waker:    Arc<AtomicWaker>,
  /// In-memory file contents.
  files:           Shared<MemoryFiles>,
  /// Paths written through the host.
  writes:          Shared<Vec<PathBuf>>,
  /// Bases passed to configuration discovery.
  discovery_bases: Shared<Vec<PathBuf>>,
  /// Whether reads should fail.
  fail_reads:      Shared<bool>,
  /// Whether writes should fail.
  fail_writes:     Shared<bool>,
  /// Whether standard-output operations should fail.
  fail_stdout:     Shared<bool>,
  /// Whether standard-error operations should fail.
  fail_stderr:     Shared<bool>,
}

impl Default for TestEnvironment {
  fn default() -> Self {
    Self {
      clock_available: Arc::new(RwLock::new(true)),
      now:             Arc::new(RwLock::new(OffsetDateTime::UNIX_EPOCH)),
      cwd:             Arc::new(RwLock::new(Some(PathBuf::from("/workspace")))),
      env_vars:        Arc::default(),
      stdin:           Arc::default(),
      stdout:          Arc::default(),
      stdout_waker:    Arc::default(),
      stderr:          Arc::default(),
      stderr_waker:    Arc::default(),
      files:           Arc::default(),
      writes:          Arc::default(),
      discovery_bases: Arc::default(),
      fail_reads:      Arc::default(),
      fail_writes:     Arc::default(),
      fail_stdout:     Arc::default(),
      fail_stderr:     Arc::default(),
    }
  }
}

impl TestEnvironment {
  /// Replace whether the host clock callback is available.
  pub fn set_clock_available(&self, available: bool) {
    *self.clock_available.write() = available;
  }

  /// Replace the deterministic host time.
  pub fn set_now(&self, now: OffsetDateTime) {
    *self.now.write() = now;
  }

  /// Replace the optional current working directory.
  pub fn set_cwd(&self, cwd: Option<PathBuf>) {
    *self.cwd.write() = cwd;
  }

  /// Set one deterministic process environment variable.
  pub fn set_env_var(&self, name: impl Into<String>, contents: impl Into<String>) {
    drop(self.env_vars.write().insert(name.into(), contents.into()));
  }

  /// Replace bytes copied into subsequently requested standard-input readers.
  pub fn set_stdin(&self, bytes: impl Into<Vec<u8>>) {
    *self.stdin.write() = TestReadSource::Snapshot(bytes.into());
  }

  /// Select an interactive standard-input pipe and return its fixture-owned writer.
  #[must_use]
  pub fn interactive_stdin(&self) -> TestInputWriter {
    let pipe = InteractiveInput {
      state: Arc::default(),
      waker: Arc::default(),
    };
    *self.stdin.write() = TestReadSource::Interactive(pipe.clone());
    TestInputWriter {
      pipe,
    }
  }

  /// Clear all captured standard-output and standard-error bytes.
  pub fn clear_output(&self) {
    self.stdout.write().clear();
    self.stderr.write().clear();
  }

  /// Seed or replace one readable in-memory file.
  pub fn insert_file(&self, path: impl Into<PathBuf>, bytes: impl Into<Vec<u8>>) {
    drop(self.files.write().insert(path.into(), bytes.into()));
  }

  /// Enable or disable injected read failures.
  pub fn set_read_failure(&self, enabled: bool) {
    *self.fail_reads.write() = enabled;
  }

  /// Enable or disable injected write failures.
  pub fn set_write_failure(&self, enabled: bool) {
    *self.fail_writes.write() = enabled;
  }

  /// Enable or disable injected standard-output failures.
  pub fn set_stdout_failure(&self, enabled: bool) {
    *self.fail_stdout.write() = enabled;
  }

  /// Enable or disable injected standard-error failures.
  pub fn set_stderr_failure(&self, enabled: bool) {
    *self.fail_stderr.write() = enabled;
  }

  /// Return a fresh reader over the current standard-input source.
  #[must_use]
  pub fn stdin_reader(&self) -> TestInput {
    TestInput {
      source:   self.stdin.read().clone(),
      position: 0,
    }
  }

  /// Return a writer connected to captured standard output.
  #[must_use]
  pub fn stdout_writer(&self) -> TestOutput {
    TestOutput::connected(&self.stdout, &self.fail_stdout, &self.stdout_waker)
  }

  /// Return a writer connected to captured standard error.
  #[must_use]
  pub fn stderr_writer(&self) -> TestOutput {
    TestOutput::connected(&self.stderr, &self.fail_stderr, &self.stderr_waker)
  }

  /// Return a reader waiting for captured standard-output bytes.
  #[must_use]
  pub fn stdout_reader(&self) -> TestOutputReader {
    TestInput::captured(&self.stdout, &self.stdout_waker)
  }

  /// Return a reader waiting for captured standard-error bytes.
  #[must_use]
  pub fn stderr_reader(&self) -> TestOutputReader {
    TestInput::captured(&self.stderr, &self.stderr_waker)
  }

  /// Return captured standard-output bytes.
  #[must_use]
  pub fn stdout(&self) -> Vec<u8> {
    self.stdout.read().clone()
  }

  /// Return captured standard-error bytes.
  #[must_use]
  pub fn stderr(&self) -> Vec<u8> {
    self.stderr.read().clone()
  }

  /// Return paths successfully written through the host.
  #[must_use]
  pub fn writes(&self) -> Vec<PathBuf> {
    self.writes.read().clone()
  }

  /// Return bases passed to configuration discovery.
  #[must_use]
  pub fn discovery_bases(&self) -> Vec<PathBuf> {
    self.discovery_bases.read().clone()
  }

  /// Return the configured time when the deterministic clock is available.
  #[must_use]
  pub fn now(&self) -> Option<OffsetDateTime> {
    (*self.clock_available.read()).then(|| *self.now.read())
  }

  /// Return one deterministic process environment variable.
  #[must_use]
  pub fn env_var(&self, name: &str) -> Option<String> {
    self.env_vars.read().get(name).cloned()
  }

  /// Return all deterministic process environment variables in name order.
  #[must_use]
  pub fn env_vars(&self) -> Vec<(String, String)> {
    let mut variables = self
      .env_vars
      .read()
      .iter()
      .map(|(name, contents)| (name.clone(), contents.clone()))
      .collect::<Vec<_>>();
    variables.sort_by(|left, right| left.0.cmp(&right.0));
    variables
  }

  /// Return every seeded file path in lexical order.
  #[must_use]
  pub fn file_paths(&self) -> Vec<PathBuf> {
    let mut paths = self.files.read().keys().cloned().collect::<Vec<_>>();
    paths.sort();
    paths
  }

  /// Return the optional deterministic current working directory.
  #[must_use]
  pub fn cwd(&self) -> Option<PathBuf> {
    self.cwd.read().clone()
  }

  /// Read one in-memory file.
  ///
  /// # Errors
  ///
  /// Returns a permission-denied error when read failure injection is active
  /// and a not-found error when `path` has not been seeded.
  pub fn read_file(&self, path: &Path) -> Result<Vec<u8>, IoError> {
    if *self.fail_reads.read() {
      return Err(IoError::from(INJECTED_IO_FAILURE));
    }
    self
      .files
      .read()
      .get(path)
      .cloned()
      .ok_or_else(|| IoError::from(ErrorKind::NotFound))
  }

  /// Persist one in-memory file and record the successful write.
  ///
  /// # Errors
  ///
  /// Returns a permission-denied error while write failure injection is active.
  pub fn write_file(&self, path: PathBuf, bytes: Vec<u8>) -> Result<(), IoError> {
    if *self.fail_writes.read() {
      return Err(IoError::from(INJECTED_IO_FAILURE));
    }
    drop(self.files.write().insert(path.clone(), bytes));
    self.writes.write().push(path);
    Ok(())
  }

  /// Record and resolve one deterministic configuration search.
  #[must_use]
  pub fn find_config_file(&self, from: &Path, names: &[&str]) -> Option<PathBuf> {
    self.discovery_bases.write().push(from.to_path_buf());
    names
      .iter()
      .map(|name| from.join(name))
      .find(|candidate| self.files.read().contains_key(candidate))
  }
}

#[cfg(test)]
mod tests {
  use std::io::Error as IoError;
  use std::io::ErrorKind;
  use std::path::Path;
  use std::path::PathBuf;

  use futures::future::join;
  use strict_test_support::ComparisonFailure;
  use strict_test_support::PredicateFailure;
  use strict_test_support::ResultFailure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_that;
  use time::Duration;
  use time::OffsetDateTime;
  use tokio::io::AsyncReadExt as _;
  use tokio::io::AsyncWriteExt as _;

  use super::TestEnvironment;
  use super::drive;
  use super::ensure_result;

  /// Clock observations before absence, during absence, and after recovery.
  type ClockObservations = [Option<OffsetDateTime>; 3];

  /// Complete configuration lookup, recorded search bases, and executor output.
  type DiscoveryObservations = (Option<PathBuf>, Vec<PathBuf>, u8);

  /// Native byte extraction result, including its original I/O failure.
  type ExtractedBytes = Result<Vec<u8>, ResultFailure<IoError>>;

  /// Both native outcomes of the result extraction contract.
  #[derive(Debug)]
  struct ResultExtractions {
    /// Bytes returned by a successful operation.
    accepted: ExtractedBytes,
    /// Original error returned by an unsuccessful operation.
    rejected: ExtractedBytes,
  }

  /// Complete read result and its destination buffer, including partial bytes on failure.
  #[derive(Debug)]
  struct ReadObservation<Buffer> {
    /// Native byte count or read error.
    count: Result<usize, IoError>,
    /// Original destination buffer after the read.
    bytes: Buffer,
  }

  /// Memory-file observations across replacement and injected failures.
  #[derive(Debug)]
  struct MemoryIoObservations {
    /// Original seeded bytes or their native read failure.
    initial:        Result<Vec<u8>, IoError>,
    /// Native result of replacing the seeded file.
    replacement:    Result<(), IoError>,
    /// Bytes observed after replacement.
    replaced:       Result<Vec<u8>, IoError>,
    /// Successful write targets before failure injection.
    writes:         Vec<PathBuf>,
    /// Native result of the deliberately rejected read.
    rejected_read:  Result<Vec<u8>, IoError>,
    /// Native result of the deliberately rejected write.
    rejected_write: Result<(), IoError>,
    /// File contents after the rejected write.
    retained:       Result<Vec<u8>, IoError>,
    /// Successful write targets after the rejected write.
    final_writes:   Vec<PathBuf>,
  }

  /// Snapshot-channel observations, including every operation and byte buffer.
  #[derive(Debug)]
  struct StreamObservations {
    /// Read results and complete input buffers before and after exhaustion.
    reads:            [ReadObservation<String>; 2],
    /// Native results of each initial output write, in order.
    writes:           [Result<(), IoError>; 3],
    /// Captured standard output and standard error after initial writes.
    captured:         [Vec<u8>; 2],
    /// Native result of the injected standard-output failure.
    rejected_write:   Result<(), IoError>,
    /// Native result of the recovery write.
    recovered_write:  Result<(), IoError>,
    /// Captured output after rejecting one write and accepting the recovery.
    recovered_output: Vec<u8>,
    /// Native result of independent standard-error failure injection.
    rejected_flush:   Result<(), IoError>,
    /// Both captured channels after clearing their contents.
    cleared:          [Vec<u8>; 2],
  }

  /// Interactive-channel observations retaining both sides of every joined operation.
  #[derive(Debug)]
  struct InteractiveObservations {
    /// Native input read result and complete received bytes.
    input:           (Result<usize, IoError>, Vec<u8>),
    /// Write, flush, shutdown, and post-shutdown write results in order.
    input_writes:    [Result<(), IoError>; 4],
    /// Complete captured standard-output bytes and native read result.
    standard:        ([u8; 12], Result<usize, IoError>),
    /// Complete captured standard-error bytes and native read result.
    diagnostic:      ([u8; 10], Result<usize, IoError>),
    /// Native results of the three connected output writes.
    output_writes:   [Result<(), IoError>; 3],
    /// Complete replayed standard-error bytes and native read result.
    replayed:        ([u8; 10], Result<usize, IoError>),
    /// Native result of the injected output failure.
    rejected_write:  Result<(), IoError>,
    /// Captured output after the rejected write.
    retained_output: Vec<u8>,
  }

  #[test]
  fn result_extraction_preserves_success_and_native_failure() -> Result<(), Box<PredicateFailure<ResultExtractions>>> {
    let observations = ResultExtractions {
      accepted: ensure_result(Ok::<_, IoError>(vec![0, 159, 255]), "extract the complete byte buffer"),
      rejected: ensure_result(
        Err::<Vec<u8>, _>(IoError::from_raw_os_error(5)),
        "retain the original operating-system failure",
      ),
    };
    ensure_that(
      observations,
      "result extraction must retain bytes, context, and the native error",
      |observed| {
        observed.accepted.as_ref().is_ok_and(|bytes| bytes == &[0, 159, 255])
          && observed.rejected.as_ref().is_err_and(|failure| {
            failure.context == "retain the original operating-system failure" && failure.source.raw_os_error() == Some(5)
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn clock_absence_preserves_time_for_recovery() -> Result<(), Box<ComparisonFailure<ClockObservations, ClockObservations>>> {
    let environment = TestEnvironment::default();
    let replacement = OffsetDateTime::UNIX_EPOCH.saturating_add(Duration::hours(2));
    environment.set_now(replacement);
    let configured = environment.now();
    environment.set_clock_available(false);
    let unavailable = environment.now();
    environment.set_clock_available(true);
    ensure_eq(
      [configured, unavailable, environment.now()],
      [Some(replacement), None, Some(replacement)],
      "the clock must expose its configured time, report absence, and recover the same time",
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn memory_io_overwrites_and_injects_failures() -> Result<(), Box<PredicateFailure<MemoryIoObservations>>> {
    let environment = TestEnvironment::default();
    let path = Path::new("/workspace/value.txt");
    environment.insert_file(path, b"first".to_vec());
    let initial = environment.read_file(path);
    let replacement = environment.write_file(path.to_path_buf(), b"second".to_vec());
    let replaced = environment.read_file(path);
    let writes = environment.writes();
    environment.set_read_failure(true);
    let rejected_read = environment.read_file(path);
    environment.set_read_failure(false);
    environment.set_write_failure(true);
    let rejected_write = environment.write_file(path.to_path_buf(), b"ignored".to_vec());
    ensure_that(
      MemoryIoObservations {
        initial,
        replacement,
        replaced,
        writes,
        rejected_read,
        rejected_write,
        retained: environment.read_file(path),
        final_writes: environment.writes(),
      },
      "memory I/O must replace and record successful writes while preserving native failures and rejected-write state",
      |observed| {
        observed.initial.as_ref().is_ok_and(|bytes| bytes == b"first")
          && observed.replacement.is_ok()
          && observed.replaced.as_ref().is_ok_and(|bytes| bytes == b"second")
          && observed.writes == [path.to_path_buf()]
          && observed
            .rejected_read
            .as_ref()
            .is_err_and(|error| error.kind() == ErrorKind::PermissionDenied)
          && observed
            .rejected_write
            .as_ref()
            .is_err_and(|error| error.kind() == ErrorKind::PermissionDenied)
          && observed.retained.as_ref().is_ok_and(|bytes| bytes == b"second")
          && observed.final_writes == observed.writes
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn standard_streams_capture_order_and_recover_from_failures() -> Result<(), Box<PredicateFailure<StreamObservations>>> {
    let environment = TestEnvironment::default();
    environment.set_stdin(b"input".to_vec());

    let mut input = environment.stdin_reader();
    let mut source = String::new();
    let first_read = ReadObservation {
      count: drive(input.read_to_string(&mut source)),
      bytes: source.clone(),
    };
    let exhausted_read = ReadObservation {
      count: drive(input.read_to_string(&mut source)),
      bytes: source,
    };
    let mut stdout = environment.stdout_writer();
    let mut stderr = environment.stderr_writer();
    let writes = drive(async {
      [
        stdout.write_all(b"first").await,
        stdout.write_all(b"-second").await,
        stderr.write_all(b"diagnostic").await,
      ]
    });
    let captured = [environment.stdout(), environment.stderr()];
    environment.set_stdout_failure(true);
    let rejected_write = drive(stdout.write_all(b"rejected"));
    environment.set_stdout_failure(false);
    let recovered_write = drive(stdout.write_all(b"-recovered"));
    let recovered_output = environment.stdout();
    environment.set_stderr_failure(true);
    let rejected_flush = drive(stderr.flush());
    environment.clear_output();
    ensure_that(
      StreamObservations {
        reads: [first_read, exhausted_read],
        writes,
        captured,
        rejected_write,
        recovered_write,
        recovered_output,
        rejected_flush,
        cleared: [environment.stdout(), environment.stderr()],
      },
      "snapshot streams must preserve input exhaustion, write order, independent failures, recovery, and clearing",
      |observed| {
        let [ref first, ref exhausted] = observed.reads;
        let [ref standard, ref diagnostic] = observed.captured;
        first.count.as_ref().is_ok_and(|count| *count == 5)
          && first.bytes == "input"
          && exhausted.count.as_ref().is_ok_and(|count| *count == 0)
          && exhausted.bytes == "input"
          && observed.writes.iter().all(Result::is_ok)
          && standard == b"first-second"
          && diagnostic == b"diagnostic"
          && observed
            .rejected_write
            .as_ref()
            .is_err_and(|error| error.kind() == ErrorKind::PermissionDenied)
          && observed.recovered_write.is_ok()
          && observed.recovered_output == b"first-second-recovered"
          && observed
            .rejected_flush
            .as_ref()
            .is_err_and(|error| error.kind() == ErrorKind::PermissionDenied)
          && observed.cleared.iter().all(Vec::is_empty)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn interactive_standard_streams_wait_and_preserve_channel_boundaries() -> Result<(), Box<PredicateFailure<InteractiveObservations>>> {
    let environment = TestEnvironment::default();
    let mut input_writer = environment.interactive_stdin();
    let mut input_reader = environment.stdin_reader();
    let (input_result, writer_result) = drive(join(
      async {
        let mut bytes = Vec::new();
        (input_reader.read_to_end(&mut bytes).await, bytes)
      },
      async {
        [
          input_writer.write_all(b"later").await,
          input_writer.flush().await,
          input_writer.shutdown().await,
          input_writer.write_all(b"rejected").await,
        ]
      },
    ));
    let mut stdout = environment.stdout_writer();
    let mut stderr = environment.stderr_writer();
    let mut stdout_reader = environment.stdout_reader();
    let mut stderr_reader = environment.stderr_reader();
    let (output_result, capture_result) = drive(join(
      async {
        let mut standard = [0_u8; 12];
        let mut diagnostic = [0_u8; 10];
        let (standard_result, diagnostic_result) =
          join(stdout_reader.read_exact(&mut standard), stderr_reader.read_exact(&mut diagnostic)).await;
        ((standard, standard_result), (diagnostic, diagnostic_result))
      },
      async {
        [
          stdout.write_all(b"first").await,
          stdout.write_all(b"-second").await,
          stderr.write_all(b"diagnostic").await,
        ]
      },
    ));
    let mut replay = environment.stderr_reader();
    let mut replayed = [0_u8; 10];
    let replayed_count = drive(replay.read_exact(&mut replayed));
    environment.set_stdout_failure(true);
    let rejected_write = drive(stdout.write_all(b"ignored"));
    let (standard, diagnostic) = output_result;
    ensure_that(
      InteractiveObservations {
        input: input_result,
        input_writes: writer_result,
        standard,
        diagnostic,
        output_writes: capture_result,
        replayed: (replayed, replayed_count),
        rejected_write,
        retained_output: environment.stdout(),
      },
      "interactive streams must retain later input, shutdown failures, channel ordering, replay, and rejected-write state",
      |observed| {
        let (ref input_count, ref input_bytes) = observed.input;
        let [ref written, ref flushed, ref closed, ref post_shutdown] = observed.input_writes;
        let (ref standard_bytes, ref standard_count) = observed.standard;
        let (ref diagnostic_bytes, ref diagnostic_count) = observed.diagnostic;
        let (ref replayed_bytes, ref replay_count) = observed.replayed;
        input_count.as_ref().is_ok_and(|count| *count == 5)
          && input_bytes == b"later"
          && written.is_ok()
          && flushed.is_ok()
          && closed.is_ok()
          && post_shutdown.as_ref().is_err_and(|error| error.kind() == ErrorKind::BrokenPipe)
          && observed.output_writes.iter().all(Result::is_ok)
          && standard_count.as_ref().is_ok_and(|count| *count == standard_bytes.len())
          && diagnostic_count.as_ref().is_ok_and(|count| *count == diagnostic_bytes.len())
          && standard_bytes == b"first-second"
          && diagnostic_bytes == b"diagnostic"
          && replay_count.as_ref().is_ok_and(|count| *count == replayed_bytes.len())
          && replayed_bytes == diagnostic_bytes
          && observed
            .rejected_write
            .as_ref()
            .is_err_and(|error| error.kind() == ErrorKind::PermissionDenied)
          && observed.retained_output == *standard_bytes
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn discovery_records_bases_and_selects_supported_names()
  -> Result<(), Box<ComparisonFailure<DiscoveryObservations, DiscoveryObservations>>> {
    let environment = TestEnvironment::default();
    environment.insert_file("/workspace/taplo.toml", Vec::new());
    let discovered = environment.find_config_file(Path::new("/workspace"), &[".taplo.toml", "taplo.toml"]);
    ensure_eq(
      (discovered, environment.discovery_bases(), drive(async { 7_u8 })),
      (
        Some(PathBuf::from("/workspace/taplo.toml")),
        vec![PathBuf::from("/workspace")],
        7_u8,
      ),
      "discovery must select a supported name, record its base, and preserve the driven future's output",
    )
    .map(drop)
    .map_err(Box::new)
  }
}
