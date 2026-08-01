//! Consumer-owned repository extension composition.
//!
//! Standard repository workflows execute through the installed `template`
//! binary. This crate compiles only the guarded local `x` registry.

use std::process::ExitCode;

pub mod extensions;

/// Run the guarded repository-specific extension surface.
#[must_use]
pub fn run() -> ExitCode {
  template_stask::run_with_extensions(extensions::commands())
}

#[cfg(test)]
/// Contract tests for the consumer-owned extension façade.
mod tests {
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_ok;
  use template_core::cli::command::CommandSurface;

  use super::extensions;

  /// Keep the compiled local runner limited to the guarded `x` command group.
  #[test]
  fn extension_registry_exposes_only_the_local_x_router() -> Result<(), TestFailure> {
    let command_set = ensure_ok(extensions::commands(), "the local extension registry must build")?;
    let descriptors = command_set.descriptors();
    ensure(
      (
        descriptors.len(),
        descriptors.first().map(|descriptor| (descriptor.name(), descriptor.surface())),
      ) == (1, Some(("x", CommandSurface::StaskExtension))),
      "the consumer runner must expose only the local x extension surface",
    )
  }
}
