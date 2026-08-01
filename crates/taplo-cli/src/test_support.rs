//! Test-target adapter from the shared deterministic host into CLI environment traits.

use std::future::Future;
use std::ops::Deref;
use std::path::PathBuf;

use taplo_common::config::CONFIG_FILE_NAMES;
use taplo_common::environment::ConcurrentEnvironment;
use taplo_common::environment::Environment;
use taplo_common::environment::EnvironmentError;
use time::OffsetDateTime;

use crate::TestEnvironment;

taplo_test_support::implement_local_test_environment!(
  TestEnvironment,
  CONFIG_FILE_NAMES,
  taplo_common::implement_file_path_environment,
  taplo_common::implement_local_environment
);
taplo_test_support::implement_concurrent_test_environment!(TestEnvironment, CONFIG_FILE_NAMES);
