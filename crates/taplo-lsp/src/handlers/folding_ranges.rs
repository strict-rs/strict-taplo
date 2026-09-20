//! Tolerant syntax-to-LSP folding range transformation.

use std::mem::take;

use lsp_types::FoldingRange;
use lsp_types::FoldingRangeKind;
use lsp_types::FoldingRangeParams;
use taplo::dom::Keys;
use taplo::rowan::TextRange;
use taplo::rowan::TextSize;
use taplo::syntax::SyntaxElement;
use taplo::syntax::SyntaxNode;
use taplo::syntax::kind::ARRAY;
use taplo::syntax::kind::COMMENT;
use taplo::syntax::kind::MULTI_LINE_STRING;
use taplo::syntax::kind::MULTI_LINE_STRING_LITERAL;
use taplo::syntax::kind::NEWLINE;
use taplo::syntax::kind::TABLE_ARRAY_HEADER;
use taplo::syntax::kind::TABLE_HEADER;
use taplo::syntax::kind::WHITESPACE;
use taplo_lsp_async::Params;
use taplo_lsp_async::rpc::RpcError;
use taplo_lsp_async::util::Mapper;
use taplo_lsp_async::util::MappingError;

use crate::world::WorldState;

/// Produce tolerant folding ranges for one current document.
///
/// # Errors
///
/// Returns [`RpcError`] when parameters, coordinates, or snapshot freshness cannot be validated.
macro_rules! define_folding_range_future_family {
  (
    $folding_ranges:ident,
    $document_snapshot_for_uri:ident,
    $ensure_current_snapshot:ident;
    $future:ident,
    $environment:path,
    $transport:ident,
    $schema_execution:path;
    [$($value_bound:path),*]
  ) => {
    /// Produce tolerant folding ranges for one current document.
    ///
    /// # Errors
    ///
    /// Returns [`RpcError`] when parameters, coordinates, or snapshot freshness cannot be
    /// validated.
    #[allow(clippy::single_call_fn, reason = "one folding-range entry point per execution family, registered exactly once by its runtime family")]
    pub(super) fn $folding_ranges<E: $environment>(
      world: &WorldState<E, $transport<E>>,
      params: Params<FoldingRangeParams>,
    ) -> $future<'_, Result<Option<Vec<FoldingRange>>, RpcError>> {
      Box::pin(async move {
        let parameters = params.required()?;
        current_document_snapshot!(
          super::$document_snapshot_for_uri(world, &parameters.text_document.uri) => (document_uri, snapshot)
        );
        let Some(syntax) = snapshot.document.dom.syntax().and_then(|syntax| syntax.as_node()) else {
          super::$ensure_current_snapshot(world, &document_uri, &snapshot).await?;
          return Ok(Some(Vec::new()));
        };
        let ranges = create_folding_ranges(syntax, &snapshot.document.mapper).map_err(|error| super::uri::mapping_rpc_error(&error))?;
        super::$ensure_current_snapshot(world, &document_uri, &snapshot).await?;
        Ok(Some(ranges))
      })
    }
  };
}

define_checked_document_handler_execution_families!(
  define_folding_range_future_family;
  (folding_ranges_local, folding_ranges_concurrent),
);

/// Transform syntax-owned regions into checked LSP folding ranges.
///
/// # Errors
///
/// Returns [`MappingError`] when a source range cannot be represented in LSP coordinates.
#[tracing::instrument(skip_all)]
fn create_folding_ranges(syntax: &SyntaxNode, mapper: &Mapper) -> Result<Vec<FoldingRange>, MappingError> {
  let mut ranges = Vec::new();
  let mut table_headers: Vec<(Keys, TextRange)> = Vec::new();
  let mut last_non_header: Option<TextRange> = None;
  let mut comment_start = None;
  let mut last_comment = None;

  for element in syntax.children_with_tokens() {
    if matches!(element.kind(), TABLE_HEADER | TABLE_ARRAY_HEADER) {
      flush_comment_block(mapper, &mut comment_start, &mut last_comment, &mut ranges)?;
      let Some(header) = element.as_node() else {
        continue;
      };
      let Some(key_syntax) = header.first_child() else {
        continue;
      };
      let key = Keys::from_syntax(&key_syntax.into());
      if let Some(content_end) = last_non_header {
        close_completed_table_headers(mapper, &key, content_end, &mut table_headers, &mut ranges)?;
      }
      table_headers.push((key, element.text_range()));
      last_non_header = None;
      continue;
    }

    if let Some(token) = element.as_token() {
      if token.kind() == COMMENT {
        if comment_start.is_none() {
          comment_start = Some(token.text_range());
        }
        last_comment = Some(token.text_range());
      } else {
        let continues_comment = (token.kind() == WHITESPACE && comment_start.is_some())
          || (token.kind() == NEWLINE && comment_start.is_some() && token.text().matches('\n').count() == 1);
        if !continues_comment {
          flush_comment_block(mapper, &mut comment_start, &mut last_comment, &mut ranges)?;
        }
      }
    } else {
      flush_comment_block(mapper, &mut comment_start, &mut last_comment, &mut ranges)?;
    }

    if let Some(node) = element.as_node() {
      collect_multiline_value_ranges(node, mapper, &mut ranges)?;
    }
    if element.kind() != WHITESPACE {
      last_non_header = Some(element.text_range());
    }
  }

  flush_comment_block(mapper, &mut comment_start, &mut last_comment, &mut ranges)?;
  if let Some(content_end) = last_non_header {
    for (_, header) in table_headers {
      if let Some(range) = fold_range(
        mapper,
        TextRange::new(header.start(), content_end.end()),
        FoldingRangeKind::Region,
        false,
      )? {
        ranges.push(range);
      }
    }
  }
  Ok(ranges)
}

/// Close headers that no longer own the next table and retain their ancestors.
#[allow(
  clippy::single_call_fn,
  reason = "header closure owns the fallible fold conversion that cannot run inside Vec::retain's boolean callback"
)]
fn close_completed_table_headers(
  mapper: &Mapper,
  next_key: &Keys,
  content_end: TextRange,
  table_headers: &mut Vec<(Keys, TextRange)>,
  ranges: &mut Vec<FoldingRange>,
) -> Result<(), MappingError> {
  for (open_key, open_range) in take(table_headers) {
    if next_key.contains(&open_key) && *next_key != open_key {
      table_headers.push((open_key, open_range));
      continue;
    }
    if let Some(range) = fold_range(
      mapper,
      TextRange::new(open_range.start(), content_end.end()),
      FoldingRangeKind::Region,
      false,
    )? {
      ranges.push(range);
    }
  }
  Ok(())
}

/// Collect folds for multiline arrays and multiline string tokens beneath one top-level node.
#[allow(
  clippy::single_call_fn,
  reason = "naming the descendant sweep separates value-owned folds from the header and comment-block bookkeeping that \
            `create_folding_ranges` maintains across top-level elements"
)]
fn collect_multiline_value_ranges(node: &SyntaxNode, mapper: &Mapper, ranges: &mut Vec<FoldingRange>) -> Result<(), MappingError> {
  for descendant in node.descendants_with_tokens() {
    let Some(source_range) = multiline_value_range(&descendant) else {
      continue;
    };
    if let Some(range) = fold_range(mapper, source_range, FoldingRangeKind::Region, true)? {
      ranges.push(range);
    }
  }
  Ok(())
}

/// Return the source range of an array or string that spans multiple lines.
#[allow(
  clippy::single_call_fn,
  reason = "the named classifier states the multiline-value rule once for both the node and token halves of a syntax element, so the \
            traversal loop reads as a single foldable-or-not decision"
)]
fn multiline_value_range(element: &SyntaxElement) -> Option<TextRange> {
  if let Some(array) = element.as_node() {
    return (array.kind() == ARRAY && array.descendants_with_tokens().any(|descendant| descendant.kind() == NEWLINE))
      .then(|| array.text_range());
  }
  element.as_token().and_then(|string| {
    (matches!(string.kind(), MULTI_LINE_STRING | MULTI_LINE_STRING_LITERAL) && string.text().contains('\n')).then(|| string.text_range())
  })
}

/// Flush a contiguous comment block only when it spans multiple lines.
fn flush_comment_block(
  mapper: &Mapper,
  start: &mut Option<TextRange>,
  end: &mut Option<TextRange>,
  ranges: &mut Vec<FoldingRange>,
) -> Result<(), MappingError> {
  if let (Some(start_range), Some(end_range)) = (start.take(), end.take())
    && let Some(range) = fold_range(
      mapper,
      TextRange::new(start_range.start(), end_range.end()),
      FoldingRangeKind::Comment,
      false,
    )?
  {
    ranges.push(range);
  }
  Ok(())
}

/// Map a source range to a nonempty multiline LSP fold, safely excluding its terminal byte.
fn fold_range(
  mapper: &Mapper,
  range: TextRange,
  kind: FoldingRangeKind,
  include_characters: bool,
) -> Result<Option<FoldingRange>, MappingError> {
  let Some(terminal) = range.end().checked_sub(TextSize::from(1)) else {
    return Ok(None);
  };
  let start = mapper.position(range.start())?;
  let end = mapper.position(terminal)?;
  if end.line <= start.line {
    return Ok(None);
  }
  Ok(Some(FoldingRange {
    start_line:      start.line,
    start_character: include_characters.then_some(start.character),
    end_line:        end.line,
    end_character:   include_characters.then_some(end.character),
    kind:            Some(kind),
    collapsed_text:  None,
  }))
}

#[cfg(test)]
mod tests {
  use std::fmt::Debug;

  use lsp_types::FoldingRange;
  use lsp_types::FoldingRangeKind;
  use strict_test_support::ensure_that;
  use taplo::parser;
  use taplo::parser::Parse;
  use taplo::parser::ParseFailure;
  use taplo_lsp_async::util::Mapper;
  use taplo_lsp_async::util::MappingError;

  use super::create_folding_ranges;

  /// Complete parser, mapper, and range observations for one source pair.
  #[derive(Debug)]
  struct FoldingObservation {
    /// Lossless parse and recoverable syntax diagnostics.
    parsed: Parse,
    /// Independently constructed coordinate model or its native failure.
    mapper: Result<Mapper, MappingError>,
    /// Range projection attempted when the coordinate model was constructed.
    ranges: Option<Result<Vec<FoldingRange>, MappingError>>,
  }

  /// Build folding ranges while retaining their parse, coordinate model, and native failures.
  fn ranges(source: &str, mapper_source: &str) -> Result<FoldingObservation, ParseFailure> {
    let parsed = parser::parse(source)?;
    let syntax = parsed.clone().into_syntax();
    let mapper = Mapper::new_utf16(mapper_source);
    let ranges = mapper
      .as_ref()
      .ok()
      .map(|coordinates| create_folding_ranges(&syntax, coordinates));
    Ok(FoldingObservation {
      parsed,
      mapper,
      ranges,
    })
  }

  /// Project ranges onto the behaviorally relevant line and kind tuple.
  fn line_ranges(ranges: &[FoldingRange]) -> Vec<(u32, u32, Option<FoldingRangeKind>)> {
    ranges
      .iter()
      .map(|range| (range.start_line, range.end_line, range.kind.clone()))
      .collect()
  }

  #[test]
  fn table_folds_respect_dotted_ancestry_not_textual_prefixes() -> Result<(), impl Debug> {
    let source = "[a]\nx = 1\n[a.b]\ny = 2\n[ab]\nz = 3\n[a]\nw = 4\n";
    ensure_that(
      ranges(source, source),
      "nested headers must stay open while equal, sibling, and textual-prefix headers close",
      |result| {
        let Ok(ref observed) = *result else {
          return false;
        };
        observed.parsed.diagnostics().is_empty()
          && observed.mapper.is_ok()
          && observed
            .ranges
            .as_ref()
            .and_then(|mapped| mapped.as_ref().ok())
            .is_some_and(|actual| {
              line_ranges(actual)
                == vec![
                  (0, 3, Some(FoldingRangeKind::Region)),
                  (2, 3, Some(FoldingRangeKind::Region)),
                  (4, 5, Some(FoldingRangeKind::Region)),
                  (6, 7, Some(FoldingRangeKind::Region)),
                ]
            })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn multiline_values_and_comment_blocks_fold_but_single_lines_do_not() -> Result<(), impl Debug> {
    let source = concat!(
      "single = [1]\n", "multi = [\n", "  1,\n", "]\n", "one = \"x\"\n", "text = \"\"\"\n", "x\n", "\"\"\"\n", "# first\n", "# second\n",
      "\n", "# isolated\n", "value = 1\n",
    );
    ensure_that(
      ranges(source, source),
      "multiline values and contiguous comments must preserve fold kinds and endpoint precision",
      |result| {
        let Ok(ref observed) = *result else {
          return false;
        };
        observed
          .ranges
          .as_ref()
          .and_then(|mapped| mapped.as_ref().ok())
          .is_some_and(|actual| {
            line_ranges(actual)
              == vec![
                (1, 3, Some(FoldingRangeKind::Region)),
                (5, 7, Some(FoldingRangeKind::Region)),
                (8, 9, Some(FoldingRangeKind::Comment)),
              ]
              && actual
                .iter()
                .take(2)
                .all(|range| (range.start_character.is_some(), range.end_character.is_some()) == (true, true))
              && actual.get(2).map(|range| (range.start_character, range.end_character)) == Some((None, None))
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn incomplete_syntax_is_skipped_and_unmappable_ranges_are_typed_failures() -> Result<(), impl Debug> {
    let incomplete = "array = [\n";
    let multiline = "array = [\n  1,\n]\n";
    ensure_that(
      (ranges(incomplete, incomplete), ranges(multiline, ""), ranges("", "")),
      "incomplete and empty syntax must not fabricate folds while missing mapper endpoints retain their typed failure",
      |observed| {
        observed.0.as_ref().is_ok_and(|incomplete_ranges| {
          incomplete_ranges
            .ranges
            .as_ref()
            .is_some_and(|mapped| mapped.as_ref().is_ok_and(Vec::is_empty))
        }) && observed
          .1
          .as_ref()
          .is_ok_and(|unmappable| matches!(unmappable.ranges, Some(Err(MappingError::OffsetOutOfBounds { .. }))))
          && observed.2.as_ref().is_ok_and(|empty| {
            empty
              .ranges
              .as_ref()
              .is_some_and(|mapped| mapped.as_ref().is_ok_and(Vec::is_empty))
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }
}
