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
#[allow(
  clippy::single_call_fn,
  reason = "the explicit-presentation conversion entry point owns JSON policy selection for every caller, including the pretty-printing \
            default"
)]
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
#[allow(
  clippy::single_call_fn,
  reason = "the established conversion entry point pins pretty JSON as the default presentation independently of the policy-selecting form"
)]
pub fn toml_to_json(toml: &str) -> Result<String, ConvertError> {
  toml_to_json_with_format(toml, JsonFormatting::Pretty)
}

/// Parse and validate one TOML document for either JSON presentation.
#[allow(
  clippy::single_call_fn,
  reason = "document validation isolates syntax- and semantic-diagnostic rejection from JSON presentation"
)]
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
  use strict_test_support::PredicateFailure;
  use strict_test_support::ensure_that;

  use super::ConvertError;
  use super::JsonFormatting;
  use super::json_to_toml;
  use super::toml_to_json;
  use super::toml_to_json_with_format;

  /// Conversion results and the decoded meaning of the pretty JSON output.
  #[derive(Debug)]
  struct Conversions {
    /// Explicit pretty presentation.
    pretty:  Result<String, ConvertError>,
    /// Decoding is attempted only after conversion yields JSON text.
    decoded: Option<Result<serde_json::Value, serde_json::Error>>,
    /// Established pretty-printing entry point.
    default: Result<String, ConvertError>,
    /// Compact presentation.
    compact: Result<String, ConvertError>,
    /// Reverse conversion to TOML.
    toml:    Result<String, ConvertError>,
  }

  #[test]
  fn converts_only_valid_toml_and_supported_json_values() -> Result<(), Box<PredicateFailure<Conversions>>> {
    let source = "name = \"taplo\"\nenabled = true\n";
    let pretty = toml_to_json_with_format(source, JsonFormatting::Pretty);
    let decoded = pretty.as_ref().ok().map(|json| serde_json::from_str(json));
    let observed = Conversions {
      pretty,
      decoded,
      default: toml_to_json(source),
      compact: toml_to_json_with_format(source, JsonFormatting::Compact),
      toml: json_to_toml(r#"{"name":"taplo","enabled":true}"#, false),
    };
    ensure_that(
      observed,
      "conversions must preserve scalar meaning, member order, and the selected presentation",
      |actual| {
        actual
          .pretty
          .as_ref()
          .is_ok_and(|json| json == "{\n  \"name\": \"taplo\",\n  \"enabled\": true\n}")
          && actual.decoded.as_ref().is_some_and(|result| {
            result
              .as_ref()
              .is_ok_and(|value| value == &serde_json::json!({"name": "taplo", "enabled": true}))
          })
          && actual
            .default
            .as_ref()
            .is_ok_and(|json| actual.pretty.as_ref().is_ok_and(|formatted| json == formatted))
          && actual
            .compact
            .as_ref()
            .is_ok_and(|json| json == r#"{"name":"taplo","enabled":true}"#)
          && actual
            .toml
            .as_ref()
            .is_ok_and(|toml| toml.contains("name = \"taplo\"") && toml.contains("enabled = true"))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// All rejected conversions, retaining the native diagnostics in presentation order.
  type RejectedConversions = [Result<String, ConvertError>; 6];

  #[test]
  fn rejects_syntax_semantic_and_json_failures_distinctly() -> Result<(), Box<PredicateFailure<RejectedConversions>>> {
    let observed = [
      toml_to_json_with_format("value = [1 2]", JsonFormatting::Compact),
      toml_to_json_with_format("value = 1\nvalue = 2\n", JsonFormatting::Compact),
      toml_to_json_with_format("value = [1 2]", JsonFormatting::Pretty),
      toml_to_json_with_format("value = 1\nvalue = 2\n", JsonFormatting::Pretty),
      json_to_toml("{", false),
      json_to_toml("null", false),
    ];
    ensure_that(observed, "syntax, semantic, malformed JSON, and unrepresentable JSON failures must retain distinct native channels", |actual| {
      let [ref compact_syntax, ref compact_semantic, ref pretty_syntax, ref pretty_semantic, ref malformed, ref unrepresentable] = *actual;
      [compact_syntax, pretty_syntax].into_iter().all(|result| matches!(result, Err(error @ ConvertError::SyntaxDiagnostics { .. }) if error.to_string() == "the TOML input contains syntax diagnostics"))
        && [compact_semantic, pretty_semantic].into_iter().all(|result| matches!(result, Err(error @ ConvertError::SemanticDiagnostics { .. }) if error.to_string() == "the TOML input contains semantic diagnostics"))
        && matches!(malformed, Err(ConvertError::Json(_)))
        && matches!(unrepresentable, Err(ConvertError::Json(_)))
    }).map(drop).map_err(Box::new)
  }
}
