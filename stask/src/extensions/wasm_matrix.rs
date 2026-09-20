//! Repository-specific `taplo-wasm` feature-matrix compilation.

use strict_standard::Workspace;
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
pub(super) fn run(context: &CommandContext<impl Workspace>) -> template_core::Result<()> {
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
  use strict_test_support::PredicateFailure;
  use strict_test_support::ensure_that;

  use super::FeatureSet;
  use super::run;
  use crate::extensions::test_support::Execution;
  use crate::extensions::test_support::ExtensionTestFailure;
  use crate::extensions::test_support::recording_context;

  /// Pin the complete feature-matrix command contract.
  #[test]
  fn plans_every_wasm_feature_configuration() -> Result<(), PredicateFailure<Vec<Vec<String>>>> {
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
    ensure_that(planned, "the WASM matrix must retain every required feature polarity", |observed| {
      *observed == expected
    })
    .map(drop)
  }

  #[test]
  fn executes_every_wasm_configuration_and_stops_at_the_first_failure() -> Result<(), ExtensionTestFailure<(Execution, Execution)>> {
    let (context, effects) = recording_context(&[0, 0, 0, 0])?;
    let result = run(&context);
    let (failing_context, failing_effects) = recording_context(&[0, 19])?;
    let failed_result = run(&failing_context);
    ensure_that(
      ((result, effects), (failed_result, failing_effects)),
      "the matrix must run every Cargo configuration on success and stop at a failed CLI configuration before LSP and default checks",
      |observed| {
        let completed_events = observed.0.1.events();
        let failed_events = observed.1.1.events();
        observed.0.0.is_ok()
          && completed_events.len() == FeatureSet::ALL.len()
          && completed_events
            .iter()
            .all(|event| matches!(event, EffectEvent::Process(request) if request.program == "cargo"))
          && observed.1.0.is_err()
          && failed_events.len() == 2
          && failed_events.get(1).is_some_and(
            |event| matches!(event, EffectEvent::Process(request) if request.arguments.iter().any(|argument| argument == "cli")),
          )
      },
    )
    .map(drop)
    .map_err(Into::into)
  }
}
