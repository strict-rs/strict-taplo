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
  #[allow(
    clippy::single_call_fn,
    reason = "the named projection supplies NodeId's Debug output with a stable arena-record kind label"
  )]
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
  #[allow(
    clippy::single_call_fn,
    reason = "the named constructor owns the record-free arena shared by default detached container views"
  )]
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
  #[allow(
    clippy::single_call_fn,
    reason = "the named constructor is the sole publication point pairing scalar kind, decode failure, and source anchor"
  )]
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
  use core::cmp::Ordering;
  use core::fmt::Debug;
  use std::collections::HashSet;
  use std::sync::Arc;

  use strict_test_support::ensure_that;

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

  /// Complete parse, semantic root, and validation evidence for a DOM fixture.
  type DomFixture = (Parse, Node, Result<(), Vec<Diagnostic>>);

  /// Parse a fixture without erasing either diagnostic channel.
  fn dom_fixture(source: &str) -> Result<DomFixture, ParseFailure> {
    parse(source).map(|parsed| {
      let root = parsed.clone().into_dom();
      let validation = root.validate();
      (parsed, root, validation)
    })
  }

  /// Count key conflicts by borrowing complete semantic validation evidence.
  fn conflict_count(validation: &Result<(), Vec<Diagnostic>>) -> usize {
    validation.as_ref().err().map_or(0, |diagnostics| {
      diagnostics
        .iter()
        .filter(|diagnostic| matches!(diagnostic, Diagnostic::ConflictingKeys { .. }))
        .count()
    })
  }

  /// Merge compatible pseudo tables while retaining every contributing source range.
  #[test]
  fn shared_pseudo_tables_merge_entries_and_source_ranges() -> Result<(), impl Debug> {
    ensure_that(
      dom_fixture("a.b.c = 1\na.b.d = 2\n"),
      "pseudo-table merges must preserve both leaves, source ranges, and clean semantics",
      |fixture| {
        let &Ok((ref parsed, ref root, ref validation)) = fixture else {
          return false;
        };
        let Some(table) = root.as_table() else {
          return false;
        };
        let Some(outer) = root.get_key("a") else {
          return false;
        };
        let Some(outer_table) = outer.as_table() else {
          return false;
        };
        let Some(inner) = outer.get_key("b") else {
          return false;
        };
        parsed.diagnostics().is_empty()
          && conflict_count(validation) == 0
          && table
            .entries()
            .iter()
            .next()
            .is_some_and(|entry| entry.0.text_ranges().count() == 2)
          && outer_table
            .entries()
            .iter()
            .next()
            .is_some_and(|entry| entry.0.text_ranges().count() == 2)
          && ["c", "d"]
            .into_iter()
            .all(|key| inner.get_key(key).is_some_and(|node| node.is_integer()))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Report incompatible collisions without deleting either historical entry.
  #[test]
  fn incompatible_source_collisions_report_conflicts_and_keep_history() -> Result<(), impl Debug> {
    ensure_that(
      ["a.b = 1\na = 2\n", "a = 1\n[a]\nb = 2\n", "a = 1\na = 2\n"].map(dom_fixture),
      "incompatible collisions must retain conflicts and both historical entries",
      |fixtures| {
        fixtures.iter().all(|fixture| {
          matches!(*fixture, Ok(ref value) if value.0.diagnostics().is_empty()
          && conflict_count(&value.2) > 0 && value.1.as_table().is_some_and(|table| table.entries().len() == 2))
        })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Preserve conflict history in both iterator directions while lookup selects the final value.
  #[test]
  fn conflicting_entries_preserve_order_while_lookup_uses_the_latest_value() -> Result<(), impl Debug> {
    let observed = dom_fixture("value = 1\nvalue = 2\nvalue = 3\n").map(|fixture| {
      let entries = fixture.1.as_table().map(|table| {
        (
          table.entries().iter().collect::<Vec<_>>(),
          table.entries().iter().rev().collect::<Vec<_>>(),
        )
      });
      let winner = fixture.1.get_key("value");
      (fixture, entries, winner)
    });
    ensure_that(
      observed,
      "conflict history must retain both iterator orders while lookup chooses the latest value",
      |fixture| {
        let Ok(ref value) = *fixture else {
          return false;
        };

        value.0.0.diagnostics().is_empty()
          && value.1.as_ref().is_some_and(|entries| {
            entries
              .0
              .iter()
              .map(|entry| entry.1.as_integer().map(super::Integer::value))
              .collect::<Vec<_>>()
              == [
                Some(IntegerValue::Positive(1)),
                Some(IntegerValue::Positive(2)),
                Some(IntegerValue::Positive(3)),
              ]
              && entries
                .1
                .iter()
                .map(|entry| entry.1.as_integer().map(super::Integer::value))
                .collect::<Vec<_>>()
                == [
                  Some(IntegerValue::Positive(3)),
                  Some(IntegerValue::Positive(2)),
                  Some(IntegerValue::Positive(1)),
                ]
          })
          && value.2.as_ref().is_some_and(|node| {
            node
              .as_integer()
              .is_some_and(|integer| integer.value() == IntegerValue::Positive(3))
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Permit one explicit definition of a table previously implied by a descendant header.
  #[test]
  fn implicit_header_can_be_defined_once() -> Result<(), impl Debug> {
    ensure_that(
      dom_fixture("[a.b]\nvalue = 1\n[a]\nother = 2\n"),
      "one explicit super-table definition must preserve earlier children and accept later entries without conflicts",
      |fixture| {
        let Ok(ref value) = *fixture else {
          return false;
        };

        value.0.diagnostics().is_empty()
          && conflict_count(&value.2) == 0
          && value.1.get_key("a").is_some_and(|node| {
            node.as_table().is_some_and(|table| table.kind() == TableKind::Regular)
              && node
                .get_key("b")
                .and_then(|child| child.get_key("value"))
                .is_some_and(|child| child.is_integer())
              && node.get_key("other").is_some_and(|child| child.is_integer())
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Diagnose a second explicit definition after an implicit table has been materialized.
  #[test]
  fn implicit_header_redefinition_is_a_conflict() -> Result<(), impl Debug> {
    ensure_that(
      dom_fixture("[a.b]\nvalue = 1\n[a]\nfirst = 2\n[a]\nsecond = 3\n"),
      "a second explicit super-table definition must produce exactly one conflict",
      |fixture| {
        fixture
          .as_ref()
          .is_ok_and(|value| value.0.diagnostics().is_empty() && conflict_count(&value.2) == 1)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Resolve array-table intermediates while diagnosing incompatible inline container prefixes.
  #[test]
  fn intermediate_headers_preserve_container_compatibility_contracts() -> Result<(), impl Debug> {
    ensure_that(
      [
        "a = { value = 1 }\n[a.b]\nchild = 2\n",
        "a = [1]\n[a.b]\nchild = 2\n",
        "[[a]]\nvalue = 1\n[a.b]\nchild = 2\n",
      ]
      .map(dom_fixture),
      "headers must reject incompatible inline prefixes and attach beneath the latest array-table element",
      |fixtures| {
        let &[Ok(ref table), Ok(ref array), Ok(ref tables)] = fixtures else {
          return false;
        };
        table.0.diagnostics().is_empty()
          && array.0.diagnostics().is_empty()
          && tables.0.diagnostics().is_empty()
          && table
            .2
            .as_ref()
            .is_err_and(|errors| errors.iter().any(|error| matches!(error, Diagnostic::ExpectedTable { .. })))
          && array.2.as_ref().is_err_and(|errors| {
            errors
              .iter()
              .any(|error| matches!(error, Diagnostic::ExpectedArrayOfTables { .. }))
          })
          && tables.2.is_ok()
          && tables
            .1
            .get_key("a")
            .and_then(|node| node.get_index(0))
            .and_then(|node| node.get_key("b"))
            .and_then(|node| node.get_key("child"))
            .is_some_and(|node| node.is_integer())
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Keep distinct decoded keys addressable without semantic conflicts.
  #[test]
  fn nonconflicting_insertion_is_clean() -> Result<(), impl Debug> {
    ensure_that(
      dom_fixture("a = 1\nb = 2\n"),
      "distinct keys must remain addressable without conflicts",
      |fixture| {
        let Ok(ref value) = *fixture else {
          return false;
        };

        value.0.diagnostics().is_empty()
          && conflict_count(&value.2) == 0
          && ["a", "b"]
            .into_iter()
            .all(|key| value.1.get_key(key).is_some_and(|node| node.is_integer()))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Expose exact forward and reverse source order from immutable entries.
  #[test]
  fn ordered_entries_iterate_forward_and_backward() -> Result<(), impl Debug> {
    let observed = dom_fixture("first = 1\nsecond = 2\nthird = 3\n").map(|fixture| {
      let entries = fixture.1.as_table().map(|table| {
        (
          table.entries().iter().collect::<Vec<_>>(),
          table.entries().iter().rev().collect::<Vec<_>>(),
        )
      });
      (fixture, entries)
    });
    ensure_that(
      observed,
      "immutable entries must retain exact forward and reverse source order",
      |fixture| {
        let Ok(ref value) = *fixture else {
          return false;
        };

        value.0.0.diagnostics().is_empty()
          && value.1.as_ref().is_some_and(|entries| {
            entries.0.iter().map(|entry| entry.0.value()).collect::<Vec<_>>() == ["first", "second", "third"]
              && entries.1.iter().map(|entry| entry.0.value()).collect::<Vec<_>>() == ["third", "second", "first"]
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Convert a missing value into a tolerant invalid node without consuming its successor.
  #[test]
  fn missing_entry_value_becomes_invalid_without_consuming_the_next_entry() -> Result<(), impl Debug> {
    ensure_that(
      dom_fixture("missing =\nnext = 1\n"),
      "missing values must retain syntax diagnostics and an invalid node without consuming the next entry",
      |fixture| {
        let Ok(ref value) = *fixture else {
          return false;
        };

        !value.0.diagnostics().is_empty()
          && value.1.get_key("missing").is_some_and(|node| node.is_invalid())
          && value.1.get_key("next").is_some_and(|node| node.is_integer())
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Preserve the intended scalar kind and decode failure on malformed numeric source.
  #[test]
  fn malformed_scalar_becomes_typed_invalid_node() -> Result<(), impl Debug> {
    ensure_that(
      dom_fixture("value = 999999999999999999999999999999\n"),
      "out-of-range integers must retain their typed invalid reason and semantic diagnostic",
      |fixture| {
        let &Ok((ref parsed, ref root, _)) = fixture else {
          return false;
        };
        let Some(node) = root.get_key("value") else {
          return false;
        };
        let Some(invalid) = node.as_invalid() else {
          return false;
        };
        parsed.diagnostics().is_empty()
          && matches!(invalid.reason(), InvalidReason::MalformedScalar(malformed) if malformed.kind() == ScalarKind::Integer)
          && invalid
            .errors()
            .iter()
            .any(|error| matches!(error, Diagnostic::MalformedScalar(_)))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Represent lookup absence as None and match globs against actual decoded keys.
  #[test]
  fn missing_lookup_is_none_and_glob_matches_real_keys() -> Result<(), impl Debug> {
    let observed = dom_fixture("alpha = 1\nbeta = 2\n").map(|fixture| {
      let matched = fixture.1.get_matches("a*").map(Iterator::collect::<Vec<_>>);
      (fixture, matched)
    });
    ensure_that(
      observed,
      "missing lookup must remain absent and globs must return actual decoded keys",
      |fixture| {
        let Ok(ref value) = *fixture else {
          return false;
        };

        value.0.0.diagnostics().is_empty()
          && value.0.1.get_key("missing").is_none()
          && value.1.as_ref().is_ok_and(|matched| {
            matched.len() == 1
              && matched
                .first()
                .is_some_and(|entry| entry.0.as_key().is_some_and(|key| key.value() == "alpha"))
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Validate glob syntax independently of document contents and honor exact-depth queries.
  #[test]
  fn path_globs_are_validated_before_traversal_and_respect_depth() -> Result<(), impl Debug> {
    let empty = dom_fixture("").map(|fixture| {
      let rejected = fixture
        .1
        .find_all_matches(&Keys::single(Key::new("[")), false)
        .map(Iterator::collect::<Vec<_>>);
      (fixture, rejected)
    });
    let nested = dom_fixture("a.b = 1\na.c.d = 2\n").map(|fixture| {
      let query = Keys::single(Key::new("a"));
      let exact = fixture.1.find_all_matches(&query, false).map(Iterator::collect::<Vec<_>>);
      let descendants = fixture.1.find_all_matches(&query, true).map(Iterator::collect::<Vec<_>>);
      (fixture, query, exact, descendants)
    });
    ensure_that(
      (empty, nested),
      "invalid globs must fail before traversal while exact and prefix queries retain their depths",
      |observed| {
        observed
          .0
          .as_ref()
          .is_ok_and(|value| value.0.0.diagnostics().is_empty() && matches!(value.1, Err(QueryError::InvalidGlob(_))))
          && observed.1.as_ref().is_ok_and(|value| {
            value.0.0.diagnostics().is_empty()
              && value.2.as_ref().is_ok_and(|matched| matched.len() == 1)
              && value.3.as_ref().is_ok_and(|matched| matched.len() == 4)
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Keep detached semantic values free of fabricated source provenance.
  #[test]
  fn detached_nodes_do_not_fabricate_source_ranges() -> Result<(), impl Debug> {
    let detached: Node = BoolInner {
      diagnostics: Arc::default(),
      syntax:      None,
      value:       true,
    }
    .into();
    let ranges = detached.text_ranges(true).collect::<Vec<_>>();
    ensure_that(
      (detached, ranges),
      "detached semantic nodes must not fabricate source ranges",
      |observed| observed.1.is_empty(),
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Preserve valid detached-key rendering and identity-based equality for invalid keys.
  #[test]
  fn detached_and_invalid_keys_preserve_valid_rendering_and_equality() -> Result<(), impl Debug> {
    let parent = Key::new("parent");
    let joined = parent.join(Key::new("child"));
    let expected = Keys::new([KeyOrIndex::Key(parent.clone()), KeyOrIndex::Key(Key::new("child"))].into_iter());
    let escaped = Key::new("can't\n");
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
    let mut invalid_keys = HashSet::new();
    let inserted = invalid_keys.insert(invalid.clone());
    ensure_that(
      (parent, joined, expected, escaped, invalid, same, distinct, invalid_keys, inserted),
      "detached rendering and invalid-key hashing must retain semantic and allocation identity",
      |observed| {
        observed.0.as_ref() == "parent"
          && observed.1 == observed.2
          && observed.3.to_string() == "\"can't\\n\""
          && observed.4 == observed.5
          && observed.4 != observed.6
          && observed.8
          && observed.7.contains(&observed.5)
          && !observed.7.contains(&observed.6)
      },
    )
    .map(drop)
    .map_err(Box::new)
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
  fn frozen_comments_are_shared_from_the_document_root() -> Result<(), impl Debug> {
    let observed = dom_fixture("# header\n#:schema memory://fixture\nvalue = 1 # trailing\n").map(|fixture| {
      let root_comments = fixture.1.comments().collect::<Vec<_>>();
      let table_comments = fixture.1.as_table().map(|table| table.comments().collect::<Vec<_>>());
      let scalar = fixture.1.get_key("value").map(|node| {
        let comments = node.comments().collect::<Vec<_>>();
        let headers = node.header_comments().collect::<Vec<_>>();
        (node, comments, headers)
      });
      let concrete = scalar
        .as_ref()
        .and_then(|scalar_parts| scalar_parts.0.as_integer())
        .map(|integer| {
          (
            integer.comments().collect::<Vec<_>>(),
            integer.header_comments().collect::<Vec<_>>(),
          )
        });
      (fixture, root_comments, table_comments, scalar, concrete)
    });
    ensure_that(
      observed,
      "all frozen node wrappers must share complete ordinary/directive comments and header selection",
      |fixture| {
        let &Ok((
          ref value,
          ref comments,
          Some(ref table_comments),
          Some((_, ref scalar_comments, ref scalar_headers)),
          Some((ref concrete_comments, ref concrete_headers)),
        )) = fixture
        else {
          return false;
        };
        value.0.diagnostics().is_empty()
          && comments
            .iter()
            .map(|comment| (comment.directive(), comment.value()))
            .collect::<Vec<_>>()
            == [(None, " header"), (Some("schema"), "memory://fixture"), (None, " trailing")]
          && table_comments.iter().map(Comment::to_string).collect::<Vec<_>>()
            == comments.iter().map(Comment::to_string).collect::<Vec<_>>()
          && scalar_comments.iter().map(Comment::to_string).collect::<Vec<_>>()
            == comments.iter().map(Comment::to_string).collect::<Vec<_>>()
          && scalar_headers.len() == 2
          && scalar_headers
            .iter()
            .zip([" header", "memory://fixture"])
            .all(|(comment, expected)| comment.value() == expected)
          && concrete_comments.iter().map(Comment::to_string).collect::<Vec<_>>()
            == comments.iter().map(Comment::to_string).collect::<Vec<_>>()
          && concrete_headers.iter().map(Comment::to_string).collect::<Vec<_>>()
            == scalar_headers.iter().map(Comment::to_string).collect::<Vec<_>>()
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Complete source fixture shared by the concrete wrapper contract tests.
  const CONCRETE_WRAPPER_SOURCE: &str =
    "# document\nboolean = true\nstring = \"value\"\nnegative = -2\npositive = 3\nbinary = 0b10\noctal = 0o10\nhexadecimal = 0x10\nfloat \
     = 1.5\noffset = 1979-05-27T07:32:00Z\nlocal = 1979-05-27T07:32:00\nlocal_fraction = 1979-05-27T07:32:00.5\ndate = 1979-05-27\ntime = \
     07:32:00\ntime_fraction = 07:32:00.5\narray = [1]\ninvalid = 999999999999999999999999999999\n[[tables]]\nname = \"first\"\n";

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

  /// Expose source metadata and decoded values through table, Boolean, and string wrappers.
  #[test]
  fn table_boolean_and_string_wrappers_preserve_source_contracts() -> Result<(), impl Debug> {
    ensure_that(
      dom_fixture(CONCRETE_WRAPPER_SOURCE),
      "table, Boolean, and string wrappers must retain decoded values, metadata, comments, and debug identity",
      |fixture| {
        let &Ok((ref parsed, ref root, _)) = fixture else {
          return false;
        };
        let Some(table) = root.as_table() else {
          return false;
        };
        let Some(boolean_node) = root.get_key("boolean") else {
          return false;
        };
        let Some(boolean) = boolean_node.as_bool() else {
          return false;
        };
        let Some(string_node) = root.get_key("string") else {
          return false;
        };
        let Some(string) = string_node.as_str() else {
          return false;
        };
        parsed.diagnostics().is_empty()
          && table.syntax().is_some()
          && table.errors().is_empty()
          && table.is_valid_node()
          && table.kind() == TableKind::Regular
          && table.header_comments().count() == 1
          && format!("{table:?}").contains("Table")
          && source_facts(&boolean_node) == (true, 0, true, 1, 1)
          && boolean.value()
          && format!("{boolean:?}").contains("Bool")
          && source_facts(&string_node) == (true, 0, true, 1, 1)
          && string.value() == "value"
          && format!("{string:?}").contains("Str")
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Preserve signed integer identity and each source radix representation.
  #[test]
  fn integer_wrappers_preserve_signedness_and_radix() -> Result<(), impl Debug> {
    ensure_that(
      dom_fixture(CONCRETE_WRAPPER_SOURCE),
      "integer wrappers must retain signedness, radix, source metadata, and decoded values",
      |fixture| {
        let &Ok((ref parsed, ref root, _)) = fixture else {
          return false;
        };
        let Some(negative_node) = root.get_key("negative") else {
          return false;
        };
        let Some(negative) = negative_node.as_integer() else {
          return false;
        };
        let Some(positive_node) = root.get_key("positive") else {
          return false;
        };
        let Some(positive) = positive_node.as_integer() else {
          return false;
        };
        parsed.diagnostics().is_empty()
          && negative.representation() == IntegerRepr::Dec
          && negative.value().is_negative()
          && !negative.value().is_positive()
          && negative.value().as_negative() == Some(-2)
          && negative.value().as_positive().is_none()
          && negative.value().to_string() == "-2"
          && negative.syntax().is_some()
          && negative.errors().is_empty()
          && negative.is_valid_node()
          && format!("{negative:?}").contains("Integer")
          && positive.representation() == IntegerRepr::Dec
          && positive.value().is_positive()
          && !positive.value().is_negative()
          && positive.value().as_positive() == Some(3)
          && positive.value().as_negative().is_none()
          && positive.value().to_string() == "3"
          && [
            ("binary", IntegerRepr::Bin, 2_u64),
            ("octal", IntegerRepr::Oct, 8_u64),
            ("hexadecimal", IntegerRepr::Hex, 16_u64),
          ]
          .into_iter()
          .all(|(key, repr, expected)| {
            matches!(root.get_key(key), Some(ref node) if node.as_integer().is_some_and(|integer|
              integer.representation() == repr && integer.value() == IntegerValue::Positive(expected)))
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Preserve floating-point metadata and every supported date/time rendering.
  #[test]
  fn float_and_date_time_wrappers_preserve_metadata_and_rendering() -> Result<(), impl Debug> {
    ensure_that(
      dom_fixture(CONCRETE_WRAPPER_SOURCE),
      "float and date-time wrappers must preserve decoded values, metadata, comments, and semantic rendering",
      |fixture| {
        let &Ok((ref parsed, ref root, _)) = fixture else {
          return false;
        };
        let Some(float_node) = root.get_key("float") else {
          return false;
        };
        let Some(float) = float_node.as_float() else {
          return false;
        };
        parsed.diagnostics().is_empty()
          && source_facts(&float_node) == (true, 0, true, 1, 1)
          && float.value().partial_cmp(&1.5) == Some(Ordering::Equal)
          && format!("{float:?}").contains("Float")
          && [
            ("offset", "1979-05-27T07:32:00Z"),
            ("local", "1979-05-27T07:32:00"),
            ("local_fraction", "1979-05-27T07:32:00.5"),
            ("date", "1979-05-27"),
            ("time", "07:32:00"),
            ("time_fraction", "07:32:00.5"),
          ]
          .into_iter()
          .all(|(key, expected)| {
            matches!(root.get_key(key), Some(ref node) if source_facts(node) == (true, 0, true, 1, 1)
              && node.as_date().is_some_and(|date| date.value().to_string() == expected && format!("{date:?}").contains("DateTime")))
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Normalize each supported alternate date/time spelling without changing its semantic value.
  #[test]
  fn date_time_decoding_normalizes_alternate_spellings() -> Result<(), impl Debug> {
    ensure_that(
      dom_fixture("space = 1979-05-27 07:32:00z\nlower = 1979-05-27t07:32:00z\ncomma = 07:32:00,5\n"),
      "alternate date-time spellings must normalize without changing semantic values",
      |fixture| {
        let Ok(ref value) = *fixture else {
          return false;
        };

        value.0.diagnostics().is_empty()
          && [
            ("space", "1979-05-27T07:32:00Z"),
            ("lower", "1979-05-27T07:32:00Z"),
            ("comma", "07:32:00.5"),
          ]
          .into_iter()
          .all(|(key, expected)| {
            matches!(value.1.get_key(key), Some(ref node) if node.as_date().is_some_and(|date| date.value().to_string() == expected))
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Distinguish inline arrays from arrays of tables through both kind polarities.
  #[test]
  fn array_wrappers_preserve_inline_and_table_polarities() -> Result<(), impl Debug> {
    ensure_that(
      dom_fixture(CONCRETE_WRAPPER_SOURCE),
      "array wrappers must retain representation, source metadata, and opposite kind polarities",
      |fixture| {
        let &Ok((ref parsed, ref root, _)) = fixture else {
          return false;
        };
        let Some(inline_node) = root.get_key("array") else {
          return false;
        };
        let Some(inline) = inline_node.as_array() else {
          return false;
        };
        let Some(tables_node) = root.get_key("tables") else {
          return false;
        };
        let Some(tables) = tables_node.as_array() else {
          return false;
        };
        parsed.diagnostics().is_empty()
          && source_facts(&inline_node) == (true, 0, true, 1, 1)
          && inline.kind() == ArrayKind::Inline
          && inline.kind().is_inline()
          && !inline.kind().is_tables()
          && format!("{inline:?}").contains("Array")
          && tables.kind() == ArrayKind::Tables
          && tables.kind().is_tables()
          && !tables.kind().is_inline()
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Preserve malformed scalar provenance, diagnostics, and reader-facing type names.
  #[test]
  fn invalid_wrappers_preserve_typed_decoder_failures() -> Result<(), impl Debug> {
    let fixture_kinds = [
      ScalarKind::Bool,
      ScalarKind::String,
      ScalarKind::Integer,
      ScalarKind::Float,
      ScalarKind::DateTime,
    ];
    ensure_that((dom_fixture(CONCRETE_WRAPPER_SOURCE), fixture_kinds), "invalid wrappers must retain native decoder evidence and every scalar family's reader-facing name", |observed| {
      let &(Ok((ref parsed, ref root, _)), ref kinds) = observed else { return false; };
      let Some(node) = root.get_key("invalid") else { return false; };
      let Some(invalid) = node.as_invalid() else { return false; };
      parsed.diagnostics().is_empty()
        && matches!(invalid.reason(), InvalidReason::MalformedScalar(malformed) if malformed.kind() == ScalarKind::Integer
          && matches!(malformed.failure(), DecodeFailure::InvalidValue { .. }) && invalid.syntax().is_some_and(|syntax| syntax.text_range() == malformed.syntax().text_range()))
        && !invalid.errors().is_empty() && !invalid.is_valid_node() && invalid.comments().count() == 1 && invalid.header_comments().count() == 1 && format!("{invalid:?}").contains("Invalid")
        && kinds.map(|kind| (kind.as_str(), kind.to_string())) == [
          ("Boolean", String::from("Boolean")), ("string", String::from("string")), ("integer", String::from("integer")), ("floating-point", String::from("floating-point")), ("date-time", String::from("date-time")),
        ]
    }).map(drop).map_err(Box::new)
  }
}
