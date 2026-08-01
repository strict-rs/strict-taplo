use std::ops::Range;

use codespan_reporting::diagnostic::Diagnostic;
use codespan_reporting::diagnostic::Label;
use codespan_reporting::diagnostic::LabelStyle;
use codespan_reporting::files;
use codespan_reporting::files::SimpleFile;
use codespan_reporting::term;
use codespan_reporting::term::Styles;
use codespan_reporting::term::StylesWriter;
use codespan_reporting::term::termcolor::Ansi;
use itertools::Itertools as _;
use taplo::dom;
use taplo::dom::node::Key;
use taplo::parser;
use taplo::rowan::TextRange;
use taplo_common::environment::LocalEnvironment;
#[cfg(feature = "lint")]
use taplo_common::schema::NodeValidationError;
use tokio::io::AsyncWriteExt as _;

use crate::CliError;
use crate::LocalCommandFuture;
use crate::Taplo;

/// Render and write parser diagnostics for one source file.
pub(crate) fn print_parse_errors<'operation, E: LocalEnvironment>(
  taplo: &'operation Taplo<E>,
  file: &'operation SimpleFile<&str, &str>,
  errors: &'operation [parser::Diagnostic],
) -> LocalCommandFuture<'operation, Result<(), CliError>> {
  Box::pin(async move {
    let out = render_diagnostics(taplo.colors, file, &parse_error_diagnostics(errors)?)?;
    write_stderr(taplo, &out).await
  })
}

/// Render and write semantic diagnostics for one source file.
pub(crate) fn print_semantic_errors<'operation, E, I>(
  taplo: &'operation Taplo<E>,
  file: &'operation SimpleFile<&str, &str>,
  errors: I,
) -> LocalCommandFuture<'operation, Result<(), CliError>>
where
  E: LocalEnvironment,
  I: Iterator<Item = dom::Diagnostic> + 'operation,
{
  Box::pin(async move {
    let diagnostics = errors
      .map(|error| dom_error_diagnostic(&error))
      .collect::<Result<Vec<_>, _>>()?;
    let out = render_diagnostics(taplo.colors, file, &diagnostics)?;
    write_stderr(taplo, &out).await
  })
}

/// Render and write schema-validation diagnostics for one source file.
#[cfg(feature = "lint")]
pub(crate) fn print_schema_errors<'operation, E: LocalEnvironment>(
  taplo: &'operation Taplo<E>,
  file: &'operation SimpleFile<&str, &str>,
  errors: &'operation [NodeValidationError],
) -> LocalCommandFuture<'operation, Result<(), CliError>> {
  Box::pin(async move {
    let out = render_diagnostics(taplo.colors, file, &schema_error_diagnostics(errors)?)?;
    write_stderr(taplo, &out).await
  })
}

/// Write rendered diagnostics to the host error stream.
fn write_stderr<'operation, E: LocalEnvironment>(
  taplo: &'operation Taplo<E>,
  bytes: &'operation [u8],
) -> LocalCommandFuture<'operation, Result<(), CliError>> {
  Box::pin(async move {
    let mut stderr = taplo.env.stderr();
    stderr.write_all(bytes).await?;
    stderr.flush().await?;
    Ok(())
  })
}

/// Convert parser diagnostics into the codespan reporting model.
pub(crate) fn parse_error_diagnostics(errors: &[parser::Diagnostic]) -> Result<Vec<Diagnostic<()>>, CliError> {
  errors
    .iter()
    .unique_by(|error| error.range())
    .map(|error| {
      Ok(Diagnostic::error().with_message("invalid TOML").with_labels(Vec::from([
        Label::primary((), std_range(error.range())?).with_message(error.message()),
      ])))
    })
    .collect()
}

/// Convert one semantic diagnostic into the codespan reporting model.
pub(crate) fn dom_error_diagnostic(error: &dom::Diagnostic) -> Result<Diagnostic<()>, CliError> {
  Ok(match *error {
    dom::Diagnostic::ConflictingKeys {
      ref key,
      ref other,
    } => keyed_diagnostic(
      error,
      key_label(LabelStyle::Primary, key, "duplicate key")?,
      key_label(LabelStyle::Secondary, other, "duplicate found here")?,
    ),
    dom::Diagnostic::ExpectedTable {
      ref not_table,
      ref required_by,
    } => keyed_diagnostic(
      error,
      key_label(LabelStyle::Primary, not_table, "expected table")?,
      key_label(LabelStyle::Secondary, required_by, "required by this key")?,
    ),
    dom::Diagnostic::ExpectedArrayOfTables {
      ref not_array_of_tables,
      ref required_by,
    } => keyed_diagnostic(
      error,
      key_label(LabelStyle::Primary, not_array_of_tables, "expected array of tables")?,
      key_label(LabelStyle::Secondary, required_by, "required by this key")?,
    ),
    dom::Diagnostic::InvalidEscapeSequence {
      ref string,
    } => Diagnostic::error().with_message(error.to_string()).with_labels(Vec::from([
      Label::primary((), std_range(string.text_range())?).with_message("the string contains invalid escape sequences")
    ])),
    dom::Diagnostic::MalformedScalar(ref malformed) => {
      Diagnostic::error()
        .with_message(error.to_string())
        .with_labels(Vec::from([
          Label::primary((), std_range(malformed.syntax().text_range())?).with_message("the scalar value is malformed")
        ]))
    }
    dom::Diagnostic::UnexpectedSyntax {
      ref syntax,
    } => {
      Diagnostic::error().with_message("unexpected syntax").with_labels(Vec::from([
        Label::primary((), std_range(syntax.text_range())?).with_message("unexpected syntax")
      ]))
    }
  })
}

/// Convert schema-validation failures into the codespan reporting model.
#[cfg(feature = "lint")]
pub(crate) fn schema_error_diagnostics(errors: &[NodeValidationError]) -> Result<Vec<Diagnostic<()>>, CliError> {
  errors
    .iter()
    .map(|error| message_diagnostics(&error.message, error.text_ranges()))
    .collect::<Result<Vec<_>, _>>()
    .map(|groups| groups.into_iter().flatten().collect())
}

/// Render one diagnostic for each source range attached to a message.
#[cfg(feature = "lint")]
pub(crate) fn message_diagnostics(message: &str, ranges: impl IntoIterator<Item = TextRange>) -> Result<Vec<Diagnostic<()>>, CliError> {
  ranges
    .into_iter()
    .map(|range| {
      Ok(
        Diagnostic::error()
          .with_message(message)
          .with_labels(Vec::from([Label::primary((), std_range(range)?).with_message(message)])),
      )
    })
    .collect()
}

/// Render codespan diagnostics into ANSI or plain bytes.
pub(crate) fn render_diagnostics(
  colors: bool,
  file: &SimpleFile<&str, &str>,
  diagnostics: &[Diagnostic<()>],
) -> Result<Vec<u8>, files::Error> {
  let config = term::Config::default();
  let mut output = Vec::<u8>::new();
  if colors {
    let styles = Styles::default();
    for diagnostic in diagnostics {
      term::emit_to_write_style(&mut StylesWriter::new(Ansi::new(&mut output), &styles), &config, file, diagnostic)?;
    }
  } else {
    for diagnostic in diagnostics {
      term::emit_to_io_write(&mut output, &config, file, diagnostic)?;
    }
  }
  Ok(output)
}

/// Build a two-location semantic diagnostic.
fn keyed_diagnostic(error: &dom::Diagnostic, primary: Option<Label<()>>, secondary: Option<Label<()>>) -> Diagnostic<()> {
  Diagnostic::error()
    .with_message(error.to_string())
    .with_labels(primary.into_iter().chain(secondary).collect())
}

/// Convert the first source range for one key into an optional label.
fn key_label(style: LabelStyle, key: &Key, message: &str) -> Result<Option<Label<()>>, CliError> {
  key
    .text_ranges()
    .next()
    .map(|text_range| std_range(text_range).map(|host_range| Label::new(style, (), host_range).with_message(message)))
    .transpose()
}

/// Convert a Rowan text range into host-sized diagnostic offsets.
fn std_range(range: TextRange) -> Result<Range<usize>, CliError> {
  let start_offset = u32::from(range.start());
  let end_offset = u32::from(range.end());
  let start = usize::try_from(start_offset).map_err(|source| CliError::CoordinateOverflow {
    offset: start_offset,
    source,
  })?;
  let end = usize::try_from(end_offset).map_err(|source| CliError::CoordinateOverflow {
    offset: end_offset,
    source,
  })?;
  Ok(start..end)
}
