//! Deterministic process and filesystem effects for extension behavior tests.

use std::path::PathBuf;
use std::sync::Arc;

use strict_test_support::RecordingEffects;
use strict_test_support::TestFailure;
use strict_test_support::process_output;
use template_core::cli::color::ColorContext;
use template_core::cli::context::CommandContext;

/// Construct one command context with process results consumed in call order.
pub(super) fn recording_context(statuses: &[i32]) -> Result<(CommandContext, Arc<RecordingEffects>), TestFailure> {
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
