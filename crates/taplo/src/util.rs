//! Shared lexical, syntax-tree, and source-range utilities.
//!
//! This module owns TOML basic-string escape handling, source-character validation, syntax-tree
//! navigation and construction helpers, and range operations used by the parser, DOM, and
//! formatter. It does not own parsing or semantic DOM policy.

use rowan::TextRange;
use rowan::TextSize;

use crate::syntax::SyntaxElement;
use crate::syntax::SyntaxKind;
use crate::syntax::SyntaxNode;

/// TOML basic-string escape encoding, decoding, and typed validation.
mod escape;
/// Fallible syntax-subtree copying into checked Rowan builders.
pub mod syntax;

pub use escape::EscapeError;
pub use escape::EscapeErrorKind;
pub use escape::check_escape;
pub use escape::escape;
pub use escape::unescape;

/// TOML character policy applied after lexical tokenization.
#[derive(Clone, Copy)]
pub(crate) enum CharacterPolicy {
  /// Comment or single-line literal-string characters.
  CommentOrLiteral,
  /// Single-line basic-string characters.
  BasicString,
  /// Multiline basic-string characters.
  MultilineBasicString,
  /// Multiline literal-string characters.
  MultilineLiteralString,
}

impl CharacterPolicy {
  /// Return whether one character satisfies this policy.
  fn permits(self, character: char) -> bool {
    match self {
      Self::CommentOrLiteral => character == '\t' || !character.is_control(),
      Self::BasicString => permits_basic_string(character),
      Self::MultilineBasicString => permits_multiline_basic_string(character),
      Self::MultilineLiteralString => is_multiline_whitespace(character) || !character.is_control(),
    }
  }
}

/// Return whether one character is permitted in a single-line basic string.
fn permits_basic_string(character: char) -> bool {
  character == '\t'
    || !(('\u{0000}'..='\u{0008}').contains(&character) || ('\u{000A}'..='\u{001F}').contains(&character) || character == '\u{007F}')
}

/// Return whether one character is TOML multiline whitespace.
const fn is_multiline_whitespace(character: char) -> bool {
  matches!(character, '\t' | '\n' | '\r')
}

/// Return whether one character is permitted in a multiline basic string.
fn permits_multiline_basic_string(character: char) -> bool {
  is_multiline_whitespace(character) || permits_basic_string(character)
}

/// Return every byte offset whose character violates the selected TOML policy.
pub(crate) fn validate_characters(source: &str, policy: CharacterPolicy) -> Result<(), Vec<usize>> {
  let invalid = source
    .char_indices()
    .filter_map(|(index, character)| (!policy.permits(character)).then_some(index))
    .collect::<Vec<_>>();
  if invalid.is_empty() { Ok(()) } else { Err(invalid) }
}

/// Borrowed-string operations for text with an optional single-character quote pair.
pub trait StrExt {
  /// Remove one matching pair of outer single or double quotes.
  ///
  /// Mismatched or absent delimiters leave the original slice unchanged. The returned slice
  /// preserves its interior verbatim; this operation does not decode escape sequences.
  #[must_use]
  fn strip_quotes(self) -> Self;
}

impl StrExt for &str {
  fn strip_quotes(self) -> Self {
    self
      .strip_prefix('"')
      .and_then(|interior| interior.strip_suffix('"'))
      .or_else(|| self.strip_prefix('\'').and_then(|interior| interior.strip_suffix('\'')))
      .unwrap_or(self)
  }
}

/// Utility extension methods for Syntax Nodes.
pub trait SyntaxExt {
  /// Return a syntax node that contains the given offset.
  fn find_node(&self, offset: TextSize, inclusive: bool) -> Option<SyntaxNode>;

  /// Find the deepest node that contains the given offset.
  fn find_node_deep(&self, offset: TextSize, inclusive: bool) -> Option<SyntaxNode> {
    let mut node = self.find_node(offset, inclusive);
    while let Some(ref syntax_node) = node {
      let new_node = syntax_node.find_node(offset, inclusive);
      if new_node.is_some() {
        node = new_node;
      } else {
        break;
      }
    }

    node
  }

  /// Find a node or token by its kind.
  fn find(&self, kind: SyntaxKind) -> Option<SyntaxElement>;
}

impl SyntaxExt for SyntaxNode {
  fn find_node(&self, offset: TextSize, inclusive: bool) -> Option<SyntaxNode> {
    for descendant in self.descendants().skip(1) {
      let range = descendant.text_range();

      if (inclusive && range.contains_inclusive(offset)) || range.contains(offset) {
        return Some(descendant);
      }
    }

    None
  }

  fn find(&self, kind: SyntaxKind) -> Option<SyntaxElement> {
    self.descendants_with_tokens().find(|element| element.kind() == kind)
  }
}

/// Return one range covering every supplied range, or [`None`] for an empty input.
#[allow(
  clippy::single_call_fn,
  reason = "the public utility defines empty-input and covering-range semantics for source-backed semantic paths"
)]
#[must_use]
pub fn try_join_ranges<I: IntoIterator<Item = TextRange>>(ranges: I) -> Option<TextRange> {
  ranges.into_iter().fold(None, |covered, range| {
    covered.map_or(Some(range), |previous| Some(range.cover(previous)))
  })
}

/// Return whether two source ranges share content, contain one another, or meet at an endpoint.
///
/// Endpoint contact counts as overlap for formatter error protection; only strictly separated
/// ranges return `false`.
#[allow(
  clippy::single_call_fn,
  reason = "the public predicate names the formatter's endpoint-inclusive source-range protection policy"
)]
#[must_use]
pub fn overlaps(range: TextRange, other: TextRange) -> bool {
  range.contains_range(other)
    || other.contains_range(range)
    || range.contains(other.start())
    || range.contains(other.end())
    || other.contains(range.start())
    || other.contains(range.end())
}

#[cfg(test)]
/// Character, string, syntax-navigation, and range utility contracts.
mod tests {
  use rowan::TextRange;
  use rowan::TextSize;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_some;

  use super::CharacterPolicy;
  use super::StrExt as _;
  use super::SyntaxExt as _;
  use super::overlaps;
  use super::try_join_ranges;
  use super::validate_characters;
  use crate::syntax::SyntaxKind;
  use crate::test_support::parse_syntax;

  #[test]
  fn character_policies_accept_their_whitespace_and_report_exact_invalid_bytes() -> Result<(), TestFailure> {
    for (policy, valid) in [
      (CharacterPolicy::CommentOrLiteral, "text\t\u{00e9}"),
      (CharacterPolicy::BasicString, "text\t\u{00e9}"),
      (CharacterPolicy::MultilineBasicString, "text\t\r\n\u{00e9}"),
      (CharacterPolicy::MultilineLiteralString, "text\t\r\n\u{00e9}"),
    ] {
      ensure(
        validate_characters(valid, policy).is_ok(),
        "each TOML character policy must accept its complete supported whitespace and text set",
      )?;
    }

    for (policy, source, expected) in [
      (CharacterPolicy::CommentOrLiteral, "a\n\u{0000}", vec![1, 2]),
      (CharacterPolicy::BasicString, "a\n\u{007f}", vec![1, 2]),
      (CharacterPolicy::MultilineBasicString, "a\n\u{0000}", vec![2]),
      (CharacterPolicy::MultilineLiteralString, "a\n\u{0000}", vec![2]),
    ] {
      ensure(
        validate_characters(source, policy) == Err(expected),
        "each TOML character policy must return every violating byte offset in source order",
      )?;
    }
    Ok(())
  }

  #[test]
  fn quote_stripping_requires_one_matching_outer_pair() -> Result<(), TestFailure> {
    ensure(
      ["\"double\"".strip_quotes(), "'single'".strip_quotes()] == ["double", "single"],
      "matching basic and literal quote pairs must expose their exact interior",
    )?;
    ensure(
      [
        "\"mismatch'".strip_quotes(),
        "\"unterminated".strip_quotes(),
        "plain".strip_quotes(),
      ] == ["\"mismatch'", "\"unterminated", "plain"],
      "mismatched, incomplete, and absent quote pairs must preserve the original slice",
    )
  }

  #[test]
  fn syntax_navigation_distinguishes_deep_containment_boundaries_and_kind_lookup() -> Result<(), TestFailure> {
    let syntax = parse_syntax("alpha = [1]\n", "the syntax-navigation fixture must parse")?;
    let array = ensure_some(syntax.find(SyntaxKind::ARRAY), "kind lookup must find the array element")?;
    let array_end = array.text_range().end();
    ensure(
      (array.kind(), array.to_string()) == (SyntaxKind::ARRAY, String::from("[1]")),
      "kind lookup must return the first complete matching syntax element",
    )?;

    let value_offset = TextSize::new(9);
    let deepest = ensure_some(
      syntax.find_node_deep(value_offset, false),
      "deep lookup must find the innermost node containing the scalar offset",
    )?;
    ensure(
      [
        deepest.text_range().contains(value_offset),
        deepest.find_node(value_offset, false).is_none(),
      ] == [true, true],
      "deep lookup must stop only when no descendant node contains the offset",
    )?;
    ensure(
      [
        syntax.find_node(array_end, false).is_none(),
        syntax.find_node(array_end, true).is_some(),
        syntax.find_node(syntax.text_range().end(), true).is_none(),
        syntax.find_node(TextSize::new(99), true).is_none(),
      ] == [true, true, true, true],
      "node lookup must distinguish descendant endpoints, root-only trailing trivia, and out-of-document offsets",
    )
  }

  #[test]
  fn range_utilities_join_empty_and_covering_inputs_and_treat_only_strict_separation_as_disjoint() -> Result<(), TestFailure> {
    let first = TextRange::new(TextSize::new(1), TextSize::new(3));
    let touching = TextRange::new(TextSize::new(3), TextSize::new(5));
    let contained = TextRange::new(TextSize::new(2), TextSize::new(3));
    let separated = TextRange::new(TextSize::new(4), TextSize::new(6));

    ensure(
      try_join_ranges(Vec::<TextRange>::new()).is_none(),
      "joining no source ranges must remain absent",
    )?;
    ensure(
      try_join_ranges([touching, first]) == Some(TextRange::new(TextSize::new(1), TextSize::new(5))),
      "joining source ranges must cover the minimum start and maximum end independent of input order",
    )?;
    ensure(
      [
        overlaps(first, touching),
        overlaps(first, contained),
        overlaps(first, separated),
      ] == [true, true, false],
      "range overlap must include endpoint contact and containment while rejecting strict separation",
    )
  }
}
