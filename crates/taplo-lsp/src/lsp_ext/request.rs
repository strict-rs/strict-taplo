//! Taplo-specific requests and modern replacements for legacy LSP wire types.

use lsp_types::DocumentSymbolParams;
use lsp_types::Range;
use lsp_types::SymbolKind;
use lsp_types::SymbolTag;
use lsp_types::request::Request;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use url::Url;

/// Standard document-symbol request using Taplo's modern nested response DTO.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModernDocumentSymbolRequest {}

impl Request for ModernDocumentSymbolRequest {
  type Params = DocumentSymbolParams;
  type Result = Option<Vec<ModernDocumentSymbol>>;
  const METHOD: &'static str = "textDocument/documentSymbol";
}

/// A modern nested document symbol without LSP's deprecated `deprecated` field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModernDocumentSymbol {
  /// Symbol name.
  pub name:            String,
  /// Optional symbol detail.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub detail:          Option<String>,
  /// Symbol kind.
  pub kind:            SymbolKind,
  /// Optional modern symbol tags.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub tags:            Option<Vec<SymbolTag>>,
  /// Complete symbol range.
  pub range:           Range,
  /// Range most suitable for editor selection.
  pub selection_range: Range,
  /// Nested child symbols.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub children:        Option<Vec<Self>>,
}

/// Serialize a TOML text to JSON.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConvertToJsonRequest {}

/// Parameters for TOML-to-JSON conversion.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConvertToJsonParams {
  /// TOML or JSON text.
  pub text: String,
}

/// TOML-to-JSON conversion result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConvertToJsonResponse {
  /// JSON text.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub text: Option<String>,

  /// Conversion failure text, when conversion did not succeed.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub error: Option<String>,
}

impl Request for ConvertToJsonRequest {
  type Params = ConvertToJsonParams;
  type Result = ConvertToJsonResponse;
  const METHOD: &'static str = "taplo/convertToJson";
}

/// Serialize JSON text to TOML.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConvertToTomlRequest {}

/// Parameters for JSON-to-TOML conversion.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConvertToTomlParams {
  /// JSON text.
  pub text: String,
}

/// JSON-to-TOML conversion result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConvertToTomlResponse {
  /// TOML text.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub text: Option<String>,

  /// Conversion failure text, when conversion did not succeed.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub error: Option<String>,
}

impl Request for ConvertToTomlRequest {
  type Params = ConvertToTomlParams;
  type Result = ConvertToTomlResponse;
  const METHOD: &'static str = "taplo/convertToToml";
}

/// Request all configured schemas visible to one document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ListSchemasRequest {}

/// Document whose visible schema associations should be listed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSchemasParams {
  /// Target document URL.
  pub document_uri: Url,
}

/// Schemas visible to the requested document.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSchemasResponse {
  /// Visible schemas in association precedence order.
  pub schemas: Vec<SchemaInfo>,
}

impl Request for ListSchemasRequest {
  type Params = ListSchemasParams;
  type Result = ListSchemasResponse;
  const METHOD: &'static str = "taplo/listSchemas";
}

/// Serializable schema association metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaInfo {
  /// Resolved schema URL.
  pub url:  Url,
  /// Association metadata retained from its source.
  pub meta: Value,
}

/// Request the selected schema for one document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssociatedSchemaRequest {}

/// Document whose selected schema should be returned.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssociatedSchemaParams {
  /// Target document URL.
  pub document_uri: Url,
}

/// Selected schema association, if one exists.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssociatedSchemaResponse {
  /// Highest-precedence schema associated with the document.
  pub schema: Option<SchemaInfo>,
}

impl Request for AssociatedSchemaRequest {
  type Params = AssociatedSchemaParams;
  type Result = AssociatedSchemaResponse;
  const METHOD: &'static str = "taplo/associatedSchema";
}

#[cfg(test)]
mod tests {
  use lsp_types::Position;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;

  use super::ModernDocumentSymbol;
  use super::Range;
  use super::SymbolKind;
  use super::SymbolTag;

  #[test]
  fn modern_document_symbol_serializes_without_the_deprecated_field() -> Result<(), TestFailure> {
    let child_range = Range::new(Position::new(1, 2), Position::new(1, 5));
    let symbol = ModernDocumentSymbol {
      name:            "root".into(),
      detail:          None,
      kind:            SymbolKind::OBJECT,
      tags:            Some(vec![SymbolTag::DEPRECATED]),
      range:           Range::new(Position::new(0, 0), Position::new(2, 0)),
      selection_range: Range::new(Position::new(0, 0), Position::new(0, 4)),
      children:        Some(vec![ModernDocumentSymbol {
        name:            "value".into(),
        detail:          Some("integer".into()),
        kind:            SymbolKind::NUMBER,
        tags:            None,
        range:           child_range,
        selection_range: child_range,
        children:        None,
      }]),
    };

    let serialized = ensure_ok(serde_json::to_value(symbol), "the modern document-symbol DTO must serialize")?;
    ensure(
      serialized.get("deprecated").is_none(),
      "the owned DTO must omit the deprecated LSP field",
    )?;
    let tags = ensure_some(serialized.get("tags"), "modern symbol tags must be present")?;
    ensure_eq(
      tags,
      &serde_json::json!([1]),
      "modern symbol tags must use the standard numeric wire shape",
    )?;
    let selection_line = ensure_some(
      serialized.pointer("/children/0/selectionRange/start/line"),
      "the nested selection-range start line must be present",
    )?;
    ensure_eq(
      selection_line,
      &serde_json::json!(1),
      "nested symbols must retain the standard camel-case LSP range shape",
    )
  }
}
