use taplo::dom::Diagnostic;
use taplo::dom::Node;
use taplo::dom::RenderError;
use taplo::parser::Diagnostic as ParseDiagnostic;
use taplo::parser::ParseFailure;
use taplo::parser::parse;
use thiserror::Error;

/// A document-conversion failure.
#[derive(Debug, Error)]
pub enum ConvertError {
  /// JSON decoding or encoding failed.
  #[error("JSON conversion failed")]
  Json(#[from] serde_json::Error),
  /// TOML syntax-tree construction failed.
  #[error("TOML parsing failed")]
  Parse(#[from] ParseFailure),
  /// TOML parsing recovered one or more syntax diagnostics.
  #[error("the TOML input contains syntax diagnostics")]
  SyntaxDiagnostics {
    /// Ordered recoverable parser diagnostics.
    diagnostics: Vec<ParseDiagnostic>,
  },
  /// TOML DOM construction recovered one or more semantic diagnostics.
  #[error("the TOML input contains semantic diagnostics")]
  SemanticDiagnostics {
    /// Ordered semantic diagnostics.
    diagnostics: Vec<Diagnostic>,
  },
  /// A semantic DOM value could not be rendered as TOML.
  #[error(transparent)]
  Render(#[from] RenderError),
}

/// JSON presentation selected after one TOML document validates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonFormatting {
  /// Emit JSON without presentation whitespace.
  Compact,
  /// Emit indented, line-oriented JSON.
  Pretty,
}

/// Convert a JSON document to TOML text.
///
/// # Errors
///
/// Returns [`ConvertError::Json`] when the input is not valid JSON.
pub fn json_to_toml(json: &str, inline: bool) -> Result<String, ConvertError> {
  let root: Node = serde_json::from_str(json)?;
  root.to_toml(inline, false).map_err(ConvertError::from)
}

/// Convert a TOML document to JSON using an explicit presentation policy.
///
/// # Errors
///
/// Returns [`ConvertError::Parse`] when Rowan cannot construct the syntax tree,
/// [`ConvertError::SyntaxDiagnostics`] or
/// [`ConvertError::SemanticDiagnostics`] when the source is not valid TOML,
/// or [`ConvertError::Json`] when the resulting DOM cannot be serialized.
pub fn toml_to_json_with_format(toml: &str, formatting: JsonFormatting) -> Result<String, ConvertError> {
  let root = validated_toml(toml)?;
  match formatting {
    JsonFormatting::Compact => serde_json::to_string(&root).map_err(ConvertError::from),
    JsonFormatting::Pretty => serde_json::to_string_pretty(&root).map_err(ConvertError::from),
  }
}

/// Convert a TOML document to pretty-printed JSON.
///
/// # Errors
///
/// Returns the typed conversion failures documented by
/// [`toml_to_json_with_format`].
pub fn toml_to_json(toml: &str) -> Result<String, ConvertError> {
  toml_to_json_with_format(toml, JsonFormatting::Pretty)
}

/// Parse and validate one TOML document for either JSON presentation.
fn validated_toml(toml: &str) -> Result<Node, ConvertError> {
  let parsed = parse(toml)?;
  if !parsed.diagnostics().is_empty() {
    return Err(ConvertError::SyntaxDiagnostics {
      diagnostics: parsed.diagnostics().to_vec(),
    });
  }
  let root = parsed.into_dom();
  if let Err(diagnostics) = root.validate() {
    return Err(ConvertError::SemanticDiagnostics {
      diagnostics,
    });
  }
  Ok(root)
}

#[cfg(test)]
mod tests {
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;

  use super::ConvertError;
  use super::JsonFormatting;
  use super::json_to_toml;
  use super::toml_to_json;
  use super::toml_to_json_with_format;

  #[test]
  fn converts_only_valid_toml_and_supported_json_values() -> Result<(), TestFailure> {
    let json = ensure_ok(
      toml_to_json_with_format("name = \"taplo\"\nenabled = true\n", JsonFormatting::Pretty),
      "valid TOML must convert to JSON",
    )?;
    ensure_eq(
      &json.as_str(),
      &"{\n  \"name\": \"taplo\",\n  \"enabled\": true\n}",
      "the established conversion entry point must retain pretty JSON output",
    )?;
    let decoded: serde_json::Value = ensure_ok(serde_json::from_str(&json), "converted JSON must decode")?;
    ensure_eq(
      &decoded,
      &serde_json::json!({
        "name": "taplo",
        "enabled": true,
      }),
      "conversion must preserve TOML scalar meaning",
    )?;
    ensure_eq(
      &ensure_ok(
        toml_to_json("name = \"taplo\"\nenabled = true\n"),
        "the compatibility conversion must succeed",
      )?,
      &json,
      "the established conversion entry point must select explicit pretty formatting",
    )?;
    let compact = ensure_ok(
      toml_to_json_with_format("name = \"taplo\"\nenabled = true\n", JsonFormatting::Compact),
      "valid TOML must convert to compact JSON",
    )?;
    ensure_eq(
      &compact.as_str(),
      &"{\"name\":\"taplo\",\"enabled\":true}",
      "compact conversion must preserve member order without presentation whitespace",
    )?;

    let toml = ensure_ok(
      json_to_toml(r#"{"name":"taplo","enabled":true}"#, false),
      "supported JSON objects must convert to TOML",
    )?;
    ensure(
      [toml.contains("name = \"taplo\""), toml.contains("enabled = true")] == [true, true],
      "JSON conversion must render both object members",
    )
  }

  #[test]
  fn rejects_syntax_semantic_and_json_failures_distinctly() -> Result<(), TestFailure> {
    for formatting in [JsonFormatting::Compact, JsonFormatting::Pretty] {
      let syntax = ensure_some(
        toml_to_json_with_format("value = [1 2]", formatting).err(),
        "recoverable syntax diagnostics must reject every JSON presentation",
      )?;
      ensure(
        matches!(syntax, ConvertError::SyntaxDiagnostics { .. }),
        "recoverable syntax diagnostics must not produce partial JSON",
      )?;
      ensure_eq(
        &syntax.to_string().as_str(),
        &"the TOML input contains syntax diagnostics",
        "syntax failures must identify the rejected JavaScript-facing input",
      )?;
      let semantic = ensure_some(
        toml_to_json_with_format("value = 1\nvalue = 2\n", formatting).err(),
        "semantic conflicts must reject every JSON presentation",
      )?;
      ensure(
        matches!(semantic, ConvertError::SemanticDiagnostics { .. }),
        "semantic conflicts must not produce partial JSON",
      )?;
      ensure_eq(
        &semantic.to_string().as_str(),
        &"the TOML input contains semantic diagnostics",
        "semantic failures must identify the rejected JavaScript-facing input",
      )?;
    }
    ensure(
      matches!(json_to_toml("{", false), Err(ConvertError::Json(_))),
      "malformed JSON must retain the JSON error channel",
    )?;
    ensure(
      matches!(json_to_toml("null", false), Err(ConvertError::Json(_))),
      "JSON values without a TOML representation must be rejected",
    )
  }
}
