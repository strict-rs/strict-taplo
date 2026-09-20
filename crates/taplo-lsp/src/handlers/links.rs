//! Standalone schema documentation links over immutable document snapshots.

use lsp_types::DocumentLink;
use lsp_types::DocumentLinkParams;
use serde_json::Value;
use taplo::dom::KeyOrIndex;
use taplo::dom::node::Key;
use taplo_common::schema::ext::SchemaExtensionError;
use taplo_common::schema::ext::schema_ext_of;
use taplo_lsp_async::Params;
use taplo_lsp_async::rpc::RpcError;
use taplo_lsp_async::util::Mapper;
use taplo_lsp_async::util::MappingError;
use thiserror::Error as ThisError;
use url::Url;

use crate::world::SchemaExecution as _;
use crate::world::WorldState;

/// A failed schema documentation-link projection.
#[derive(Debug, ThisError)]
enum LinkError {
  /// Taplo-specific schema metadata is malformed.
  #[error(transparent)]
  SchemaExtension(#[from] SchemaExtensionError),
  /// A schema link target is not an absolute URL.
  #[error("invalid schema documentation link target `{target}`")]
  InvalidTarget {
    /// Rejected target.
    target: String,
    /// Underlying URL failure.
    #[source]
    source: url::ParseError,
  },
  /// An absolute URL cannot be represented by the LSP URI type.
  #[error("schema documentation link target `{url}` is not representable by LSP")]
  UnsupportedTarget {
    /// Rejected absolute URL.
    url: Url,
  },
  /// A source range cannot be represented in LSP coordinates.
  #[error(transparent)]
  Mapping(#[from] MappingError),
}

/// Produce standalone schema documentation links for one current document.
///
/// # Errors
///
/// Returns [`RpcError`] when parameters, schema data, URL conversion, source coordinates, or
/// snapshot freshness cannot be validated.
macro_rules! define_link_future_family {
  (
    $links:ident,
    $document_snapshot_for_uri:ident,
    $current_snapshot_response:ident;
    $future:ident,
    $environment:path,
    $transport:ident,
    $schema_execution:path;
    [$($value_bound:path),*]
  ) => {
    /// Produce standalone schema documentation links for one current document.
    ///
    /// # Errors
    ///
    /// Returns [`RpcError`] when parameters, schema data, URL conversion, source coordinates, or
    /// snapshot freshness cannot be validated.
    #[allow(clippy::single_call_fn, reason = "one document-link entry point per execution family, registered exactly once by its runtime family")]
    pub(super) fn $links<E: $environment>(
      world: &WorldState<E, $transport<E>>,
      params: Params<DocumentLinkParams>,
    ) -> $future<'_, Result<Option<Vec<DocumentLink>>, RpcError>> {
      Box::pin(async move {
        let parameters = params.required()?;
        current_document_snapshot!(
          super::$document_snapshot_for_uri(world, &parameters.text_document.uri) => (document_uri, snapshot)
        );
        if !snapshot.config.schema.enabled || !snapshot.config.schema.links {
          return super::$current_snapshot_response(world, &document_uri, &snapshot, None).await;
        }
        let Some(association) = snapshot.schemas.associations().association_for(&document_uri) else {
          return super::$current_snapshot_response(world, &document_uri, &snapshot, Some(Vec::new())).await;
        };

        let mut links = Vec::new();
        for (keys, last_key, node) in snapshot
          .document
          .dom
          .flat_iter()
          .filter_map(|(keys, node)| match keys.iter().last().cloned() {
            Some(KeyOrIndex::Key(last_key)) => Some((keys, last_key, node)),
            _ => None,
          })
        {
          let instance = serde_json::to_value(&node).map_err(|error| RpcError::internal_error().with_details(error.to_string()))?;
          let schemas = <$schema_execution>::schemas_at_path(&snapshot.schemas, &association.url, &instance, &keys)
            .await
            .map_err(|error| RpcError::internal_error().with_details(error.to_string()))?;
          for (_, schema) in schemas {
            links.extend(
              key_document_links(&schema, &last_key, &snapshot.document.mapper)
                .map_err(|error| RpcError::internal_error().with_details(error.to_string()))?,
            );
          }
        }
        super::$current_snapshot_response(world, &document_uri, &snapshot, Some(links)).await
      })
    }
  };
}

define_response_document_handler_execution_families!(
  define_link_future_family;
  (links_local, links_concurrent),
);

/// Build standalone links for every mappable source occurrence of one schema-backed key.
fn key_document_links(schema: &Value, key: &Key, mapper: &Mapper) -> Result<Vec<DocumentLink>, LinkError> {
  let Some(key_link) = schema_ext_of(schema)?
    .and_then(|extension| extension.links)
    .and_then(|external| external.key)
  else {
    return Ok(Vec::new());
  };
  let target_url = Url::parse(&key_link).map_err(|source| LinkError::InvalidTarget {
    target: key_link,
    source,
  })?;
  let target = super::uri::to_uri(&target_url).ok_or(LinkError::UnsupportedTarget {
    url: target_url
  })?;
  key
    .text_ranges()
    .map(|source_range| {
      Ok(DocumentLink {
        range:   super::uri::to_lsp_range(mapper, source_range)?,
        target:  Some(target.clone()),
        tooltip: None,
        data:    None,
      })
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use std::fmt::Debug;

  use futures::executor::block_on;
  use lsp_types::DocumentLink;
  use lsp_types::DocumentLinkParams;
  use lsp_types::Position;
  use lsp_types::Range;
  use serde_json::json;
  use strict_test_support::ResultFailure;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_that;
  use taplo::dom::KeyOrIndex;
  use taplo::dom::Keys;
  use taplo::parser;
  use taplo_common::schema::transport::LocalSchemaTransport;
  use taplo_lsp_async::Params;
  use taplo_lsp_async::rpc::RpcError;
  use taplo_lsp_async::util::Mapper;
  use url::Url;

  use super::key_document_links;
  use super::links_concurrent;
  use super::links_local;
  use crate::LocalFuture;
  use crate::handlers::test_support::FixtureFailure;
  use crate::handlers::test_support::SchemaFixture;
  use crate::handlers::test_support::concurrent_world;
  use crate::handlers::test_support::install_schema_document;
  use crate::handlers::test_support::local_world;
  use crate::handlers::test_support::replace_concurrent_document;
  use crate::handlers::test_support::replace_local_document;
  use crate::handlers::test_support::schema_fixture;
  use crate::lsp_ext::notification::DidChangeSchemaAssociationParams;
  use crate::world::DocumentSnapshot;
  use crate::world::DocumentUpdate;
  use crate::world::LocalWorld;
  use crate::world::TestEnvironment;
  use crate::world::WorldError;

  /// Source shared by each standalone document-link handler scenario.
  const LINK_SOURCE: &str = "setting = 1\nother = 2\n";

  /// Construct the complete URL identity of a standalone document-link scenario.
  fn link_fixture() -> Result<SchemaFixture, ResultFailure<url::ParseError>> {
    schema_fixture(
      "file:///workspace/links.toml",
      "https://example.com/link-schema.json",
      "the document-link URL must parse",
      "the document-link schema URL must parse",
    )
  }

  /// Construct the client policy that enables standalone schema links.
  fn link_configuration() -> serde_json::Value {
    json!({ "schema": { "enabled": true, "links": true, "catalogs": [] } })
  }

  /// Construct the schema that documents exactly one fixture property.
  fn link_schema() -> serde_json::Value {
    json!({ "type": "object", "properties": {
      "setting": { "type": "integer", "x-taplo": { "links": { "key": "https://example.com/docs/setting" } } },
      "other": { "type": "integer" }
    } })
  }

  /// Decode one document-link request through its public wire shape.
  fn link_params(document: &Url) -> Result<DocumentLinkParams, ResultFailure<serde_json::Error>> {
    ensure_ok(
      serde_json::from_value(json!({ "textDocument": { "uri": document.as_str() } })),
      "the document-link request fixture must decode",
    )
  }

  /// Native world, configuration, schema installation, and response of one link projection.
  type LinkObservation<Owner, Transport> = (
    Owner,
    Result<Vec<DidChangeSchemaAssociationParams>, WorldError>,
    (
      Result<DocumentUpdate, Box<ResultFailure<WorldError>>>,
      Option<DocumentSnapshot<Transport>>,
    ),
    Result<Option<Vec<DocumentLink>>, RpcError>,
  );

  /// Current-thread link projection retaining its concrete transport and world.
  type LocalLinkObservation = LinkObservation<LocalWorld<TestEnvironment>, LocalSchemaTransport<TestEnvironment>>;

  /// Execute a local link projection and return all setup, mutation and protocol evidence.
  fn configured_local_links(fixture: &SchemaFixture) -> LocalFuture<'_, Result<LocalLinkObservation, FixtureFailure>> {
    Box::pin(async move {
      let world = local_world()?;
      let parameters = link_params(&fixture.document)?;
      let configured = world.apply_configuration_values_local(Some(&link_configuration()), &[]).await;
      let installed = install_schema_document(
        replace_local_document(
          &world,
          &fixture.document,
          LINK_SOURCE,
          "the configured local document-link source must install",
        ),
        world.document_snapshot(&fixture.document),
        &fixture.document,
        &fixture.schema_url,
        link_schema(),
      )
      .await;
      let response = links_local(&world, Params::from(Some(parameters))).await;
      Ok((world, configured, installed, response))
    })
  }

  #[test]
  fn standalone_links_require_valid_targets_and_mappable_ranges() -> Result<(), impl Debug> {
    let source = "setting = 1\n";
    let parsed = parser::parse(source);
    let keys = "setting".parse::<Keys>();
    let located = parsed.as_ref().ok().zip(keys.as_ref().ok()).map(|(document, path)| {
      let dom = document.clone().into_dom();
      let key = path.iter().find_map(KeyOrIndex::as_key).cloned();
      let selected = dom.path(path);
      (dom, key, selected)
    });
    let mapper = Mapper::new_utf16(source);
    let empty_mapper = Mapper::new_utf16("");
    let schema = json!({ "x-taplo": { "links": { "key": "https://example.com/docs" } } });
    let projections = located
      .as_ref()
      .and_then(|location| location.1.as_ref())
      .zip(mapper.as_ref().ok())
      .zip(empty_mapper.as_ref().ok())
      .map(|((key, coordinates), empty)| {
        (
          key_document_links(&schema, key, coordinates),
          key_document_links(&json!({ "x-taplo": { "links": { "key": "not a URL" } } }), key, coordinates),
          key_document_links(&schema, key, empty),
          key_document_links(&json!({ "description": "no standalone link" }), key, coordinates),
        )
      });
    ensure_that(
      (parsed, keys, located, mapper, empty_mapper, schema, projections),
      "standalone links must retain exact source and target, reject invalid targets or ranges, and omit schemas without link metadata",
      |observed| {
        let Some(ref links) = observed.6 else {
          return false;
        };
        let Ok(ref projected) = links.0 else {
          return false;
        };
        observed.2.as_ref().is_some_and(|location| location.2.is_some())
          && projected.first().is_some_and(|link| {
            link.range == Range::new(Position::new(0, 0), Position::new(0, 7))
              && link
                .target
                .as_ref()
                .is_some_and(|target| target.as_str() == "https://example.com/docs")
          })
          && links.1.is_err()
          && links.2.is_err()
          && links.3.as_ref().is_ok_and(Vec::is_empty)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn document_links_remain_disabled_by_default_and_reject_missing_parameters() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let fixture = link_fixture()?;
      let local = local_world()?;
      let parameters = link_params(&fixture.document)?;
      let installed = replace_local_document(
        &local,
        &fixture.document,
        LINK_SOURCE,
        "the local document-link source must install",
      )
      .await;
      let disabled = links_local(&local, Params::from(Some(parameters))).await;
      let rejected = links_local(&local, Params::<DocumentLinkParams>::from(None)).await;
      Ok::<_, FixtureFailure>((fixture, local, installed, disabled, rejected))
    });
    ensure_that(
      observed,
      "default-disabled links must return absence and missing parameters must retain the typed invalid-params response",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        scenario.2.is_ok()
          && matches!(scenario.3, Ok(None))
          && scenario
            .4
            .as_ref()
            .is_err_and(|error| error.code == -32602 && error.details.is_some())
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn document_links_require_an_association_and_preserve_exact_protocol_fields() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let fixture = link_fixture()?;
      let unassociated = local_world()?;
      let parameters = link_params(&fixture.document)?;
      let configured = unassociated
        .apply_configuration_values_local(Some(&link_configuration()), &[])
        .await;
      let installed = replace_local_document(
        &unassociated,
        &fixture.document,
        LINK_SOURCE,
        "the unassociated document-link source must install",
      )
      .await;
      let absent = links_local(&unassociated, Params::from(Some(parameters))).await;
      let associated = configured_local_links(&fixture).await;
      Ok::<_, FixtureFailure>((fixture, unassociated, configured, installed, absent, associated))
    });
    ensure_that(
      observed,
      "links require an association and must preserve exactly one documented key range and target",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        let Ok(ref associated) = scenario.5 else {
          return false;
        };
        let Ok(Some(ref links)) = associated.3 else {
          return false;
        };
        scenario.2.is_ok()
          && scenario.3.is_ok()
          && scenario
            .4
            .as_ref()
            .is_ok_and(|response| response.as_ref().is_some_and(Vec::is_empty))
          && associated.1.is_ok()
          && associated.2.0.is_ok()
          && associated.2.1.is_some()
          && links.len() == 1
          && links.first().is_some_and(|link| {
            link.range == Range::new(Position::new(0, 0), Position::new(0, 7))
              && link
                .target
                .as_ref()
                .is_some_and(|target| target.as_str() == "https://example.com/docs/setting")
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn document_link_execution_families_preserve_identical_protocol_output() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let fixture = link_fixture()?;
      let concurrent = concurrent_world()?;
      let parameters = link_params(&fixture.document)?;
      let local = configured_local_links(&fixture).await;
      let configured = concurrent
        .apply_configuration_values_concurrent(Some(&link_configuration()), &[])
        .await;
      let installed = install_schema_document(
        replace_concurrent_document(
          &concurrent,
          &fixture.document,
          LINK_SOURCE,
          "the concurrent document-link source must install",
        ),
        concurrent.document_snapshot_concurrent(&fixture.document),
        &fixture.document,
        &fixture.schema_url,
        link_schema(),
      )
      .await;
      let response = links_concurrent(&concurrent, Params::from(Some(parameters))).await;
      Ok::<_, FixtureFailure>((fixture, local, (concurrent, configured, installed, response)))
    });
    ensure_that(
      observed,
      "local and concurrent standalone links must preserve identical complete protocol output",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        let Ok(ref local) = scenario.1 else {
          return false;
        };
        local.1.is_ok()
          && local.2.0.is_ok()
          && local.2.1.is_some()
          && scenario.2.1.is_ok()
          && scenario.2.2.0.is_ok()
          && scenario.2.2.1.is_some()
          && local.3.as_ref().is_ok_and(Option::is_some)
          && local.3 == scenario.2.3
      },
    )
    .map(drop)
    .map_err(Box::new)
  }
}
