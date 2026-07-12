//! Tolerant syntax-to-LSP folding range transformation.

use crate::world::World;
use lsp_async_stub::{rpc::Error, util::Mapper, Context, Params};
use lsp_types::{FoldingRange, FoldingRangeKind, FoldingRangeParams};
use taplo::{
    dom::{node::DomNode, FromSyntax, Keys},
    rowan::{TextRange, TextSize},
    syntax::{
        SyntaxElement,
        SyntaxKind::{
            ARRAY, COMMENT, MULTI_LINE_STRING, MULTI_LINE_STRING_LITERAL, NEWLINE,
            TABLE_ARRAY_HEADER, TABLE_HEADER, WHITESPACE,
        },
        SyntaxNode,
    },
};
use taplo_common::environment::Environment;

#[tracing::instrument(skip_all)]
pub(crate) async fn folding_ranges<E: Environment>(
    context: Context<World<E>>,
    params: Params<FoldingRangeParams>,
) -> Result<Option<Vec<FoldingRange>>, Error> {
    let params = params.required()?;
    let Some(document_uri) = crate::uri::to_url(&params.text_document.uri) else {
        return Ok(None);
    };
    let Some(snapshot) = context.document_snapshot(&document_uri).await else {
        return Ok(None);
    };
    let Some(syntax) = snapshot
        .document
        .dom
        .syntax()
        .and_then(|syntax| syntax.as_node())
    else {
        return Ok(Some(Vec::new()));
    };
    Ok(Some(create_folding_ranges(
        syntax,
        &snapshot.document.mapper,
    )))
}

/// Create table, multiline value, and contiguous comment folds from tolerant syntax.
#[tracing::instrument(skip_all)]
pub fn create_folding_ranges(syntax: &SyntaxNode, mapper: &Mapper) -> Vec<FoldingRange> {
    let mut ranges = Vec::new();
    let mut table_headers: Vec<(Keys, TextRange)> = Vec::new();
    let mut last_non_header: Option<TextRange> = None;
    let mut comment_start = None;
    let mut last_comment = None;

    for element in syntax.children_with_tokens() {
        if matches!(element.kind(), TABLE_HEADER | TABLE_ARRAY_HEADER) {
            flush_comment_block(
                mapper,
                &mut comment_start,
                &mut last_comment,
                &mut ranges,
            );
            let Some(header) = element.as_node() else {
                continue;
            };
            let Some(key_syntax) = header.first_child() else {
                continue;
            };
            let key = Keys::from_syntax(key_syntax.into());
            if let Some(content_end) = last_non_header {
                table_headers.retain(|(open_key, open_range)| {
                    let stays_open = key.contains(open_key) && key != *open_key;
                    if !stays_open
                        && let Some(range) = fold_range(
                            mapper,
                            TextRange::new(open_range.start(), content_end.end()),
                            FoldingRangeKind::Region,
                            false,
                        )
                    {
                        ranges.push(range);
                    }
                    stays_open
                });
            }
            table_headers.push((key, element.text_range()));
            last_non_header = None;
            continue;
        }

        match &element {
            SyntaxElement::Token(token) if token.kind() == COMMENT => {
                comment_start.get_or_insert(token.text_range());
                last_comment = Some(token.text_range());
            }
            SyntaxElement::Token(token) if token.kind() == WHITESPACE && comment_start.is_some() => {}
            SyntaxElement::Token(token)
                if token.kind() == NEWLINE
                    && comment_start.is_some()
                    && token.text().matches('\n').count() == 1 => {}
            _ => flush_comment_block(
                mapper,
                &mut comment_start,
                &mut last_comment,
                &mut ranges,
            ),
        }

        if let SyntaxElement::Node(node) = &element {
            collect_multiline_value_ranges(node, mapper, &mut ranges);
        }
        if element.kind() != WHITESPACE {
            last_non_header = Some(element.text_range());
        }
    }

    flush_comment_block(
        mapper,
        &mut comment_start,
        &mut last_comment,
        &mut ranges,
    );
    if let Some(content_end) = last_non_header {
        for (_, header) in table_headers {
            if let Some(range) = fold_range(
                mapper,
                TextRange::new(header.start(), content_end.end()),
                FoldingRangeKind::Region,
                false,
            ) {
                ranges.push(range);
            }
        }
    }
    ranges
}

/// Collect folds for multiline arrays and multiline string tokens beneath one top-level node.
fn collect_multiline_value_ranges(
    node: &SyntaxNode,
    mapper: &Mapper,
    ranges: &mut Vec<FoldingRange>,
) {
    for descendant in node.descendants_with_tokens() {
        match descendant {
            SyntaxElement::Node(array)
                if array.kind() == ARRAY
                    && array
                        .descendants_with_tokens()
                        .any(|element| element.kind() == NEWLINE) =>
            {
                if let Some(range) = fold_range(
                    mapper,
                    array.text_range(),
                    FoldingRangeKind::Region,
                    true,
                ) {
                    ranges.push(range);
                }
            }
            SyntaxElement::Token(string)
                if matches!(string.kind(), MULTI_LINE_STRING | MULTI_LINE_STRING_LITERAL)
                    && string.text().contains('\n') =>
            {
                if let Some(range) = fold_range(
                    mapper,
                    string.text_range(),
                    FoldingRangeKind::Region,
                    true,
                ) {
                    ranges.push(range);
                }
            }
            _ => {}
        }
    }
}

/// Flush a contiguous comment block only when it spans multiple lines.
fn flush_comment_block(
    mapper: &Mapper,
    start: &mut Option<TextRange>,
    end: &mut Option<TextRange>,
    ranges: &mut Vec<FoldingRange>,
) {
    if let (Some(start_range), Some(end_range)) = (start.take(), end.take())
        && let Some(range) = fold_range(
            mapper,
            TextRange::new(start_range.start(), end_range.end()),
            FoldingRangeKind::Comment,
            false,
        )
    {
        ranges.push(range);
    }
}

/// Map a source range to a nonempty multiline LSP fold, safely excluding its terminal byte.
fn fold_range(
    mapper: &Mapper,
    range: TextRange,
    kind: FoldingRangeKind,
    include_characters: bool,
) -> Option<FoldingRange> {
    let terminal = range.end().checked_sub(TextSize::from(1))?;
    let start = mapper.position(range.start())?;
    let end = mapper.position(terminal)?;
    let start_line = u32::try_from(start.line).ok()?;
    let end_line = u32::try_from(end.line).ok()?;
    if end_line <= start_line {
        return None;
    }
    Some(FoldingRange {
        start_line,
        start_character: include_characters
            .then(|| u32::try_from(start.character).ok())
            .flatten(),
        end_line,
        end_character: include_characters
            .then(|| u32::try_from(end.character).ok())
            .flatten(),
        kind: Some(kind),
        collapsed_text: None,
    })
}

#[cfg(test)]
mod tests {
    use super::create_folding_ranges;
    use lsp_async_stub::util::Mapper;
    use lsp_types::FoldingRangeKind;
    use strict_test_support::{ensure, ensure_some, TestFailure};
    use taplo::dom::node::DomNode;

    /// Build folding ranges from one parsed source and an independently selectable mapper source.
    fn ranges(
        source: &str,
        mapper_source: &str,
    ) -> Result<Vec<lsp_types::FoldingRange>, TestFailure> {
        let dom = taplo::parser::parse(source).into_dom();
        let syntax = ensure_some(
            dom.syntax().and_then(|syntax| syntax.as_node()),
            "the folding fixture must retain root syntax",
        )?;
        Ok(create_folding_ranges(
            syntax,
            &Mapper::new_utf16(mapper_source, false),
        ))
    }

    /// Project ranges onto the behaviorally relevant line and kind tuple.
    fn line_ranges(
        ranges: &[lsp_types::FoldingRange],
    ) -> Vec<(u32, u32, Option<FoldingRangeKind>)> {
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
    fn multiline_values_and_comment_blocks_fold_but_single_lines_do_not(
    ) -> Result<(), TestFailure> {
        let source = concat!(
            "single = [1]\n",
            "multi = [\n",
            "  1,\n",
            "]\n",
            "one = \"x\"\n",
            "text = \"\"\"\n",
            "x\n",
            "\"\"\"\n",
            "# first\n",
            "# second\n",
            "\n",
            "# isolated\n",
            "value = 1\n",
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
            actual.iter().take(2).all(|range| {
                range.start_character.is_some() && range.end_character.is_some()
            }),
            "multiline value folds must retain exact character endpoints",
        )?;
        ensure(
            actual
                .get(2)
                .is_some_and(|range| range.start_character.is_none() && range.end_character.is_none()),
            "comment blocks must remain line-only folds",
        )
    }

    #[test]
    fn incomplete_or_unmappable_syntax_is_skipped_without_fabricated_ranges(
    ) -> Result<(), TestFailure> {
        let incomplete = "array = [\n";
        ensure(
            ranges(incomplete, incomplete)?.is_empty(),
            "an incomplete one-line array must not fabricate a fold",
        )?;

        let multiline = "array = [\n  1,\n]\n";
        ensure(
            ranges(multiline, "")?.is_empty(),
            "syntax endpoints absent from the mapper must be skipped",
        )?;
        ensure(
            ranges("", "")?.is_empty(),
            "an empty syntax tree must produce no folds",
        )
    }
}
