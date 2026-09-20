use std::fmt::Debug;
#[cfg(feature = "lint")]
use std::iter::empty;
use std::slice::from_ref;
use std::string::FromUtf8Error;

use codespan_reporting::diagnostic::Diagnostic;
use codespan_reporting::diagnostic::LabelStyle;
use codespan_reporting::files;
use codespan_reporting::files::SimpleFile;
use strict_test_support::PredicateFailure;
use strict_test_support::ensure_that;
use taplo::dom;
use taplo::parser;
use taplo::parser::Parse;
use taplo::parser::ParseFailure;
#[cfg(feature = "lint")]
use taplo::rowan::TextRange;
#[cfg(feature = "lint")]
use taplo::rowan::TextSize;
use thiserror::Error;

use crate::CliError;
use crate::printing::dom_error_diagnostic;
#[cfg(feature = "lint")]
use crate::printing::message_diagnostics;
use crate::printing::parse_error_diagnostics;
use crate::printing::render_diagnostics;
#[cfg(feature = "lint")]
use crate::printing::schema_error_diagnostics;

/// ANSI control-sequence prefix expected only from colored rendering.
const ESC: &str = "\u{1b}[";

/// Native failures at the byte-rendering and text-decoding boundaries.
#[derive(Debug, Error)]
enum RenderingError {
  /// Codespan rendering failed.
  #[error(transparent)]
  Render(#[from] files::Error),
  /// Rendered bytes could not be decoded as UTF-8.
  #[error(transparent)]
  Utf8(#[from] FromUtf8Error),
}

/// Render complete diagnostics through the public byte boundary.
fn render(source: &str, diagnostics: &[Diagnostic<()>], colored: bool) -> Result<String, RenderingError> {
  Ok(String::from_utf8(render_diagnostics(
    colored,
    &SimpleFile::new("test.toml", source),
    diagnostics,
  )?)?)
}

/// Complete ordered diagnostic collection returned by the CLI mapping boundary.
type MappedDiagnostics = Result<Vec<Diagnostic<()>>, CliError>;

/// Complete parse and host-diagnostic mapping observations.
#[derive(Debug)]
struct ParseDiagnostics {
  /// Native parser result, including recoverable syntax diagnostics.
  parsed: Result<Parse, ParseFailure>,
  /// Mapping attempted when a tree could be constructed.
  mapped: Option<MappedDiagnostics>,
}

/// Parse one fixture and retain both native results.
fn parse_diagnostics(source: &str) -> ParseDiagnostics {
  let parsed = parser::parse(source);
  let mapped = parsed.as_ref().ok().map(|parse| parse_error_diagnostics(parse.diagnostics()));
  ParseDiagnostics {
    parsed,
    mapped,
  }
}

/// Complete parsed syntax diagnostics, DOM owner, and semantic validation result.
type Validation = (Vec<parser::Diagnostic>, dom::Node, Result<(), Vec<dom::Diagnostic>>);

/// DOM validation and the selected diagnostic converted for rendering.
#[derive(Debug)]
struct DomDiagnostic {
  /// Syntax and semantic evidence owned by the fixture.
  validation: Result<Validation, ParseFailure>,
  /// Selected native semantic diagnostic's host representation.
  mapped:     Option<Result<Diagnostic<()>, CliError>>,
}

/// Select a semantic diagnostic only from a syntax-clean fixture while retaining the complete
/// source evidence.
fn select_dom_diagnostic(source: &str, selected: impl Fn(&dom::Diagnostic) -> bool) -> DomDiagnostic {
  let validation = parser::parse(source).map(|parse| {
    let diagnostics = parse.diagnostics().to_vec();
    let root = parse.into_dom();
    let result = root.validate();
    (diagnostics, root, result)
  });
  let mapped = validation
    .as_ref()
    .ok()
    .filter(|observed| observed.0.is_empty())
    .and_then(|observed| observed.2.as_ref().err())
    .and_then(|errors| errors.iter().find(|error| selected(error)))
    .map(dom_error_diagnostic);
  DomDiagnostic {
    validation,
    mapped,
  }
}

/// Require a selected structural diagnostic to retain both source labels and its full native
/// evidence.
fn ensure_two_label_diagnostic(
  source: &str,
  selected: impl Fn(&dom::Diagnostic) -> bool,
  expected: &str,
) -> Result<DomDiagnostic, Box<PredicateFailure<DomDiagnostic>>> {
  ensure_that(
    select_dom_diagnostic(source, selected),
    "the structural diagnostic must retain its header, offender, and requirer labels",
    |actual| {
      actual.validation.as_ref().is_ok_and(|parsed| parsed.0.is_empty())
        && actual.mapped.as_ref().is_some_and(|result| {
          result
            .as_ref()
            .is_ok_and(|diagnostic| diagnostic.message == expected && diagnostic.labels.len() == 2)
        })
    },
  )
  .map_err(Box::new)
}

#[test]
fn parse_errors_map_to_invalid_toml_diagnostics() -> Result<(), impl Debug> {
  ensure_that(
    parse_diagnostics("x = "),
    "parse failures must map to a primary invalid-TOML diagnostic carrying the parser message",
    |actual| {
      let Some(Ok(ref diagnostics)) = actual.mapped else {
        return false;
      };
      let Some(diagnostic) = diagnostics.first() else {
        return false;
      };
      actual.parsed.as_ref().is_ok_and(|parse| !parse.diagnostics().is_empty())
        && diagnostic.message == "invalid TOML"
        && diagnostic
          .labels
          .first()
          .is_some_and(|label| label.style == LabelStyle::Primary && !label.message.is_empty())
    },
  )
  .map(drop)
  .map_err(Box::new)
}

#[test]
fn clean_source_yields_no_parse_diagnostics() -> Result<(), impl Debug> {
  ensure_that(
    parse_diagnostics("x = 1\n"),
    "clean source must retain an empty parser and host diagnostic collection",
    |actual| {
      actual.parsed.as_ref().is_ok_and(|parse| parse.diagnostics().is_empty())
        && actual
          .mapped
          .as_ref()
          .is_some_and(|result| result.as_ref().is_ok_and(Vec::is_empty))
    },
  )
  .map(drop)
  .map_err(Box::new)
}

#[test]
fn parse_errors_with_identical_ranges_dedup_to_one() -> Result<(), impl Debug> {
  let parsed = parser::parse("x = ");
  let errors = parsed
    .as_ref()
    .ok()
    .and_then(|parse| parse.diagnostics().first())
    .map(|error| [error.clone(), error.clone()]);
  let mapped = errors.as_ref().map(|diagnostics| parse_error_diagnostics(diagnostics));
  ensure_that(
    (parsed, errors, mapped),
    "identical parser ranges must produce exactly one diagnostic",
    |actual| {
      actual
        .2
        .as_ref()
        .is_some_and(|result| result.as_ref().is_ok_and(|diagnostics| diagnostics.len() == 1))
    },
  )
  .map(drop)
  .map_err(Box::new)
}

#[test]
fn parse_errors_with_distinct_ranges_stay_separate() -> Result<(), impl Debug> {
  let parsed = parser::parse("x = @\ny = $\n");
  let errors = parsed.as_ref().ok().and_then(|parse| {
    parse.diagnostics().first().and_then(|first| {
      parse
        .diagnostics()
        .iter()
        .find(|error| error.range() != first.range())
        .map(|second| [first.clone(), second.clone()])
    })
  });
  let mapped = errors.as_ref().map(|diagnostics| parse_error_diagnostics(diagnostics));
  ensure_that(
    (parsed, errors, mapped),
    "distinct parser ranges must remain two separate diagnostics",
    |actual| {
      actual
        .2
        .as_ref()
        .is_some_and(|result| result.as_ref().is_ok_and(|diagnostics| diagnostics.len() == 2))
    },
  )
  .map(drop)
  .map_err(Box::new)
}

#[test]
fn colored_rendering_emits_ansi_styles() -> Result<(), impl Debug> {
  let diagnostics = parse_diagnostics("x = ");
  let rendered = diagnostics
    .mapped
    .as_ref()
    .and_then(|mapped| mapped.as_ref().ok())
    .map(|mapped| render("x = ", mapped, true));
  ensure_that(
    (diagnostics, rendered),
    "colored parse diagnostics must retain their header and ANSI styles",
    |actual| {
      actual.1.as_ref().is_some_and(|result| {
        result
          .as_ref()
          .is_ok_and(|text| text.contains(ESC) && text.contains("invalid TOML"))
      })
    },
  )
  .map(drop)
  .map_err(Box::new)
}

#[test]
fn plain_rendering_carries_text_without_ansi_styles() -> Result<(), impl Debug> {
  let diagnostics = parse_diagnostics("x = ");
  let rendered = diagnostics
    .mapped
    .as_ref()
    .and_then(|mapped| mapped.as_ref().ok())
    .map(|mapped| render("x = ", mapped, false));
  ensure_that(
    (diagnostics, rendered),
    "plain parse diagnostics must retain their header without ANSI styles",
    |actual| {
      actual.1.as_ref().is_some_and(|result| {
        result
          .as_ref()
          .is_ok_and(|text| !text.contains(ESC) && text.contains("invalid TOML"))
      })
    },
  )
  .map(drop)
  .map_err(Box::new)
}

#[test]
fn conflicting_keys_diagnostic_carries_both_labels() -> Result<(), impl Debug> {
  let observed = select_dom_diagnostic("a = 1\na = 2\n", |diagnostic| {
    matches!(diagnostic, dom::Diagnostic::ConflictingKeys { .. })
  });
  ensure_that(observed, "conflicting keys must retain both labeled occurrences in primary-then-secondary order", |actual| {
    actual.mapped.as_ref().is_some_and(|result| result.as_ref().is_ok_and(|diagnostic| diagnostic.message == "conflicting keys"
      && matches!(diagnostic.labels.as_slice(), [primary, secondary] if primary.style == LabelStyle::Primary && secondary.style == LabelStyle::Secondary)))
  }).map(drop).map_err(Box::new)
}

#[test]
fn conflicting_keys_render_names_file_line_and_roles() -> Result<(), impl Debug> {
  let source = "a = 1\na = 2\n";
  let observed = select_dom_diagnostic(source, |diagnostic| matches!(diagnostic, dom::Diagnostic::ConflictingKeys { .. }));
  let rendered = observed
    .mapped
    .as_ref()
    .and_then(|result| result.as_ref().ok())
    .map(|diagnostic| render(source, from_ref(diagnostic), false));
  ensure_that(
    (observed, rendered),
    "conflicting-key rendering must retain the header, file line, both roles, and plain presentation",
    |actual| {
      actual.1.as_ref().is_some_and(|result| {
        result.as_ref().is_ok_and(|text| {
          ["conflicting keys", "test.toml:2:", "duplicate key", "duplicate found here"]
            .iter()
            .all(|part| text.contains(part))
            && !text.contains(ESC)
        })
      })
    },
  )
  .map(drop)
  .map_err(Box::new)
}

#[test]
fn expected_table_diagnostic_labels_offender_and_requirer() -> Result<(), impl Debug> {
  ensure_two_label_diagnostic(
    "a = 1\n[a.b]\n",
    |diagnostic| matches!(diagnostic, dom::Diagnostic::ExpectedTable { .. }),
    "expected table",
  )
  .map(drop)
}

#[test]
fn expected_array_of_tables_diagnostic_labels_offender_and_requirer() -> Result<(), impl Debug> {
  ensure_two_label_diagnostic(
    "a = 1\n[[a]]\n",
    |diagnostic| matches!(diagnostic, dom::Diagnostic::ExpectedArrayOfTables { .. }),
    "expected array of tables",
  )
  .map(drop)
}

#[test]
fn unexpected_syntax_renders_instead_of_panicking() -> Result<(), impl Debug> {
  let parsed = parser::parse("a = 1").map(|parse| dom::Diagnostic::UnexpectedSyntax {
    syntax: parse.into_syntax().into(),
  });
  let mapped = parsed.as_ref().ok().map(dom_error_diagnostic);
  let rendered = mapped
    .as_ref()
    .and_then(|result| result.as_ref().ok())
    .map(|diagnostic| render("a = 1", from_ref(diagnostic), false));
  ensure_that(
    (parsed, mapped, rendered),
    "unexpected syntax must retain its single source label and render its diagnostic header",
    |actual| {
      actual.1.as_ref().is_some_and(|result| {
        result
          .as_ref()
          .is_ok_and(|diagnostic| diagnostic.message == "unexpected syntax" && diagnostic.labels.len() == 1)
      }) && actual
        .2
        .as_ref()
        .is_some_and(|result| result.as_ref().is_ok_and(|text| text.contains("unexpected syntax")))
    },
  )
  .map(drop)
  .map_err(Box::new)
}

#[test]
fn invalid_escape_sequence_diagnostic_labels_the_string() -> Result<(), impl Debug> {
  let parsed = parser::parse("a = 1").map(|parse| dom::Diagnostic::InvalidEscapeSequence {
    string: parse.into_syntax().into(),
  });
  let mapped = parsed.as_ref().ok().map(dom_error_diagnostic);
  ensure_that(
    (parsed, mapped),
    "invalid escapes must retain their diagnostic message and offending string label",
    |actual| {
      actual.1.as_ref().is_some_and(|result| {
        result
          .as_ref()
          .is_ok_and(|diagnostic| diagnostic.message == "the string contains invalid escape sequence(s)" && diagnostic.labels.len() == 1)
      })
    },
  )
  .map(drop)
  .map_err(Box::new)
}

#[test]
fn syntaxless_keys_degrade_to_labelless_diagnostic() -> Result<(), impl Debug> {
  use taplo::dom::node::Key;
  let error = dom::Diagnostic::ConflictingKeys {
    key:   Key::new("a"),
    other: Key::new("a"),
  };
  let mapped = dom_error_diagnostic(&error);
  let rendered = mapped
    .as_ref()
    .ok()
    .map(|diagnostic| render("a = 1", from_ref(diagnostic), false));
  ensure_that(
    (error, mapped, rendered),
    "syntaxless keys must retain a renderable conflicting-keys header with no fabricated source labels",
    |actual| {
      actual.1.as_ref().is_ok_and(|diagnostic| diagnostic.labels.is_empty())
        && actual
          .2
          .as_ref()
          .is_some_and(|result| result.as_ref().is_ok_and(|text| text.contains("conflicting keys")))
    },
  )
  .map(drop)
  .map_err(Box::new)
}

#[cfg(feature = "lint")]
#[test]
fn message_diagnostics_yield_one_per_range() -> Result<(), impl Debug> {
  let ranges = [
    TextRange::new(TextSize::from(0), TextSize::from(1)),
    TextRange::new(TextSize::from(2), TextSize::from(3)),
  ];
  let mapped = message_diagnostics("value mismatch", ranges);
  ensure_that(
    (ranges, mapped),
    "message diagnostics must preserve one primary diagnostic per input range",
    |actual| {
      actual.1.as_ref().is_ok_and(|diagnostics| {
        diagnostics.len() == 2
          && diagnostics.first().is_some_and(|diagnostic| {
            diagnostic.message == "value mismatch"
              && diagnostic
                .labels
                .first()
                .is_some_and(|label| label.style == LabelStyle::Primary && label.range == (0..1))
          })
      })
    },
  )
  .map(drop)
  .map_err(Box::new)
}

#[cfg(feature = "lint")]
#[test]
fn empty_schema_errors_yield_no_diagnostics() -> Result<(), impl Debug> {
  let observed = [message_diagnostics("msg", empty::<TextRange>()), schema_error_diagnostics(&[])];
  ensure_that(
    observed,
    "empty message ranges and schema failures must produce empty diagnostic collections",
    |actual| actual.iter().all(|result| result.as_ref().is_ok_and(Vec::is_empty)),
  )
  .map(drop)
  .map_err(Box::new)
}
