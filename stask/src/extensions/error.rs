//! Typed failures shared by the repository-specific extension handlers.

use std::path::PathBuf;

use template_core::CoreError;
use template_core::cli::context::CommandContext;
use template_core::cli::output::plain;
use template_core::cli::runner::CommandFailure;

/// Every failure introduced by a strict-Taplo extension command.
#[derive(Debug, thiserror::Error)]
pub enum StaskError {
  /// Complete shared parser, filesystem, or process failure.
  #[error(transparent)]
  Core(Box<CoreError>),
  /// No checksum-pinned `toml-test` release artifact exists for this host.
  #[error("toml-test v2.2.0 has no configured artifact for host `{os}-{architecture}`")]
  UnsupportedTomlTestHost {
    /// Host operating-system identifier.
    os:           &'static str,
    /// Host architecture identifier.
    architecture: &'static str,
  },
  /// A tool path cannot be represented as a child-process argument.
  #[error("tool path `{path}` is not valid Unicode")]
  PathUnicode {
    /// Rejected path.
    path: PathBuf,
  },
}

impl From<CoreError> for StaskError {
  fn from(source: CoreError) -> Self {
    Self::Core(Box::new(source))
  }
}

impl CommandFailure for StaskError {
  fn render(&self, context: &CommandContext, program: &str) -> template_core::Result<()> {
    match *self {
      Self::Core(ref source) => source.render(context, program),
      Self::UnsupportedTomlTestHost {
        ..
      }
      | Self::PathUnicode {
        ..
      } => context.output().error(plain(format!("{program}: {self}"))),
    }
  }
}
