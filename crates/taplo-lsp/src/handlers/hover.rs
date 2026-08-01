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
  use futures::executor::block_on;
  use lsp_types::Hover;
  use lsp_types::HoverContents;
  use lsp_types::HoverParams;
  use lsp_types::Position;
  use lsp_types::Range;
  use serde_json::json;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_contains;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_lacks;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use taplo::syntax::kind::FLOAT;
  use taplo::syntax::kind::IDENT;
  use taplo::syntax::kind::STRING;
  use taplo_lsp_async::Params;
  use url::Url;

  use super::hover_concurrent;
  use super::hover_local;
  use super::is_primitive;
  use super::key_documentation;
  use super::primitive_documentation;
  use crate::handlers::test_support::SchemaFixture;
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

  /// One exact schema-backed hover expectation.
  struct HoverExpectation {
    /// Zero-based source line.
    line:              u32,
    /// Zero-based UTF-16 source character.
    character:         u32,
    /// Request execution failure context.
    execution_context: &'static str,
    /// Missing-response failure context.
    presence_context:  &'static str,
    /// Expected Markdown content.
    content:           &'static str,
    /// Exact selected range when the contract pins one.
    range:             Option<Range>,
    /// Observable-content failure context.
    assertion_context: &'static str,
  }

  /// Construct one schema-backed hover scenario without retaining transport state.
  fn hover_fixture() -> Result<SchemaFixture, TestFailure> {
    schema_fixture(
      "file:///workspace/hover.toml",
      "https://example.com/hover-schema.json",
      "the hover document URL must parse",
      "the hover schema URL must parse",
    )
  }

  /// Install one local hover document together with its exact manual schema association.
  async fn prepared_local_hover_world(fixture: &SchemaFixture) -> Result<LocalWorld<TestEnvironment>, TestFailure> {
    let world = local_world()?;
    install_schema_document(
      replace_local_document(&world, &fixture.document, HOVER_SOURCE, "the local hover document must install"),
      world.document_snapshot(&fixture.document),
      &fixture.document,
      &fixture.schema_url,
      hover_schema(),
      "the local hover document must expose a snapshot",
    )
    .await?;
    Ok(world)
  }

  /// Execute one local hover request through its complete public wire representation.
  async fn local_hover_at(
    world: &LocalWorld<TestEnvironment>,
    document: &Url,
    line: u32,
    character: u32,
    context: &'static str,
  ) -> Result<Option<Hover>, TestFailure> {
    ensure_ok(
      hover_local(
        world,
        Params::from(Some(position_params::<HoverParams>(
          document,
          line,
          character,
          "the hover request fixture must decode",
        )?)),
      )
      .await,
      context,
    )
  }

  /// Execute one local hover request and require concrete content.
  async fn required_local_hover_at(
    world: &LocalWorld<TestEnvironment>,
    document: &Url,
    line: u32,
    character: u32,
    execution_context: &'static str,
    presence_context: &'static str,
  ) -> Result<Hover, TestFailure> {
    ensure_some(
      local_hover_at(world, document, line, character, execution_context).await?,
      presence_context,
    )
  }

  /// Construct the complete schema shared by both hover execution families.
  fn hover_schema() -> serde_json::Value {
    json!({
      "type": "object",
      "properties": {
        "name": {
          "title": "Name",
          "description": "name documentation",
          "type": "string"
        },
        "empty": {
          "type": "integer"
        },
        "table": {
          "description": "table documentation",
          "type": "object",
          "properties": {
            "enabled": {
              "type": "boolean",
              "default": true,
              "x-taplo": {
                "docs": {
                  "defaultValue": "enabled-by-default documentation"
                }
              }
            }
          }
        }
      }
    })
  }

  /// Extract the complete observable Markdown and range from one hover response.
  fn hover_observation(hover: &Hover) -> Option<(&str, Option<Range>)> {
    match hover.contents {
      HoverContents::Markup(ref markup) => Some((markup.value.as_str(), hover.range)),
      HoverContents::Scalar(_) | HoverContents::Array(_) => None,
    }
  }

  /// Require one hover to retain exact Markdown and an exact source range.
  fn ensure_hover_content_and_range(
    hover: &Hover,
    expected_content: &str,
    expected_range: Range,
    context: &'static str,
  ) -> Result<(), TestFailure> {
    ensure(hover_observation(hover) == Some((expected_content, Some(expected_range))), context)
  }

  /// Require one hover to retain exact Markdown regardless of its selected primitive range.
  fn ensure_hover_content(hover: &Hover, expected_content: &str, context: &'static str) -> Result<(), TestFailure> {
    ensure(
      hover_observation(hover).is_some_and(|observation| observation.0 == expected_content),
      context,
    )
  }

  #[test]
  fn key_documentation_prefers_extensions_and_embeds_links_only_in_hover() -> Result<(), TestFailure> {
    let schema = json!({
        "title": "Setting",
        "description": "schema description",
        "x-taplo": {
            "docs": { "main": "extension documentation" },
            "links": { "key": "https://example.com/key" }
        }
    });
    let embedded = ensure_some(
      ensure_ok(
        key_documentation(&schema, true),
        "valid extension metadata must decode for key hover",
      )?,
      "key documentation with extension content must exist",
    )?;
    ensure_contains(
      &embedded,
      "[Setting](https://example.com/key)",
      "embedded hover mode must prefix the key link",
    )?;
    ensure_contains(
      &embedded,
      "extension documentation",
      "extension docs must override the schema description",
    )?;
    ensure_lacks(
      &embedded,
      "schema description",
      "lower-precedence schema description must not leak into extension docs",
    )?;

    let standalone_mode = ensure_some(
      ensure_ok(
        key_documentation(&schema, false),
        "valid extension metadata must decode for standalone-link mode",
      )?,
      "standalone-link mode must retain key documentation",
    )?;
    ensure_eq(
      &standalone_mode.as_str(),
      &"extension documentation",
      "standalone-link mode must not duplicate the URL inside hover content",
    )?;

    let description_only = json!({ "description": "fallback description" });
    ensure(
      ensure_ok(
        key_documentation(&description_only, true),
        "an absent extension must retain schema documentation",
      )? == Some("fallback description".into()),
      "schema description must backfill absent extension docs",
    )?;
    ensure(
      ensure_ok(
        key_documentation(&json!({ "description": "" }), true),
        "an absent extension with empty documentation must decode",
      )?
      .is_none(),
      "empty key documentation must produce no hover content",
    )
  }

  #[test]
  fn primitive_enum_documentation_obeys_link_and_embedding_precedence() -> Result<(), TestFailure> {
    let schema = json!({
        "title": "Value",
        "description": "description docs",
        "enum": [1, 2],
        "default": 2,
        "const": 2,
        "x-taplo": {
            "docs": {
                "main": "main docs",
                "enumValues": ["one docs", "two docs"],
                "defaultValue": "default docs",
                "constValue": "const docs"
            },
            "links": {
                "enumValues": [null, "https://example.com/two"]
            }
        }
    });
    let enum_embedded = ensure_some(
      ensure_ok(
        primitive_documentation(&schema, &json!(2), true),
        "valid enum extension metadata must decode",
      )?,
      "a matching documented enum value must produce content",
    )?;
    ensure_contains(
      &enum_embedded,
      "[Value](https://example.com/two)",
      "embedded mode must prefix a matching enum link",
    )?;
    ensure_contains(&enum_embedded, "two docs", "matching enum docs")?;

    ensure(
      ensure_ok(
        primitive_documentation(&schema, &json!(2), false),
        "valid enum metadata must decode in standalone-link mode",
      )? == Some("two docs".into()),
      "standalone-link mode must retain enum docs without embedding the link",
    )
  }

  #[test]
  fn primitive_documentation_falls_through_default_const_and_general_sources() -> Result<(), TestFailure> {
    let no_enum_docs = json!({
        "enum": [2],
        "default": 2,
        "const": 2,
        "x-taplo": {
            "docs": {
                "defaultValue": "default docs",
                "constValue": "const docs"
            }
        }
    });
    ensure(
      ensure_ok(
        primitive_documentation(&no_enum_docs, &json!(2), true),
        "valid default metadata must decode",
      )? == Some("default docs".into()),
      "a matching enum without enum docs must fall through to default before const",
    )?;

    let const_only = json!({
        "const": true,
        "x-taplo": { "docs": { "constValue": "const docs" } }
    });
    ensure(
      ensure_ok(
        primitive_documentation(&const_only, &json!(true), true),
        "valid constant metadata must decode",
      )? == Some("const docs".into()),
      "matching const docs must be selected when no default docs match",
    )?;

    let general = json!({
        "title": "Value",
        "description": "description docs",
        "x-taplo": { "docs": { "main": "main docs" } }
    });
    ensure(
      ensure_ok(
        primitive_documentation(&general, &json!(99), true),
        "valid main metadata must decode",
      )? == Some("main docs".into()),
      "a nonmatching value must fall through to extension main docs",
    )?;
    ensure(
      ensure_ok(
        primitive_documentation(&json!({ "description": "description" }), &json!(1), true),
        "an absent extension must retain the schema description",
      )? == Some("description".into()),
      "description must follow extension docs",
    )?;
    ensure(
      ensure_ok(
        primitive_documentation(&json!({ "title": "title" }), &json!(1), true),
        "an absent extension must retain the schema title",
      )? == Some("title".into()),
      "title must be the final nonempty fallback",
    )?;
    ensure(
      ensure_ok(
        primitive_documentation(&json!({}), &json!(1), true),
        "an empty schema must decode without extension metadata",
      )?
      .is_none(),
      "a schema without documentation must produce no hover",
    )
  }

  #[test]
  fn primitive_selection_includes_floats_but_excludes_identifiers() -> Result<(), TestFailure> {
    ensure(is_primitive(FLOAT), "floating-point values must be hoverable")?;
    ensure(is_primitive(STRING), "string values must be hoverable")?;
    ensure(!is_primitive(IDENT), "identifier handling must remain a distinct hover branch")
  }

  #[test]
  fn schema_backed_hover_projects_key_value_header_and_default_documentation() -> Result<(), TestFailure> {
    block_on(async {
      let fixture = hover_fixture()?;
      let local = prepared_local_hover_world(&fixture).await?;

      for expectation in [
        HoverExpectation {
          line:              0,
          character:         1,
          execution_context: "local key hover must execute",
          presence_context:  "a documented key must produce local hover content",
          content:           "name documentation",
          range:             Some(Range::new(Position::new(0, 0), Position::new(0, 4))),
          assertion_context: "key hover must preserve schema documentation and the exact identifier range",
        },
        HoverExpectation {
          line:              0,
          character:         9,
          execution_context: "local primitive hover must execute",
          presence_context:  "a documented primitive must produce local hover content",
          content:           "name documentation",
          range:             None,
          assertion_context: "primitive hover must resolve documentation through the same schema path as its key",
        },
        HoverExpectation {
          line:              2,
          character:         2,
          execution_context: "local table-header hover must execute",
          presence_context:  "a documented table header must produce local hover content",
          content:           "table documentation",
          range:             Some(Range::new(Position::new(2, 1), Position::new(2, 6))),
          assertion_context: "table-header hover must resolve the indexed header path and exact identifier range",
        },
        HoverExpectation {
          line:              3,
          character:         11,
          execution_context: "local default-value hover must execute",
          presence_context:  "a documented schema default must produce local hover content",
          content:           "enabled-by-default documentation",
          range:             None,
          assertion_context: "primitive hover must honor specialized default-value documentation",
        },
      ] {
        let hover = required_local_hover_at(
          &local, &fixture.document, expectation.line, expectation.character, expectation.execution_context, expectation.presence_context,
        )
        .await?;
        if let Some(expected_range) = expectation.range {
          ensure_hover_content_and_range(&hover, expectation.content, expected_range, expectation.assertion_context)?;
        } else {
          ensure_hover_content(&hover, expectation.content, expectation.assertion_context)?;
        }
      }
      Ok(())
    })
  }

  #[test]
  fn array_table_header_hover_resolves_the_schema_path_without_runtime_indices() -> Result<(), TestFailure> {
    block_on(async {
      let fixture = schema_fixture(
        "file:///workspace/array-hover.toml",
        "https://example.com/array-hover-schema.json",
        "the array-hover document URL must parse",
        "the array-hover schema URL must parse",
      )?;
      let world = local_world()?;
      install_schema_document(
        replace_local_document(
          &world,
          &fixture.document,
          "[[products]]\nname = \"first\"\n",
          "the array-hover document must install",
        ),
        world.document_snapshot(&fixture.document),
        &fixture.document,
        &fixture.schema_url,
        json!({
          "type": "object",
          "properties": {
            "products": {
              "description": "product collection documentation",
              "type": "array",
              "items": {
                "type": "object",
                "properties": {
                  "name": {
                    "type": "string"
                  }
                }
              }
            }
          }
        }),
        "the array-hover document must expose a snapshot",
      )
      .await?;

      let hover = required_local_hover_at(
        &world,
        &fixture.document,
        0,
        3,
        "array-table header hover must execute",
        "a documented array-table header must produce hover content",
      )
      .await?;
      ensure_hover_content_and_range(
        &hover,
        "product collection documentation",
        Range::new(Position::new(0, 2), Position::new(0, 10)),
        "array-table header hover must remove runtime item indices before schema lookup and retain the exact identifier range",
      )
    })
  }

  #[test]
  fn hover_rejects_missing_parameters_and_omits_unsupported_or_unassociated_targets() -> Result<(), TestFailure> {
    block_on(async {
      let fixture = hover_fixture()?;
      let local = prepared_local_hover_world(&fixture).await?;

      for (line, character, context) in [
        (1, 1, "a schema without documentation must not fabricate hover content"),
        (0, 5, "a nonidentifier operator position must not fabricate a hover target"),
      ] {
        ensure(
          local_hover_at(&local, &fixture.document, line, character, context)
            .await?
            .is_none(),
          context,
        )?;
      }

      let unassociated = local_world()?;
      replace_local_document(
        &unassociated,
        &fixture.document,
        HOVER_SOURCE,
        "the unassociated hover document must install",
      )
      .await?;
      ensure(
        local_hover_at(
          &unassociated,
          &fixture.document,
          0,
          1,
          "unassociated hover must remain an absent success",
        )
        .await?
        .is_none(),
        "a document without a schema association must not fabricate hover content",
      )?;
      let missing_params = ensure_some(
        hover_local(&local, Params::<HoverParams>::from(None)).await.err(),
        "hover without parameters must return a typed invalid-params error",
      )?;
      ensure_eq(
        &missing_params.code,
        &-32602,
        "hover without parameters must retain the standard invalid-params code",
      )
    })
  }

  #[test]
  fn hover_execution_families_preserve_observable_output() -> Result<(), TestFailure> {
    block_on(async {
      let fixture = hover_fixture()?;
      let local = prepared_local_hover_world(&fixture).await?;
      let local_hover = required_local_hover_at(
        &local,
        &fixture.document,
        0,
        1,
        "local parity hover must execute",
        "a documented key must produce local parity hover content",
      )
      .await?;

      let concurrent = concurrent_world()?;
      install_schema_document(
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
        "the concurrent hover document must expose a snapshot",
      )
      .await?;
      let concurrent_hover = ensure_some(
        ensure_ok(
          hover_concurrent(
            &concurrent,
            Params::from(Some(position_params::<HoverParams>(
              &fixture.document,
              0,
              1,
              "the concurrent hover request fixture must decode",
            )?)),
          )
          .await,
          "concurrent key hover must execute",
        )?,
        "a documented key must produce concurrent hover content",
      )?;
      ensure(
        hover_observation(&concurrent_hover) == hover_observation(&local_hover),
        "local and concurrent hover families must preserve the same observable content and range",
      )
    })
  }
}
