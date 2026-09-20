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
#[derive(Debug)]
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
    #[allow(clippy::single_call_fn, reason = "naming the schema phase keeps association lookup and validation projection out of the syntax-then-DOM-then-schema precedence ladder that selects it")]
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
  use std::fmt::Debug;

  use lsp_types::Diagnostic;
  use lsp_types::DiagnosticSeverity;
  use lsp_types::Uri;
  use strict_test_support::ResultFailure;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_that;
  use taplo_lsp_async::util::MappingError;
  use url::Url;

  use super::cleared_diagnostics;
  use super::collect_dom_errors;
  use super::collect_syntax_errors;
  use super::excluded_diagnostics;
  use crate::handlers::test_support::parse_document;
  use crate::world::DocumentState;
  use crate::world::WorldError;

  /// A complete parsed document and its native diagnostic projection.
  type SemanticObservation = (DocumentState, Result<Vec<Diagnostic>, MappingError>);

  /// Expected diagnostic contract alongside its complete semantic observation.
  type SemanticCase<Expected> = (Expected, Result<SemanticObservation, Box<ResultFailure<WorldError>>>);

  /// Parse one absolute document URL used by diagnostic wire fixtures.
  fn document_url() -> Result<Url, ResultFailure<url::ParseError>> {
    ensure_ok(
      Url::parse("file:///workspace/diagnostics.toml"),
      "the diagnostic document URL must parse",
    )
  }

  /// Preserve a complete semantic fixture and its native diagnostic projection.
  fn semantic_observation(source: &str, uri: &Uri) -> Result<SemanticObservation, Box<ResultFailure<WorldError>>> {
    parse_document(source, "the semantic-diagnostic document must construct").map(|document| {
      let diagnostics = collect_dom_errors(&document, uri);
      (document, diagnostics)
    })
  }

  #[test]
  fn syntax_diagnostics_preserve_error_ranges_while_clean_source_stays_empty() -> Result<(), impl Debug> {
    let observed = ["value =\n", "value = 1\n"].map(|source| {
      parse_document(source, "the syntax-diagnostic document must construct").map(|document| {
        let diagnostics = collect_syntax_errors(&document);
        (document, diagnostics)
      })
    });
    ensure_that(
      observed,
      "malformed syntax must retain error ranges, source and messages while clean source remains empty",
      |fixtures| {
        let [ref malformed, ref clean] = *fixtures;
        let Ok(ref malformed_fixture) = *malformed else {
          return false;
        };
        let Ok(ref diagnostics) = malformed_fixture.1 else {
          return false;
        };
        !diagnostics.is_empty()
          && diagnostics.iter().all(|diagnostic| {
            (diagnostic.severity, diagnostic.source.as_deref(), diagnostic.message.is_empty())
              == (Some(DiagnosticSeverity::ERROR), Some("Even Better TOML"), false)
          })
          && clean.as_ref().is_ok_and(|fixture| fixture.1.as_ref().is_ok_and(Vec::is_empty))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn semantic_diagnostics_cover_paired_source_contracts() -> Result<(), impl Debug> {
    let observed = document_url().map(|url| {
      let wire = super::super::uri::to_uri(&url);
      let cases = wire.as_ref().map(|uri| {
        [
          ("a = 1\na = 2\n", "conflicting keys"),
          ("a = 1\n[a.b]\n", "expected table"),
          ("a = 1\n[[a]]\n", "expected array of tables"),
        ]
        .map(|(source, message)| (message, semantic_observation(source, uri)))
      });
      (url, wire, cases)
    });
    let matches_paired = |case: &SemanticCase<&str>| {
      let Ok(ref fixture) = case.1 else {
        return false;
      };
      let Ok(ref diagnostics) = fixture.1 else {
        return false;
      };
      let matching = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message == case.0)
        .collect::<Vec<_>>();
      matching.len() == 2
        && matching.first().zip(matching.get(1)).map(|(primary, related)| {
          (
            primary.severity,
            related.severity,
            primary.related_information.as_ref().map(Vec::len),
            related.related_information.as_ref().map(Vec::len),
          )
        }) == Some((Some(DiagnosticSeverity::ERROR), Some(DiagnosticSeverity::HINT), Some(1), Some(1)))
    };
    ensure_that(
      observed,
      "paired semantic diagnostics must retain both source locations, error/hint polarity and reciprocal context",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        let Some(ref cases) = scenario.2 else {
          return false;
        };
        cases.iter().all(matches_paired)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn semantic_diagnostics_cover_single_source_and_clean_contracts() -> Result<(), impl Debug> {
    let observed = document_url().map(|url| {
      let wire = super::super::uri::to_uri(&url);
      let cases = wire.as_ref().map(|uri| {
        [
          ("\"\\q\" = 1\n", Some("the string contains invalid escape sequence(s)")),
          (
            "value = 999999999999999999999999999999\n",
            Some("the integer scalar could not be decoded:"),
          ),
          ("missing =\nnext = 1\n", Some("the syntax was not expected here:")),
          ("value = 1\n", None),
        ]
        .map(|(source, prefix)| (prefix, semantic_observation(source, uri)))
      });
      (url, wire, cases)
    });
    let matches_single = |case: &SemanticCase<Option<&str>>| {
      let Ok(ref fixture) = case.1 else {
        return false;
      };
      let Ok(ref diagnostics) = fixture.1 else {
        return false;
      };
      case.0.map_or_else(
        || diagnostics.is_empty(),
        |prefix| {
          diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.starts_with(prefix))
            .map(|diagnostic| diagnostic.severity)
            .eq([Some(DiagnosticSeverity::ERROR)])
        },
      )
    };
    ensure_that(
      observed,
      "single-source semantic errors must retain error severity while clean semantic state remains empty",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        let Some(ref cases) = scenario.2 else {
          return false;
        };
        cases.iter().all(matches_single)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn cleared_and_excluded_batches_map_to_complete_publish_payloads() -> Result<(), impl Debug> {
    let observed = document_url().map(|url| {
      let cleared = cleared_diagnostics(&url);
      let excluded = excluded_diagnostics(&url).map(super::super::documents::publish_params);
      (url, cleared, excluded)
    });
    ensure_that(
      observed,
      "cleared publication must remove stale entries and excluded publication must retain one unversioned explanatory hint",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        let Ok(ref cleared) = scenario.1 else {
          return false;
        };
        let Ok(ref published) = scenario.2 else {
          return false;
        };
        cleared.diagnostics.is_empty()
          && published.uri == cleared.uri
          && published.version.is_none()
          && published.diagnostics.len() == 1
          && published.diagnostics.first().is_some_and(|diagnostic| {
            (diagnostic.severity, diagnostic.message.as_str()) == (Some(DiagnosticSeverity::HINT), "this document has been excluded")
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }
}
