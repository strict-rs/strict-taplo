//! `cargo xtask` binary entrypoint.

use std::process::ExitCode;

/// Run the composed workspace automation runner.
fn main() -> ExitCode {
  xtask::run()
}
