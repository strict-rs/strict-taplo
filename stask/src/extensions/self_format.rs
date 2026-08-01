//! Taplo formatter dogfood gate.

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
pub(super) fn run(context: &CommandContext) -> template_core::Result<()> {
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
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_ok;
  use template_core::sys::process::ToolColor;

  use super::run;
  use crate::extensions::command;
  use crate::extensions::test_support::recording_context;

  #[test]
  fn self_format_runs_the_worktree_binary_with_the_complete_check_contract() -> Result<(), TestFailure> {
    let (context, effects) = recording_context(&[0])?;
    ensure_ok(run(&context), "the self-format extension must accept a clean repository")?;
    let arguments = command::arguments(["run", "--package", "taplo-cli", "--bin", "taplo", "--", "fmt", "--check"]);
    let expected = context.process_request(
      "cargo",
      &arguments,
      ToolColor::CargoGlobal,
      <_ as Default>::default(),
      <_ as Default>::default(),
      &[],
    );
    ensure(
      effects.events() == [EffectEvent::Process(expected)],
      "self-format must execute the worktree Taplo binary with check mode and no alternate directory",
    )
  }

  #[test]
  fn self_format_propagates_formatter_rejection() -> Result<(), TestFailure> {
    let (context, effects) = recording_context(&[1])?;
    ensure(
      run(&context).is_err(),
      "a worktree formatter mismatch must fail the self-format extension",
    )?;
    ensure(
      effects.events().len() == 1,
      "a formatter rejection must execute exactly one child request",
    )
  }
}
