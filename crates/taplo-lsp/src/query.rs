//! Cursor queries of a TOML document.

use taplo::{
    dom::{
        node::{DomNode, Key},
        FromSyntax, KeyOrIndex, Keys, Node,
    },
    rowan::{Direction, TextRange, TextSize},
    syntax::{SyntaxElement, SyntaxKind::*, SyntaxNode, SyntaxToken},
    util::join_ranges,
};

#[derive(Debug, Default)]
pub struct Query {
    /// The offset the query was made for.
    pub offset: TextSize,
    /// Before the cursor.
    pub before: Option<PositionInfo>,
    /// After the cursor.
    pub after: Option<PositionInfo>,
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

        Query {
            offset,
            before: offset
                .checked_sub(TextSize::from(1))
                .and_then(|offset| Self::position_info_at(root, &syntax, offset)),
            after: if offset >= syntax.text_range().end() {
                None
            } else {
                Self::position_info_at(root, &syntax, offset)
            },
        }
    }

    fn position_info_at(
        root: &Node,
        syntax: &SyntaxNode,
        offset: TextSize,
    ) -> Option<PositionInfo> {
        let syntax = match syntax.token_at_offset(offset) {
            taplo::rowan::TokenAtOffset::None => return None,
            taplo::rowan::TokenAtOffset::Single(s) => s,
            taplo::rowan::TokenAtOffset::Between(_, right) => right,
        };

        Some(PositionInfo {
            syntax,
            dom_node: root
                .flat_iter()
                .filter(|(k, n)| full_range(k, n).contains(offset))
                .max_by_key(|(k, _)| k.len()),
        })
    }
}

impl Query {
    /// Select the first cursor-adjacent position matching `predicate`, preferring `before`.
    #[must_use]
    pub fn first_matching(
        &self,
        predicate: impl Fn(&PositionInfo) -> bool,
    ) -> Option<&PositionInfo> {
        self.before
            .as_ref()
            .filter(|position| predicate(position))
            .or_else(|| {
                self.after
                    .as_ref()
                    .filter(|position| predicate(position))
            })
    }

    #[must_use]
    pub fn in_table_header(&self) -> bool {
        match (&self.before, &self.after) {
            (Some(before), Some(after)) => {
                let Some(header_syntax) = before
                    .syntax
                    .parent_ancestors()
                    .find(|s| s.kind() == TABLE_HEADER)
                else {
                    return false;
                };

                if !after.syntax.parent_ancestors().any(|a| a == header_syntax) {
                    return false;
                }

                let Some(bracket_start) = header_syntax.children_with_tokens().find_map(|t| {
                    if t.kind() == BRACKET_START {
                        t.into_token()
                    } else {
                        None
                    }
                }) else {
                    return false;
                };

                let Some(bracket_end) = header_syntax.children_with_tokens().find_map(|t| {
                    if t.kind() == BRACKET_END {
                        t.into_token()
                    } else {
                        None
                    }
                }) else {
                    return false;
                };

                (before.syntax == bracket_start
                    || before.syntax.text_range().start() >= bracket_start.text_range().end())
                    && (after.syntax == bracket_end
                        || after.syntax.text_range().end() <= bracket_end.text_range().start())
            }
            _ => false,
        }
    }

    #[must_use]
    pub fn in_table_array_header(&self) -> bool {
        match (&self.before, &self.after) {
            (Some(before), Some(after)) => {
                let Some(header_syntax) = before
                    .syntax
                    .parent_ancestors()
                    .find(|s| s.kind() == TABLE_ARRAY_HEADER)
                else {
                    return false;
                };

                if !after.syntax.parent_ancestors().any(|a| a == header_syntax) {
                    return false;
                }

                let Some(bracket_start) = header_syntax
                    .children_with_tokens()
                    .filter_map(|t| {
                        if t.kind() == BRACKET_START {
                            t.into_token()
                        } else {
                            None
                        }
                    })
                    .nth(1)
                else {
                    return false;
                };

                let Some(bracket_end) = header_syntax.children_with_tokens().find_map(|t| {
                    if t.kind() == BRACKET_END {
                        t.into_token()
                    } else {
                        None
                    }
                }) else {
                    return false;
                };

                (before.syntax == bracket_start
                    || before.syntax.text_range().start() >= bracket_start.text_range().end())
                    && (after.syntax == bracket_end
                        || after.syntax.text_range().end() <= bracket_end.text_range().start())
            }
            _ => false,
        }
    }

    #[must_use]
    pub fn header_key(&self) -> Option<SyntaxNode> {
        match (&self.before, &self.after) {
            (Some(before), _) => {
                let header_syntax = before
                    .syntax
                    .parent_ancestors()
                    .find(|s| matches!(s.kind(), TABLE_ARRAY_HEADER | TABLE_HEADER))?;

                header_syntax.descendants().find(|n| n.kind() == KEY)
            }
            _ => None,
        }
    }

    #[must_use]
    pub fn entry_key(&self) -> Option<SyntaxNode> {
        self.entry_child(KEY)
    }

    #[must_use]
    pub fn entry_value(&self) -> Option<SyntaxNode> {
        self.entry_child(VALUE)
    }

    /// Find a child of the cursor's nearest entry.
    fn entry_child(&self, kind: taplo::syntax::SyntaxKind) -> Option<SyntaxNode> {
        let syntax = &self.before.as_ref().or(self.after.as_ref())?.syntax;
        syntax
            .parent_ancestors()
            .find(|n| n.kind() == ENTRY)
            .and_then(|entry| entry.children().find(|child| child.kind() == kind))
    }

    /// Return an identifier token's zero-based segment index within its table header.
    #[must_use]
    pub fn header_identifier_index(&self, token: &SyntaxToken) -> Option<usize> {
        if token.kind() != IDENT {
            return None;
        }
        let header = token
            .parent_ancestors()
            .find(|node| matches!(node.kind(), TABLE_HEADER | TABLE_ARRAY_HEADER))?;
        let key = header.descendants().find(|node| node.kind() == KEY)?;
        key.descendants_with_tokens()
            .filter_map(SyntaxElement::into_token)
            .filter(|candidate| candidate.kind() == IDENT)
            .position(|candidate| candidate == *token)
    }

    #[must_use]
    pub fn parent_table_or_array_table(&self, root: &Node) -> (Keys, Node) {
        let syntax = match self.before.as_ref().or(self.after.as_ref()) {
            Some(s) => s.syntax.clone(),
            None => return (Keys::empty(), root.clone()),
        };

        let Some(root_syntax) = root.syntax().and_then(|syntax| syntax.as_node()) else {
            return (Keys::empty(), root.clone());
        };
        let last_header = root_syntax
            .descendants()
            .skip(1)
            .filter(|n| matches!(n.kind(), TABLE_HEADER | TABLE_ARRAY_HEADER))
            .take_while(|n| n.text_range().end() <= syntax.text_range().end())
            .last();

        let Some(last_header) = last_header else {
            return (Keys::empty(), root.clone());
        };

        let Some(key_syntax) = last_header.descendants().find(|node| node.kind() == KEY) else {
            return (Keys::empty(), root.clone());
        };
        let keys = Keys::from_syntax(key_syntax.into());
        let Some(node) = root.path(&keys) else {
            return (Keys::empty(), root.clone());
        };

        (keys, node)
    }

    #[must_use]
    pub fn empty_line(&self) -> bool {
        let before_syntax = match self.before.as_ref() {
            Some(s) => &s.syntax,
            None => return true,
        };

        match &self.after {
            Some(after) => {
                if matches!(after.syntax.kind(), WHITESPACE | NEWLINE) {
                    let new_line_after = after
                        .syntax
                        .siblings_with_tokens(Direction::Next)
                        .find_map(|s| match s.kind() {
                            NEWLINE => Some(true),
                            WHITESPACE | COMMENT => None,
                            _ => Some(false),
                        })
                        .unwrap_or(true);

                    if !new_line_after {
                        return false;
                    }
                } else {
                    return false;
                }
            }
            None => {}
        }

        before_syntax
            .siblings_with_tokens(Direction::Prev)
            .find_map(|s| match s.kind() {
                NEWLINE => Some(true),
                WHITESPACE | COMMENT => None,
                _ => Some(false),
            })
            .unwrap_or(true)
    }

    #[must_use]
    pub fn in_entry_keys(&self) -> bool {
        self.entry_key()
            .is_some_and(|k| k.text_range().contains(self.offset))
    }

    #[must_use]
    pub fn entry_has_eq(&self) -> bool {
        let Some(key_syntax) = self.entry_key() else {
            return false;
        };

        key_syntax
            .siblings(Direction::Next)
            .find_map(|s| match s.kind() {
                EQ => Some(true),
                WHITESPACE => None,
                _ => Some(false),
            })
            .unwrap_or(false)
    }

    #[must_use]
    pub fn in_entry_value(&self) -> bool {
        let in_value = self
            .entry_value()
            // We are inside the value even if the cursor is right after it.
            .is_some_and(|k| k.text_range().contains_inclusive(self.offset));

        if in_value {
            return true;
        }

        let syntax = match self.before.as_ref().or(self.after.as_ref()) {
            Some(p) => &p.syntax,
            None => return false,
        };

        syntax
            .siblings_with_tokens(Direction::Prev)
            .find_map(|s| match s.kind() {
                EQ => Some(true),
                WHITESPACE | COMMENT | NEWLINE => None,
                _ => Some(false),
            })
            .unwrap_or(false)
    }

    #[must_use]
    pub fn is_single_quote_value(&self) -> bool {
        self.entry_value().is_some_and(|v| {
            v.descendants_with_tokens()
                .any(|t| matches!(t.kind(), STRING_LITERAL | MULTI_LINE_STRING_LITERAL))
        })
    }

    #[must_use]
    pub fn is_inline(&self) -> bool {
        let syntax = match self.before.as_ref().or(self.after.as_ref()) {
            Some(p) => &p.syntax,
            None => return false,
        };

        syntax
            .parent_ancestors()
            .any(|a| matches!(a.kind(), INLINE_TABLE | ARRAY))
    }

    #[must_use]
    pub fn in_inline_table(&self) -> bool {
        let syntax = match self.before.as_ref().or(self.after.as_ref()) {
            Some(p) => &p.syntax,
            None => return false,
        };

        match syntax.parent() {
            Some(parent) => {
                if parent.kind() != INLINE_TABLE {
                    return false;
                }

                parent
                    .children_with_tokens()
                    .find_map(|t| {
                        if t.kind() == BRACE_END {
                            Some(self.offset <= t.text_range().start())
                        } else {
                            None
                        }
                    })
                    .unwrap_or(true)
            }
            None => false,
        }
    }

    #[must_use]
    pub fn in_array(&self) -> bool {
        let syntax = match self.before.as_ref().or(self.after.as_ref()) {
            Some(p) => &p.syntax,
            None => return false,
        };

        match syntax.parent() {
            Some(parent) => {
                if parent.kind() != ARRAY {
                    return false;
                }

                parent
                    .children_with_tokens()
                    .find_map(|t| {
                        if t.kind() == BRACKET_END {
                            Some(self.offset <= t.text_range().start())
                        } else {
                            None
                        }
                    })
                    .unwrap_or(true)
            }
            None => false,
        }
    }

    pub fn entry_keys(&self) -> Keys {
        self.entry_key()
            .map_or_else(Keys::empty, |keys| Keys::from_syntax(keys.into()))
    }

    pub fn header_keys(&self) -> Keys {
        self.header_key()
            .map_or_else(Keys::empty, |keys| Keys::from_syntax(keys.into()))
    }

    #[must_use]
    pub fn dom_node(&self) -> Option<&(Keys, Node)> {
        self.before
            .as_ref()
            .and_then(|p| p.dom_node.as_ref())
            .or_else(|| self.after.as_ref().and_then(|p| p.dom_node.as_ref()))
    }
}

/// Transform the lookup keys to account for arrays of tables and arrays.
///
/// It appends an index after each array so that we get the item type
/// during lookups.
#[must_use]
pub fn lookup_keys(root: Node, keys: &Keys) -> Keys {
    let mut node = root;
    let mut new_keys = Keys::empty();

    for key in keys.iter().cloned() {
        node = node.get(&key);
        new_keys = new_keys.join(key);
        if let Some(arr) = node.as_array() {
            new_keys = new_keys.join(arr.items().read().len().saturating_sub(1));
        }
    }

    new_keys
}

#[derive(Debug, Clone)]
pub struct PositionInfo {
    /// The narrowest syntax element that contains the position.
    pub syntax: SyntaxToken,
    /// The narrowest node that covers the position.
    pub dom_node: Option<(Keys, Node)>,
}

fn full_range(keys: &Keys, node: &Node) -> TextRange {
    let Some(last_key) = keys
        .iter()
        .filter_map(KeyOrIndex::as_key)
        .next_back()
        .map(Key::text_ranges)
    else {
        return join_ranges(node.text_ranges(true));
    };

    join_ranges(last_key.chain(node.text_ranges(true)))
}

#[cfg(test)]
mod tests {
    use super::Query;
    use strict_test_support::{ensure, ensure_eq, ensure_ok, ensure_some, TestFailure};
    use taplo::{
        dom::{node::DomNode, Node},
        rowan::{TextSize, TokenAtOffset},
        syntax::SyntaxKind::{BOOL, IDENT, INTEGER, PERIOD},
    };

    /// Parse one source fixture into its tolerant DOM.
    fn dom(source: &str) -> Node {
        taplo::parser::parse(source).into_dom()
    }

    #[test]
    fn cursor_selection_prefers_before_then_falls_back_after() -> Result<(), TestFailure> {
        let before_dom = dom("key = 1\n");
        let before_query = Query::at(&before_dom, TextSize::from(3));
        let before = ensure_some(
            before_query.first_matching(|position| position.syntax.kind() == IDENT),
            "an identifier immediately before the cursor must match",
        )?;
        ensure_eq(
            &before.syntax.text(),
            &"key",
            "the before candidate must take precedence",
        )?;

        let after_dom = dom(" key = 1\n");
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
        ensure_eq(
            &after.syntax.text(),
            &"key",
            "the after candidate must retain its identifier",
        )?;
        ensure(
            after_query
                .first_matching(|position| position.syntax.kind() == BOOL)
                .is_none(),
            "cursor-adjacent nonboolean syntax must produce no boolean candidate",
        )
    }

    #[test]
    fn entry_lookup_and_header_identifier_index_are_total() -> Result<(), TestFailure> {
        let entry_dom = dom("alpha = 1\n");
        let entry_query = Query::at(&entry_dom, TextSize::from(8));
        let key = ensure_some(entry_query.entry_key(), "an entry query must find its key")?;
        let value = ensure_some(entry_query.entry_value(), "an entry query must find its value")?;
        ensure(
            key.kind() == taplo::syntax::SyntaxKind::KEY,
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

        let header_dom = dom("[alpha.beta]\n");
        let header_query = Query::at(&header_dom, TextSize::from(8));
        let beta = ensure_some(
            header_query.first_matching(|position| position.syntax.text() == "beta"),
            "the dotted header fixture must expose its second identifier",
        )?;
        ensure(
            header_query.header_identifier_index(&beta.syntax) == Some(1),
            "the second dotted identifier must have index one",
        )?;

        let header_syntax = ensure_some(
            header_dom.syntax().and_then(|syntax| syntax.as_node()),
            "the parsed header must retain root syntax",
        )?;
        let period = ensure_some(
            header_syntax
                .descendants_with_tokens()
                .filter_map(taplo::syntax::SyntaxElement::into_token)
                .find(|token| token.kind() == PERIOD),
            "the dotted header must contain a period token",
        )?;
        ensure(
            header_query.header_identifier_index(&period).is_none(),
            "a nonidentifier header token must not receive a segment index",
        )?;

        let entry_syntax = ensure_some(
            entry_dom.syntax().and_then(|syntax| syntax.as_node()),
            "the parsed entry must retain root syntax",
        )?;
        let entry_identifier = match entry_syntax.token_at_offset(TextSize::from(1)) {
            TokenAtOffset::Single(token) | TokenAtOffset::Between(_, token) => Some(token),
            TokenAtOffset::None => None,
        };
        ensure(
            ensure_some(entry_identifier, "the entry identifier must be addressable")?
                .kind()
                == IDENT,
            "the out-of-header fixture must use an identifier token",
        )?;
        ensure(
            header_query
                .header_identifier_index(&ensure_some(
                    match entry_syntax.token_at_offset(TextSize::from(1)) {
                        TokenAtOffset::Single(token) | TokenAtOffset::Between(_, token) => {
                            Some(token)
                        }
                        TokenAtOffset::None => None,
                    },
                    "the entry identifier must remain addressable",
                )?)
                .is_none(),
            "an identifier outside a table header must not receive a header index",
        )?;

        let outside_entry = Query::at(&header_dom, TextSize::from(2));
        ensure(
            outside_entry.entry_key().is_none() && outside_entry.entry_value().is_none(),
            "a table header query must not invent entry children",
        )?;
        ensure(
            value.kind() == taplo::syntax::SyntaxKind::VALUE,
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
            syntaxless_query.before.is_none() && syntaxless_query.after.is_none(),
            "syntaxless DOM input must return an empty query",
        )?;
        let (keys, parent) = syntaxless_query.parent_table_or_array_table(&syntaxless);
        ensure(keys.is_empty(), "syntaxless parent lookup must use empty keys")?;
        ensure_eq(
            &ensure_ok(
                serde_json::to_value(parent),
                "the fallback parent DOM must serialize",
            )?,
            &serde_json::json!({ "value": 1 }),
            "syntaxless parent lookup must return the supplied root",
        )?;

        let parsed = dom("value = 1\n");
        let outside = Query::at(&parsed, TextSize::new(u32::MAX));
        ensure(
            outside.before.is_none() && outside.after.is_none(),
            "an unusable offset must return an empty query instead of panicking",
        )
    }
}
