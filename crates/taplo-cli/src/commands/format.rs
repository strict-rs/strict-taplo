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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
  use std::path::Path;

  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_contains;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_lacks;

  use super::append_equal_context;
  use super::render_diff;

  #[test]
  fn equal_diff_context_preserves_short_interiors_and_bounds_long_edges() -> Result<(), TestFailure> {
    let mut visible = Vec::new();
    let short = ["middle-a", "middle-b"];
    let short_length = append_equal_context(&short, 1, 3, &mut visible);
    ensure_eq(
      &short_length,
      &short.len(),
      "a short unchanged segment between edits must remain complete",
    )?;
    ensure(
      visible.iter().map(String::as_str).collect::<Vec<_>>() == short,
      "a short interior context segment must retain every unchanged line in order",
    )?;

    let long = [
      "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve", "thirteen", "fourteen",
      "fifteen",
    ];
    visible.clear();
    let leading_length = append_equal_context(&long, 0, 2, &mut visible);
    ensure_eq(
      &leading_length,
      &super::DIFF_CONTEXT_LINES,
      "leading unchanged content must expose only the trailing context window",
    )?;
    ensure(
      visible.iter().map(String::as_str).collect::<Vec<_>>() == ["nine", "ten", "eleven", "twelve", "thirteen", "fourteen", "fifteen"],
      "leading context must retain the lines closest to the following edit",
    )?;

    visible.clear();
    let trailing_length = append_equal_context(&long, 1, 2, &mut visible);
    ensure_eq(
      &trailing_length,
      &super::DIFF_CONTEXT_LINES,
      "trailing unchanged content must expose only the leading context window",
    )?;
    ensure(
      visible.iter().map(String::as_str).collect::<Vec<_>>() == ["zero", "one", "two", "three", "four", "five", "six"],
      "trailing context must retain the lines closest to the preceding edit",
    )?;

    visible.clear();
    let middle_length = append_equal_context(&long, 1, 3, &mut visible);
    ensure_eq(
      &middle_length,
      &super::DIFF_CONTEXT_LINES.saturating_mul(2),
      "a long interior segment must expose one context window beside each edit",
    )?;
    ensure(
      visible.iter().map(String::as_str).collect::<Vec<_>>()
        == [
          "zero", "one", "two", "three", "four", "five", "six", "nine", "ten", "eleven", "twelve", "thirteen", "fourteen", "fifteen",
        ],
      "long interior context must retain both edit-adjacent windows without the distant middle",
    )
  }

  #[test]
  fn rendered_diffs_distinguish_insert_remove_replace_context_and_color() -> Result<(), TestFailure> {
    let path = Path::new("nested/document.toml");
    let inserted = render_diff(path, "keep\n", "keep\nadded\n", false);
    ensure_contains(
      &inserted,
      "diff a/nested/document.toml b/nested/document.toml",
      "diff output must identify both sides of the selected path",
    )?;
    ensure_contains(&inserted, "+added", "an insertion must be rendered with the added-line prefix")?;
    ensure_lacks(&inserted, "-added", "an insertion must not fabricate a removed counterpart")?;
    ensure_contains(
      &inserted,
      "keep",
      "an edit beside unchanged content must retain its bounded context",
    )?;

    let removed = render_diff(path, "keep\nremoved\n", "keep\n", false);
    ensure_contains(&removed, "-removed", "a removal must be rendered with the removed-line prefix")?;
    ensure_lacks(&removed, "+removed", "a removal must not fabricate an inserted counterpart")?;

    let replaced = render_diff(path, "old\n", "new\n", true);
    ensure_contains(
      &replaced,
      "\u{1b}[31m-old\u{1b}[0m",
      "colored replacement output must style the removed line in red",
    )?;
    ensure_contains(
      &replaced,
      "\u{1b}[32m+new\u{1b}[0m",
      "colored replacement output must style the inserted line in green",
    )?;
    ensure(
      [replaced.contains("\n-old\n"), replaced.contains("\n+new\n")] == [false, false],
      "colored replacement output must not duplicate unstyled edit lines",
    )
  }
}
