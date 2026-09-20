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
mod tests {
  use strict_test_support::PredicateFailure;
  use strict_test_support::ensure_that;
  use template_core::cli::command::CommandSurface;
  use template_stask::ExtensionCommandSet;

  use super::extensions;

  /// Complete extension registration retaining repository-owned command failures.
  type Registration = template_stask::Result<ExtensionCommandSet<(), extensions::StaskError>>;

  #[test]
  fn extension_registry_exposes_only_the_local_x_router() -> Result<(), PredicateFailure<Registration>> {
    ensure_that(
      extensions::commands(),
      "the local extension registry must build and expose only the local x extension surface",
      |registration| {
        registration.as_ref().is_ok_and(|command_set| {
          let descriptors = command_set.descriptors();
          descriptors.len() == 1
            && descriptors
              .first()
              .is_some_and(|descriptor| descriptor.name() == "x" && descriptor.surface() == CommandSurface::StaskExtension)
        })
      },
    )
    .map(drop)
  }
}
