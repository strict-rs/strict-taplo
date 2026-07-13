use std::ops::Range;

use codespan_reporting::diagnostic::Diagnostic;
use codespan_reporting::diagnostic::Label;
use codespan_reporting::diagnostic::LabelStyle;
use codespan_reporting::files::SimpleFile;
use codespan_reporting::files::{
  self,
};
use codespan_reporting::term::Styles;
use codespan_reporting::term::StylesWriter;
use codespan_reporting::term::termcolor::Ansi;
use codespan_reporting::term::{
  self,
};
use itertools::Itertools;
use taplo::dom;
use taplo::dom::node::Key;
use taplo::parser;
use taplo::rowan::TextRange;
use taplo_common::environment::Environment;
#[cfg(feature = "lint")]
use taplo_common::schema::NodeValidationError;
use tokio::io::AsyncWriteExt;

use crate::Taplo;

impl<E: Environment> Taplo<E> {
  pub(crate) async fn print_parse_errors(&self, file: &SimpleFile<&str, &str>, errors: &[parser::Error]) -> Result<(), anyhow::Error> {
    let out = render_diagnostics(self.colors, file, &parse_error_diagnostics(errors))?;
    self.write_stderr(&out).await
  }

  pub(crate) async fn print_semantic_errors(
    &self,
    file: &SimpleFile<&str, &str>,
    errors: impl Iterator<Item = dom::Error>,
  ) -> Result<(), anyhow::Error> {
    let diagnostics: Vec<_> = errors.map(|error| dom_error_diagnostic(&error)).collect();
    let out = render_diagnostics(self.colors, file, &diagnostics)?;
    self.write_stderr(&out).await
  }

  #[cfg(feature = "lint")]
  pub(crate) async fn print_schema_errors(
    &self,
    file: &SimpleFile<&str, &str>,
    errors: &[NodeValidationError],
  ) -> Result<(), anyhow::Error> {
    let out = render_diagnostics(self.colors, file, &schema_error_diagnostics(errors))?;
    self.write_stderr(&out).await
  }

  async fn write_stderr(&self, bytes: &[u8]) -> Result<(), anyhow::Error> {
    let mut stderr = self.env.stderr();
    stderr.write_all(bytes).await?;
    stderr.flush().await?;
    Ok(())
  }
}

pub(crate) fn parse_error_diagnostics(errors: &[parser::Error]) -> Vec<Diagnostic<()>> {
  errors
    .iter()
    .unique_by(|e| e.range)
    .map(|error| {
      Diagnostic::error()
        .with_message("invalid TOML")
        .with_labels(Vec::from([Label::primary((), std_range(error.range)).with_message(&error.message)]))
    })
    .collect()
}

pub(crate) fn dom_error_diagnostic(error: &dom::Error) -> Diagnostic<()> {
  match error {
    dom::Error::ConflictingKeys {
      key,
      other,
    } => keyed_diagnostic(
      error,
      key_label(LabelStyle::Primary, key, "duplicate key"),
      key_label(LabelStyle::Secondary, other, "duplicate found here"),
    ),
    dom::Error::ExpectedTable {
      not_table,
      required_by,
    } => keyed_diagnostic(
      error,
      key_label(LabelStyle::Primary, not_table, "expected table"),
      key_label(LabelStyle::Secondary, required_by, "required by this key"),
    ),
    dom::Error::ExpectedArrayOfTables {
      not_array_of_tables,
      required_by,
    } => keyed_diagnostic(
      error,
      key_label(LabelStyle::Primary, not_array_of_tables, "expected array of tables"),
      key_label(LabelStyle::Secondary, required_by, "required by this key"),
    ),
    dom::Error::InvalidEscapeSequence {
      string,
    } => Diagnostic::error().with_message(error.to_string()).with_labels(Vec::from([
      Label::primary((), std_range(string.text_range())).with_message("the string contains invalid escape sequences")
    ])),
    dom::Error::UnexpectedSyntax {
      syntax,
    } => Diagnostic::error().with_message("unexpected syntax").with_labels(Vec::from([
      Label::primary((), std_range(syntax.text_range())).with_message("unexpected syntax")
    ])),
    dom::Error::Query(_) => Diagnostic::error().with_message(error.to_string()),
  }
}

#[cfg(feature = "lint")]
pub(crate) fn schema_error_diagnostics(errors: &[NodeValidationError]) -> Vec<Diagnostic<()>> {
  errors
    .iter()
    .flat_map(|err| message_diagnostics(&err.message, err.text_ranges()))
    .collect()
}

#[cfg(feature = "lint")]
pub(crate) fn message_diagnostics(message: &str, ranges: impl Iterator<Item = TextRange>) -> Vec<Diagnostic<()>> {
  ranges
    .map(|range| {
      Diagnostic::error()
        .with_message(message)
        .with_labels(Vec::from([Label::primary((), std_range(range)).with_message(message)]))
    })
    .collect()
}

pub(crate) fn render_diagnostics(
  colors: bool,
  file: &SimpleFile<&str, &str>,
  diagnostics: &[Diagnostic<()>],
) -> Result<Vec<u8>, files::Error> {
  let config = term::Config::default();
  let mut buf = Vec::<u8>::new();
  if colors {
    let styles = Styles::default();
    for diag in diagnostics {
      term::emit_to_write_style(&mut StylesWriter::new(Ansi::new(&mut buf), &styles), &config, file, diag)?;
    }
  } else {
    for diag in diagnostics {
      term::emit_to_io_write(&mut buf, &config, file, diag)?;
    }
  }
  Ok(buf)
}

fn keyed_diagnostic(error: &dom::Error, primary: Option<Label<()>>, secondary: Option<Label<()>>) -> Diagnostic<()> {
  Diagnostic::error()
    .with_message(error.to_string())
    .with_labels(primary.into_iter().chain(secondary).collect())
}

fn key_label(style: LabelStyle, key: &Key, message: &str) -> Option<Label<()>> {
  key
    .text_ranges()
    .next()
    .map(|range| Label::new(style, (), std_range(range)).with_message(message))
}

fn std_range(range: TextRange) -> Range<usize> {
  let start: usize = u32::from(range.start()) as _;
  let end: usize = u32::from(range.end()) as _;
  start..end
}
