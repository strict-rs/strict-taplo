#[cfg(feature = "lint")]
use std::iter::empty;

use codespan_reporting::diagnostic::Diagnostic;
use codespan_reporting::diagnostic::LabelStyle;
use codespan_reporting::files::SimpleFile;
use strict_test_support::TestFailure;
use strict_test_support::ensure;
use strict_test_support::ensure_contains;
use strict_test_support::ensure_eq;
use strict_test_support::ensure_lacks;
use strict_test_support::ensure_ok;
use strict_test_support::ensure_some;
use taplo::dom;
use taplo::parser;
use taplo::parser::Parse;
#[cfg(feature = "lint")]
use taplo::rowan::TextRange;
#[cfg(feature = "lint")]
use taplo::rowan::TextSize;
use taplo::syntax::SyntaxElement;

use crate::printing::dom_error_diagnostic;
#[cfg(feature = "lint")]
use crate::printing::message_diagnostics;
use crate::printing::parse_error_diagnostics;
use crate::printing::render_diagnostics;
#[cfg(feature = "lint")]
use crate::printing::schema_error_diagnostics;

const ESC: &str = "\u{1b}[";

fn render_plain(source: &str, diagnostics: &[Diagnostic<()>]) -> Result<String, TestFailure> {
  let file = SimpleFile::new("test.toml", source);
  let bytes = ensure_ok(render_diagnostics(false, &file, diagnostics), "plain rendering must succeed")?;
  ensure_ok(String::from_utf8(bytes), "rendered output must be valid UTF-8")
}

/// Parse one source fixture through the panic-free test vocabulary.
fn parse_fixture(source: &str) -> Result<Parse, TestFailure> {
  ensure_ok(parser::parse(source), "the diagnostic fixture tree must build")
}

/// Parse one fixture and extract its first typed parser diagnostic.
fn parse_with_first_diagnostic(source: &str, context: &'static str) -> Result<(Parse, parser::Diagnostic), TestFailure> {
  let parse = parse_fixture(source)?;
  let diagnostic = ensure_some(parse.diagnostics().first(), context)?.clone();
  Ok((parse, diagnostic))
}

/// Map and render one source fixture that is expected to contain parse diagnostics.
fn render_parse_failure(source: &str, colored: bool) -> Result<String, TestFailure> {
  let parse = parse_fixture(source)?;
  let diagnostics = ensure_ok(
    parse_error_diagnostics(parse.diagnostics()),
    "parse diagnostics must map into host ranges",
  )?;
  ensure(!diagnostics.is_empty(), "the parse-failure fixture must yield diagnostics")?;
  if colored {
    let file = SimpleFile::new("test.toml", source);
    let bytes = ensure_ok(render_diagnostics(true, &file, &diagnostics), "colored rendering must succeed")?;
    ensure_ok(String::from_utf8(bytes), "rendered output must be valid UTF-8")
  } else {
    render_plain(source, &diagnostics)
  }
}

fn validation_errors(source: &str) -> Result<Vec<dom::Diagnostic>, TestFailure> {
  let parse = ensure_ok(parser::parse(source), "fixture syntax tree must build")?;
  ensure(parse.diagnostics().is_empty(), "fixture must parse cleanly")?;
  Ok(match parse.into_dom().validate() {
    Ok(()) => Vec::new(),
    Err(errors) => errors,
  })
}

fn sample_syntax() -> Result<SyntaxElement, TestFailure> {
  parse_fixture("a = 1").map(|parse| parse.into_syntax().into())
}

/// Require one selected DOM diagnostic to retain its two-party source labeling.
fn ensure_two_label_diagnostic(
  source: &str,
  selected: impl Fn(&dom::Diagnostic) -> bool,
  expected_message: &str,
) -> Result<(), TestFailure> {
  let errors = validation_errors(source)?;
  let error = ensure_some(
    errors.iter().find(|diagnostic| selected(diagnostic)),
    "the fixture must produce the selected structural diagnostic",
  )?;
  let diagnostic = ensure_ok(
    dom_error_diagnostic(error),
    "the structural diagnostic ranges must map into host coordinates",
  )?;
  ensure_eq(&diagnostic.message.as_str(), &expected_message, "diagnostic header")?;
  ensure_eq(&diagnostic.labels.len(), &2, "offender and requirer must both be labelled")
}

/// Require one DOM diagnostic to retain its message and single source label.
fn single_label_diagnostic(error: &dom::Diagnostic, expected_message: &str) -> Result<Diagnostic<()>, TestFailure> {
  let diagnostic = ensure_ok(
    dom_error_diagnostic(error),
    "the single-label diagnostic range must map into host coordinates",
  )?;
  ensure_eq(&diagnostic.message.as_str(), &expected_message, "diagnostic header")?;
  ensure_eq(&diagnostic.labels.len(), &1, "the offending span must be labelled")?;
  Ok(diagnostic)
}

/// Require parse-diagnostic range deduplication to produce one exact count.
fn ensure_parse_diagnostic_count(errors: &[parser::Diagnostic], expected: usize, context: &'static str) -> Result<(), TestFailure> {
  let diagnostics = ensure_ok(parse_error_diagnostics(errors), "parse diagnostics must map into host ranges")?;
  ensure_eq(&diagnostics.len(), &expected, context)
}

#[test]
fn parse_errors_map_to_invalid_toml_diagnostics() -> Result<(), TestFailure> {
  let parse = parse_fixture("x = ")?;
  ensure(!parse.diagnostics().is_empty(), "fixture must produce parse errors")?;
  let diagnostics = ensure_ok(
    parse_error_diagnostics(parse.diagnostics()),
    "parse diagnostics must map into host ranges",
  )?;
  ensure(!diagnostics.is_empty(), "parse errors must yield diagnostics")?;
  let first = ensure_some(diagnostics.first(), "first diagnostic must exist")?;
  ensure_eq(&first.message.as_str(), &"invalid TOML", "diagnostic header")?;
  let label = ensure_some(first.labels.first(), "diagnostic must carry a primary label")?;
  ensure(label.style == LabelStyle::Primary, "label must be primary")?;
  ensure(!label.message.is_empty(), "label must carry the parser message")?;
  Ok(())
}

#[test]
fn clean_source_yields_no_parse_diagnostics() -> Result<(), TestFailure> {
  let parse = parse_fixture("x = 1\n")?;
  ensure(parse.diagnostics().is_empty(), "fixture must parse cleanly")?;
  let diagnostics = ensure_ok(
    parse_error_diagnostics(parse.diagnostics()),
    "clean parse diagnostics must map into host ranges",
  )?;
  ensure_eq(&diagnostics.len(), &0, "clean source must yield zero diagnostics")
}

#[test]
fn parse_errors_with_identical_ranges_dedup_to_one() -> Result<(), TestFailure> {
  let (_, error) = parse_with_first_diagnostic("x = ", "the duplicate diagnostic fixture must have one diagnostic")?;
  let errors = Vec::from([error.clone(), error.clone()]);
  ensure_parse_diagnostic_count(&errors, 1, "identical ranges must dedup to one diagnostic")
}

#[test]
fn parse_errors_with_distinct_ranges_stay_separate() -> Result<(), TestFailure> {
  let (parse, first) = parse_with_first_diagnostic("x = @\ny = $\n", "the distinct diagnostic fixture must have a first diagnostic")?;
  let second = ensure_some(
    parse
      .diagnostics()
      .iter()
      .find(|diagnostic| diagnostic.range() != first.range()),
    "the distinct diagnostic fixture must have another diagnostic range",
  )?;
  let errors = Vec::from([first, second.clone()]);
  ensure_parse_diagnostic_count(&errors, 2, "distinct ranges must not dedup")
}

#[test]
fn colored_rendering_emits_ansi_styles() -> Result<(), TestFailure> {
  let text = render_parse_failure("x = ", true)?;
  ensure_contains(&text, ESC, "colored output must contain ANSI escape sequences")?;
  ensure_contains(&text, "invalid TOML", "colored output must contain the diagnostic header")
}

#[test]
fn plain_rendering_carries_text_without_ansi_styles() -> Result<(), TestFailure> {
  let text = render_parse_failure("x = ", false)?;
  ensure_lacks(&text, ESC, "plain output must not contain ANSI escape sequences")?;
  ensure_contains(&text, "invalid TOML", "plain output must contain the diagnostic header")
}

#[test]
fn conflicting_keys_diagnostic_carries_both_labels() -> Result<(), TestFailure> {
  let errors = validation_errors("a = 1\na = 2\n")?;
  let error = ensure_some(
    errors
      .iter()
      .find(|diagnostic| matches!(diagnostic, dom::Diagnostic::ConflictingKeys { .. })),
    "fixture must produce a ConflictingKeys error",
  )?;
  let diagnostic = ensure_ok(dom_error_diagnostic(error), "conflicting-key ranges must map into host coordinates")?;
  ensure_eq(&diagnostic.message.as_str(), &"conflicting keys", "diagnostic header")?;
  ensure_eq(&diagnostic.labels.len(), &2, "both key occurrences must be labelled")?;
  let primary = ensure_some(diagnostic.labels.first(), "primary label must exist")?;
  ensure(primary.style == LabelStyle::Primary, "first label must be primary")?;
  let secondary = ensure_some(diagnostic.labels.get(1), "secondary label must exist")?;
  ensure(secondary.style == LabelStyle::Secondary, "second label must be secondary")?;
  Ok(())
}

#[test]
fn conflicting_keys_render_names_file_line_and_roles() -> Result<(), TestFailure> {
  let source = "a = 1\na = 2\n";
  let errors = validation_errors(source)?;
  let error = ensure_some(
    errors
      .iter()
      .find(|diagnostic| matches!(diagnostic, dom::Diagnostic::ConflictingKeys { .. })),
    "fixture must produce a ConflictingKeys error",
  )?;
  let diagnostic = ensure_ok(
    dom_error_diagnostic(error),
    "rendered conflicting-key ranges must map into host coordinates",
  )?;
  let text = render_plain(source, &[diagnostic])?;
  ensure_contains(&text, "conflicting keys", "rendered header")?;
  ensure_contains(&text, "test.toml:2:", "primary location is the second occurrence")?;
  ensure_contains(&text, "duplicate key", "primary label message")?;
  ensure_contains(&text, "duplicate found here", "secondary label message")?;
  ensure_lacks(&text, ESC, "plain rendering must not contain ANSI escapes")
}

#[test]
fn expected_table_diagnostic_labels_offender_and_requirer() -> Result<(), TestFailure> {
  ensure_two_label_diagnostic(
    "a = 1\n[a.b]\n",
    |diagnostic| matches!(diagnostic, dom::Diagnostic::ExpectedTable { .. }),
    "expected table",
  )
}

#[test]
fn expected_array_of_tables_diagnostic_labels_offender_and_requirer() -> Result<(), TestFailure> {
  ensure_two_label_diagnostic(
    "a = 1\n[[a]]\n",
    |diagnostic| matches!(diagnostic, dom::Diagnostic::ExpectedArrayOfTables { .. }),
    "expected array of tables",
  )
}

#[test]
fn unexpected_syntax_renders_instead_of_panicking() -> Result<(), TestFailure> {
  let error = dom::Diagnostic::UnexpectedSyntax {
    syntax: sample_syntax()?
  };
  let diagnostic = single_label_diagnostic(&error, "unexpected syntax")?;
  let text = render_plain("a = 1", &[diagnostic])?;
  ensure_contains(&text, "unexpected syntax", "rendered output must carry the header")
}

#[test]
fn invalid_escape_sequence_diagnostic_labels_the_string() -> Result<(), TestFailure> {
  let error = dom::Diagnostic::InvalidEscapeSequence {
    string: sample_syntax()?
  };
  drop(single_label_diagnostic(&error, "the string contains invalid escape sequence(s)")?);
  Ok(())
}

#[test]
fn syntaxless_keys_degrade_to_labelless_diagnostic() -> Result<(), TestFailure> {
  use taplo::dom::node::Key;
  let error = dom::Diagnostic::ConflictingKeys {
    key:   Key::new("a"),
    other: Key::new("a"),
  };
  let diagnostic = ensure_ok(dom_error_diagnostic(&error), "syntax-less key diagnostics must remain convertible")?;
  ensure_eq(&diagnostic.labels.len(), &0, "syntax-less keys must yield no labels")?;
  let text = render_plain("a = 1", &[diagnostic])?;
  ensure_contains(&text, "conflicting keys", "label-less diagnostic must still render its header")
}

#[cfg(feature = "lint")]
#[test]
fn message_diagnostics_yield_one_per_range() -> Result<(), TestFailure> {
  let ranges = Vec::from([
    TextRange::new(TextSize::from(0), TextSize::from(1)),
    TextRange::new(TextSize::from(2), TextSize::from(3)),
  ]);
  let diagnostics = ensure_ok(
    message_diagnostics("value mismatch", ranges),
    "schema message ranges must map into host coordinates",
  )?;
  ensure_eq(&diagnostics.len(), &2, "each range must yield a diagnostic")?;
  let first = ensure_some(diagnostics.first(), "first diagnostic must exist")?;
  ensure_eq(&first.message.as_str(), &"value mismatch", "diagnostic header")?;
  let label = ensure_some(first.labels.first(), "diagnostic must carry a primary label")?;
  ensure(label.style == LabelStyle::Primary, "label must be primary")?;
  ensure(label.range == (0..1), "label must carry the source range")?;
  Ok(())
}

#[cfg(feature = "lint")]
#[test]
fn empty_schema_errors_yield_no_diagnostics() -> Result<(), TestFailure> {
  let messages = ensure_ok(
    message_diagnostics("msg", empty::<TextRange>()),
    "empty schema message ranges must remain convertible",
  )?;
  ensure_eq(&messages.len(), &0, "no ranges, no diagnostics")?;
  let diagnostics = ensure_ok(schema_error_diagnostics(&[]), "empty schema failures must remain convertible")?;
  ensure_eq(&diagnostics.len(), &0, "no errors, no diagnostics")
}
