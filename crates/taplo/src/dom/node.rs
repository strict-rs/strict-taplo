//! Immutable concrete node types for decoded TOML values and malformed source.
//!
//! [`Node`] provides unified traversal across the table, array, scalar, and invalid-node wrappers
//! re-exported by this module. Each wrapper retains its source anchor and direct semantic
//! diagnostics, while comment-bearing values share the containing document's frozen comment
//! index.

use std::fmt::Write;

use crate::syntax::SyntaxElement;

/// Concrete immutable storage wrappers and decoded scalar value types.
mod nodes;
pub(super) use nodes::ArenaEntries;
pub(super) use nodes::ArenaEntry;
pub use nodes::Array;
pub(super) use nodes::ArrayInner;
pub use nodes::ArrayKind;
pub use nodes::Bool;
pub(super) use nodes::BoolInner;
pub use nodes::DateTime;
pub(super) use nodes::DateTimeInner;
pub use nodes::DateTimeValue;
pub use nodes::DecodeFailure;
pub(super) use nodes::DomArena;
pub use nodes::Float;
pub(super) use nodes::FloatInner;
pub use nodes::Integer;
pub(super) use nodes::IntegerInner;
pub use nodes::IntegerRepr;
pub use nodes::IntegerValue;
pub use nodes::Invalid;
pub(super) use nodes::InvalidInner;
pub use nodes::InvalidReason;
pub use nodes::Key;
pub(super) use nodes::KeyInner;
pub use nodes::MalformedScalar;
pub(super) use nodes::NodeId;
pub(super) use nodes::NodeSeed;
pub use nodes::ScalarKind;
pub use nodes::Str;
pub(super) use nodes::StrInner;
pub use nodes::StrRepr;
pub use nodes::Table;
pub(super) use nodes::TableInner;
pub use nodes::TableKind;
use rowan::TextRange;

use super::Comment;
use super::CommentStore;
use super::KeyOrIndex;
use super::Keys;
use super::RenderError;
use super::error::Diagnostic;
use super::error::QueryError;

/// An immutable semantic TOML node.
#[derive(Debug, Clone)]
pub enum Node {
  /// A regular, inline, or implicit table.
  Table(Table),
  /// An inline array or array of tables.
  Array(Array),
  /// A Boolean value.
  Bool(Bool),
  /// A string value.
  Str(Str),
  /// An integer value.
  Integer(Integer),
  /// A floating-point value.
  Float(Float),
  /// A date or time value.
  Date(DateTime),
  /// Malformed source retained for tolerant editing.
  Invalid(Invalid),
}

/// Explicit work item for range collection without recursive stack growth.
enum RangeTask {
  /// Visit one semantic node.
  Node {
    /// Node to visit.
    node:             Node,
    /// Whether descendants contribute ranges.
    include_children: bool,
  },
  /// Append one already-known range.
  Range(TextRange),
  /// Finalize a composite node's placeholder range after its children have
  /// been collected.
  Finish {
    /// Placeholder range index.
    index:          usize,
    /// First child range following the placeholder.
    children_start: usize,
    /// Composite source anchor.
    syntax:         SyntaxElement,
  },
}

/// Precompiled query segment used during one path traversal.
enum CompiledQuerySegment {
  /// A key or index rendered as text and matched against a glob.
  Key(globset::GlobMatcher),
  /// An exact array index.
  Index(usize),
}

impl CompiledQuerySegment {
  /// Return whether this compiled segment accepts one candidate path segment.
  fn matches(&self, candidate: &KeyOrIndex) -> bool {
    match *self {
      Self::Key(ref glob) => match *candidate {
        KeyOrIndex::Key(ref key) => glob.is_match(key.value()),
        KeyOrIndex::Index(index) => glob.is_match(index.to_string()),
      },
      Self::Index(expected) => match *candidate {
        KeyOrIndex::Index(actual) => expected == actual,
        KeyOrIndex::Key(_) => false,
      },
    }
  }
}

impl Node {
  /// Return the immutable syntax anchor retained from the source document.
  #[must_use]
  pub fn syntax(&self) -> Option<&SyntaxElement> {
    match *self {
      Self::Table(ref node) => node.syntax(),
      Self::Array(ref node) => node.syntax(),
      Self::Bool(ref node) => node.syntax(),
      Self::Str(ref node) => node.syntax(),
      Self::Integer(ref node) => node.syntax(),
      Self::Float(ref node) => node.syntax(),
      Self::Date(ref node) => node.syntax(),
      Self::Invalid(ref node) => node.syntax(),
    }
  }

  /// Return semantic diagnostics attached directly to this node.
  #[must_use]
  pub fn errors(&self) -> &[Diagnostic] {
    match *self {
      Self::Table(ref node) => node.errors(),
      Self::Array(ref node) => node.errors(),
      Self::Bool(ref node) => node.errors(),
      Self::Str(ref node) => node.errors(),
      Self::Integer(ref node) => node.errors(),
      Self::Float(ref node) => node.errors(),
      Self::Date(ref node) => node.errors(),
      Self::Invalid(ref node) => node.errors(),
    }
  }

  /// Return whether this node has no directly attached semantic diagnostics.
  #[must_use]
  pub fn is_valid_node(&self) -> bool {
    self.errors().is_empty()
  }

  /// Borrow the immutable document comment index shared by this node.
  fn comment_store(&self) -> &CommentStore {
    match *self {
      Self::Table(ref node) => &node.arena.comments,
      Self::Array(ref node) => &node.arena.comments,
      Self::Bool(ref node) => &node.arena.comments,
      Self::Str(ref node) => &node.arena.comments,
      Self::Integer(ref node) => &node.arena.comments,
      Self::Float(ref node) => &node.arena.comments,
      Self::Date(ref node) => &node.arena.comments,
      Self::Invalid(ref node) => &node.arena.comments,
    }
  }

  /// Follow an explicit key/index path from this node.
  #[must_use]
  pub fn path(&self, keys: &Keys) -> Option<Self> {
    let mut node = self.clone();
    for key in keys.iter() {
      node = node.get(key)?;
    }

    Some(node)
  }

  /// Look up one explicit key or array index.
  #[must_use]
  pub fn get(&self, index: &KeyOrIndex) -> Option<Self> {
    match *index {
      KeyOrIndex::Key(ref key) => self.get_key(key.value()),
      KeyOrIndex::Index(array_index) => self.get_index(array_index),
    }
  }

  /// Look up one table key.
  #[must_use]
  pub fn get_key(&self, key: &str) -> Option<Self> {
    match *self {
      Self::Table(ref table) => table.get(key),
      Self::Array(_) | Self::Bool(_) | Self::Str(_) | Self::Integer(_) | Self::Float(_) | Self::Date(_) | Self::Invalid(_) => None,
    }
  }

  /// Look up one array index.
  #[must_use]
  pub fn get_index(&self, index: usize) -> Option<Self> {
    match *self {
      Self::Array(ref array) => array.items().get(index),
      Self::Table(_) | Self::Bool(_) | Self::Str(_) | Self::Integer(_) | Self::Float(_) | Self::Date(_) | Self::Invalid(_) => None,
    }
  }

  /// Return direct table keys or array indices matching one glob pattern.
  ///
  /// # Errors
  ///
  /// Returns [`QueryError::InvalidGlob`] when `pattern` is not a valid glob.
  pub fn get_matches(&self, pattern: &str) -> Result<impl ExactSizeIterator<Item = (KeyOrIndex, Self)>, QueryError> {
    let glob = globset::Glob::new(pattern).map_err(QueryError::from)?.compile_matcher();
    let mut matched = Vec::new();

    match *self {
      Self::Table(ref table) => {
        matched.extend(
          table
            .entries()
            .iter()
            .filter(|entry| glob.is_match(entry.0.value()))
            .map(|(key, node)| (KeyOrIndex::from(key), node)),
        );
      }
      Self::Array(ref array) => {
        matched.extend(
          array
            .items()
            .iter()
            .enumerate()
            .filter(|entry| glob.is_match(entry.0.to_string()))
            .map(|(index, node)| (KeyOrIndex::from(index), node)),
        );
      }
      Self::Bool(_) | Self::Str(_) | Self::Integer(_) | Self::Float(_) | Self::Date(_) | Self::Invalid(_) => {}
    }

    Ok(matched.into_iter())
  }

  /// Validate the node and then all children recursively.
  ///
  /// # Errors
  ///
  /// Returns every recoverable semantic diagnostic attached to this node,
  /// its keys, or its descendants.
  pub fn validate(&self) -> Result<(), Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();
    let mut pending = Vec::from([self.clone()]);
    while let Some(node) = pending.pop() {
      diagnostics.extend_from_slice(node.errors());
      match node {
        Self::Table(table) => {
          let entries = table.entries();
          diagnostics.extend(entries.iter().flat_map(|(key, _)| key.errors().to_vec()));
          pending.extend(entries.iter().rev().map(|(_, child)| child));
        }
        Self::Array(array) => pending.extend(array.items().iter().rev()),
        Self::Bool(_) | Self::Str(_) | Self::Integer(_) | Self::Float(_) | Self::Date(_) | Self::Invalid(_) => {}
      }
    }
    if diagnostics.is_empty() {
      Ok(())
    } else {
      Err(diagnostics)
    }
  }

  /// Iterate over every descendant paired with its semantic path.
  #[must_use]
  pub fn flat_iter(&self) -> impl DoubleEndedIterator<Item = (Keys, Self)> {
    let mut all = Vec::new();
    let mut pending = Vec::new();
    self.push_flat_children(&Keys::empty(), &mut pending);
    while let Some((path, node)) = pending.pop() {
      all.push((path.clone(), node.clone()));
      node.push_flat_children(&path, &mut pending);
    }

    all.into_iter()
  }

  /// Return descendants whose semantic paths match `keys`.
  ///
  /// Glob expressions are supported in key segments. When
  /// `include_children` is `false`, only exact-depth matches are returned;
  /// otherwise descendants below each matched prefix are returned as well.
  ///
  /// # Errors
  ///
  /// Returns [`QueryError::InvalidGlob`] when any key segment contains an
  /// invalid glob expression.
  pub fn find_all_matches(&self, keys: &Keys, include_children: bool) -> Result<impl ExactSizeIterator<Item = (Keys, Self)>, QueryError> {
    let query = keys
      .iter()
      .map(|segment| match *segment {
        KeyOrIndex::Key(ref key) => globset::Glob::new(key.value())
          .map(|glob| CompiledQuerySegment::Key(glob.compile_matcher()))
          .map_err(QueryError::from),
        KeyOrIndex::Index(index) => Ok(CompiledQuerySegment::Index(index)),
      })
      .collect::<Result<Vec<_>, _>>()?;
    let mut all = self.flat_iter().collect::<Vec<_>>();

    all.retain(|candidate_entry| {
      let candidate = &candidate_entry.0;
      candidate.len() >= query.len()
        && query
          .iter()
          .zip(candidate.iter())
          .all(|(expected, actual)| expected.matches(actual))
    });

    if !include_children {
      all.retain(|candidate_entry| candidate_entry.0.len() == query.len());
    }

    Ok(all.into_iter())
  }

  /// Return source ranges owned by this node.
  ///
  /// Composite nodes precede their descendants when `include_children` is
  /// `true`. Detached nodes without source anchors contribute no range.
  ///
  /// `+ use<>` makes the iterator explicitly owned under Rust 2024 capture
  /// rules so callers can invoke this method on temporary nodes.
  #[must_use]
  pub fn text_ranges(&self, include_children: bool) -> impl ExactSizeIterator<Item = TextRange> + use<> {
    let mut ranges = Vec::with_capacity(1);
    let mut pending = Vec::from([RangeTask::Node {
      node: self.clone(),
      include_children,
    }]);
    while let Some(task) = pending.pop() {
      match task {
        RangeTask::Range(range) => ranges.push(range),
        RangeTask::Finish {
          index,
          children_start,
          syntax,
        } => {
          let covered = ranges.get(children_start..).map_or_else(
            || syntax.text_range(),
            |children| children.iter().fold(syntax.text_range(), |range, child| range.cover(*child)),
          );
          ranges.get_mut(index).into_iter().for_each(|placeholder| *placeholder = covered);
        }
        RangeTask::Node {
          node,
          include_children: task_include_children,
        } => match node {
          Self::Table(table) => queue_table_ranges(&table, task_include_children, &mut ranges, &mut pending),
          Self::Array(array) => queue_array_ranges(&array, task_include_children, &mut ranges, &mut pending),
          Self::Bool(boolean) => ranges.extend(boolean.syntax().map(SyntaxElement::text_range)),
          Self::Str(string) => ranges.extend(string.syntax().map(SyntaxElement::text_range)),
          Self::Integer(integer) => ranges.extend(integer.syntax().map(SyntaxElement::text_range)),
          Self::Float(float) => ranges.extend(float.syntax().map(SyntaxElement::text_range)),
          Self::Date(date_time) => ranges.extend(date_time.syntax().map(SyntaxElement::text_range)),
          Self::Invalid(invalid) => ranges.extend(invalid.syntax().map(SyntaxElement::text_range)),
        },
      }
    }

    ranges.into_iter()
  }

  /// Iterate over all comments in the containing document.
  #[must_use]
  pub fn comments(&self) -> impl ExactSizeIterator<Item = Comment> + '_ {
    self.comment_store().all().iter().cloned()
  }

  /// Comments before the first item in the file.
  ///
  /// These are computed once from the document root and the same values are
  /// returned from every node in that document.
  #[must_use]
  pub fn header_comments(&self) -> impl ExactSizeIterator<Item = Comment> + '_ {
    self.comment_store().header().iter().cloned()
  }

  /// Push direct children in reverse source order for stack-based traversal.
  fn push_flat_children(&self, parent: &Keys, pending: &mut Vec<(Keys, Self)>) {
    match *self {
      Self::Table(ref table) => {
        pending.extend(table.entries().iter().rev().map(|(key, entry)| (parent.join(key), entry)));
      }
      Self::Array(ref array) => {
        pending.extend(
          array
            .items()
            .iter()
            .enumerate()
            .rev()
            .map(|(index, child)| (parent.join(index), child)),
        );
      }
      Self::Bool(_) | Self::Str(_) | Self::Integer(_) | Self::Float(_) | Self::Date(_) | Self::Invalid(_) => {}
    }
  }

  /// Returns `true` if the node is [`Table`].
  ///
  /// [`Table`]: Node::Table
  #[allow(
    clippy::single_call_fn,
    reason = "the public predicate completes Node's symmetric variant-query API and serves directly as an iterator predicate"
  )]
  #[must_use]
  pub const fn is_table(&self) -> bool {
    matches!(self, Self::Table(..))
  }

  /// Returns `true` if the node is [`Array`].
  ///
  /// [`Array`]: Node::Array
  #[must_use]
  pub const fn is_array(&self) -> bool {
    matches!(self, Self::Array(..))
  }

  /// Returns `true` if the node is [`Bool`].
  ///
  /// [`Bool`]: Node::Bool
  #[must_use]
  pub const fn is_bool(&self) -> bool {
    matches!(self, Self::Bool(..))
  }

  /// Returns `true` if the node is [`Str`].
  ///
  /// [`Str`]: Node::Str
  #[must_use]
  pub const fn is_str(&self) -> bool {
    matches!(self, Self::Str(..))
  }

  /// Returns `true` if the node is [`Integer`].
  ///
  /// [`Integer`]: Node::Integer
  #[must_use]
  pub const fn is_integer(&self) -> bool {
    matches!(self, Self::Integer(..))
  }

  /// Returns `true` if the node is [`Float`].
  ///
  /// [`Float`]: Node::Float
  #[must_use]
  pub const fn is_float(&self) -> bool {
    matches!(self, Self::Float(..))
  }

  /// Returns `true` if the node is [`Date`].
  ///
  /// [`Date`]: Node::Date
  #[must_use]
  pub const fn is_date(&self) -> bool {
    matches!(self, Self::Date(..))
  }

  /// Returns `true` if the node is [`Invalid`].
  ///
  /// [`Invalid`]: Node::Invalid
  #[must_use]
  pub const fn is_invalid(&self) -> bool {
    matches!(self, Self::Invalid(..))
  }

  /// Borrow this node as a table.
  #[must_use]
  pub const fn as_table(&self) -> Option<&Table> {
    if let Self::Table(ref table) = *self {
      Some(table)
    } else {
      None
    }
  }

  /// Borrow this node as an array.
  #[must_use]
  pub const fn as_array(&self) -> Option<&Array> {
    if let Self::Array(ref array) = *self {
      Some(array)
    } else {
      None
    }
  }

  /// Borrow this node as a Boolean.
  #[must_use]
  pub const fn as_bool(&self) -> Option<&Bool> {
    if let Self::Bool(ref boolean) = *self {
      Some(boolean)
    } else {
      None
    }
  }

  /// Borrow this node as a string.
  #[must_use]
  pub const fn as_str(&self) -> Option<&Str> {
    if let Self::Str(ref string) = *self {
      Some(string)
    } else {
      None
    }
  }

  /// Borrow this node as an integer.
  #[must_use]
  pub const fn as_integer(&self) -> Option<&Integer> {
    if let Self::Integer(ref integer) = *self {
      Some(integer)
    } else {
      None
    }
  }

  /// Borrow this node as a floating-point value.
  #[must_use]
  pub const fn as_float(&self) -> Option<&Float> {
    if let Self::Float(ref float) = *self {
      Some(float)
    } else {
      None
    }
  }

  /// Borrow this node as a date or time.
  #[must_use]
  pub const fn as_date(&self) -> Option<&DateTime> {
    if let Self::Date(ref date_time) = *self {
      Some(date_time)
    } else {
      None
    }
  }

  /// Borrow this node as malformed source.
  #[must_use]
  pub const fn as_invalid(&self) -> Option<&Invalid> {
    if let Self::Invalid(ref invalid) = *self {
      Some(invalid)
    } else {
      None
    }
  }

  /// Convert this node into a table.
  ///
  /// # Errors
  ///
  /// Returns the original node when it is not a table.
  pub fn try_into_table(self) -> Result<Table, Self> {
    if let Self::Table(table) = self {
      Ok(table)
    } else {
      Err(self)
    }
  }

  /// Convert this node into an array.
  ///
  /// # Errors
  ///
  /// Returns the original node when it is not an array.
  pub fn try_into_array(self) -> Result<Array, Self> {
    if let Self::Array(array) = self {
      Ok(array)
    } else {
      Err(self)
    }
  }

  /// Convert this node into a Boolean.
  ///
  /// # Errors
  ///
  /// Returns the original node when it is not a Boolean.
  pub fn try_into_bool(self) -> Result<Bool, Self> {
    if let Self::Bool(boolean) = self {
      Ok(boolean)
    } else {
      Err(self)
    }
  }

  /// Convert this node into a string.
  ///
  /// # Errors
  ///
  /// Returns the original node when it is not a string.
  pub fn try_into_str(self) -> Result<Str, Self> {
    if let Self::Str(string) = self {
      Ok(string)
    } else {
      Err(self)
    }
  }

  /// Convert this node into an integer.
  ///
  /// # Errors
  ///
  /// Returns the original node when it is not an integer.
  pub fn try_into_integer(self) -> Result<Integer, Self> {
    if let Self::Integer(integer) = self {
      Ok(integer)
    } else {
      Err(self)
    }
  }

  /// Convert this node into a floating-point value.
  ///
  /// # Errors
  ///
  /// Returns the original node when it is not a floating-point value.
  pub fn try_into_float(self) -> Result<Float, Self> {
    if let Self::Float(float) = self {
      Ok(float)
    } else {
      Err(self)
    }
  }

  /// Convert this node into a date or time.
  ///
  /// # Errors
  ///
  /// Returns the original node when it is not a date or time.
  pub fn try_into_date(self) -> Result<DateTime, Self> {
    if let Self::Date(date_time) = self {
      Ok(date_time)
    } else {
      Err(self)
    }
  }

  /// Convert this node into malformed source.
  ///
  /// # Errors
  ///
  /// Returns the original node when it is not malformed source.
  pub fn try_into_invalid(self) -> Result<Invalid, Self> {
    if let Self::Invalid(invalid) = self {
      Ok(invalid)
    } else {
      Err(self)
    }
  }

  /// Render this semantic value as TOML.
  ///
  /// # Errors
  ///
  /// Returns [`super::RenderError::InvalidNode`] for malformed source and
  /// [`super::RenderError::NegativeNonDecimal`] for an unsupported detached
  /// integer representation. Returns
  /// [`super::RenderError::SemanticDiagnostics`] when the complete subtree
  /// contains conflicting or otherwise ambiguous semantic state.
  pub fn to_toml(&self, inline: bool, prefer_single_quote: bool) -> Result<String, RenderError> {
    let mut output = String::new();
    super::to_toml::render(self, &mut output, inline, prefer_single_quote)?;
    Ok(output)
  }

  /// Write this semantic value as TOML.
  ///
  /// # Errors
  ///
  /// Returns a typed rendering error when the DOM contains malformed or
  /// unsupported semantic state, or when the destination rejects a write.
  pub fn to_toml_fmt(&self, formatter: &mut impl Write, inline: bool, prefer_single_quote: bool) -> Result<(), RenderError> {
    super::to_toml::render(self, formatter, inline, prefer_single_quote)
  }
}

/// Schedule one composite anchor before its semantic descendants.
fn queue_container_range(syntax: Option<&SyntaxElement>, ranges: &mut Vec<TextRange>, pending: &mut Vec<RangeTask>) {
  if let Some(source_syntax) = syntax.cloned() {
    let index = ranges.len();
    ranges.push(source_syntax.text_range());
    pending.push(RangeTask::Finish {
      index,
      children_start: ranges.len(),
      syntax: source_syntax,
    });
  }
}

/// Schedule one table anchor and, when requested, its key and entry descendants.
#[allow(
  clippy::single_call_fn,
  reason = "the named scheduler keeps table key-range and entry ordering out of the iterative range-collection loop"
)]
fn queue_table_ranges(table: &Table, include_children: bool, ranges: &mut Vec<TextRange>, pending: &mut Vec<RangeTask>) {
  queue_container_range(table.syntax(), ranges, pending);
  if !include_children {
    return;
  }

  let mut children = Vec::new();
  for (key, entry) in &table.entries() {
    children.extend(key.text_ranges().map(RangeTask::Range));
    children.push(RangeTask::Node {
      node:             entry,
      include_children: true,
    });
  }
  pending.extend(children.into_iter().rev());
}

/// Schedule one array anchor and, when requested, its semantic element descendants.
#[allow(
  clippy::single_call_fn,
  reason = "the named scheduler keeps array element ordering out of the iterative range-collection loop"
)]
fn queue_array_ranges(array: &Array, include_children: bool, ranges: &mut Vec<TextRange>, pending: &mut Vec<RangeTask>) {
  queue_container_range(array.syntax(), ranges, pending);
  if include_children {
    pending.extend(array.items().iter().rev().map(|node| RangeTask::Node {
      node,
      include_children: true,
    }));
  }
}

impl From<DateTime> for Node {
  fn from(date_time: DateTime) -> Self {
    Self::Date(date_time)
  }
}

impl From<Float> for Node {
  fn from(float: Float) -> Self {
    Self::Float(float)
  }
}

impl From<Integer> for Node {
  fn from(integer: Integer) -> Self {
    Self::Integer(integer)
  }
}

impl From<Str> for Node {
  fn from(string: Str) -> Self {
    Self::Str(string)
  }
}

impl From<Bool> for Node {
  fn from(boolean: Bool) -> Self {
    Self::Bool(boolean)
  }
}

impl From<Array> for Node {
  fn from(array: Array) -> Self {
    Self::Array(array)
  }
}

impl From<Table> for Node {
  fn from(table: Table) -> Self {
    Self::Table(table)
  }
}

impl From<Invalid> for Node {
  fn from(invalid: Invalid) -> Self {
    Self::Invalid(invalid)
  }
}

#[cfg(test)]
/// Unified node-variant API contracts.
mod tests {
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_some;

  use super::Node;
  use crate::dom::Key;
  use crate::dom::KeyOrIndex;
  use crate::dom::Keys;
  use crate::test_support::parse_dom;

  /// Return the public variant-predicate results in declaration order.
  fn variant_flags(node: &Node) -> [bool; 8] {
    [
      node.is_table(),
      node.is_array(),
      node.is_bool(),
      node.is_str(),
      node.is_integer(),
      node.is_float(),
      node.is_date(),
      node.is_invalid(),
    ]
  }

  /// Return the public borrowed-projection results in declaration order.
  #[allow(
    clippy::single_call_fn,
    reason = "the paired fixture names the borrowed-projection order compared against the variant-predicate order"
  )]
  fn projection_flags(node: &Node) -> [bool; 8] {
    [
      node.as_table().is_some(),
      node.as_array().is_some(),
      node.as_bool().is_some(),
      node.as_str().is_some(),
      node.as_integer().is_some(),
      node.as_float().is_some(),
      node.as_date().is_some(),
      node.as_invalid().is_some(),
    ]
  }

  /// Extract one named root entry from a parsed document.
  fn entry(root: &Node, name: &str) -> Result<Node, TestFailure> {
    ensure_some(root.get_key(name), "the named node-variant fixture must exist")
  }

  /// Parse one source-backed instance of every public node variant.
  fn variant_fixture() -> Result<[Node; 8], TestFailure> {
    let root = parse_dom(
      "array = [true]\nboolean = true\nstring = \"text\"\ninteger = 7\nfloat = 1.5\ndate = 1979-05-27\ninvalid = \
       999999999999999999999999999999\n",
      "the node-variant fixture must parse",
    )?;
    Ok([
      root.clone(),
      entry(&root, "array")?,
      entry(&root, "boolean")?,
      entry(&root, "string")?,
      entry(&root, "integer")?,
      entry(&root, "float")?,
      entry(&root, "date")?,
      entry(&root, "invalid")?,
    ])
  }

  /// Exercise one positive and negative owned node conversion.
  macro_rules! conversion_contract {
    ($positive:expr, $negative:expr, $method:ident, $expected:expr, $positive_context:literal, $negative_context:literal) => {{
      let Ok(converted) = $positive.clone().$method() else {
        return ensure(false, $positive_context);
      };
      let reconstructed = Node::from(converted);
      ensure(variant_flags(&reconstructed) == $expected, $positive_context)?;
      let rejected = $negative.clone();
      let rejected_flags = variant_flags(&rejected);
      let Err(original) = rejected.$method() else {
        return ensure(false, $negative_context);
      };
      ensure(variant_flags(&original) == rejected_flags, $negative_context)?;
    }};
  }

  #[test]
  fn variant_predicates_and_borrowed_projections_are_symmetric() -> Result<(), TestFailure> {
    let [table, array, boolean, string, integer, float, date, invalid] = variant_fixture()?;
    let variants = [
      (&table, [true, false, false, false, false, false, false, false]),
      (&array, [false, true, false, false, false, false, false, false]),
      (&boolean, [false, false, true, false, false, false, false, false]),
      (&string, [false, false, false, true, false, false, false, false]),
      (&integer, [false, false, false, false, true, false, false, false]),
      (&float, [false, false, false, false, false, true, false, false]),
      (&date, [false, false, false, false, false, false, true, false]),
      (&invalid, [false, false, false, false, false, false, false, true]),
    ];
    for (node, expected) in variants {
      ensure(
        (variant_flags(node), projection_flags(node)) == (expected, expected),
        "every node predicate and borrowed projection must select exactly its declared variant",
      )?;
      ensure(
        node.syntax().is_some(),
        "every source-backed node variant must retain its immutable syntax anchor",
      )?;
    }
    ensure(
      [
        table.is_valid_node(),
        array.is_valid_node(),
        boolean.is_valid_node(),
        string.is_valid_node(),
        integer.is_valid_node(),
        float.is_valid_node(),
        date.is_valid_node(),
        invalid.is_valid_node(),
        invalid.errors().len() == 1,
      ] == [true, true, true, true, true, true, true, false, true],
      "valid scalar and container variants must stay diagnostic-free while malformed source retains its direct diagnostic",
    )
  }

  #[test]
  fn owned_variant_conversions_reconstruct_or_preserve_the_original() -> Result<(), TestFailure> {
    let [table, array, boolean, string, integer, float, date, invalid] = variant_fixture()?;
    conversion_contract!(
      table,
      array,
      try_into_table,
      [true, false, false, false, false, false, false, false],
      "owned table conversion must reconstruct the table variant",
      "owned table conversion must reject and preserve an array"
    );
    conversion_contract!(
      array,
      table,
      try_into_array,
      [false, true, false, false, false, false, false, false],
      "owned array conversion must reconstruct the array variant",
      "owned array conversion must reject and preserve a table"
    );
    conversion_contract!(
      boolean,
      table,
      try_into_bool,
      [false, false, true, false, false, false, false, false],
      "owned Boolean conversion must reconstruct the Boolean variant",
      "owned Boolean conversion must reject and preserve a table"
    );
    conversion_contract!(
      string,
      table,
      try_into_str,
      [false, false, false, true, false, false, false, false],
      "owned string conversion must reconstruct the string variant",
      "owned string conversion must reject and preserve a table"
    );
    conversion_contract!(
      integer,
      table,
      try_into_integer,
      [false, false, false, false, true, false, false, false],
      "owned integer conversion must reconstruct the integer variant",
      "owned integer conversion must reject and preserve a table"
    );
    conversion_contract!(
      float,
      table,
      try_into_float,
      [false, false, false, false, false, true, false, false],
      "owned float conversion must reconstruct the float variant",
      "owned float conversion must reject and preserve a table"
    );
    conversion_contract!(
      date,
      table,
      try_into_date,
      [false, false, false, false, false, false, true, false],
      "owned date conversion must reconstruct the date variant",
      "owned date conversion must reject and preserve a table"
    );
    conversion_contract!(
      invalid,
      table,
      try_into_invalid,
      [false, false, false, false, false, false, false, true],
      "owned invalid conversion must reconstruct the invalid variant",
      "owned invalid conversion must reject and preserve a table"
    );
    Ok(())
  }

  #[test]
  fn typed_lookup_routes_table_keys_and_array_indices_without_scalar_fallbacks() -> Result<(), TestFailure> {
    let root = parse_dom(
      "items = [{ name = \"first\" }, { name = \"second\" }]\n",
      "the typed node-lookup fixture must parse",
    )?;
    let items = entry(&root, "items")?;
    let second_path = Keys::new(
      [
        KeyOrIndex::from(Key::new("items")),
        KeyOrIndex::from(1_usize),
        KeyOrIndex::from(Key::new("name")),
      ]
      .into_iter(),
    );
    ensure(
      root
        .path(&second_path)
        .and_then(|node| node.as_str().map(|string| string.value().to_owned()))
        .as_deref()
        == Some("second"),
      "typed path lookup must route key and index segments through nested containers",
    )?;
    ensure(
      [
        items.get(&KeyOrIndex::from(0_usize)).is_some(),
        items.get_index(9).is_none(),
        items.get_key("0").is_none(),
        root.get_index(0).is_none(),
        items.get_key("missing").is_none(),
        items.get_index(0).and_then(|node| node.get_key("name")).is_some(),
      ] == [true, true, true, true, true, true],
      "container lookup must preserve key/index kind, bounds, and absence without scalar coercion",
    )?;
    let scalar = ensure_some(
      root.path(&Keys::new(
        [
          KeyOrIndex::from(Key::new("items")),
          KeyOrIndex::from(0_usize),
          KeyOrIndex::from(Key::new("name")),
        ]
        .into_iter(),
      )),
      "the scalar node-lookup fixture must exist",
    )?;
    ensure(
      [scalar.get_key("child").is_none(), scalar.get_index(0).is_none()] == [true, true],
      "scalar nodes must not fabricate table or array children",
    )
  }
}
