//! Two-phase construction from lossless syntax into an immutable semantic DOM.
//!
//! `DomBuilder` owns every mutable arena node, conflict decision, key occurrence, and diagnostic.
//! Publication initializes preallocated immutable identities directly and shares one document
//! comment index.

use std::collections::VecDeque;
use std::mem::take;
use std::sync::Arc;

use rowan::GreenNode;
use rowan::WalkEvent;
use time::Date;
use time::OffsetDateTime;
use time::PrimitiveDateTime;
use time::Time;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;

use super::CommentStore;
use super::error::Diagnostic;
use super::node::ArenaEntries;
use super::node::ArenaEntry;
use super::node::ArrayInner;
use super::node::ArrayKind;
use super::node::BoolInner;
use super::node::DateTimeInner;
use super::node::DateTimeValue;
use super::node::DecodeFailure;
use super::node::DomArena;
use super::node::FloatInner;
use super::node::IntegerInner;
use super::node::IntegerRepr;
use super::node::IntegerValue;
use super::node::InvalidInner;
use super::node::InvalidReason;
use super::node::Key;
use super::node::KeyInner;
use super::node::MalformedScalar;
use super::node::Node;
use super::node::NodeId as ArenaNodeId;
use super::node::NodeSeed;
use super::node::ScalarKind;
use super::node::StrInner;
use super::node::StrRepr;
use super::node::TableInner;
use super::node::TableKind;
use crate::HashMap;
use crate::syntax::SyntaxElement;
use crate::syntax::SyntaxKind;
use crate::syntax::kind::ARRAY;
use crate::syntax::kind::BOOL;
use crate::syntax::kind::DATE;
use crate::syntax::kind::DATE_TIME_LOCAL;
use crate::syntax::kind::DATE_TIME_OFFSET;
use crate::syntax::kind::ENTRY;
use crate::syntax::kind::FLOAT;
use crate::syntax::kind::IDENT;
use crate::syntax::kind::INLINE_TABLE;
use crate::syntax::kind::INTEGER;
use crate::syntax::kind::INTEGER_BIN;
use crate::syntax::kind::INTEGER_HEX;
use crate::syntax::kind::INTEGER_OCT;
use crate::syntax::kind::MULTI_LINE_STRING;
use crate::syntax::kind::MULTI_LINE_STRING_LITERAL;
use crate::syntax::kind::ROOT;
use crate::syntax::kind::STRING;
use crate::syntax::kind::STRING_LITERAL;
use crate::syntax::kind::TABLE_ARRAY_HEADER;
use crate::syntax::kind::TABLE_HEADER;
use crate::syntax::kind::TIME;
use crate::syntax::kind::VALUE;
use crate::util::unescape;

/// Stable identity shared by one mutable build record and its write-once immutable record.
#[derive(Clone, Debug)]
struct NodeId {
  /// Mutable construction-arena index.
  index: usize,
  /// Preallocated immutable arena identity.
  arena: ArenaNodeId,
}

/// One mutable node retained in the construction arena.
#[derive(Debug)]
enum BuildNode {
  /// A mutable table.
  Table(BuildTable),
  /// A mutable array.
  Array(BuildArray),
  /// A mutable scalar or malformed leaf.
  Leaf(BuildLeaf),
}

/// Define one mutable container record retained until arena publication.
macro_rules! define_build_container {
  (
    $(#[$metadata:meta])*
    $name:ident,
    kind = $kind:ty;
    $(
      $(#[$field_metadata:meta])*
      $field:ident: $field_type:ty
    ),+ $(,)?
  ) => {
    $(#[$metadata])*
    #[derive(Debug)]
    struct $name {
      /// Diagnostics attached directly to the container.
      diagnostics: Vec<Diagnostic>,
      /// Source anchor for the container.
      syntax:      Option<SyntaxElement>,
      /// Semantic container representation.
      kind:        $kind,
      $(
        $(#[$field_metadata])*
        $field: $field_type,
      )+
    }
  };
}

define_build_container! {
  /// Mutable table state used while resolving keys and conflicts.
  BuildTable,
  kind = TableKind;
  /// Whether this table originated from a header.
  header: bool,
  /// Mutable ordered entries and their decoded-key winner index.
  entries: BuildEntries,
}

/// One mutable table entry.
#[derive(Debug)]
struct BuildEntry {
  /// Decoded key.
  key:  BuildKey,
  /// Arena identifier of the value.
  node: NodeId,
}

/// Mutable table-entry owner used until the semantic graph is frozen.
#[derive(Debug, Default)]
struct BuildEntries {
  /// Every entry in source insertion order, including conflicts.
  ordered: Vec<BuildEntry>,
  /// Latest ordered-entry index for each valid decoded key.
  winners: HashMap<Arc<str>, usize>,
}

impl BuildEntries {
  /// Return the latest valid entry matching `key`.
  fn winner(&self, key: &BuildKey) -> Option<(usize, NodeId)> {
    if !key.is_valid {
      return None;
    }
    let index = *self.winners.get(&key.value)?;
    self.ordered.get(index).map(|entry| (index, entry.node.clone()))
  }

  /// Borrow the key stored at one ordered-entry index.
  fn key(&self, index: usize) -> Option<&BuildKey> {
    self.ordered.get(index).map(|entry| &entry.key)
  }

  /// Attach another source occurrence to an existing semantic key.
  fn attach_occurrence(&mut self, index: usize, pending_occurrence: Option<SyntaxElement>) {
    let Some(occurrence) = pending_occurrence else {
      return;
    };
    if let Some(entry) = self.ordered.get_mut(index) {
      entry.key.additional_syntaxes.push(occurrence);
    }
  }

  /// Append one historical entry and update the valid-key winner index.
  fn append(&mut self, entry: BuildEntry) {
    let index = self.ordered.len();
    if entry.key.is_valid {
      if let Some(winner) = self.winners.get_mut(&entry.key.value) {
        *winner = index;
      } else {
        self.winners.extend([(Arc::clone(&entry.key.value), index)]);
      }
    }
    self.ordered.push(entry);
  }

  /// Transfer every ordered entry out of this mutable owner.
  fn take_ordered(&mut self) -> Vec<BuildEntry> {
    take(self).ordered
  }

  /// Freeze ordered entries and rebuild the latest-winner lookup from source order.
  fn freeze(self) -> ArenaEntries {
    let mut lookup = HashMap::default();
    let mut all = Vec::with_capacity(self.ordered.len());
    for (index, entry) in self.ordered.into_iter().enumerate() {
      let key = entry.key.freeze();
      if entry.key.is_valid {
        lookup.extend([(key.clone(), index)]);
      }
      all.push(ArenaEntry {
        key,
        node: entry.node.arena,
      });
    }
    ArenaEntries {
      lookup,
      all: all.into(),
    }
  }
}

/// Mutable decoded key state.
#[derive(Debug)]
struct BuildKey {
  /// Diagnostics attached directly to the key.
  diagnostics:         Vec<Diagnostic>,
  /// Primary source anchor.
  syntax:              Option<SyntaxElement>,
  /// Whether the key can participate in equality and lookup.
  is_valid:            bool,
  /// Decoded key value.
  value:               Arc<str>,
  /// Other source occurrences merged into this semantic key.
  additional_syntaxes: Vec<SyntaxElement>,
}

impl BuildKey {
  /// Freeze this key into its public immutable representation.
  fn freeze(&self) -> Key {
    KeyInner {
      diagnostics:         self.diagnostics.clone().into(),
      syntax:              self.syntax.clone(),
      is_valid:            self.is_valid,
      value:               Arc::clone(&self.value),
      additional_syntaxes: self.additional_syntaxes.clone().into(),
    }
    .wrap()
  }
}

define_build_container! {
  /// Mutable array state.
  BuildArray,
  kind = ArrayKind;
  /// Ordered arena identifiers of array items.
  items: Vec<NodeId>,
}

/// Mutable leaf state.
#[derive(Debug)]
struct BuildLeaf {
  /// Diagnostics attached directly to the leaf.
  diagnostics: Vec<Diagnostic>,
  /// Source anchor for the leaf.
  syntax:      Option<SyntaxElement>,
  /// Decoded leaf value or invalid reason.
  value:       BuildLeafValue,
}

/// Decoded leaf value retained until the graph freezes.
#[derive(Debug)]
enum BuildLeafValue {
  /// A Boolean value.
  Bool(bool),
  /// A decoded string value.
  String(Arc<str>),
  /// An integer value and its representation.
  Integer {
    /// Source representation.
    repr:  IntegerRepr,
    /// Decoded value.
    value: IntegerValue,
  },
  /// A floating-point value.
  Float(f64),
  /// A date or time value.
  DateTime(DateTimeValue),
  /// Malformed source.
  Invalid(InvalidReason),
}

impl BuildNode {
  /// Borrow this record as a table.
  #[allow(
    clippy::single_call_fn,
    reason = "the named projection completes BuildNode's symmetric borrow API and drives arena table lookup"
  )]
  const fn as_table(&self) -> Option<&BuildTable> {
    match *self {
      Self::Table(ref table) => Some(table),
      Self::Array(_) | Self::Leaf(_) => None,
    }
  }

  /// Mutably borrow this record as a table.
  #[allow(
    clippy::single_call_fn,
    reason = "the named projection completes BuildNode's symmetric borrow API and drives mutable arena table lookup"
  )]
  const fn as_table_mut(&mut self) -> Option<&mut BuildTable> {
    match *self {
      Self::Table(ref mut table) => Some(table),
      Self::Array(_) | Self::Leaf(_) => None,
    }
  }

  /// Borrow this record as an array.
  const fn as_array(&self) -> Option<&BuildArray> {
    match *self {
      Self::Array(ref array) => Some(array),
      Self::Table(_) | Self::Leaf(_) => None,
    }
  }

  /// Mutably borrow this record as an array.
  #[allow(
    clippy::single_call_fn,
    reason = "the named projection completes BuildNode's symmetric borrow API and drives mutable arena array lookup"
  )]
  const fn as_array_mut(&mut self) -> Option<&mut BuildArray> {
    match *self {
      Self::Array(ref mut array) => Some(array),
      Self::Table(_) | Self::Leaf(_) => None,
    }
  }

  /// Mutably borrow the diagnostic collection shared by every build variant.
  const fn diagnostics_mut(&mut self) -> &mut Vec<Diagnostic> {
    match *self {
      Self::Table(ref mut table) => &mut table.diagnostics,
      Self::Array(ref mut array) => &mut array.diagnostics,
      Self::Leaf(ref mut leaf) => &mut leaf.diagnostics,
    }
  }
}

/// Private mutable construction arena.
#[derive(Debug, Default)]
struct DomBuilder {
  /// All mutable nodes, addressed by stable insertion index.
  nodes:      Vec<BuildNode>,
  /// Preallocated immutable identities in construction order.
  identities: Vec<ArenaNodeId>,
  /// Immutable comment index published into every frozen node.
  comments:   Arc<CommentStore>,
}

/// Destination for one value produced by the iterative syntax walk.
#[derive(Debug)]
enum BuildAttachment {
  /// Append the value to an array.
  Array(NodeId),
  /// Insert the value beneath decoded entry keys.
  Entry {
    /// Destination table.
    table:  NodeId,
    /// Decoded dotted keys.
    keys:   Vec<BuildKey>,
    /// Entry syntax used when no key is present.
    syntax: SyntaxElement,
  },
}

/// Header leaf behavior selected after shared dotted-path traversal.
#[derive(Clone, Copy, Debug)]
enum HeaderKind {
  /// Define or merge one regular table.
  Table,
  /// Append one regular table to an array of tables.
  ArrayTable,
}

/// One pending iterative DOM-construction task.
#[derive(Debug)]
struct BuildTask {
  /// Syntax element to convert.
  syntax:     SyntaxElement,
  /// Destination for the converted value.
  attachment: BuildAttachment,
}

impl DomBuilder {
  /// Insert one node and return its stable arena identifier.
  fn insert(&mut self, node: BuildNode) -> NodeId {
    let index = self.nodes.len();
    let arena = ArenaNodeId::pending(index);
    self.nodes.push(node);
    self.identities.push(arena.clone());
    NodeId {
      index,
      arena,
    }
  }

  /// Insert one table.
  fn insert_table(&mut self, syntax: Option<SyntaxElement>, header: bool, kind: TableKind) -> NodeId {
    self.insert(BuildNode::Table(BuildTable {
      diagnostics: Vec::new(),
      syntax,
      header,
      kind,
      entries: BuildEntries::default(),
    }))
  }

  /// Insert one array.
  fn insert_array(&mut self, syntax: Option<SyntaxElement>, kind: ArrayKind) -> NodeId {
    self.insert(BuildNode::Array(BuildArray {
      diagnostics: Vec::new(),
      syntax,
      kind,
      items: Vec::new(),
    }))
  }

  /// Insert one decoded or malformed leaf.
  fn insert_leaf(&mut self, syntax: Option<SyntaxElement>, diagnostics: Vec<Diagnostic>, leaf_value: BuildLeafValue) -> NodeId {
    self.insert(BuildNode::Leaf(BuildLeaf {
      diagnostics,
      syntax,
      value: leaf_value,
    }))
  }

  /// Convert one syntax element into mutable DOM state.
  fn node_from_syntax(&mut self, source_syntax: SyntaxElement) -> NodeId {
    let mut tasks = Vec::new();
    let root = self.build_node(source_syntax, &mut tasks);

    while let Some(task) = tasks.pop() {
      let BuildTask {
        syntax: task_syntax,
        attachment,
      } = task;
      let node = self.build_node(task_syntax, &mut tasks);
      self.attach(node, attachment);
    }
    root
  }

  /// Convert one concrete syntax element and queue its direct descendants.
  fn build_node(&mut self, mut syntax: SyntaxElement, tasks: &mut Vec<BuildTask>) -> NodeId {
    while syntax.kind() == VALUE {
      let Some(child) = syntax.as_node().and_then(rowan::SyntaxNode::first_child_or_token) else {
        return self.invalid_unexpected(syntax);
      };
      syntax = child;
    }

    match syntax.kind() {
      ARRAY => {
        let array = self.insert_array(Some(syntax.clone()), ArrayKind::Inline);
        let children = syntax
          .as_node()
          .map(|node| node.children().map(Into::into).collect::<Vec<SyntaxElement>>())
          .unwrap_or_default();
        tasks.extend(children.into_iter().rev().map(|child| BuildTask {
          syntax:     child,
          attachment: BuildAttachment::Array(array.clone()),
        }));
        array
      }
      INLINE_TABLE => self.inline_table_from_syntax(&syntax, tasks),
      ROOT => self.root_from_syntax(syntax),
      STRING | MULTI_LINE_STRING | STRING_LITERAL | MULTI_LINE_STRING_LITERAL => self.string_from_syntax(syntax),
      INTEGER | INTEGER_HEX | INTEGER_OCT | INTEGER_BIN => self.integer_from_syntax(syntax),
      FLOAT => self.float_from_syntax(syntax),
      BOOL => self.bool_from_syntax(syntax),
      DATE_TIME_OFFSET | DATE_TIME_LOCAL | DATE | TIME => self.date_time_from_syntax(syntax),
      _ => self.invalid_unexpected(syntax),
    }
  }

  /// Allocate one inline table and queue its entries in reverse traversal order.
  fn inline_table_from_syntax(&mut self, syntax: &SyntaxElement, tasks: &mut Vec<BuildTask>) -> NodeId {
    let table = self.insert_table(Some(syntax.clone()), false, TableKind::Inline);
    let entries = syntax
      .as_node()
      .map(|node| node.children().map(Into::into).collect::<Vec<SyntaxElement>>())
      .unwrap_or_default();
    for entry in entries.into_iter().rev() {
      let (keys, value_syntax) = entry_parts(&entry);
      let child_syntax = value_syntax.unwrap_or_else(|| entry.clone());
      tasks.push(BuildTask {
        syntax:     child_syntax,
        attachment: BuildAttachment::Entry {
          table: table.clone(),
          keys,
          syntax: entry,
        },
      });
    }
    table
  }

  /// Attach one completed value to its construction destination.
  fn attach(&mut self, node: NodeId, attachment: BuildAttachment) {
    match attachment {
      BuildAttachment::Array(array) => self.push_array_item(&array, node),
      BuildAttachment::Entry {
        table,
        keys,
        syntax,
      } => self.insert_entry_path(&table, keys, syntax, node),
    }
  }

  /// Insert a completed value beneath one decoded dotted-key path.
  fn insert_entry_path(&mut self, table: &NodeId, path: Vec<BuildKey>, syntax: SyntaxElement, entry_node: NodeId) {
    let mut segments = path.into_iter();
    let first = segments.next().unwrap_or_else(|| invalid_key(syntax));
    let remaining = segments.collect::<Vec<_>>();
    if remaining.is_empty() {
      self.add_entry(table, first, entry_node);
      return;
    }

    let top = self.dotted_entry_value(&first, remaining, &entry_node);
    self.add_entry(table, first, top);
  }

  /// Build the pseudo-table chain below the first segment of one dotted entry.
  fn dotted_entry_value(&mut self, first: &BuildKey, remaining: Vec<BuildKey>, entry_node: &NodeId) -> NodeId {
    let top = self.insert_table(first.syntax.clone(), false, TableKind::Pseudo);
    let mut current = top.clone();
    let mut segments = remaining.into_iter().peekable();
    while let Some(key) = segments.next() {
      if segments.peek().is_none() {
        self.add_entry(&current, key, entry_node.clone());
      } else {
        let child = self.insert_table(key.syntax.clone(), false, TableKind::Pseudo);
        self.add_entry(&current, key, child.clone());
        current = child;
      }
    }
    top
  }

  /// Convert a document root and resolve its header state transitions.
  fn root_from_syntax(&mut self, syntax: SyntaxElement) -> NodeId {
    let root = self.insert_table(Some(syntax.clone()), false, TableKind::Regular);
    let Some(root_syntax) = syntax.as_node() else {
      self.add_diagnostic(&root, Diagnostic::UnexpectedSyntax {
        syntax,
      });
      return root;
    };

    let mut current = root.clone();
    for child in root_syntax.children() {
      match child.kind() {
        TABLE_HEADER => {
          current = self.resolve_header(&root, &child.into(), HeaderKind::Table);
        }
        TABLE_ARRAY_HEADER => {
          current = self.resolve_header(&root, &child.into(), HeaderKind::ArrayTable);
        }
        ENTRY => {
          let (key, entry_node) = self.entry_from_syntax(&child.into());
          self.add_entry(&current, key, entry_node);
        }
        _ => {}
      }
    }
    root
  }

  /// Traverse one header's dotted path and apply its selected leaf behavior.
  fn resolve_header(&mut self, root: &NodeId, syntax: &SyntaxElement, kind: HeaderKind) -> NodeId {
    let mut current = root.clone();
    let mut keys = header_keys(syntax).into_iter().peekable();
    while let Some(key) = keys.next() {
      if keys.peek().is_some() {
        current = self.merge_intermediate(&current, key);
        continue;
      }

      current = match kind {
        HeaderKind::Table => self.resolve_table_leaf(&current, key, syntax),
        HeaderKind::ArrayTable => self.resolve_array_table_leaf(&current, key, syntax),
      };
    }
    current
  }

  /// Define or merge the terminal segment of one regular table header.
  fn resolve_table_leaf(&mut self, parent: &NodeId, key: BuildKey, syntax: &SyntaxElement) -> NodeId {
    let Some((entry_index, existing_node)) = self.find_entry(parent, &key) else {
      let new_table = self.insert_table(Some(syntax.clone()), true, TableKind::Regular);
      self.add_entry(parent, key, new_table.clone());
      return new_table;
    };
    if self.resolve_existing_table_header(parent, entry_index, &existing_node, &key, syntax) {
      existing_node
    } else {
      parent.clone()
    }
  }

  /// Resolve an explicit table header against one pre-existing entry.
  #[allow(
    clippy::single_call_fn,
    reason = "the helper isolates table-header conflict policy from dotted-path traversal while preserving the existing-node continuation \
              decision"
  )]
  fn resolve_existing_table_header(
    &mut self,
    current: &NodeId,
    entry_index: usize,
    existing_node: &NodeId,
    key: &BuildKey,
    syntax: &SyntaxElement,
  ) -> bool {
    self.add_key_occurrence(current, entry_index, key.syntax.clone());
    let Some((kind, header)) = self.table_state(existing_node) else {
      self.add_diagnostic(current, Diagnostic::ConflictingKeys {
        key:   key.freeze(),
        other: self
          .entry_key(current, entry_index)
          .map_or_else(|| key.freeze(), BuildKey::freeze),
      });
      return false;
    };

    if kind == TableKind::Pseudo && header {
      self.define_implicit_header(existing_node, syntax.clone());
    } else {
      self.add_diagnostic(existing_node, Diagnostic::ConflictingKeys {
        key:   key.freeze(),
        other: self
          .entry_key(current, entry_index)
          .map_or_else(|| key.freeze(), BuildKey::freeze),
      });
    }
    true
  }

  /// Materialize one header-created pseudo table as an explicitly defined table.
  fn define_implicit_header(&mut self, table_id: &NodeId, syntax: SyntaxElement) {
    if let Some(table) = self.table_mut(table_id) {
      table.syntax = Some(syntax);
      table.header = true;
      table.kind = TableKind::Regular;
    }
  }

  /// Append the terminal segment of one array-of-tables header.
  fn resolve_array_table_leaf(&mut self, parent: &NodeId, key: BuildKey, syntax: &SyntaxElement) -> NodeId {
    let new_table = self.insert_table(Some(syntax.clone()), true, TableKind::Regular);
    let Some((entry_index, existing_node)) = self.find_entry(parent, &key) else {
      let array = self.insert_array(Some(syntax.clone()), ArrayKind::Tables);
      self.push_array_item(&array, new_table.clone());
      self.add_entry(parent, key, array);
      return new_table;
    };
    if self.extend_array_table(parent, entry_index, &existing_node, new_table.clone(), &key) {
      new_table
    } else {
      parent.clone()
    }
  }

  /// Append a table to one existing array while retaining kind diagnostics.
  #[allow(
    clippy::single_call_fn,
    reason = "the helper owns the array-of-tables continuation decision and its kind diagnostic independently of header path traversal"
  )]
  fn extend_array_table(
    &mut self,
    current: &NodeId,
    entry_index: usize,
    existing_node: &NodeId,
    new_table: NodeId,
    key: &BuildKey,
  ) -> bool {
    self.add_key_occurrence(current, entry_index, key.syntax.clone());
    let Some(kind) = self.array_kind(existing_node) else {
      self.add_expected_array_diagnostic(existing_node, current, entry_index, key);
      return false;
    };
    if kind != ArrayKind::Tables {
      self.add_expected_array_diagnostic(existing_node, current, entry_index, key);
    }
    self.push_array_item(existing_node, new_table);
    true
  }

  /// Attach the standard array-of-tables expectation diagnostic.
  #[allow(
    clippy::single_call_fn,
    reason = "the helper keeps both invalid existing-entry branches on the same typed array-of-tables diagnostic contract"
  )]
  fn add_expected_array_diagnostic(&mut self, diagnostic_node: &NodeId, current: &NodeId, entry_index: usize, key: &BuildKey) {
    self.add_diagnostic(diagnostic_node, Diagnostic::ExpectedArrayOfTables {
      not_array_of_tables: self
        .entry_key(current, entry_index)
        .map_or_else(|| key.freeze(), BuildKey::freeze),
      required_by:         key.freeze(),
    });
  }

  /// Merge or create one intermediate dotted-key table.
  fn merge_intermediate(&mut self, current: &NodeId, key: BuildKey) -> NodeId {
    let new_table = self.insert_table(key.syntax.clone(), true, TableKind::Pseudo);
    let Some((entry_index, existing_node)) = self.find_entry(current, &key) else {
      self.add_entry(current, key, new_table.clone());
      return new_table;
    };

    self.add_key_occurrence(current, entry_index, key.syntax.clone());
    if let Some((kind, _)) = self.table_state(&existing_node) {
      if !matches!(kind, TableKind::Regular | TableKind::Pseudo) {
        self.add_diagnostic(&existing_node, Diagnostic::ExpectedTable {
          not_table:   self
            .entry_key(current, entry_index)
            .map_or_else(|| key.freeze(), BuildKey::freeze),
          required_by: key.freeze(),
        });
      }
      return existing_node;
    }

    if let Some(kind) = self.array_kind(&existing_node) {
      if kind != ArrayKind::Tables {
        self.add_diagnostic(&existing_node, Diagnostic::ExpectedArrayOfTables {
          not_array_of_tables: self
            .entry_key(current, entry_index)
            .map_or_else(|| key.freeze(), BuildKey::freeze),
          required_by:         key.freeze(),
        });
      }
      if let Some(last_table) = self.last_array_table(&existing_node) {
        return last_table;
      }
      self.push_array_item(&existing_node, new_table.clone());
      return new_table;
    }

    self.add_diagnostic(current, Diagnostic::ExpectedTable {
      not_table:   self
        .entry_key(current, entry_index)
        .map_or_else(|| key.freeze(), BuildKey::freeze),
      required_by: key.freeze(),
    });
    self.add_entry(current, key, new_table.clone());
    new_table
  }

  /// Convert one key/value entry, introducing pseudo tables for dotted keys.
  fn entry_from_syntax(&mut self, syntax: &SyntaxElement) -> (BuildKey, NodeId) {
    let (path, pending_value) = entry_parts(syntax);
    let mut segments = path.into_iter();
    let first = segments.next().unwrap_or_else(|| invalid_key(syntax.clone()));
    let entry_node = match pending_value {
      Some(value_syntax) => self.node_from_syntax(value_syntax),
      None => self.invalid_unexpected(syntax.clone()),
    };

    let remaining = segments.collect::<Vec<_>>();
    if remaining.is_empty() {
      return (first, entry_node);
    }

    let top = self.dotted_entry_value(&first, remaining, &entry_node);
    (first, top)
  }

  /// Add an entry while preserving history and collecting semantic conflicts.
  fn add_entry(&mut self, table: &NodeId, key: BuildKey, node: NodeId) {
    if let Some((entry_index, existing_node)) = self.find_entry(table, &key) {
      let merge_pseudo = matches!(self.table_state(&existing_node), Some((TableKind::Pseudo, _)))
        && matches!(self.table_state(&node), Some((TableKind::Pseudo, _)));
      if merge_pseudo {
        self.merge_pseudo_entries(table, entry_index, &existing_node, &node);
        return;
      }

      let other = self
        .entry_key(table, entry_index)
        .map_or_else(|| key.freeze(), BuildKey::freeze);
      self.add_diagnostic(table, Diagnostic::ConflictingKeys {
        key: key.freeze(),
        other,
      });
    }

    if let Some(build_table) = self.table_mut(table) {
      build_table.entries.append(BuildEntry {
        key,
        node,
      });
    }
  }

  /// Merge all ordered entries from a newly built pseudo table into its existing owner.
  #[allow(
    clippy::single_call_fn,
    reason = "the helper preserves pseudo-table occurrence history and recursive conflict handling as one merge operation"
  )]
  fn merge_pseudo_entries(&mut self, table: &NodeId, entry_index: usize, existing_node: &NodeId, node: &NodeId) {
    let merged_entries = self
      .table_mut(node)
      .map(|new_table| new_table.entries.take_ordered())
      .unwrap_or_default();
    for entry in merged_entries {
      self.add_key_occurrence(table, entry_index, entry.key.syntax.clone());
      self.add_entry(existing_node, entry.key, entry.node);
    }
  }

  /// Find the lookup-winning entry for one key.
  fn find_entry(&self, table: &NodeId, key: &BuildKey) -> Option<(usize, NodeId)> {
    self.table(table)?.entries.winner(key)
  }

  /// Borrow one construction table.
  fn table(&self, id: &NodeId) -> Option<&BuildTable> {
    self.nodes.get(id.index).and_then(BuildNode::as_table)
  }

  /// Mutably borrow one construction table.
  fn table_mut(&mut self, id: &NodeId) -> Option<&mut BuildTable> {
    self.nodes.get_mut(id.index).and_then(BuildNode::as_table_mut)
  }

  /// Return table kind/header metadata for one node.
  fn table_state(&self, id: &NodeId) -> Option<(TableKind, bool)> {
    self.table(id).map(|table| (table.kind, table.header))
  }

  /// Return one array's kind.
  fn array_kind(&self, id: &NodeId) -> Option<ArrayKind> {
    self.nodes.get(id.index).and_then(BuildNode::as_array).map(|array| array.kind)
  }

  /// Return the final table in an array, if present.
  fn last_array_table(&self, id: &NodeId) -> Option<NodeId> {
    self
      .nodes
      .get(id.index)
      .and_then(BuildNode::as_array)?
      .items
      .last()
      .cloned()
      .filter(|candidate| self.table_state(candidate).is_some())
  }

  /// Append one item to a construction array.
  fn push_array_item(&mut self, array_id: &NodeId, child: NodeId) {
    if let Some(array) = self.nodes.get_mut(array_id.index).and_then(BuildNode::as_array_mut) {
      array.items.push(child);
    }
  }

  /// Borrow one entry key.
  fn entry_key(&self, table: &NodeId, entry_index: usize) -> Option<&BuildKey> {
    self.table(table)?.entries.key(entry_index)
  }

  /// Record another source occurrence on one semantic key.
  fn add_key_occurrence(&mut self, table_id: &NodeId, entry_index: usize, syntax: Option<SyntaxElement>) {
    if let Some(table) = self.table_mut(table_id) {
      table.entries.attach_occurrence(entry_index, syntax);
    }
  }

  /// Attach one semantic diagnostic to any mutable node.
  fn add_diagnostic(&mut self, id: &NodeId, diagnostic: Diagnostic) {
    if let Some(node) = self.nodes.get_mut(id.index) {
      node.diagnostics_mut().push(diagnostic);
    }
  }

  /// Insert an unexpected-syntax invalid node.
  fn invalid_unexpected(&mut self, syntax: SyntaxElement) -> NodeId {
    self.insert_leaf(
      Some(syntax.clone()),
      Vec::from([Diagnostic::UnexpectedSyntax {
        syntax: syntax.clone()
      }]),
      BuildLeafValue::Invalid(InvalidReason::UnexpectedSyntax {
        syntax,
      }),
    )
  }

  /// Insert a malformed-scalar invalid node.
  fn invalid_scalar(&mut self, syntax: SyntaxElement, kind: ScalarKind, failure: DecodeFailure) -> NodeId {
    let malformed = MalformedScalar::new(kind, failure, syntax.clone());
    self.insert_leaf(
      Some(syntax),
      Vec::from([Diagnostic::MalformedScalar(malformed.clone())]),
      BuildLeafValue::Invalid(InvalidReason::MalformedScalar(malformed)),
    )
  }

  /// Decode one Boolean token.
  fn bool_from_syntax(&mut self, syntax: SyntaxElement) -> NodeId {
    let decoded = token_text(&syntax).and_then(|text| text.parse::<bool>().map_err(|error| invalid_value(error.to_string())));
    match decoded {
      Ok(boolean) => self.insert_leaf(Some(syntax), Vec::new(), BuildLeafValue::Bool(boolean)),
      Err(failure) => self.invalid_scalar(syntax, ScalarKind::Bool, failure),
    }
  }

  /// Decode one string token.
  fn string_from_syntax(&mut self, syntax: SyntaxElement) -> NodeId {
    let repr = match syntax.kind() {
      STRING => StrRepr::Basic,
      MULTI_LINE_STRING => StrRepr::MultiLine,
      STRING_LITERAL => StrRepr::Literal,
      MULTI_LINE_STRING_LITERAL => StrRepr::MultiLineLiteral,
      _ => return self.invalid_unexpected(syntax),
    };
    let decoded = token_text(&syntax).and_then(|text| decode_string(text, repr));
    match decoded {
      Ok(string) => self.insert_leaf(Some(syntax), Vec::new(), BuildLeafValue::String(Arc::from(string))),
      Err(failure) => self.invalid_scalar(syntax, ScalarKind::String, failure),
    }
  }

  /// Decode one integer token.
  fn integer_from_syntax(&mut self, syntax: SyntaxElement) -> NodeId {
    let repr = match syntax.kind() {
      INTEGER => IntegerRepr::Dec,
      INTEGER_BIN => IntegerRepr::Bin,
      INTEGER_OCT => IntegerRepr::Oct,
      INTEGER_HEX => IntegerRepr::Hex,
      _ => return self.invalid_unexpected(syntax),
    };
    let decoded = token_text(&syntax).and_then(|text| decode_integer(text, repr));
    match decoded {
      Ok(integer) => self.insert_leaf(Some(syntax), Vec::new(), BuildLeafValue::Integer {
        repr,
        value: integer,
      }),
      Err(failure) => self.invalid_scalar(syntax, ScalarKind::Integer, failure),
    }
  }

  /// Decode one floating-point token.
  fn float_from_syntax(&mut self, syntax: SyntaxElement) -> NodeId {
    let decoded = token_text(&syntax).and_then(|text| {
      text
        .replace('_', "")
        .replace("nan", "NaN")
        .parse::<f64>()
        .map_err(|error| invalid_value(error.to_string()))
    });
    match decoded {
      Ok(float) => self.insert_leaf(Some(syntax), Vec::new(), BuildLeafValue::Float(float)),
      Err(failure) => self.invalid_scalar(syntax, ScalarKind::Float, failure),
    }
  }

  /// Decode one date or time token.
  fn date_time_from_syntax(&mut self, syntax: SyntaxElement) -> NodeId {
    let decoded = token_text(&syntax).and_then(|text| decode_date_time(text, syntax.kind()));
    match decoded {
      Ok(date_time) => self.insert_leaf(Some(syntax), Vec::new(), BuildLeafValue::DateTime(date_time)),
      Err(failure) => self.invalid_scalar(syntax, ScalarKind::DateTime, failure),
    }
  }

  /// Publish every mutable record directly into its preallocated immutable identity.
  fn freeze(self, root: &NodeId, syntax_guards: VecDeque<GreenNode>) -> Node {
    let Self {
      nodes,
      identities,
      comments,
    } = self;
    for (node, identity) in nodes.into_iter().zip(&identities) {
      let record = match node {
        BuildNode::Table(table) => freeze_table(table),
        BuildNode::Array(array) => freeze_array(array),
        BuildNode::Leaf(leaf) => freeze_leaf(leaf),
      };
      identity.initialize(record);
    }
    DomArena::publish(&root.arena, comments, syntax_guards)
  }
}

/// Freeze one table into an immutable arena record.
#[allow(
  clippy::single_call_fn,
  reason = "the named freezer keeps the table's immutable record shape beside its sibling array and leaf freezers"
)]
fn freeze_table(table: BuildTable) -> NodeSeed {
  NodeSeed::Table {
    inner:   Arc::new(TableInner::new(table.diagnostics.into(), table.syntax, table.kind)),
    entries: Arc::new(table.entries.freeze()),
  }
}

/// Freeze one array into an immutable arena record.
#[allow(
  clippy::single_call_fn,
  reason = "the named freezer keeps the array's immutable record shape beside its sibling table and leaf freezers"
)]
fn freeze_array(array: BuildArray) -> NodeSeed {
  NodeSeed::Array {
    inner: Arc::new(ArrayInner::new(array.diagnostics.into(), array.syntax, array.kind)),
    items: array.items.into_iter().map(|node| node.arena).collect(),
  }
}

/// Convert one syntax element into a fully frozen DOM.
#[allow(
  clippy::single_call_fn,
  reason = "the module entry point owns the sole two-phase construction boundary exposed to the parent dom module"
)]
pub(super) fn node_from_syntax(syntax: SyntaxElement) -> Node {
  let syntax_guards = syntax_green_guards(&syntax);
  let mut builder = DomBuilder {
    nodes:      Vec::new(),
    identities: Vec::new(),
    comments:   Arc::new(CommentStore::from_syntax(&syntax)),
  };
  let root = builder.node_from_syntax(syntax);
  builder.freeze(&root, syntax_guards)
}

/// Retain every syntax node's shallow green reference in parent-before-child order.
#[allow(
  clippy::single_call_fn,
  reason = "the named helper isolates green-node lifetime retention from mutable DOM construction and freezing"
)]
fn syntax_green_guards(syntax: &SyntaxElement) -> VecDeque<GreenNode> {
  let Some(root) = syntax.ancestors().last() else {
    return VecDeque::new();
  };
  root
    .preorder()
    .filter_map(|event| match event {
      WalkEvent::Enter(node) => Some(node.green().clone()),
      WalkEvent::Leave(_) => None,
    })
    .collect()
}

/// Decode keys from one `KEY` syntax node.
pub(super) fn keys_from_syntax(syntax: &SyntaxElement) -> impl ExactSizeIterator<Item = Key> + use<> {
  build_keys_from_syntax(syntax).into_iter().map(|key| key.freeze())
}

/// Collect mutable decoded keys from one `KEY` syntax node.
fn build_keys_from_syntax(syntax: &SyntaxElement) -> Vec<BuildKey> {
  syntax
    .as_node()
    .map(|node| {
      node
        .children_with_tokens()
        .filter(|child| child.kind() == IDENT)
        .map(key_from_syntax)
        .collect()
    })
    .unwrap_or_default()
}

/// Split one entry into decoded key segments and its optional value syntax.
fn entry_parts(syntax: &SyntaxElement) -> (Vec<BuildKey>, Option<SyntaxElement>) {
  let key_node = syntax.as_node().and_then(rowan::SyntaxNode::first_child);
  let keys = key_node
    .as_ref()
    .map(|candidate| build_keys_from_syntax(&candidate.clone().into()))
    .unwrap_or_default();
  let entry_value = key_node.and_then(|candidate| candidate.next_sibling()).map(Into::into);
  (keys, entry_value)
}

/// Collect header keys from the first `KEY` child.
#[allow(
  clippy::single_call_fn,
  reason = "the named helper distinguishes header key decoding from the entry key-and-value split of entry_parts"
)]
fn header_keys(syntax: &SyntaxElement) -> Vec<BuildKey> {
  syntax
    .as_node()
    .and_then(rowan::SyntaxNode::first_child)
    .map(|key| build_keys_from_syntax(&key.into()))
    .unwrap_or_default()
}

/// Decode one syntax-backed key.
#[allow(
  clippy::single_call_fn,
  reason = "the key-decoding callback centralizes bare, literal, basic, and invalid-key construction"
)]
fn key_from_syntax(syntax: SyntaxElement) -> BuildKey {
  let Some(token) = syntax.as_token() else {
    return invalid_key(syntax);
  };
  let text = token.text();
  let decoded = if text.starts_with('\'') {
    strip_delimiters(text, "'", "'").map(ToOwned::to_owned)
  } else if text.starts_with('"') {
    strip_delimiters(text, "\"", "\"").and_then(|body| {
      unescape(body).map_err(|error| DecodeFailure::InvalidEscape {
        offset: error.offset()
      })
    })
  } else {
    Ok(text.to_owned())
  };

  match decoded {
    Ok(decoded_key) => BuildKey {
      diagnostics:         Vec::new(),
      syntax:              Some(syntax),
      is_valid:            true,
      value:               Arc::from(decoded_key),
      additional_syntaxes: Vec::new(),
    },
    Err(_failure) => BuildKey {
      diagnostics:         Vec::from([Diagnostic::InvalidEscapeSequence {
        string: syntax.clone()
      }]),
      syntax:              Some(syntax),
      is_valid:            false,
      value:               Arc::from(""),
      additional_syntaxes: Vec::new(),
    },
  }
}

/// Construct one invalid key.
fn invalid_key(syntax: SyntaxElement) -> BuildKey {
  BuildKey {
    diagnostics:         Vec::from([Diagnostic::UnexpectedSyntax {
      syntax: syntax.clone()
    }]),
    syntax:              Some(syntax),
    is_valid:            false,
    value:               Arc::from(""),
    additional_syntaxes: Vec::new(),
  }
}

/// Freeze one leaf into an immutable arena record.
#[allow(
  clippy::single_call_fn,
  reason = "the named freezer keeps every leaf value's immutable record shape beside its sibling container freezers"
)]
fn freeze_leaf(leaf: BuildLeaf) -> NodeSeed {
  let diagnostics: Arc<[Diagnostic]> = leaf.diagnostics.into();
  match leaf.value {
    BuildLeafValue::Bool(boolean) => NodeSeed::Bool {
      inner: Arc::new(BoolInner {
        diagnostics,
        syntax: leaf.syntax,
        value: boolean,
      }),
    },
    BuildLeafValue::String(string) => NodeSeed::Str {
      inner: Arc::new(StrInner {
        diagnostics,
        syntax: leaf.syntax,
        value: string,
      }),
    },
    BuildLeafValue::Integer {
      repr,
      value: integer,
    } => NodeSeed::Integer {
      inner: Arc::new(IntegerInner {
        diagnostics,
        syntax: leaf.syntax,
        repr,
        value: integer,
      }),
    },
    BuildLeafValue::Float(float) => NodeSeed::Float {
      inner: Arc::new(FloatInner {
        diagnostics,
        syntax: leaf.syntax,
        value: float,
      }),
    },
    BuildLeafValue::DateTime(date_time) => NodeSeed::Date {
      inner: Arc::new(DateTimeInner {
        diagnostics,
        syntax: leaf.syntax,
        value: date_time,
      }),
    },
    BuildLeafValue::Invalid(reason) => NodeSeed::Invalid {
      inner: Arc::new(InvalidInner {
        diagnostics,
        syntax: leaf.syntax,
        reason,
      }),
    },
  }
}

/// Borrow token text or report that a scalar syntax element was not a token.
fn token_text(syntax: &SyntaxElement) -> Result<&str, DecodeFailure> {
  syntax
    .as_token()
    .map(rowan::SyntaxToken::text)
    .ok_or(DecodeFailure::MissingToken)
}

/// Decode one TOML string representation.
#[allow(
  clippy::single_call_fn,
  reason = "the representation-directed decoder centralizes delimiter, multiline, and escape semantics"
)]
fn decode_string(text: &str, repr: StrRepr) -> Result<String, DecodeFailure> {
  match repr {
    StrRepr::Basic => strip_delimiters(text, "\"", "\"").and_then(decode_escaped),
    StrRepr::Literal => strip_delimiters(text, "'", "'").map(ToOwned::to_owned),
    StrRepr::MultiLine => {
      let body = strip_delimiters(text, "\"\"\"", "\"\"\"")?;
      decode_escaped(strip_initial_newline(body))
    }
    StrRepr::MultiLineLiteral => {
      let body = strip_delimiters(text, "'''", "'''")?;
      Ok(strip_initial_newline(body).to_owned())
    }
  }
}

/// Remove a required opening and closing delimiter.
fn strip_delimiters<'text>(text: &'text str, prefix: &str, suffix: &str) -> Result<&'text str, DecodeFailure> {
  text
    .strip_prefix(prefix)
    .and_then(|body| body.strip_suffix(suffix))
    .ok_or_else(|| invalid_value("the scalar delimiters are incomplete"))
}

/// Remove the newline immediately following a multiline opening delimiter.
fn strip_initial_newline(text: &str) -> &str {
  text.strip_prefix("\r\n").or_else(|| text.strip_prefix('\n')).unwrap_or(text)
}

/// Decode TOML basic-string escapes.
fn decode_escaped(text: &str) -> Result<String, DecodeFailure> {
  unescape(text).map_err(|error| DecodeFailure::InvalidEscape {
    offset: error.offset()
  })
}

/// Decode one TOML integer.
#[allow(
  clippy::single_call_fn,
  reason = "the representation-directed decoder preserves decimal signedness and radix-specific parsing"
)]
fn decode_integer(text: &str, repr: IntegerRepr) -> Result<IntegerValue, DecodeFailure> {
  let normalized = text.replace('_', "");
  match repr {
    IntegerRepr::Dec if normalized.starts_with('-') => normalized
      .parse::<i64>()
      .map(IntegerValue::Negative)
      .map_err(|error| invalid_value(error.to_string())),
    IntegerRepr::Dec => normalized
      .parse::<u64>()
      .map(IntegerValue::Positive)
      .map_err(|error| invalid_value(error.to_string())),
    IntegerRepr::Bin => decode_radix(&normalized, "0b", 2),
    IntegerRepr::Oct => decode_radix(&normalized, "0o", 8),
    IntegerRepr::Hex => decode_radix(&normalized, "0x", 16),
  }
}

/// Decode one non-decimal unsigned integer.
fn decode_radix(text: &str, prefix: &str, radix: u32) -> Result<IntegerValue, DecodeFailure> {
  let digits = text
    .strip_prefix(prefix)
    .ok_or_else(|| invalid_value("the integer radix prefix is missing"))?;
  u64::from_str_radix(digits, radix)
    .map(IntegerValue::Positive)
    .map_err(|error| invalid_value(error.to_string()))
}

/// Decode one TOML date or time token without mutating string bytes unsafely.
#[allow(
  clippy::single_call_fn,
  reason = "the named decoder centralizes TOML spelling normalization and value-kind-specific time parsing"
)]
fn decode_date_time(text: &str, kind: SyntaxKind) -> Result<DateTimeValue, DecodeFailure> {
  let normalized = text
    .chars()
    .map(|character| match character {
      ' ' | 't' => 'T',
      'z' => 'Z',
      ',' => '.',
      other => other,
    })
    .collect::<String>();

  match kind {
    DATE_TIME_OFFSET => OffsetDateTime::parse(&normalized, &Rfc3339)
      .map(DateTimeValue::OffsetDateTime)
      .map_err(|error| invalid_value(error.to_string())),
    DATE_TIME_LOCAL => {
      let description = if normalized.contains('.') {
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond]")
      } else {
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]")
      };
      PrimitiveDateTime::parse(&normalized, &description)
        .map(DateTimeValue::LocalDateTime)
        .map_err(|error| invalid_value(error.to_string()))
    }
    DATE => Date::parse(&normalized, &format_description!("[year]-[month]-[day]"))
      .map(DateTimeValue::Date)
      .map_err(|error| invalid_value(error.to_string())),
    TIME => {
      let description = if normalized.contains('.') {
        format_description!("[hour]:[minute]:[second].[subsecond]")
      } else {
        format_description!("[hour]:[minute]:[second]")
      };
      Time::parse(&normalized, &description)
        .map(DateTimeValue::Time)
        .map_err(|error| invalid_value(error.to_string()))
    }
    _ => Err(invalid_value("the scalar does not have a date or time syntax kind")),
  }
}

/// Construct a typed invalid-value diagnostic payload.
fn invalid_value(message: impl Into<Arc<str>>) -> DecodeFailure {
  DecodeFailure::InvalidValue {
    message: message.into()
  }
}
