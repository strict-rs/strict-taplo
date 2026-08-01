//! Checksum-pinned TOML 1.1 conformance orchestration.

use std::env::consts::ARCH;
use std::env::consts::OS;
use std::path::Path;
use std::path::PathBuf;

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
/// Returns a workflow-wrapped [`StaskError::PathUnicode`] when `path` cannot be
/// represented as UTF-8.
fn path_argument(path: &Path) -> template_core::Result<String> {
  path.to_str().map(str::to_owned).ok_or_else(|| {
    CoreError::workflow(StaskError::PathUnicode {
      path: path.to_owned()
    })
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
fn prepare_runner(context: &CommandContext, artifact: &Artifact) -> template_core::Result<PathBuf> {
  let tool_directory = PathBuf::from(TOOL_DIRECTORY);
  context.file_system().create_dir_all(&tool_directory).map_err(CoreError::from)?;

  let compressed_path = tool_directory.join(artifact.file_name);
  let executable_path = compressed_path.with_extension("");
  let checksum_path = compressed_path.with_extension("sha256");
  let checksum = format!("{}  {}\n", artifact.sha256, compressed_path.display());
  context
    .file_system()
    .write_bytes(&checksum_path, checksum.as_bytes())
    .map_err(CoreError::from)?;

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
pub(super) fn run(context: &CommandContext) -> template_core::Result<()> {
  let artifact = artifact_for_host(OS, ARCH).map_err(CoreError::workflow)?;
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
}

#[cfg(test)]
/// Host-selection contract tests for checksum-pinned conformance artifacts.
mod tests {
  use std::env::consts::ARCH;
  use std::env::consts::OS;
  use std::ffi::OsString;
  use std::path::Path;
  use std::path::PathBuf;

  use strict_test_support::EffectEvent;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_contains;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;

  use super::StaskError;
  use super::artifact_for_host;
  use super::path_argument;
  use super::prepare_runner;
  use super::run;
  use crate::extensions::test_support::recording_context;

  /// Pin the CI runner artifact and its official checksum.
  #[test]
  fn selects_checksum_pinned_linux_runner() -> Result<(), TestFailure> {
    let artifact = ensure_ok(
      artifact_for_host("linux", "x86_64"),
      "the CI host must have a configured toml-test artifact",
    )?;
    ensure_eq(
      &artifact.sha256,
      &"08f9e0a97da1151c33debf01358a8f5ef45e2a56be201241ae5eb5c2e9323fef",
      "the Linux runner checksum must remain release-pinned",
    )
  }

  /// Reject hosts that lack an explicitly checksum-pinned artifact.
  #[test]
  fn rejects_unconfigured_toml_test_host() -> Result<(), TestFailure> {
    ensure(
      artifact_for_host("unknown", "unknown")
        == Err(StaskError::UnsupportedTomlTestHost {
          os:           "unknown",
          architecture: "unknown",
        }),
      "unknown hosts must fail instead of downloading an unverified artifact",
    )
  }

  #[test]
  fn provisions_the_pinned_runner_with_checksum_and_command_order() -> Result<(), TestFailure> {
    let artifact = ensure_ok(
      artifact_for_host("linux", "x86_64"),
      "the deterministic provisioning fixture must select the Linux artifact",
    )?;
    let (context, effects) = recording_context(&[0, 0, 0, 0])?;
    let runner = ensure_ok(
      prepare_runner(&context, &artifact),
      "the checksum-pinned runner provisioning sequence must succeed",
    )?;
    ensure(
      runner.as_path() == Path::new("target/stask-tools/toml-test-v2.2.0/toml-test-v2.2.0-linux-amd64"),
      "runner provisioning must return the decompressed executable path",
    )?;

    let events = effects.events();
    let checksum = ensure_some(
      events.iter().find_map(|event| match *event {
        EffectEvent::WriteBytes {
          ref path,
          ref contents,
          atomic: false,
        } if path.extension().is_some_and(|extension| extension == "sha256") => Some(contents),
        EffectEvent::Process(_)
        | EffectEvent::PathState(_)
        | EffectEvent::CreateDirAll(_)
        | EffectEvent::RemovePath(_)
        | EffectEvent::Rename {
          ..
        }
        | EffectEvent::CopyDirAll {
          ..
        }
        | EffectEvent::WriteBytes {
          ..
        }
        | EffectEvent::ReadBytes(_)
        | EffectEvent::CreateDirectorySymlink {
          ..
        }
        | EffectEvent::ReadDirectory(_)
        | EffectEvent::EnvironmentVariable(_)
        | EffectEvent::CurrentDirectory
        | EffectEvent::ClockNow
        | EffectEvent::CreateWorkspace {
          ..
        }
        | EffectEvent::CloseWorkspace(_) => None,
      }),
      "runner provisioning must write one checksum manifest",
    )?;
    let compressed = PathBuf::from("target/stask-tools/toml-test-v2.2.0").join(artifact.file_name);
    let expected_checksum = format!("{}  {}\n", artifact.sha256, compressed.display());
    ensure(
      checksum == expected_checksum.as_bytes(),
      "the checksum manifest must bind the official digest to the downloaded artifact path",
    )?;
    let requests = effects.process_requests();
    let request_programs = requests.iter().map(|request| request.program.clone()).collect::<Vec<_>>();
    ensure(
      request_programs == ["curl", artifact.checksum_program, "gzip", "chmod"].map(OsString::from),
      "runner provisioning must download, verify, decompress, and mark the executable in order",
    )
  }

  #[test]
  fn provisioning_and_conformance_execution_stop_at_the_first_failed_process() -> Result<(), TestFailure> {
    let artifact = ensure_ok(
      artifact_for_host("linux", "x86_64"),
      "the deterministic failure fixture must select the Linux artifact",
    )?;
    let (provisioning_context, provisioning_effects) = recording_context(&[0, 7])?;
    ensure(
      prepare_runner(&provisioning_context, &artifact).is_err(),
      "a checksum verification failure must reject runner provisioning",
    )?;
    let provisioning_requests = provisioning_effects.process_requests();
    ensure(
      (
        provisioning_requests.len(),
        provisioning_requests.get(1).map(|request| request.program.clone()),
      ) == (2, Some(OsString::from(artifact.checksum_program))),
      "checksum failure must stop before decompression and permission changes",
    )?;

    if artifact_for_host(OS, ARCH).is_err() {
      return Ok(());
    }
    let (run_context, run_effects) = recording_context(&[0, 0, 0, 0, 11])?;
    ensure(
      run(&run_context).is_err(),
      "a decoder build failure must reject the conformance extension",
    )?;
    let run_requests = run_effects.process_requests();
    ensure(
      (run_requests.len(), run_requests.last().map(|request| request.program.clone())) == (5, Some(OsString::from("cargo"))),
      "a decoder build failure must stop before invoking the conformance runner",
    )
  }

  #[test]
  fn complete_conformance_execution_builds_then_invokes_the_pinned_runner() -> Result<(), TestFailure> {
    let artifact = match artifact_for_host(OS, ARCH) {
      Ok(artifact) => artifact,
      Err(_unsupported) => return Ok(()),
    };
    let (context, effects) = recording_context(&[0, 0, 0, 0, 0, 0])?;
    ensure_ok(
      run(&context),
      "the supported host must complete runner provisioning, decoder build, and conformance execution",
    )?;
    let requests = effects.process_requests();
    let runner = ensure_some(requests.last(), "complete conformance execution must emit its runner request")?;
    let expected_runner = PathBuf::from(super::TOOL_DIRECTORY).join(artifact.file_name).with_extension("");
    let expected_arguments = ["test", "-toml", "1.1", "-decoder", "./target/debug/taplo toml-test"].map(OsString::from);
    ensure(
      (runner.program.as_os_str(), runner.arguments.as_slice()) == (expected_runner.as_os_str(), expected_arguments.as_slice()),
      "the pinned runner must execute the complete TOML 1.1 decoder contract",
    )
  }

  #[test]
  fn process_paths_accept_unicode_and_reject_unrepresentable_host_paths() -> Result<(), TestFailure> {
    ensure_eq(
      &ensure_ok(
        path_argument(Path::new("target/tool")),
        "a Unicode process path must convert to an owned argument",
      )?
      .as_str(),
      &"target/tool",
      "Unicode process paths must preserve exact text",
    )?;

    #[cfg(unix)]
    {
      use std::os::unix::ffi::OsStringExt as _;

      let invalid = PathBuf::from(OsString::from_vec(vec![0xff]));
      let message = ensure_some(
        path_argument(&invalid).err().map(|error| error.to_string()),
        "a non-Unicode process path must return a typed workflow error",
      )?;
      ensure_contains(
        &message,
        "not valid Unicode",
        "the path failure must retain the rejected representation contract",
      )
    }?;
    Ok(())
  }
}
