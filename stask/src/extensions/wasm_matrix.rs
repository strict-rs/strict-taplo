//! Repository-specific `taplo-wasm` feature-matrix compilation.

use template_core::cli::context::CommandContext;
use template_core::sys::process::ToolColor;

use super::command;

/// Compile every supported `taplo-wasm` feature configuration.
///
/// # Errors
///
/// Returns a typed process failure when any feature configuration fails to
/// compile.
#[allow(
  clippy::single_call_fn,
  reason = "the named handler keeps the complete WASM feature matrix as one extension operation"
)]
pub(super) fn run(context: &CommandContext) -> template_core::Result<()> {
  for features in FeatureSet::ALL {
    command::run(context, "cargo", &features.arguments(), ToolColor::CargoGlobal, None)?;
  }
  Ok(())
}

/// One required `taplo-wasm` feature configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FeatureSet {
  /// No optional WASM surface.
  NoDefault,
  /// CLI bindings only.
  Cli,
  /// LSP bindings only.
  Lsp,
  /// Default combined CLI and LSP bindings.
  Default,
}

impl FeatureSet {
  /// Every feature configuration in stable execution order.
  const ALL: [Self; 4] = [Self::NoDefault, Self::Cli, Self::Lsp, Self::Default];

  /// Build the Cargo argument vector for this configuration.
  #[allow(
    clippy::single_call_fn,
    reason = "the independently tested planner keeps feature-enum policy separate from process execution"
  )]
  fn arguments(self) -> Vec<String> {
    let mut arguments = command::arguments(["check", "--target", "wasm32-unknown-unknown", "--package", "taplo-wasm"]);
    match self {
      Self::NoDefault => arguments.push("--no-default-features".to_owned()),
      Self::Cli => arguments.extend(command::arguments(["--no-default-features", "--features", "cli"])),
      Self::Lsp => arguments.extend(command::arguments(["--no-default-features", "--features", "lsp"])),
      Self::Default => {}
    }
    arguments
  }
}

#[cfg(test)]
/// Command-planning tests for the supported `taplo-wasm` feature matrix.
mod tests {
  use strict_test_support::EffectEvent;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_ok;

  use super::FeatureSet;
  use super::run;
  use crate::extensions::test_support::recording_context;

  /// Pin the complete feature-matrix command contract.
  #[test]
  fn plans_every_wasm_feature_configuration() -> Result<(), TestFailure> {
    let planned = FeatureSet::ALL.into_iter().map(FeatureSet::arguments).collect::<Vec<_>>();
    let expected = [
      vec![
        "check", "--target", "wasm32-unknown-unknown", "--package", "taplo-wasm", "--no-default-features",
      ],
      vec![
        "check", "--target", "wasm32-unknown-unknown", "--package", "taplo-wasm", "--no-default-features", "--features", "cli",
      ],
      vec![
        "check", "--target", "wasm32-unknown-unknown", "--package", "taplo-wasm", "--no-default-features", "--features", "lsp",
      ],
      vec!["check", "--target", "wasm32-unknown-unknown", "--package", "taplo-wasm"],
    ]
    .into_iter()
    .map(|arguments| arguments.into_iter().map(str::to_owned).collect::<Vec<_>>())
    .collect::<Vec<_>>();
    ensure(planned == expected, "the WASM matrix must retain every required feature polarity")
  }

  #[test]
  fn executes_every_wasm_configuration_and_stops_at_the_first_failure() -> Result<(), TestFailure> {
    let (context, effects) = recording_context(&[0, 0, 0, 0])?;
    ensure_ok(run(&context), "the WASM matrix must accept four successful feature checks")?;
    ensure(
      (
        effects.events().len(),
        effects
          .events()
          .iter()
          .all(|event| matches!(event, EffectEvent::Process(request) if request.program == "cargo")),
      ) == (FeatureSet::ALL.len(), true),
      "the WASM matrix must execute one Cargo request per declared feature configuration",
    )?;

    let (failing_context, failing_effects) = recording_context(&[0, 19])?;
    ensure(run(&failing_context).is_err(), "a failed WASM configuration must fail the matrix")?;
    let failed_events = failing_effects.events();
    ensure(
      (
        failed_events.len(),
        failed_events.get(1).map(|event| {
          matches!(
            event,
            EffectEvent::Process(request)
              if request.arguments.iter().any(|argument| argument == "cli")
          )
        }),
      ) == (2, Some(true)),
      "the matrix must stop at the failed CLI configuration before LSP and default checks",
    )
  }
}
