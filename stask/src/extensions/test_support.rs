//! Deterministic process and filesystem effects for extension behavior tests.

use std::path::PathBuf;
use std::sync::Arc;

use strict_test_support::PredicateFailure;
use strict_test_support::RecordingEffects;
use strict_test_support::RecordingWorkspace;
use strict_test_support::ResultFailure;
use strict_test_support::TestFailure;
use strict_test_support::process_output;
use template_core::cli::color::ColorContext;
use template_core::cli::context::CommandContext;

use super::error::StaskError;

/// Complete command result and its deterministic effect recorder.
pub(super) type Execution<Outcome = (), Failure = template_core::CoreError> = (Result<Outcome, Failure>, Arc<RecordingEffects>);

/// Native setup failures and complete rejected extension observations.
#[derive(Debug, thiserror::Error)]
pub(super) enum ExtensionTestFailure<Subject> {
  /// A native process-output fixture could not be constructed.
  #[error(transparent)]
  Fixture(#[from] TestFailure),
  /// A required host artifact could not be selected for the test.
  #[error(transparent)]
  Artifact(#[from] ResultFailure<StaskError>),
  /// An extension result or effect sequence violated its contract.
  #[error(transparent)]
  Observation(Box<PredicateFailure<Subject>>),
}

impl<Subject> From<PredicateFailure<Subject>> for ExtensionTestFailure<Subject> {
  fn from(source: PredicateFailure<Subject>) -> Self {
    Self::Observation(Box::new(source))
  }
}

/// Construct one command context with process results consumed in call order.
pub(super) fn recording_context(statuses: &[i32]) -> Result<(CommandContext<RecordingWorkspace>, Arc<RecordingEffects>), TestFailure> {
  let recorder = Arc::new(RecordingEffects::default());
  for status in statuses {
    recorder.queue_process_result(Ok(process_output(*status, Vec::new(), Vec::new())?));
  }
  let (context, _output) = CommandContext::with_captured_effects(
    PathBuf::from("/work/repository"),
    ColorContext::captured_auto_for_tests(),
    Arc::clone(&recorder),
  );
  Ok((context, recorder))
}
