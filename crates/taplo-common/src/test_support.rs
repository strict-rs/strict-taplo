//! Test-target adapter from the shared deterministic host into this crate's environment traits.

use std::future::Future;
use std::ops::Deref;
use std::path::PathBuf;

use taplo_test_support::TestEnvironment as SharedTestEnvironment;
use time::OffsetDateTime;

use crate::config::CONFIG_FILE_NAMES;
use crate::environment::ConcurrentEnvironment;
use crate::environment::Environment;
use crate::environment::EnvironmentError;

/// Environment-trait adapter around the acyclic shared deterministic host.
#[derive(Clone, Debug, Default)]
pub struct TestEnvironment(SharedTestEnvironment);

taplo_test_support::implement_local_test_environment!(
  TestEnvironment,
  CONFIG_FILE_NAMES,
  crate::implement_file_path_environment,
  crate::implement_local_environment
);
taplo_test_support::implement_concurrent_test_environment!(TestEnvironment, CONFIG_FILE_NAMES);
