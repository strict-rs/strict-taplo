use std::io;
use std::io::Write;
#[cfg(target_arch = "wasm32")]
use std::sync::Arc;

#[cfg(not(target_arch = "wasm32"))]
use futures::executor::block_on;
#[cfg(target_arch = "wasm32")]
use parking_lot::Mutex;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt as _;
#[cfg(target_arch = "wasm32")]
use tokio::sync::mpsc::UnboundedReceiver;
#[cfg(target_arch = "wasm32")]
use tokio::sync::mpsc::UnboundedSender;
#[cfg(target_arch = "wasm32")]
use tokio::sync::mpsc::unbounded_channel;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::fmt::layer;
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry;

use crate::environment::Environment;
use crate::environment::EnvironmentError;
#[cfg(target_arch = "wasm32")]
use crate::environment::LocalEnvironment;

/// Synchronous tracing adapter for native asynchronous standard error.
#[cfg(not(target_arch = "wasm32"))]
struct BlockingWrite<W: AsyncWrite>(W);

#[cfg(not(target_arch = "wasm32"))]
impl<W: AsyncWrite + Unpin> Write for BlockingWrite<W> {
  fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
    block_on(self.0.write(buf))
  }

  fn flush(&mut self) -> io::Result<()> {
    block_on(self.0.flush())
  }
}

/// Synchronous enqueue boundary drained by one ordered local WASM task.
#[cfg(target_arch = "wasm32")]
#[derive(Clone)]
struct QueuedWrite {
  /// Ordered byte-message sender.
  sender:  UnboundedSender<Vec<u8>>,
  /// First asynchronous stream failure category, when one occurs.
  failure: Arc<Mutex<Option<io::ErrorKind>>>,
}

#[cfg(target_arch = "wasm32")]
impl QueuedWrite {
  /// Return an asynchronous failure already observed by the drain task.
  fn observed_failure(&self) -> Option<io::Error> {
    self.failure.lock().as_ref().copied().map(io::Error::from)
  }
}

#[cfg(target_arch = "wasm32")]
impl Write for QueuedWrite {
  fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
    if let Some(error) = self.observed_failure() {
      return Err(error);
    }
    if self.sender.send(buf.to_vec()).is_err() {
      return Err(io::Error::from(io::ErrorKind::BrokenPipe));
    }
    Ok(buf.len())
  }

  fn flush(&mut self) -> io::Result<()> {
    if let Some(error) = self.observed_failure() {
      return Err(error);
    }
    if self.sender.is_closed() {
      return Err(io::Error::from(io::ErrorKind::BrokenPipe));
    }
    Ok(())
  }
}

/// Drain queued tracing bytes through one local asynchronous writer.
#[cfg(target_arch = "wasm32")]
async fn drain_stderr<W>(mut stderr: W, mut receiver: UnboundedReceiver<Vec<u8>>, failure: Arc<Mutex<Option<io::ErrorKind>>>)
where
  W: AsyncWrite + Unpin,
{
  while let Some(bytes) = receiver.recv().await {
    if let Err(error) = stderr.write_all(&bytes).await {
      *failure.lock() = Some(error.kind());
      receiver.close();
      return;
    }
  }
  if let Err(error) = stderr.flush().await {
    *failure.lock() = Some(error.kind());
  }
}

/// Install the standard Taplo stderr subscriber.
///
/// # Errors
///
/// Returns [`EnvironmentError`] when terminal or environment-variable host
/// facts cannot be read.
#[cfg(not(target_arch = "wasm32"))]
pub fn setup_stderr_logging(
  environment: &(impl Environment + Send + Sync),
  spans: bool,
  verbose: bool,
  colors: Option<bool>,
) -> Result<(), EnvironmentError> {
  let writer_environment = <_ as Clone>::clone(environment);
  install_stderr_logging(
    environment,
    move || BlockingWrite(writer_environment.stderr()),
    spans,
    verbose,
    colors,
  )
}

/// Install the standard Taplo stderr subscriber with an ordered local drain.
///
/// # Errors
///
/// Returns [`EnvironmentError`] when terminal or environment-variable host
/// facts cannot be read or the subscriber cannot be installed.
#[cfg(target_arch = "wasm32")]
pub fn setup_stderr_logging(
  environment: &impl LocalEnvironment,
  spans: bool,
  verbose: bool,
  colors: Option<bool>,
) -> Result<(), EnvironmentError> {
  let (sender, receiver) = unbounded_channel();
  let failure = Arc::new(Mutex::new(None));
  environment.spawn_local(drain_stderr(environment.stderr(), receiver, Arc::clone(&failure)))?;
  install_stderr_logging(
    environment,
    move || QueuedWrite {
      sender:  sender.clone(),
      failure: Arc::clone(&failure),
    },
    spans,
    verbose,
    colors,
  )
}

/// Install a configured subscriber using one synchronous writer factory.
#[allow(
  clippy::single_call_fn,
  reason = "subscriber installation centralizes filtering and formatting shared by native and browser stderr adapters"
)]
fn install_stderr_logging<E, W>(
  environment: &E,
  writer: W,
  spans: bool,
  verbose: bool,
  colors: Option<bool>,
) -> Result<(), EnvironmentError>
where
  E: Environment,
  W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
  let registry = registry();

  let use_colors = match colors {
    None => environment.atty_stderr()?,
    Some(configured_colors) => configured_colors,
  };

  let filtered_registry = registry.with(
    environment
      .env_var("RUST_LOG")?
      .map_or_else(|| EnvFilter::default().add_directive(tracing::Level::INFO.into()), EnvFilter::new),
  );

  let event_format = format().pretty().with_ansi(use_colors);

  let base_layer = layer().with_ansi(use_colors).with_writer(writer);

  let configured_layer = if spans {
    base_layer.with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
  } else {
    base_layer
  };

  let result = if verbose {
    filtered_registry.with(configured_layer.event_format(event_format)).try_init()
  } else {
    filtered_registry
      .with(
        configured_layer
          .event_format(
            event_format
              .compact()
              .with_source_location(false)
              .with_target(false)
              .without_time(),
          )
          .without_time()
          .with_file(false)
          .with_line_number(false),
      )
      .try_init()
  };
  result.map_err(|error| EnvironmentError::LoggingInitialization {
    message: error.to_string(),
  })
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
  use std::io::ErrorKind;
  use std::io::Write as _;

  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_contains;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;

  use super::BlockingWrite;
  use super::install_stderr_logging;
  use super::setup_stderr_logging;
  use crate::environment::EnvironmentError;
  use crate::environment::native::NativeEnvironment;
  use crate::test_support::TestEnvironment;

  #[test]
  fn blocking_writer_preserves_order_failure_kind_and_recovery() -> Result<(), TestFailure> {
    let environment = taplo_test_support::TestEnvironment::default();
    let mut writer = BlockingWrite(environment.stdout_writer());
    ensure_eq(
      &ensure_ok(writer.write(b"first"), "the blocking writer must accept the first byte segment")?,
      &5_usize,
      "the blocking writer must report the complete accepted length",
    )?;
    ensure_ok(writer.flush(), "the blocking writer must flush its asynchronous output")?;
    ensure(
      environment.stdout() == b"first",
      "the blocking writer must capture the complete first byte segment",
    )?;

    environment.set_stdout_failure(true);
    ensure(
      writer.write(b"blocked").as_ref().err().map(std::io::Error::kind) == Some(ErrorKind::PermissionDenied),
      "the blocking writer must preserve the asynchronous stream failure kind",
    )?;
    ensure(
      writer.flush().as_ref().err().map(std::io::Error::kind) == Some(ErrorKind::PermissionDenied),
      "the blocking writer must preserve flush failures independently",
    )?;

    environment.set_stdout_failure(false);
    ensure_eq(
      &ensure_ok(writer.write(b"-second"), "the blocking writer must recover after failure removal")?,
      &7_usize,
      "the recovered writer must report the complete accepted length",
    )?;
    ensure(
      environment.stdout() == b"first-second",
      "the recovered writer must append bytes in call order without partial failure output",
    )
  }

  #[test]
  fn subscriber_installation_emits_configured_events_and_rejects_reinstallation() -> Result<(), TestFailure> {
    let environment = TestEnvironment::default();
    environment.set_env_var("RUST_LOG", "taplo_common::log::tests=trace");
    let writer_environment = environment.clone();
    ensure_ok(
      install_stderr_logging(
        &environment,
        move || BlockingWrite(writer_environment.stdout_writer()),
        true,
        true,
        Some(false),
      ),
      "the first process-wide subscriber installation must succeed",
    )?;
    tracing::info!(target: "taplo_common::log::tests", "configured subscriber event");
    let output = ensure_ok(
      String::from_utf8(environment.stdout()),
      "captured tracing output must be valid UTF-8",
    )?;
    ensure_contains(
      &output,
      "configured subscriber event",
      "the configured subscriber must write an enabled event",
    )?;

    let default_environment = TestEnvironment::default();
    let repeated_writer_environment = default_environment.clone();
    ensure(
      matches!(
        install_stderr_logging(
          &default_environment,
          move || BlockingWrite(repeated_writer_environment.stderr_writer()),
          false,
          false,
          None,
        ),
        Err(EnvironmentError::LoggingInitialization {
          message
        }) if !message.is_empty()
      ),
      "a second subscriber must exercise default filtering and terminal policy before returning a typed installation failure",
    )?;

    let runtime = ensure_ok(
      tokio::runtime::Builder::new_current_thread().enable_all().build(),
      "the native logging test runtime must initialize",
    )?;
    let native_environment = NativeEnvironment::from_handle(runtime.handle().clone());
    ensure(
      matches!(
        setup_stderr_logging(&native_environment, false, false, Some(false)),
        Err(EnvironmentError::LoggingInitialization {
          message
        }) if !message.is_empty()
      ),
      "the public native logging adapter must preserve process-wide installation failures",
    )
  }
}
