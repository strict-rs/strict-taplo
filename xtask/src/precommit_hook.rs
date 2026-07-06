//! Git pre-commit hook entry point.

use std::process::ExitCode;

/// Run the composed self-refreshing pre-commit hook.
fn main() -> ExitCode {
  xtask::run_precommit_hook()
}
