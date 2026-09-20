//! Shared child-process orchestration for repository-specific extension handlers.

use std::path::Path;

use strict_standard::Workspace;
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
  context: &CommandContext<impl Workspace>,
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
  use strict_test_support::ensure_that;
  use template_core::sys::process::ToolColor;

  use super::arguments;
  use super::run;
  use crate::extensions::test_support::Execution;
  use crate::extensions::test_support::ExtensionTestFailure;
  use crate::extensions::test_support::recording_context;

  /// Both argument vectors and the complete child-command observation.
  type ArgumentExecution = (Vec<String>, Vec<String>, Execution);

  #[test]
  fn owned_arguments_and_child_execution_preserve_complete_request_policy() -> Result<(), ExtensionTestFailure<ArgumentExecution>> {
    let owned = arguments(["first", "second"]);
    let empty = arguments::<0>([]);
    let (context, effects) = recording_context(&[0])?;
    let result = run(&context, "fixture", &owned, ToolColor::CapturedPlain, Some(Path::new("js")));
    let mut expected = context.process_request(
      "fixture",
      &owned,
      ToolColor::CapturedPlain,
      <_ as Default>::default(),
      <_ as Default>::default(),
      &[],
    );
    expected.current_dir = Some(PathBuf::from("js"));
    ensure_that(
      (owned, empty, (result, effects)),
      "argument conversion and successful execution must preserve exact text, order, request policy, and working directory",
      |observed| {
        observed.0 == ["first", "second"].map(str::to_owned)
          && observed.1.is_empty()
          && observed.2.0.is_ok()
          && observed.2.1.events() == [EffectEvent::Process(expected)]
      },
    )
    .map(drop)
    .map_err(Into::into)
  }

  #[test]
  fn child_execution_rejects_nonzero_status_without_erasing_the_request() -> Result<(), ExtensionTestFailure<Execution>> {
    let (context, effects) = recording_context(&[17])?;
    let result = run(&context, "fixture", &arguments(["check"]), ToolColor::EnvOnly, None);
    ensure_that(
      (result, effects),
      "a rejected child must retain its native failure naming the program and record the complete request exactly once",
      |observed| observed.0.as_ref().is_err_and(|error| error.to_string().contains("fixture")) && observed.1.events().len() == 1,
    )
    .map(drop)
    .map_err(Into::into)
  }
}
