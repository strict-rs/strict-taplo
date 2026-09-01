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
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use taplo::dom::Keys;
  use taplo::dom::Node;
  use taplo::parser;
  use taplo::parser::Parse;
  use taplo::rowan::TextSize;
  use taplo::rowan::TokenAtOffset;
  use taplo::syntax::SyntaxElement;
  use taplo::syntax::SyntaxKind;
  use taplo::syntax::kind::BOOL;
  use taplo::syntax::kind::IDENT;
  use taplo::syntax::kind::INTEGER;
  use taplo::syntax::kind::PERIOD;

  use super::Query;
  use super::lookup_keys;

  /// Parse one source fixture into its tolerant DOM.
  fn dom(source: &str) -> Result<Node, TestFailure> {
    ensure_ok(parser::parse(source), "the query fixture tree must build").map(Parse::into_dom)
  }

  /// Resolve one cursor offset relative to a unique source fragment.
  #[allow(
    clippy::single_call_fn,
    reason = "the named resolver keeps cursor fixtures anchored to readable source fragments instead of hardcoded byte offsets, and \
              confines the checked conversions into Rowan coordinates to one place"
  )]
  fn source_offset(source: &str, fragment: &str, relative: usize) -> Result<TextSize, TestFailure> {
    let fragment_start = ensure_some(source.find(fragment), "the query fixture fragment must exist")?;
    let offset = ensure_some(
      fragment_start.checked_add(relative),
      "the query fixture offset must remain representable",
    )?;
    let raw = ensure_ok(u32::try_from(offset), "the query fixture offset must fit Rowan coordinates")?;
    Ok(TextSize::from(raw))
  }

  /// Query one parsed fixture at an offset relative to a unique source fragment.
  fn query_at_fragment(root: &Node, source: &str, fragment: &str, relative: usize) -> Result<Query, TestFailure> {
    Ok(Query::at(root, source_offset(source, fragment, relative)?))
  }

  /// Parse one source and query it at an offset relative to a unique fragment.
  fn query_fixture(source: &str, fragment: &str, relative: usize) -> Result<(Node, Query), TestFailure> {
    let root = dom(source)?;
    let query = query_at_fragment(&root, source, fragment, relative)?;
    Ok((root, query))
  }

  #[test]
  fn cursor_selection_prefers_before_then_falls_back_after() -> Result<(), TestFailure> {
    let before_dom = dom("key = 1\n")?;
    let before_query = Query::at(&before_dom, TextSize::from(3));
    let before = ensure_some(
      before_query.first_matching(|position| position.syntax.kind() == IDENT),
      "an identifier immediately before the cursor must match",
    )?;
    ensure_eq(&before.syntax.text(), &"key", "the before candidate must take precedence")?;

    let after_dom = dom(" key = 1\n")?;
    let after_query = Query::at(&after_dom, TextSize::from(1));
    ensure(
      after_query
        .before
        .as_ref()
        .is_some_and(|position| position.syntax.kind() != IDENT),
      "the fallback fixture must have a nonmatching before token",
    )?;
    let after = ensure_some(
      after_query.first_matching(|position| position.syntax.kind() == IDENT),
      "a matching after token must be selected when before does not match",
    )?;
    ensure_eq(&after.syntax.text(), &"key", "the after candidate must retain its identifier")?;
    ensure(
      after_query.first_matching(|position| position.syntax.kind() == BOOL).is_none(),
      "cursor-adjacent nonboolean syntax must produce no boolean candidate",
    )
  }

  #[test]
  fn entry_lookup_and_header_identifier_index_are_total() -> Result<(), TestFailure> {
    let entry_dom = dom("alpha = 1\n")?;
    let entry_query = Query::at(&entry_dom, TextSize::from(8));
    let key = ensure_some(entry_query.entry_key(), "an entry query must find its key")?;
    let value = ensure_some(entry_query.entry_value(), "an entry query must find its value")?;
    ensure(
      key.kind() == SyntaxKind::KEY,
      "the entry key helper must return the key node rather than an identifier token",
    )?;
    ensure_eq(
      &entry_query.entry_keys().to_string().as_str(),
      &"alpha",
      "entry keys must normalize layout trivia away",
    )?;
    ensure_eq(
      &value.to_string().trim(),
      &"1",
      "entry value child text must retain its primitive value",
    )?;

    let header_dom = dom("[alpha.beta]\n")?;
    let header_query = Query::at(&header_dom, TextSize::from(8));
    let beta = ensure_some(
      header_query.first_matching(|position| position.syntax.text() == "beta"),
      "the dotted header fixture must expose its second identifier",
    )?;
    ensure(
      Query::header_identifier_index(&beta.syntax) == Some(1),
      "the second dotted identifier must have index one",
    )?;

    let header_syntax = ensure_some(
      header_dom.syntax().and_then(|syntax| syntax.as_node()),
      "the parsed header must retain root syntax",
    )?;
    let period = ensure_some(
      header_syntax
        .descendants_with_tokens()
        .filter_map(SyntaxElement::into_token)
        .find(|token| token.kind() == PERIOD),
      "the dotted header must contain a period token",
    )?;
    ensure(
      Query::header_identifier_index(&period).is_none(),
      "a nonidentifier header token must not receive a segment index",
    )?;

    let entry_syntax = ensure_some(
      entry_dom.syntax().and_then(|syntax| syntax.as_node()),
      "the parsed entry must retain root syntax",
    )?;
    let entry_identifier = match ensure_ok(
      entry_syntax.token_at_offset(TextSize::from(1)),
      "the entry identifier offset must be valid",
    )? {
      TokenAtOffset::Single(token) | TokenAtOffset::Between(_, token) => Some(token),
      TokenAtOffset::None => None,
    };
    ensure(
      ensure_some(entry_identifier, "the entry identifier must be addressable")?.kind() == IDENT,
      "the out-of-header fixture must use an identifier token",
    )?;
    ensure(
      Query::header_identifier_index(&ensure_some(
        match ensure_ok(
          entry_syntax.token_at_offset(TextSize::from(1)),
          "the repeated entry identifier offset must be valid",
        )? {
          TokenAtOffset::Single(token) | TokenAtOffset::Between(_, token) => Some(token),
          TokenAtOffset::None => None,
        },
        "the entry identifier must remain addressable",
      )?)
      .is_none(),
      "an identifier outside a table header must not receive a header index",
    )?;

    let outside_entry = Query::at(&header_dom, TextSize::from(2));
    ensure(
      [outside_entry.entry_key().is_none(), outside_entry.entry_value().is_none()] == [true, true],
      "a table header query must not invent entry children",
    )?;
    ensure(
      value.kind() == SyntaxKind::VALUE,
      "the entry value helper must return the value node rather than its primitive token",
    )?;
    ensure(
      entry_query
        .first_matching(|position| position.syntax.kind() == INTEGER)
        .is_some(),
      "the value-side cursor must still expose its integer token",
    )
  }

  #[test]
  fn syntaxless_and_out_of_range_queries_return_empty_fallbacks() -> Result<(), TestFailure> {
    let syntaxless: Node = ensure_ok(
      serde_json::from_value(serde_json::json!({ "value": 1 })),
      "the syntaxless DOM fixture must deserialize",
    )?;
    let syntaxless_query = Query::at(&syntaxless, TextSize::from(0));
    ensure(
      [syntaxless_query.before.is_none(), syntaxless_query.after.is_none()] == [true, true],
      "syntaxless DOM input must return an empty query",
    )?;
    let (keys, parent) = syntaxless_query.parent_table_or_array_table(&syntaxless);
    ensure(keys.is_empty(), "syntaxless parent lookup must use empty keys")?;
    ensure_eq(
      &ensure_ok(serde_json::to_value(parent), "the fallback parent DOM must serialize")?,
      &serde_json::json!({ "value": 1 }),
      "syntaxless parent lookup must return the supplied root",
    )?;

    let parsed = dom("value = 1\n")?;
    let outside = Query::at(&parsed, TextSize::new(u32::MAX));
    ensure(
      [outside.before.is_none(), outside.after.is_none()] == [true, true],
      "an unusable offset must return an empty query instead of panicking",
    )
  }

  #[test]
  fn cursor_predicates_classify_headers_entries_and_quoting_polarities() -> Result<(), TestFailure> {
    let header_source = "[alpha.beta]\n[[records]]\n";
    let (header_dom, table_header) = query_fixture(header_source, "alpha.beta", 3)?;
    ensure(
      (
        table_header.in_table_header(),
        table_header.in_table_array_header(),
        table_header.header_keys().to_string(),
        table_header.entry_key().is_none(),
      ) == (true, false, "alpha.beta".into(), true),
      "a standard table-header cursor must expose only its complete normalized header path",
    )?;

    let array_header = query_at_fragment(&header_dom, header_source, "records", 3)?;
    ensure(
      (
        array_header.in_table_header(),
        array_header.in_table_array_header(),
        array_header.header_keys().to_string(),
      ) == (false, true, "records".into()),
      "an array-table cursor must expose its array-header category and normalized path",
    )?;

    let literal_source = "alpha = 'literal'\n";
    let literal_dom = dom(literal_source)?;
    let key_query = query_at_fragment(&literal_dom, literal_source, "alpha", 3)?;
    ensure(
      (
        key_query.in_entry_keys(),
        key_query.entry_has_eq(),
        key_query.in_entry_value(),
        key_query.is_single_quote_value(),
      ) == (true, true, false, true),
      "an assigned literal-string key must retain key position, equals-sign, and quoting facts",
    )?;
    let value_query = query_at_fragment(&literal_dom, literal_source, "'literal'", 5)?;
    ensure(
      (
        value_query.in_entry_keys(),
        value_query.in_entry_value(),
        value_query.is_single_quote_value(),
        value_query.dom_node().is_some(),
      ) == (false, true, true, true),
      "a literal-string value cursor must expose value position, literal quoting, and its semantic node",
    )?;

    let basic_source = "alpha = \"basic\"\n";
    let basic_dom = dom(basic_source)?;
    let basic_query = query_at_fragment(&basic_dom, basic_source, "\"basic\"", 4)?;
    ensure(
      basic_query.in_entry_value() && !basic_query.is_single_quote_value(),
      "a basic string must remain a value without being classified as literal-string quoting",
    )?;

    let incomplete_source = "alpha";
    let incomplete_dom = dom(incomplete_source)?;
    let incomplete_query = query_at_fragment(&incomplete_dom, incomplete_source, "alpha", 3)?;
    ensure(
      incomplete_query.in_entry_keys() && !incomplete_query.entry_has_eq(),
      "an incomplete entry key must not fabricate an equals-sign contract",
    )
  }

  #[test]
  fn line_and_container_queries_preserve_inside_outside_boundaries() -> Result<(), TestFailure> {
    let line_source = "first = 1\n  \nsecond = 2\n";
    let line_dom = dom(line_source)?;
    let blank_line = query_at_fragment(&line_dom, line_source, "  \n", 1)?;
    let content_line = query_at_fragment(&line_dom, line_source, "first", 2)?;
    ensure(
      (blank_line.empty_line(), content_line.empty_line()) == (true, false),
      "trivia-only lines must be distinguished from lines containing entry syntax",
    )?;

    let container_source = "table = { nested = 1 }\narray = [ 1 ]\n";
    let container_dom = dom(container_source)?;
    let inline_inside = query_at_fragment(&container_dom, container_source, "{ ", 1)?;
    let inline_after = query_at_fragment(&container_dom, container_source, "}\n", 1)?;
    let array_inside = query_at_fragment(&container_dom, container_source, "[ ", 1)?;
    let array_after = query_at_fragment(&container_dom, container_source, "]\n", 1)?;
    ensure(
      (
        inline_inside.is_inline(),
        inline_inside.in_inline_table(),
        inline_inside.in_array(),
        inline_after.in_inline_table(),
        array_inside.is_inline(),
        array_inside.in_inline_table(),
        array_inside.in_array(),
        array_after.in_array(),
      ) == (true, true, false, false, true, false, true, false),
      "inline tables and arrays must report their own interior while rejecting the opposite container and post-closing cursor",
    )
  }

  #[test]
  fn parent_and_lookup_paths_preserve_table_and_array_semantics() -> Result<(), TestFailure> {
    let table_source = "root = 0\n[parent]\nvalue = 1\n";
    let (table_dom, parent_query) = query_fixture(table_source, "value", 3)?;
    let (parent_keys, parent_node) = parent_query.parent_table_or_array_table(&table_dom);
    ensure_eq(
      &parent_keys.to_string().as_str(),
      &"parent",
      "a table entry must resolve the preceding semantic table path",
    )?;
    ensure_eq(
      &ensure_ok(serde_json::to_value(parent_node), "the resolved parent table must serialize")?,
      &serde_json::json!({
        "value": 1
      }),
      "a table entry must resolve the table node rather than the document root",
    )?;

    let root_query = query_at_fragment(&table_dom, table_source, "root", 2)?;
    ensure(
      root_query.parent_table_or_array_table(&table_dom).0.is_empty(),
      "an entry before the first header must remain rooted at the document path",
    )?;

    let populated_array = dom("items = [{ name = \"first\" }, { name = \"second\" }]\n")?;
    let items = ensure_ok("items".parse::<Keys>(), "the array lookup path must parse")?;
    ensure_eq(
      &lookup_keys(populated_array, &items).to_string().as_str(),
      &"items.1",
      "schema lookup must target the last concrete item of a populated array",
    )?;
    let empty_array = dom("items = []\n")?;
    ensure_eq(
      &lookup_keys(empty_array, &items).to_string().as_str(),
      &"items.0",
      "schema lookup must use the first prospective item of an empty array without underflow",
    )?;
    let scalar = dom("name = \"value\"\n")?;
    let name = ensure_ok("name".parse::<Keys>(), "the scalar lookup path must parse")?;
    ensure_eq(
      &lookup_keys(scalar, &name).to_string().as_str(),
      &"name",
      "schema lookup must leave non-array paths unchanged",
    )
  }
}
