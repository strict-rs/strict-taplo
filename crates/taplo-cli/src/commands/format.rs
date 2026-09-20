//! TOML formatting command implementation.

use std::mem;
use std::path::Path;
use std::path::PathBuf;

use codespan_reporting::files::SimpleFile;
use taplo::formatter;
use taplo::parser;
use taplo_common::config::Config;
use taplo_common::environment::LocalEnvironment;
use taplo_common::util::Normalize as _;
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;

use crate::CliError;
use crate::CliFailure;
use crate::LocalCommandFuture;
use crate::Taplo;
use crate::args::FormatCommand;
use crate::path_text;
use crate::printing::print_parse_errors;

/// Execute one formatting command.
pub(super) fn execute_format<E: LocalEnvironment>(
  taplo: &mut Taplo<E>,
  command: FormatCommand,
) -> LocalCommandFuture<'_, Result<(), CliError>> {
  Box::pin(async move {
    if matches!(command.files.first().map(String::as_str), Some("-")) {
      format_stdin(taplo, command).await
    } else {
      format_files(taplo, command).await
    }
  })
}

/// Format standard input.
#[tracing::instrument(skip_all)]
fn format_stdin<E: LocalEnvironment>(taplo: &mut Taplo<E>, command: FormatCommand) -> LocalCommandFuture<'_, Result<(), CliError>> {
  Box::pin(async move {
    let mut source = String::new();
    let bytes_read = taplo.env.stdin().read_to_string(&mut source).await?;
    tracing::trace!(bytes_read, "read formatting input from standard input");

    let config = taplo.load_config(&command.general).await?;
    let display_path = match command.stdin_filepath.as_deref() {
      Some(filepath) if taplo.env.is_absolute(filepath.as_ref())? => PathBuf::from(filepath).normalize(),
      Some(filepath) => {
        let cwd = taplo.env.cwd_normalized()?.ok_or(CliFailure::WorkingDirectoryRequired)?;
        cwd.join(filepath).normalize()
      }
      None => PathBuf::from("-"),
    };
    let parse = parser::parse(&source)?;

    if !parse.diagnostics().is_empty() {
      print_parse_errors(
        taplo,
        &SimpleFile::new(path_text(&display_path)?, source.as_str()),
        parse.diagnostics(),
      )
      .await?;
      if !command.input.force {
        return Err(CliFailure::FormattingBlocked.into());
      }
    }

    let format_options = format_options(&config, &command, &display_path)?;
    let error_ranges = parse.diagnostics().iter().map(parser::Diagnostic::range).collect::<Vec<_>>();
    let dom = parse.into_dom();
    let formatted = formatter::format_with_path_scopes(&dom, &format_options, &error_ranges, config.format_scopes(&display_path))?;

    if command.output.check {
      if source != formatted {
        return Err(CliFailure::FormattingMismatch.into());
      }
    } else {
      let mut stdout = taplo.env.stdout();
      stdout.write_all(formatted.as_bytes()).await?;
      stdout.flush().await?;
    }
    Ok(())
  })
}

/// Report that diff output is not available to the browser build.
#[cfg(target_arch = "wasm32")]
fn print_diff<E: LocalEnvironment>(
  _taplo: &Taplo<E>,
  _path: &Path,
  _original: &str,
  _formatted: &str,
) -> LocalCommandFuture<'static, Result<(), CliError>> {
  Box::pin(async {
    tracing::warn!("the `--diff` flag is not available in this build");
    Ok(())
  })
}

/// Write one colored unified-style diff.
#[cfg(not(target_arch = "wasm32"))]
fn print_diff<'operation, E: LocalEnvironment>(
  taplo: &'operation Taplo<E>,
  path: &'operation Path,
  original: &'operation str,
  formatted: &'operation str,
) -> LocalCommandFuture<'operation, Result<(), CliError>> {
  Box::pin(async move {
    let rendered = render_diff(path, original, formatted, taplo.colors);
    let mut stdout = taplo.env.stdout();
    stdout.write_all(rendered.as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
  })
}

/// Format every selected file.
#[tracing::instrument(skip_all)]
fn format_files<E: LocalEnvironment>(taplo: &mut Taplo<E>, mut command: FormatCommand) -> LocalCommandFuture<'_, Result<(), CliError>> {
  Box::pin(async move {
    if command.stdin_filepath.is_some() {
      tracing::warn!("using `--stdin-filepath` has no effect unless input comes from stdin");
    }

    let config = taplo.load_config(&command.general).await?;
    let cwd = taplo.env.cwd_normalized()?.ok_or(CliFailure::WorkingDirectoryRequired)?;
    let files = taplo
      .collect_files(&cwd, &config, mem::take(&mut command.files).into_iter())
      .await?;
    let mut result: Result<(), CliError> = Ok(());

    for path in files {
      let format_options = format_options(&config, &command, &path)?;
      let source = String::from_utf8(taplo.env.read_file(&path).await?)?;
      let parse = parser::parse(&source)?;

      if !parse.diagnostics().is_empty() {
        print_parse_errors(taplo, &SimpleFile::new(path_text(&path)?, source.as_str()), parse.diagnostics()).await?;
        if !command.input.force {
          result = Err(CliFailure::FileFormattingFailed.into());
          continue;
        }
      }

      let error_ranges = parse.diagnostics().iter().map(parser::Diagnostic::range).collect::<Vec<_>>();
      let dom = parse.into_dom();
      let formatted = formatter::format_with_path_scopes(&dom, &format_options, &error_ranges, config.format_scopes(&path))?;

      if source == formatted {
        continue;
      }
      if command.output.diff {
        print_diff(taplo, &path, &source, &formatted).await?;
      }
      if command.output.check {
        tracing::error!(?path, "the file is not properly formatted");
        result = Err(CliFailure::FileFormattingFailed.into());
      } else {
        taplo.env.write_file(&path, formatted.as_bytes()).await?;
      }
    }
    result
  })
}

/// Merge configuration and command-line formatting options.
fn format_options(config: &Config, command: &FormatCommand, path: &Path) -> Result<formatter::Options, CliError> {
  let mut format_options = formatter::Options::default();
  config.update_format_options(path, &mut format_options);

  let mut parsed = Vec::with_capacity(command.options.len());
  for option in &command.options {
    let Some((key, option_value)) = option.split_once('=') else {
      return Err(formatter::OptionParseError::InvalidOption(option.clone()).into());
    };
    parsed.push((key, option_value));
  }
  format_options.update_from_str(parsed.into_iter())?;
  Ok(format_options)
}

/// Number of unchanged lines retained around each rendered diff hunk.
#[cfg(not(target_arch = "wasm32"))]
const DIFF_CONTEXT_LINES: usize = 7;

/// Append the visible portion of one unchanged diff segment.
#[cfg(not(target_arch = "wasm32"))]
#[allow(
  clippy::single_call_fn,
  reason = "equal-segment selection isolates boundary and context-window policy from diff operation dispatch"
)]
fn append_equal_context(slices: &[&str], index: usize, hunk_count: usize, accumulated: &mut Vec<String>) -> usize {
  let next_index = index.saturating_add(1);
  let complete_context = DIFF_CONTEXT_LINES.saturating_mul(2);
  if slices.len() < complete_context && index > 0 && next_index < hunk_count {
    accumulated.extend(slices.iter().map(|line| (*line).to_owned()));
    return slices.len();
  }

  let mut visible_length = 0_usize;
  if index > 0 {
    let end = usize::min(DIFF_CONTEXT_LINES, slices.len());
    accumulated.extend(slices.iter().take(end).map(|line| (*line).to_owned()));
    visible_length = visible_length.saturating_add(end);
  }
  if next_index < hunk_count {
    let skip = slices.len().saturating_sub(DIFF_CONTEXT_LINES);
    accumulated.extend(slices.iter().skip(skip).map(|line| (*line).to_owned()));
    visible_length = visible_length.saturating_add(slices.len().saturating_sub(skip));
  }
  visible_length
}

/// Render a compact unified-style line diff.
#[cfg(not(target_arch = "wasm32"))]
fn render_diff(path: &Path, original: &str, formatted: &str, colors: bool) -> String {
  use anstyle::AnsiColor;
  use anstyle::Style;
  use prettydiff::basic::DiffOp;

  /// Apply one optional ANSI style to prefixed lines.
  fn styled_lines(lines: &[&str], prefix: &str, style: Style, colors: bool) -> Vec<String> {
    lines
      .iter()
      .map(|line| {
        let text = format!("{prefix}{line}");
        if colors {
          format!("{style}{text}{style:#}")
        } else {
          text
        }
      })
      .collect()
  }

  let green = Style::new().fg_color(Some(AnsiColor::Green.into()));
  let red = Style::new().fg_color(Some(AnsiColor::Red.into()));
  let line_diff = prettydiff::diff_lines(original, formatted);
  let hunks = line_diff.diff();
  let hunk_count = hunks.len();
  let mut output = format!("diff a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n", path = path.display());
  let mut accumulated = Vec::<String>::with_capacity(hunk_count);
  let mut pre_line = 0_usize;
  let mut post_line = 0_usize;

  for (index, diff_operation) in hunks.into_iter().enumerate() {
    let mut pre_length = 0_usize;
    let mut post_length = 0_usize;
    match diff_operation {
      DiffOp::Equal(slices) => {
        let visible_length = append_equal_context(slices, index, hunk_count, &mut accumulated);
        pre_length = pre_length.saturating_add(visible_length);
        post_length = post_length.saturating_add(visible_length);
      }
      DiffOp::Insert(inserted) => {
        accumulated.extend(styled_lines(inserted, "+", green, colors));
        post_length = post_length.saturating_add(inserted.len());
      }
      DiffOp::Remove(removed) => {
        accumulated.extend(styled_lines(removed, "-", red, colors));
        pre_length = pre_length.saturating_add(removed.len());
      }
      DiffOp::Replace(removed, inserted) => {
        accumulated.extend(styled_lines(removed, "-", red, colors));
        accumulated.extend(styled_lines(inserted, "+", green, colors));
        pre_length = pre_length.saturating_add(removed.len());
        post_length = post_length.saturating_add(inserted.len());
      }
    }

    let hunk = format!(
      "@@ -{pre_line},{pre_length} +{post_line},{post_length} @@\n{}\n",
      accumulated.join("\n")
    );
    output.push_str(&hunk);
    pre_line = pre_line.saturating_add(pre_length);
    post_line = post_line.saturating_add(post_length);
    accumulated.clear();
  }
  output
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
  use std::fmt::Debug;
  use std::path::Path;

  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_that;

  use super::append_equal_context;
  use super::render_diff;

  #[test]
  fn equal_diff_context_preserves_short_interiors_and_bounds_long_edges() -> Result<(), impl Debug> {
    let short = ["middle-a", "middle-b"];
    let long = [
      "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve", "thirteen", "fourteen",
      "fifteen",
    ];
    let observed = [(&short[..], 1, 3), (&long[..], 0, 2), (&long[..], 1, 2), (&long[..], 1, 3)].map(|(lines, index, count)| {
      let mut visible = Vec::new();
      let length = append_equal_context(lines, index, count, &mut visible);
      (length, visible)
    });
    let expected = [
      (short.len(), &short[..]),
      (
        super::DIFF_CONTEXT_LINES,
        &["nine", "ten", "eleven", "twelve", "thirteen", "fourteen", "fifteen"][..],
      ),
      (
        super::DIFF_CONTEXT_LINES,
        &["zero", "one", "two", "three", "four", "five", "six"][..],
      ),
      (
        super::DIFF_CONTEXT_LINES.saturating_mul(2),
        &[
          "zero", "one", "two", "three", "four", "five", "six", "nine", "ten", "eleven", "twelve", "thirteen", "fourteen", "fifteen",
        ][..],
      ),
    ]
    .map(|(length, lines)| (length, lines.iter().map(|line| (*line).to_owned()).collect::<Vec<_>>()));
    ensure_eq(
      observed,
      expected,
      "diff context must preserve complete short interiors and exact windows beside long-segment edits",
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn rendered_diffs_distinguish_insert_remove_replace_context_and_color() -> Result<(), impl Debug> {
    let path = Path::new("nested/document.toml");
    let observed = [
      render_diff(path, "keep\n", "keep\nadded\n", false),
      render_diff(path, "keep\nremoved\n", "keep\n", false),
      render_diff(path, "old\n", "new\n", true),
    ];
    ensure_that(
      observed,
      "rendered diffs must preserve path headers, bounded context, edit polarity, and unique colored replacements",
      |actual| {
        let [ref inserted, ref removed, ref replaced] = *actual;
        inserted.contains("diff a/nested/document.toml b/nested/document.toml")
          && inserted.contains("+added")
          && !inserted.contains("-added")
          && inserted.contains("keep")
          && removed.contains("-removed")
          && !removed.contains("+removed")
          && replaced.contains("\u{1b}[31m-old\u{1b}[0m")
          && replaced.contains("\u{1b}[32m+new\u{1b}[0m")
          && !replaced.contains("\n-old\n")
          && !replaced.contains("\n+new\n")
      },
    )
    .map(drop)
    .map_err(Box::new)
  }
}
