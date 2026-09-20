//! Schema-backed identifier and primitive hover presentation.

use std::sync::Arc;

use itertools::Itertools as _;
use lsp_types::Hover;
use lsp_types::HoverContents;
use lsp_types::HoverParams;
use lsp_types::MarkupContent;
use lsp_types::MarkupKind;
use serde_json::Value;
use taplo::dom::KeyOrIndex;
use taplo::dom::Keys;
use taplo::syntax::SyntaxKind;
use taplo::syntax::kind::BOOL;
use taplo::syntax::kind::DATE;
use taplo::syntax::kind::DATE_TIME_LOCAL;
use taplo::syntax::kind::DATE_TIME_OFFSET;
use taplo::syntax::kind::FLOAT;
use taplo::syntax::kind::IDENT;
use taplo::syntax::kind::INTEGER;
use taplo::syntax::kind::INTEGER_BIN;
use taplo::syntax::kind::INTEGER_HEX;
use taplo::syntax::kind::INTEGER_OCT;
use taplo::syntax::kind::MULTI_LINE_STRING;
use taplo::syntax::kind::MULTI_LINE_STRING_LITERAL;
use taplo::syntax::kind::STRING;
use taplo::syntax::kind::STRING_LITERAL;
use taplo::syntax::kind::TIME;
use taplo_common::schema::ext::SchemaExtensionError;
use taplo_common::schema::ext::schema_ext_of;
use taplo_lsp_async::Params;
use taplo_lsp_async::rpc::RpcError;
use url::Url;

use crate::query::Query;
use crate::query::lookup_keys;
use crate::world::DocumentSnapshot;
use crate::world::SchemaExecution as _;
use crate::world::WorldState;

/// Produce schema-backed hover content for one current identifier or primitive.
///
/// # Errors
///
/// Returns [`RpcError`] when parameters, coordinates, schema data, serialization, or snapshot
/// freshness cannot be validated.
macro_rules! define_hover_future_family {
  (
    $hover:ident,
    $hover_schemas:ident,
    $document_snapshot_for_uri:ident,
    $current_snapshot_response:ident;
    $future:ident,
    $environment:path,
    $transport:ident,
    $schema_execution:path;
    [$($value_bound:path),*]
  ) => {
    /// Produce schema-backed hover content for one current identifier or primitive.
    ///
    /// # Errors
    ///
    /// Returns [`RpcError`] when parameters, coordinates, schema data, serialization, or snapshot
    /// freshness cannot be validated.
    #[allow(clippy::single_call_fn, reason = "one hover entry point per execution family, registered exactly once by its runtime family")]
    pub(super) fn $hover<E: $environment>(
      world: &WorldState<E, $transport<E>>,
      params: Params<HoverParams>,
    ) -> $future<'_, Result<Option<Hover>, RpcError>> {
      Box::pin(async move {
        let parameters = params.required()?;
        current_document_snapshot!(
          super::$document_snapshot_for_uri(world, &parameters.text_document_position_params.text_document.uri) => (document_uri, snapshot)
        );
        let document = &snapshot.document;
        let offset = document
          .mapper
          .offset(parameters.text_document_position_params.position)
          .map_err(|error| super::uri::mapping_rpc_error(&error))?;
        let query = Query::at(&document.dom, offset);
        let Some(position) = query.first_matching(|position| position.syntax.kind() == IDENT || is_primitive(position.syntax.kind()))
        else {
          return super::$current_snapshot_response(world, &document_uri, &snapshot, None).await;
        };
        let Some(association) = snapshot.schemas.associations().association_for(&document_uri) else {
          return super::$current_snapshot_response(world, &document_uri, &snapshot, None).await;
        };
        let instance = serde_json::to_value(&document.dom).map_err(|error| RpcError::internal_error().with_details(error.to_string()))?;
        let Some(position_node) = position.dom_node.as_ref() else {
          return super::$current_snapshot_response(world, &document_uri, &snapshot, None).await;
        };
        let mut keys = position_node.0.clone();
        if query.header_key().is_some() {
          let Some(index) = Query::header_identifier_index(&position.syntax) else {
            return super::$current_snapshot_response(world, &document_uri, &snapshot, None).await;
          };
          keys = lookup_keys(document.dom.clone(), &Keys::new(keys.into_iter().take(index.saturating_add(1))));
        }
        let Some(node) = document.dom.path(&keys) else {
          return super::$current_snapshot_response(world, &document_uri, &snapshot, None).await;
        };
        let links_in_hover = !snapshot.config.schema.links;

        let content = if position.syntax.kind() == IDENT {
          keys = lookup_keys(document.dom.clone(), &keys);
          while matches!(keys.iter().last(), Some(KeyOrIndex::Index(_))) {
            keys = keys.skip_right(1);
          }
          let schemas = $hover_schemas(&snapshot, &association.url, &instance, &keys).await?;
          join_schema_documentation(&schemas, "\n\n", |schema| key_documentation(schema, links_in_hover))
            .map_err(|error| RpcError::internal_error().with_details(error.to_string()))?
        } else {
          let schemas = $hover_schemas(&snapshot, &association.url, &instance, &keys).await?;
          let primitive = serde_json::to_value(node).map_err(|error| RpcError::internal_error().with_details(error.to_string()))?;
          join_schema_documentation(&schemas, "\n", |schema| primitive_documentation(schema, &primitive, links_in_hover))
            .map_err(|error| RpcError::internal_error().with_details(error.to_string()))?
        };
        if content.is_empty() {
          return super::$current_snapshot_response(world, &document_uri, &snapshot, None).await;
        }
        let range = super::uri::to_lsp_range(&document.mapper, position.syntax.text_range())
          .map_err(|error| super::uri::mapping_rpc_error(&error))?;
        super::$current_snapshot_response(
          world,
          &document_uri,
          &snapshot,
          Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
              kind:  MarkupKind::Markdown,
              value: content,
            }),
            range:    Some(range),
          }),
        )
        .await
      })
    }

    /// Resolve hover schema candidates through the shared RPC error boundary.
    fn $hover_schemas<'operation, E: $environment>(
      snapshot: &'operation DocumentSnapshot<$transport<E>>,
      schema_url: &'operation Url,
      instance: &'operation Value,
      keys: &'operation Keys,
    ) -> $future<'operation, Result<Vec<(Keys, Arc<Value>)>, RpcError>> {
      Box::pin(async move {
        <$schema_execution>::schemas_at_path(&snapshot.schemas, schema_url, instance, keys)
          .await
          .map_err(|error| RpcError::internal_error().with_details(error.to_string()))
      })
    }
  };
}

define_response_document_handler_execution_families!(
  define_hover_future_family;
  (hover_local, hover_concurrent),
  (hover_schemas_local, hover_schemas_concurrent),
);

/// Render all available schema documentation in traversal order.
fn join_schema_documentation(
  schemas: &[(Keys, Arc<Value>)],
  separator: &str,
  mut render: impl FnMut(&Value) -> Result<Option<String>, SchemaExtensionError>,
) -> Result<String, SchemaExtensionError> {
  let mut documentation = Vec::new();
  for schema in schemas.iter().map(|schema_entry| &schema_entry.1) {
    if let Some(content) = render(schema)? {
      documentation.push(content);
    }
  }
  Ok(documentation.into_iter().join(separator))
}

/// Render key documentation with its optional embedded external link.
fn key_documentation(schema: &Value, links_in_hover: bool) -> Result<Option<String>, SchemaExtensionError> {
  let extension = schema_ext_of(schema)?.unwrap_or_default();
  let Some(mut content) = extension
    .docs
    .as_ref()
    .and_then(|docs| docs.main.clone())
    .or_else(|| schema["description"].as_str().map(ToOwned::to_owned))
  else {
    return Ok(None);
  };
  if content.is_empty() {
    return Ok(None);
  }
  if links_in_hover && let Some(link) = extension.links.and_then(|links| links.key) {
    let title = schema["title"].as_str().unwrap_or("...");
    content = format!("[{title}]({link})\n\n{content}");
  }
  Ok(Some(content))
}

/// Select primitive-value documentation in the established specialized-to-general precedence.
fn primitive_documentation(schema: &Value, instance: &Value, links_in_hover: bool) -> Result<Option<String>, SchemaExtensionError> {
  let extension = schema_ext_of(schema)?.unwrap_or_default();
  let docs = extension.docs.unwrap_or_default();
  let links = extension.links.unwrap_or_default();
  if let Some(index) = schema["enum"]
    .as_array()
    .and_then(|values| values.iter().position(|candidate| candidate == instance))
    && let Some(mut content) = docs
      .enum_values
      .as_ref()
      .and_then(|values| values.get(index))
      .cloned()
      .flatten()
  {
    if links_in_hover
      && let Some(link) = links
        .enum_values
        .as_ref()
        .and_then(|values| values.get(index))
        .and_then(Option::as_ref)
    {
      let title = schema["title"].as_str().unwrap_or("...");
      content = format!("[{title}]({link})\n\n{content}");
    }
    return Ok((!content.is_empty()).then_some(content));
  }
  if schema.get("default") == Some(instance)
    && let Some(content) = docs.default_value
  {
    return Ok((!content.is_empty()).then_some(content));
  }
  if schema.get("const") == Some(instance)
    && let Some(content) = docs.const_value
  {
    return Ok((!content.is_empty()).then_some(content));
  }
  Ok(
    docs
      .main
      .or_else(|| schema["description"].as_str().map(ToOwned::to_owned))
      .or_else(|| schema["title"].as_str().map(ToOwned::to_owned))
      .filter(|content| !content.is_empty()),
  )
}

/// Whether a syntax token represents a primitive TOML value.
const fn is_primitive(kind: SyntaxKind) -> bool {
  matches!(
    kind,
    BOOL
      | DATE
      | DATE_TIME_LOCAL
      | DATE_TIME_OFFSET
      | TIME
      | STRING
      | MULTI_LINE_STRING
      | STRING_LITERAL
      | MULTI_LINE_STRING_LITERAL
      | FLOAT
      | INTEGER
      | INTEGER_HEX
      | INTEGER_OCT
      | INTEGER_BIN
  )
}

#[cfg(test)]
mod tests {
  use std::fmt::Debug;

  use futures::executor::block_on;
  use lsp_types::Hover;
  use lsp_types::HoverContents;
  use lsp_types::HoverParams;
  use lsp_types::Position;
  use lsp_types::Range;
  use serde_json::json;
  use strict_test_support::ResultFailure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_that;
  use taplo::syntax::kind::FLOAT;
  use taplo::syntax::kind::IDENT;
  use taplo::syntax::kind::STRING;
  use taplo_common::schema::transport::LocalSchemaTransport;
  use taplo_lsp_async::Params;
  use taplo_lsp_async::rpc::RpcError;
  use url::Url;

  use super::hover_concurrent;
  use super::hover_local;
  use super::is_primitive;
  use super::key_documentation;
  use super::primitive_documentation;
  use crate::LocalFuture;
  use crate::handlers::test_support::FixtureFailure;
  use crate::handlers::test_support::SchemaFixture;
  use crate::handlers::test_support::SchemaInstallation;
  use crate::handlers::test_support::concurrent_world;
  use crate::handlers::test_support::install_schema_document;
  use crate::handlers::test_support::local_world;
  use crate::handlers::test_support::position_params;
  use crate::handlers::test_support::replace_concurrent_document;
  use crate::handlers::test_support::replace_local_document;
  use crate::handlers::test_support::schema_fixture;
  use crate::world::LocalWorld;
  use crate::world::TestEnvironment;

  /// Source shared by every schema-backed hover integration scenario.
  const HOVER_SOURCE: &str = "name = \"taplo\"\nempty = 1\n[table]\nenabled = true\n";

  /// Local world owner and its complete document/schema installation evidence.
  type PreparedHover = (
    LocalWorld<TestEnvironment>,
    SchemaInstallation<LocalSchemaTransport<TestEnvironment>>,
  );

  /// Native wire decoding and complete handler response for one hover request.
  type HoverRequest = Result<Result<Option<Hover>, RpcError>, ResultFailure<serde_json::Error>>;

  /// One exact schema-backed hover expectation retained beside its response.
  #[derive(Debug)]
  struct HoverExpectation {
    /// Zero-based source line.
    line:      u32,
    /// Zero-based UTF-16 source character.
    character: u32,
    /// Expected Markdown content.
    content:   &'static str,
    /// Exact selected range when the contract pins one.
    range:     Option<Range>,
  }

  /// Construct complete document and schema URL identities for the hover scenario.
  fn hover_fixture() -> Result<SchemaFixture, ResultFailure<url::ParseError>> {
    schema_fixture(
      "file:///workspace/hover.toml",
      "https://example.com/hover-schema.json",
      "the hover document URL must parse",
      "the hover schema URL must parse",
    )
  }

  /// Install one local hover document while retaining its world, transition and schema snapshot.
  fn prepared_local_hover_world(fixture: &SchemaFixture) -> LocalFuture<'_, Result<PreparedHover, FixtureFailure>> {
    Box::pin(async move {
      let world = local_world()?;
      let installation = install_schema_document(
        replace_local_document(&world, &fixture.document, HOVER_SOURCE, "the local hover document must install"),
        world.document_snapshot(&fixture.document),
        &fixture.document,
        &fixture.schema_url,
        hover_schema(),
      )
      .await;
      Ok((world, installation))
    })
  }

  /// Execute one local hover request while retaining wire decoding and native protocol results.
  fn local_hover_at<'operation>(
    world: &'operation LocalWorld<TestEnvironment>,
    document: &'operation Url,
    line: u32,
    character: u32,
  ) -> LocalFuture<'operation, HoverRequest> {
    Box::pin(async move {
      let parameters = position_params::<HoverParams>(document, line, character, "the hover request fixture must decode")?;
      Ok(hover_local(world, Params::from(Some(parameters))).await)
    })
  }

  /// Construct the complete schema shared by both hover execution families.
  fn hover_schema() -> serde_json::Value {
    json!({ "type": "object", "properties": {
      "name": { "title": "Name", "description": "name documentation", "type": "string" },
      "empty": { "type": "integer" },
      "table": { "description": "table documentation", "type": "object", "properties": {
        "enabled": { "type": "boolean", "default": true, "x-taplo": { "docs": { "defaultValue": "enabled-by-default documentation" } } }
      } }
    } })
  }

  /// Borrow observable Markdown and range without replacing the complete hover response.
  fn hover_observation(hover: &Hover) -> Option<(&str, Option<Range>)> {
    match hover.contents {
      HoverContents::Markup(ref markup) => Some((markup.value.as_str(), hover.range)),
      HoverContents::Scalar(_) | HoverContents::Array(_) => None,
    }
  }

  #[test]
  fn key_documentation_prefers_extensions_and_embeds_links_only_in_hover() -> Result<(), impl Debug> {
    let schema = json!({ "title": "Setting", "description": "schema description",
      "x-taplo": { "docs": { "main": "extension documentation" }, "links": { "key": "https://example.com/key" } } });
    let embedded = key_documentation(&schema, true);
    let standalone = key_documentation(&schema, false);
    let description = json!({ "description": "fallback description" });
    let fallback = key_documentation(&description, true);
    let empty_schema = json!({ "description": "" });
    let empty = key_documentation(&empty_schema, true);
    ensure_that(
      (schema, embedded, standalone, description, fallback, empty_schema, empty),
      "key hover must prefer extension docs, embed links only in hover mode, fall back to description and omit empty content",
      |observed| {
        observed.1.as_ref().ok().and_then(Option::as_ref).is_some_and(|text| {
          text.contains("[Setting](https://example.com/key)")
            && text.contains("extension documentation")
            && !text.contains("schema description")
        }) && observed
          .2
          .as_ref()
          .is_ok_and(|content| content.as_deref() == Some("extension documentation"))
          && observed
            .4
            .as_ref()
            .is_ok_and(|content| content.as_deref() == Some("fallback description"))
          && matches!(observed.6, Ok(None))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn primitive_enum_documentation_obeys_link_and_embedding_precedence() -> Result<(), impl Debug> {
    let schema = json!({ "title": "Value", "description": "description docs", "enum": [1, 2], "default": 2, "const": 2,
      "x-taplo": { "docs": { "main": "main docs", "enumValues": ["one docs", "two docs"], "defaultValue": "default docs", "constValue": "const docs" },
        "links": { "enumValues": [null, "https://example.com/two"] } } });
    let embedded = primitive_documentation(&schema, &json!(2), true);
    let standalone = primitive_documentation(&schema, &json!(2), false);
    ensure_that(
      (schema, embedded, standalone),
      "matching enum docs must precede default or const docs and embed their link only in hover mode",
      |observed| {
        observed.1.as_ref().is_ok_and(|content| {
          content
            .as_ref()
            .is_some_and(|text| text.contains("[Value](https://example.com/two)") && text.contains("two docs"))
        }) && observed.2.as_ref().is_ok_and(|content| content.as_deref() == Some("two docs"))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn primitive_documentation_falls_through_default_const_and_general_sources() -> Result<(), impl Debug> {
    let cases = [
      (json!({ "enum": [2], "default": 2, "const": 2, "x-taplo": { "docs": { "defaultValue": "default docs", "constValue": "const docs" } } }), json!(2), Some("default docs")),
      (json!({ "const": true, "x-taplo": { "docs": { "constValue": "const docs" } } }), json!(true), Some("const docs")),
      (json!({ "title": "Value", "description": "description docs", "x-taplo": { "docs": { "main": "main docs" } } }), json!(99), Some("main docs")),
      (json!({ "description": "description" }), json!(1), Some("description")),
      (json!({ "title": "title" }), json!(1), Some("title")),
      (json!({}), json!(1), None),
    ].map(|(schema, instance, expected)| {
      let actual = primitive_documentation(&schema, &instance, true);
      (schema, instance, expected, actual)
    });
    ensure_that(
      cases,
      "primitive hover must fall through enum, default, const, main, description and title without fabricating empty docs",
      |observed| {
        observed
          .iter()
          .all(|case| case.3.as_ref().is_ok_and(|content| content.as_deref() == case.2))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn primitive_selection_includes_floats_but_excludes_identifiers() -> Result<(), impl Debug> {
    ensure_eq(
      [is_primitive(FLOAT), is_primitive(STRING), is_primitive(IDENT)],
      [true, true, false],
      "float and string values must be hoverable while identifiers retain their distinct branch",
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn schema_backed_hover_projects_key_value_header_and_default_documentation() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let fixture = hover_fixture()?;
      let prepared = prepared_local_hover_world(&fixture).await?;
      let mut requests = Vec::new();
      for expectation in [
        HoverExpectation {
          line:      0,
          character: 1,
          content:   "name documentation",
          range:     Some(Range::new(Position::new(0, 0), Position::new(0, 4))),
        },
        HoverExpectation {
          line:      0,
          character: 9,
          content:   "name documentation",
          range:     None,
        },
        HoverExpectation {
          line:      2,
          character: 2,
          content:   "table documentation",
          range:     Some(Range::new(Position::new(2, 1), Position::new(2, 6))),
        },
        HoverExpectation {
          line:      3,
          character: 11,
          content:   "enabled-by-default documentation",
          range:     None,
        },
      ] {
        let response = local_hover_at(&prepared.0, &fixture.document, expectation.line, expectation.character).await;
        requests.push((expectation, response));
      }
      Ok::<_, FixtureFailure>((fixture, prepared, requests))
    });
    let matches_expectation = |request: &(HoverExpectation, HoverRequest)| {
      let Ok(Ok(Some(ref content))) = request.1 else {
        return false;
      };
      hover_observation(content)
        .is_some_and(|hover| hover.0 == request.0.content && request.0.range.is_none_or(|range| hover.1 == Some(range)))
    };
    ensure_that(
      observed,
      "schema hover must preserve key, primitive, table and specialized default documentation with exact selected ranges",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        scenario.1.1.0.is_ok() && scenario.1.1.1.is_some() && scenario.2.iter().all(matches_expectation)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn array_table_header_hover_resolves_the_schema_path_without_runtime_indices() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let fixture = schema_fixture(
        "file:///workspace/array-hover.toml",
        "https://example.com/array-hover-schema.json",
        "the array-hover document URL must parse",
        "the array-hover schema URL must parse",
      )?;
      let world = local_world()?;
      let installed = install_schema_document(
        replace_local_document(
          &world,
          &fixture.document,
          "[[products]]\nname = \"first\"\n",
          "the array-hover document must install",
        ),
        world.document_snapshot(&fixture.document),
        &fixture.document,
        &fixture.schema_url,
        json!({ "type": "object", "properties": { "products": { "description": "product collection documentation", "type": "array",
          "items": { "type": "object", "properties": { "name": { "type": "string" } } } } } }),
      )
      .await;
      let response = local_hover_at(&world, &fixture.document, 0, 3).await;
      Ok::<_, FixtureFailure>((fixture, world, installed, response))
    });
    ensure_that(
      observed,
      "array-table hover must remove runtime item indices and retain documentation with the exact header identifier range",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        scenario.2.0.is_ok()
          && scenario.2.1.is_some()
          && scenario
            .3
            .as_ref()
            .ok()
            .and_then(|response| response.as_ref().ok())
            .and_then(Option::as_ref)
            .and_then(hover_observation)
            == Some((
              "product collection documentation",
              Some(Range::new(Position::new(0, 2), Position::new(0, 10))),
            ))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn hover_rejects_missing_parameters_and_omits_unsupported_or_unassociated_targets() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let fixture = hover_fixture()?;
      let unassociated = local_world()?;
      let prepared = prepared_local_hover_world(&fixture).await?;
      let mut absent = Vec::new();
      for position in [(1, 1), (0, 5)] {
        let response = local_hover_at(&prepared.0, &fixture.document, position.0, position.1).await;
        absent.push((position, response));
      }
      let installed = replace_local_document(
        &unassociated,
        &fixture.document,
        HOVER_SOURCE,
        "the unassociated hover document must install",
      )
      .await;
      let unassociated_response = local_hover_at(&unassociated, &fixture.document, 0, 1).await;
      let rejected = hover_local(&prepared.0, Params::<HoverParams>::from(None)).await;
      Ok::<_, FixtureFailure>((fixture, prepared, absent, unassociated, installed, unassociated_response, rejected))
    });
    ensure_that(
      observed,
      "unsupported, undocumented and unassociated hover targets must remain absent while missing parameters retain their typed error",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        scenario.1.1.0.is_ok()
          && scenario.1.1.1.is_some()
          && scenario.2.iter().all(|request| matches!(request.1, Ok(Ok(None))))
          && scenario.4.is_ok()
          && matches!(scenario.5, Ok(Ok(None)))
          && scenario.6.as_ref().is_err_and(|error| error.code == -32602)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn hover_execution_families_preserve_observable_output() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let fixture = hover_fixture()?;
      let concurrent = concurrent_world()?;
      let concurrent_params = position_params::<HoverParams>(&fixture.document, 0, 1, "the concurrent hover request fixture must decode")?;
      let prepared = prepared_local_hover_world(&fixture).await?;
      let local = local_hover_at(&prepared.0, &fixture.document, 0, 1).await;
      let installed = install_schema_document(
        replace_concurrent_document(
          &concurrent,
          &fixture.document,
          HOVER_SOURCE,
          "the concurrent hover document must install",
        ),
        concurrent.document_snapshot_concurrent(&fixture.document),
        &fixture.document,
        &fixture.schema_url,
        hover_schema(),
      )
      .await;
      let response = hover_concurrent(&concurrent, Params::from(Some(concurrent_params))).await;
      Ok::<_, FixtureFailure>((fixture, prepared, local, concurrent, installed, response))
    });
    ensure_that(
      observed,
      "both hover execution families must retain identical complete content and range",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        let Ok(Ok(Some(ref local))) = scenario.2 else {
          return false;
        };
        let Ok(Some(ref concurrent)) = scenario.5 else {
          return false;
        };
        scenario.1.1.0.is_ok()
          && scenario.1.1.1.is_some()
          && scenario.4.0.is_ok()
          && scenario.4.1.is_some()
          && hover_observation(local)
            .zip(hover_observation(concurrent))
            .is_some_and(|(left, right)| left == right)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }
}
