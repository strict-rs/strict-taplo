//! Concrete immutable DOM wrappers and their decoded value representations.
//!
//! Each public wrapper owns an `Arc` to fully frozen storage. Source-backed values retain their
//! Rowan anchor, while detached values retain only semantic data and explicit rendering policy.

use std::collections::HashSet;
use std::collections::VecDeque;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fmt::Write as _;
use std::hash::Hash;
use std::hash::Hasher;
use std::iter::once;
use std::sync::Arc;
use std::sync::OnceLock;

use rowan::GreenNode;
use rowan::TextRange;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;

use super::Node;
use crate::HashMap;
use crate::dom::ArrayItems;
use crate::dom::CommentStore;
use crate::dom::Entries;
use crate::dom::KeyOrIndex;
use crate::dom::Keys;
use crate::dom::error::Diagnostic;
use crate::syntax::SyntaxElement;
use crate::util::escape;

/// Validated identity and immutable record of one arena node.
#[derive(Clone)]
pub(in crate::dom) struct NodeId {
  /// Stable construction identity.
  index:  usize,
  /// Write-once immutable record retained independently by the arena.
  record: Arc<OnceLock<NodeSeed>>,
}

impl fmt::Debug for NodeId {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("NodeId")
      .field("index", &self.index)
      .field("kind", &self.record.get().map(NodeSeed::kind_name))
      .finish()
  }
}

/// One table entry linking a decoded key to an immutable child record.
#[derive(Clone, Debug)]
pub(in crate::dom) struct ArenaEntry {
  /// Decoded source key.
  pub(in crate::dom) key:  Key,
  /// Validated child identity.
  pub(in crate::dom) node: NodeId,
}

/// Immutable table entry records and their latest-winner index.
#[derive(Debug, Default)]
pub(in crate::dom) struct ArenaEntries {
  /// Latest source-order entry index for each valid key.
  pub(in crate::dom) lookup: HashMap<Key, usize>,
  /// Every entry in source order, including conflict history.
  pub(in crate::dom) all:    Arc<[ArenaEntry]>,
}

/// Immutable arena record used to materialize a public node handle.
#[derive(Debug)]
pub(in crate::dom) enum NodeSeed {
  /// Table record and its ordered child identities.
  Table {
    /// Table metadata.
    inner:   Arc<TableInner>,
    /// Ordered table edges.
    entries: Arc<ArenaEntries>,
  },
  /// Array record and its ordered child identities.
  Array {
    /// Array metadata.
    inner: Arc<ArrayInner>,
    /// Ordered array edges.
    items: Arc<[NodeId]>,
  },
  /// Boolean record.
  Bool {
    /// Boolean metadata and value.
    inner: Arc<BoolInner>,
  },
  /// String record.
  Str {
    /// String metadata and value.
    inner: Arc<StrInner>,
  },
  /// Integer record.
  Integer {
    /// Integer metadata and value.
    inner: Arc<IntegerInner>,
  },
  /// Floating-point record.
  Float {
    /// Floating-point metadata and value.
    inner: Arc<FloatInner>,
  },
  /// Date or time record.
  Date {
    /// Date or time metadata and value.
    inner: Arc<DateTimeInner>,
  },
  /// Malformed record.
  Invalid {
    /// Malformed-node metadata.
    inner: Arc<InvalidInner>,
  },
}

impl NodeSeed {
  /// Return a stable diagnostic name for this record kind.
  const fn kind_name(&self) -> &'static str {
    match *self {
      Self::Table {
        ..
      } => "table",
      Self::Array {
        ..
      } => "array",
      Self::Bool {
        ..
      } => "boolean",
      Self::Str {
        ..
      } => "string",
      Self::Integer {
        ..
      } => "integer",
      Self::Float {
        ..
      } => "float",
      Self::Date {
        ..
      } => "date",
      Self::Invalid {
        ..
      } => "invalid",
    }
  }
}

impl NodeId {
  /// Allocate one stable identity before its immutable record is published.
  pub(in crate::dom) fn pending(index: usize) -> Self {
    Self {
      index,
      record: Arc::new(OnceLock::new()),
    }
  }

  /// Publish this identity's immutable record exactly once.
  pub(in crate::dom) fn initialize(&self, record: NodeSeed) {
    let _record = self.record.get_or_init(move || record);
  }

  /// Construct one validated identity from its immutable record.
  #[cfg(test)]
  pub(in crate::dom) fn new(index: usize, record: NodeSeed) -> Self {
    let id = Self::pending(index);
    id.initialize(record);
    id
  }

  /// Borrow the initialized immutable record.
  fn record(&self) -> &NodeSeed {
    self.record.wait()
  }

  /// Materialize one cheap public handle against the shared arena.
  pub(in crate::dom) fn node(&self, arena: Arc<DomArena>) -> Node {
    let id = self.clone();
    match *self.record() {
      NodeSeed::Table {
        ref inner,
        ref entries,
      } => Node::Table(Table {
        inner: Arc::clone(inner),
        entries: Arc::clone(entries),
        id,
        arena,
      }),
      NodeSeed::Array {
        ref inner,
        ref items,
      } => Node::Array(Array {
        inner: Arc::clone(inner),
        items: Arc::clone(items),
        id,
        arena,
      }),
      NodeSeed::Bool {
        ref inner,
      } => Node::Bool(Bool {
        inner: Arc::clone(inner),
        id,
        arena,
      }),
      NodeSeed::Str {
        ref inner,
      } => Node::Str(Str {
        inner: Arc::clone(inner),
        id,
        arena,
      }),
      NodeSeed::Integer {
        ref inner,
      } => Node::Integer(Integer {
        inner: Arc::clone(inner),
        id,
        arena,
      }),
      NodeSeed::Float {
        ref inner,
      } => Node::Float(Float {
        inner: Arc::clone(inner),
        id,
        arena,
      }),
      NodeSeed::Date {
        ref inner,
      } => Node::Date(DateTime {
        inner: Arc::clone(inner),
        id,
        arena,
      }),
      NodeSeed::Invalid {
        ref inner,
      } => Node::Invalid(Invalid {
        inner: Arc::clone(inner),
        id,
        arena,
      }),
    }
  }

  /// Return direct child identities without following them recursively.
  fn children(&self) -> Vec<Self> {
    match *self.record() {
      NodeSeed::Table {
        ref entries, ..
      } => entries.all.iter().map(|entry| entry.node.clone()).collect(),
      NodeSeed::Array {
        ref items, ..
      } => items.iter().cloned().collect(),
      NodeSeed::Bool {
        ..
      }
      | NodeSeed::Str {
        ..
      }
      | NodeSeed::Integer {
        ..
      }
      | NodeSeed::Float {
        ..
      }
      | NodeSeed::Date {
        ..
      }
      | NodeSeed::Invalid {
        ..
      } => Vec::new(),
    }
  }
}

/// Immutable document arena retaining every record for iterative destruction.
pub(in crate::dom) struct DomArena {
  /// Document-wide comment index stored exactly once.
  pub(in crate::dom) comments: Arc<CommentStore>,
  /// Validated records in parent-before-child destruction order.
  records:                     VecDeque<NodeId>,
  /// Shallow green-node guards ordered from syntax parents to syntax children.
  syntax_guards:               VecDeque<GreenNode>,
}

impl fmt::Debug for DomArena {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("DomArena")
      .field("records", &self.records.len())
      .field("syntax_guards", &self.syntax_guards.len())
      .finish_non_exhaustive()
  }
}

impl Drop for DomArena {
  fn drop(&mut self) {
    while self.records.pop_front().is_some() {}
    self.comments = Arc::default();
    while self.syntax_guards.pop_front().is_some() {}
  }
}

impl DomArena {
  /// Publish one root record and retain every reachable record independently.
  pub(in crate::dom) fn publish(root: &NodeId, comments: Arc<CommentStore>, syntax_guards: VecDeque<GreenNode>) -> Node {
    let mut records = VecDeque::new();
    let mut pending = Vec::from([root.clone()]);
    let mut visited = HashSet::new();
    while let Some(record) = pending.pop() {
      if !visited.insert(record.index) {
        continue;
      }
      pending.extend(record.children().into_iter().rev());
      records.push_back(record);
    }
    let arena = Arc::new(Self {
      comments,
      records,
      syntax_guards,
    });
    root.node(arena)
  }

  /// Construct an empty detached arena for a view without child records.
  pub(in crate::dom) fn empty() -> Arc<Self> {
    Arc::new(Self {
      comments:      Arc::default(),
      records:       VecDeque::new(),
      syntax_guards: VecDeque::new(),
    })
  }

  /// Construct a detached arena retaining one scalar or malformed record.
  #[cfg(test)]
  fn detached(root: NodeId) -> Arc<Self> {
    Arc::new(Self {
      comments:      Arc::default(),
      records:       VecDeque::from([root]),
      syntax_guards: VecDeque::new(),
    })
  }
}

/// Implement immutable ownership plus common syntax and diagnostic accessors.
macro_rules! wrap_node {
    (
    $(#[$attrs:meta])*
    $vis:vis struct $name:ident {
        inner: $inner:ident
    }
    ) => {
        $(#[$attrs])*
        $vis struct $name {
            /// Shared immutable storage for this decoded value.
            pub(in crate::dom) inner: Arc<$inner>,
        }

        impl $name {
            /// Return the immutable syntax anchor retained from the source document.
            #[must_use]
            pub fn syntax(&self) -> Option<&$crate::syntax::SyntaxElement> {
                self.inner.syntax.as_ref()
            }

            /// Return semantic diagnostics attached directly to this value.
            #[must_use]
            pub fn errors(&self) -> &[$crate::dom::error::Diagnostic] {
                &self.inner.diagnostics
            }

            /// Return whether this value has no directly attached semantic diagnostics.
            #[must_use]
            pub fn is_valid_node(&self) -> bool {
                self.inner.diagnostics.is_empty()
            }
        }

        impl $inner {
            /// Freeze this completed value behind shared immutable ownership.
            pub(in crate::dom) fn wrap(self) -> $name {
                self.into()
            }
        }

        impl From<$inner> for $name {
            fn from(inner: $inner) -> $name {
                $name {
                    inner: Arc::new(inner)
                }
            }
        }
    };
    (
    $(#[$attrs:meta])*
    $vis:vis struct $name:ident, $variant:ident {
        inner: $inner:ident
    }
    ) => {
        $(#[$attrs])*
        $vis struct $name {
            /// Shared immutable storage for this concrete semantic node.
            pub(in crate::dom) inner: Arc<$inner>,
            /// Stable arena identity.
            pub(in crate::dom) id: NodeId,
            /// Shared immutable document arena.
            pub(in crate::dom) arena: Arc<DomArena>,
        }

        impl $name {
            /// Return the immutable syntax anchor retained from the source document.
            #[must_use]
            pub fn syntax(&self) -> Option<&$crate::syntax::SyntaxElement> {
                self.inner.syntax.as_ref()
            }

            /// Return semantic diagnostics attached directly to this node.
            #[must_use]
            pub fn errors(&self) -> &[$crate::dom::error::Diagnostic] {
                &self.inner.diagnostics
            }

            /// Return whether this node has no directly attached semantic diagnostics.
            #[must_use]
            pub fn is_valid_node(&self) -> bool {
                self.inner.diagnostics.is_empty()
            }
        }

        #[cfg(test)]
        impl From<$inner> for $name {
            fn from(inner: $inner) -> $name {
                let inner = Arc::new(inner);
                let id = NodeId::new(
                    0,
                    NodeSeed::$variant {
                        inner: Arc::clone(&inner),
                    },
                );
                $name {
                    inner,
                    id: id.clone(),
                    arena: DomArena::detached(id),
                }
            }
        }

        #[cfg(test)]
        impl From<$inner> for Node {
            fn from(inner: $inner) -> Node {
                Node::$variant($name::from(inner))
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_struct(stringify!($name))
                    .field("id", &self.id)
                    .field("syntax", &self.inner.syntax)
                    .field("diagnostics", &self.inner.diagnostics)
                    .finish_non_exhaustive()
            }
        }
    };
}

/// Implement document comment accessors for one comment-bearing DOM wrapper.
macro_rules! impl_comment_accessors {
  ($name:ident) => {
    impl $name {
      /// Iterate over every source comment in the containing document.
      #[must_use]
      pub fn comments(&self) -> impl ExactSizeIterator<Item = $crate::dom::Comment> + '_ {
        self.arena.comments.all().iter().cloned()
      }

      /// Iterate over comments preceding the first document item.
      #[must_use]
      pub fn header_comments(&self) -> impl ExactSizeIterator<Item = $crate::dom::Comment> + '_ {
        self.arena.comments.header().iter().cloned()
      }
    }
  };
}

/// Implement direct metadata accessors for one immutable container wrapper.
macro_rules! impl_container_accessors {
  ($name:ident) => {
    impl $name {
      /// Return the immutable syntax anchor retained from the source document.
      #[must_use]
      pub fn syntax(&self) -> Option<&SyntaxElement> {
        self.inner.syntax.as_ref()
      }

      /// Return semantic diagnostics attached directly to this node.
      #[must_use]
      pub fn errors(&self) -> &[Diagnostic] {
        &self.inner.diagnostics
      }

      /// Return whether this node has no directly attached semantic diagnostics.
      #[must_use]
      pub fn is_valid_node(&self) -> bool {
        self.inner.diagnostics.is_empty()
      }
    }
  };
}

/// Render the stable non-exhaustive identity of one composite arena node.
fn fmt_container(
  formatter: &mut Formatter<'_>,
  name: &'static str,
  id: &NodeId,
  kind: &impl fmt::Debug,
  children_name: &'static str,
  children_len: usize,
) -> fmt::Result {
  formatter
    .debug_struct(name)
    .field("id", id)
    .field("kind", kind)
    .field(children_name, &children_len)
    .finish_non_exhaustive()
}

/// Immutable metadata shared by table and array containers.
#[derive(Debug)]
pub(in crate::dom) struct ContainerInner<K> {
  /// Diagnostics attached directly to the container.
  pub(in crate::dom) diagnostics: Arc<[Diagnostic]>,
  /// Source anchor for the container.
  pub(in crate::dom) syntax:      Option<SyntaxElement>,
  /// Semantic container representation.
  pub(in crate::dom) kind:        K,
}

impl<K> ContainerInner<K> {
  /// Publish owned container metadata in its immutable storage shape.
  pub(in crate::dom) const fn new(diagnostics: Arc<[Diagnostic]>, syntax: Option<SyntaxElement>, kind: K) -> Self {
    Self {
      diagnostics,
      syntax,
      kind,
    }
  }
}

/// Immutable table storage published after DOM construction.
pub(in crate::dom) type TableInner = ContainerInner<TableKind>;

/// A TOML table.
#[derive(Clone)]
pub struct Table {
  /// Shared immutable table metadata.
  pub(in crate::dom) inner:   Arc<TableInner>,
  /// Immutable table edges containing child identities.
  pub(in crate::dom) entries: Arc<ArenaEntries>,
  /// Stable arena identity.
  pub(in crate::dom) id:      NodeId,
  /// Shared immutable document arena.
  pub(in crate::dom) arena:   Arc<DomArena>,
}
impl_comment_accessors!(Table);
impl_container_accessors!(Table);

impl fmt::Debug for Table {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    fmt_container(formatter, "Table", &self.id, &self.inner.kind, "entries", self.entries.all.len())
  }
}

impl Table {
  /// Look up a table entry by its decoded key.
  #[must_use]
  pub fn get(&self, key: impl Into<Key>) -> Option<Node> {
    let index = *self.entries.lookup.get(&key.into())?;
    self
      .entries
      .all
      .get(index)
      .map(|entry| entry.node.node(Arc::clone(&self.arena)))
  }

  /// Return the immutable ordered table entries.
  #[must_use]
  pub fn entries(&self) -> Entries {
    Entries::new(Arc::clone(&self.arena), Arc::clone(&self.entries))
  }

  /// Return this table's semantic representation.
  #[must_use]
  pub fn kind(&self) -> TableKind {
    self.inner.kind
  }
}

/// Define one public source-representation enum.
macro_rules! define_representation {
  (
    $(#[$metadata:meta])*
    pub enum $name:ident {
      $(
        $(#[$variant_metadata:meta])*
        $variant:ident
      ),+ $(,)?
    }
  ) => {
    $(#[$metadata])*
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum $name {
      $(
        $(#[$variant_metadata])*
        $variant,
      )+
    }
  };
}

define_representation! {
  /// Semantic representation of a TOML table.
  pub enum TableKind {
    /// A document or table-header table.
    Regular,
    /// An inline table value.
    Inline,
    /// An implicit table introduced by a dotted key or header path.
    Pseudo,
  }
}

/// Immutable decoded key storage.
#[derive(Debug)]
pub(in crate::dom) struct KeyInner {
  /// Diagnostics attached directly to the key.
  pub(in crate::dom) diagnostics:         Arc<[Diagnostic]>,
  /// Primary source anchor for the key.
  pub(in crate::dom) syntax:              Option<SyntaxElement>,
  /// Whether the decoded key can participate in equality and lookup.
  pub(in crate::dom) is_valid:            bool,
  /// Decoded key value.
  pub(in crate::dom) value:               Arc<str>,
  /// Other source occurrences merged into the same semantic key.
  pub(in crate::dom) additional_syntaxes: Arc<[SyntaxElement]>,
}

wrap_node! {
    /// A decoded TOML key.
    #[derive(Debug, Clone)]
    pub struct Key { inner: KeyInner }
}

impl<S> From<S> for Key
where
  S: Into<String>,
{
  fn from(key: S) -> Self {
    Self::new(key)
  }
}

impl Key {
  /// Return a detached key with the given decoded value.
  ///
  /// This constructor does not validate or modify the supplied value.
  #[must_use]
  pub fn new(key: impl Into<String>) -> Self {
    KeyInner {
      diagnostics:         Arc::default(),
      syntax:              None,
      is_valid:            true,
      value:               Arc::from(key.into()),
      additional_syntaxes: Arc::default(),
    }
    .wrap()
  }

  /// Return the decoded key value.
  #[must_use]
  pub fn value(&self) -> &str {
    &self.inner.value
  }

  /// Iterate over every source range merged into this semantic key.
  #[allow(
    clippy::single_call_fn,
    reason = "the public iterator exposes every merged key occurrence as one stable source-provenance query"
  )]
  #[must_use]
  pub fn text_ranges(&self) -> impl ExactSizeIterator<Item = TextRange> {
    let mut ranges = Vec::with_capacity(self.inner.additional_syntaxes.len().saturating_add(1));
    if let Some(syntax) = self.syntax() {
      ranges.push(syntax.text_range());
    }
    ranges.extend(self.inner.additional_syntaxes.iter().map(SyntaxElement::text_range));
    ranges.into_iter()
  }

  /// Return a path containing this key followed by one child segment.
  #[must_use]
  pub fn join(&self, key: impl Into<KeyOrIndex>) -> Keys {
    Keys::new(once(self.clone().into()).chain(once(key.into())))
  }
}

impl AsRef<str> for Key {
  fn as_ref(&self) -> &str {
    self.value()
  }
}

impl Display for Key {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    if let Some(syntax) = self.syntax() {
      return syntax.fmt(formatter);
    }

    let key_value = self.value();
    let is_bare = !key_value.is_empty()
      && key_value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if !is_bare {
      formatter.write_char('"')?;
      formatter.write_str(&escape(key_value))?;
      formatter.write_char('"')?;
      return Ok(());
    }

    key_value.fmt(formatter)
  }
}

impl PartialEq for Key {
  fn eq(&self, other: &Self) -> bool {
    if self.inner.is_valid && other.inner.is_valid {
      self.value() == other.value()
    } else {
      Arc::ptr_eq(&self.inner, &other.inner)
    }
  }
}

impl Eq for Key {}

impl Hash for Key {
  fn hash<H: Hasher>(&self, state: &mut H) {
    if self.inner.is_valid {
      self.value().hash(state);
    } else {
      0_u8.hash(state);
    }
  }
}

/// Immutable array storage published after DOM construction.
pub(in crate::dom) type ArrayInner = ContainerInner<ArrayKind>;

/// A TOML array or array of tables.
#[derive(Clone)]
pub struct Array {
  /// Shared immutable array metadata.
  pub(in crate::dom) inner: Arc<ArrayInner>,
  /// Immutable ordered child identities.
  pub(in crate::dom) items: Arc<[NodeId]>,
  /// Stable arena identity.
  pub(in crate::dom) id:    NodeId,
  /// Shared immutable document arena.
  pub(in crate::dom) arena: Arc<DomArena>,
}
impl_comment_accessors!(Array);
impl_container_accessors!(Array);

impl fmt::Debug for Array {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    fmt_container(formatter, "Array", &self.id, &self.inner.kind, "items", self.items.len())
  }
}

impl Array {
  /// Return the immutable ordered array items.
  #[must_use]
  pub fn items(&self) -> ArrayItems {
    ArrayItems::new(Arc::clone(&self.arena), Arc::clone(&self.items))
  }

  /// Return this array's semantic representation.
  #[must_use]
  pub fn kind(&self) -> ArrayKind {
    self.inner.kind
  }
}

define_representation! {
  /// Semantic representation of a TOML array.
  pub enum ArrayKind {
    /// An array of tables.
    Tables,
    /// An inline array value.
    Inline,
  }
}

impl ArrayKind {
  /// Return whether this is an array of tables.
  #[must_use]
  pub const fn is_tables(self) -> bool {
    matches!(self, Self::Tables)
  }

  /// Return whether this is an inline array.
  #[must_use]
  pub const fn is_inline(self) -> bool {
    matches!(self, Self::Inline)
  }
}

/// Immutable metadata shared by decoded scalar values.
#[derive(Debug)]
pub(in crate::dom) struct ScalarInner<V> {
  /// Diagnostics attached directly to the value.
  pub(in crate::dom) diagnostics: Arc<[Diagnostic]>,
  /// Source anchor for the value.
  pub(in crate::dom) syntax:      Option<SyntaxElement>,
  /// Decoded scalar value.
  pub(in crate::dom) value:       V,
}

/// Immutable Boolean storage.
pub(in crate::dom) type BoolInner = ScalarInner<bool>;

wrap_node! {
    /// A decoded TOML Boolean.
    #[derive(Clone)]
    pub struct Bool, Bool { inner: BoolInner }
}
impl_comment_accessors!(Bool);

impl Bool {
  /// Return the decoded Boolean value.
  #[must_use]
  pub fn value(&self) -> bool {
    self.inner.value
  }
}

/// Immutable string storage.
pub(in crate::dom) type StrInner = ScalarInner<Arc<str>>;

wrap_node! {
    /// A decoded TOML string.
    #[derive(Clone)]
    pub struct Str, Str { inner: StrInner }
}
impl_comment_accessors!(Str);

impl Str {
  /// Return the decoded string value.
  #[must_use]
  pub fn value(&self) -> &str {
    &self.inner.value
  }
}

define_representation! {
  /// Source representation of a string value.
  pub enum StrRepr {
    /// A double-quoted basic string.
    Basic,
    /// A multiline double-quoted basic string.
    MultiLine,
    /// A single-quoted literal string.
    Literal,
    /// A multiline single-quoted literal string.
    MultiLineLiteral,
  }
}

/// Immutable integer storage.
#[derive(Debug)]
pub(in crate::dom) struct IntegerInner {
  /// Diagnostics attached directly to the value.
  pub(in crate::dom) diagnostics: Arc<[Diagnostic]>,
  /// Source anchor for the value.
  pub(in crate::dom) syntax:      Option<SyntaxElement>,
  /// Original integer representation.
  pub(in crate::dom) repr:        IntegerRepr,
  /// Decoded integer value.
  pub(in crate::dom) value:       IntegerValue,
}

wrap_node! {
    /// A decoded TOML integer.
    #[derive(Clone)]
    pub struct Integer, Integer { inner: IntegerInner }
}
impl_comment_accessors!(Integer);

impl Integer {
  /// Return the decoded integer value.
  #[must_use]
  pub fn value(&self) -> IntegerValue {
    self.inner.value
  }

  /// Return the source representation used for this integer.
  #[must_use]
  pub fn representation(&self) -> IntegerRepr {
    self.inner.repr
  }
}

/// Source representation of an integer value.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum IntegerRepr {
  /// Decimal notation.
  Dec,
  /// Binary notation.
  Bin,
  /// Octal notation.
  Oct,
  /// Hexadecimal notation.
  Hex,
}

/// Decoded signedness-preserving integer value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegerValue {
  /// A negative decimal integer.
  Negative(i64),
  /// A nonnegative integer.
  Positive(u64),
}

impl IntegerValue {
  /// Return whether this integer is negative.
  #[must_use]
  pub const fn is_negative(self) -> bool {
    matches!(self, Self::Negative(..))
  }

  /// Return whether this integer is nonnegative.
  #[must_use]
  pub const fn is_positive(self) -> bool {
    matches!(self, Self::Positive(..))
  }

  /// Return the signed value when this integer is negative.
  #[must_use]
  pub const fn as_negative(self) -> Option<i64> {
    if let Self::Negative(negative) = self {
      Some(negative)
    } else {
      None
    }
  }

  /// Return the unsigned value when this integer is nonnegative.
  #[must_use]
  pub const fn as_positive(self) -> Option<u64> {
    if let Self::Positive(positive) = self {
      Some(positive)
    } else {
      None
    }
  }
}

impl Display for IntegerValue {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match *self {
      Self::Negative(negative) => negative.fmt(formatter),
      Self::Positive(positive) => positive.fmt(formatter),
    }
  }
}

/// Immutable floating-point storage.
pub(in crate::dom) type FloatInner = ScalarInner<f64>;

wrap_node! {
    /// A decoded TOML floating-point value.
    #[derive(Clone)]
    pub struct Float, Float { inner: FloatInner }
}
impl_comment_accessors!(Float);

impl Float {
  /// Return the decoded floating-point value.
  #[must_use]
  pub fn value(&self) -> f64 {
    self.inner.value
  }
}

/// Immutable date or time storage.
pub(in crate::dom) type DateTimeInner = ScalarInner<DateTimeValue>;

wrap_node! {
    /// A decoded TOML date or time.
    #[derive(Clone)]
    pub struct DateTime, Date { inner: DateTimeInner }
}
impl_comment_accessors!(DateTime);

impl DateTime {
  /// Return the decoded date or time value.
  #[must_use]
  pub fn value(&self) -> DateTimeValue {
    self.inner.value
  }
}

/// Decoded TOML date or time value.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum DateTimeValue {
  /// An offset date-time.
  OffsetDateTime(time::OffsetDateTime),
  /// A local date-time.
  LocalDateTime(time::PrimitiveDateTime),
  /// A local date.
  Date(time::Date),
  /// A local time.
  Time(time::Time),
}

impl Display for DateTimeValue {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match *self {
      Self::OffsetDateTime(date_time) => date_time.format(&Rfc3339).map_err(|_format_error| fmt::Error)?.fmt(formatter),
      Self::LocalDateTime(date_time) => date_time
        .format(if date_time.time().nanosecond() > 0 {
          &format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond]")
        } else {
          &format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]")
        })
        .map_err(|_format_error| fmt::Error)?
        .fmt(formatter),
      Self::Date(calendar_date) => calendar_date
        .format(&format_description!("[year]-[month]-[day]"))
        .map_err(|_format_error| fmt::Error)?
        .fmt(formatter),
      Self::Time(time) => time
        .format(if time.nanosecond() > 0 {
          &format_description!("[hour]:[minute]:[second].[subsecond]")
        } else {
          &format_description!("[hour]:[minute]:[second]")
        })
        .map_err(|_format_error| fmt::Error)?
        .fmt(formatter),
    }
  }
}

/// Immutable malformed-node storage.
#[derive(Debug)]
pub(in crate::dom) struct InvalidInner {
  /// Diagnostics attached directly to the malformed node.
  pub(in crate::dom) diagnostics: Arc<[Diagnostic]>,
  /// Source anchor for the malformed node.
  pub(in crate::dom) syntax:      Option<SyntaxElement>,
  /// Typed reason the node is malformed.
  pub(in crate::dom) reason:      InvalidReason,
}

wrap_node! {
    /// A malformed source node retained for tolerant editor behavior.
    #[derive(Clone)]
    pub struct Invalid, Invalid { inner: InvalidInner }
}
impl_comment_accessors!(Invalid);

impl Invalid {
  /// Return the typed reason this source node is invalid.
  #[must_use]
  pub fn reason(&self) -> &InvalidReason {
    &self.inner.reason
  }
}

/// Intended semantic kind of a malformed scalar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScalarKind {
  /// A Boolean scalar.
  Bool,
  /// A string scalar.
  String,
  /// An integer scalar.
  Integer,
  /// A floating-point scalar.
  Float,
  /// A date or date-time scalar.
  DateTime,
}

impl ScalarKind {
  /// Return the stable reader-facing name of this scalar kind.
  #[must_use]
  pub const fn as_str(self) -> &'static str {
    match self {
      Self::Bool => "Boolean",
      Self::String => "string",
      Self::Integer => "integer",
      Self::Float => "floating-point",
      Self::DateTime => "date-time",
    }
  }
}

impl Display for ScalarKind {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter.write_str(self.as_str())
  }
}

/// Reason a scalar token could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeFailure {
  /// The syntax element did not contain the expected token.
  #[error("the scalar syntax did not contain a token")]
  MissingToken,
  /// A string escape failed at the given byte offset.
  #[error("invalid escape sequence at byte offset {offset}")]
  InvalidEscape {
    /// Byte offset within the scalar body.
    offset: usize,
  },
  /// A typed scalar parser rejected its source text.
  #[error("{message}")]
  InvalidValue {
    /// Typed parser diagnostic.
    message: Arc<str>,
  },
}

/// Canonical typed description of one scalar that could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the {kind} scalar could not be decoded: {failure}")]
pub struct MalformedScalar {
  /// Intended scalar kind.
  kind:    ScalarKind,
  /// Typed decode failure.
  #[source]
  failure: DecodeFailure,
  /// Source anchor for the malformed scalar.
  syntax:  SyntaxElement,
}

impl MalformedScalar {
  /// Construct one malformed-scalar value during DOM publication.
  pub(in crate::dom) const fn new(kind: ScalarKind, failure: DecodeFailure, syntax: SyntaxElement) -> Self {
    Self {
      kind,
      failure,
      syntax,
    }
  }

  /// Return the intended scalar kind.
  #[must_use]
  pub const fn kind(&self) -> ScalarKind {
    self.kind
  }

  /// Return the typed scalar decode failure.
  #[must_use]
  pub const fn failure(&self) -> &DecodeFailure {
    &self.failure
  }

  /// Return the source anchor for the malformed scalar.
  #[must_use]
  pub const fn syntax(&self) -> &SyntaxElement {
    &self.syntax
  }
}

/// Reason a DOM node represents malformed source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvalidReason {
  /// The syntax element was not valid in its semantic position.
  UnexpectedSyntax {
    /// Source anchor for the unexpected syntax.
    syntax: SyntaxElement,
  },
  /// A lexically classified scalar could not be decoded.
  MalformedScalar(MalformedScalar),
}

#[cfg(test)]
/// Immutable DOM construction, traversal, identity, and thread-safety contracts.
mod tests {
  use std::collections::HashSet;
  use std::sync::Arc;

  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;

  use super::ArrayKind;
  use super::BoolInner;
  use super::DecodeFailure;
  use super::IntegerRepr;
  use super::IntegerValue;
  use super::InvalidReason;
  use super::Key;
  use super::KeyInner;
  use super::Node;
  use super::ScalarKind;
  use super::TableKind;
  use crate::dom::Comment;
  use crate::dom::Entries;
  use crate::dom::KeyOrIndex;
  use crate::dom::Keys;
  use crate::dom::error::Diagnostic;
  use crate::dom::error::QueryError;
  use crate::dom::rewrite;
  use crate::parser::Diagnostic as ParseDiagnostic;
  use crate::parser::Parse;
  use crate::parser::ParseFailure;
  use crate::parser::parse;

  /// Require the intended immutable core values to be transferable and shareable.
  fn require_send_sync<T: Send + Sync>() {}

  /// Parse a fixture whose syntax is expected to be clean.
  fn clean_dom(source: &str) -> Result<Node, TestFailure> {
    let parsed = ensure_ok(parse(source), "the DOM fixture tree must construct")?;
    ensure(parsed.diagnostics().is_empty(), "the DOM fixture syntax must be clean")?;
    Ok(parsed.into_dom())
  }

  /// Resolve one expected dotted path.
  fn path(root: &Node, dotted: &str) -> Result<Node, TestFailure> {
    let keys = ensure_ok(dotted.parse::<Keys>(), "the fixture path must be valid dotted keys")?;
    ensure_some(root.path(&keys), "the expected DOM path must exist")
  }

  /// Count conflicting-key diagnostics in a complete DOM.
  fn conflict_count(root: &Node) -> usize {
    match root.validate() {
      Ok(()) => 0,
      Err(diagnostics) => diagnostics
        .iter()
        .filter(|diagnostic| matches!(diagnostic, Diagnostic::ConflictingKeys { .. }))
        .count(),
    }
  }

  /// Merge compatible pseudo tables while retaining every contributing source range.
  #[test]
  fn shared_pseudo_tables_merge_entries_and_source_ranges() -> Result<(), TestFailure> {
    let root = clean_dom("a.b.c = 1\na.b.d = 2\n")?;
    ensure(
      matches!(path(&root, "a.b.c")?, Node::Integer(_)),
      "the first dotted-key leaf must survive the pseudo-table merge",
    )?;
    ensure(
      matches!(path(&root, "a.b.d")?, Node::Integer(_)),
      "the second dotted-key leaf must survive the pseudo-table merge",
    )?;

    let Node::Table(ref root_table) = root else {
      return ensure(false, "a parsed document root must be a table");
    };
    let root_key = ensure_some(
      root_table.entries().iter().next().map(|(key, _)| key),
      "the root pseudo-table key must exist",
    )?;
    ensure_eq(
      &root_key.text_ranges().count(),
      &2,
      "the shared root key must retain both source occurrences",
    )?;

    let Node::Table(a_table) = path(&root, "a")? else {
      return ensure(false, "the shared root path must remain a table");
    };
    let nested_key = ensure_some(
      a_table.entries().iter().next().map(|(key, _)| key),
      "the nested pseudo-table key must exist",
    )?;
    ensure_eq(
      &nested_key.text_ranges().count(),
      &2,
      "the shared nested key must retain both source occurrences",
    )?;
    ensure_eq(&conflict_count(&root), &0, "compatible pseudo tables must not create conflicts")
  }

  /// Report incompatible collisions without deleting either historical entry.
  #[test]
  fn incompatible_source_collisions_report_conflicts_and_keep_history() -> Result<(), TestFailure> {
    for source in ["a.b = 1\na = 2\n", "a = 1\n[a]\nb = 2\n", "a = 1\na = 2\n"] {
      let root = clean_dom(source)?;
      ensure(
        conflict_count(&root) > 0,
        "every incompatible source collision must report a key conflict",
      )?;
      let Node::Table(root_table) = root else {
        return ensure(false, "a parsed document root must be a table");
      };
      ensure_eq(
        &root_table.entries().len(),
        &2,
        "a collision must retain insertion history alongside the lookup winner",
      )?;
    }
    Ok(())
  }

  /// Preserve conflict history in both iterator directions while lookup selects the final value.
  #[test]
  fn conflicting_entries_preserve_order_while_lookup_uses_the_latest_value() -> Result<(), TestFailure> {
    let root = clean_dom("value = 1\nvalue = 2\nvalue = 3\n")?;
    let table = ensure_some(root.as_table(), "the conflicting-entry fixture root must be a table")?;
    let forward = ensure_some(
      table
        .entries()
        .iter()
        .map(|(_, node)| node.as_integer().map(super::Integer::value))
        .collect::<Option<Vec<_>>>(),
      "every conflicting historical value must remain an integer",
    )?;
    let reverse = ensure_some(
      table
        .entries()
        .iter()
        .rev()
        .map(|(_, node)| node.as_integer().map(super::Integer::value))
        .collect::<Option<Vec<_>>>(),
      "reverse iteration must retain every conflicting historical integer",
    )?;
    ensure(
      forward == vec![IntegerValue::Positive(1), IntegerValue::Positive(2), IntegerValue::Positive(3)],
      "forward iteration must retain conflicting values in source order",
    )?;
    ensure(
      reverse == vec![IntegerValue::Positive(3), IntegerValue::Positive(2), IntegerValue::Positive(1)],
      "reverse iteration must retain the exact opposite historical order",
    )?;

    let winner = ensure_some(root.get_key("value"), "decoded-key lookup must retain a conflict winner")?;
    let winner_value = ensure_some(winner.as_integer(), "the winning conflicting value must remain an integer")?.value();
    ensure_eq(
      &winner_value,
      &IntegerValue::Positive(3),
      "decoded-key lookup must return the final source occurrence",
    )
  }

  /// Permit one explicit definition of a table previously implied by a descendant header.
  #[test]
  fn implicit_header_can_be_defined_once() -> Result<(), TestFailure> {
    let root = clean_dom("[a.b]\nvalue = 1\n[a]\nother = 2\n")?;
    ensure_eq(
      &conflict_count(&root),
      &0,
      "a super-table implied by an earlier header may be explicitly defined once",
    )?;
    let Node::Table(table) = path(&root, "a")? else {
      return ensure(false, "the defined super-table must remain a table");
    };
    ensure(
      table.kind() == TableKind::Regular,
      "an explicitly defined super-table must no longer remain pseudo",
    )?;
    ensure(
      [path(&root, "a.b.value")?.is_integer(), path(&root, "a.other")?.is_integer()] == [true, true],
      "defining the super-table must preserve earlier children and accept later entries",
    )
  }

  /// Diagnose a second explicit definition after an implicit table has been materialized.
  #[test]
  fn implicit_header_redefinition_is_a_conflict() -> Result<(), TestFailure> {
    let root = clean_dom("[a.b]\nvalue = 1\n[a]\nfirst = 2\n[a]\nsecond = 3\n")?;
    ensure_eq(
      &conflict_count(&root),
      &1,
      "an implicit super-table becomes explicitly defined after its first header",
    )
  }

  /// Resolve array-table intermediates while diagnosing incompatible inline container prefixes.
  #[test]
  fn intermediate_headers_preserve_container_compatibility_contracts() -> Result<(), TestFailure> {
    let inline_table = clean_dom("a = { value = 1 }\n[a.b]\nchild = 2\n")?;
    let inline_table_errors = ensure_some(
      inline_table.validate().err(),
      "an inline-table prefix must reject a descendant regular header",
    )?;
    ensure(
      inline_table_errors
        .iter()
        .any(|diagnostic| matches!(diagnostic, Diagnostic::ExpectedTable { .. })),
      "an inline-table prefix must retain the expected-table diagnostic family",
    )?;

    let inline_array = clean_dom("a = [1]\n[a.b]\nchild = 2\n")?;
    let inline_array_errors = ensure_some(
      inline_array.validate().err(),
      "an inline-array prefix must reject a descendant regular header",
    )?;
    ensure(
      inline_array_errors
        .iter()
        .any(|diagnostic| matches!(diagnostic, Diagnostic::ExpectedArrayOfTables { .. })),
      "an inline-array prefix must retain the expected-array-of-tables diagnostic family",
    )?;

    let array_tables = clean_dom("[[a]]\nvalue = 1\n[a.b]\nchild = 2\n")?;
    ensure(
      array_tables.validate().is_ok(),
      "a descendant header must attach to the latest array-table element",
    )?;
    let array = ensure_some(
      array_tables.get_key("a").and_then(|node| node.as_array().cloned()),
      "the compatible array-table root must remain an array",
    )?;
    let item = ensure_some(
      array.items().first(),
      "the compatible array-table root must retain its first element",
    )?;
    let child = ensure_some(
      item.get_key("b").and_then(|table| table.get_key("child")),
      "the descendant header must remain beneath the concrete array-table element",
    )?;
    ensure(child.is_integer(), "the compatible array-table descendant must remain addressable")
  }

  /// Keep distinct decoded keys addressable without semantic conflicts.
  #[test]
  fn nonconflicting_insertion_is_clean() -> Result<(), TestFailure> {
    let root = clean_dom("a = 1\nb = 2\n")?;
    ensure_eq(&conflict_count(&root), &0, "distinct keys must not create a conflict")?;
    ensure(
      [path(&root, "a")?.is_integer(), path(&root, "b")?.is_integer()] == [true, true],
      "both distinct entries must remain addressable",
    )
  }

  /// Expose exact forward and reverse source order from immutable entries.
  #[test]
  fn ordered_entries_iterate_forward_and_backward() -> Result<(), TestFailure> {
    let root = clean_dom("first = 1\nsecond = 2\nthird = 3\n")?;
    let table = ensure_some(root.as_table(), "a parsed document root must expose its table")?;
    let forward = table
      .entries()
      .iter()
      .map(|(key, _)| key.value().to_owned())
      .collect::<Vec<_>>();
    let backward = table
      .entries()
      .iter()
      .rev()
      .map(|(key, _)| key.value().to_owned())
      .collect::<Vec<_>>();

    ensure(
      forward == vec!["first".to_owned(), "second".to_owned(), "third".to_owned()],
      "forward entry iteration must retain source insertion order",
    )?;
    ensure(
      backward == vec!["third".to_owned(), "second".to_owned(), "first".to_owned()],
      "reverse entry iteration must expose the exact opposite insertion order",
    )
  }

  /// Convert a missing value into a tolerant invalid node without consuming its successor.
  #[test]
  fn missing_entry_value_becomes_invalid_without_consuming_the_next_entry() -> Result<(), TestFailure> {
    let parsed = ensure_ok(
      parse("missing =\nnext = 1\n"),
      "the malformed entry fixture tree must still construct",
    )?;
    ensure(
      !parsed.diagnostics().is_empty(),
      "a missing entry value must retain a recoverable syntax diagnostic",
    )?;
    let root = parsed.into_dom();

    ensure(
      path(&root, "missing")?.is_invalid(),
      "a missing entry value must become an invalid semantic node",
    )?;
    ensure(
      path(&root, "next")?.is_integer(),
      "entry recovery must preserve the following valid scalar",
    )
  }

  /// Preserve the intended scalar kind and decode failure on malformed numeric source.
  #[test]
  fn malformed_scalar_becomes_typed_invalid_node() -> Result<(), TestFailure> {
    let root = clean_dom("value = 999999999999999999999999999999\n")?;
    let Node::Invalid(invalid) = path(&root, "value")? else {
      return ensure(false, "an out-of-range integer must become an invalid node");
    };
    ensure(
      matches!(
        invalid.reason(),
        InvalidReason::MalformedScalar(malformed) if malformed.kind() == ScalarKind::Integer
      ),
      "the invalid node must retain its intended integer kind and decode failure",
    )?;
    ensure(
      invalid
        .errors()
        .iter()
        .any(|diagnostic| matches!(diagnostic, Diagnostic::MalformedScalar(_))),
      "the malformed scalar must publish a semantic diagnostic",
    )
  }

  /// Represent lookup absence as `None` and match globs against actual decoded keys.
  #[test]
  fn missing_lookup_is_none_and_glob_matches_real_keys() -> Result<(), TestFailure> {
    let root = clean_dom("alpha = 1\nbeta = 2\n")?;
    ensure(
      root.get_key("missing").is_none(),
      "lookup absence must not manufacture an invalid node",
    )?;
    let matches = ensure_ok(root.get_matches("a*"), "the glob query must compile")?.collect::<Vec<_>>();
    ensure_eq(&matches.len(), &1, "the glob must match only the actual alpha key")?;
    ensure(
      matches
        .first()
        .is_some_and(|matched| matched.0.as_key().is_some_and(|matched_key| matched_key.value() == "alpha")),
      "the matched key must be alpha rather than the pattern itself",
    )
  }

  /// Validate glob syntax independently of document contents and honor exact-depth queries.
  #[test]
  fn path_globs_are_validated_before_traversal_and_respect_depth() -> Result<(), TestFailure> {
    let empty = clean_dom("")?;
    ensure(
      matches!(
        empty.find_all_matches(&Keys::single(Key::new("[")), false),
        Err(QueryError::InvalidGlob(_))
      ),
      "an invalid glob must be rejected even when the document has no descendants",
    )?;

    let root = clean_dom("a.b = 1\na.c.d = 2\n")?;
    let query = Keys::single(Key::new("a"));
    let exact_count = ensure_ok(root.find_all_matches(&query, false), "the exact-depth query must compile")?.len();
    ensure_eq(&exact_count, &1, "an exact-depth query must return only the matched table")?;

    let descendant_count = ensure_ok(root.find_all_matches(&query, true), "the prefix query must compile")?.len();
    ensure_eq(
      &descendant_count,
      &4,
      "a prefix query must include the matched table and all of its descendants",
    )
  }

  /// Keep detached semantic values free of fabricated source provenance.
  #[test]
  fn detached_nodes_do_not_fabricate_source_ranges() -> Result<(), TestFailure> {
    let detached: Node = BoolInner {
      diagnostics: Arc::default(),
      syntax:      None,
      value:       true,
    }
    .into();

    ensure_eq(
      &detached.text_ranges(true).count(),
      &0,
      "a detached semantic value must not claim the source origin range",
    )
  }

  /// Preserve valid detached-key rendering and identity-based equality for invalid keys.
  #[test]
  fn detached_and_invalid_keys_preserve_valid_rendering_and_equality() -> Result<(), TestFailure> {
    let parent = Key::new("parent");
    ensure(parent.as_ref() == "parent", "a key must expose its decoded value through `AsRef`")?;
    let joined = parent.join(Key::new("child"));
    let expected = Keys::new([KeyOrIndex::Key(parent), KeyOrIndex::Key(Key::new("child"))].into_iter());
    ensure(
      joined == expected,
      "joining a key must retain the parent and append exactly one child segment",
    )?;
    ensure_eq(
      &Key::new("can't\n").to_string(),
      &"\"can't\\n\"".to_owned(),
      "a detached non-bare key must use escaped basic-key syntax",
    )?;

    let invalid = KeyInner {
      diagnostics:         Arc::default(),
      syntax:              None,
      is_valid:            false,
      value:               Arc::from("invalid"),
      additional_syntaxes: Arc::default(),
    }
    .wrap();
    let same = invalid.clone();
    let distinct = KeyInner {
      diagnostics:         Arc::default(),
      syntax:              None,
      is_valid:            false,
      value:               Arc::from("invalid"),
      additional_syntaxes: Arc::default(),
    }
    .wrap();
    ensure(invalid == same, "clones of one invalid key must remain reflexively equal")?;
    ensure(
      invalid != distinct,
      "separate invalid keys must not collapse into one semantic lookup identity",
    )?;
    let mut invalid_keys = HashSet::new();
    ensure(invalid_keys.insert(invalid), "the first invalid key identity must be insertable")?;
    ensure(
      (invalid_keys.contains(&same), invalid_keys.contains(&distinct)) == (true, false),
      "invalid-key hashing must preserve clone identity without conflating distinct invalid keys",
    )
  }

  /// Require every published immutable DOM and rewrite value to remain `Send + Sync`.
  #[test]
  fn frozen_core_contracts_are_send_and_sync() {
    require_send_sync::<Parse>();
    require_send_sync::<ParseDiagnostic>();
    require_send_sync::<ParseFailure>();
    require_send_sync::<Node>();
    require_send_sync::<super::Table>();
    require_send_sync::<super::Array>();
    require_send_sync::<super::Bool>();
    require_send_sync::<super::Str>();
    require_send_sync::<super::Integer>();
    require_send_sync::<super::Float>();
    require_send_sync::<super::DateTime>();
    require_send_sync::<super::Invalid>();
    require_send_sync::<Diagnostic>();
    require_send_sync::<Comment>();
    require_send_sync::<Entries>();
    require_send_sync::<KeyOrIndex>();
    require_send_sync::<Keys>();
    require_send_sync::<Key>();
    require_send_sync::<InvalidReason>();
    require_send_sync::<rewrite::Rewrite>();
    require_send_sync::<rewrite::ExactPath>();
    require_send_sync::<rewrite::ValueFragment>();
    require_send_sync::<rewrite::EntryFragment>();
    require_send_sync::<rewrite::ArrayElementFragment>();
    require_send_sync::<rewrite::TableBlockFragment>();
    require_send_sync::<rewrite::Patch>();
    require_send_sync::<rewrite::PendingPatch>();
    require_send_sync::<rewrite::SemanticDiagnostic>();
    require_send_sync::<rewrite::RewriteError>();
  }

  /// Share one frozen document comment index across enum and concrete node wrappers.
  #[test]
  fn frozen_comments_are_shared_from_the_document_root() -> Result<(), TestFailure> {
    let root = clean_dom("# header\n#:schema memory://fixture\nvalue = 1 # trailing\n")?;
    let scalar = path(&root, "value")?;
    let table = ensure_some(root.as_table(), "the document root must expose its concrete table")?;
    let integer = ensure_some(scalar.as_integer(), "the scalar fixture must expose its concrete integer wrapper")?;
    let root_comments = root
      .comments()
      .map(|comment| (comment.directive().map(ToOwned::to_owned), comment.value().to_owned()))
      .collect::<Vec<_>>();
    let scalar_comments = scalar
      .comments()
      .map(|comment| (comment.directive().map(ToOwned::to_owned), comment.value().to_owned()))
      .collect::<Vec<_>>();
    let table_comments = table
      .comments()
      .map(|comment| (comment.directive().map(ToOwned::to_owned), comment.value().to_owned()))
      .collect::<Vec<_>>();
    let integer_comments = integer
      .comments()
      .map(|comment| (comment.directive().map(ToOwned::to_owned), comment.value().to_owned()))
      .collect::<Vec<_>>();
    ensure(
      scalar_comments == root_comments,
      "every frozen node must share the document-wide immutable comment index",
    )?;
    ensure(
      table_comments == root_comments,
      "a concrete table wrapper must share the document-wide immutable comment index",
    )?;
    ensure(
      integer_comments == root_comments,
      "a concrete scalar wrapper must share the document-wide immutable comment index",
    )?;
    ensure(
      root_comments
        == vec![
          (None, " header".to_owned()),
          (Some("schema".to_owned()), "memory://fixture".to_owned()),
          (None, " trailing".to_owned()),
        ],
      "ordinary and directive comments must decode once in source order",
    )?;

    let node_headers = scalar
      .header_comments()
      .map(|comment| comment.value().to_owned())
      .collect::<Vec<_>>();
    let concrete_headers = integer
      .header_comments()
      .map(|comment| comment.value().to_owned())
      .collect::<Vec<_>>();
    ensure(
      node_headers == vec![" header".to_owned(), "memory://fixture".to_owned()],
      "header comments must exclude comments attached after the first document item",
    )?;
    ensure(
      concrete_headers == node_headers,
      "concrete and enum wrappers must expose the same frozen header comments",
    )
  }

  /// Complete source fixture shared by the concrete wrapper contract tests.
  const CONCRETE_WRAPPER_SOURCE: &str =
    "# document\nboolean = true\nstring = \"value\"\nnegative = -2\npositive = 3\nbinary = 0b10\noctal = 0o10\nhexadecimal = 0x10\nfloat \
     = 1.5\noffset = 1979-05-27T07:32:00Z\nlocal = 1979-05-27T07:32:00\nlocal_fraction = 1979-05-27T07:32:00.5\ndate = 1979-05-27\ntime = \
     07:32:00\ntime_fraction = 07:32:00.5\narray = [1]\ninvalid = 999999999999999999999999999999\n[[tables]]\nname = \"first\"\n";

  /// Parse the complete concrete wrapper fixture.
  fn concrete_wrapper_dom() -> Result<Node, TestFailure> {
    clean_dom(CONCRETE_WRAPPER_SOURCE)
  }

  /// Project source metadata shared by every concrete node wrapper.
  fn source_facts(node: &Node) -> (bool, usize, bool, usize, usize) {
    (
      node.syntax().is_some(),
      node.errors().len(),
      node.is_valid_node(),
      node.comments().count(),
      node.header_comments().count(),
    )
  }

  /// Require one source-backed wrapper to retain shared fixture metadata.
  fn ensure_source_backed_wrapper(node: &Node, debug_name_present: bool, context: &'static str) -> Result<(), TestFailure> {
    ensure((source_facts(node), debug_name_present) == ((true, 0, true, 1, 1), true), context)
  }

  /// Expose source metadata and decoded values through table, Boolean, and string wrappers.
  #[test]
  fn table_boolean_and_string_wrappers_preserve_source_contracts() -> Result<(), TestFailure> {
    let root = concrete_wrapper_dom()?;
    let table = ensure_some(root.as_table(), "the wrapper fixture root must be a table")?;
    ensure(
      (
        table.syntax().is_some(),
        table.errors().len(),
        table.is_valid_node(),
        table.kind(),
        table.header_comments().count(),
        format!("{table:?}").contains("Table"),
      ) == (true, 0, true, TableKind::Regular, 1, true),
      "the root table wrapper must retain source metadata, comments, kind, and stable debug identity",
    )?;

    let boolean_node = path(&root, "boolean")?;
    let boolean = ensure_some(boolean_node.as_bool(), "the Boolean path must expose its concrete wrapper")?;
    let string_node = path(&root, "string")?;
    let string = ensure_some(string_node.as_str(), "the string path must expose its concrete wrapper")?;
    ensure_eq(&boolean.value(), &true, "the Boolean wrapper must expose its decoded value")?;
    ensure_eq(&string.value(), &"value", "the string wrapper must expose its decoded value")?;
    ensure(
      [
        (source_facts(&boolean_node), format!("{boolean:?}").contains("Bool")),
        (source_facts(&string_node), format!("{string:?}").contains("Str")),
      ] == [((true, 0, true, 1, 1), true), ((true, 0, true, 1, 1), true)],
      "Boolean and string wrappers must expose shared source metadata and stable debug identities",
    )
  }

  /// Preserve signed integer identity and each source radix representation.
  #[test]
  fn integer_wrappers_preserve_signedness_and_radix() -> Result<(), TestFailure> {
    let root = concrete_wrapper_dom()?;
    let negative_node = path(&root, "negative")?;
    let negative = ensure_some(
      negative_node.as_integer(),
      "the negative integer path must expose its concrete wrapper",
    )?;
    ensure(
      (
        negative.representation(),
        negative.value().is_negative(),
        negative.value().is_positive(),
        negative.value().as_negative(),
        negative.value().as_positive(),
        negative.value().to_string(),
        negative.syntax().is_some(),
        negative.errors().len(),
        negative.is_valid_node(),
        format!("{negative:?}").contains("Integer"),
      ) == (
        IntegerRepr::Dec,
        true,
        false,
        Some(-2),
        None,
        String::from("-2"),
        true,
        0,
        true,
        true,
      ),
      "negative integers must retain signedness, decimal representation, provenance, and debug identity",
    )?;

    let positive_node = path(&root, "positive")?;
    let positive = ensure_some(
      positive_node.as_integer(),
      "the positive integer path must expose its concrete wrapper",
    )?;
    ensure(
      (
        positive.representation(),
        positive.value().is_positive(),
        positive.value().is_negative(),
        positive.value().as_positive(),
        positive.value().as_negative(),
        positive.value().to_string(),
      ) == (IntegerRepr::Dec, true, false, Some(3), None, String::from("3")),
      "nonnegative integers must retain unsigned identity and the opposite projection polarity",
    )?;
    for (path_name, representation, value) in [
      ("binary", IntegerRepr::Bin, 2_u64),
      ("octal", IntegerRepr::Oct, 8_u64),
      ("hexadecimal", IntegerRepr::Hex, 16_u64),
    ] {
      let node = path(&root, path_name)?;
      let integer = ensure_some(node.as_integer(), "a radix integer path must expose an integer wrapper")?;
      ensure(
        (integer.representation(), integer.value()) == (representation, IntegerValue::Positive(value)),
        "each radix integer must retain both its semantic value and source representation",
      )?;
    }
    Ok(())
  }

  /// Preserve floating-point metadata and every supported date/time rendering.
  #[test]
  fn float_and_date_time_wrappers_preserve_metadata_and_rendering() -> Result<(), TestFailure> {
    let root = concrete_wrapper_dom()?;
    let float_node = path(&root, "float")?;
    let float = ensure_some(float_node.as_float(), "the float path must expose its concrete wrapper")?;
    ensure_eq(&float.value(), &1.5, "the float wrapper must expose its decoded value")?;
    ensure_source_backed_wrapper(
      &float_node,
      format!("{float:?}").contains("Float"),
      "the float wrapper must expose shared source metadata",
    )?;

    for (path_name, rendered) in [
      ("offset", "1979-05-27T07:32:00Z"),
      ("local", "1979-05-27T07:32:00"),
      ("local_fraction", "1979-05-27T07:32:00.5"),
      ("date", "1979-05-27"),
      ("time", "07:32:00"),
      ("time_fraction", "07:32:00.5"),
    ] {
      let node = path(&root, path_name)?;
      let date_time = ensure_some(node.as_date(), "a date/time path must expose its concrete wrapper")?;
      ensure_eq(
        &date_time.value().to_string(),
        &rendered.to_owned(),
        "each date/time family must preserve its stable semantic rendering",
      )?;
      ensure_source_backed_wrapper(
        &node,
        format!("{date_time:?}").contains("DateTime"),
        "each date/time family must preserve shared source metadata",
      )?;
    }
    Ok(())
  }

  /// Normalize each supported alternate date/time spelling without changing its semantic value.
  #[test]
  fn date_time_decoding_normalizes_alternate_spellings() -> Result<(), TestFailure> {
    let root = clean_dom("space = 1979-05-27 07:32:00z\nlower = 1979-05-27t07:32:00z\ncomma = 07:32:00,5\n")?;
    for (path_name, expected) in [
      ("space", "1979-05-27T07:32:00Z"),
      ("lower", "1979-05-27T07:32:00Z"),
      ("comma", "07:32:00.5"),
    ] {
      let node = path(&root, path_name)?;
      let value = ensure_some(node.as_date(), "a supported alternate date/time spelling must decode")?;
      ensure_eq(
        &value.value().to_string(),
        &expected.to_owned(),
        "alternate date/time spellings must normalize without changing meaning",
      )?;
    }
    Ok(())
  }

  /// Distinguish inline arrays from arrays of tables through both kind polarities.
  #[test]
  fn array_wrappers_preserve_inline_and_table_polarities() -> Result<(), TestFailure> {
    let root = concrete_wrapper_dom()?;
    let array_node = path(&root, "array")?;
    let array = ensure_some(array_node.as_array(), "the inline array path must expose its concrete wrapper")?;
    ensure(
      (
        array.kind(),
        array.kind().is_inline(),
        array.kind().is_tables(),
        source_facts(&array_node),
        format!("{array:?}").contains("Array"),
      ) == (ArrayKind::Inline, true, false, (true, 0, true, 1, 1), true),
      "an inline array must retain its representation, source metadata, and opposite kind polarity",
    )?;
    let tables_node = path(&root, "tables")?;
    let tables = ensure_some(tables_node.as_array(), "the array-of-tables path must expose its concrete wrapper")?;
    ensure(
      (tables.kind(), tables.kind().is_tables(), tables.kind().is_inline()) == (ArrayKind::Tables, true, false),
      "an array of tables must retain its representation and opposite kind polarity",
    )
  }

  /// Preserve malformed scalar provenance, diagnostics, and reader-facing type names.
  #[test]
  fn invalid_wrappers_preserve_typed_decoder_failures() -> Result<(), TestFailure> {
    let root = concrete_wrapper_dom()?;
    let invalid_node = path(&root, "invalid")?;
    let invalid = ensure_some(
      invalid_node.as_invalid(),
      "the malformed scalar path must expose its invalid wrapper",
    )?;
    let InvalidReason::MalformedScalar(ref malformed) = *invalid.reason() else {
      return ensure(false, "an out-of-range integer must retain a malformed-scalar reason");
    };
    let invalid_syntax = ensure_some(invalid.syntax(), "a malformed source scalar must retain its syntax anchor")?;
    ensure(
      (
        malformed.kind(),
        matches!(malformed.failure(), DecodeFailure::InvalidValue { .. }),
        malformed.syntax().text_range(),
        invalid_syntax.text_range(),
        invalid.errors().is_empty(),
        invalid.is_valid_node(),
        invalid.comments().count(),
        invalid.header_comments().count(),
        format!("{invalid:?}").contains("Invalid"),
      ) == (
        ScalarKind::Integer,
        true,
        malformed.syntax().text_range(),
        malformed.syntax().text_range(),
        false,
        false,
        1,
        1,
        true,
      ),
      "an invalid wrapper must retain its typed decoder failure, provenance, diagnostics, and shared comments",
    )?;
    ensure(
      [
        ScalarKind::Bool,
        ScalarKind::String,
        ScalarKind::Integer,
        ScalarKind::Float,
        ScalarKind::DateTime,
      ]
      .map(|kind| (kind.as_str(), kind.to_string()))
        == [
          ("Boolean", "Boolean".to_owned()),
          ("string", "string".to_owned()),
          ("integer", "integer".to_owned()),
          ("floating-point", "floating-point".to_owned()),
          ("date-time", "date-time".to_owned()),
        ],
      "every malformed-scalar family must expose its stable reader-facing name",
    )
  }
}
