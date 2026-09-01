//! Taplo language-server composition for local and concurrent execution models.

#![forbid(unsafe_code)]

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;

#[cfg(not(target_arch = "wasm32"))]
use taplo_common::environment::ConcurrentEnvironment;
use taplo_common::environment::LocalEnvironment;
#[cfg(not(target_arch = "wasm32"))]
use taplo_common::schema::transport::ConcurrentSchemaTransport;
use taplo_common::schema::transport::LocalSchemaTransport;
#[cfg(not(target_arch = "wasm32"))]
use world::ConcurrentWorld;
use world::LocalWorld;
use world::WorldError;
use world::WorldState;

/// Generate aligned local and concurrent Taplo LSP operation families.
macro_rules! define_lsp_execution_families {
  (
    handler
    $operations:ident;
    $(($local:ident, $concurrent:ident)),+ $(,)?
  ) => {
    use crate::LocalFuture as HandlerLocalFuture;
    use taplo_common::environment::LocalEnvironment as HandlerLocalEnvironment;
    use taplo_common::schema::transport::LocalSchemaTransport as HandlerLocalSchemaTransport;
    #[cfg(not(target_arch = "wasm32"))]
    use crate::ConcurrentFuture as HandlerConcurrentFuture;
    #[cfg(not(target_arch = "wasm32"))]
    use taplo_common::environment::ConcurrentEnvironment as HandlerConcurrentEnvironment;
    #[cfg(not(target_arch = "wasm32"))]
    use taplo_common::schema::transport::ConcurrentSchemaTransport as HandlerConcurrentSchemaTransport;

    define_lsp_execution_families!(
      @families
      $operations;
      $(($local, $concurrent)),+;
      local = (
        HandlerLocalFuture,
        HandlerLocalEnvironment,
        HandlerLocalSchemaTransport,
        crate::world::LocalSchemaExecution
      );
      concurrent = (
        HandlerConcurrentFuture,
        HandlerConcurrentEnvironment,
        HandlerConcurrentSchemaTransport,
        crate::world::ConcurrentSchemaExecution
      );
    );
  };
  (
    world
    $operations:ident;
    $(($local:ident, $concurrent:ident)),+ $(,)?
  ) => {
    define_lsp_execution_families!(
      @families
      $operations;
      $(($local, $concurrent)),+;
      local = (
        LocalFuture,
        LocalEnvironment,
        LocalSchemaTransport,
        LocalSchemaExecution
      );
      concurrent = (
        ConcurrentFuture,
        ConcurrentEnvironment,
        ConcurrentSchemaTransport,
        ConcurrentSchemaExecution
      );
    );
  };
  (
    @families
    $operations:ident;
    $(($local:ident, $concurrent:ident)),+;
    local = (
      $local_future:ident,
      $local_environment:path,
      $local_transport:ident,
      $local_schema_execution:path
    );
    concurrent = (
      $concurrent_future:ident,
      $concurrent_environment:path,
      $concurrent_transport:ident,
      $concurrent_schema_execution:path
    );
  ) => {
    $operations!(
      $($local),+;
      $local_future,
      $local_environment,
      $local_transport,
      $local_schema_execution;
      []
    );
    #[cfg(not(target_arch = "wasm32"))]
    $operations!(
      $($concurrent),+;
      $concurrent_future,
      $concurrent_environment,
      $concurrent_transport,
      $concurrent_schema_execution;
      [Send]
    );
  };
}

pub(crate) mod handlers;

pub mod config;
pub mod lsp_ext;
pub mod query;
pub mod world;

pub use handlers::create_local_server;

/// A current-thread operation that may retain local or WebAssembly state.
pub(crate) type LocalFuture<'operation, Output> = Pin<Box<dyn Future<Output = Output> + 'operation>>;

/// A native operation that may cross executor threads.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) type ConcurrentFuture<'operation, Output> = Pin<Box<dyn Future<Output = Output> + Send + 'operation>>;

/// A current-thread test operation resolving to the panic-free test outcome.
#[cfg(test)]
pub(crate) type LocalTestFuture<'operation, Output> = LocalFuture<'operation, Result<Output, strict_test_support::TestFailure>>;

/// Construct a local world using an explicitly configured browser-compatible HTTP client.
///
/// # Errors
///
/// Returns [`WorldError`] when the detached workspace or schema services cannot initialize.
pub fn create_local_world<E: LocalEnvironment>(environment: E, http: reqwest::Client) -> Result<LocalWorld<E>, WorldError> {
  let transport = LocalSchemaTransport::new(environment.clone(), http);
  WorldState::with_transport(environment, transport).map(Rc::new)
}

#[cfg(not(target_arch = "wasm32"))]
pub use handlers::create_concurrent_server;

/// Construct a concurrent world using an explicitly configured native HTTP client.
///
/// # Errors
///
/// Returns [`WorldError`] when the detached workspace or schema services cannot initialize.
#[cfg(not(target_arch = "wasm32"))]
pub fn create_concurrent_world<E: ConcurrentEnvironment>(environment: E, http: reqwest::Client) -> Result<ConcurrentWorld<E>, WorldError> {
  let transport = ConcurrentSchemaTransport::new(environment.clone(), http);
  WorldState::with_transport(environment, transport).map(Arc::new)
}
