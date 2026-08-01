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
  use lsp_types::FoldingRange;
  use lsp_types::FoldingRangeKind;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use taplo::parser;
  use taplo_lsp_async::util::Mapper;
  use taplo_lsp_async::util::MappingError;

  use super::create_folding_ranges;

  /// Build folding ranges from one parsed source and an independently selectable mapper source.
  fn ranges(source: &str, mapper_source: &str) -> Result<Vec<FoldingRange>, TestFailure> {
    let dom = ensure_ok(parser::parse(source), "the folding fixture tree must build")?.into_dom();
    let syntax = ensure_some(
      dom.syntax().and_then(|syntax| syntax.as_node()),
      "the folding fixture must retain root syntax",
    )?;
    let mapper = ensure_ok(Mapper::new_utf16(mapper_source), "the folding mapper must build")?;
    ensure_ok(create_folding_ranges(syntax, &mapper), "the folding ranges must map")
  }

  /// Project ranges onto the behaviorally relevant line and kind tuple.
  fn line_ranges(ranges: &[FoldingRange]) -> Vec<(u32, u32, Option<FoldingRangeKind>)> {
    ranges
      .iter()
      .map(|range| (range.start_line, range.end_line, range.kind.clone()))
      .collect()
  }

  #[test]
  fn table_folds_respect_dotted_ancestry_not_textual_prefixes() -> Result<(), TestFailure> {
    let source = "[a]\nx = 1\n[a.b]\ny = 2\n[ab]\nz = 3\n[a]\nw = 4\n";
    let actual = ranges(source, source)?;
    ensure(
      line_ranges(&actual)
        == vec![
          (0, 3, Some(FoldingRangeKind::Region)),
          (2, 3, Some(FoldingRangeKind::Region)),
          (4, 5, Some(FoldingRangeKind::Region)),
          (6, 7, Some(FoldingRangeKind::Region)),
        ],
      "nested headers must stay open while equal, sibling, and textual-prefix headers close",
    )
  }

  #[test]
  fn multiline_values_and_comment_blocks_fold_but_single_lines_do_not() -> Result<(), TestFailure> {
    let source = concat!(
      "single = [1]\n", "multi = [\n", "  1,\n", "]\n", "one = \"x\"\n", "text = \"\"\"\n", "x\n", "\"\"\"\n", "# first\n", "# second\n",
      "\n", "# isolated\n", "value = 1\n",
    );
    let actual = ranges(source, source)?;
    ensure(
      line_ranges(&actual)
        == vec![
          (1, 3, Some(FoldingRangeKind::Region)),
          (5, 7, Some(FoldingRangeKind::Region)),
          (8, 9, Some(FoldingRangeKind::Comment)),
        ],
      "only multiline arrays, strings, and contiguous comment blocks must fold",
    )?;
    ensure(
      actual
        .iter()
        .take(2)
        .all(|range| (range.start_character.is_some(), range.end_character.is_some()) == (true, true)),
      "multiline value folds must retain exact character endpoints",
    )?;
    ensure(
      actual.get(2).map(|range| (range.start_character, range.end_character)) == Some((None, None)),
      "comment blocks must remain line-only folds",
    )
  }

  #[test]
  fn incomplete_syntax_is_skipped_and_unmappable_ranges_are_typed_failures() -> Result<(), TestFailure> {
    let incomplete = "array = [\n";
    ensure(
      ranges(incomplete, incomplete)?.is_empty(),
      "an incomplete one-line array must not fabricate a fold",
    )?;

    let multiline = "array = [\n  1,\n]\n";
    let dom = ensure_ok(parser::parse(multiline), "the unmappable folding fixture tree must build")?.into_dom();
    let syntax = ensure_some(
      dom.syntax().and_then(|syntax| syntax.as_node()),
      "the unmappable folding fixture must retain root syntax",
    )?;
    let empty_mapper = ensure_ok(Mapper::new_utf16(""), "the empty folding mapper must build")?;
    let mapping_failure = ensure_some(
      create_folding_ranges(syntax, &empty_mapper).err(),
      "a folding range outside its mapper must return a typed failure",
    )?;
    ensure(
      matches!(mapping_failure, MappingError::OffsetOutOfBounds { .. }),
      "syntax endpoints absent from the mapper must retain the mapping error family",
    )?;
    ensure(ranges("", "")?.is_empty(), "an empty syntax tree must produce no folds")
  }
}
