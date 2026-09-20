//! Cursor queries of a TOML document.

use taplo::dom::KeyOrIndex;
use taplo::dom::Keys;
use taplo::dom::Node;
use taplo::dom::node::Key;
use taplo::rowan::Direction;
use taplo::rowan::TextRange;
use taplo::rowan::TextSize;
use taplo::rowan::TokenAtOffset;
use taplo::syntax::SyntaxElement;
use taplo::syntax::SyntaxKind;
use taplo::syntax::SyntaxNode;
use taplo::syntax::SyntaxToken;
use taplo::syntax::kind::ARRAY;
use taplo::syntax::kind::BRACE_END;
use taplo::syntax::kind::BRACKET_END;
use taplo::syntax::kind::BRACKET_START;
use taplo::syntax::kind::COMMENT;
use taplo::syntax::kind::ENTRY;
use taplo::syntax::kind::EQ;
use taplo::syntax::kind::IDENT;
use taplo::syntax::kind::INLINE_TABLE;
use taplo::syntax::kind::KEY;
use taplo::syntax::kind::MULTI_LINE_STRING_LITERAL;
use taplo::syntax::kind::NEWLINE;
use taplo::syntax::kind::ROOT;
use taplo::syntax::kind::STRING_LITERAL;
use taplo::syntax::kind::TABLE_ARRAY_HEADER;
use taplo::syntax::kind::TABLE_HEADER;
use taplo::syntax::kind::VALUE;
use taplo::syntax::kind::WHITESPACE;
use taplo::util::try_join_ranges;

/// Cursor-relative syntactic and semantic facts for one TOML document.
#[derive(Debug, Default)]
pub struct Query {
  /// The offset the query was made for.
  pub offset: TextSize,
  /// Before the cursor.
  pub before: Option<PositionInfo>,
  /// After the cursor.
  pub after:  Option<PositionInfo>,
}

impl Query {
  /// Query a DOM root with the given cursor offset.
  ///
  /// Syntaxless/non-root DOM values and offsets outside the syntax tree produce an empty query.
  #[must_use]
  pub fn at(root: &Node, offset: TextSize) -> Self {
    let Some(syntax) = root.syntax().cloned().and_then(SyntaxElement::into_node) else {
      return Self {
        offset,
        ..Self::default()
      };
    };
    if syntax.kind() != ROOT || offset > syntax.text_range().end() {
      return Self {
        offset,
        ..Self::default()
      };
    }

    Self {
      offset,
      before: offset
        .checked_sub(TextSize::from(1))
        .and_then(|previous_offset| Self::position_info_at(root, &syntax, previous_offset)),
      after: if offset >= syntax.text_range().end() {
        None
      } else {
        Self::position_info_at(root, &syntax, offset)
      },
    }
  }

  /// Resolve the syntax token and narrowest semantic node at one valid source offset.
  fn position_info_at(root: &Node, syntax: &SyntaxNode, offset: TextSize) -> Option<PositionInfo> {
    let token = match syntax.token_at_offset(offset).ok()? {
      TokenAtOffset::None => return None,
      TokenAtOffset::Single(single) => single,
      TokenAtOffset::Between(_, right) => right,
    };

    Some(PositionInfo {
      syntax:   token,
      dom_node: root
        .flat_iter()
        .filter(|entry| full_range(&entry.0, &entry.1).is_some_and(|range| range.contains(offset)))
        .max_by_key(|entry| entry.0.len()),
    })
  }

  /// Select the first cursor-adjacent position matching `predicate`, preferring `before`.
  #[must_use]
  pub fn first_matching(&self, predicate: impl Fn(&PositionInfo) -> bool) -> Option<&PositionInfo> {
    self
      .before
      .as_ref()
      .filter(|position| predicate(position))
      .or_else(|| self.after.as_ref().filter(|position| predicate(position)))
  }

  #[must_use]
  /// Return whether the cursor lies inside a standard table header.
  pub fn in_table_header(&self) -> bool {
    self.in_header(TABLE_HEADER, 0)
  }

  #[must_use]
  /// Return whether the cursor lies inside an array-of-tables header.
  pub fn in_table_array_header(&self) -> bool {
    self.in_header(TABLE_ARRAY_HEADER, 1)
  }

  /// Return whether the cursor lies between a matched header's brackets.
  fn in_header(&self, header_kind: SyntaxKind, opening_bracket_index: usize) -> bool {
    let Some(before) = self.before.as_ref() else {
      return false;
    };
    let Some(after) = self.after.as_ref() else {
      return false;
    };
    let Some(header_syntax) = before.syntax.parent_ancestors().find(|syntax| syntax.kind() == header_kind) else {
      return false;
    };
    if !after.syntax.parent_ancestors().any(|ancestor| ancestor == header_syntax) {
      return false;
    }
    let Some(bracket_start) = header_syntax
      .children_with_tokens()
      .filter(|element| element.kind() == BRACKET_START)
      .filter_map(SyntaxElement::into_token)
      .nth(opening_bracket_index)
    else {
      return false;
    };
    let Some(bracket_end) = header_syntax
      .children_with_tokens()
      .find(|element| element.kind() == BRACKET_END)
      .and_then(SyntaxElement::into_token)
    else {
      return false;
    };
    (before.syntax == bracket_start || before.syntax.text_range().start() >= bracket_start.text_range().end())
      && (after.syntax == bracket_end || after.syntax.text_range().end() <= bracket_end.text_range().start())
  }

  #[must_use]
  /// Return the key syntax of the cursor's nearest table header.
  pub fn header_key(&self) -> Option<SyntaxNode> {
    let before = self.before.as_ref()?;
    let header_syntax = before
      .syntax
      .parent_ancestors()
      .find(|ancestor| matches!(ancestor.kind(), TABLE_ARRAY_HEADER | TABLE_HEADER))?;
    header_syntax.descendants().find(|node| node.kind() == KEY)
  }

  #[must_use]
  /// Return the key syntax of the cursor's nearest entry.
  pub fn entry_key(&self) -> Option<SyntaxNode> {
    self.entry_child(KEY)
  }

  #[must_use]
  /// Return the value syntax of the cursor's nearest entry.
  pub fn entry_value(&self) -> Option<SyntaxNode> {
    self.entry_child(VALUE)
  }

  /// Find a child of the cursor's nearest entry.
  fn entry_child(&self, kind: SyntaxKind) -> Option<SyntaxNode> {
    let syntax = &self.before.as_ref().or(self.after.as_ref())?.syntax;
    syntax
      .parent_ancestors()
      .find(|ancestor| ancestor.kind() == ENTRY)
      .and_then(|entry| entry.children().find(|child| child.kind() == kind))
  }

  /// Return an identifier token's zero-based segment index within its table header.
  #[must_use]
  pub fn header_identifier_index(token: &SyntaxToken) -> Option<usize> {
    if token.kind() != IDENT {
      return None;
    }
    let header = token
      .parent_ancestors()
      .find(|node| matches!(node.kind(), TABLE_HEADER | TABLE_ARRAY_HEADER))?;
    let key = header.descendants().find(|node| node.kind() == KEY)?;
    key
      .descendants_with_tokens()
      .filter_map(SyntaxElement::into_token)
      .filter(|candidate| candidate.kind() == IDENT)
      .position(|candidate| candidate == *token)
  }

  #[must_use]
  /// Resolve the table or array-table that semantically contains the cursor.
  pub fn parent_table_or_array_table(&self, root: &Node) -> (Keys, Node) {
    let cursor_syntax = match self.before.as_ref().or(self.after.as_ref()) {
      Some(position) => position.syntax.clone(),
      None => return (Keys::empty(), root.clone()),
    };

    let Some(root_syntax) = root.syntax().and_then(|syntax| syntax.as_node()) else {
      return (Keys::empty(), root.clone());
    };
    let last_header = root_syntax
      .descendants()
      .skip(1)
      .filter(|node| matches!(node.kind(), TABLE_HEADER | TABLE_ARRAY_HEADER))
      .take_while(|node| node.text_range().end() <= cursor_syntax.text_range().end())
      .last();

    let Some(header) = last_header else {
      return (Keys::empty(), root.clone());
    };

    let Some(key_syntax) = header.descendants().find(|node| node.kind() == KEY) else {
      return (Keys::empty(), root.clone());
    };
    let keys = Keys::from_syntax(&key_syntax.into());
    let Some(node) = root.path(&keys) else {
      return (Keys::empty(), root.clone());
    };

    (keys, node)
  }

  #[must_use]
  /// Return whether the cursor is on a line containing only trivia.
  pub fn empty_line(&self) -> bool {
    let before_syntax = match self.before.as_ref() {
      Some(position) => &position.syntax,
      None => return true,
    };

    if self
      .after
      .as_ref()
      .is_some_and(|after| !matches!(after.syntax.kind(), WHITESPACE | NEWLINE) || !reaches_line_boundary(&after.syntax, Direction::Next))
    {
      return false;
    }

    reaches_line_boundary(before_syntax, Direction::Prev)
  }

  #[must_use]
  /// Return whether the cursor lies inside an entry key.
  pub fn in_entry_keys(&self) -> bool {
    self.entry_key().is_some_and(|key| key.text_range().contains(self.offset))
  }

  #[must_use]
  /// Return whether the cursor's entry contains an equals sign before its value.
  pub fn entry_has_eq(&self) -> bool {
    let Some(key_syntax) = self.entry_key() else {
      return false;
    };

    key_syntax
      .siblings_with_tokens(Direction::Next)
      .skip(1)
      .find_map(|sibling| match sibling.kind() {
        EQ => Some(true),
        WHITESPACE => None,
        _ => Some(false),
      })
      .unwrap_or(false)
  }

  #[must_use]
  /// Return whether the cursor lies in or immediately after an entry value.
  pub fn in_entry_value(&self) -> bool {
    let in_value = self
            .entry_value()
            // We are inside the value even if the cursor is right after it.
            .is_some_and(|value_syntax| value_syntax.text_range().contains_inclusive(self.offset));

    if in_value {
      return true;
    }

    let syntax = match self.before.as_ref().or(self.after.as_ref()) {
      Some(position) => &position.syntax,
      None => return false,
    };

    syntax
      .siblings_with_tokens(Direction::Prev)
      .find_map(|sibling| match sibling.kind() {
        EQ => Some(true),
        WHITESPACE | COMMENT | NEWLINE => None,
        _ => Some(false),
      })
      .unwrap_or(false)
  }

  #[must_use]
  /// Return whether the cursor's entry value uses literal-string quoting.
  pub fn is_single_quote_value(&self) -> bool {
    self.entry_value().is_some_and(|value_syntax| {
      value_syntax
        .descendants_with_tokens()
        .any(|token| matches!(token.kind(), STRING_LITERAL | MULTI_LINE_STRING_LITERAL))
    })
  }

  #[must_use]
  /// Return whether the cursor is nested inside an inline table or array.
  pub fn is_inline(&self) -> bool {
    let syntax = match self.before.as_ref().or(self.after.as_ref()) {
      Some(position) => &position.syntax,
      None => return false,
    };

    syntax
      .parent_ancestors()
      .any(|ancestor| matches!(ancestor.kind(), INLINE_TABLE | ARRAY))
  }

  #[must_use]
  /// Return whether the cursor lies before the end of an inline table.
  pub fn in_inline_table(&self) -> bool {
    self.before_container_end(INLINE_TABLE, BRACE_END)
  }

  #[must_use]
  /// Return whether the cursor lies before the end of an array.
  pub fn in_array(&self) -> bool {
    self.before_container_end(ARRAY, BRACKET_END)
  }

  /// Return whether the cursor is within a container and before its closing token.
  fn before_container_end(&self, container_kind: SyntaxKind, closing_kind: SyntaxKind) -> bool {
    let Some(syntax) = self.before.as_ref().or(self.after.as_ref()).map(|position| &position.syntax) else {
      return false;
    };
    let Some(parent) = syntax.parent() else {
      return false;
    };
    if parent.kind() != container_kind {
      return false;
    }
    parent
      .children_with_tokens()
      .find(|element| element.kind() == closing_kind)
      .and_then(SyntaxElement::into_token)
      .is_none_or(|closing| self.offset <= closing.text_range().start())
  }

  /// Decode the cursor's nearest entry key path.
  #[must_use]
  pub fn entry_keys(&self) -> Keys {
    self
      .entry_key()
      .map_or_else(Keys::empty, |keys| Keys::from_syntax(&keys.into()))
  }

  /// Decode the cursor's nearest header key path.
  #[must_use]
  pub fn header_keys(&self) -> Keys {
    self
      .header_key()
      .map_or_else(Keys::empty, |keys| Keys::from_syntax(&keys.into()))
  }

  #[must_use]
  /// Return the narrowest semantic DOM node covering the cursor.
  pub fn dom_node(&self) -> Option<&(Keys, Node)> {
    self
      .before
      .as_ref()
      .and_then(|position| position.dom_node.as_ref())
      .or_else(|| self.after.as_ref().and_then(|position| position.dom_node.as_ref()))
  }
}

/// Transform the lookup keys to account for arrays of tables and arrays.
///
/// It appends an index after each array so that we get the item type
/// during lookups.
#[must_use]
pub fn lookup_keys(root: Node, keys: &Keys) -> Keys {
  let mut node = Some(root);
  let mut new_keys = Keys::empty();

  for key in keys.iter().cloned() {
    node = node.and_then(|current| current.get(&key));
    new_keys = new_keys.join(key);
    if let Some(arr) = node.as_ref().and_then(Node::as_array) {
      new_keys = new_keys.join(arr.items().len().saturating_sub(1));
    }
  }

  new_keys
}

/// Syntax and semantic DOM state at one side of a cursor.
#[derive(Debug, Clone)]
pub struct PositionInfo {
  /// The narrowest syntax element that contains the position.
  pub syntax:   SyntaxToken,
  /// The narrowest node that covers the position.
  pub dom_node: Option<(Keys, Node)>,
}

/// Return whether traversing trivia in one direction reaches a line boundary before content.
fn reaches_line_boundary(syntax: &SyntaxToken, direction: Direction) -> bool {
  syntax
    .siblings_with_tokens(direction)
    .find_map(|sibling| match sibling.kind() {
      NEWLINE => Some(true),
      WHITESPACE | COMMENT => None,
      _ => Some(false),
    })
    .unwrap_or(true)
}

/// Join a semantic node's key and value ranges into its complete source range.
#[allow(
  clippy::single_call_fn,
  reason = "the name states that a semantic node's cursor extent spans its owning key as well as its value, a rule the narrowest-node \
            filter would otherwise bury in a chained range join"
)]
fn full_range(keys: &Keys, node: &Node) -> Option<TextRange> {
  let Some(last_key) = keys.iter().filter_map(KeyOrIndex::as_key).next_back().map(Key::text_ranges) else {
    return try_join_ranges(node.text_ranges(true));
  };

  try_join_ranges(last_key.chain(node.text_ranges(true)))
}

#[cfg(test)]
mod tests {
  use std::fmt::Debug;
  use std::num::TryFromIntError;

  use strict_test_support::OptionFailure;
  use strict_test_support::ResultFailure;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use strict_test_support::ensure_that;
  use taplo::dom::Node;
  use taplo::parser;
  use taplo::parser::Parse;
  use taplo::parser::ParseFailure;
  use taplo::rowan::TextSize;
  use taplo::rowan::TokenAtOffset;
  use taplo::syntax::SyntaxElement;
  use taplo::syntax::SyntaxKind;
  use taplo::syntax::kind::BOOL;
  use taplo::syntax::kind::IDENT;
  use taplo::syntax::kind::INTEGER;
  use taplo::syntax::kind::PERIOD;
  use thiserror::Error;

  use super::Query;
  use super::lookup_keys;

  /// Native failures while locating a cursor in a parsed fixture.
  #[derive(Debug, Error)]
  enum QueryFixtureFailure {
    /// Lossless syntax construction failed.
    #[error(transparent)]
    Parse(#[from] ResultFailure<ParseFailure>),
    /// The fragment or checked offset was absent.
    #[error(transparent)]
    Offset(#[from] OptionFailure<usize>),
    /// The located byte offset exceeded Rowan's coordinate type.
    #[error(transparent)]
    Coordinate(#[from] ResultFailure<TryFromIntError>),
  }

  /// Parse one source fixture into its tolerant DOM.
  fn dom(source: &str) -> Result<Node, ResultFailure<ParseFailure>> {
    ensure_ok(parser::parse(source), "the query fixture tree must build").map(Parse::into_dom)
  }

  /// Query one source at a checked offset relative to a unique fragment.
  fn query_fixture(source: &str, fragment: &str, relative: usize) -> Result<(Node, Query), QueryFixtureFailure> {
    let fragment_start = ensure_some(source.find(fragment), "the query fixture fragment must exist")?;
    let offset = ensure_some(
      fragment_start.checked_add(relative),
      "the query fixture offset must remain representable",
    )?;
    let raw = ensure_ok(u32::try_from(offset), "the query fixture offset must fit Rowan coordinates")?;
    let root = dom(source)?;
    let query = Query::at(&root, TextSize::from(raw));
    Ok((root, query))
  }

  #[test]
  fn cursor_selection_prefers_before_then_falls_back_after() -> Result<(), impl Debug> {
    let before = query_fixture("key = 1\n", "key", 3);
    let after = query_fixture(" key = 1\n", "key", 0);
    ensure_that(
      (before, after),
      "cursor selection must prefer a matching before token and otherwise try after",
      |observed| {
        observed.0.as_ref().is_ok_and(|fixture| {
          fixture
            .1
            .first_matching(|position| position.syntax.kind() == IDENT)
            .is_some_and(|position| position.syntax.text() == "key")
        }) && observed.1.as_ref().is_ok_and(|fixture| {
          fixture
            .1
            .before
            .as_ref()
            .is_some_and(|position| position.syntax.kind() != IDENT)
            && fixture
              .1
              .first_matching(|position| position.syntax.kind() == IDENT)
              .is_some_and(|position| position.syntax.text() == "key")
            && fixture.1.first_matching(|position| position.syntax.kind() == BOOL).is_none()
        })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn entry_lookup_and_header_identifier_index_are_total() -> Result<(), impl Debug> {
    let entry = query_fixture("alpha = 1\n", "1", 0);
    let header = query_fixture("[alpha.beta]\n", "beta", 1);
    ensure_that(
      (entry, header),
      "entry children and header segment indices must preserve syntax kinds and reject unrelated tokens",
      |observed| {
        let Ok(ref entry_fixture) = observed.0 else {
          return false;
        };
        let Ok(ref header_fixture) = observed.1 else {
          return false;
        };
        let Some(entry_syntax) = entry_fixture.0.syntax().and_then(|syntax| syntax.as_node()) else {
          return false;
        };
        let Ok(entry_tokens) = entry_syntax.token_at_offset(TextSize::from(1)) else {
          return false;
        };
        entry_fixture.1.entry_key().is_some_and(|key| key.kind() == SyntaxKind::KEY)
          && entry_fixture.1.entry_keys().to_string() == "alpha"
          && entry_fixture
            .1
            .entry_value()
            .is_some_and(|value| value.kind() == SyntaxKind::VALUE && value.to_string().trim() == "1")
          && entry_fixture
            .1
            .first_matching(|position| position.syntax.kind() == INTEGER)
            .is_some()
          && matches!(entry_tokens, TokenAtOffset::Single(ref token) | TokenAtOffset::Between(_, ref token)
            if token.kind() == IDENT && Query::header_identifier_index(token).is_none())
          && header_fixture
            .1
            .first_matching(|position| position.syntax.text() == "beta")
            .is_some_and(|position| Query::header_identifier_index(&position.syntax) == Some(1))
          && header_fixture
            .0
            .syntax()
            .and_then(|syntax| syntax.as_node())
            .and_then(|syntax| {
              syntax
                .descendants_with_tokens()
                .filter_map(SyntaxElement::into_token)
                .find(|token| token.kind() == PERIOD)
            })
            .is_some_and(|period| Query::header_identifier_index(&period).is_none())
          && header_fixture.1.entry_key().is_none()
          && header_fixture.1.entry_value().is_none()
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn syntaxless_and_out_of_range_queries_return_empty_fallbacks() -> Result<(), impl Debug> {
    let syntaxless = serde_json::from_value::<Node>(serde_json::json!({ "value": 1 })).map(|root| {
      let query = Query::at(&root, TextSize::from(0));
      let parent = query.parent_table_or_array_table(&root);
      let serialized = serde_json::to_value(&parent.1);
      (root, query, parent, serialized)
    });
    let outside = dom("value = 1\n").map(|root| {
      let query = Query::at(&root, TextSize::new(u32::MAX));
      (root, query)
    });
    ensure_that(
      (syntaxless, outside),
      "syntaxless and out-of-range cursors must remain empty with the original parent fallback",
      |observed| {
        observed.0.as_ref().is_ok_and(|fixture| {
          fixture.1.before.is_none()
            && fixture.1.after.is_none()
            && fixture.2.0.is_empty()
            && fixture
              .3
              .as_ref()
              .is_ok_and(|value| *value == serde_json::json!({ "value": 1 }))
        }) && observed
          .1
          .as_ref()
          .is_ok_and(|fixture| fixture.1.before.is_none() && fixture.1.after.is_none())
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn cursor_predicates_classify_headers_entries_and_quoting_polarities() -> Result<(), impl Debug> {
    let observed = [
      query_fixture("[alpha.beta]\n[[records]]\n", "alpha.beta", 3),
      query_fixture("[alpha.beta]\n[[records]]\n", "records", 3),
      query_fixture("alpha = 'literal'\n", "alpha", 3),
      query_fixture("alpha = 'literal'\n", "'literal'", 5),
      query_fixture("alpha = \"basic\"\n", "\"basic\"", 4),
      query_fixture("alpha", "alpha", 3),
    ];
    ensure_that(
      observed,
      "cursor predicates must distinguish table kinds, entry positions, assignment and string quoting",
      |fixtures| {
        let [ref table, ref array, ref key, ref literal, ref basic, ref incomplete] = *fixtures;
        table.as_ref().is_ok_and(|fixture| {
          fixture.1.in_table_header()
            && !fixture.1.in_table_array_header()
            && fixture.1.header_keys().to_string() == "alpha.beta"
            && fixture.1.entry_key().is_none()
        }) && array.as_ref().is_ok_and(|fixture| {
          !fixture.1.in_table_header() && fixture.1.in_table_array_header() && fixture.1.header_keys().to_string() == "records"
        }) && key.as_ref().is_ok_and(|fixture| {
          fixture.1.in_entry_keys() && fixture.1.entry_has_eq() && !fixture.1.in_entry_value() && fixture.1.is_single_quote_value()
        }) && literal.as_ref().is_ok_and(|fixture| {
          !fixture.1.in_entry_keys() && fixture.1.in_entry_value() && fixture.1.is_single_quote_value() && fixture.1.dom_node().is_some()
        }) && basic
          .as_ref()
          .is_ok_and(|fixture| fixture.1.in_entry_value() && !fixture.1.is_single_quote_value())
          && incomplete
            .as_ref()
            .is_ok_and(|fixture| fixture.1.in_entry_keys() && !fixture.1.entry_has_eq())
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn line_and_container_queries_preserve_inside_outside_boundaries() -> Result<(), impl Debug> {
    let line_source = "first = 1\n  \nsecond = 2\n";
    let container_source = "table = { nested = 1 }\narray = [ 1 ]\n";
    let observed = [
      query_fixture(line_source, "  \n", 1),
      query_fixture(line_source, "first", 2),
      query_fixture(container_source, "{ ", 1),
      query_fixture(container_source, "}\n", 1),
      query_fixture(container_source, "[ ", 1),
      query_fixture(container_source, "]\n", 1),
    ];
    ensure_that(
      observed,
      "line and container queries must distinguish trivia, content, interiors and post-closing positions",
      |fixtures| {
        let [ref blank, ref content, ref inline, ref after_inline, ref array, ref after_array] = *fixtures;
        blank.as_ref().is_ok_and(|fixture| fixture.1.empty_line())
          && content.as_ref().is_ok_and(|fixture| !fixture.1.empty_line())
          && inline
            .as_ref()
            .is_ok_and(|fixture| fixture.1.is_inline() && fixture.1.in_inline_table() && !fixture.1.in_array())
          && after_inline.as_ref().is_ok_and(|fixture| !fixture.1.in_inline_table())
          && array
            .as_ref()
            .is_ok_and(|fixture| fixture.1.is_inline() && !fixture.1.in_inline_table() && fixture.1.in_array())
          && after_array.as_ref().is_ok_and(|fixture| !fixture.1.in_array())
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn parent_and_lookup_paths_preserve_table_and_array_semantics() -> Result<(), impl Debug> {
    let table_source = "root = 0\n[parent]\nvalue = 1\n";
    let parent = query_fixture(table_source, "value", 3).map(|fixture| {
      let parent = fixture.1.parent_table_or_array_table(&fixture.0);
      let serialized = serde_json::to_value(&parent.1);
      (fixture, parent, serialized)
    });
    let root = query_fixture(table_source, "root", 2);
    let arrays = [
      query_fixture("items = [{ name = \"first\" }, { name = \"second\" }]\n", "items", 2),
      query_fixture("items = []\n", "items", 2),
      query_fixture("name = \"value\"\n", "name", 2),
    ];
    ensure_that(
      (parent, root, arrays),
      "parent and schema lookup paths must preserve table ownership and concrete or prospective array indices",
      |observed| {
        let [ref populated, ref empty, ref scalar] = observed.2;
        observed.0.as_ref().is_ok_and(|fixture| {
          fixture.1.0.to_string() == "parent"
            && fixture
              .2
              .as_ref()
              .is_ok_and(|value| *value == serde_json::json!({ "value": 1 }))
        }) && observed
          .1
          .as_ref()
          .is_ok_and(|fixture| fixture.1.parent_table_or_array_table(&fixture.0).0.is_empty())
          && populated
            .as_ref()
            .is_ok_and(|fixture| lookup_keys(fixture.0.clone(), &fixture.1.entry_keys()).to_string() == "items.1")
          && empty
            .as_ref()
            .is_ok_and(|fixture| lookup_keys(fixture.0.clone(), &fixture.1.entry_keys()).to_string() == "items.0")
          && scalar
            .as_ref()
            .is_ok_and(|fixture| lookup_keys(fixture.0.clone(), &fixture.1.entry_keys()).to_string() == "name")
      },
    )
    .map(drop)
    .map_err(Box::new)
  }
}
