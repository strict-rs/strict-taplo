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
  use futures::executor::block_on;
  use lsp_types::DocumentLink;
  use lsp_types::DocumentLinkParams;
  use lsp_types::Position;
  use lsp_types::Range;
  use serde_json::json;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use taplo::dom::KeyOrIndex;
  use taplo::dom::Keys;
  use taplo::dom::error::QueryError;
  use taplo::dom::node::Key;
  use taplo::parser;
  use taplo_lsp_async::Params;
  use taplo_lsp_async::util::Mapper;
  use url::Url;

  use super::key_document_links;
  use super::links_concurrent;
  use super::links_local;
  use crate::LocalTestFuture;
  use crate::handlers::test_support::SchemaFixture;
  use crate::handlers::test_support::concurrent_world;
  use crate::handlers::test_support::install_schema_document;
  use crate::handlers::test_support::local_world;
  use crate::handlers::test_support::replace_concurrent_document;
  use crate::handlers::test_support::replace_local_document;
  use crate::handlers::test_support::schema_fixture;
  use crate::world::LocalWorld;
  use crate::world::TestEnvironment;

  /// Source shared by each standalone document-link handler scenario.
  const LINK_SOURCE: &str = "setting = 1\nother = 2\n";

  /// Construct one standalone document-link scenario without retaining transport state.
  fn link_fixture() -> Result<SchemaFixture, TestFailure> {
    schema_fixture(
      "file:///workspace/links.toml",
      "https://example.com/link-schema.json",
      "the document-link URL must parse",
      "the document-link schema URL must parse",
    )
  }

  /// Construct the client policy that enables standalone schema links.
  fn link_configuration() -> serde_json::Value {
    json!({
      "schema": {
        "enabled": true,
        "links": true,
        "catalogs": []
      }
    })
  }

  /// Construct the schema that documents exactly one fixture property.
  fn link_schema() -> serde_json::Value {
    json!({
      "type": "object",
      "properties": {
        "setting": {
          "type": "integer",
          "x-taplo": {
            "links": {
              "key": "https://example.com/docs/setting"
            }
          }
        },
        "other": {
          "type": "integer"
        }
      }
    })
  }

  /// Enable standalone schema links in one local handler world.
  fn enable_local_links(world: &LocalWorld<TestEnvironment>) -> LocalTestFuture<'_, ()> {
    Box::pin(async move {
      drop(ensure_ok(
        world.apply_configuration_values_local(Some(&link_configuration()), &[]).await,
        "the local document-link policy must commit",
      )?);
      Ok(())
    })
  }

  /// Execute one fully configured local link projection.
  fn configured_local_links(fixture: &SchemaFixture) -> LocalTestFuture<'_, Vec<DocumentLink>> {
    Box::pin(async move {
      let world = local_world()?;
      enable_local_links(&world).await?;
      install_schema_document(
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
        "the configured local document must expose a snapshot",
      )
      .await?;
      ensure_some(
        ensure_ok(
          links_local(&world, Params::from(Some(link_params(&fixture.document)?))).await,
          "configured local standalone-link generation must execute",
        )?,
        "enabled local standalone links must return a concrete collection",
      )
    })
  }

  /// Decode one document-link request through its public wire shape.
  fn link_params(document: &Url) -> Result<DocumentLinkParams, TestFailure> {
    ensure_ok(
      serde_json::from_value(json!({
        "textDocument": {
          "uri": document.as_str()
        }
      })),
      "the document-link request fixture must decode",
    )
  }

  /// Extract the first real key from one parsed document path.
  #[allow(
    clippy::single_call_fn,
    reason = "the named fixture keeps the parse, path decode, and DOM existence check together, so link ranges are asserted against a key \
              that provably exists in the fixture tree"
  )]
  fn key(source: &str, path: &str) -> Result<Key, TestFailure> {
    let dom = ensure_ok(parser::parse(source), "the document-link fixture tree must build")?.into_dom();
    let keys: Keys = path.parse().map_err(|error: QueryError| TestFailure::WasErr {
      context: "the document-link fixture path must parse",
      cause:   error.to_string(),
    })?;
    ensure_some(
      keys.iter().find_map(KeyOrIndex::as_key).cloned(),
      "the document-link fixture must contain a key",
    )
    .and_then(|parsed_key| {
      ensure(dom.path(&keys).is_some(), "the document-link fixture path must exist")?;
      Ok(parsed_key)
    })
  }

  #[test]
  fn standalone_links_require_valid_targets_and_mappable_ranges() -> Result<(), TestFailure> {
    let source = "setting = 1\n";
    let key = key(source, "setting")?;
    let schema = json!({
        "x-taplo": { "links": { "key": "https://example.com/docs" } }
    });
    let mapper = ensure_ok(Mapper::new_utf16(source), "the document-link fixture mapper must build")?;
    let links = ensure_ok(
      key_document_links(&schema, &key, &mapper),
      "a valid schema documentation link must be projected",
    )?;
    let link = ensure_some(links.first(), "a valid standalone link must be emitted")?;
    ensure(
      link.range == Range::new(Position::new(0, 0), Position::new(0, 7)),
      "standalone link range must cover the exact key source",
    )?;
    ensure(
      link
        .target
        .as_ref()
        .is_some_and(|target| target.as_str() == "https://example.com/docs"),
      "standalone link target must retain the schema URL",
    )?;

    let invalid_target = key_document_links(
      &json!({
          "x-taplo": { "links": { "key": "not a URL" } }
      }),
      &key,
      &mapper,
    );
    ensure(invalid_target.is_err(), "an invalid target URL must not be silently skipped")?;
    let empty_mapper = ensure_ok(Mapper::new_utf16(""), "the empty document-link fixture mapper must build")?;
    let unmappable = key_document_links(&schema, &key, &empty_mapper);
    ensure(
      unmappable.is_err(),
      "an unmappable key range must return its typed projection error",
    )?;
    let absent = ensure_ok(
      key_document_links(&json!({ "description": "no standalone link" }), &key, &mapper),
      "a schema without link metadata must remain a successful empty projection",
    )?;
    ensure(absent.is_empty(), "a schema without an external key link must emit nothing")
  }

  #[test]
  fn document_links_remain_disabled_by_default_and_reject_missing_parameters() -> Result<(), TestFailure> {
    block_on(async {
      let fixture = link_fixture()?;
      let local = local_world()?;
      replace_local_document(
        &local,
        &fixture.document,
        LINK_SOURCE,
        "the local document-link source must install",
      )
      .await?;
      ensure(
        ensure_ok(
          links_local(&local, Params::from(Some(link_params(&fixture.document)?))).await,
          "disabled standalone links must remain an absent success",
        )?
        .is_none(),
        "the default link-disabled policy must not emit standalone schema links",
      )?;
      let missing_params = ensure_some(
        links_local(&local, Params::<DocumentLinkParams>::from(None)).await.err(),
        "document links without parameters must return a typed invalid-params error",
      )?;
      ensure(
        (missing_params.code, missing_params.details.is_some()) == (-32602, true),
        "document links without parameters must retain the standard typed invalid-params response",
      )
    })
  }

  #[test]
  fn document_links_require_an_association_and_preserve_exact_protocol_fields() -> Result<(), TestFailure> {
    block_on(async {
      let fixture = link_fixture()?;
      let unassociated = local_world()?;
      enable_local_links(&unassociated).await?;
      replace_local_document(
        &unassociated,
        &fixture.document,
        LINK_SOURCE,
        "the unassociated document-link source must install",
      )
      .await?;
      let unassociated_links = ensure_some(
        ensure_ok(
          links_local(&unassociated, Params::from(Some(link_params(&fixture.document)?))).await,
          "unassociated document-link generation must execute",
        )?,
        "enabled links without an association must return an empty collection",
      )?;
      ensure(
        unassociated_links.is_empty(),
        "an unassociated document must not fabricate schema documentation links",
      )?;

      let local_links = configured_local_links(&fixture).await?;
      let local_link = ensure_some(local_links.first(), "the linked schema property must emit one standalone link")?;
      ensure(
        (
          local_links.len(),
          local_link.range,
          local_link.target.as_ref().map(|target| target.as_str()),
        ) == (
          1,
          Range::new(Position::new(0, 0), Position::new(0, 7)),
          Some("https://example.com/docs/setting"),
        ),
        "standalone-link generation must emit only documented keys with exact ranges and targets",
      )
    })
  }

  #[test]
  fn document_link_execution_families_preserve_identical_protocol_output() -> Result<(), TestFailure> {
    block_on(async {
      let fixture = link_fixture()?;
      let local_links = configured_local_links(&fixture).await?;
      let concurrent = concurrent_world()?;
      drop(ensure_ok(
        concurrent
          .apply_configuration_values_concurrent(Some(&link_configuration()), &[])
          .await,
        "the concurrent document-link configuration must commit",
      )?);
      install_schema_document(
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
        "the configured concurrent document must expose a snapshot",
      )
      .await?;
      let concurrent_links = ensure_some(
        ensure_ok(
          links_concurrent(&concurrent, Params::from(Some(link_params(&fixture.document)?))).await,
          "concurrent standalone-link generation must execute",
        )?,
        "enabled concurrent standalone links must return a concrete collection",
      )?;
      ensure(
        concurrent_links == local_links,
        "local and concurrent standalone-link families must preserve identical protocol output",
      )
    })
  }
}
