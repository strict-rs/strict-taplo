//! Shared language-server request, notification, snapshot, and effect handling.

#[cfg(not(target_arch = "wasm32"))]
use taplo_common::environment::ConcurrentEnvironment;
use taplo_common::environment::LocalEnvironment;
#[cfg(not(target_arch = "wasm32"))]
use taplo_lsp_async::ConcurrentServer;
use taplo_lsp_async::LocalServer;
use taplo_lsp_async::rpc::RpcError;
use url::Url;

#[cfg(not(target_arch = "wasm32"))]
use crate::world::ConcurrentWorld;
use crate::world::DocumentDisposition;
use crate::world::DocumentSnapshot;
use crate::world::LocalWorld;
use crate::world::WorldState;

/// Bind one current document snapshot or return an absent optional handler response.
macro_rules! current_document_snapshot {
  ($lookup:expr => ($document:ident, $snapshot:ident)) => {
    let Some(($document, $snapshot)) = $lookup.await else {
      return Ok(None);
    };
  };
}

/// Generate one document handler family with shared snapshot lookup and selected freshness logic.
macro_rules! define_document_handler_execution_families {
  (
    $operations:ident;
    $freshness_local:ident,
    $freshness_concurrent:ident;
    $(($local:ident, $concurrent:ident)),+;
    $(($suffix_local:ident, $suffix_concurrent:ident)),* $(,)?
  ) => {
    define_lsp_execution_families!(
      handler
      $operations;
      $(($local, $concurrent)),+,
      (
        document_snapshot_for_uri_local,
        document_snapshot_for_uri_concurrent
      ),
      ($freshness_local, $freshness_concurrent),
      $(($suffix_local, $suffix_concurrent)),*
    );
  };
}

/// Generate one document handler family that validates freshness before returning a result.
macro_rules! define_checked_document_handler_execution_families {
  (
    $operations:ident;
    $(($local:ident, $concurrent:ident)),+ $(,)?
  ) => {
    define_document_handler_execution_families!(
      $operations;
      ensure_current_snapshot_local,
      ensure_current_snapshot_concurrent;
      $(($local, $concurrent)),+;
    );
  };
}

/// Generate one document handler family that preserves an optional current-snapshot response.
macro_rules! define_response_document_handler_execution_families {
  (
    $operations:ident;
    $(($local:ident, $concurrent:ident)),+ $(,)?
  ) => {
    define_document_handler_execution_families!(
      $operations;
      current_snapshot_response_local,
      current_snapshot_response_concurrent;
      $(($local, $concurrent)),+;
    );
  };
}

/// Generate coordination operations over one concrete LSP execution family.
macro_rules! define_handler_coordination_family {
  (
    $document_snapshot_for_uri:ident,
    $collect_document_diagnostics:ident,
    $ensure_current_snapshot:ident,
    $current_snapshot_response:ident,
    $document_snapshot:ident,
    $snapshot_is_current:ident,
    $open_document_dispositions:ident,
    $collect_diagnostics:ident;
    $future:ident,
    $environment:path,
    $transport:ident,
    $schema_execution:path;
    [$($value_bound:path),*]
  ) => {
    /// Resolve one wire document URI to its current immutable world snapshot.
    fn $document_snapshot_for_uri<'operation, E: $environment>(
      world: &'operation WorldState<E, $transport<E>>,
      uri: &'operation lsp_types::Uri,
    ) -> $future<'operation, Option<(Url, DocumentSnapshot<$transport<E>>)>> {
      Box::pin(async move {
        let document = uri::to_url(uri)?;
        let snapshot = world.$document_snapshot(&document).await?;
        Some((document, snapshot))
      })
    }

    /// Collect current diagnostics for open documents selected by one semantic predicate.
    fn $collect_document_diagnostics<'operation, E, P>(
      world: &'operation WorldState<E, $transport<E>>,
      mut selected: P,
    ) -> $future<'operation, Result<Vec<DiagnosticBatch>, DiagnosticError>>
    where
      E: $environment,
      P: FnMut(&Url) -> bool $(+ $value_bound)* + 'operation,
    {
      Box::pin(async move {
        let mut diagnostics = Vec::new();
        for (document, disposition) in world.$open_document_dispositions().await {
          if !selected(&document) {
            continue;
          }
          match disposition {
            DocumentDisposition::Included => {
              if let Some(batch) = $collect_diagnostics(world, &document).await? {
                diagnostics.push(batch);
              }
            }
            DocumentDisposition::Excluded => {
              diagnostics.push(excluded_diagnostics(&document)?);
            }
          }
        }
        Ok(diagnostics)
      })
    }

    /// Reject generation-dependent output after its captured document state changes.
    fn $ensure_current_snapshot<'operation, E: $environment>(
      world: &'operation WorldState<E, $transport<E>>,
      document: &'operation Url,
      snapshot: &'operation DocumentSnapshot<$transport<E>>,
    ) -> $future<'operation, Result<(), RpcError>> {
      Box::pin(async move {
        if world.$snapshot_is_current(document, snapshot).await {
          Ok(())
        } else {
          Err(RpcError::content_modified())
        }
      })
    }

    /// Return one optional generation-dependent response only while its snapshot remains current.
    fn $current_snapshot_response<'operation, E, R>(
      world: &'operation WorldState<E, $transport<E>>,
      document: &'operation Url,
      snapshot: &'operation DocumentSnapshot<$transport<E>>,
      response: Option<R>,
    ) -> $future<'operation, Result<Option<R>, RpcError>>
    where
      E: $environment,
      R: $($value_bound +)* 'operation,
    {
      Box::pin(async move {
        $ensure_current_snapshot(world, document, snapshot).await?;
        Ok(response)
      })
    }
  };
}

mod initialize;

mod documents;
use documents::DocumentEffects;
use documents::publish_params;

mod semantic_tokens;

mod folding_ranges;

mod document_symbols;

mod formatting;

mod hover;

mod completion;

mod schema;

mod configuration;
use configuration::ConfigurationEffects;

mod workspaces;
use workspaces::WorkspaceChangeEffects;

mod links;

mod rename;

mod conversion;
use conversion::convert_to_json;
use conversion::convert_to_toml;

#[path = "diagnostics.rs"]
mod diagnostics;
use diagnostics::DiagnosticBatch;
use diagnostics::DiagnosticError;
#[cfg(not(target_arch = "wasm32"))]
use diagnostics::collect_diagnostics_concurrent;
use diagnostics::collect_diagnostics_local;
use diagnostics::excluded_diagnostics;

/// Shared panic-free fixtures for sibling handler behavior tests.
#[cfg(test)]
mod test_support {
  use std::future::Future;
  use std::sync::Arc;

  use serde::de::DeserializeOwned;
  use serde_json::Value;
  use serde_json::json;
  use strict_test_support::ResultFailure;
  use strict_test_support::ensure_ok;
  use taplo::dom::error::QueryError;
  use taplo_common::schema::associations::AssociationRule;
  use taplo_common::schema::associations::SchemaAssociation;
  use taplo_common::schema::associations::priority;
  use taplo_common::schema::associations::source;
  use taplo_common::schema::transport::SchemaTransport;
  use taplo_common::schema::transport::TransportError;
  use taplo_common::schema::transport::local_http_client;
  use taplo_lsp_async::util::MappingError;
  use thiserror::Error;
  use url::Url;

  use crate::LocalFuture;
  #[cfg(not(target_arch = "wasm32"))]
  use crate::world::ConcurrentWorld;
  use crate::world::DocumentSnapshot;
  use crate::world::DocumentState;
  use crate::world::DocumentUpdate;
  use crate::world::LocalWorld;
  use crate::world::TestEnvironment;
  use crate::world::WorldError;

  /// Native failures while constructing handler fixtures.
  #[derive(Debug, Error)]
  pub(super) enum FixtureFailure {
    /// HTTP capability construction failed.
    #[error(transparent)]
    Http(#[from] Box<ResultFailure<TransportError>>),
    /// World or document fixture construction failed.
    #[error(transparent)]
    World(#[from] Box<ResultFailure<WorldError>>),
    /// A fixture URL was invalid.
    #[error(transparent)]
    Url(#[from] ResultFailure<url::ParseError>),
    /// A protocol fixture could not be decoded.
    #[error(transparent)]
    Json(#[from] ResultFailure<serde_json::Error>),
    /// Coordinate fixture construction failed.
    #[error(transparent)]
    Mapping(#[from] ResultFailure<MappingError>),
    /// A semantic fixture path could not be decoded.
    #[error(transparent)]
    Query(#[from] ResultFailure<QueryError>),
  }

  /// Canonical document and schema URLs for one schema-backed handler scenario.
  #[derive(Debug)]
  pub(super) struct SchemaFixture {
    /// Document URL presented through the protocol.
    pub(super) document:   Url,
    /// Schema URL installed in the world-owned schema service.
    pub(super) schema_url: Url,
  }

  /// Native source replacement outcome including every emitted document effect.
  pub(super) type DocumentInstallation = Result<DocumentUpdate, Box<ResultFailure<WorldError>>>;

  /// Construct one fresh detached local world through the production crate façade.
  pub(super) fn local_world() -> Result<LocalWorld<TestEnvironment>, FixtureFailure> {
    let http = ensure_ok(local_http_client(), "the local handler-fixture HTTP client must construct").map_err(Box::new)?;
    ensure_ok(
      crate::create_local_world(TestEnvironment::default(), http),
      "the local handler-fixture world must construct",
    )
    .map_err(Box::new)
    .map_err(FixtureFailure::from)
  }

  /// Construct one fresh detached concurrent world through the production crate façade.
  #[cfg(not(target_arch = "wasm32"))]
  pub(super) fn concurrent_world() -> Result<ConcurrentWorld<TestEnvironment>, FixtureFailure> {
    let http = ensure_ok(local_http_client(), "the concurrent handler-fixture HTTP client must construct").map_err(Box::new)?;
    ensure_ok(
      crate::create_concurrent_world(TestEnvironment::default(), http),
      "the concurrent handler-fixture world must construct",
    )
    .map_err(Box::new)
    .map_err(FixtureFailure::from)
  }

  /// Parse one absolute handler-fixture URL with a behavior-specific failure context.
  pub(super) fn url(input: &str, context: &'static str) -> Result<Url, ResultFailure<url::ParseError>> {
    ensure_ok(Url::parse(input), context)
  }

  /// Parse the complete URL identity for one schema-backed handler scenario.
  pub(super) fn schema_fixture(
    document: &str,
    schema_url: &str,
    document_context: &'static str,
    schema_context: &'static str,
  ) -> Result<SchemaFixture, ResultFailure<url::ParseError>> {
    Ok(SchemaFixture {
      document:   url(document, document_context)?,
      schema_url: url(schema_url, schema_context)?,
    })
  }

  /// Decode one positioned text-document request with an optional rename field.
  fn positioned_params<P: DeserializeOwned>(
    document: &Url,
    line: u32,
    character: u32,
    new_name: Option<&str>,
    context: &'static str,
  ) -> Result<P, ResultFailure<serde_json::Error>> {
    let mut request = serde_json::Map::new();
    drop(request.insert(
      "textDocument".into(),
      json!({
        "uri": document.as_str()
      }),
    ));
    drop(request.insert(
      "position".into(),
      json!({
        "line": line,
        "character": character
      }),
    ));
    if let Some(name) = new_name {
      drop(request.insert("newName".into(), Value::String(name.into())));
    }
    ensure_ok(serde_json::from_value(Value::Object(request)), context)
  }

  /// Decode one text-document position request through its public wire shape.
  pub(super) fn position_params<P: DeserializeOwned>(
    document: &Url,
    line: u32,
    character: u32,
    context: &'static str,
  ) -> Result<P, ResultFailure<serde_json::Error>> {
    positioned_params(document, line, character, None, context)
  }

  /// Decode one rename request through its public wire shape.
  #[allow(
    clippy::single_call_fn,
    reason = "the named decoder marks rename as the one positioned request that also carries `newName`, so its wire shape stays distinct \
              from `position_params` over the shared builder"
  )]
  pub(super) fn rename_params<P: DeserializeOwned>(
    document: &Url,
    line: u32,
    character: u32,
    new_name: &str,
    context: &'static str,
  ) -> Result<P, ResultFailure<serde_json::Error>> {
    positioned_params(document, line, character, Some(new_name), context)
  }

  /// Install one source revision in a local handler world.
  pub(super) fn replace_local_document<'operation>(
    world: &'operation LocalWorld<TestEnvironment>,
    document: &'operation Url,
    source: &'operation str,
    context: &'static str,
  ) -> LocalFuture<'operation, DocumentInstallation> {
    Box::pin(async move { ensure_ok(world.replace_document(document, source).await, context).map_err(Box::new) })
  }

  /// Install one source revision in a concurrent handler world.
  #[cfg(not(target_arch = "wasm32"))]
  pub(super) async fn replace_concurrent_document(
    world: &ConcurrentWorld<TestEnvironment>,
    document: &Url,
    source: &str,
    context: &'static str,
  ) -> DocumentInstallation {
    ensure_ok(world.replace_document_concurrent(document, source).await, context).map_err(Box::new)
  }

  /// Parse one immutable document fixture through the production document boundary.
  pub(super) fn parse_document(source: &str, context: &'static str) -> Result<DocumentState, Box<ResultFailure<WorldError>>> {
    ensure_ok(DocumentState::parse(source), context).map_err(Box::new)
  }

  /// Install one exact manual association and its complete in-memory schema.
  #[allow(
    clippy::single_call_fn,
    reason = "the named fixture keeps the exact-URL association and its schema body installed together, so no scenario can associate a \
              schema URL whose document is never resolvable"
  )]
  pub(super) fn install_schema<T: SchemaTransport>(snapshot: &DocumentSnapshot<T>, document: &Url, schema_url: &Url, schema: Value) {
    snapshot
      .schemas
      .associations()
      .add(AssociationRule::Url(document.clone()), SchemaAssociation {
        meta:     json!({ "source": source::MANUAL }),
        url:      schema_url.clone(),
        priority: priority::MAX,
      });
    snapshot.schemas.add_schema(schema_url, Arc::new(schema));
  }

  /// Complete document replacement and schema-bearing snapshot retained by a fixture.
  pub(super) type SchemaInstallation<T> = (DocumentInstallation, Option<DocumentSnapshot<T>>);

  /// Commit one source replacement, then install its schema into the resulting snapshot.
  pub(super) async fn install_schema_document<T: SchemaTransport>(
    pending_replacement: impl Future<Output = DocumentInstallation>,
    pending_snapshot: impl Future<Output = Option<DocumentSnapshot<T>>>,
    document: &Url,
    schema_url: &Url,
    schema: Value,
  ) -> SchemaInstallation<T> {
    let replacement = pending_replacement.await;
    let snapshot = pending_snapshot.await;
    if replacement.is_ok()
      && let Some(ref current) = snapshot
    {
      install_schema(current, document, schema_url, schema);
    }
    (replacement, snapshot)
  }
}

define_lsp_execution_families!(
  handler
  define_handler_coordination_family;
  (
    document_snapshot_for_uri_local,
    document_snapshot_for_uri_concurrent
  ),
  (
    collect_document_diagnostics_local,
    collect_document_diagnostics_concurrent
  ),
  (
    ensure_current_snapshot_local,
    ensure_current_snapshot_concurrent
  ),
  (
    current_snapshot_response_local,
    current_snapshot_response_concurrent
  ),
  (document_snapshot, document_snapshot_concurrent),
  (snapshot_is_current, snapshot_is_current_concurrent),
  (
    open_document_dispositions,
    open_document_dispositions_concurrent
  ),
  (collect_diagnostics_local, collect_diagnostics_concurrent),
);

/// Current-thread handler façade consumed by the generated local runtime family.
mod local {
  pub(super) use super::completion::completion_local as completion;
  pub(super) use super::configuration::apply_configuration_response_local as apply_configuration_response;
  pub(super) use super::configuration::configuration_change_local as configuration_change;
  pub(super) use super::configuration::configuration_request_local as configuration_request;
  pub(super) use super::diagnostics::collect_diagnostics_local as collect_diagnostics;
  pub(super) use super::document_symbols::document_symbols_local as document_symbols;
  pub(super) use super::documents::document_change_local as document_change;
  pub(super) use super::documents::document_close_local as document_close;
  pub(super) use super::documents::document_open_local as document_open;
  pub(super) use super::folding_ranges::folding_ranges_local as folding_ranges;
  pub(super) use super::formatting::format_local as format;
  pub(super) use super::hover::hover_local as hover;
  pub(super) use super::initialize::initialize_local as initialize;
  pub(super) use super::links::links_local as links;
  pub(super) use super::rename::prepare_rename_local as prepare_rename;
  pub(super) use super::rename::rename_local as rename;
  pub(super) use super::schema::associate_schema_local as associate_schema;
  pub(super) use super::schema::associated_schema_local as associated_schema;
  pub(super) use super::schema::list_schemas_local as list_schemas;
  pub(super) use super::semantic_tokens::semantic_tokens_local as semantic_tokens;
  pub(super) use super::workspaces::workspace_change_local as workspace_change;
}

/// Cross-thread handler façade consumed by the generated concurrent runtime family.
#[cfg(not(target_arch = "wasm32"))]
mod concurrent {
  pub(super) use super::completion::completion_concurrent as completion;
  pub(super) use super::configuration::apply_configuration_response_concurrent as apply_configuration_response;
  pub(super) use super::configuration::configuration_change_concurrent as configuration_change;
  pub(super) use super::configuration::configuration_request_concurrent as configuration_request;
  pub(super) use super::diagnostics::collect_diagnostics_concurrent as collect_diagnostics;
  pub(super) use super::document_symbols::document_symbols_concurrent as document_symbols;
  pub(super) use super::documents::document_change_concurrent as document_change;
  pub(super) use super::documents::document_close_concurrent as document_close;
  pub(super) use super::documents::document_open_concurrent as document_open;
  pub(super) use super::folding_ranges::folding_ranges_concurrent as folding_ranges;
  pub(super) use super::formatting::format_concurrent as format;
  pub(super) use super::hover::hover_concurrent as hover;
  pub(super) use super::initialize::initialize_concurrent as initialize;
  pub(super) use super::links::links_concurrent as links;
  pub(super) use super::rename::prepare_rename_concurrent as prepare_rename;
  pub(super) use super::rename::rename_concurrent as rename;
  pub(super) use super::schema::associate_schema_concurrent as associate_schema;
  pub(super) use super::schema::associated_schema_concurrent as associated_schema;
  pub(super) use super::schema::list_schemas_concurrent as list_schemas;
  pub(super) use super::semantic_tokens::semantic_tokens_concurrent as semantic_tokens;
  pub(super) use super::workspaces::workspace_change_concurrent as workspace_change;
}

#[path = "runtime.rs"]
mod runtime;

#[path = "uri.rs"]
mod uri;

/// Construct the current-thread server from the shared handler registry.
#[must_use]
#[allow(
  clippy::single_call_fn,
  reason = "one of the crate's four documented public entry points; its consumer contract is the published `taplo-lsp` API, not the \
            in-crate call count"
)]
pub fn create_local_server<E: LocalEnvironment>() -> LocalServer<LocalWorld<E>> {
  runtime::create_local_server()
}

/// Construct the native multi-threaded server from the shared handler registry.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
#[allow(
  clippy::single_call_fn,
  reason = "one of the crate's four documented public entry points; its consumer contract is the published `taplo-lsp` API, not the \
            in-crate call count"
)]
pub fn create_concurrent_server<E: ConcurrentEnvironment>() -> ConcurrentServer<ConcurrentWorld<E>> {
  runtime::create_concurrent_server()
}

#[cfg(test)]
mod tests {
  use std::fmt::Debug;
  use std::str::FromStr as _;

  use futures::executor::block_on;
  use lsp_types::DocumentSymbolParams;
  use lsp_types::FoldingRangeParams;
  use lsp_types::Position;
  use lsp_types::Range;
  use lsp_types::SemanticTokensParams;
  use lsp_types::SemanticTokensResult;
  use lsp_types::Uri;
  use serde::de::DeserializeOwned;
  use serde_json::json;
  use strict_test_support::ResultFailure;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_that;
  use taplo_lsp_async::Params;
  use taplo_lsp_async::rpc::RpcError;
  use url::Url;

  use super::current_snapshot_response_local as current_snapshot_response;
  use super::document_snapshot_for_uri_local as document_snapshot_for_uri;
  use super::document_symbols::document_symbols_concurrent;
  use super::document_symbols::document_symbols_local;
  use super::folding_ranges::folding_ranges_concurrent;
  use super::folding_ranges::folding_ranges_local;
  use super::semantic_tokens::semantic_tokens_concurrent;
  use super::semantic_tokens::semantic_tokens_local;
  use super::uri;
  use crate::LocalFuture;
  use crate::handlers::test_support::DocumentInstallation;
  use crate::handlers::test_support::FixtureFailure;
  use crate::handlers::test_support::concurrent_world;
  use crate::handlers::test_support::local_world;
  use crate::handlers::test_support::replace_concurrent_document;
  use crate::handlers::test_support::replace_local_document;
  use crate::handlers::test_support::url as fixture_url;
  use crate::world::ConcurrentWorld;
  use crate::world::LocalWorld;
  use crate::world::TestEnvironment;

  /// Source shared by the direct document-projection handler scenarios.
  const PROJECTION_SOURCE: &str = "# first\n# second\nitems = [\n  1,\n  2,\n]\ninline = { nested = true }\n[table]\nvalue = 1\n";

  /// Open local and concurrent worlds and their native document-installation outcomes.
  #[derive(Debug)]
  struct ProjectionFixture {
    /// Canonical document URL installed in both worlds.
    document:   Url,
    /// Current-thread world holding the source.
    local:      LocalWorld<TestEnvironment>,
    /// Cross-thread world holding the same source.
    concurrent: ConcurrentWorld<TestEnvironment>,
    /// Complete local and concurrent document installation outcomes.
    installed:  [DocumentInstallation; 2],
  }

  /// Parse one handler fixture URL.
  fn document_url() -> Result<Url, ResultFailure<url::ParseError>> {
    fixture_url("file:///workspace/document.toml", "the handler fixture URL must parse")
  }

  /// Decode one document-only request through its public wire shape.
  fn document_params<P: DeserializeOwned>(document: &Url) -> Result<P, ResultFailure<serde_json::Error>> {
    ensure_ok(
      serde_json::from_value(json!({
        "textDocument": {
          "uri": document.as_str()
        }
      })),
      "the document-only handler request fixture must decode",
    )
  }

  /// Install the same projection source through both public world execution families.
  fn projection_fixture() -> LocalFuture<'static, Result<ProjectionFixture, FixtureFailure>> {
    Box::pin(async {
      let document = document_url()?;
      let local = local_world()?;
      let concurrent = concurrent_world()?;
      let local_installation =
        replace_local_document(&local, &document, PROJECTION_SOURCE, "the local projection document must install").await;
      let concurrent_installation = replace_concurrent_document(
        &concurrent,
        &document,
        PROJECTION_SOURCE,
        "the concurrent projection document must install",
      )
      .await;
      Ok(ProjectionFixture {
        document,
        local,
        concurrent,
        installed: [local_installation, concurrent_installation],
      })
    })
  }

  #[test]
  fn generation_dependent_responses_accept_current_and_reject_stale_snapshots() -> Result<(), impl Debug> {
    let observation = block_on(async {
      let world = local_world()?;
      let document = document_url()?;
      let initial = world.replace_document(&document, "value = 1\n").await;
      let snapshot = world.document_snapshot(&document).await;
      let current = if let Some(ref captured) = snapshot {
        Some(current_snapshot_response(&world, &document, captured, Some(7_u8)).await)
      } else {
        None
      };
      let replacement = world.replace_document(&document, "value = 2\n").await;
      let stale = if let Some(ref captured) = snapshot {
        Some(current_snapshot_response(&world, &document, captured, Option::<u8>::None).await)
      } else {
        None
      };
      Ok::<_, FixtureFailure>((world, document, initial, snapshot, current, replacement, stale))
    });
    ensure_that(
      observation,
      "current responses preserve their payload and stale empty responses return content-modified",
      |subject| {
        let Ok(ref scenario) = *subject else {
          return false;
        };
        scenario.2.is_ok()
          && scenario.3.is_some()
          && scenario.4 == Some(Ok(Some(7_u8)))
          && scenario.5.is_ok()
          && scenario.6 == Some(Err(RpcError::content_modified()))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn document_snapshot_loader_rejects_unresolvable_inputs_and_returns_current_state() -> Result<(), impl Debug> {
    let observation = block_on(async {
      let world = local_world()?;
      let relative = Uri::from_str("workspace/document.toml");
      let relative_lookup = if let Ok(ref wire) = relative {
        Some(document_snapshot_for_uri(&world, wire).await)
      } else {
        None
      };
      let document = document_url()?;
      let document_uri = uri::to_uri(&document);
      let missing = if let Some(ref wire) = document_uri {
        Some(document_snapshot_for_uri(&world, wire).await)
      } else {
        None
      };
      let installed = world.replace_document(&document, "value = 1\n").await;
      let current = if let Some(ref wire) = document_uri {
        Some(document_snapshot_for_uri(&world, wire).await)
      } else {
        None
      };
      Ok::<_, FixtureFailure>((
        world, relative, relative_lookup, document, document_uri, missing, installed, current,
      ))
    });
    ensure_that(
      observation,
      "snapshot lookup rejects relative and unopened URIs while preserving an open document's identity and coordinates",
      |subject| {
        let Ok(ref scenario) = *subject else {
          return false;
        };
        scenario.1.is_ok()
          && matches!(scenario.2, Some(None))
          && scenario.4.is_some()
          && matches!(scenario.5, Some(None))
          && scenario.6.is_ok()
          && scenario.7.as_ref().and_then(Option::as_ref).is_some_and(|captured| {
            captured.0 == scenario.3 && captured.1.document.mapper.all_range() == Range::new(Position::new(0, 0), Position::new(1, 0))
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn document_projection_handlers_match_across_execution_families() -> Result<(), impl Debug> {
    let observation = block_on(async {
      let fixture = projection_fixture().await?;
      let tokens = document_params::<SemanticTokensParams>(&fixture.document)?;
      let symbols = document_params::<DocumentSymbolParams>(&fixture.document)?;
      let folds = document_params::<FoldingRangeParams>(&fixture.document)?;
      let local_tokens = semantic_tokens_local(&fixture.local, Params::from(Some(tokens.clone()))).await;
      let concurrent_tokens = semantic_tokens_concurrent(&fixture.concurrent, Params::from(Some(tokens))).await;
      let local_symbols = document_symbols_local(&fixture.local, Params::from(Some(symbols.clone()))).await;
      let concurrent_symbols = document_symbols_concurrent(&fixture.concurrent, Params::from(Some(symbols))).await;
      let local_folds = folding_ranges_local(&fixture.local, Params::from(Some(folds.clone()))).await;
      let concurrent_folds = folding_ranges_concurrent(&fixture.concurrent, Params::from(Some(folds))).await;
      Ok::<_, FixtureFailure>((fixture, [local_tokens, concurrent_tokens], [local_symbols, concurrent_symbols], [
        local_folds, concurrent_folds,
      ]))
    });
    ensure_that(
      observation,
      "local and concurrent projections preserve complete matching tokens, symbols, and folding ranges",
      |subject| {
        let Ok(ref scenario) = *subject else {
          return false;
        };
        let [ref local_tokens, ref concurrent_tokens] = scenario.1;
        let [ref local_symbols, ref concurrent_symbols] = scenario.2;
        let [ref local_folds, ref concurrent_folds] = scenario.3;
        scenario.0.installed.iter().all(Result::is_ok)
          && local_tokens == concurrent_tokens
          && matches!(local_tokens, Ok(Some(SemanticTokensResult::Tokens(tokens))) if tokens.data.len() == 2)
          && local_symbols == concurrent_symbols
          && local_symbols
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .is_some_and(|symbols| symbols.len() == 3)
          && local_folds == concurrent_folds
          && local_folds
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .is_some_and(|folds| folds.len() >= 3)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn document_projection_guards_suppress_disabled_features_and_missing_documents() -> Result<(), impl Debug> {
    let observation = block_on(async {
      let fixture = projection_fixture().await?;
      let missing_document = fixture_url("file:///workspace/missing.toml", "the missing projection document URL must parse")?;
      let tokens = document_params::<SemanticTokensParams>(&fixture.document)?;
      let symbols = document_params::<DocumentSymbolParams>(&missing_document)?;
      let disabled = json!({
        "schema": {
          "catalogs": []
        },
        "syntax": {
          "semanticTokens": false
        }
      });
      let configuration = fixture.local.apply_configuration_values_local(Some(&disabled), &[]).await;
      let disabled_tokens = semantic_tokens_local(&fixture.local, Params::from(Some(tokens))).await;
      let missing_symbols = document_symbols_local(&fixture.local, Params::from(Some(symbols))).await;
      Ok::<_, FixtureFailure>((fixture, disabled, configuration, disabled_tokens, missing_document, missing_symbols))
    });
    ensure_that(
      observation,
      "disabled semantic tokens and unopened document symbols remain absent successful responses",
      |subject| {
        let Ok(ref scenario) = *subject else {
          return false;
        };
        scenario.0.installed.iter().all(Result::is_ok)
          && scenario.2.is_ok()
          && matches!(scenario.3, Ok(None))
          && matches!(scenario.5, Ok(None))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }
}
