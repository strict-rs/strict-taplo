//! Typed Taplo JSON Schema extension decoding.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

/// JSON Schema property containing Taplo-specific metadata.
pub const EXTENSION_KEY: &str = "x-taplo";

/// A typed Taplo schema-extension decoding failure.
#[derive(Debug, Error)]
pub enum SchemaExtensionError {
  /// The extension exists but is not an object.
  #[error("schema extension `{EXTENSION_KEY}` must be an object")]
  InvalidShape,
  /// The extension object does not match the supported wire schema.
  #[error("schema extension `{EXTENSION_KEY}` is invalid")]
  Decode {
    /// Underlying JSON decoding failure.
    #[source]
    source: serde_json::Error,
  },
}

/// Taplo-specific JSON Schema metadata.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub struct TaploSchemaExt {
  /// Whether completion should hide this schema.
  pub hidden:    Option<bool>,
  /// External documentation links.
  pub links:     Option<ExtLinks>,
  /// Human-readable documentation.
  pub docs:      Option<ExtDocs>,
  /// Keys initialized by object snippets.
  pub init_keys: Option<Vec<String>>,
  /// Plugin identifiers associated with the schema.
  #[serde(default)]
  pub plugins:   Vec<String>,
}

/// Human-readable documentation attached to schema values.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub struct ExtDocs {
  /// General schema documentation.
  pub main:          Option<String>,
  /// Documentation for the constant value.
  pub const_value:   Option<String>,
  /// Documentation for the default value.
  pub default_value: Option<String>,
  /// Documentation aligned with enum values by index.
  pub enum_values:   Option<Vec<Option<String>>>,
}

/// External documentation links attached to schema values.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub struct ExtLinks {
  /// Documentation for a key.
  pub key:         Option<String>,
  /// Documentation aligned with enum values by index.
  pub enum_values: Option<Vec<Option<String>>>,
}

/// Decode the optional Taplo extension from one JSON Schema value.
///
/// # Errors
///
/// Returns [`SchemaExtensionError`] when the extension exists but is not a valid extension object.
pub fn schema_ext_of(schema: &Value) -> Result<Option<TaploSchemaExt>, SchemaExtensionError> {
  let Some(extension) = schema.get(EXTENSION_KEY) else {
    return Ok(None);
  };
  if !extension.is_object() {
    return Err(SchemaExtensionError::InvalidShape);
  }
  serde_json::from_value(extension.clone())
    .map(Some)
    .map_err(|source| SchemaExtensionError::Decode {
      source,
    })
}
