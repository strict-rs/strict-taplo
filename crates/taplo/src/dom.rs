//! Immutable semantic TOML values constructed from Taplo's lossless syntax.
//!
//! The DOM decodes keys and scalar values while retaining source anchors, insertion order, and
//! recoverable semantic diagnostics. It owns semantic paths, document comments, queries,
//! rendering, and source-preserving rewrites; syntax parsing remains the parser's responsibility.

use core::fmt;
use core::fmt::Display;
use core::fmt::Formatter;
use core::hash::Hash;
use core::hash::Hasher;
use core::iter::once;
use std::iter::empty;
use std::slice;
use std::str::FromStr;
use std::sync::Arc;
use std::vec::IntoIter;

use self::from_syntax::keys_from_syntax;
use self::node::ArenaEntries;
use self::node::ArenaEntry;
use self::node::DomArena;
use self::node::Key;
use self::node::NodeId;
use crate::parser::Parser;
use crate::syntax::SyntaxElement;
use crate::syntax::SyntaxKind;
use crate::util::try_join_ranges;

#[cfg(feature = "serde")]
/// Serde projection for detached and source-backed semantic values.
mod serde;

/// Two-phase mutable construction and immutable DOM publication.
mod from_syntax;

/// Typed semantic diagnostics and query failures.
pub mod error;
/// Immutable table, array, scalar, key, and malformed-node wrappers.
pub mod node;
/// Exact-path source-preserving query and rewrite transactions.
pub mod rewrite;
/// Typed semantic DOM rendering into valid TOML source.
mod to_toml;

pub use error::Diagnostic;
pub use error::QueryError;
use itertools::Itertools as _;
pub use node::Node;
use rowan::TextRange;
pub use to_toml::RenderError;

/// Convert one syntax element through the private mutable builder and publish
/// its immutable DOM arena.
pub(crate) fn node_from_syntax(syntax: SyntaxElement) -> Node {
  from_syntax::node_from_syntax(syntax)
}

/// One segment in a semantic TOML path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum KeyOrIndex {
  /// A table key.
  Key(Key),
  /// An array index.
  Index(usize),
}

impl<N> From<N> for KeyOrIndex
where
  N: Into<usize>,
{
  fn from(index: N) -> Self {
    Self::Index(index.into())
  }
}

impl From<Key> for KeyOrIndex {
  fn from(key: Key) -> Self {
    Self::Key(key)
  }
}

impl PartialEq<str> for KeyOrIndex {
  fn eq(&self, other: &str) -> bool {
    match *self {
      Self::Key(ref key) => key.value() == other,
      Self::Index(_) => false,
    }
  }
}

impl Display for KeyOrIndex {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match *self {
      Self::Key(ref key) => key.fmt(formatter),
      Self::Index(index) => index.fmt(formatter),
    }
  }
}

impl KeyOrIndex {
  /// Returns `true` if the key or index is [`Key`].
  ///
  /// [`Key`]: KeyOrIndex::Key
  #[allow(
    clippy::single_call_fn,
    reason = "the public predicate completes KeyOrIndex's symmetric segment API alongside is_index, as_key, and as_index"
  )]
  #[must_use]
  pub const fn is_key(&self) -> bool {
    matches!(self, Self::Key(..))
  }

  /// Returns `true` if the key or index is [`Index`].
  ///
  /// [`Index`]: KeyOrIndex::Index
  #[allow(
    clippy::single_call_fn,
    reason = "the public predicate completes KeyOrIndex's symmetric segment API alongside is_key, as_key, and as_index"
  )]
  #[must_use]
  pub const fn is_index(&self) -> bool {
    matches!(self, Self::Index(..))
  }

  /// Borrow the table-key segment.
  #[allow(
    clippy::single_call_fn,
    reason = "the public projection completes KeyOrIndex's symmetric segment API and drives key-range traversal"
  )]
  #[must_use]
  pub const fn as_key(&self) -> Option<&Key> {
    if let Self::Key(ref key) = *self {
      Some(key)
    } else {
      None
    }
  }

  /// Borrow the array-index segment.
  #[must_use]
  pub const fn as_index(&self) -> Option<&usize> {
    if let Self::Index(ref index) = *self {
      Some(index)
    } else {
      None
    }
  }
}

/// An immutable semantic TOML path.
#[derive(Debug, Clone)]
pub struct Keys {
  /// Pre-rendered dotted form used by display and table-header rendering.
  dotted: Arc<str>,
  /// Typed key and array-index segments in traversal order.
  keys:   Arc<[KeyOrIndex]>,
}

impl Keys {
  #[inline]
  /// Construct an empty path.
  #[must_use]
  pub fn empty() -> Self {
    Self::new(empty())
  }

  /// Construct a path containing one segment.
  #[must_use]
  pub fn single(key: impl Into<KeyOrIndex>) -> Self {
    Self::new(once(key.into()))
  }

  /// Construct a path from decoded segments.
  #[must_use]
  pub fn new(keys: impl Iterator<Item = KeyOrIndex>) -> Self {
    let collected_keys: Arc<[KeyOrIndex]> = keys.collect();
    let dotted: Arc<str> = Arc::from(collected_keys.iter().join(".").as_str());
    Self {
      dotted,
      keys: collected_keys,
    }
  }

  /// Decode a path from one `KEY` syntax node.
  #[must_use]
  pub fn from_syntax(syntax: &SyntaxElement) -> Self {
    Self::new(keys_from_syntax(syntax).map(Into::into))
  }

  /// Return a new path with one segment appended.
  #[must_use]
  pub fn join(&self, key: impl Into<KeyOrIndex>) -> Self {
    self.extend(once(key.into()))
  }

  /// Return a new path with all supplied segments appended.
  #[must_use]
  pub fn extend<I, K>(&self, keys: I) -> Self
  where
    I: IntoIterator<Item = K>,
    K: Into<KeyOrIndex>,
  {
    Self::new(self.keys.iter().cloned().chain(keys.into_iter().map(Into::into)))
  }

  /// Iterate over path segments.
  #[must_use]
  pub fn iter(&self) -> impl ExactSizeIterator<Item = &KeyOrIndex> + DoubleEndedIterator {
    self.keys.iter()
  }

  /// Return the dotted source representation.
  #[must_use]
  pub fn dotted(&self) -> &str {
    &self.dotted
  }

  /// Return the number of path segments.
  #[must_use]
  pub fn len(&self) -> usize {
    self.keys.len()
  }

  /// Return whether the path contains no segments.
  #[must_use]
  pub fn is_empty(&self) -> bool {
    self.keys.is_empty()
  }

  /// Count the shared leading segments with another path.
  #[must_use]
  pub fn common_prefix_count(&self, other: &Self) -> usize {
    self
      .iter()
      .zip(other.iter())
      .take_while(|segments| segments.0 == segments.1)
      .count()
  }

  /// Return whether this path contains `other` as a prefix.
  #[must_use]
  pub fn contains(&self, other: &Self) -> bool {
    self.len() >= other.len() && self.common_prefix_count(other) == other.len()
  }

  /// Return whether this path is contained by `other`.
  #[must_use]
  pub fn part_of(&self, other: &Self) -> bool {
    other.contains(self)
  }

  /// Return a path without the first `n` segments.
  #[must_use]
  pub fn skip_left(&self, n: usize) -> Self {
    Self::new(self.keys.iter().skip(n).cloned())
  }

  /// Return a path without the final `n` segments.
  #[must_use]
  pub fn skip_right(&self, n: usize) -> Self {
    Self::new(self.keys.iter().rev().skip(n).cloned().rev())
  }

  /// Return one range covering all source-backed key segments.
  #[must_use]
  pub fn all_text_range(&self) -> Option<TextRange> {
    try_join_ranges(self.keys.iter().filter_map(KeyOrIndex::as_key).flat_map(Key::text_ranges))
  }
}

impl IntoIterator for Keys {
  type Item = KeyOrIndex;

  type IntoIter = IntoIter<KeyOrIndex>;

  fn into_iter(self) -> Self::IntoIter {
    Vec::from(&*self.keys).into_iter()
  }
}

impl Display for Keys {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    self.dotted().fmt(formatter)
  }
}

impl FromStr for Keys {
  type Err = QueryError;

  fn from_str(source: &str) -> Result<Self, Self::Err> {
    let parse = Parser::new(source).parse_key_only().map_err(QueryError::from)?;
    if let Some(diagnostic) = parse.diagnostics().first() {
      return Err(QueryError::InvalidKey(diagnostic.clone()));
    }
    Ok(Self::from_syntax(&parse.into_syntax().into()))
  }
}

impl PartialEq for Keys {
  fn eq(&self, other: &Self) -> bool {
    self.keys == other.keys
  }
}

impl Eq for Keys {}

impl Hash for Keys {
  fn hash<H: Hasher>(&self, state: &mut H) {
    self.keys.hash(state);
  }
}

impl From<Key> for Keys {
  fn from(key: Key) -> Self {
    Self::new(once(key.into()))
  }
}

impl<N> From<N> for Keys
where
  N: Into<usize>,
{
  fn from(index: N) -> Self {
    Self::new(once(index.into().into()))
  }
}

/// Immutable view of ordered table entries with a decoded-key lookup index.
#[derive(Clone)]
pub struct Entries {
  /// Immutable entry records.
  inner: Arc<ArenaEntries>,
  /// Arena used to materialize cheap child handles.
  arena: Arc<DomArena>,
}

impl Entries {
  /// Construct a view over one immutable table record.
  #[allow(
    clippy::single_call_fn,
    reason = "the named constructor is the sole arena-plus-record pairing that materializes cheap table-entry handles"
  )]
  pub(in crate::dom) const fn new(arena: Arc<DomArena>, inner: Arc<ArenaEntries>) -> Self {
    Self {
      inner,
      arena,
    }
  }

  /// Return the number of entries, including conflicting historical entries.
  #[must_use]
  pub fn len(&self) -> usize {
    self.inner.all.len()
  }

  /// Return whether there are no entries.
  #[must_use]
  pub fn is_empty(&self) -> bool {
    self.inner.all.is_empty()
  }

  /// Iterate in source insertion order.
  #[must_use]
  pub fn iter(&self) -> EntriesIter<'_> {
    EntriesIter {
      entries: self.inner.all.iter(),
      arena:   &self.arena,
    }
  }
}

impl Default for Entries {
  fn default() -> Self {
    Self {
      inner: Arc::new(ArenaEntries::default()),
      arena: DomArena::empty(),
    }
  }
}

impl fmt::Debug for Entries {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter.debug_struct("Entries").field("len", &self.len()).finish()
  }
}

/// Borrowing iterator over immutable table-entry records.
#[derive(Debug)]
pub struct EntriesIter<'entries> {
  /// Remaining source-order entry records.
  entries: slice::Iter<'entries, ArenaEntry>,
  /// Arena used to materialize child handles.
  arena:   &'entries Arc<DomArena>,
}

impl Iterator for EntriesIter<'_> {
  type Item = (Key, Node);

  fn next(&mut self) -> Option<Self::Item> {
    self
      .entries
      .next()
      .map(|entry| (entry.key.clone(), entry.node.node(Arc::clone(self.arena))))
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    self.entries.size_hint()
  }
}

impl DoubleEndedIterator for EntriesIter<'_> {
  fn next_back(&mut self) -> Option<Self::Item> {
    self
      .entries
      .next_back()
      .map(|entry| (entry.key.clone(), entry.node.node(Arc::clone(self.arena))))
  }
}

impl ExactSizeIterator for EntriesIter<'_> {}

impl<'entries> IntoIterator for &'entries Entries {
  type Item = (Key, Node);
  type IntoIter = EntriesIter<'entries>;

  fn into_iter(self) -> Self::IntoIter {
    self.iter()
  }
}

/// Immutable view of one array's ordered child identities.
#[derive(Clone)]
pub struct ArrayItems {
  /// Immutable child identities.
  items: Arc<[NodeId]>,
  /// Arena used to materialize cheap child handles.
  arena: Arc<DomArena>,
}

impl ArrayItems {
  /// Construct a view over one immutable array record.
  #[allow(
    clippy::single_call_fn,
    reason = "the named constructor is the sole arena-plus-record pairing that materializes cheap array-item handles"
  )]
  pub(in crate::dom) const fn new(arena: Arc<DomArena>, items: Arc<[NodeId]>) -> Self {
    Self {
      items,
      arena,
    }
  }

  /// Return the number of array items.
  #[must_use]
  pub fn len(&self) -> usize {
    self.items.len()
  }

  /// Return whether the array contains no items.
  #[must_use]
  pub fn is_empty(&self) -> bool {
    self.items.is_empty()
  }

  /// Return one item by index.
  #[must_use]
  pub fn get(&self, index: usize) -> Option<Node> {
    self.items.get(index).map(|id| id.node(Arc::clone(&self.arena)))
  }

  /// Return the first array item.
  #[must_use]
  pub fn first(&self) -> Option<Node> {
    self.items.first().map(|id| id.node(Arc::clone(&self.arena)))
  }

  /// Iterate over cheap owned child handles.
  #[must_use]
  pub fn iter(&self) -> ArrayItemsIter<'_> {
    ArrayItemsIter {
      items: self.items.iter(),
      arena: &self.arena,
    }
  }
}

impl fmt::Debug for ArrayItems {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter.debug_struct("ArrayItems").field("len", &self.len()).finish()
  }
}

/// Borrowing iterator over one array's immutable child identities.
#[derive(Debug)]
pub struct ArrayItemsIter<'items> {
  /// Remaining child identities.
  items: slice::Iter<'items, NodeId>,
  /// Arena used to materialize public handles.
  arena: &'items Arc<DomArena>,
}

impl Iterator for ArrayItemsIter<'_> {
  type Item = Node;

  fn next(&mut self) -> Option<Self::Item> {
    self.items.next().map(|id| id.node(Arc::clone(self.arena)))
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    self.items.size_hint()
  }
}

impl DoubleEndedIterator for ArrayItemsIter<'_> {
  fn next_back(&mut self) -> Option<Self::Item> {
    self.items.next_back().map(|id| id.node(Arc::clone(self.arena)))
  }
}

impl ExactSizeIterator for ArrayItemsIter<'_> {}

/// Owning iterator over one array's immutable child identities.
#[derive(Debug)]
pub struct ArrayItemsIntoIter {
  /// Remaining child identities.
  items: IntoIter<NodeId>,
  /// Arena used to materialize public handles.
  arena: Arc<DomArena>,
}

impl Iterator for ArrayItemsIntoIter {
  type Item = Node;

  fn next(&mut self) -> Option<Self::Item> {
    self.items.next().map(|id| id.node(Arc::clone(&self.arena)))
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    self.items.size_hint()
  }
}

impl DoubleEndedIterator for ArrayItemsIntoIter {
  fn next_back(&mut self) -> Option<Self::Item> {
    self.items.next_back().map(|id| id.node(Arc::clone(&self.arena)))
  }
}

impl ExactSizeIterator for ArrayItemsIntoIter {}

impl IntoIterator for ArrayItems {
  type Item = Node;
  type IntoIter = ArrayItemsIntoIter;

  fn into_iter(self) -> Self::IntoIter {
    ArrayItemsIntoIter {
      items: Vec::from(&*self.items).into_iter(),
      arena: self.arena,
    }
  }
}

impl<'items> IntoIterator for &'items ArrayItems {
  type Item = Node;
  type IntoIter = ArrayItemsIter<'items>;

  fn into_iter(self) -> Self::IntoIter {
    self.iter()
  }
}

/// An immutable source-backed or detached TOML comment.
#[derive(Debug, Clone, Default)]
pub struct Comment {
  /// Optional source token retained for source-backed display and ranges.
  syntax: Option<SyntaxElement>,
  /// Eagerly decoded ordinary comment or directive payload.
  value:  CommentValue,
}

/// Immutable document-wide comment index shared by every frozen DOM node.
#[derive(Debug, Default)]
pub(crate) struct CommentStore {
  /// Every decoded source comment in document order.
  all:        Arc<[Comment]>,
  /// Number of leading comments preceding the first document item.
  header_len: usize,
}

impl CommentStore {
  /// Decode the comments belonging to the syntax element's document root.
  #[allow(
    clippy::single_call_fn,
    reason = "the named constructor owns document-wide comment discovery and header-boundary calculation"
  )]
  pub(crate) fn from_syntax(syntax: &SyntaxElement) -> Self {
    let Some(root) = syntax.ancestors().last() else {
      return Self::default();
    };
    let all: Arc<[Comment]> = root
      .descendants_with_tokens()
      .filter(|element| element.kind() == SyntaxKind::COMMENT)
      .map(Comment::from_syntax_element)
      .collect();
    let header_len = root.descendants().nth(1).map_or(all.len(), |first_item| {
      all
        .iter()
        .take_while(|comment| {
          comment
            .syntax()
            .is_some_and(|comment_syntax| comment_syntax.text_range().end() <= first_item.text_range().start())
        })
        .count()
    });
    Self {
      all,
      header_len,
    }
  }

  /// Borrow every decoded document comment.
  pub(crate) fn all(&self) -> &[Comment] {
    &self.all
  }

  /// Borrow the leading document comments.
  pub(crate) fn header(&self) -> &[Comment] {
    self.all.get(..self.header_len).unwrap_or_default()
  }
}

impl Comment {
  /// Construct a detached ordinary comment.
  #[allow(
    clippy::single_call_fn,
    reason = "the public constructor is the detached ordinary-comment entry point paired with new_directive"
  )]
  #[must_use]
  pub fn new(comment_text: impl Into<String>) -> Self {
    Self {
      syntax: None,
      value:  CommentValue::Comment(comment_text.into()),
    }
  }

  /// Construct a detached directive comment.
  #[allow(
    clippy::single_call_fn,
    reason = "the public constructor is the detached directive-comment entry point paired with new"
  )]
  #[must_use]
  pub fn new_directive(name: impl Into<String>, directive_value: impl Into<String>) -> Self {
    Self {
      syntax: None,
      value:  CommentValue::Directive {
        name:  name.into(),
        value: directive_value.into(),
      },
    }
  }

  /// Construct a comment by eagerly decoding a syntax token.
  #[allow(
    clippy::single_call_fn,
    reason = "the conversion callback binds one source token to its eagerly decoded immutable comment value"
  )]
  pub(crate) fn from_syntax_element(syntax: SyntaxElement) -> Self {
    let comment_value = syntax
      .as_token()
      .map(rowan::SyntaxToken::text)
      .map_or_else(CommentValue::default, decode_comment);
    Self {
      syntax: Some(syntax),
      value:  comment_value,
    }
  }

  /// Return the immutable syntax anchor retained from the source.
  #[must_use]
  pub const fn syntax(&self) -> Option<&SyntaxElement> {
    self.syntax.as_ref()
  }

  /// Return whether this comment is a directive.
  #[must_use]
  pub const fn is_directive(&self) -> bool {
    self.value.is_directive()
  }

  /// Return the directive name, if this is a directive.
  #[must_use]
  pub fn directive(&self) -> Option<&str> {
    if let CommentValue::Directive {
      ref name, ..
    } = self.value
    {
      Some(name)
    } else {
      None
    }
  }

  /// Return the ordinary comment text or directive value.
  #[must_use]
  pub fn value(&self) -> &str {
    match self.value {
      CommentValue::Comment(ref comment) => comment,
      CommentValue::Directive {
        value: ref directive_text,
        ..
      } => directive_text,
    }
  }
}

impl Display for Comment {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    if let Some(syntax) = self.syntax.as_ref() {
      syntax.fmt(formatter)
    } else {
      match self.value {
        CommentValue::Comment(ref comment) => {
          formatter.write_str("#")?;
          comment.fmt(formatter)
        }
        CommentValue::Directive {
          ref name,
          value: ref directive_text,
        } => {
          formatter.write_str("#:")?;
          name.fmt(formatter)?;
          formatter.write_str(" ")?;
          directive_text.fmt(formatter)
        }
      }
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Eager semantic interpretation of one TOML comment token.
enum CommentValue {
  /// Ordinary comment text following the leading hash.
  Comment(String),
  /// Structured directive introduced by `#:`.
  Directive {
    /// Directive name.
    name:  String,
    /// Directive payload following the name.
    value: String,
  },
}

impl CommentValue {
  /// Returns `true` if the comment value is [`Directive`].
  ///
  /// [`Directive`]: CommentValue::Directive
  const fn is_directive(&self) -> bool {
    matches!(self, Self::Directive { .. })
  }
}

impl Default for CommentValue {
  fn default() -> Self {
    Self::Comment(String::new())
  }
}

/// Decode one comment token into its semantic value.
#[allow(
  clippy::single_call_fn,
  reason = "the named decoder centralizes the distinction between directive metadata and ordinary comment text"
)]
fn decode_comment(text: &str) -> CommentValue {
  if let Some(directive_content) = text.strip_prefix("#:") {
    let mut fields = directive_content.split_whitespace();
    return CommentValue::Directive {
      name:  fields.next().unwrap_or("").into(),
      value: fields.next().unwrap_or("").into(),
    };
  }
  text
    .strip_prefix('#')
    .map_or_else(CommentValue::default, |content| CommentValue::Comment(content.into()))
}

#[cfg(test)]
/// Semantic path identity and hashing contracts.
mod tests {
  use std::collections::HashSet;
  use std::str::FromStr as _;

  use rowan::TextRange;
  use rowan::TextSize;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;

  use super::Comment;
  use super::Entries;
  use super::Key;
  use super::KeyOrIndex;
  use super::Keys;
  use super::Node;
  use crate::test_support::parse_dom;

  /// Parse the document shared by immutable table-entry and array-item view contracts.
  fn collection_document() -> Result<Node, TestFailure> {
    parse_dom(
      "first = 1\nsecond = 2\nvalues = [3, 4, 5]\n",
      "the immutable collection-view fixture must parse",
    )
  }

  /// Distinguish a numeric table key from an array index in equality and hashing.
  #[test]
  fn path_identity_preserves_segment_kind() -> Result<(), TestFailure> {
    let numeric_key = Keys::single(Key::new("0"));
    let array_index = Keys::single(0_usize);
    ensure(
      numeric_key != array_index,
      "a table key that renders as a number must not equal an array index",
    )?;

    let distinct = HashSet::from([numeric_key, array_index]);
    ensure_eq(
      &distinct.len(),
      &2,
      "path hashing must preserve the same key-versus-index distinction as equality",
    )
  }

  #[test]
  fn path_segments_preserve_key_and_index_identity() -> Result<(), TestFailure> {
    let key = KeyOrIndex::from(Key::new("alpha"));
    let index = KeyOrIndex::from(2_usize);
    ensure(
      (
        key.is_key(),
        key.is_index(),
        key.as_key().map(Key::value),
        key.as_index(),
        <KeyOrIndex as PartialEq<str>>::eq(&key, "alpha"),
        key.to_string(),
      ) == (true, false, Some("alpha"), None, true, String::from("alpha")),
      "a key segment must expose only key identity and its decoded display value",
    )?;
    ensure(
      (
        index.is_index(),
        index.is_key(),
        index.as_index(),
        index.as_key().map(Key::value),
        <KeyOrIndex as PartialEq<str>>::eq(&index, "2"),
        index.to_string(),
      ) == (true, false, Some(&2_usize), None, false, String::from("2")),
      "an index segment must expose only numeric identity without comparing equal to a numeric key",
    )
  }

  #[test]
  fn path_operations_preserve_order_prefixes_and_source_identity() -> Result<(), TestFailure> {
    let empty = Keys::empty();
    let parent = Keys::single(Key::new("parent"));
    let child = parent.join(Key::new("child"));
    let indexed = child.extend([0_usize, 1_usize]);
    ensure(
      (
        empty.is_empty(),
        empty.len(),
        parent.dotted(),
        child.dotted(),
        indexed.dotted(),
        indexed.len(),
      ) == (true, 0, "parent", "parent.child", "parent.child.0.1", 4),
      "path construction must preserve typed segment order and the matching dotted representation",
    )?;
    ensure(
      (
        indexed.common_prefix_count(&child),
        indexed.contains(&child),
        child.part_of(&indexed),
        child.contains(&indexed),
        indexed.skip_left(2),
        indexed.skip_right(2),
        indexed.skip_left(99).is_empty(),
        indexed.skip_right(99).is_empty(),
      ) == (
        2,
        true,
        true,
        false,
        Keys::new([KeyOrIndex::from(0_usize), KeyOrIndex::from(1_usize)].into_iter()),
        child.clone(),
        true,
        true,
      ),
      "path prefix and slicing operations must distinguish containment direction and saturate at empty",
    )?;
    ensure(
      (
        indexed.clone().into_iter().collect::<Vec<_>>(),
        indexed.iter().cloned().collect::<Vec<_>>(),
        indexed.iter().next().map(KeyOrIndex::is_key),
        indexed.iter().next_back().map(KeyOrIndex::is_index),
      ) == (
        indexed.iter().cloned().collect::<Vec<_>>(),
        indexed.iter().cloned().collect::<Vec<_>>(),
        Some(true),
        Some(true),
      ),
      "owned and borrowed path iteration must preserve the same double-ended segment order",
    )?;

    let source_backed = ensure_ok(Keys::from_str("parent.\"child\""), "a valid dotted key path must parse")?;
    ensure(
      (source_backed.all_text_range(), Keys::single(Key::new("detached")).all_text_range())
        == (Some(TextRange::new(TextSize::new(0), TextSize::new(14))), None),
      "source-backed paths must cover every key segment while detached paths retain no fabricated range",
    )?;
    ensure(
      Keys::from_str("parent.").is_err(),
      "an incomplete dotted key path must retain its typed parse failure",
    )
  }

  #[test]
  fn immutable_entry_views_iterate_from_both_ends_without_losing_identity() -> Result<(), TestFailure> {
    let root = collection_document()?;
    let table = ensure_some(root.as_table(), "the collection-view root must remain a table")?;
    let entries = table.entries();
    let entry_names = (&entries)
      .into_iter()
      .map(|entry| entry.0.value().to_owned())
      .collect::<Vec<_>>();
    ensure(
      (entries.len(), entries.is_empty(), format!("{entries:?}"), entry_names)
        == (
          3,
          false,
          String::from("Entries { len: 3 }"),
          ["first", "second", "values"].map(str::to_owned).to_vec(),
        ),
      "entry views must expose exact cardinality, debug summary, and source insertion order",
    )?;
    let mut entry_iter = entries.iter();
    let entry_size = entry_iter.size_hint();
    let front = entry_iter.next().map(|entry| entry.0.value().to_owned());
    let back = entry_iter.next_back().map(|entry| entry.0.value().to_owned());
    ensure(
      (entry_size, front, back, entry_iter.len()) == ((3, Some(3)), Some(String::from("first")), Some(String::from("values")), 1),
      "entry iterators must preserve size and independent front/back traversal",
    )?;
    let empty_entries = Entries::default();
    ensure(
      (
        empty_entries.is_empty(),
        empty_entries.len(),
        empty_entries.iter().next().is_none(),
        empty_entries.iter().next_back().is_none(),
      ) == (true, 0, true, true),
      "the default entry view must be a complete empty double-ended collection",
    )
  }

  #[test]
  fn immutable_array_views_iterate_from_both_ends_without_losing_identity() -> Result<(), TestFailure> {
    let root = collection_document()?;
    let values = ensure_some(root.get_key("values"), "the array-view fixture must exist")?;
    let items = ensure_some(values.as_array(), "the array-view fixture must remain an array")?.items();
    let first = ensure_some(items.first(), "the first immutable array item must exist")?;
    let last = ensure_some(items.get(2), "the final immutable array item must exist")?;
    let first_text = ensure_ok(first.to_toml(false, false), "the first immutable array item must render")?;
    let last_text = ensure_ok(last.to_toml(false, false), "the final immutable array item must render")?;
    ensure(
      (
        items.len(),
        items.is_empty(),
        first_text,
        last_text,
        items.get(3).is_none(),
        format!("{items:?}"),
      ) == (
        3,
        false,
        String::from("3"),
        String::from("5"),
        true,
        String::from("ArrayItems { len: 3 }"),
      ),
      "array views must expose ordered bounds-checked children and a stable cardinality summary",
    )?;
    let mut borrowed = items.iter();
    let borrowed_size = borrowed.size_hint();
    let borrowed_first = ensure_some(borrowed.next(), "borrowed iteration must yield its first item")?;
    let borrowed_last = ensure_some(borrowed.next_back(), "borrowed iteration must yield its final item")?;
    ensure(
      (
        borrowed_size,
        ensure_ok(borrowed_first.to_toml(false, false), "the first borrowed array item must render")?,
        ensure_ok(borrowed_last.to_toml(false, false), "the final borrowed array item must render")?,
        borrowed.len(),
      ) == ((3, Some(3)), String::from("3"), String::from("5"), 1),
      "borrowed array iteration must preserve size and independent front/back traversal",
    )?;
    let mut owned = items.clone().into_iter();
    let owned_size = owned.size_hint();
    let owned_first = ensure_some(owned.next(), "owned iteration must yield its first item")?;
    let owned_last = ensure_some(owned.next_back(), "owned iteration must yield its final item")?;
    ensure(
      (
        owned_size,
        ensure_ok(owned_first.to_toml(false, false), "the first owned array item must render")?,
        ensure_ok(owned_last.to_toml(false, false), "the final owned array item must render")?,
        owned.len(),
        (&items).into_iter().count(),
      ) == ((3, Some(3)), String::from("3"), String::from("5"), 1, 3),
      "owned and borrowed array iteration must materialize the same immutable child identities",
    )
  }

  #[test]
  fn detached_and_source_comments_preserve_directive_and_header_polarities() -> Result<(), TestFailure> {
    let ordinary = Comment::new(" ordinary");
    let directive = Comment::new_directive("schema", "memory://fixture");
    let empty = Comment::default();
    ensure(
      (
        ordinary.syntax(),
        ordinary.is_directive(),
        ordinary.directive(),
        ordinary.value(),
        ordinary.to_string(),
        empty.to_string(),
      ) == (None, false, None, " ordinary", String::from("# ordinary"), String::from("#")),
      "detached ordinary comments must preserve their text without fabricating directive metadata or source",
    )?;
    ensure(
      (
        directive.syntax(),
        directive.is_directive(),
        directive.directive(),
        directive.value(),
        directive.to_string(),
      ) == (
        None,
        true,
        Some("schema"),
        "memory://fixture",
        String::from("#:schema memory://fixture"),
      ),
      "detached directives must preserve name, payload, and canonical rendering",
    )?;

    let root = parse_dom(
      "# header\n#:schema memory://fixture\nvalue = 1 # trailing\n",
      "the source-comment fixture must parse",
    )?;
    let comments = root.comments().collect::<Vec<_>>();
    let headers = root.header_comments().collect::<Vec<_>>();
    let first = ensure_some(comments.first(), "the first source comment must exist")?;
    let source_directive = ensure_some(comments.get(1), "the source directive must exist")?;
    let trailing = ensure_some(comments.last(), "the trailing source comment must exist")?;
    ensure(
      (
        comments.len(),
        headers.len(),
        comments.iter().map(|comment| comment.syntax().is_some()).collect::<Vec<_>>(),
        (first.is_directive(), first.value(), first.to_string()),
        (
          source_directive.is_directive(),
          source_directive.directive(),
          source_directive.value(),
          source_directive.to_string(),
        ),
        trailing.value(),
        headers.iter().map(Comment::to_string).collect::<Vec<_>>(),
      ) == (
        3,
        2,
        vec![true, true, true],
        (false, " header", String::from("# header")),
        (true, Some("schema"), "memory://fixture", String::from("#:schema memory://fixture")),
        " trailing",
        vec![String::from("# header"), String::from("#:schema memory://fixture")],
      ),
      "source comments must retain exact token rendering while header selection excludes trailing comments",
    )
  }
}
