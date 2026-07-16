//! Consumer-owned extension registry for `just x <name>` commands.

use template_core::cli::command::CommandSet;

/// Build this repository's intentionally empty extension registry.
///
/// # Errors
///
/// Returns a typed registration error if the controlled `x` router metadata
/// is invalid.
pub fn commands() -> template_xtask::Result<CommandSet> {
  template_xtask::empty_registry("strict-taplo extensions")
}
