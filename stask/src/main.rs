//! Guarded repository-specific `x` extension entrypoint.

use std::process::ExitCode;

/// Run the local extension-only runner.
fn main() -> ExitCode {
  stask::run()
}
