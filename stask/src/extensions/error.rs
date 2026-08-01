//! Typed failures shared by the repository-specific extension handlers.

use std::path::PathBuf;

/// Every failure introduced by a strict-Taplo extension command.
#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub(super) enum StaskError {
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
