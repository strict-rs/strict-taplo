//! Consumer-owned extension registry for `just x <name>` commands.

use strict_xtask_core::CommandSet;
use strict_xtask_core::empty_extension_command_set;

/// Build the local extension command set.
///
/// # Errors
///
/// Returns an error if the top-level extension router cannot be registered.
#[allow(
  clippy::single_call_fn,
  reason = "local xtask composition consumes this command set once when building the runner"
)]
pub fn commands() -> strict_xtask_core::Result<CommandSet> {
  empty_extension_command_set(
    "local xtask extensions",
    "x",
    "Run a consumer-registered extension command",
    "no extension commands are registered (add one in xtask/src/extensions.rs)",
  )
}
