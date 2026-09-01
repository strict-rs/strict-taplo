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
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use taplo_common::schema::associations::AssociationRule;
  use taplo_common::schema::associations::SchemaAssociation;
  use taplo_common::schema::associations::priority;
  use taplo_common::schema::associations::source;
  use taplo_common::schema::transport::SchemaTransport;
  use taplo_common::schema::transport::local_http_client;
  use url::Url;

  use crate::LocalTestFuture;
  #[cfg(not(target_arch = "wasm32"))]
  use crate::world::ConcurrentWorld;
  use crate::world::DocumentSnapshot;
  use crate::world::DocumentState;
  use crate::world::LocalWorld;
  use crate::world::TestEnvironment;

  /// Canonical document and schema URLs for one schema-backed handler scenario.
  pub(super) struct SchemaFixture {
    /// Document URL presented through the protocol.
    pub(super) document:   Url,
    /// Schema URL installed in the world-owned schema service.
    pub(super) schema_url: Url,
  }

  /// Construct one fresh detached local world through the production crate façade.
  pub(super) fn local_world() -> Result<LocalWorld<TestEnvironment>, TestFailure> {
    let http = ensure_ok(local_http_client(), "the local handler-fixture HTTP client must construct")?;
    ensure_ok(
      crate::create_local_world(TestEnvironment::default(), http),
      "the local handler-fixture world must construct",
    )
  }

  /// Construct one fresh detached concurrent world through the production crate façade.
  #[cfg(not(target_arch = "wasm32"))]
  pub(super) fn concurrent_world() -> Result<ConcurrentWorld<TestEnvironment>, TestFailure> {
    let http = ensure_ok(local_http_client(), "the concurrent handler-fixture HTTP client must construct")?;
    ensure_ok(
      crate::create_concurrent_world(TestEnvironment::default(), http),
      "the concurrent handler-fixture world must construct",
    )
  }

  /// Parse one absolute handler-fixture URL with a behavior-specific failure context.
  pub(super) fn url(input: &str, context: &'static str) -> Result<Url, TestFailure> {
    ensure_ok(Url::parse(input), context)
  }

  /// Parse the complete URL identity for one schema-backed handler scenario.
  pub(super) fn schema_fixture(
    document: &str,
    schema_url: &str,
    document_context: &'static str,
    schema_context: &'static str,
  ) -> Result<SchemaFixture, TestFailure> {
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
  ) -> Result<P, TestFailure> {
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
  ) -> Result<P, TestFailure> {
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
  ) -> Result<P, TestFailure> {
    positioned_params(document, line, character, Some(new_name), context)
  }

  /// Install one source revision in a local handler world.
  pub(super) fn replace_local_document<'operation>(
    world: &'operation LocalWorld<TestEnvironment>,
    document: &'operation Url,
    source: &'operation str,
    context: &'static str,
  ) -> LocalTestFuture<'operation, ()> {
    Box::pin(async move {
      drop(ensure_ok(world.replace_document(document, source).await, context)?);
      Ok(())
    })
  }

  /// Install one source revision in a concurrent handler world.
  #[cfg(not(target_arch = "wasm32"))]
  pub(super) async fn replace_concurrent_document(
    world: &ConcurrentWorld<TestEnvironment>,
    document: &Url,
    source: &str,
    context: &'static str,
  ) -> Result<(), TestFailure> {
    drop(ensure_ok(world.replace_document_concurrent(document, source).await, context)?);
    Ok(())
  }

  /// Parse one immutable document fixture through the production document boundary.
  pub(super) fn parse_document(source: &str, context: &'static str) -> Result<DocumentState, TestFailure> {
    ensure_ok(DocumentState::parse(source), context)
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

  /// Commit one source replacement, then install its schema into the resulting snapshot.
  pub(super) async fn install_schema_document<T: SchemaTransport>(
    replacement: impl Future<Output = Result<(), TestFailure>>,
    pending_snapshot: impl Future<Output = Option<DocumentSnapshot<T>>>,
    document: &Url,
    schema_url: &Url,
    schema: Value,
    snapshot_context: &'static str,
  ) -> Result<(), TestFailure> {
    replacement.await?;
    let snapshot = ensure_some(pending_snapshot.await, snapshot_context)?;
    install_schema(&snapshot, document, schema_url, schema);
    Ok(())
  }

  /// Require one committed handler transition to emit no schema or diagnostic effects.
  pub(super) fn ensure_no_client_effects<A, D>(associations: &[A], diagnostics: &[D], context: &'static str) -> Result<(), TestFailure> {
    ensure((associations.len(), diagnostics.len()) == (0, 0), context)
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
  use std::future::Future;
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
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use taplo_common::schema::transport::LocalSchemaTransport;
  use taplo_common::schema::transport::local_http_client;
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
  use crate::LocalTestFuture;
  use crate::handlers::test_support::concurrent_world;
  use crate::handlers::test_support::local_world as handler_local_world;
  use crate::handlers::test_support::replace_concurrent_document;
  use crate::handlers::test_support::replace_local_document;
  use crate::handlers::test_support::url as fixture_url;
  use crate::world::ConcurrentWorld;
  use crate::world::LocalWorld;
  use crate::world::TestEnvironment;
  use crate::world::WorldState;

  /// Source shared by the direct document-projection handler scenarios.
  const PROJECTION_SOURCE: &str = "# first\n# second\nitems = [\n  1,\n  2,\n]\ninline = { nested = true }\n[table]\nvalue = 1\n";

  /// Open local and concurrent worlds for one document-projection scenario.
  struct ProjectionFixture {
    /// Canonical document URL installed in both worlds.
    document:   Url,
    /// Current-thread world holding the source.
    local:      LocalWorld<TestEnvironment>,
    /// Cross-thread world holding the same source.
    concurrent: ConcurrentWorld<TestEnvironment>,
  }

  /// Resolve one document projection through both generated handler families.
  async fn projection_pair<R>(
    pending_local: impl Future<Output = Result<Option<R>, RpcError>>,
    pending_concurrent: impl Future<Output = Result<Option<R>, RpcError>>,
    local_execution: &'static str,
    local_presence: &'static str,
    concurrent_execution: &'static str,
    concurrent_presence: &'static str,
  ) -> Result<(R, R), TestFailure> {
    let local = ensure_some(ensure_ok(pending_local.await, local_execution)?, local_presence)?;
    let concurrent = ensure_some(ensure_ok(pending_concurrent.await, concurrent_execution)?, concurrent_presence)?;
    Ok((local, concurrent))
  }

  /// Execute a complete set of document projections through both generated families.
  macro_rules! projection_pairs {
    ($fixture:ident; $(($params:ty, $local:path, $concurrent:path, $local_execution:literal, $local_presence:literal, $concurrent_execution:literal, $concurrent_presence:literal)),+ $(,)?) => {
      (
        $(
          projection_pair(
            $local(
              &$fixture.local,
              Params::from(Some(document_params::<$params>(&$fixture.document)?)),
            ),
            $concurrent(
              &$fixture.concurrent,
              Params::from(Some(document_params::<$params>(&$fixture.document)?)),
            ),
            $local_execution,
            $local_presence,
            $concurrent_execution,
            $concurrent_presence,
          ).await?
        ),+
      )
    };
  }

  /// Parse one handler fixture URL.
  fn document_url() -> Result<Url, TestFailure> {
    fixture_url("file:///workspace/document.toml", "the handler fixture URL must parse")
  }

  /// Construct one local handler world whose HTTP capability is not exercised.
  fn local_world() -> Result<WorldState<TestEnvironment, LocalSchemaTransport<TestEnvironment>>, TestFailure> {
    let environment = TestEnvironment::default();
    let client = ensure_ok(local_http_client(), "the local schema client must construct")?;
    let transport = LocalSchemaTransport::new(environment.clone(), client);
    ensure_ok(
      WorldState::with_transport(environment, transport),
      "the local handler world must construct",
    )
  }

  /// Decode one document-only request through its public wire shape.
  fn document_params<P: DeserializeOwned>(document: &Url) -> Result<P, TestFailure> {
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
  fn projection_fixture() -> LocalTestFuture<'static, ProjectionFixture> {
    Box::pin(async {
      let document = document_url()?;
      let local = handler_local_world()?;
      replace_local_document(&local, &document, PROJECTION_SOURCE, "the local projection document must install").await?;
      let concurrent = concurrent_world()?;
      replace_concurrent_document(
        &concurrent,
        &document,
        PROJECTION_SOURCE,
        "the concurrent projection document must install",
      )
      .await?;
      Ok(ProjectionFixture {
        document,
        local,
        concurrent,
      })
    })
  }

  #[test]
  fn generation_dependent_responses_accept_current_and_reject_stale_snapshots() -> Result<(), TestFailure> {
    block_on(async {
      let world = local_world()?;
      let document = document_url()?;
      drop(ensure_ok(
        world.replace_document(&document, "value = 1\n").await,
        "the initial handler document must install",
      )?);
      let snapshot = ensure_some(
        world.document_snapshot(&document).await,
        "the handler document must expose a snapshot",
      )?;
      let current = ensure_ok(
        current_snapshot_response(&world, &document, &snapshot, Some(7_u8)).await,
        "a response captured from current state must be accepted",
      )?;
      let response_payload = ensure_some(current, "snapshot validation must preserve a present response payload")?;
      ensure_eq(&response_payload, &7_u8, "snapshot validation must preserve the response payload")?;

      drop(ensure_ok(
        world.replace_document(&document, "value = 2\n").await,
        "the replacement handler document must install",
      )?);
      let stale = current_snapshot_response(&world, &document, &snapshot, Option::<u8>::None).await;
      let error = ensure_some(
        stale.err(),
        "even an empty response must be rejected after its snapshot becomes stale",
      )?;
      ensure_eq(
        &error,
        &RpcError::content_modified(),
        "stale handler output must use the standard LSP content-modified error",
      )
    })
  }

  #[test]
  fn document_snapshot_loader_rejects_unresolvable_inputs_and_returns_current_state() -> Result<(), TestFailure> {
    block_on(async {
      let world = local_world()?;
      let relative = ensure_ok(
        Uri::from_str("workspace/document.toml"),
        "the relative handler URI fixture must parse",
      )?;
      ensure(
        document_snapshot_for_uri(&world, &relative).await.is_none(),
        "a relative wire URI must not resolve to an internal document",
      )?;

      let document = document_url()?;
      let document_uri = ensure_some(uri::to_uri(&document), "the absolute handler document URL must map to a wire URI")?;
      ensure(
        document_snapshot_for_uri(&world, &document_uri).await.is_none(),
        "an absolute URI without an open document must not fabricate a snapshot",
      )?;

      drop(ensure_ok(
        world.replace_document(&document, "value = 1\n").await,
        "the handler document must install before snapshot loading",
      )?);
      let (resolved, snapshot) = ensure_some(
        document_snapshot_for_uri(&world, &document_uri).await,
        "an open absolute document must resolve to its immutable snapshot",
      )?;
      ensure_eq(&resolved, &document, "snapshot loading must preserve the canonical document URL")?;
      ensure(
        snapshot.document.mapper.all_range() == Range::new(Position::new(0, 0), Position::new(1, 0)),
        "snapshot loading must preserve the complete source coordinate range",
      )
    })
  }

  #[test]
  fn document_projection_handlers_match_across_execution_families() -> Result<(), TestFailure> {
    block_on(async {
      let fixture = projection_fixture().await?;
      let ((local_tokens, concurrent_tokens), (local_symbols, concurrent_symbols), (local_folds, concurrent_folds)) = projection_pairs!(
        fixture;
        (
          SemanticTokensParams,
          semantic_tokens_local,
          semantic_tokens_concurrent,
          "local semantic-token projection must execute",
          "enabled local semantic tokens must return a concrete response",
          "concurrent semantic-token projection must execute",
          "enabled concurrent semantic tokens must return a concrete response"
        ),
        (
          DocumentSymbolParams,
          document_symbols_local,
          document_symbols_concurrent,
          "local document-symbol projection must execute",
          "an open local document must return a concrete symbol collection",
          "concurrent document-symbol projection must execute",
          "an open concurrent document must return a concrete symbol collection"
        ),
        (
          FoldingRangeParams,
          folding_ranges_local,
          folding_ranges_concurrent,
          "local folding-range projection must execute",
          "an open local document must return a concrete folding-range collection",
          "concurrent folding-range projection must execute",
          "an open concurrent document must return a concrete folding-range collection"
        ),
      );
      ensure(
        local_tokens == concurrent_tokens,
        "local and concurrent semantic-token projections must be identical",
      )?;
      ensure(
        matches!(
          &local_tokens,
          SemanticTokensResult::Tokens(tokens) if tokens.data.len() == 2
        ),
        "array and inline-table keys must produce the two advertised custom semantic tokens",
      )?;
      ensure(
        (local_symbols.len(), &concurrent_symbols) == (3, &local_symbols),
        "local and concurrent symbols must preserve the same three root declarations and nested structure",
      )?;
      ensure(
        local_folds == concurrent_folds && local_folds.len() >= 3,
        "local and concurrent folding ranges must preserve every multiline comment, array, and table region",
      )
    })
  }

  #[test]
  fn document_projection_guards_suppress_disabled_features_and_missing_documents() -> Result<(), TestFailure> {
    block_on(async {
      let fixture = projection_fixture().await?;
      let disabled = json!({
        "schema": {
          "catalogs": []
        },
        "syntax": {
          "semanticTokens": false
        }
      });
      drop(ensure_ok(
        fixture.local.apply_configuration_values_local(Some(&disabled), &[]).await,
        "the semantic-token-disabled configuration must commit",
      )?);
      ensure(
        ensure_ok(
          semantic_tokens_local(
            &fixture.local,
            Params::from(Some(document_params::<SemanticTokensParams>(&fixture.document)?)),
          )
          .await,
          "disabled semantic-token projection must remain an absent success",
        )?
        .is_none(),
        "the semantic-token feature guard must suppress protocol output when disabled",
      )?;
      let missing_document = fixture_url("file:///workspace/missing.toml", "the missing projection document URL must parse")?;
      ensure(
        ensure_ok(
          document_symbols_local(
            &fixture.local,
            Params::from(Some(document_params::<DocumentSymbolParams>(&missing_document)?)),
          )
          .await,
          "document symbols for an unopened document must remain an absent success",
        )?
        .is_none(),
        "document-symbol projection must not fabricate state for an unopened document",
      )
    })
  }
}
