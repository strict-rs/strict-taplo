//! Local `xtask` composition crate.
//!
//! The reusable runner lives in `strict-xtask-core`; Rust/Cargo workflows live
//! in `strict-xtask-cargo`; generated-doc rendering lives in
//! `strict-xtask-agents-md`. This crate only registers those command sets plus
//! consumer-owned local extensions.

use std::process::ExitCode;

use strict_xtask_agents_md::AgentGuidanceConfig;
use strict_xtask_agents_md::AgentGuidanceSource;

pub mod extensions;

/// Run the composed `cargo xtask` command runner.
#[must_use]
pub fn run() -> ExitCode {
  strict_xtask_cargo::run_with_extensions(cargo_command_config(), extensions::commands())
}

/// Run the self-refreshing pre-commit hook command.
#[must_use]
pub fn run_precommit_hook() -> ExitCode {
  strict_xtask_cargo::run_precommit_hook_with_config(cargo_command_config())
}

/// Build the local template guidance configuration.
#[allow(
  clippy::single_call_fn,
  reason = "local guidance wiring is named so the Cargo command config boundary stays small and readable"
)]
fn agent_guidance_config() -> AgentGuidanceConfig {
  AgentGuidanceConfig::with_template_source(
    AgentGuidanceSource::new(agents_md::fragments_dir(), agents_md::FRAGMENTS_DISPLAY_ROOT),
    strict_xtask_agents_md::DEFAULT_CONSUMER_FRAGMENTS_ROOT,
  )
}

/// Build the local Cargo command configuration.
#[allow(
  clippy::single_call_fn,
  reason = "the precommit binary needs the same named Cargo config boundary as the main runner without duplicating guidance wiring"
)]
fn cargo_command_config() -> strict_xtask_cargo::CargoCommandConfig {
  strict_xtask_cargo::CargoCommandConfig::new(agent_guidance_config())
}

#[cfg(test)]
mod tests {
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;

  use super::extensions;

  #[test]
  fn extension_registry_exposes_the_local_x_router() -> Result<(), TestFailure> {
    let command_set = ensure_ok(extensions::commands(), "local extension command set must build")?;
    ensure(
      command_set.name() == "local xtask extensions",
      "local extension command set must keep its user-facing name",
    )?;

    let command = ensure_some(command_set.find("x"), "local extension command set must expose the x router")?;
    ensure(
      command.description() == "Run a consumer-registered extension command",
      "local extension router must keep its user-facing help description",
    )
  }
}
