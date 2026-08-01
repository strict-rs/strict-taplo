//! Shared child-process orchestration for repository-specific extension handlers.

use std::path::Path;

use template_core::cli::context::CommandContext;
use template_core::sys::process::ToolColor;
use template_core::sys::process::require_success;

/// Run one required child command with an optional working directory.
///
/// # Errors
///
/// Returns a typed process failure when the command cannot execute or exits
/// unsuccessfully.
pub(super) fn run(
  context: &CommandContext,
  program: &str,
  arguments: &[String],
  color: ToolColor,
  current_dir: Option<&Path>,
) -> template_core::Result<()> {
  let mut request = context.process_request(program, arguments, color, <_ as Default>::default(), <_ as Default>::default(), &[]);
  request.current_dir = current_dir.map(Path::to_path_buf);
  let output = context.execute_process(&request)?;
  drop(require_success(&request, output)?);
  Ok(())
}

/// Convert borrowed command arguments into the owned representation required
/// by the process planner.
pub(super) fn arguments<const LENGTH: usize>(values: [&str; LENGTH]) -> Vec<String> {
  values.into_iter().map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
  use std::path::Path;
  use std::path::PathBuf;

  use strict_test_support::EffectEvent;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_contains;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use template_core::sys::process::ToolColor;

  use super::arguments;
  use super::run;
  use crate::extensions::test_support::recording_context;

  #[test]
  fn owned_arguments_and_child_execution_preserve_complete_request_policy() -> Result<(), TestFailure> {
    let owned = arguments(["first", "second"]);
    ensure(
      owned == ["first", "second"].map(str::to_owned),
      "borrowed extension arguments must retain their order and exact text",
    )?;
    ensure(arguments::<0>([]).is_empty(), "an empty extension argument list must remain empty")?;

    let (context, effects) = recording_context(&[0])?;
    ensure_ok(
      run(&context, "fixture", &owned, ToolColor::CapturedPlain, Some(Path::new("js"))),
      "a successful child process must satisfy the extension boundary",
    )?;
    let mut expected = context.process_request(
      "fixture",
      &owned,
      ToolColor::CapturedPlain,
      <_ as Default>::default(),
      <_ as Default>::default(),
      &[],
    );
    expected.current_dir = Some(PathBuf::from("js"));
    ensure(
      effects.events() == [EffectEvent::Process(expected)],
      "child execution must preserve the exact process request and working directory",
    )
  }

  #[test]
  fn child_execution_rejects_nonzero_status_without_erasing_the_request() -> Result<(), TestFailure> {
    let (context, effects) = recording_context(&[17])?;
    let result = run(&context, "fixture", &arguments(["check"]), ToolColor::EnvOnly, None);
    let message = ensure_some(
      result.err().map(|error| error.to_string()),
      "a nonzero child status must return a typed process failure",
    )?;
    ensure_contains(&message, "fixture", "the child-process failure must retain the attempted program")?;
    ensure_eq(
      &effects.events().len(),
      &1_usize,
      "a failed child process must still record its complete request exactly once",
    )
  }
}
