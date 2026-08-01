//! Reproducible JavaScript workspace installation and compilation.

use std::path::Path;

use template_core::cli::context::CommandContext;
use template_core::sys::process::ToolColor;

use super::command;

/// JavaScript workspace root.
const JAVASCRIPT_ROOT: &str = "js";

/// Run the committed Yarn release against the immutable lockfile and build all
/// workspaces.
///
/// # Errors
///
/// Returns a typed process failure when dependency installation or compilation
/// fails.
#[allow(
  clippy::single_call_fn,
  reason = "the named handler keeps immutable installation, workspace builds, and boundary checks together"
)]
pub(super) fn run(context: &CommandContext) -> template_core::Result<()> {
  let root = Path::new(JAVASCRIPT_ROOT);
  command::run(
    context,
    "node",
    &command::arguments([".yarn/releases/yarn-4.0.2.cjs", "install", "--immutable"]),
    ToolColor::EnvOnly,
    Some(root),
  )?;
  command::run(
    context,
    "node",
    &command::arguments([".yarn/releases/yarn-4.0.2.cjs", "build"]),
    ToolColor::EnvOnly,
    Some(root),
  )?;
  command::run(
    context,
    "node",
    &command::arguments(["tests/wasm-boundaries.cjs"]),
    ToolColor::EnvOnly,
    Some(root),
  )
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;

  use strict_test_support::EffectEvent;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_ok;
  use template_core::sys::process::ToolColor;

  use super::run;
  use crate::extensions::command;
  use crate::extensions::test_support::recording_context;

  #[test]
  fn javascript_build_executes_immutable_install_build_and_boundary_checks_in_order() -> Result<(), TestFailure> {
    let (context, effects) = recording_context(&[0, 0, 0])?;
    ensure_ok(run(&context), "the complete JavaScript extension command stream must succeed")?;
    let expected = [
      command::arguments([".yarn/releases/yarn-4.0.2.cjs", "install", "--immutable"]),
      command::arguments([".yarn/releases/yarn-4.0.2.cjs", "build"]),
      command::arguments(["tests/wasm-boundaries.cjs"]),
    ]
    .into_iter()
    .map(|arguments| {
      let mut request = context.process_request(
        "node",
        &arguments,
        ToolColor::EnvOnly,
        <_ as Default>::default(),
        <_ as Default>::default(),
        &[],
      );
      request.current_dir = Some(PathBuf::from("js"));
      EffectEvent::Process(request)
    })
    .collect::<Vec<_>>();
    ensure(
      effects.events() == expected,
      "the JavaScript extension must retain its immutable install, workspace build, and boundary-test sequence",
    )
  }

  #[test]
  fn javascript_build_stops_after_the_first_failed_phase() -> Result<(), TestFailure> {
    let (context, effects) = recording_context(&[0, 12])?;
    ensure(run(&context).is_err(), "a failed JavaScript build phase must fail the extension")?;
    let events = effects.events();
    ensure(
      (
        events.len(),
        events.get(1).map(|event| {
          matches!(
            event,
            EffectEvent::Process(request)
              if request.arguments.iter().any(|argument| argument == "build")
          )
        }),
      ) == (2, Some(true)),
      "a failed build must record install and build without running the boundary suite",
    )
  }
}
