//! Checksum-pinned TOML 1.1 conformance orchestration.

use std::env::consts::ARCH;
use std::env::consts::OS;
use std::path::Path;
use std::path::PathBuf;

use strict_standard::Workspace;
use template_core::CoreError;
use template_core::cli::context::CommandContext;
use template_core::sys::process::ToolColor;

use super::command;
use super::error::StaskError;

/// Repository-local ignored tool cache.
const TOOL_DIRECTORY: &str = "target/stask-tools/toml-test-v2.2.0";
/// Decoder executable built by the conformance command.
const DECODER: &str = "./target/debug/taplo toml-test";

/// One checksum-pinned `toml-test` release artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Artifact {
  /// Compressed release filename.
  file_name:        &'static str,
  /// Official GitHub release URL.
  url:              &'static str,
  /// SHA-256 published by the GitHub release API.
  sha256:           &'static str,
  /// Host checksum program.
  checksum_program: &'static str,
  /// Arguments preceding the checksum manifest path.
  checksum_prefix:  &'static [&'static str],
}

/// Select the official release artifact for one host.
///
/// # Errors
///
/// Returns [`StaskError::UnsupportedTomlTestHost`] when the host has no
/// checksum-pinned `toml-test` release artifact.
#[allow(
  clippy::single_call_fn,
  reason = "the pure selector is independently tested and owns checksum-pinned host mapping"
)]
fn artifact_for_host(os: &'static str, architecture: &'static str) -> Result<Artifact, StaskError> {
  match (os, architecture) {
    ("linux", "x86_64") => Ok(Artifact {
      file_name:        "toml-test-v2.2.0-linux-amd64.gz",
      url:              "https://github.com/toml-lang/toml-test/releases/download/v2.2.0/toml-test-v2.2.0-linux-amd64.gz",
      sha256:           "08f9e0a97da1151c33debf01358a8f5ef45e2a56be201241ae5eb5c2e9323fef",
      checksum_program: "sha256sum",
      checksum_prefix:  &["--check"],
    }),
    ("linux", "aarch64") => Ok(Artifact {
      file_name:        "toml-test-v2.2.0-linux-arm64.gz",
      url:              "https://github.com/toml-lang/toml-test/releases/download/v2.2.0/toml-test-v2.2.0-linux-arm64.gz",
      sha256:           "2f2e7f3e7cdbaa252bd6a3f1480f044b25aaf7a76353eb8f229ce8befa5a82b3",
      checksum_program: "sha256sum",
      checksum_prefix:  &["--check"],
    }),
    ("macos", "x86_64") => Ok(Artifact {
      file_name:        "toml-test-v2.2.0-darwin-amd64.gz",
      url:              "https://github.com/toml-lang/toml-test/releases/download/v2.2.0/toml-test-v2.2.0-darwin-amd64.gz",
      sha256:           "17e0365948ab7da54e0541bf22dce7dc809e407cd1e14bf78576cfbfd48ffee1",
      checksum_program: "shasum",
      checksum_prefix:  &["-a", "256", "--check"],
    }),
    ("macos", "aarch64") => Ok(Artifact {
      file_name:        "toml-test-v2.2.0-darwin-arm64.gz",
      url:              "https://github.com/toml-lang/toml-test/releases/download/v2.2.0/toml-test-v2.2.0-darwin-arm64.gz",
      sha256:           "f36b1310b03a95dfa6b92ef535018db8ccc997ba20e79f3fd28d0f97c9174f35",
      checksum_program: "shasum",
      checksum_prefix:  &["-a", "256", "--check"],
    }),
    _unsupported => Err(StaskError::UnsupportedTomlTestHost {
      os,
      architecture,
    }),
  }
}

/// Convert one path to the owned child-process argument representation.
///
/// # Errors
///
/// Returns [`StaskError::PathUnicode`] when `path` cannot be
/// represented as UTF-8.
fn path_argument(path: &Path) -> Result<String, StaskError> {
  path.to_str().map(str::to_owned).ok_or_else(|| StaskError::PathUnicode {
    path: path.to_owned()
  })
}

/// Download, verify, decompress, and mark the configured release artifact executable.
///
/// # Errors
///
/// Returns a typed filesystem or process failure when any provisioning step
/// fails through the injected command context.
#[allow(
  clippy::single_call_fn,
  reason = "the named provisioning step separates verified runner acquisition from conformance execution"
)]
fn prepare_runner(context: &CommandContext<impl Workspace>, artifact: &Artifact) -> Result<PathBuf, StaskError> {
  let tool_directory = PathBuf::from(TOOL_DIRECTORY);
  drop(context.file_system().create_dir_all(&tool_directory).map_err(CoreError::from)?);

  let compressed_path = tool_directory.join(artifact.file_name);
  let executable_path = compressed_path.with_extension("");
  let checksum_path = compressed_path.with_extension("sha256");
  let checksum = format!("{}  {}\n", artifact.sha256, compressed_path.display());
  drop(
    context
      .file_system()
      .write_bytes(&checksum_path, checksum.as_bytes())
      .map_err(CoreError::from)?,
  );

  command::run(
    context,
    "curl",
    &[
      "--fail".to_owned(),
      "--location".to_owned(),
      "--silent".to_owned(),
      "--show-error".to_owned(),
      "--output".to_owned(),
      path_argument(&compressed_path)?,
      artifact.url.to_owned(),
    ],
    ToolColor::EnvOnly,
    None,
  )?;

  let mut checksum_arguments = artifact
    .checksum_prefix
    .iter()
    .map(|argument| (*argument).to_owned())
    .collect::<Vec<_>>();
  checksum_arguments.push(path_argument(&checksum_path)?);
  command::run(
    context,
    artifact.checksum_program,
    &checksum_arguments,
    ToolColor::CapturedPlain,
    None,
  )?;
  command::run(
    context,
    "gzip",
    &[
      "--decompress".to_owned(),
      "--keep".to_owned(),
      "--force".to_owned(),
      path_argument(&compressed_path)?,
    ],
    ToolColor::EnvOnly,
    None,
  )?;
  command::run(
    context,
    "chmod",
    &["+x".to_owned(), path_argument(&executable_path)?],
    ToolColor::EnvOnly,
    None,
  )?;
  Ok(executable_path)
}

/// Run the complete TOML 1.1 decoder conformance suite with no skip list.
///
/// # Errors
///
/// Returns a typed host-selection, filesystem, download, checksum, build, or
/// conformance-runner failure.
#[allow(
  clippy::single_call_fn,
  reason = "the named handler keeps checksum-pinned conformance orchestration behind its extension boundary"
)]
pub(super) fn run(context: &CommandContext<impl Workspace>) -> Result<(), StaskError> {
  let artifact = artifact_for_host(OS, ARCH)?;
  let runner_path = prepare_runner(context, &artifact)?;
  let runner_argument = path_argument(&runner_path)?;
  command::run(
    context,
    "cargo",
    &command::arguments([
      "build", "--bin", "taplo", "--no-default-features", "--features", "rustls-tls,toml-test",
    ]),
    ToolColor::CargoGlobal,
    None,
  )?;
  command::run(
    context,
    &runner_argument,
    &[
      "test".to_owned(),
      "-toml".to_owned(),
      "1.1".to_owned(),
      "-decoder".to_owned(),
      DECODER.to_owned(),
    ],
    ToolColor::EnvOnly,
    None,
  )
  .map_err(Into::into)
}

#[cfg(test)]
/// Host-selection contract tests for checksum-pinned conformance artifacts.
mod tests {
  use std::env::consts::ARCH;
  use std::env::consts::OS;
  use std::ffi::OsString;
  use std::fmt::Debug;
  use std::path::Path;
  use std::path::PathBuf;

  use strict_test_support::EffectEvent;
  use strict_test_support::PredicateFailure;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_that;

  use super::Artifact;
  use super::StaskError;
  use super::artifact_for_host;
  use super::path_argument;
  use super::prepare_runner;
  use super::run;
  use crate::extensions::test_support::Execution;
  use crate::extensions::test_support::ExtensionTestFailure;
  use crate::extensions::test_support::recording_context;

  /// Complete host artifact selection, including native rejection details.
  type ArtifactSelection = Result<Artifact, StaskError>;
  /// Provisioned executable or native failure alongside every recorded effect.
  type Provisioning = Execution<PathBuf, StaskError>;
  /// Complete conformance result alongside every recorded effect.
  type Conformance = Execution<(), StaskError>;

  /// Pin the CI runner artifact and its official checksum.
  #[test]
  fn selects_checksum_pinned_linux_runner() -> Result<(), PredicateFailure<ArtifactSelection>> {
    ensure_that(
      artifact_for_host("linux", "x86_64"),
      "the CI host must select the release-pinned Linux runner checksum",
      |observed| {
        observed
          .as_ref()
          .is_ok_and(|artifact| artifact.sha256 == "08f9e0a97da1151c33debf01358a8f5ef45e2a56be201241ae5eb5c2e9323fef")
      },
    )
    .map(drop)
  }

  /// Reject hosts that lack an explicitly checksum-pinned artifact.
  #[test]
  fn rejects_unconfigured_toml_test_host() -> Result<(), PredicateFailure<ArtifactSelection>> {
    ensure_that(
      artifact_for_host("unknown", "unknown"),
      "unknown hosts must fail instead of downloading an unverified artifact",
      |observed| {
        matches!(
          observed,
          &Err(StaskError::UnsupportedTomlTestHost {
            os:           "unknown",
            architecture: "unknown",
          })
        )
      },
    )
    .map(drop)
  }

  #[test]
  fn provisions_the_pinned_runner_with_checksum_and_command_order() -> Result<(), ExtensionTestFailure<Provisioning>> {
    let artifact = ensure_ok(
      artifact_for_host("linux", "x86_64"),
      "the deterministic provisioning fixture must select the Linux artifact",
    )?;
    let (context, effects) = recording_context(&[0, 0, 0, 0])?;
    let result = prepare_runner(&context, &artifact);
    let compressed = PathBuf::from("target/stask-tools/toml-test-v2.2.0").join(artifact.file_name);
    let expected_checksum = format!("{}  {}\n", artifact.sha256, compressed.display());
    ensure_that(
      (result, effects),
      "provisioning must return the executable, bind the official checksum to the artifact path, and download, verify, decompress, and \
       mark it executable in order",
      |observed| {
        let events = observed.1.events();
        let checksum = events.iter().find(|event| {
          matches!(**event, EffectEvent::WriteBytes { ref path, atomic: None, .. }
            if path.extension().is_some_and(|extension| extension == "sha256"))
        });
        let requests = observed.1.process_requests();
        observed
          .0
          .as_ref()
          .is_ok_and(|runner| runner.as_path() == Path::new("target/stask-tools/toml-test-v2.2.0/toml-test-v2.2.0-linux-amd64"))
          && checksum
            .is_some_and(|event| matches!(*event, EffectEvent::WriteBytes { ref contents, .. } if contents == expected_checksum.as_bytes()))
          && requests.iter().map(|request| request.program.clone()).collect::<Vec<_>>()
            == ["curl", artifact.checksum_program, "gzip", "chmod"].map(OsString::from)
      },
    )
    .map(drop)
    .map_err(Into::into)
  }

  #[test]
  fn provisioning_stops_at_the_first_failed_process() -> Result<(), ExtensionTestFailure<Provisioning>> {
    let artifact = ensure_ok(
      artifact_for_host("linux", "x86_64"),
      "the deterministic failure fixture must select the Linux artifact",
    )?;
    let (provisioning_context, provisioning_effects) = recording_context(&[0, 7])?;
    let result = prepare_runner(&provisioning_context, &artifact);
    ensure_that(
      (result, provisioning_effects),
      "checksum failure must reject provisioning before decompression and permission changes",
      |observed| {
        let requests = observed.1.process_requests();
        observed.0.is_err()
          && requests.len() == 2
          && requests
            .get(1)
            .is_some_and(|request| request.program == artifact.checksum_program)
      },
    )
    .map(drop)
    .map_err(Into::into)
  }

  #[test]
  fn conformance_execution_stops_at_the_first_failed_process() -> Result<(), ExtensionTestFailure<Conformance>> {
    if artifact_for_host(OS, ARCH).is_err() {
      return Ok(());
    }
    let (run_context, run_effects) = recording_context(&[0, 0, 0, 0, 11])?;
    let result = run(&run_context);
    ensure_that(
      (result, run_effects),
      "a decoder build failure must reject conformance before invoking the runner",
      |observed| {
        let requests = observed.1.process_requests();
        observed.0.is_err() && requests.len() == 5 && requests.last().is_some_and(|request| request.program == "cargo")
      },
    )
    .map(drop)
    .map_err(Into::into)
  }

  #[test]
  fn complete_conformance_execution_builds_then_invokes_the_pinned_runner() -> Result<(), ExtensionTestFailure<Conformance>> {
    let artifact = match artifact_for_host(OS, ARCH) {
      Ok(artifact) => artifact,
      Err(_unsupported) => return Ok(()),
    };
    let (context, effects) = recording_context(&[0, 0, 0, 0, 0, 0])?;
    let result = run(&context);
    let expected_runner = PathBuf::from(super::TOOL_DIRECTORY).join(artifact.file_name).with_extension("");
    let expected_arguments = ["test", "-toml", "1.1", "-decoder", "./target/debug/taplo toml-test"].map(OsString::from);
    ensure_that(
      (result, effects),
      "the supported host must provision, build, and execute the pinned runner with the complete TOML 1.1 decoder contract",
      |observed| {
        observed.0.is_ok()
          && observed.1.process_requests().last().is_some_and(|runner| {
            (runner.program.as_os_str(), runner.arguments.as_slice()) == (expected_runner.as_os_str(), expected_arguments.as_slice())
          })
      },
    )
    .map(drop)
    .map_err(Into::into)
  }

  #[test]
  fn process_paths_accept_unicode_and_reject_unrepresentable_host_paths() -> Result<(), impl Debug> {
    let unicode = path_argument(Path::new("target/tool"));

    #[cfg(unix)]
    {
      use std::os::unix::ffi::OsStringExt as _;

      let invalid = PathBuf::from(OsString::from_vec(vec![0xff]));
      let rejected = path_argument(&invalid);
      ensure_that(
        (unicode, invalid, rejected),
        "process paths must preserve exact Unicode text and retain a rejected non-Unicode path in its native error",
        |observed| {
          matches!(observed.0, Ok(ref argument) if argument == "target/tool")
            && matches!(observed.2, Err(StaskError::PathUnicode { ref path }) if path == &observed.1)
            && observed
              .2
              .as_ref()
              .is_err_and(|error| error.to_string().contains("not valid Unicode"))
        },
      )
      .map(drop)
      .map_err(Box::new)
    }

    #[cfg(not(unix))]
    ensure_that(unicode, "Unicode process paths must preserve exact text", |observed| {
      observed.as_ref().is_ok_and(|argument| argument == "target/tool")
    })
    .map(drop)
    .map_err(Box::new)
  }
}
