//! Schema-backed identifier and primitive hover presentation.

use itertools::Itertools;
use lsp_async_stub::Context;
use lsp_async_stub::Params;
use lsp_async_stub::rpc::Error;
use lsp_types::Hover;
use lsp_types::HoverContents;
use lsp_types::HoverParams;
use lsp_types::MarkupContent;
use lsp_types::MarkupKind;
use serde_json::Value;
use taplo::dom::KeyOrIndex;
use taplo::dom::Keys;
use taplo::syntax::SyntaxKind::BOOL;
use taplo::syntax::SyntaxKind::DATE;
use taplo::syntax::SyntaxKind::DATE_TIME_LOCAL;
use taplo::syntax::SyntaxKind::DATE_TIME_OFFSET;
use taplo::syntax::SyntaxKind::FLOAT;
use taplo::syntax::SyntaxKind::IDENT;
use taplo::syntax::SyntaxKind::INTEGER;
use taplo::syntax::SyntaxKind::INTEGER_BIN;
use taplo::syntax::SyntaxKind::INTEGER_HEX;
use taplo::syntax::SyntaxKind::INTEGER_OCT;
use taplo::syntax::SyntaxKind::MULTI_LINE_STRING;
use taplo::syntax::SyntaxKind::MULTI_LINE_STRING_LITERAL;
use taplo::syntax::SyntaxKind::STRING;
use taplo::syntax::SyntaxKind::STRING_LITERAL;
use taplo::syntax::SyntaxKind::TIME;
use taplo::syntax::SyntaxKind::{
  self,
};
use taplo_common::environment::Environment;
use taplo_common::schema::ext::schema_ext_of;

use crate::query::Query;
use crate::query::lookup_keys;
use crate::world::World;

#[tracing::instrument(skip_all)]
pub(crate) async fn hover<E: Environment>(context: Context<World<E>>, params: Params<HoverParams>) -> Result<Option<Hover>, Error> {
  let params = params.required()?;
  let Some(document_uri) = crate::uri::to_url(&params.text_document_position_params.text_document.uri) else {
    return Ok(None);
  };
  let Some(snapshot) = context.document_snapshot(&document_uri).await else {
    return Ok(None);
  };
  let document = &snapshot.document;
  let Some(offset) = document
    .mapper
    .offset(crate::uri::from_lsp_position(params.text_document_position_params.position))
  else {
    return Ok(None);
  };
  let query = Query::at(&document.dom, offset);
  let Some(position) = query.first_matching(|position| position.syntax.kind() == IDENT || is_primitive(position.syntax.kind())) else {
    return Ok(None);
  };
  let Some(association) = snapshot.schemas.associations().association_for(&document_uri) else {
    return Ok(None);
  };
  let value = match serde_json::to_value(&document.dom) {
    Ok(value) => value,
    Err(error) => {
      tracing::warn!(%error, "cannot turn DOM into JSON");
      return Ok(None);
    }
  };
  let Some((position_keys, _)) = &position.dom_node else {
    return Ok(None);
  };
  let mut keys = position_keys.clone();
  if query.header_key().is_some() {
    let Some(index) = query.header_identifier_index(&position.syntax) else {
      return Ok(None);
    };
    keys = lookup_keys(document.dom.clone(), &Keys::new(keys.into_iter().take(index.saturating_add(1))));
  }
  let Some(node) = document.dom.path(&keys) else {
    return Ok(None);
  };
  let links_in_hover = !snapshot.config.schema.links;

  let content = if position.syntax.kind() == IDENT {
    keys = lookup_keys(document.dom.clone(), &keys);
    while matches!(keys.iter().last(), Some(KeyOrIndex::Index(_))) {
      keys = keys.skip_right(1);
    }
    let schemas = match snapshot.schemas.schemas_at_path(&association.url, &value, &keys).await {
      Ok(schemas) => schemas,
      Err(error) => {
        tracing::error!(%error, "schema resolution failed");
        return Ok(None);
      }
    };
    schemas
      .iter()
      .filter_map(|(_, schema)| key_documentation(schema, links_in_hover))
      .join("\n\n")
  } else {
    let schemas = match snapshot.schemas.schemas_at_path(&association.url, &value, &keys).await {
      Ok(schemas) => schemas,
      Err(error) => {
        tracing::error!(%error, "schema resolution failed");
        return Ok(None);
      }
    };
    let primitive = match serde_json::to_value(node) {
      Ok(value) => value,
      Err(error) => {
        tracing::warn!(%error, "failed to turn DOM value into JSON");
        return Ok(None);
      }
    };
    schemas
      .iter()
      .filter_map(|(_, schema)| primitive_documentation(schema, &primitive, links_in_hover))
      .join("\n")
  };
  if content.is_empty() {
    return Ok(None);
  }
  let Some(range) = crate::uri::to_lsp_range(&document.mapper, position.syntax.text_range()) else {
    return Ok(None);
  };
  Ok(Some(Hover {
    contents: HoverContents::Markup(MarkupContent {
      kind:  MarkupKind::Markdown,
      value: content,
    }),
    range:    Some(range),
  }))
}

/// Render key documentation with its optional embedded external link.
fn key_documentation(schema: &Value, links_in_hover: bool) -> Option<String> {
  let extension = schema_ext_of(schema).unwrap_or_default();
  let mut content = extension
    .docs
    .as_ref()
    .and_then(|docs| docs.main.clone())
    .or_else(|| schema["description"].as_str().map(ToOwned::to_owned))?;
  if content.is_empty() {
    return None;
  }
  if links_in_hover && let Some(link) = extension.links.and_then(|links| links.key) {
    let title = schema["title"].as_str().unwrap_or("...");
    content = format!("[{title}]({link})\n\n{content}");
  }
  Some(content)
}

/// Select primitive-value documentation in the established specialized-to-general precedence.
fn primitive_documentation(schema: &Value, value: &Value, links_in_hover: bool) -> Option<String> {
  let extension = schema_ext_of(schema).unwrap_or_default();
  let docs = extension.docs.unwrap_or_default();
  let links = extension.links.unwrap_or_default();
  if let Some(index) = schema["enum"]
    .as_array()
    .and_then(|values| values.iter().position(|candidate| candidate == value))
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
    return (!content.is_empty()).then_some(content);
  }
  if schema.get("default") == Some(value)
    && let Some(content) = docs.default_value
  {
    return (!content.is_empty()).then_some(content);
  }
  if schema.get("const") == Some(value)
    && let Some(content) = docs.const_value
  {
    return (!content.is_empty()).then_some(content);
  }
  docs
    .main
    .or_else(|| schema["description"].as_str().map(ToOwned::to_owned))
    .or_else(|| schema["title"].as_str().map(ToOwned::to_owned))
    .filter(|content| !content.is_empty())
}

/// Whether a syntax token represents a primitive TOML value.
fn is_primitive(kind: SyntaxKind) -> bool {
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
  use serde_json::json;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_contains;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_lacks;
  use strict_test_support::ensure_some;
  use taplo::syntax::SyntaxKind::FLOAT;
  use taplo::syntax::SyntaxKind::IDENT;
  use taplo::syntax::SyntaxKind::STRING;

  use super::is_primitive;
  use super::key_documentation;
  use super::primitive_documentation;

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
      key_documentation(&schema, true),
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
      key_documentation(&schema, false),
      "standalone-link mode must retain key documentation",
    )?;
    ensure_eq(
      &standalone_mode.as_str(),
      &"extension documentation",
      "standalone-link mode must not duplicate the URL inside hover content",
    )?;

    let description_only = json!({ "description": "fallback description" });
    ensure(
      key_documentation(&description_only, true) == Some("fallback description".into()),
      "schema description must backfill absent extension docs",
    )?;
    ensure(
      key_documentation(&json!({ "description": "" }), true).is_none(),
      "empty key documentation must produce no hover content",
    )
  }

  #[test]
  fn primitive_documentation_obeys_specialized_then_general_precedence() -> Result<(), TestFailure> {
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
      primitive_documentation(&schema, &json!(2), true),
      "a matching documented enum value must produce content",
    )?;
    ensure_contains(
      &enum_embedded,
      "[Value](https://example.com/two)",
      "embedded mode must prefix a matching enum link",
    )?;
    ensure_contains(&enum_embedded, "two docs", "matching enum docs")?;

    ensure(
      primitive_documentation(&schema, &json!(2), false) == Some("two docs".into()),
      "standalone-link mode must retain enum docs without embedding the link",
    )?;

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
      primitive_documentation(&no_enum_docs, &json!(2), true) == Some("default docs".into()),
      "a matching enum without enum docs must fall through to default before const",
    )?;

    let const_only = json!({
        "const": true,
        "x-taplo": { "docs": { "constValue": "const docs" } }
    });
    ensure(
      primitive_documentation(&const_only, &json!(true), true) == Some("const docs".into()),
      "matching const docs must be selected when no default docs match",
    )?;

    ensure(
      primitive_documentation(&schema, &json!(99), true) == Some("main docs".into()),
      "a nonmatching value must fall through to extension main docs",
    )?;
    ensure(
      primitive_documentation(&json!({ "description": "description" }), &json!(1), true) == Some("description".into()),
      "description must follow extension docs",
    )?;
    ensure(
      primitive_documentation(&json!({ "title": "title" }), &json!(1), true) == Some("title".into()),
      "title must be the final nonempty fallback",
    )?;
    ensure(
      primitive_documentation(&json!({}), &json!(1), true).is_none(),
      "a schema without documentation must produce no hover",
    )
  }

  #[test]
  fn primitive_selection_includes_floats_but_excludes_identifiers() -> Result<(), TestFailure> {
    ensure(is_primitive(FLOAT), "floating-point values must be hoverable")?;
    ensure(is_primitive(STRING), "string values must be hoverable")?;
    ensure(!is_primitive(IDENT), "identifier handling must remain a distinct hover branch")
  }
}
