//! Revision-checked syntax, DOM, and schema diagnostic construction.

use lsp_types::Diagnostic;
use lsp_types::DiagnosticRelatedInformation;
use lsp_types::DiagnosticSeverity;
use lsp_types::Location;
use lsp_types::Range;
use lsp_types::Uri;
use taplo::dom::Diagnostic as DomDiagnostic;
use taplo::dom::node::Key;
use taplo::rowan::TextRange;
use taplo_common::schema::SchemaError;
use taplo_lsp_async::util::MappingError;
use thiserror::Error;
use tracing::Instrument as _;
use url::Url;

use crate::world::DocumentSnapshot;
use crate::world::DocumentState;
use crate::world::SchemaExecution as _;
use crate::world::WorldState;

/// A failed diagnostic projection.
#[derive(Debug, Error)]
pub(super) enum DiagnosticError {
  /// A source range cannot be represented in LSP coordinates.
  #[error(transparent)]
  Mapping(#[from] MappingError),
  /// Schema validation failed.
  #[error(transparent)]
  Schema(#[from] SchemaError),
  /// A document URL cannot be represented by the LSP URI type.
  #[error("document URL `{url}` is not representable by LSP")]
  UnsupportedDocumentUri {
    /// Rejected document URL.
    url: Url,
  },
}

/// One current diagnostics result ready for protocol output.
pub(super) struct DiagnosticBatch {
  /// Document URI on the LSP wire.
  pub(super) uri:         Uri,
  /// Diagnostics selected by syntax/DOM/schema precedence.
  pub(super) diagnostics: Vec<Diagnostic>,
}

/// Collect the highest-priority current diagnostic phase for one document.
///
/// A stale snapshot returns `Ok(None)` so background work cannot overwrite a newer document,
/// configuration, or schema generation.
///
/// # Errors
///
/// Returns [`DiagnosticError`] when URI, coordinate, or schema projection fails.
macro_rules! define_diagnostic_future_family {
  (
    $collect_diagnostics:ident,
    $collect_schema_errors:ident,
    $document_snapshot:ident,
    $snapshot_is_current:ident;
    $future:ident,
    $environment:path,
    $transport:ident,
    $schema_execution:path;
    [$($value_bound:path),*]
  ) => {
    /// Collect the highest-priority current diagnostic phase for one document.
    ///
    /// A stale snapshot returns `Ok(None)` so background work cannot overwrite a newer document,
    /// configuration, or schema generation.
    ///
    /// # Errors
    ///
    /// Returns [`DiagnosticError`] when URI, coordinate, or schema projection fails.
    pub(super) fn $collect_diagnostics<'operation, E: $environment>(
      world: &'operation WorldState<E, $transport<E>>,
      document_url: &'operation Url,
    ) -> $future<'operation, Result<Option<DiagnosticBatch>, DiagnosticError>> {
      let span = tracing::info_span!("collect_diagnostics", %document_url);
      Box::pin(
        async move {
          let uri = super::uri::to_uri(document_url).ok_or_else(|| DiagnosticError::UnsupportedDocumentUri {
            url: document_url.clone()
          })?;
          let Some(snapshot) = world.$document_snapshot(document_url).await else {
            return Ok(None);
          };

          let syntax = collect_syntax_errors(&snapshot.document)?;
          let diagnostics = if syntax.is_empty() {
            let dom = collect_dom_errors(&snapshot.document, &uri)?;
            if dom.is_empty() {
              $collect_schema_errors(&snapshot, document_url).await?
            } else {
              dom
            }
          } else {
            syntax
          };
          if !world.$snapshot_is_current(document_url, &snapshot).await {
            return Ok(None);
          }
          Ok(Some(DiagnosticBatch {
            uri,
            diagnostics,
          }))
        }
        .instrument(span),
      )
    }

    /// Validate a clean snapshot against its active schema association.
    fn $collect_schema_errors<'operation, E: $environment>(
      snapshot: &'operation DocumentSnapshot<$transport<E>>,
      document_url: &'operation Url,
    ) -> $future<'operation, Result<Vec<Diagnostic>, DiagnosticError>> {
      Box::pin(async move {
        if !snapshot.config.schema.enabled {
          return Ok(Vec::new());
        }
        let Some(association) = snapshot.schemas.associations().association_for(document_url) else {
          return Ok(Vec::new());
        };
        let errors = <$schema_execution>::validate_root(
          &snapshot.schemas,
          &association.url,
          &snapshot.document.dom,
        )
        .await?;
        let mut diagnostics = Vec::new();
        for error in errors {
          if let Some(source) = error.text_ranges().into_iter().next() {
            diagnostics.push(Diagnostic {
              range: super::uri::to_lsp_range(&snapshot.document.mapper, source)?,
              severity: Some(DiagnosticSeverity::ERROR),
              source: Some("Even Better TOML".into()),
              message: error.message,
              ..Diagnostic::default()
            });
          }
        }
        Ok(diagnostics)
      })
    }
  };
}

define_lsp_execution_families!(
  handler
  define_diagnostic_future_family;
  (collect_diagnostics_local, collect_diagnostics_concurrent),
  (
    collect_schema_errors_local,
    collect_schema_errors_concurrent
  ),
  (document_snapshot, document_snapshot_concurrent),
  (snapshot_is_current, snapshot_is_current_concurrent),
);

/// Build an empty diagnostics batch for one closed document.
///
/// # Errors
///
/// Returns [`DiagnosticError`] when the document URL is not representable by LSP.
pub(super) fn cleared_diagnostics(document_url: &Url) -> Result<DiagnosticBatch, DiagnosticError> {
  let uri = super::uri::to_uri(document_url).ok_or_else(|| DiagnosticError::UnsupportedDocumentUri {
    url: document_url.clone()
  })?;
  Ok(DiagnosticBatch {
    uri,
    diagnostics: Vec::new(),
  })
}

/// Build the single hint representing an open document excluded by workspace rules.
///
/// # Errors
///
/// Returns [`DiagnosticError`] when the document URL is not representable by LSP.
pub(super) fn excluded_diagnostics(document_url: &Url) -> Result<DiagnosticBatch, DiagnosticError> {
  let uri = super::uri::to_uri(document_url).ok_or_else(|| DiagnosticError::UnsupportedDocumentUri {
    url: document_url.clone()
  })?;
  Ok(DiagnosticBatch {
    uri,
    diagnostics: vec![Diagnostic {
      range: Range::default(),
      severity: Some(DiagnosticSeverity::HINT),
      source: Some("Even Better TOML".into()),
      message: "this document has been excluded".into(),
      ..Diagnostic::default()
    }],
  })
}

/// Convert every parser diagnostic to a checked LSP range.
fn collect_syntax_errors(document: &DocumentState) -> Result<Vec<Diagnostic>, MappingError> {
  document
    .parse
    .diagnostics()
    .iter()
    .map(|error| {
      Ok(Diagnostic {
        range: super::uri::to_lsp_range(&document.mapper, error.range())?,
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some("Even Better TOML".into()),
        message: error.message().to_owned(),
        ..Diagnostic::default()
      })
    })
    .collect()
}

/// Convert every semantic DOM diagnostic without fake ranges or partial related-information pairs.
fn collect_dom_errors(document: &DocumentState, document_uri: &Uri) -> Result<Vec<Diagnostic>, MappingError> {
  let mut diagnostics = Vec::new();
  let Err(errors) = document.dom.validate() else {
    return Ok(diagnostics);
  };

  for error in errors {
    let message = error.to_string();
    match error {
      DomDiagnostic::ConflictingKeys {
        key,
        other,
      } => push_paired_dom_error(
        document,
        document_uri,
        PairedDomDiagnostic {
          primary_key: &key,
          related_key: &other,
          message,
          primary_relation: "other key defined here",
          related_relation: "other key defined here",
        },
        &mut diagnostics,
      )?,
      DomDiagnostic::ExpectedTable {
        not_table,
        required_by,
      } => push_paired_dom_error(
        document,
        document_uri,
        PairedDomDiagnostic {
          primary_key: &not_table,
          related_key: &required_by,
          message,
          primary_relation: "required by this key",
          related_relation: "table defined here",
        },
        &mut diagnostics,
      )?,
      DomDiagnostic::ExpectedArrayOfTables {
        not_array_of_tables,
        required_by,
      } => push_paired_dom_error(
        document,
        document_uri,
        PairedDomDiagnostic {
          primary_key: &not_array_of_tables,
          related_key: &required_by,
          message,
          primary_relation: "required by this key",
          related_relation: "array of tables defined here",
        },
        &mut diagnostics,
      )?,
      DomDiagnostic::InvalidEscapeSequence {
        string,
      } => diagnostics.push(single_dom_error(document, string.text_range(), message)?),
      DomDiagnostic::MalformedScalar(malformed) => {
        diagnostics.push(single_dom_error(document, malformed.syntax().text_range(), message)?);
      }
      DomDiagnostic::UnexpectedSyntax {
        syntax,
      } => diagnostics.push(single_dom_error(document, syntax.text_range(), message)?),
    }
  }
  Ok(diagnostics)
}

/// Source keys and relationship labels for one two-sided semantic diagnostic.
struct PairedDomDiagnostic<'key> {
  /// Primary key highlighted as the error.
  primary_key:      &'key Key,
  /// Related key highlighted as contextual information.
  related_key:      &'key Key,
  /// Shared diagnostic message.
  message:          String,
  /// Relationship shown from the primary key.
  primary_relation: &'static str,
  /// Relationship shown from the related key.
  related_relation: &'static str,
}

/// Add both sides of a two-range DOM diagnostic.
fn push_paired_dom_error(
  document: &DocumentState,
  document_uri: &Uri,
  diagnostic: PairedDomDiagnostic<'_>,
  diagnostics: &mut Vec<Diagnostic>,
) -> Result<(), MappingError> {
  let Some(primary_source) = diagnostic.primary_key.text_ranges().next() else {
    return Ok(());
  };
  let Some(related_source) = diagnostic.related_key.text_ranges().next() else {
    return Ok(());
  };
  let primary = super::uri::to_lsp_range(&document.mapper, primary_source)?;
  let related = super::uri::to_lsp_range(&document.mapper, related_source)?;

  diagnostics.push(Diagnostic {
    range: primary,
    severity: Some(DiagnosticSeverity::ERROR),
    source: Some("Even Better TOML".into()),
    message: diagnostic.message.clone(),
    related_information: Some(vec![DiagnosticRelatedInformation {
      location: Location {
        uri:   document_uri.clone(),
        range: related,
      },
      message:  diagnostic.primary_relation.into(),
    }]),
    ..Diagnostic::default()
  });
  diagnostics.push(Diagnostic {
    range: related,
    severity: Some(DiagnosticSeverity::HINT),
    source: Some("Even Better TOML".into()),
    message: diagnostic.message,
    related_information: Some(vec![DiagnosticRelatedInformation {
      location: Location {
        uri:   document_uri.clone(),
        range: primary,
      },
      message:  diagnostic.related_relation.into(),
    }]),
    ..Diagnostic::default()
  });
  Ok(())
}

/// Build one source-anchored semantic diagnostic.
fn single_dom_error(document: &DocumentState, source_range: TextRange, message: String) -> Result<Diagnostic, MappingError> {
  Ok(Diagnostic {
    range: super::uri::to_lsp_range(&document.mapper, source_range)?,
    severity: Some(DiagnosticSeverity::ERROR),
    source: Some("Even Better TOML".into()),
    message,
    ..Diagnostic::default()
  })
}

#[cfg(test)]
mod tests {
  use lsp_types::DiagnosticSeverity;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use url::Url;

  use super::cleared_diagnostics;
  use super::collect_dom_errors;
  use super::collect_syntax_errors;
  use super::excluded_diagnostics;
  use crate::handlers::test_support::parse_document;

  /// Parse one absolute document URL used by diagnostic wire fixtures.
  fn document_url() -> Result<Url, TestFailure> {
    ensure_ok(
      Url::parse("file:///workspace/diagnostics.toml"),
      "the diagnostic document URL must parse",
    )
  }

  #[test]
  fn syntax_diagnostics_preserve_error_ranges_while_clean_source_stays_empty() -> Result<(), TestFailure> {
    let malformed = parse_document("value =\n", "the recoverable syntax-error document must construct")?;
    let diagnostics = ensure_ok(
      collect_syntax_errors(&malformed),
      "recoverable parser diagnostics must map to LSP coordinates",
    )?;
    ensure(
      !diagnostics.is_empty(),
      "recoverable malformed source must produce at least one parser diagnostic",
    )?;
    ensure(
      diagnostics.iter().all(|diagnostic| {
        (diagnostic.severity, diagnostic.source.as_deref(), diagnostic.message.is_empty())
          == (Some(DiagnosticSeverity::ERROR), Some("Even Better TOML"), false)
      }),
      "every parser diagnostic must retain its error severity, source, and message",
    )?;

    let clean = parse_document("value = 1\n", "the clean diagnostic document must construct")?;
    ensure(
      ensure_ok(collect_syntax_errors(&clean), "clean parser state must remain mappable")?.is_empty(),
      "clean source must not fabricate parser diagnostics",
    )
  }

  #[test]
  fn semantic_diagnostics_cover_paired_and_single_source_contracts() -> Result<(), TestFailure> {
    let url = document_url()?;
    let uri = ensure_some(
      super::super::uri::to_uri(&url),
      "the diagnostic document URL must convert to an LSP URI",
    )?;
    for (source, expected_message, expected_count, context) in [
      (
        "a = 1\na = 2\n",
        "conflicting keys",
        2_usize,
        "conflicting keys must retain paired diagnostics",
      ),
      (
        "a = 1\n[a.b]\n",
        "expected table",
        2_usize,
        "table requirements must retain paired diagnostics",
      ),
      (
        "a = 1\n[[a]]\n",
        "expected array of tables",
        2_usize,
        "array-of-table requirements must retain paired diagnostics",
      ),
      (
        "\"\\q\" = 1\n",
        "the string contains invalid escape sequence(s)",
        1_usize,
        "invalid key escapes must retain one source diagnostic",
      ),
      (
        "value = 999999999999999999999999999999\n",
        "the integer scalar could not be decoded:",
        1_usize,
        "malformed integers must retain one typed decode diagnostic",
      ),
      (
        "missing =\nnext = 1\n",
        "the syntax was not expected here:",
        1_usize,
        "missing values must retain one unexpected-source diagnostic",
      ),
    ] {
      let document = parse_document(source, "the semantic-diagnostic document must construct")?;
      let diagnostics = ensure_ok(
        collect_dom_errors(&document, &uri),
        "semantic diagnostics must map to complete LSP values",
      )?;
      let matching = diagnostics
        .iter()
        .filter(|diagnostic| {
          if expected_count == 2 {
            diagnostic.message == expected_message
          } else {
            diagnostic.message.starts_with(expected_message)
          }
        })
        .collect::<Vec<_>>();
      ensure(matching.len() == expected_count, context)?;
      if expected_count == 2 {
        let primary = ensure_some(
          matching.first().copied(),
          "a paired semantic diagnostic must retain its primary side",
        )?;
        let related = ensure_some(
          matching.get(1).copied(),
          "a paired semantic diagnostic must retain its contextual side",
        )?;
        ensure(
          (
            primary.severity,
            related.severity,
            primary.related_information.as_ref().map(Vec::len),
            related.related_information.as_ref().map(Vec::len),
          ) == (Some(DiagnosticSeverity::ERROR), Some(DiagnosticSeverity::HINT), Some(1), Some(1)),
          "paired semantic diagnostics must retain error/hint polarity and reciprocal source context",
        )?;
      } else {
        ensure(
          matching
            .first()
            .is_some_and(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR)),
          "single-source semantic diagnostics must retain error severity",
        )?;
      }
    }

    let clean = parse_document("value = 1\n", "the clean semantic-diagnostic document must construct")?;
    ensure(
      ensure_ok(collect_dom_errors(&clean, &uri), "clean semantic state must remain mappable")?.is_empty(),
      "clean semantic state must not fabricate diagnostics",
    )
  }

  #[test]
  fn cleared_and_excluded_batches_map_to_complete_publish_payloads() -> Result<(), TestFailure> {
    let url = document_url()?;
    let cleared = ensure_ok(
      cleared_diagnostics(&url),
      "a closed document must produce a cleared diagnostics batch",
    )?;
    ensure(
      cleared.diagnostics.is_empty(),
      "a cleared diagnostics batch must contain no stale entries",
    )?;

    let excluded = ensure_ok(
      excluded_diagnostics(&url),
      "an excluded document must produce its explanatory diagnostics batch",
    )?;
    let excluded_observation = excluded
      .diagnostics
      .first()
      .map(|diagnostic| (diagnostic.severity, diagnostic.message.as_str()));
    ensure(
      (excluded.diagnostics.len(), excluded_observation) == (1, Some((Some(DiagnosticSeverity::HINT), "this document has been excluded"))),
      "an excluded document must publish exactly one explanatory hint",
    )?;
    let published = super::super::documents::publish_params(excluded);
    ensure(
      (published.uri, published.version, published.diagnostics.len()) == (cleared.uri, None, 1),
      "diagnostic publication must preserve the URI, omit a version, and retain the complete batch",
    )
  }
}
