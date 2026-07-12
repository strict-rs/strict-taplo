use codespan_reporting::{
    diagnostic::{Diagnostic, LabelStyle},
    files::SimpleFile,
};
use strict_test_support::{
    ensure, ensure_contains, ensure_eq, ensure_lacks, ensure_ok, ensure_some, TestFailure,
};
use taplo::{
    dom, parser,
    rowan::{TextRange, TextSize},
};

#[cfg(feature = "lint")]
use crate::printing::{message_diagnostics, schema_error_diagnostics};
use crate::printing::{dom_error_diagnostic, parse_error_diagnostics, render_diagnostics};

const ESC: &str = "\u{1b}[";

fn render_plain(source: &str, diagnostics: &[Diagnostic<()>]) -> Result<String, TestFailure> {
    let file = SimpleFile::new("test.toml", source);
    let bytes = ensure_ok(
        render_diagnostics(false, &file, diagnostics),
        "plain rendering must succeed",
    )?;
    ensure_ok(String::from_utf8(bytes), "rendered output must be valid UTF-8")
}

fn validation_errors(source: &str) -> Result<Vec<dom::Error>, TestFailure> {
    let parse = parser::parse(source);
    ensure(parse.errors.is_empty(), "fixture must parse cleanly")?;
    Ok(match parse.into_dom().validate() {
        Ok(()) => Vec::new(),
        Err(errors) => errors.collect(),
    })
}

fn sample_syntax() -> taplo::syntax::SyntaxElement {
    parser::parse("a = 1").into_syntax().into()
}

#[test]
fn parse_errors_map_to_invalid_toml_diagnostics() -> Result<(), TestFailure> {
    let parse = parser::parse("x = ");
    ensure(!parse.errors.is_empty(), "fixture must produce parse errors")?;
    let diagnostics = parse_error_diagnostics(&parse.errors);
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
    let parse = parser::parse("x = 1\n");
    ensure(parse.errors.is_empty(), "fixture must parse cleanly")?;
    ensure_eq(
        &parse_error_diagnostics(&parse.errors).len(),
        &0,
        "clean source must yield zero diagnostics",
    )
}

#[test]
fn parse_errors_with_identical_ranges_dedup_to_one() -> Result<(), TestFailure> {
    let range = TextRange::new(TextSize::from(0), TextSize::from(1));
    let errors = Vec::from([
        parser::Error {
            range,
            message: "first".into(),
        },
        parser::Error {
            range,
            message: "second".into(),
        },
    ]);
    ensure_eq(
        &parse_error_diagnostics(&errors).len(),
        &1,
        "identical ranges must dedup to one diagnostic",
    )
}

#[test]
fn parse_errors_with_distinct_ranges_stay_separate() -> Result<(), TestFailure> {
    let errors = Vec::from([
        parser::Error {
            range: TextRange::new(TextSize::from(0), TextSize::from(1)),
            message: "first".into(),
        },
        parser::Error {
            range: TextRange::new(TextSize::from(2), TextSize::from(3)),
            message: "second".into(),
        },
    ]);
    ensure_eq(
        &parse_error_diagnostics(&errors).len(),
        &2,
        "distinct ranges must not dedup",
    )
}

#[test]
fn colored_rendering_emits_ansi_styles() -> Result<(), TestFailure> {
    let source = "x = ";
    let diagnostics = parse_error_diagnostics(&parser::parse(source).errors);
    ensure(!diagnostics.is_empty(), "fixture must yield diagnostics")?;
    let file = SimpleFile::new("test.toml", source);
    let bytes = ensure_ok(
        render_diagnostics(true, &file, &diagnostics),
        "colored rendering must succeed",
    )?;
    let text = ensure_ok(String::from_utf8(bytes), "rendered output must be valid UTF-8")?;
    ensure_contains(&text, ESC, "colored output must contain ANSI escape sequences")?;
    ensure_contains(&text, "invalid TOML", "colored output must contain the diagnostic header")
}

#[test]
fn plain_rendering_carries_text_without_ansi_styles() -> Result<(), TestFailure> {
    let source = "x = ";
    let diagnostics = parse_error_diagnostics(&parser::parse(source).errors);
    ensure(!diagnostics.is_empty(), "fixture must yield diagnostics")?;
    let text = render_plain(source, &diagnostics)?;
    ensure_lacks(&text, ESC, "plain output must not contain ANSI escape sequences")?;
    ensure_contains(&text, "invalid TOML", "plain output must contain the diagnostic header")
}

#[test]
fn conflicting_keys_diagnostic_carries_both_labels() -> Result<(), TestFailure> {
    let errors = validation_errors("a = 1\na = 2\n")?;
    let error = ensure_some(
        errors
            .iter()
            .find(|e| matches!(e, dom::Error::ConflictingKeys { .. })),
        "fixture must produce a ConflictingKeys error",
    )?;
    let diagnostic = dom_error_diagnostic(error);
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
            .find(|e| matches!(e, dom::Error::ConflictingKeys { .. })),
        "fixture must produce a ConflictingKeys error",
    )?;
    let text = render_plain(source, &[dom_error_diagnostic(error)])?;
    ensure_contains(&text, "conflicting keys", "rendered header")?;
    ensure_contains(&text, "test.toml:2:", "primary location is the second occurrence")?;
    ensure_contains(&text, "duplicate key", "primary label message")?;
    ensure_contains(&text, "duplicate found here", "secondary label message")?;
    ensure_lacks(&text, ESC, "plain rendering must not contain ANSI escapes")
}

#[test]
fn expected_table_diagnostic_labels_offender_and_requirer() -> Result<(), TestFailure> {
    let errors = validation_errors("a = 1\n[a.b]\n")?;
    let error = ensure_some(
        errors
            .iter()
            .find(|e| matches!(e, dom::Error::ExpectedTable { .. })),
        "fixture must produce an ExpectedTable error",
    )?;
    let diagnostic = dom_error_diagnostic(error);
    ensure_eq(&diagnostic.message.as_str(), &"expected table", "diagnostic header")?;
    ensure_eq(
        &diagnostic.labels.len(),
        &2,
        "offender and requirer must both be labelled",
    )
}

#[test]
fn expected_array_of_tables_diagnostic_labels_offender_and_requirer() -> Result<(), TestFailure> {
    let errors = validation_errors("a = 1\n[[a]]\n")?;
    let error = ensure_some(
        errors
            .iter()
            .find(|e| matches!(e, dom::Error::ExpectedArrayOfTables { .. })),
        "fixture must produce an ExpectedArrayOfTables error",
    )?;
    let diagnostic = dom_error_diagnostic(error);
    ensure_eq(
        &diagnostic.message.as_str(),
        &"expected array of tables",
        "diagnostic header",
    )?;
    ensure_eq(
        &diagnostic.labels.len(),
        &2,
        "offender and requirer must both be labelled",
    )
}

#[test]
fn unexpected_syntax_renders_instead_of_panicking() -> Result<(), TestFailure> {
    let error = dom::Error::UnexpectedSyntax {
        syntax: sample_syntax(),
    };
    let diagnostic = dom_error_diagnostic(&error);
    ensure_eq(&diagnostic.message.as_str(), &"unexpected syntax", "diagnostic header")?;
    ensure_eq(&diagnostic.labels.len(), &1, "the offending span must be labelled")?;
    let text = render_plain("a = 1", &[diagnostic])?;
    ensure_contains(&text, "unexpected syntax", "rendered output must carry the header")
}

#[test]
fn query_error_renders_message_only() -> Result<(), TestFailure> {
    let error = dom::Error::Query(dom::error::QueryError::NotFound);
    let diagnostic = dom_error_diagnostic(&error);
    ensure_eq(
        &diagnostic.message.as_str(),
        &"the key or index was not found",
        "diagnostic header",
    )?;
    ensure_eq(&diagnostic.labels.len(), &0, "query errors have no span")?;
    let text = render_plain("a = 1", &[diagnostic])?;
    ensure_contains(
        &text,
        "the key or index was not found",
        "rendered output must carry the header",
    )
}

#[test]
fn invalid_escape_sequence_diagnostic_labels_the_string() -> Result<(), TestFailure> {
    let error = dom::Error::InvalidEscapeSequence {
        string: sample_syntax(),
    };
    let diagnostic = dom_error_diagnostic(&error);
    ensure_eq(
        &diagnostic.message.as_str(),
        &"the string contains invalid escape sequence(s)",
        "diagnostic header",
    )?;
    ensure_eq(&diagnostic.labels.len(), &1, "the string span must be labelled")
}

#[test]
fn syntaxless_keys_degrade_to_labelless_diagnostic() -> Result<(), TestFailure> {
    use taplo::dom::node::Key;
    let error = dom::Error::ConflictingKeys {
        key: Key::new("a"),
        other: Key::new("a"),
    };
    let diagnostic = dom_error_diagnostic(&error);
    ensure_eq(&diagnostic.labels.len(), &0, "syntax-less keys must yield no labels")?;
    let text = render_plain("a = 1", &[diagnostic])?;
    ensure_contains(
        &text,
        "conflicting keys",
        "label-less diagnostic must still render its header",
    )
}

#[cfg(feature = "lint")]
#[test]
fn message_diagnostics_yield_one_per_range() -> Result<(), TestFailure> {
    let ranges = Vec::from([
        TextRange::new(TextSize::from(0), TextSize::from(1)),
        TextRange::new(TextSize::from(2), TextSize::from(3)),
    ]);
    let diagnostics = message_diagnostics("value mismatch", ranges.into_iter());
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
    ensure_eq(
        &message_diagnostics("msg", core::iter::empty::<TextRange>()).len(),
        &0,
        "no ranges, no diagnostics",
    )?;
    ensure_eq(
        &schema_error_diagnostics(&[]).len(),
        &0,
        "no errors, no diagnostics",
    )
}
