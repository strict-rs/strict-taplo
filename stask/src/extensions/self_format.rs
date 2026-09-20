//! Taplo formatter dogfood gate.

use strict_standard::Workspace;
use template_core::cli::context::CommandContext;
use template_core::sys::process::ToolColor;

use super::command;

/// Check every repository TOML file with the worktree Taplo binary.
///
/// # Errors
///
/// Returns a typed process failure when the formatter cannot run or reports a
/// changed file.
#[allow(
  clippy::single_call_fn,
  reason = "the named handler isolates the Taplo dogfood gate from extension registry dispatch"
)]
pub(super) fn run(context: &CommandContext<impl Workspace>) -> template_core::Result<()> {
  command::run(
    context,
    "cargo",
    &command::arguments(["run", "--package", "taplo-cli", "--bin", "taplo", "--", "fmt", "--check"]),
    ToolColor::CargoGlobal,
    None,
  )
}

#[cfg(test)]
mod tests {
  use strict_test_support::EffectEvent;
  use strict_test_support::ensure_that;
  use template_core::sys::process::ToolColor;

  use super::run;
  use crate::extensions::command;
  use crate::extensions::test_support::Execution;
  use crate::extensions::test_support::ExtensionTestFailure;
  use crate::extensions::test_support::recording_context;

  #[test]
  fn self_format_runs_the_worktree_binary_with_the_complete_check_contract() -> Result<(), ExtensionTestFailure<Execution>> {
    let (context, effects) = recording_context(&[0])?;
    let result = run(&context);
    let arguments = command::arguments(["run", "--package", "taplo-cli", "--bin", "taplo", "--", "fmt", "--check"]);
    let expected = context.process_request(
      "cargo",
      &arguments,
      ToolColor::CargoGlobal,
      <_ as Default>::default(),
      <_ as Default>::default(),
      &[],
    );
    ensure_that(
      (result, effects),
      "self-format must succeed through the worktree Taplo binary with check mode and no alternate directory",
      |observed| observed.0.is_ok() && observed.1.events() == [EffectEvent::Process(expected)],
    )
    .map(drop)
    .map_err(Into::into)
  }

  #[test]
  fn self_format_propagates_formatter_rejection() -> Result<(), ExtensionTestFailure<Execution>> {
    let (context, effects) = recording_context(&[1])?;
    let result = run(&context);
    ensure_that(
      (result, effects),
      "a formatter rejection must fail the extension after exactly one child request",
      |observed| observed.0.is_err() && observed.1.events().len() == 1,
    )
    .map(drop)
    .map_err(Into::into)
  }
}
