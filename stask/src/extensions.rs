//! Consumer-owned extension registry for `just x <name>` commands.

/// Child-process planning shared by repository-specific extension handlers.
mod command;
/// Typed failures introduced by this repository's extension layer.
mod error;
/// Immutable JavaScript installation and build extension.
mod javascript;
/// Repository self-formatting extension.
mod self_format;
#[cfg(test)]
/// Deterministic effect recorder shared by extension behavior tests.
mod test_support;
/// Checksum-pinned TOML conformance extension.
mod toml_conformance;
/// Supported `taplo-wasm` feature-matrix extension.
mod wasm_matrix;

use bpaf::Parser as _;
use bpaf::pure;
use strict_standard::Workspace;
use template_core::cli::context::CommandContext;
use template_stask::ExtensionCommandSet;

pub use self::error::StaskError;

/// Every repository-specific extension command.
#[derive(Clone, Copy, Debug)]
enum ProjectCommand {
  /// Run the pinned TOML 1.1 conformance suite.
  TomlConformance,
  /// Compile every supported WASM feature configuration.
  WasmMatrix,
  /// Build every JavaScript workspace.
  JavaScriptBuild,
  /// Check this repository with its own formatter.
  TaploSelfFormat,
}

/// Dispatch one parsed repository command through the shared command context.
#[allow(
  clippy::single_call_fn,
  reason = "the registry callback keeps the ProjectCommand dispatch match explicit and exhaustive"
)]
fn execute(command: ProjectCommand, context: &CommandContext<impl Workspace>) -> Result<(), StaskError> {
  match command {
    ProjectCommand::TomlConformance => toml_conformance::run(context),
    ProjectCommand::WasmMatrix => wasm_matrix::run(context).map_err(Into::into),
    ProjectCommand::JavaScriptBuild => javascript::run(context).map_err(Into::into),
    ProjectCommand::TaploSelfFormat => self_format::run(context).map_err(Into::into),
  }
}

/// Build this repository's guarded extension registry.
///
/// # Errors
///
/// Returns a typed registration error if any command metadata is invalid or
/// any nested command name is duplicated.
#[allow(
  clippy::single_call_fn,
  reason = "the public constructor is the crate facade and contract-test seam for extension composition"
)]
pub fn commands() -> template_stask::Result<ExtensionCommandSet<(), StaskError>> {
  let toml_conformance = template_stask::extension_command(
    "toml-conformance",
    "Run the pinned TOML 1.1 conformance suite without skips",
    pure(()).to_options(),
    |()| ProjectCommand::TomlConformance,
  )?;
  let wasm_matrix = template_stask::extension_command(
    "wasm-matrix",
    "Compile the no-default, CLI, LSP, and combined WASM feature sets",
    pure(()).to_options(),
    |()| ProjectCommand::WasmMatrix,
  )?;
  let javascript_build = template_stask::extension_command(
    "js-build",
    "Install and build every JavaScript workspace from the committed lockfile",
    pure(()).to_options(),
    |()| ProjectCommand::JavaScriptBuild,
  )?;
  let taplo_self_format = template_stask::extension_command(
    "taplo-self-format",
    "Check repository TOML with the worktree Taplo formatter",
    pure(()).to_options(),
    |()| ProjectCommand::TaploSelfFormat,
  )?;

  template_stask::registry(
    "strict-taplo extensions",
    vec![toml_conformance, wasm_matrix, javascript_build, taplo_self_format],
    execute,
  )
}

#[cfg(test)]
mod tests {
  use strict_test_support::ensure_that;

  use super::ProjectCommand;
  use super::StaskError;
  use super::execute;
  use super::test_support::Execution;
  use super::test_support::ExtensionTestFailure;
  use super::test_support::recording_context;

  /// Successful extension-dispatch expectation.
  struct SuccessfulCase {
    /// Extension variant routed through the registry.
    command:            ProjectCommand,
    /// Child-process statuses supplied to its deterministic effects.
    statuses:           &'static [i32],
    /// Complete number of process requests expected from the handler.
    expected_processes: usize,
  }

  #[test]
  fn project_dispatcher_routes_every_extension_and_preserves_fail_fast_errors()
  -> Result<(), ExtensionTestFailure<Execution<(), StaskError>>> {
    let successful_cases = [
      SuccessfulCase {
        command:            ProjectCommand::TomlConformance,
        statuses:           &[0, 0, 0, 0, 0, 0],
        expected_processes: 6,
      },
      SuccessfulCase {
        command:            ProjectCommand::WasmMatrix,
        statuses:           &[0, 0, 0, 0],
        expected_processes: 4,
      },
      SuccessfulCase {
        command:            ProjectCommand::JavaScriptBuild,
        statuses:           &[0, 0, 0],
        expected_processes: 3,
      },
      SuccessfulCase {
        command:            ProjectCommand::TaploSelfFormat,
        statuses:           &[0],
        expected_processes: 1,
      },
    ];
    for SuccessfulCase {
      command,
      statuses,
      expected_processes,
    } in successful_cases
    {
      let (context, effects) = recording_context(statuses)?;
      let result = execute(command, &context);
      drop(ensure_that(
        (result, effects),
        "each repository extension variant must succeed with its complete child-command sequence",
        |observed| observed.0.is_ok() && observed.1.process_requests().len() == expected_processes,
      )?);
    }

    for command in [
      ProjectCommand::TomlConformance,
      ProjectCommand::WasmMatrix,
      ProjectCommand::JavaScriptBuild,
      ProjectCommand::TaploSelfFormat,
    ] {
      let (context, effects) = recording_context(&[13])?;
      let result = execute(command, &context);
      drop(ensure_that(
        (result, effects),
        "each failed repository extension must propagate its first child-process failure and stop",
        |observed| observed.0.is_err() && observed.1.process_requests().len() == 1,
      )?);
    }
    Ok(())
  }
}
