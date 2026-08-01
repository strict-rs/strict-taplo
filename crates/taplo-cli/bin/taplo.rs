//! Native Taplo CLI process boundary.

use std::io;
use std::io::Write;
use std::process::ExitCode;

use clap::Parser as _;
use taplo_cli::CliError;
use taplo_cli::LocalCommandFuture;
use taplo_cli::Taplo;
use taplo_cli::args::Colors;
use taplo_cli::args::TaploArgs;
use taplo_common::environment::native::NativeEnvironment;
use taplo_common::log::setup_stderr_logging;
use tracing::Instrument as _;

#[tokio::main]
async fn main() -> ExitCode {
  match run().await {
    Ok(()) => ExitCode::SUCCESS,
    Err(error) => {
      if let Err(render_error) = write_cli_error(&mut io::stderr().lock(), &error) {
        tracing::error!(%render_error, "failed to render CLI error");
      }
      ExitCode::FAILURE
    }
  }
}

/// Render one typed CLI failure at the native binary boundary.
fn write_cli_error(writer: &mut impl Write, error: &CliError) -> io::Result<()> {
  writer.write_all(b"error: ")?;
  writer.write_all(error.to_string().as_bytes())?;
  writer.write_all(b"\n")
}

/// Parse, initialize, and execute one native CLI invocation.
fn run() -> LocalCommandFuture<'static, Result<(), CliError>> {
  Box::pin(async {
    let cli = TaploArgs::parse();
    let environment = NativeEnvironment::new()?;
    setup_stderr_logging(&environment, cli.log_spans, cli.verbose, match cli.colors {
      Colors::Auto => None,
      Colors::Always => Some(true),
      Colors::Never => Some(false),
    })?;

    Taplo::new(environment)?
      .execute(cli)
      .instrument(tracing::info_span!("taplo"))
      .await
  })
}

#[cfg(test)]
mod tests {
  use std::io;
  use std::io::Write;

  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use taplo_cli::CliError;
  use taplo_cli::CliFailure;

  use super::write_cli_error;

  /// Writer that rejects every byte sequence.
  struct RejectingWriter;

  impl Write for RejectingWriter {
    fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
      Err(io::ErrorKind::BrokenPipe.into())
    }

    fn flush(&mut self) -> io::Result<()> {
      Ok(())
    }
  }

  #[test]
  fn typed_cli_errors_render_with_a_stable_boundary_prefix() -> Result<(), TestFailure> {
    let mut output = Vec::new();
    let error = CliError::from(CliFailure::NoQueryMatches);
    write_cli_error(&mut output, &error).map_err(|source| TestFailure::WasErr {
      context: "the in-memory CLI writer must accept the rendered error",
      cause:   source.to_string(),
    })?;
    ensure(
      output == b"error: the query matched no values\n",
      "the binary boundary must preserve the typed error and stable prefix",
    )
  }

  #[test]
  fn cli_error_rendering_propagates_writer_failure() -> Result<(), TestFailure> {
    let error = CliError::from(CliFailure::NoQueryMatches);
    ensure(
      write_cli_error(&mut RejectingWriter, &error).is_err(),
      "the binary boundary must not hide an output failure",
    )
  }
}
