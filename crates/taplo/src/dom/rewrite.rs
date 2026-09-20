//! Source-preserving TOML queries and rewrites.
//!
//! [`crate::dom::rewrite::Rewrite`] owns one parsed document and records non-overlapping source
//! patches against it. Call [`crate::dom::rewrite::Rewrite::commit`] between structural phases
//! when a later edit depends on a shape created by an earlier edit.

use core::fmt;
use core::iter::once;
use std::cmp::Reverse;
use std::ops::Range;
use std::sync::Arc;

use rowan::TextRange;
use rowan::TextSize;
use thiserror::Error;

use super::Keys;
use super::error::Diagnostic as DomDiagnostic;
use super::error::QueryError;
use super::keys_from_syntax;
use super::node::ArrayKind;
use super::node::Node;
use super::node::TableKind;
use crate::dom;
use crate::parser;
use crate::syntax::SyntaxElement;
use crate::syntax::SyntaxKind;
use crate::syntax::SyntaxNode;

/// A literal TOML path containing key segments only.
///
/// Unlike [`Keys`], this type never treats `*`, `?`, or bracket notation as
/// query syntax.  An empty path identifies the document root and is the parent
/// path used when inserting a root entry.  Exact operations may cross an array
/// of tables only when it contains one element; multiple elements are
/// ambiguous, and sequence reconciliation remains a dedicated operation.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct ExactPath {
  /// Unescaped literal key values in path order.
  segments: Arc<[Arc<str>]>,
}

impl ExactPath {
  /// Parse an exact dotted TOML key path.
  ///
  /// Quoted keys are decoded as literal segments.  Unquoted glob syntax and
  /// bracket/index syntax are rejected by the ordinary TOML parser.
  ///
  /// # Errors
  ///
  /// Returns [`RewriteError::InvalidPath`] when `path` is not an exact TOML
  /// key path.
  #[allow(
    clippy::single_call_fn,
    reason = "the public constructor defines exact-path validation independently of the TryFrom convenience boundary"
  )]
  pub fn parse(path: &str) -> Result<Self, RewriteError> {
    if path.is_empty() {
      return Ok(Self::default());
    }

    let synthetic = format!("{path} = true\n");
    let parsed = parser::parse(&synthetic)?;
    if !parsed.diagnostics().is_empty() {
      return Err(RewriteError::InvalidPath {
        path:   path.into(),
        reason: "the path is not an exact TOML key".into(),
      });
    }

    let syntax = parsed.into_syntax();
    let mut entries = syntax.children().filter(|child| child.kind() == SyntaxKind::ENTRY);
    let Some(entry) = entries.next() else {
      return Err(RewriteError::InvalidPath {
        path:   path.into(),
        reason: "the path does not contain a key".into(),
      });
    };
    if entries.next().is_some() {
      return Err(RewriteError::InvalidPath {
        path:   path.into(),
        reason: "the path contains more than one TOML entry".into(),
      });
    }

    let Some(key) = entry.children().find(|child| child.kind() == SyntaxKind::KEY) else {
      return Err(RewriteError::InvalidPath {
        path:   path.into(),
        reason: "the path does not contain a key".into(),
      });
    };
    let segments = keys_from_syntax(&key.into())
      .map(|segment| Arc::<str>::from(segment.value()))
      .collect::<Vec<_>>();
    if segments.is_empty() {
      return Err(RewriteError::InvalidPath {
        path:   path.into(),
        reason: "the path does not contain a key".into(),
      });
    }

    Ok(Self {
      segments: segments.into()
    })
  }

  /// Construct a path from already-literal key values.
  ///
  /// An empty iterator constructs the document-root path.
  pub fn from_segments<I, S>(segments: I) -> Self
  where
    I: IntoIterator<Item = S>,
    S: Into<Arc<str>>,
  {
    Self {
      segments: segments.into_iter().map(Into::into).collect::<Vec<_>>().into(),
    }
  }

  /// Iterate over the literal path segments.
  pub fn segments(&self) -> impl ExactSizeIterator<Item = &str> {
    self.segments.iter().map(AsRef::as_ref)
  }

  /// Return whether this path identifies the document root.
  #[must_use]
  pub fn is_root(&self) -> bool {
    self.segments.is_empty()
  }

  /// Return whether this path contains no key segments.
  #[must_use]
  pub fn is_empty(&self) -> bool {
    self.segments.is_empty()
  }

  /// Return the number of literal key segments.
  #[must_use]
  pub fn len(&self) -> usize {
    self.segments.len()
  }

  /// Return the final literal key, if this is not the root path.
  #[must_use]
  pub fn key(&self) -> Option<&str> {
    self.segments.last().map(AsRef::as_ref)
  }

  /// Return the parent path, if this is not the root path.
  #[must_use]
  pub fn parent(&self) -> Option<Self> {
    let parent_len = self.segments.len().checked_sub(1)?;
    Some(Self::from_segments(self.segments.iter().take(parent_len).cloned()))
  }

  /// Return a path with one literal child appended.
  #[must_use]
  pub fn child(&self, segment: impl Into<Arc<str>>) -> Self {
    Self::from_segments(self.segments.iter().cloned().chain(once(segment.into())))
  }

  /// Return a path with every segment from `suffix` appended.
  #[must_use]
  pub fn extend(&self, suffix: &Self) -> Self {
    Self::from_segments(self.segments.iter().cloned().chain(suffix.segments.iter().cloned()))
  }

  /// Return whether this path is a strict parent of `candidate`.
  fn is_strict_parent_of(&self, candidate: &Self) -> bool {
    self.len() < candidate.len() && self.segments().zip(candidate.segments()).all(|(left, right)| left == right)
  }

  /// Render this path as a valid dotted TOML key.
  fn render(&self) -> String {
    self.segments().map(render_key).collect::<Vec<_>>().join(".")
  }
}

impl TryFrom<&str> for ExactPath {
  type Error = RewriteError;

  fn try_from(source: &str) -> Result<Self, Self::Error> {
    Self::parse(source)
  }
}

impl fmt::Display for ExactPath {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    self.render().fmt(formatter)
  }
}

/// The TOML structure found at an exact path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TomlKind {
  /// A regular or pseudo table.
  Table,
  /// An inline table value.
  InlineTable,
  /// An inline array value.
  Array,
  /// An array of tables.
  ArrayOfTables,
  /// A string scalar.
  String,
  /// An integer scalar.
  Integer,
  /// A floating-point scalar.
  Float,
  /// A Boolean scalar.
  Boolean,
  /// A date or date-time scalar.
  DateTime,
  /// An invalid DOM node supplied to [`Rewrite::new`].
  Invalid,
}

/// Implement stable variant-name rendering for one fieldless public enum.
macro_rules! impl_variant_display {
  ($name:ident { $($variant:ident),+ $(,)? }) => {
    impl fmt::Display for $name {
      fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match *self {
          $(Self::$variant => stringify!($variant),)+
        })
      }
    }
  };
}

impl_variant_display!(TomlKind {
  Table,
  InlineTable,
  Array,
  ArrayOfTables,
  String,
  Integer,
  Float,
  Boolean,
  DateTime,
  Invalid,
});

/// The result of one source mutation request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditOutcome {
  /// The requested representation already existed.
  Unchanged,
  /// New source was inserted.
  Inserted,
  /// Existing source was replaced.
  Replaced,
  /// Existing source was removed.
  Removed,
}

impl_variant_display!(EditOutcome {
  Unchanged,
  Inserted,
  Replaced,
  Removed,
});

/// Whether exact-entry removal may prune proven-empty regular parents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoveEmptyParents {
  /// Retain all parent tables.
  Keep,
  /// Remove only concrete regular parent blocks proven empty by this edit.
  Prune,
}

/// A borrowed TOML value and its structural kind.
#[derive(Clone, Copy, Debug)]
pub struct ValueView<'source> {
  /// Exact source text for the value.
  text: &'source str,
  /// Parsed structural kind.
  kind: TomlKind,
}

impl<'source> ValueView<'source> {
  /// Return the exact source text for this value.
  #[must_use]
  pub const fn text(self) -> &'source str {
    self.text
  }

  /// Return the parsed structural kind.
  #[must_use]
  pub const fn kind(self) -> TomlKind {
    self.kind
  }
}

/// A borrowed key/value entry and its value.
#[derive(Clone, Copy, Debug)]
pub struct EntryView<'source> {
  /// Exact source text from the key through the value.
  text:  &'source str,
  /// Borrowed value view.
  value: ValueView<'source>,
}

impl<'source> EntryView<'source> {
  /// Return the exact key/value source without attached line trivia.
  #[must_use]
  pub const fn text(self) -> &'source str {
    self.text
  }

  /// Return the parsed value kind.
  #[must_use]
  pub const fn kind(self) -> TomlKind {
    self.value.kind
  }

  /// Return the entry's value view.
  #[must_use]
  pub const fn value(self) -> ValueView<'source> {
    self.value
  }
}

/// Generate the common typed constructor façade for validated TOML fragments.
macro_rules! define_fragment_parsers {
  (
    $(
      $(#[$metadata:meta])*
      for $fragment:ident |$source:ident| $body:block
    )+
  ) => {
    $(
      impl $fragment {
        /// Parse and validate one source fragment of this semantic type.
        ///
        /// # Errors
        ///
        /// Returns a typed syntax, semantic, or fragment-shape error when
        /// `source` does not satisfy this fragment type's structural contract.
        $(#[$metadata])*
        pub fn parse($source: &str) -> Result<Self, RewriteError> $body
      }
    )+
  };
}

/// A validated fragment containing exactly one TOML value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValueFragment {
  /// Exact value source.
  source: Arc<str>,
  /// Parsed value kind.
  kind:   TomlKind,
}

impl ValueFragment {
  /// Return the exact value source.
  #[must_use]
  pub fn as_str(&self) -> &str {
    &self.source
  }

  /// Return the parsed structural kind.
  #[must_use]
  pub const fn kind(&self) -> TomlKind {
    self.kind
  }
}

/// A validated fragment containing one key/value entry and attached trivia.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntryFragment {
  /// Attached source text.
  source: Arc<str>,
  /// Entry key relative to its eventual parent.
  key:    ExactPath,
  /// Validated value fragment.
  value:  ValueFragment,
}

impl EntryFragment {
  /// Return the entry key relative to its parent.
  #[must_use]
  pub const fn key(&self) -> &ExactPath {
    &self.key
  }

  /// Return the exact entry source including attached trivia.
  #[must_use]
  pub fn as_str(&self) -> &str {
    &self.source
  }

  /// Return the validated value fragment.
  #[must_use]
  pub const fn value(&self) -> &ValueFragment {
    &self.value
  }

  /// Return source suitable for insertion after table positioning is chosen.
  fn insertion_source(&self) -> &str {
    self.source.trim()
  }
}

/// A validated inline-array element and its attached comment trivia.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArrayElementFragment {
  /// Source spelling retained for callers that copy fragments.
  source:           Arc<str>,
  /// Exact element value.
  value:            ValueFragment,
  /// Contiguous standalone comments attached before the element.
  leading_comments: Arc<[Arc<str>]>,
  /// Inline comment attached after the element.
  trailing_comment: Option<Arc<str>>,
}

impl ArrayElementFragment {
  /// Return the fragment's source spelling.
  #[must_use]
  pub fn as_str(&self) -> &str {
    &self.source
  }

  /// Return the element value.
  #[must_use]
  pub const fn value(&self) -> &ValueFragment {
    &self.value
  }

  /// Return the element value kind.
  #[must_use]
  pub const fn kind(&self) -> TomlKind {
    self.value.kind()
  }
}

/// A validated regular-table or array-of-tables semantic element.
///
/// The fragment includes its root header and every following strict-descendant
/// header owned by that element.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableBlockFragment {
  /// Complete block source including attached trivia.
  source: Arc<str>,
  /// Header path.
  path:   ExactPath,
  /// Header kind.
  kind:   TomlKind,
}

define_fragment_parsers! {
  for ValueFragment |source| {
    let synthetic = format!("fragment = {source}\n");
    let rewrite = Rewrite::parse(&synthetic)?;
    let path = ExactPath::from_segments(["fragment"]);
    let view = rewrite.value(&path)?;
    if view.text().trim() != source.trim() {
      return Err(RewriteError::InvalidFragment {
        kind:   FragmentKind::Value,
        reason: "the fragment must contain exactly one TOML value".into(),
      });
    }
    Ok(Self {
      source: source.trim().into(),
      kind:   view.kind(),
    })
  }
  #[allow(
    clippy::single_call_fn,
    reason = "the public constructor owns entry-fragment shape and attached-trivia validation as one reusable boundary"
  )]
  for EntryFragment |source| {
    let rewrite = Rewrite::parse(source)?;
    let entries = rewrite.top_level_entries()?;
    if entries.len() != 1 || !rewrite.header_records()?.is_empty() {
      return Err(RewriteError::InvalidFragment {
        kind:   FragmentKind::Entry,
        reason: "the fragment must contain exactly one root key/value entry".into(),
      });
    }
    let Some(entry) = entries.first() else {
      return Err(RewriteError::InvalidFragment {
        kind:   FragmentKind::Entry,
        reason: "the fragment does not contain an entry".into(),
      });
    };
    let key = path_from_entry(entry)?;
    let parsed_value = rewrite.value_fragment(&key)?;
    let core = text_range(entry.text_range())?;
    let attached = rewrite.attached_line_range(core)?;
    let prefix = rewrite.source.get(..attached.start).ok_or(RewriteError::InvalidUtf8Range {
      start: 0,
      end:   attached.start,
    })?;
    let suffix = rewrite.source.get(attached.end..).ok_or(RewriteError::InvalidUtf8Range {
      start: attached.end,
      end:   rewrite.source.len(),
    })?;
    if !prefix.trim().is_empty() || !suffix.trim().is_empty() {
      return Err(RewriteError::InvalidFragment {
        kind:   FragmentKind::Entry,
        reason: "the fragment contains trivia not attached to the entry".into(),
      });
    }
    let attached_source = rewrite.slice(attached)?;
    Ok(Self {
      source: attached_source.into(),
      key,
      value: parsed_value,
    })
  }
  for ArrayElementFragment |source| {
    let synthetic = format!("fragment = [\n{source}\n]\n");
    let rewrite = Rewrite::parse(&synthetic)?;
    let path = ExactPath::from_segments(["fragment"]);
    let node = rewrite.node(&path)?;
    let Node::Array(array) = node else {
      return Err(RewriteError::InvalidFragment {
        kind:   FragmentKind::ArrayElement,
        reason: "the fragment is not represented by an inline array".into(),
      });
    };
    let Some(syntax) = array.syntax().and_then(SyntaxElement::as_node) else {
      return Err(RewriteError::InvalidFragment {
        kind:   FragmentKind::ArrayElement,
        reason: "the fragment has no source-backed inline array".into(),
      });
    };
    if !rewrite.detached_array_comments(syntax)?.is_empty() {
      return Err(RewriteError::InvalidFragment {
        kind:   FragmentKind::ArrayElement,
        reason: "the fragment contains a blank-separated or unrelated comment".into(),
      });
    }
    let fragments = rewrite.array_elements(&path)?;
    if fragments.len() != 1 {
      return Err(RewriteError::InvalidFragment {
        kind:   FragmentKind::ArrayElement,
        reason: "the fragment must contain exactly one array element".into(),
      });
    }
    let Some(fragment) = fragments.into_iter().next() else {
      return Err(RewriteError::InvalidFragment {
        kind:   FragmentKind::ArrayElement,
        reason: "the fragment does not contain an array element".into(),
      });
    };
    Ok(Self {
      source: source.trim().into(),
      ..fragment
    })
  }
  #[allow(
    clippy::single_call_fn,
    reason = "the public constructor owns complete table-block hierarchy and source-boundary validation"
  )]
  for TableBlockFragment |source| {
    let rewrite = Rewrite::parse(source)?;
    let headers = rewrite.header_records()?;
    if headers.is_empty() || !rewrite.top_level_entries_before_first_header()?.is_empty() {
      return Err(RewriteError::InvalidFragment {
        kind:   FragmentKind::TableBlock,
        reason: "the fragment must contain one complete root table block".into(),
      });
    }
    let Some(header) = headers.first() else {
      return Err(RewriteError::InvalidFragment {
        kind:   FragmentKind::TableBlock,
        reason: "the fragment does not contain a table header".into(),
      });
    };
    if headers
      .iter()
      .skip(1)
      .any(|descendant| !header.path.is_strict_parent_of(&descendant.path))
    {
      return Err(RewriteError::InvalidFragment {
        kind:   FragmentKind::TableBlock,
        reason: "every header after the root block must be its strict descendant".into(),
      });
    }
    let elements = rewrite.semantic_block_records(&header.path)?;
    if elements.len() != 1 {
      return Err(RewriteError::InvalidFragment {
        kind:   FragmentKind::TableBlock,
        reason: "the fragment contains more than one root table element".into(),
      });
    }
    let Some(element) = elements.first() else {
      return Err(RewriteError::InvalidFragment {
        kind:   FragmentKind::TableBlock,
        reason: "the fragment does not contain a complete table element".into(),
      });
    };
    let block_source = rewrite.slice(element.block.clone())?;
    let prefix = rewrite
      .source
      .get(..element.block.start)
      .ok_or(RewriteError::InvalidUtf8Range {
        start: 0,
        end:   element.block.start,
      })?;
    if !prefix.trim().is_empty() {
      return Err(RewriteError::InvalidFragment {
        kind:   FragmentKind::TableBlock,
        reason: "the fragment contains source outside the table block".into(),
      });
    }
    let suffix = rewrite.source.get(element.block.end..).ok_or(RewriteError::InvalidUtf8Range {
      start: element.block.end,
      end:   rewrite.source.len(),
    })?;
    if !suffix.trim().is_empty() {
      return Err(RewriteError::InvalidFragment {
        kind:   FragmentKind::TableBlock,
        reason: "the fragment contains source outside the table block".into(),
      });
    }
    Ok(Self {
      source: block_source.into(),
      path:   element.path.clone(),
      kind:   element.kind,
    })
  }
}

impl TableBlockFragment {
  /// Return the exact complete block source.
  #[must_use]
  pub fn as_str(&self) -> &str {
    &self.source
  }

  /// Return the block header path.
  #[must_use]
  pub const fn path(&self) -> &ExactPath {
    &self.path
  }

  /// Return [`TomlKind::Table`] or [`TomlKind::ArrayOfTables`].
  #[must_use]
  pub const fn kind(&self) -> TomlKind {
    self.kind
  }

  /// Rebase this complete block beneath a different exact root path.
  ///
  /// Only table-header key ranges are rewritten.  Every strict-descendant
  /// header retains its suffix beneath `path`, while entries, whitespace, and
  /// attached comments remain byte-for-byte identical.
  ///
  /// # Errors
  ///
  /// Returns an invalid-fragment error when `path` is the document root, or a
  /// typed render/validation error when the rebased block is not valid TOML.
  pub fn rebase(&self, path: &ExactPath) -> Result<Self, RewriteError> {
    if path.is_root() {
      return Err(RewriteError::InvalidFragment {
        kind:   FragmentKind::TableBlock,
        reason: "a table block cannot be rebased to the document root".into(),
      });
    }
    if path == &self.path {
      return Ok(self.clone());
    }

    let mut rewrite = Rewrite::parse(&self.source)?;
    for header in rewrite.header_records()? {
      if header.path != self.path && !self.path.is_strict_parent_of(&header.path) {
        return Err(RewriteError::InvalidFragment {
          kind:   FragmentKind::TableBlock,
          reason: "the table block contains a header outside its root hierarchy".into(),
        });
      }
      let suffix = ExactPath::from_segments(header.path.segments.iter().skip(self.path.len()).cloned());
      let rebased = path.extend(&suffix);
      rewrite.push_std_patch(header.key, rebased.render().into())?;
    }
    let rendered = rewrite.render()?;
    Self::parse(&rendered)
  }
}

/// One owned, synchronous source-preserving TOML rewrite transaction.
#[derive(Debug)]
pub struct Rewrite {
  /// Parsed DOM for the committed source.
  root:    Node,
  /// Committed UTF-8 source.
  source:  String,
  /// Pending non-overlapping source patches.
  patches: Vec<PendingPatch>,
}

impl Rewrite {
  /// Construct a rewrite from an existing root DOM node.
  ///
  /// This constructor accepts an already-built DOM and therefore cannot
  /// recover parser diagnostics that were discarded before the DOM was built.
  /// Consumers starting from source should call [`Self::parse`].
  ///
  /// # Errors
  ///
  /// Returns [`RewriteError::RootNodeExpected`] unless `root` owns a syntax
  /// root.
  pub fn new(root: Node) -> Result<Self, RewriteError> {
    let Some(syntax) = root.syntax().and_then(SyntaxElement::as_node) else {
      return Err(RewriteError::RootNodeExpected);
    };
    if syntax.kind() != SyntaxKind::ROOT {
      return Err(RewriteError::RootNodeExpected);
    }
    let source = syntax.to_string();

    Ok(Self {
      root,
      source,
      patches: Vec::new(),
    })
  }

  /// Parse and fully validate a source document before exposing it for edits.
  ///
  /// # Errors
  ///
  /// Returns separate typed variants for parser syntax diagnostics and DOM
  /// semantic diagnostics.
  pub fn parse(source: &str) -> Result<Self, RewriteError> {
    let parsed = parser::parse(source)?;
    if !parsed.diagnostics().is_empty() {
      return Err(RewriteError::SyntaxDiagnostics {
        diagnostics: parsed.diagnostics().to_vec(),
      });
    }
    let root = parsed.into_dom();
    if let Err(errors) = root.validate() {
      let diagnostics = errors.into_iter().map(SemanticDiagnostic::from_dom).collect::<Vec<_>>();
      return Err(RewriteError::SemanticDiagnostics {
        diagnostics,
      });
    }
    Ok(Self {
      root,
      source: source.into(),
      patches: Vec::new(),
    })
  }

  /// Return the committed source before pending patches are rendered.
  #[must_use]
  pub fn source(&self) -> &str {
    &self.source
  }

  /// Borrow the immutable DOM corresponding to [`Self::source`].
  ///
  /// The node retains its semantic values, diagnostics, and source anchors.
  /// Pending patches do not change this view. A successful [`Self::commit`]
  /// replaces the committed root; a failed commit leaves it intact.
  #[must_use]
  pub const fn root(&self) -> &Node {
    &self.root
  }

  /// Add a low-level patch such as [`Patch::RenameKeys`].
  ///
  /// # Errors
  ///
  /// Returns a DOM query error or overlap error when a requested key range
  /// cannot be patched safely.
  pub fn add(&mut self, patch: impl Into<Patch>) -> Result<&mut Self, RewriteError> {
    let requested_patch = patch.into();
    match requested_patch {
      Patch::RenameKeys {
        key,
        to,
      } => {
        let keys = key.parse::<Keys>()?;
        let ranges = self.rename_key_ranges(&keys)?;
        self.push_replacements(ranges, &to)?;
      }
    }

    self.patches.sort_by_key(|pending| Reverse(pending.range.start()));
    Ok(self)
  }

  /// Return all pending source patches in descending source order.
  #[must_use]
  pub fn patches(&self) -> &[PendingPatch] {
    &self.patches
  }

  /// Rename every key matched by the glob query.
  ///
  /// # Errors
  ///
  /// Returns a DOM query error or overlap error.
  pub fn rename_keys(&mut self, key: &str, to: &str) -> Result<&mut Self, RewriteError> {
    self.add(Patch::RenameKeys {
      key: key.into(),
      to:  to.into(),
    })
  }

  /// Read an exact value and its TOML kind.
  ///
  /// # Errors
  ///
  /// Returns [`RewriteError::MissingPath`] when the exact path is absent,
  /// [`RewriteError::AmbiguousMatches`] when traversal reaches multiple
  /// array-table elements, or a typed range error when provenance is
  /// unavailable.
  pub fn value(&self, path: &ExactPath) -> Result<ValueView<'_>, RewriteError> {
    let node = self.node(path)?;
    let kind = node_kind(&node);
    let range = Self::node_source_range(path, &node, "the DOM value has no source provenance")?;
    let text = self.slice(range)?;
    Ok(ValueView {
      text,
      kind,
    })
  }

  /// Read an exact key/value entry and its value kind.
  ///
  /// # Errors
  ///
  /// Returns a missing-path, ambiguous-array-table, or unsupported-placement
  /// error when `path` does not identify one source-backed entry.
  pub fn entry(&self, path: &ExactPath) -> Result<EntryView<'_>, RewriteError> {
    if path.is_root() {
      return Err(RewriteError::MissingPath {
        path: path.clone()
      });
    }
    let value_view = self.value(path)?;
    let node = self.node(path)?;
    let entry = entry_syntax(&node).ok_or_else(|| RewriteError::UnsupportedPlacement {
      path:   path.clone(),
      reason: "the path is not represented by a key/value entry".into(),
    })?;
    let text = self.slice(text_range(entry.text_range())?)?;
    Ok(EntryView {
      text,
      value: value_view,
    })
  }

  /// Extract a validated value fragment for source-preserving copying.
  ///
  /// # Errors
  ///
  /// Returns the same typed errors as [`Self::value`].
  pub fn value_fragment(&self, path: &ExactPath) -> Result<ValueFragment, RewriteError> {
    let value_view = self.value(path)?;
    Ok(ValueFragment {
      source: value_view.text().into(),
      kind:   value_view.kind(),
    })
  }

  /// Extract a validated entry fragment with its attached comments.
  ///
  /// # Errors
  ///
  /// Returns the same typed errors as [`Self::entry`] plus range errors for
  /// attached trivia.
  pub fn entry_fragment(&self, path: &ExactPath) -> Result<EntryFragment, RewriteError> {
    let entry = self.entry(path)?;
    let node = self.node(path)?;
    let syntax = entry_syntax(&node).ok_or_else(|| RewriteError::UnsupportedPlacement {
      path:   path.clone(),
      reason: "the path is not represented by a key/value entry".into(),
    })?;
    let core = text_range(syntax.text_range())?;
    let attached = if syntax.ancestors().any(|ancestor| ancestor.kind() == SyntaxKind::INLINE_TABLE) {
      core
    } else {
      self.attached_line_range(core)?
    };
    let key = path_from_entry(&syntax)?;
    Ok(EntryFragment {
      source: self.slice(attached)?.into(),
      key,
      value: ValueFragment {
        source: entry.value().text().into(),
        kind:   entry.kind(),
      },
    })
  }

  /// Replace only the value range at an exact path.
  ///
  /// # Errors
  ///
  /// Returns a missing-path, ambiguous-array-table, overlap, or source-range
  /// error.
  pub fn replace_value(&mut self, path: &ExactPath, fragment: &ValueFragment) -> Result<EditOutcome, RewriteError> {
    let node = self.node(path)?;
    let range = Self::node_source_range(path, &node, "the DOM value has no source provenance")?;
    if self.slice(range.clone())? == fragment.as_str() {
      return Ok(EditOutcome::Unchanged);
    }
    self.push_std_patch(range, Arc::clone(&fragment.source))?;
    Ok(EditOutcome::Replaced)
  }

  /// Insert an entry into a root, regular, inline, or unique array table.
  ///
  /// # Errors
  ///
  /// Returns an ambiguity error if the entry already exists, a type mismatch
  /// for a non-table parent, or an unsupported-placement/overlap error when a
  /// source-preserving insertion point cannot be proven.
  pub fn insert_entry(&mut self, parent: &ExactPath, entry: &EntryFragment) -> Result<EditOutcome, RewriteError> {
    let full_path = parent.extend(entry.key());
    if self.node(&full_path).is_ok() {
      return Err(RewriteError::AmbiguousMatches {
        path:  full_path,
        count: 1,
      });
    }

    let parent_node = self.table_node(parent)?;
    let Node::Table(table) = parent_node else {
      return Err(RewriteError::TypeMismatch {
        path:     parent.clone(),
        expected: "table",
        found:    node_kind(&parent_node),
      });
    };

    match table.kind() {
      TableKind::Inline => self.insert_inline_entry(parent, entry),
      TableKind::Regular | TableKind::Pseudo => self.insert_regular_entry(parent, entry),
    }
  }

  /// Insert an entry when absent or replace only its value when present.
  ///
  /// # Errors
  ///
  /// Returns the typed errors from [`Self::replace_value`] or
  /// [`Self::insert_entry`].
  pub fn upsert_entry(&mut self, parent: &ExactPath, entry: &EntryFragment) -> Result<EditOutcome, RewriteError> {
    let path = parent.extend(entry.key());
    if self.node(&path).is_ok() {
      self.replace_value(&path, entry.value())
    } else {
      self.insert_entry(parent, entry)
    }
  }

  /// Insert or replace a value at an exact path.
  ///
  /// # Errors
  ///
  /// Returns a missing-parent, invalid-fragment, type, placement, range, or
  /// overlap error.
  pub fn upsert_value(&mut self, path: &ExactPath, fragment: &ValueFragment) -> Result<EditOutcome, RewriteError> {
    let Some((parent, key)) = path.parent().zip(path.key()) else {
      return Err(RewriteError::UnsupportedPlacement {
        path:   path.clone(),
        reason: "the document root cannot be replaced as an entry value".into(),
      });
    };
    if self.node(path).is_ok() {
      return self.replace_value(path, fragment);
    }
    let source = format!("{} = {}", render_key(key), fragment.as_str());
    let entry = EntryFragment::parse(&source)?;
    self.insert_entry(&parent, &entry)
  }

  /// Explicitly create every missing regular-table parent in `path`.
  ///
  /// Existing table prefixes are retained.  This method never infers table
  /// creation from a failed insert or upsert.
  ///
  /// # Errors
  ///
  /// Returns a type mismatch if an existing prefix is not a regular/pseudo
  /// table, or an overlap/range error at the insertion point.
  pub fn create_tables(&mut self, path: &ExactPath) -> Result<EditOutcome, RewriteError> {
    if path.is_root() {
      return Ok(EditOutcome::Unchanged);
    }

    let mut missing = Vec::new();
    let mut prefix = ExactPath::default();
    for segment in path.segments() {
      prefix = prefix.child(segment);
      match self.node(&prefix) {
        Ok(Node::Table(table)) if table.kind() != TableKind::Inline => {}
        Ok(node) => {
          return Err(RewriteError::TypeMismatch {
            path:     prefix,
            expected: "regular table",
            found:    node_kind(&node),
          });
        }
        Err(RewriteError::MissingPath {
          ..
        }) => missing.push(prefix.clone()),
        Err(error) => return Err(error),
      }
    }
    if missing.is_empty() {
      return Ok(EditOutcome::Unchanged);
    }

    let mut insertion = String::new();
    if !self.source.is_empty() && !self.source.ends_with('\n') {
      insertion.push('\n');
    }
    for table in missing {
      if !insertion.is_empty() && !insertion.ends_with("\n\n") {
        insertion.push('\n');
      }
      insertion.push('[');
      insertion.push_str(&table.render());
      insertion.push_str("]\n");
    }
    let offset = self.source.len();
    self.push_std_patch(offset..offset, insertion.into())?;
    Ok(EditOutcome::Inserted)
  }

  /// Remove an exact entry and optionally prune proven-empty regular parents.
  ///
  /// # Errors
  ///
  /// Returns a missing-path, unsupported-placement, range, or overlap error.
  pub fn remove_entry(&mut self, path: &ExactPath, empty_parents: RemoveEmptyParents) -> Result<EditOutcome, RewriteError> {
    let node = self.node(path)?;
    let entry = entry_syntax(&node).ok_or_else(|| RewriteError::UnsupportedPlacement {
      path:   path.clone(),
      reason: "the path is not represented by a key/value entry".into(),
    })?;
    let range = if entry.ancestors().any(|ancestor| ancestor.kind() == SyntaxKind::INLINE_TABLE) {
      self.inline_entry_removal_range(&entry, path)?
    } else {
      self.entry_removal_range(&entry, path, empty_parents)?
    };
    self.push_std_patch(range, Arc::from(""))?;
    Ok(EditOutcome::Removed)
  }

  /// Enumerate validated inline-array elements in source order.
  ///
  /// # Errors
  ///
  /// Returns a missing-path, type mismatch, unsupported-placement, or source
  /// range error.
  pub fn array_elements(&self, path: &ExactPath) -> Result<Vec<ArrayElementFragment>, RewriteError> {
    let syntax = self.inline_array_syntax(path)?;
    let mut fragments = Vec::new();
    for value_node in syntax.children().filter(|child| child.kind() == SyntaxKind::VALUE) {
      fragments.push(self.array_element_fragment(&syntax, &value_node)?);
    }
    Ok(fragments)
  }

  /// Reconcile an inline array to an ordered list of validated elements.
  ///
  /// # Errors
  ///
  /// Returns a missing-path, type, placement, range, or overlap error.
  pub fn reconcile_array(&mut self, path: &ExactPath, elements: &[ArrayElementFragment]) -> Result<EditOutcome, RewriteError> {
    let syntax = self.inline_array_syntax(path)?;
    let interior = delimited_interior(&syntax, SyntaxKind::BRACKET_START, SyntaxKind::BRACKET_END)?;
    let original = self.slice(interior.clone())?;
    let detached_comments = self.detached_array_comments(&syntax)?;
    let replacement = render_array_elements(original, elements, &detached_comments);
    if original == replacement {
      return Ok(EditOutcome::Unchanged);
    }
    self.push_std_patch(interior, replacement.into())?;
    Ok(EditOutcome::Replaced)
  }

  /// Resolve one exact path to its source-backed inline-array syntax.
  fn inline_array_syntax(&self, path: &ExactPath) -> Result<SyntaxNode, RewriteError> {
    let node = self.node(path)?;
    let Node::Array(ref array) = node else {
      return Err(RewriteError::TypeMismatch {
        path:     path.clone(),
        expected: "inline array",
        found:    node_kind(&node),
      });
    };
    if array.kind() != ArrayKind::Inline {
      return Err(RewriteError::TypeMismatch {
        path:     path.clone(),
        expected: "inline array",
        found:    TomlKind::ArrayOfTables,
      });
    }
    let Some(syntax) = array.syntax().and_then(SyntaxElement::as_node) else {
      return Err(RewriteError::UnsupportedPlacement {
        path:   path.clone(),
        reason: "the array has no source syntax".into(),
      });
    };
    Ok(syntax.clone())
  }

  /// Enumerate complete regular-table or array-of-tables elements for `path`.
  ///
  /// Every element includes strict-descendant header blocks up to the next
  /// same-path sibling or first non-descendant header.
  ///
  /// # Errors
  ///
  /// Returns a typed source-range error if a block cannot be sliced safely.
  pub fn table_blocks(&self, path: &ExactPath) -> Result<Vec<TableBlockFragment>, RewriteError> {
    self
      .semantic_block_records(path)?
      .into_iter()
      .map(|header| {
        Ok(TableBlockFragment {
          source: self.slice(header.block)?.into(),
          path:   header.path,
          kind:   header.kind,
        })
      })
      .collect()
  }

  /// Reconcile complete table elements at `path` in the supplied order.
  ///
  /// Fragments may come from this rewrite, another target document, or a
  /// snapshot.  Every fragment must have the same header path.
  ///
  /// # Errors
  ///
  /// Returns an invalid-fragment error for a mismatched path, an ambiguity
  /// error for non-contiguous matching blocks, or a range/overlap error.
  pub fn reconcile_table_blocks(&mut self, path: &ExactPath, blocks: &[TableBlockFragment]) -> Result<EditOutcome, RewriteError> {
    if blocks.iter().any(|block| block.path() != path) {
      return Err(RewriteError::InvalidFragment {
        kind:   FragmentKind::TableBlock,
        reason: "every table block must match the reconciled exact path".into(),
      });
    }
    let matches = self.semantic_block_records(path)?;
    if matches.is_empty() {
      if blocks.is_empty() {
        return Ok(EditOutcome::Unchanged);
      }
      let insertion_blocks = render_table_blocks(blocks, &[]);
      let offset = self.source.len();
      let mut insertion = String::new();
      if !self.source.is_empty() && !self.source.ends_with('\n') {
        insertion.push('\n');
      }
      if !self.source.is_empty() && !self.source.ends_with("\n\n") {
        insertion.push('\n');
      }
      insertion.push_str(&insertion_blocks);
      self.push_std_patch(offset..offset, insertion.into())?;
      return Ok(EditOutcome::Inserted);
    }

    let headers = self.header_records()?;
    for (first, second) in matches.iter().zip(matches.iter().skip(1)) {
      if headers
        .iter()
        .any(|header| header.core.start >= first.block.end && header.core.end <= second.block.start)
      {
        return Err(RewriteError::AmbiguousMatches {
          path:  path.clone(),
          count: matches.len(),
        });
      }
    }

    let Some(first) = matches.first() else {
      return Ok(EditOutcome::Unchanged);
    };
    let Some(last) = matches.last() else {
      return Ok(EditOutcome::Unchanged);
    };
    let gaps = matches
      .iter()
      .zip(matches.iter().skip(1))
      .enumerate()
      .map(|(position, (preceding, following))| {
        let slot = position.checked_add(1).ok_or(RewriteError::InvalidSourceRange {
          start: position,
          end:   usize::MAX,
        })?;
        Ok(TableBlockGap {
          slot,
          source: self.slice(preceding.block.end..following.block.start)?.into(),
        })
      })
      .collect::<Result<Vec<_>, RewriteError>>()?;
    let range = first.block.start..last.block.end;
    let original = self.slice(range.clone())?;
    let replacement = render_table_blocks(blocks, &gaps);
    if original == replacement {
      return Ok(EditOutcome::Unchanged);
    }
    self.push_std_patch(range, replacement.into())?;
    if blocks.is_empty() {
      Ok(EditOutcome::Removed)
    } else {
      Ok(EditOutcome::Replaced)
    }
  }

  /// Render all pending patches without mutating rewrite state.
  ///
  /// # Errors
  ///
  /// Returns typed overlap or UTF-8 range errors.  The method never performs
  /// unchecked slicing or in-place string range replacement.
  pub fn render(&self) -> Result<String, RewriteError> {
    let mut patches = self.patches.iter().collect::<Vec<_>>();
    patches.sort_by_key(|patch| patch.range.start());

    let mut rendered = String::with_capacity(self.source.len());
    let mut cursor = 0_usize;
    for patch in patches {
      let range = text_range(patch.range)?;
      if range.start > range.end || range.end > self.source.len() {
        return Err(RewriteError::InvalidSourceRange {
          start: range.start,
          end:   range.end,
        });
      }
      if range.start < cursor {
        return Err(RewriteError::Overlap);
      }
      let unchanged = self.source.get(cursor..range.start).ok_or(RewriteError::InvalidUtf8Range {
        start: cursor,
        end:   range.start,
      })?;
      rendered.push_str(unchanged);
      match patch.kind {
        PendingPatchKind::Replace(ref replacement) => rendered.push_str(replacement),
      }
      cursor = range.end;
    }
    let remainder = self.source.get(cursor..).ok_or(RewriteError::InvalidUtf8Range {
      start: cursor,
      end:   self.source.len(),
    })?;
    rendered.push_str(remainder);
    Ok(rendered)
  }

  /// Atomically commit all in-memory patches after full validation.
  ///
  /// Rendering, syntax parsing, and semantic validation all complete before
  /// this rewrite's source, root, or patch list is changed.
  ///
  /// # Errors
  ///
  /// Returns any render, syntax, or semantic error while leaving this rewrite
  /// unchanged.
  pub fn commit(&mut self) -> Result<(), RewriteError> {
    let rendered = self.render()?;
    let committed = Self::parse(&rendered)?;
    self.root = committed.root;
    self.source = committed.source;
    self.patches.clear();
    Ok(())
  }

  /// Resolve an exact path against the committed DOM.
  fn node(&self, path: &ExactPath) -> Result<Node, RewriteError> {
    if path.is_root() {
      return Ok(self.root.clone());
    }
    let mut node = self.root.clone();
    for segment in path.segments() {
      node = Self::exact_child(node, segment, path)?;
    }
    Ok(node)
  }

  /// Resolve one literal key, crossing one unique array-of-tables element when needed.
  fn exact_child(node: Node, segment: &str, path: &ExactPath) -> Result<Node, RewriteError> {
    match node {
      Node::Table(table) => table.get(segment).ok_or_else(|| RewriteError::MissingPath {
        path: path.clone()
      }),
      Node::Array(array) if array.kind() == ArrayKind::Tables => {
        let table = Self::unique_array_table_element(&array, path)?;
        Self::exact_child(table, segment, path)
      }
      Node::Array(_) | Node::Bool(_) | Node::Str(_) | Node::Integer(_) | Node::Float(_) | Node::Date(_) | Node::Invalid(_) => {
        Err(RewriteError::MissingPath {
          path: path.clone()
        })
      }
    }
  }

  /// Resolve a table insertion parent, unwrapping one terminal array-of-tables element.
  fn table_node(&self, path: &ExactPath) -> Result<Node, RewriteError> {
    let node = self.node(path)?;
    match node {
      Node::Array(array) if array.kind() == ArrayKind::Tables => Self::unique_array_table_element(&array, path),
      other @ (Node::Table(_)
      | Node::Array(_)
      | Node::Bool(_)
      | Node::Str(_)
      | Node::Integer(_)
      | Node::Float(_)
      | Node::Date(_)
      | Node::Invalid(_)) => Ok(other),
    }
  }

  /// Resolve the sole table element in an array of tables.
  fn unique_array_table_element(array: &super::node::Array, path: &ExactPath) -> Result<Node, RewriteError> {
    let elements = array.items();
    let count = elements.len();
    match elements.first() {
      None => Err(RewriteError::MissingPath {
        path: path.clone()
      }),
      Some(element) if count == 1 => Ok(element),
      Some(_) => Err(RewriteError::AmbiguousMatches {
        path: path.clone(),
        count,
      }),
    }
  }

  /// Return the root syntax node.
  fn root_syntax(&self) -> Result<SyntaxNode, RewriteError> {
    self
      .root
      .syntax()
      .and_then(SyntaxElement::as_node)
      .cloned()
      .ok_or(RewriteError::RootNodeExpected)
  }

  /// Return all direct root entries in source order.
  fn top_level_entries(&self) -> Result<Vec<SyntaxNode>, RewriteError> {
    Ok(
      self
        .root_syntax()?
        .children()
        .filter(|child| child.kind() == SyntaxKind::ENTRY)
        .collect(),
    )
  }

  /// Return root entries that precede the first table header.
  fn top_level_entries_before_first_header(&self) -> Result<Vec<SyntaxNode>, RewriteError> {
    Ok(
      self
        .root_syntax()?
        .children()
        .take_while(|child| !matches!(child.kind(), SyntaxKind::TABLE_HEADER | SyntaxKind::TABLE_ARRAY_HEADER))
        .filter(|child| child.kind() == SyntaxKind::ENTRY)
        .collect(),
    )
  }

  /// Build complete table-block records from direct root headers.
  fn header_records(&self) -> Result<Vec<HeaderRecord>, RewriteError> {
    let root = self.root_syntax()?;
    let entry_ranges = root
      .children()
      .filter(|child| child.kind() == SyntaxKind::ENTRY)
      .map(|entry| text_range(entry.text_range()))
      .collect::<Result<Vec<_>, RewriteError>>()?;
    let headers = root
      .children()
      .filter(|child| matches!(child.kind(), SyntaxKind::TABLE_HEADER | SyntaxKind::TABLE_ARRAY_HEADER))
      .map(|header| {
        let path = path_from_header(&header)?;
        let core = text_range(header.text_range())?;
        let Some(key_syntax) = header.children().find(|child| child.kind() == SyntaxKind::KEY) else {
          return Err(RewriteError::InvalidFragment {
            kind:   FragmentKind::TableBlock,
            reason: "the table header has no key syntax".into(),
          });
        };
        let key = text_range(key_syntax.text_range())?;
        let start = self.attached_start(core.start)?;
        let kind = if header.kind() == SyntaxKind::TABLE_ARRAY_HEADER {
          TomlKind::ArrayOfTables
        } else {
          TomlKind::Table
        };
        Ok(HeaderRecord {
          path,
          kind,
          core,
          key,
          block: start..self.source.len(),
        })
      })
      .collect::<Result<Vec<_>, RewriteError>>()?;

    let mut completed = Vec::with_capacity(headers.len());
    for (position, header) in headers.iter().enumerate() {
      let next_position = position.checked_add(1).ok_or(RewriteError::InvalidSourceRange {
        start: position,
        end:   usize::MAX,
      })?;
      let next_header_start = headers.get(next_position).map_or(self.source.len(), |next| next.core.start);
      let entry_end = entry_ranges
        .iter()
        .filter(|entry| entry.start >= header.core.end && entry.end <= next_header_start)
        .map(|entry| self.attached_end(entry.end))
        .collect::<Result<Vec<_>, RewriteError>>()?
        .last()
        .copied();
      let end = match entry_end {
        Some(end) => end,
        None => self.attached_end(header.core.end)?,
      };
      completed.push(header.with_block(header.block.start..end));
    }
    Ok(completed)
  }

  /// Expand matching headers to complete semantic table-element spans.
  ///
  /// A parent header owns all immediately following strict-descendant headers
  /// until the next same-path sibling or the first non-descendant header.
  fn semantic_block_records(&self, path: &ExactPath) -> Result<Vec<HeaderRecord>, RewriteError> {
    let headers = self.header_records()?;
    let mut elements = Vec::new();
    for (position, header) in headers.iter().enumerate().filter(|candidate| candidate.1.path == *path) {
      let following = headers.iter().skip(position.saturating_add(1));
      let end = following
        .take_while(|candidate| path.is_strict_parent_of(&candidate.path))
        .last()
        .map_or(header.block.end, |descendant| descendant.block.end);
      elements.push(header.with_block(header.block.start..end));
    }
    Ok(elements)
  }

  /// Insert into the root or one concrete regular table block.
  fn insert_regular_entry(&mut self, parent: &ExactPath, entry: &EntryFragment) -> Result<EditOutcome, RewriteError> {
    let offset = if parent.is_root() {
      self
        .header_records()?
        .first()
        .map_or(self.source.len(), |header| header.block.start)
    } else {
      let headers = self
        .header_records()?
        .into_iter()
        .filter(|header| header.path == *parent && matches!(header.kind, TomlKind::Table | TomlKind::ArrayOfTables))
        .collect::<Vec<_>>();
      if headers.len() != 1 {
        return Err(RewriteError::UnsupportedPlacement {
          path:   parent.clone(),
          reason: "a table insertion requires one concrete table header".into(),
        });
      }
      let Some(header) = headers.first() else {
        return Err(RewriteError::UnsupportedPlacement {
          path:   parent.clone(),
          reason: "the regular table has no concrete source header".into(),
        });
      };
      let entry_ends = self
        .root_syntax()?
        .children()
        .filter(|child| child.kind() == SyntaxKind::ENTRY)
        .filter_map(|child| text_range(child.text_range()).ok())
        .filter(|range| range.start >= header.core.end && range.end <= header.block.end)
        .map(|range| self.attached_end(range.end))
        .collect::<Result<Vec<_>, RewriteError>>()?;
      match entry_ends.last() {
        Some(end) => *end,
        None => self.attached_end(header.core.end)?,
      }
    };

    let before = self.source.get(..offset).ok_or(RewriteError::InvalidUtf8Range {
      start: 0, end: offset
    })?;
    let after = self.source.get(offset..).ok_or(RewriteError::InvalidUtf8Range {
      start: offset,
      end:   self.source.len(),
    })?;
    let mut insertion = String::new();
    if !before.is_empty() && !before.ends_with('\n') {
      insertion.push('\n');
    }
    insertion.push_str(entry.insertion_source());
    if !insertion.ends_with('\n') {
      insertion.push('\n');
    }
    if !after.is_empty() && !after.starts_with('\n') && !insertion.ends_with("\n\n") {
      insertion.push('\n');
    }
    self.push_std_patch(offset..offset, insertion.into())?;
    Ok(EditOutcome::Inserted)
  }

  /// Insert into an inline table by replacing only its interior.
  fn insert_inline_entry(&mut self, parent: &ExactPath, entry: &EntryFragment) -> Result<EditOutcome, RewriteError> {
    let node = self.node(parent)?;
    let Node::Table(table) = node else {
      return Err(RewriteError::TypeMismatch {
        path:     parent.clone(),
        expected: "inline table",
        found:    node_kind(&node),
      });
    };
    let Some(syntax) = table.syntax().and_then(SyntaxElement::as_node) else {
      return Err(RewriteError::UnsupportedPlacement {
        path:   parent.clone(),
        reason: "the inline table has no source syntax".into(),
      });
    };
    let interior = delimited_interior(syntax, SyntaxKind::BRACE_START, SyntaxKind::BRACE_END)?;
    let original = self.slice(interior.clone())?;
    let entry_source = entry.insertion_source();
    let replacement = if original.trim().is_empty() {
      format!(" {entry_source} ")
    } else if original.contains('\n') {
      let indent = infer_indent(original).unwrap_or("  ");
      let closing_indent = original.rsplit_once('\n').map_or("", |(_, trailing)| trailing);
      let content_end = original.len().saturating_sub(closing_indent.len());
      let content = original.get(..content_end).ok_or(RewriteError::InvalidUtf8Range {
        start: 0,
        end:   content_end,
      })?;
      let body = content.trim_end();
      let separator = if body.ends_with(',') { "" } else { "," };
      format!("{body}{separator}\n{indent}{entry_source}\n{closing_indent}")
    } else {
      let leading_len = original.len().saturating_sub(original.trim_start().len());
      let trailing_len = original.len().saturating_sub(original.trim_end().len());
      let leading = original.get(..leading_len).ok_or(RewriteError::InvalidUtf8Range {
        start: 0,
        end:   leading_len,
      })?;
      let content_end = original.len().saturating_sub(trailing_len);
      let content = original.get(leading_len..content_end).ok_or(RewriteError::InvalidUtf8Range {
        start: leading_len,
        end:   content_end,
      })?;
      let trailing = original.get(content_end..).ok_or(RewriteError::InvalidUtf8Range {
        start: content_end,
        end:   original.len(),
      })?;
      format!("{leading}{content}, {entry_source}{trailing}")
    };
    self.push_std_patch(interior, replacement.into())?;
    Ok(EditOutcome::Inserted)
  }

  /// Calculate the exact removal range for a regular/root entry.
  fn entry_removal_range(
    &self,
    entry: &SyntaxNode,
    path: &ExactPath,
    empty_parents: RemoveEmptyParents,
  ) -> Result<Range<usize>, RewriteError> {
    let attached = self.attached_line_range(text_range(entry.text_range())?)?;
    if empty_parents == RemoveEmptyParents::Keep {
      return Ok(attached);
    }
    self.prune_empty_parent_range(path, attached)
  }

  /// Expand a removal through each concrete, comment-free parent proven empty.
  fn prune_empty_parent_range(&self, path: &ExactPath, mut removal: Range<usize>) -> Result<Range<usize>, RewriteError> {
    let mut parent = path.parent();
    while let Some(candidate_path) = parent {
      if candidate_path.is_root() {
        break;
      }
      let candidates = self
        .semantic_block_records(&candidate_path)?
        .into_iter()
        .filter(|header| header.kind == TomlKind::Table)
        .filter(|header| header.block.start <= removal.start && removal.end <= header.block.end)
        .collect::<Vec<_>>();
      if candidates.len() != 1 {
        break;
      }
      let Some(header) = candidates.first() else {
        break;
      };
      if !self.table_block_empty_after_removal(&header.block, &removal)? {
        break;
      }
      removal = header.block.clone();
      parent = candidate_path.parent();
    }
    Ok(removal)
  }

  /// Prove that removing one nested range leaves only one empty table header.
  fn table_block_empty_after_removal(&self, block: &Range<usize>, removal: &Range<usize>) -> Result<bool, RewriteError> {
    let before = self.slice(block.start..removal.start)?;
    let after = self.slice(removal.end..block.end)?;
    let mut remaining = String::with_capacity(before.len().saturating_add(after.len()));
    remaining.push_str(before);
    remaining.push_str(after);
    let parsed = parser::parse(&remaining)?;
    if !parsed.diagnostics().is_empty() {
      return Ok(false);
    }
    let syntax = parsed.into_syntax();
    let header_count = syntax
      .children()
      .filter(|child| matches!(child.kind(), SyntaxKind::TABLE_HEADER | SyntaxKind::TABLE_ARRAY_HEADER))
      .count();
    let has_entry = syntax.children().any(|child| child.kind() == SyntaxKind::ENTRY);
    let has_comment = syntax
      .descendants_with_tokens()
      .any(|element| element.kind() == SyntaxKind::COMMENT);
    Ok(header_count == 1 && !has_entry && !has_comment)
  }

  /// Calculate a comma-aware removal range inside an inline table.
  fn inline_entry_removal_range(&self, entry: &SyntaxNode, path: &ExactPath) -> Result<Range<usize>, RewriteError> {
    let Some(inline) = entry.ancestors().find(|ancestor| ancestor.kind() == SyntaxKind::INLINE_TABLE) else {
      return Err(RewriteError::UnsupportedPlacement {
        path:   path.clone(),
        reason: "the inline entry has no inline-table ancestor".into(),
      });
    };
    let interior = delimited_interior(&inline, SyntaxKind::BRACE_START, SyntaxKind::BRACE_END)?;
    let entry_range = text_range(entry.text_range())?;
    let before = self.slice(interior.start..entry_range.start)?;
    let after = self.slice(entry_range.end..interior.end)?;
    if let Some(comma) = after.find(',') {
      let comma_end = comma
        .checked_add(1)
        .and_then(|relative_end| entry_range.end.checked_add(relative_end))
        .ok_or(RewriteError::InvalidSourceRange {
          start: entry_range.end,
          end:   usize::MAX,
        })?;
      return Ok(entry_range.start..comma_end);
    }
    if let Some(comma) = before.rfind(',') {
      let comma_start = interior.start.checked_add(comma).ok_or(RewriteError::InvalidSourceRange {
        start: interior.start,
        end:   usize::MAX,
      })?;
      return Ok(comma_start..entry_range.end);
    }
    Ok(interior)
  }

  /// Extract one array value and attached comments.
  fn array_element_fragment(&self, array: &SyntaxNode, element: &SyntaxNode) -> Result<ArrayElementFragment, RewriteError> {
    let (node, ranges) = self.array_element_ranges(array, element)?;
    let value_source = self.slice(ranges.core.clone())?.trim();
    let value_fragment = ValueFragment {
      source: value_source.into(),
      kind:   node_kind(&node),
    };
    let leading = self.slice(ranges.attached.start..ranges.core.start)?;
    let trailing = self.slice(ranges.core.end..ranges.line_end)?;
    let leading_comments = leading
      .lines()
      .map(str::trim)
      .filter(|line| line.starts_with('#'))
      .map(Arc::<str>::from)
      .collect::<Vec<_>>()
      .into();
    let trailing_comment = trailing
      .find('#')
      .and_then(|offset| trailing.get(offset..))
      .map(str::trim)
      .map(Arc::<str>::from);
    let mut fragment_source = String::from(leading);
    fragment_source.push_str(value_source);
    if let Some(comment) = trailing_comment.as_ref() {
      fragment_source.push(' ');
      fragment_source.push_str(comment);
    }
    Ok(ArrayElementFragment {
      source: fragment_source.trim().into(),
      value: value_fragment,
      leading_comments,
      trailing_comment,
    })
  }

  /// Collect comments that are not attached to any array element.
  fn detached_array_comments(&self, array: &SyntaxNode) -> Result<Vec<DetachedArrayComment>, RewriteError> {
    let element_ranges = array
      .children()
      .filter(|child| child.kind() == SyntaxKind::VALUE)
      .map(|element| {
        self
          .array_element_ranges(array, &element)
          .map(|(_node, ranges)| ranges.attached)
      })
      .collect::<Result<Vec<_>, RewriteError>>()?;

    let mut detached = Vec::new();
    for comment in array
      .descendants_with_tokens()
      .filter(|element| element.kind() == SyntaxKind::COMMENT)
    {
      let range = text_range(comment.text_range())?;
      let attached = element_ranges
        .iter()
        .any(|element| element.start <= range.start && range.end <= element.end);
      if attached {
        continue;
      }
      let slot = element_ranges.iter().filter(|element| element.end <= range.start).count();
      detached.push(DetachedArrayComment {
        slot,
        source: self.slice(range)?.into(),
      });
    }
    Ok(detached)
  }

  /// Calculate one array value's DOM node, exact source core, line end, and comment attachment
  /// range.
  fn array_element_ranges(&self, array: &SyntaxNode, element: &SyntaxNode) -> Result<(Node, ArrayElementRanges), RewriteError> {
    let node = super::node_from_syntax(element.clone().into());
    let Some(value_syntax) = node.syntax() else {
      return Err(RewriteError::InvalidFragment {
        kind:   FragmentKind::ArrayElement,
        reason: "the array element has no source-backed value".into(),
      });
    };
    let core = text_range(value_syntax.text_range())?;
    let interior = delimited_interior(array, SyntaxKind::BRACKET_START, SyntaxKind::BRACKET_END)?;
    let preceding_comma_end = array
      .children_with_tokens()
      .filter(|syntax| syntax.kind() == SyntaxKind::COMMA)
      .filter_map(|comma| text_range(comma.text_range()).ok())
      .filter(|comma| comma.end <= core.start)
      .map(|comma| comma.end)
      .last()
      .unwrap_or(interior.start);
    let start = self.attached_start(core.start)?.max(preceding_comma_end);
    let line_end = self.line_end(core.end)?.min(interior.end);
    let trailing = self.slice(core.end..line_end)?;
    let end = if trailing.contains('#') { line_end } else { core.end };
    Ok((node, ArrayElementRanges {
      core,
      line_end,
      attached: start..end,
    }))
  }

  /// Calculate one entry's comment-attached line range.
  fn attached_line_range(&self, core: Range<usize>) -> Result<Range<usize>, RewriteError> {
    Ok(self.attached_start(core.start)?..self.attached_end(core.end)?)
  }

  /// Include contiguous preceding standalone comments until a blank line.
  fn attached_start(&self, core_start: usize) -> Result<usize, RewriteError> {
    let mut start = self.line_start(core_start)?;
    loop {
      if start == 0 {
        return Ok(start);
      }
      let previous_end = start.saturating_sub(1);
      let previous_start = self.line_start(previous_end)?;
      let line = self.slice(previous_start..previous_end)?;
      if line.trim().starts_with('#') {
        start = previous_start;
      } else {
        return Ok(start);
      }
    }
  }

  /// Include the trailing inline comment and line ending when no sibling is on
  /// the same line.
  fn attached_end(&self, core_end: usize) -> Result<usize, RewriteError> {
    let end = self.line_end(core_end)?;
    let tail = self.slice(core_end..end)?;
    let trimmed = tail.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
      Ok(end)
    } else {
      Ok(core_end)
    }
  }

  /// Return the byte offset of the containing line's start.
  fn line_start(&self, offset: usize) -> Result<usize, RewriteError> {
    let prefix = self.source.get(..offset).ok_or(RewriteError::InvalidUtf8Range {
      start: 0, end: offset
    })?;
    prefix.rfind('\n').map_or(Ok(0), |newline| {
      newline.checked_add(1).ok_or(RewriteError::InvalidSourceRange {
        start: newline,
        end:   usize::MAX,
      })
    })
  }

  /// Return the byte offset immediately after the containing line ending.
  fn line_end(&self, offset: usize) -> Result<usize, RewriteError> {
    let suffix = self.source.get(offset..).ok_or(RewriteError::InvalidUtf8Range {
      start: offset,
      end:   self.source.len(),
    })?;
    suffix.find('\n').map_or(Ok(self.source.len()), |relative| {
      offset
        .checked_add(relative)
        .and_then(|newline| newline.checked_add(1))
        .ok_or(RewriteError::InvalidSourceRange {
          start: offset,
          end:   usize::MAX,
        })
    })
  }

  /// Slice source only after validating bounds and UTF-8 boundaries.
  fn slice(&self, range: Range<usize>) -> Result<&str, RewriteError> {
    if range.start > range.end || range.end > self.source.len() {
      return Err(RewriteError::InvalidSourceRange {
        start: range.start,
        end:   range.end,
      });
    }
    self.source.get(range.clone()).ok_or(RewriteError::InvalidUtf8Range {
      start: range.start,
      end:   range.end,
    })
  }

  /// Add a standard byte range patch after converting it to Rowan offsets.
  fn push_std_patch(&mut self, range: Range<usize>, replacement: Arc<str>) -> Result<(), RewriteError> {
    let start = TextSize::try_from(range.start).map_err(|_conversion_error| RewriteError::InvalidSourceRange {
      start: range.start,
      end:   range.end,
    })?;
    let end = TextSize::try_from(range.end).map_err(|_conversion_error| RewriteError::InvalidSourceRange {
      start: range.start,
      end:   range.end,
    })?;
    self.push_patch(TextRange::new(start, end), PendingPatchKind::Replace(replacement))
  }

  /// Collect exact syntax ranges for every key segment selected by one query.
  fn rename_key_ranges(&self, keys: &Keys) -> Result<Vec<TextRange>, RewriteError> {
    Ok(
      self
        .root
        .find_all_matches(keys, false)?
        .filter_map(|(matched, _)| match matched.iter().last().cloned() {
          Some(dom::KeyOrIndex::Key(key)) => Some(key),
          _ => None,
        })
        .flat_map(|key| key.text_ranges().collect::<Vec<_>>())
        .collect(),
    )
  }

  /// Validate and install one atomic family of equal replacement patches.
  fn push_replacements(&mut self, ranges: Vec<TextRange>, replacement: &Arc<str>) -> Result<(), RewriteError> {
    self.validate_new_ranges(&ranges)?;
    for range in ranges {
      self.push_patch(range, PendingPatchKind::Replace(Arc::clone(replacement)))?;
    }
    Ok(())
  }

  /// Resolve a DOM node's exact source range or return contextual provenance failure.
  fn node_source_range(path: &ExactPath, node: &Node, reason: &'static str) -> Result<Range<usize>, RewriteError> {
    node
      .syntax()
      .map(SyntaxElement::text_range)
      .ok_or_else(|| RewriteError::UnsupportedPlacement {
        path:   path.clone(),
        reason: reason.into(),
      })
      .and_then(text_range)
  }

  /// Add a patch only if it neither touches nor overlaps an existing patch.
  fn push_patch(&mut self, range: TextRange, kind: PendingPatchKind) -> Result<(), RewriteError> {
    self.check_overlap(range)?;
    self.patches.push(PendingPatch {
      range,
      kind,
    });
    self.patches.sort_by_key(|patch| Reverse(patch.range.start()));
    Ok(())
  }

  /// Reject ranges that touch or overlap a pending patch.
  fn check_overlap(&self, range: TextRange) -> Result<(), RewriteError> {
    if self.patches.iter().any(|patch| ranges_touch(range, patch.range)) {
      Err(RewriteError::Overlap)
    } else {
      Ok(())
    }
  }

  /// Validate one mutation's complete range set before adding any patch.
  fn validate_new_ranges(&self, ranges: &[TextRange]) -> Result<(), RewriteError> {
    for (position, range) in ranges.iter().enumerate() {
      self.check_overlap(*range)?;
      if ranges
        .iter()
        .skip(position.saturating_add(1))
        .any(|other| ranges_touch(*range, *other))
      {
        return Err(RewriteError::Overlap);
      }
    }
    Ok(())
  }
}

/// Return whether two source ranges touch or overlap.
fn ranges_touch(left: TextRange, right: TextRange) -> bool {
  left.start() <= right.end() && right.start() <= left.end()
}

impl fmt::Display for Rewrite {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    self.render().map_err(|_rewrite_error| fmt::Error)?.fmt(formatter)
  }
}

/// Patch requests accepted by [`Rewrite::add`].
#[derive(Debug)]
pub enum Patch {
  /// Rename every key matched by the glob query.
  RenameKeys {
    /// Dotted/glob query.
    key: Arc<str>,
    /// Replacement key spelling.
    to:  Arc<str>,
  },
}

/// One pending source patch.
#[derive(Clone, Debug)]
pub struct PendingPatch {
  /// Source range in the committed document.
  pub range: TextRange,
  /// Replacement operation.
  pub kind:  PendingPatchKind,
}

/// The operation stored by a pending patch.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum PendingPatchKind {
  /// Replace the range with this UTF-8 source.
  Replace(Arc<str>),
}

/// The validated fragment category involved in a fragment error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FragmentKind {
  /// A TOML value.
  Value,
  /// A key/value entry.
  Entry,
  /// An array element.
  ArrayElement,
  /// A regular or array-of-tables block.
  TableBlock,
}

/// Owned source coordinates for one rewrite diagnostic.
///
/// The offsets use Rowan's native UTF-8 byte-coordinate width without
/// retaining a syntax tree, DOM node, or key handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiagnosticRange {
  /// Inclusive start byte offset.
  start: u32,
  /// Exclusive end byte offset.
  end:   u32,
}

impl DiagnosticRange {
  /// Return the inclusive start byte offset.
  #[must_use]
  pub const fn start(self) -> u32 {
    self.start
  }

  /// Return the exclusive end byte offset.
  #[must_use]
  pub const fn end(self) -> u32 {
    self.end
  }

  /// Project an owned Rowan range into the public diagnostic coordinates.
  fn from_text_range(range: TextRange) -> Self {
    Self {
      start: u32::from(range.start()),
      end:   u32::from(range.end()),
    }
  }
}

/// Stable category for an owned DOM diagnostic projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticDiagnosticKind {
  /// Syntax appeared in a structurally invalid DOM position.
  UnexpectedSyntax,
  /// A string contained an invalid escape sequence.
  InvalidEscapeSequence,
  /// A scalar token could not be decoded into its advertised value kind.
  MalformedScalar,
  /// Two keys conflict semantically.
  ConflictingKeys,
  /// A value used as a table was not a table.
  ExpectedTable,
  /// A value used as an array of tables was not an array of tables.
  ExpectedArrayOfTables,
}

/// Owned, thread-safe projection of a DOM diagnostic.
///
/// Taplo DOM nodes and keys intentionally remain synchronous, owned values.
/// Rewrite errors retain only the category, rendered message, and source
/// coordinates needed by downstream error boundaries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticDiagnostic {
  /// Stable diagnostic category.
  kind:          SemanticDiagnosticKind,
  /// Human-readable DOM diagnostic.
  message:       Arc<str>,
  /// Primary source location, when the DOM supplied one.
  range:         Option<DiagnosticRange>,
  /// Related source location for two-site diagnostics.
  related_range: Option<DiagnosticRange>,
}

impl SemanticDiagnostic {
  /// Return the stable diagnostic category.
  #[must_use]
  pub const fn kind(&self) -> SemanticDiagnosticKind {
    self.kind
  }

  /// Return the human-readable diagnostic message.
  #[must_use]
  pub fn message(&self) -> &str {
    &self.message
  }

  /// Return the primary source range, when available.
  #[must_use]
  pub const fn range(&self) -> Option<DiagnosticRange> {
    self.range
  }

  /// Return the related source range, when available.
  #[must_use]
  pub const fn related_range(&self) -> Option<DiagnosticRange> {
    self.related_range
  }

  /// Consume a DOM diagnostic while retaining no DOM-backed handles.
  #[allow(
    clippy::single_call_fn,
    reason = "the conversion boundary deliberately strips DOM handles while preserving typed category and source coordinates"
  )]
  fn from_dom(diagnostic: DomDiagnostic) -> Self {
    let message = Arc::<str>::from(diagnostic.to_string());
    let (kind, range, related_range) = match diagnostic {
      DomDiagnostic::UnexpectedSyntax {
        syntax,
      } => (
        SemanticDiagnosticKind::UnexpectedSyntax,
        Some(DiagnosticRange::from_text_range(syntax.text_range())),
        None,
      ),
      DomDiagnostic::InvalidEscapeSequence {
        string,
      } => (
        SemanticDiagnosticKind::InvalidEscapeSequence,
        Some(DiagnosticRange::from_text_range(string.text_range())),
        None,
      ),
      DomDiagnostic::MalformedScalar(malformed) => (
        SemanticDiagnosticKind::MalformedScalar,
        Some(DiagnosticRange::from_text_range(malformed.syntax().text_range())),
        None,
      ),
      DomDiagnostic::ConflictingKeys {
        key,
        other,
      } => (
        SemanticDiagnosticKind::ConflictingKeys,
        key.text_ranges().next().map(DiagnosticRange::from_text_range),
        other.text_ranges().next().map(DiagnosticRange::from_text_range),
      ),
      DomDiagnostic::ExpectedTable {
        not_table,
        required_by,
      } => (
        SemanticDiagnosticKind::ExpectedTable,
        not_table.text_ranges().next().map(DiagnosticRange::from_text_range),
        required_by.text_ranges().next().map(DiagnosticRange::from_text_range),
      ),
      DomDiagnostic::ExpectedArrayOfTables {
        not_array_of_tables,
        required_by,
      } => (
        SemanticDiagnosticKind::ExpectedArrayOfTables,
        not_array_of_tables.text_ranges().next().map(DiagnosticRange::from_text_range),
        required_by.text_ranges().next().map(DiagnosticRange::from_text_range),
      ),
    };
    Self {
      kind,
      message,
      range,
      related_range,
    }
  }
}

impl fmt::Display for SemanticDiagnostic {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    self.message.fmt(formatter)
  }
}

/// Typed failures from parsing, querying, and source-preserving mutation.
#[derive(Debug, Error)]
pub enum RewriteError {
  /// A rewrite must own the document root.
  #[error("only the root node can be patched")]
  RootNodeExpected,
  /// Rowan could not construct the lossless syntax tree.
  #[error(transparent)]
  Parse(#[from] parser::ParseFailure),
  /// A DOM lookup or glob query failed.
  #[error(transparent)]
  Query(#[from] QueryError),
  /// The parser reported one or more syntax diagnostics.
  #[error("the TOML source has syntax diagnostics: {diagnostics:?}")]
  SyntaxDiagnostics {
    /// All parser diagnostics in source order.
    diagnostics: Vec<parser::Diagnostic>,
  },
  /// The DOM reported one or more semantic diagnostics.
  #[error("the TOML source has semantic diagnostics: {diagnostics:?}")]
  SemanticDiagnostics {
    /// All semantic diagnostics in traversal order.
    diagnostics: Vec<SemanticDiagnostic>,
  },
  /// An exact path could not be parsed.
  #[error("invalid exact TOML path `{path}`: {reason}")]
  InvalidPath {
    /// Rejected path source.
    path:   Arc<str>,
    /// Actionable rejection reason.
    reason: Arc<str>,
  },
  /// A validated fragment had the wrong shape.
  #[error("invalid {kind:?} fragment: {reason}")]
  InvalidFragment {
    /// Expected fragment category.
    kind:   FragmentKind,
    /// Actionable rejection reason.
    reason: Arc<str>,
  },
  /// An exact path was absent.
  #[error("the exact TOML path `{path}` was not found")]
  MissingPath {
    /// Missing path.
    path: ExactPath,
  },
  /// A path resolved to the wrong TOML structure.
  #[error("expected {expected} at `{path}`, found {found:?}")]
  TypeMismatch {
    /// Exact path that resolved.
    path:     ExactPath,
    /// Human-readable expected structure.
    expected: &'static str,
    /// Actual structure.
    found:    TomlKind,
  },
  /// An exact operation resolved ambiguously.
  #[error("the exact TOML path `{path}` resolved to {count} ambiguous matches")]
  AmbiguousMatches {
    /// Ambiguous path.
    path:  ExactPath,
    /// Number of matches.
    count: usize,
  },
  /// Pending source patches overlap or touch.
  #[error("new patches would overlap or touch existing patches")]
  Overlap,
  /// A source range was out of bounds or could not fit Rowan coordinates.
  #[error("invalid source range {start}..{end}")]
  InvalidSourceRange {
    /// Inclusive start coordinate.
    start: usize,
    /// Exclusive end coordinate.
    end:   usize,
  },
  /// A source range did not fall on UTF-8 boundaries.
  #[error("source range {start}..{end} is not on valid UTF-8 boundaries")]
  InvalidUtf8Range {
    /// Inclusive start coordinate.
    start: usize,
    /// Exclusive end coordinate.
    end:   usize,
  },
  /// A structurally valid node had no unambiguous edit placement.
  #[error("unsupported structural placement at `{path}`: {reason}")]
  UnsupportedPlacement {
    /// Exact path involved in the operation.
    path:   ExactPath,
    /// Actionable placement reason.
    reason: Arc<str>,
  },
}

/// One table header and the complete source block that it owns.
#[derive(Clone, Debug)]
struct HeaderRecord {
  /// Exact header path.
  path:  ExactPath,
  /// Regular-table or array-of-tables kind.
  kind:  TomlKind,
  /// Header syntax without attached trivia.
  core:  Range<usize>,
  /// Header key syntax without brackets or trivia.
  key:   Range<usize>,
  /// Attached header and owned content, excluding detached following trivia.
  block: Range<usize>,
}

impl HeaderRecord {
  /// Clone this record while replacing only the complete owned block range.
  fn with_block(&self, block: Range<usize>) -> Self {
    Self {
      block,
      ..self.clone()
    }
  }
}

/// Source ranges that define one inline-array element and its attached trivia.
#[derive(Clone, Debug)]
struct ArrayElementRanges {
  /// Exact value syntax.
  core:     Range<usize>,
  /// End of the value's containing source line within the array.
  line_end: usize,
  /// Leading standalone comments and optional trailing same-line comment.
  attached: Range<usize>,
}

/// One blank-separated array comment retained at its structural slot.
#[derive(Clone, Debug)]
struct DetachedArrayComment {
  /// Number of original elements that preceded the comment.
  slot:   usize,
  /// Exact comment token source.
  source: Arc<str>,
}

/// Exact trivia between two otherwise contiguous table elements.
#[derive(Clone, Debug)]
struct TableBlockGap {
  /// Number of original table elements that preceded the gap.
  slot:   usize,
  /// Exact whitespace and blank-separated comment source.
  source: Arc<str>,
}

/// Convert a Rowan source range without truncation.
fn text_range(range: TextRange) -> Result<Range<usize>, RewriteError> {
  let start = usize::try_from(u32::from(range.start())).map_err(|_conversion_error| RewriteError::InvalidSourceRange {
    start: usize::MAX,
    end:   usize::MAX,
  })?;
  let end = usize::try_from(u32::from(range.end())).map_err(|_conversion_error| RewriteError::InvalidSourceRange {
    start,
    end: usize::MAX,
  })?;
  Ok(start..end)
}

/// Return the structural TOML kind represented by a DOM node.
fn node_kind(node: &Node) -> TomlKind {
  match *node {
    Node::Table(ref table) => match table.kind() {
      TableKind::Inline => TomlKind::InlineTable,
      TableKind::Regular | TableKind::Pseudo => TomlKind::Table,
    },
    Node::Array(ref array) => match array.kind() {
      ArrayKind::Inline => TomlKind::Array,
      ArrayKind::Tables => TomlKind::ArrayOfTables,
    },
    Node::Bool(_) => TomlKind::Boolean,
    Node::Str(_) => TomlKind::String,
    Node::Integer(_) => TomlKind::Integer,
    Node::Float(_) => TomlKind::Float,
    Node::Date(_) => TomlKind::DateTime,
    Node::Invalid(_) => TomlKind::Invalid,
  }
}

/// Find the closest key/value entry that owns a DOM node's syntax.
fn entry_syntax(node: &Node) -> Option<SyntaxNode> {
  node.syntax()?.ancestors().find(|ancestor| ancestor.kind() == SyntaxKind::ENTRY)
}

/// Extract a literal path from one entry's key syntax.
fn path_from_entry(entry: &SyntaxNode) -> Result<ExactPath, RewriteError> {
  path_from_key_owner(entry, FragmentKind::Entry)
}

/// Extract a literal path from one table header.
#[allow(
  clippy::single_call_fn,
  reason = "the named wrapper binds header key extraction to its FragmentKind diagnostic beside path_from_entry"
)]
fn path_from_header(header: &SyntaxNode) -> Result<ExactPath, RewriteError> {
  path_from_key_owner(header, FragmentKind::TableBlock)
}

/// Extract a literal path from the key syntax owned by one typed fragment.
fn path_from_key_owner(owner: &SyntaxNode, kind: FragmentKind) -> Result<ExactPath, RewriteError> {
  let Some(key) = owner.children().find(|child| child.kind() == SyntaxKind::KEY) else {
    let reason = match kind {
      FragmentKind::Entry => "the entry has no key syntax",
      FragmentKind::TableBlock => "the table header has no key syntax",
      FragmentKind::Value | FragmentKind::ArrayElement => "the fragment kind cannot own key syntax",
    };
    return Err(RewriteError::InvalidFragment {
      kind,
      reason: reason.into(),
    });
  };
  Ok(ExactPath::from_segments(
    keys_from_syntax(&key.into()).map(|segment| Arc::<str>::from(segment.value())),
  ))
}

/// Return the source interior between one pair of delimiters.
fn delimited_interior(syntax: &SyntaxNode, open_kind: SyntaxKind, close_kind: SyntaxKind) -> Result<Range<usize>, RewriteError> {
  let Some(open) = syntax.children_with_tokens().find(|element| element.kind() == open_kind) else {
    return Err(RewriteError::InvalidSourceRange {
      start: 0, end: 0
    });
  };
  let Some(close) = syntax
    .children_with_tokens()
    .filter(|element| element.kind() == close_kind)
    .last()
  else {
    return Err(RewriteError::InvalidSourceRange {
      start: 0, end: 0
    });
  };
  let open_range = text_range(open.text_range())?;
  let close_range = text_range(close.text_range())?;
  if open_range.end > close_range.start {
    return Err(RewriteError::InvalidSourceRange {
      start: open_range.end,
      end:   close_range.start,
    });
  }
  Ok(open_range.end..close_range.start)
}

/// Render one literal TOML key segment without changing its value.
fn render_key(key: &str) -> String {
  if !key.is_empty()
    && key
      .chars()
      .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
  {
    return key.into();
  }
  let mut rendered = String::from("\"");
  for character in key.chars() {
    match character {
      '\"' => rendered.push_str("\\\""),
      '\\' => rendered.push_str("\\\\"),
      '\n' => rendered.push_str("\\n"),
      '\r' => rendered.push_str("\\r"),
      '\t' => rendered.push_str("\\t"),
      '\u{0008}' => rendered.push_str("\\b"),
      '\u{000C}' => rendered.push_str("\\f"),
      control if control.is_control() => {
        let escaped_control = format!("\\u{:04X}", u32::from(control));
        rendered.push_str(&escaped_control);
      }
      other => rendered.push(other),
    }
  }
  rendered.push('\"');
  rendered
}

/// Infer indentation from a multiline source interior.
fn infer_indent(source: &str) -> Option<&str> {
  source.lines().skip(1).find_map(|line| {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
      None
    } else {
      line.get(..line.len().saturating_sub(trimmed.len()))
    }
  })
}

/// Render ordered array fragments using the existing array's broad style.
#[allow(
  clippy::single_call_fn,
  reason = "the named renderer owns array element ordering, comment slots, comma style, and multiline preservation together"
)]
fn render_array_elements(original: &str, elements: &[ArrayElementFragment], detached_comments: &[DetachedArrayComment]) -> String {
  if elements.is_empty() {
    if detached_comments.is_empty() {
      if original.trim().is_empty() {
        return original.into();
      }
      return String::new();
    }
    let indent = infer_indent(original).unwrap_or("  ");
    let closing_indent = original.rsplit_once('\n').map_or("", |(_, trailing)| trailing);
    let mut rendered = String::from("\n");
    render_detached_comments(&mut rendered, indent, detached_comments.iter());
    rendered.push_str(closing_indent);
    return rendered;
  }
  let multiline = original.contains('\n')
    || !detached_comments.is_empty()
    || elements
      .iter()
      .any(|element| !element.leading_comments.is_empty() || element.trailing_comment.is_some());
  if !multiline {
    let leading = original
      .get(..original.len().saturating_sub(original.trim_start().len()))
      .unwrap_or("");
    let trailing_start = original.trim_end().len();
    let trailing = original.get(trailing_start..).unwrap_or("");
    let trailing_comma = original.get(..trailing_start).is_some_and(|content| content.ends_with(','));
    let values = elements
      .iter()
      .map(|element| element.value.as_str())
      .collect::<Vec<_>>()
      .join(", ");
    let comma = if trailing_comma { "," } else { "" };
    return format!("{leading}{values}{comma}{trailing}");
  }

  let indent = infer_indent(original).unwrap_or("  ");
  let closing_indent = original.rsplit_once('\n').map_or("", |(_, trailing)| trailing);
  let trailing_comma = original.trim_end().ends_with(',');
  let mut rendered = String::from("\n");
  for (position, element) in elements.iter().enumerate() {
    let detached_at_slot = detached_comments.iter().filter(|comment| comment.slot == position);
    render_detached_comments(&mut rendered, indent, detached_at_slot);
    for comment in element.leading_comments.iter() {
      rendered.push_str(indent);
      rendered.push_str(comment);
      rendered.push('\n');
    }
    rendered.push_str(indent);
    rendered.push_str(element.value.as_str());
    let is_last = position.checked_add(1).is_some_and(|next| next == elements.len());
    if !is_last || trailing_comma {
      rendered.push(',');
    }
    if let Some(comment) = element.trailing_comment.as_ref() {
      rendered.push(' ');
      rendered.push_str(comment);
    }
    rendered.push('\n');
  }
  let detached_after_elements = detached_comments.iter().filter(|comment| comment.slot >= elements.len());
  render_detached_comments(&mut rendered, indent, detached_after_elements);
  rendered.push_str(closing_indent);
  rendered
}

/// Render unrelated array comments without attaching them to a moved element.
fn render_detached_comments<'comment>(rendered: &mut String, indent: &str, comments: impl Iterator<Item = &'comment DetachedArrayComment>) {
  let mut found = false;
  for comment in comments {
    rendered.push_str(indent);
    rendered.push_str(&comment.source);
    rendered.push('\n');
    found = true;
  }
  if found {
    rendered.push('\n');
  }
}

/// Render table blocks while retaining detached inter-block trivia at its slot.
fn render_table_blocks(blocks: &[TableBlockFragment], gaps: &[TableBlockGap]) -> String {
  let mut rendered = String::new();
  for (position, block) in blocks.iter().enumerate() {
    for gap in gaps.iter().filter(|gap| gap.slot == position) {
      rendered.push_str(&gap.source);
    }
    if !rendered.is_empty() && !rendered.ends_with('\n') {
      rendered.push('\n');
    }
    rendered.push_str(block.as_str());
  }
  for gap in gaps.iter().filter(|gap| gap.slot >= blocks.len()) {
    rendered.push_str(&gap.source);
  }
  rendered
}

#[cfg(test)]
/// Exact-path query, fragment, transaction, trivia, and structural reconciliation contracts.
mod tests {
  use core::fmt::Debug;
  use core::iter::empty;
  use core::ops::Range;
  use core::slice::from_ref;

  use strict_test_support::ensure_that;

  use super::ArrayElementFragment;
  use super::EditOutcome;
  use super::EntryFragment;
  use super::ExactPath;
  use super::FragmentKind;
  use super::PendingPatch;
  use super::RemoveEmptyParents;
  use super::Rewrite;
  use super::RewriteError;
  use super::SemanticDiagnostic;
  use super::SemanticDiagnosticKind;
  use super::TableBlockFragment;
  use super::TomlKind;
  use super::ValueFragment;
  use crate::dom::Key;
  use crate::dom::Keys;
  use crate::dom::error::QueryError;
  use crate::parser::parse;

  /// A complete transaction, its native edit observations, and its render result.
  type RenderedTransaction<T> = Result<(Rewrite, T, Result<String, RewriteError>), RewriteError>;

  /// Entry validation and the resulting native insertion outcome.
  type EntryInsertion = Result<(EntryFragment, Result<EditOutcome, RewriteError>), RewriteError>;

  /// Original and reordered array fragments with their native reconciliation outcome.
  type ArrayReordering = Result<
    (
      Vec<ArrayElementFragment>,
      Vec<ArrayElementFragment>,
      Result<EditOutcome, RewriteError>,
    ),
    RewriteError,
  >;

  /// Patch snapshots from all four ordered key renames.
  type RenameObservations = [Result<Vec<PendingPatch>, RewriteError>; 4];

  /// Preserve an edit's native observations alongside its owning transaction and render result.
  fn edit_document<T>(source: &str, edit: impl FnOnce(&mut Rewrite) -> T) -> RenderedTransaction<T> {
    Rewrite::parse(source).map(|mut document| {
      let observations = edit(&mut document);
      let rendered = document.render();
      (document, observations, rendered)
    })
  }

  /// Insert an entry while retaining both its validated fragment and native edit result.
  fn insert_document(source: &str, parent: &ExactPath, entry_source: &str) -> RenderedTransaction<EntryInsertion> {
    edit_document(source, |document| {
      EntryFragment::parse(entry_source).map(|entry| {
        let inserted = document.insert_entry(parent, &entry);
        (entry, inserted)
      })
    })
  }

  /// Preserve extracted array fragments and their reordered copies with the native edit result.
  fn reorder_array(document: &mut Rewrite, values: &ExactPath) -> ArrayReordering {
    document.array_elements(values).map(|mut fragments| {
      let original = fragments.clone();
      fragments.reverse();
      let outcome = document.reconcile_array(values, &fragments);
      (original, fragments, outcome)
    })
  }

  /// Retain raw patch admission and rendering together with the uncommitted transaction.
  fn raw_patch_document(source: &str, range: Range<usize>, replacement: &str) -> RenderedTransaction<Result<(), RewriteError>> {
    edit_document(source, |document| document.push_std_patch(range, replacement.into()))
  }

  /// Enforce the rewrite API's concurrent-ownership contract at compile time.
  #[allow(
    clippy::single_call_fn,
    reason = "the generic constraint gives the concurrency contract test one explicit compile-time assertion boundary"
  )]
  fn require_send_sync<T: Send + Sync>() {}

  /// Retain each accepted rename's patch snapshot before rendering the complete transaction.
  fn nested_renames(source: &str, deepest_query: &str) -> RenderedTransaction<RenameObservations> {
    edit_document(source, |document| {
      [
        ("table", "table_new"),
        ("table.middle", "middle_new"),
        ("table.middle.inner", "inner_new"),
        (deepest_query, "inner2_new"),
      ]
      .map(|(query, replacement)| {
        document
          .rename_keys(query, replacement)
          .map(|updated| updated.patches().to_vec())
      })
    })
  }

  /// Apply differently sized key replacements in descending source order.
  #[test]
  fn rename_keys() -> Result<(), impl Debug> {
    let observed = nested_renames("\n[table.middle.inner]\n[table.middle.inner.inner]\n", "table.middle.inner.inner");
    ensure_that(observed, "different-length replacements must retain source order", |result| {
      let &Ok((_, ref edits, ref rendered)) = result else {
        return false;
      };

      edits.iter().all(Result::is_ok)
        && rendered
          .as_ref()
          .is_ok_and(|text| text == "\n[table_new.middle_new.inner_new]\n[table_new.middle_new.inner_new.inner2_new]\n")
    })
    .map(drop)
    .map_err(Box::new)
  }

  /// Rename every matching segment across repeated and nested array-of-tables headers.
  #[test]
  fn rename_keys_array_of_tables() -> Result<(), impl Debug> {
    let observed = nested_renames(
      "\n[[table.middle.inner]]\n[[table.middle.inner]]\n[table.middle.inner.inner]\n",
      "table.middle.inner.*.inner",
    );
    ensure_that(observed, "all matching array-table ranges must be rewritten", |result| {
      let &Ok((_, ref edits, ref rendered)) = result else {
        return false;
      };

      edits.iter().all(Result::is_ok)
        && rendered.as_ref().is_ok_and(|text| {
          text == "\n[[table_new.middle_new.inner_new]]\n[[table_new.middle_new.inner_new]]\n[table_new.middle_new.inner_new.inner2_new]\n"
        })
    })
    .map(drop)
    .map_err(Box::new)
  }

  /// Keep exact paths literal while reserving an empty path for the document root.
  #[test]
  fn exact_paths_are_literal_and_root_is_empty() -> Result<(), impl Debug> {
    let fixture_quoted = ExactPath::try_from("patch.\"https://example.com/repo\"").map(|original| {
      let child = original.child("feature flags");
      let extended = child.extend(&ExactPath::from_segments(["*", "line\nbreak"]));
      (original, child, extended)
    });
    ensure_that(
      (
        ExactPath::parse(""),
        fixture_quoted,
        ExactPath::parse("items.*"),
        ExactPath::parse("items.\"*\""),
        ExactPath::parse("items[0]"),
      ),
      "exact paths must preserve literal segments and reject query syntax",
      |root_fields| {
        let (ref root, ref quoted, ref glob, ref literal, ref index) = *root_fields;
        root.as_ref().is_ok_and(|path| {
          path.is_root()
            && path.is_empty()
            && path.key().is_none()
            && path.parent().is_none()
            && path == &ExactPath::from_segments(empty::<&str>())
            && path.to_string().is_empty()
        }) && quoted.as_ref().is_ok_and(|original_fields| {
          let (ref original, ref child, ref extended) = *original_fields;
          original.segments().eq(["patch", "https://example.com/repo"])
            && child.parent().as_ref() == Some(original)
            && extended
              .segments()
              .eq(["patch", "https://example.com/repo", "feature flags", "*", "line\nbreak"])
        }) && matches!(glob, Err(RewriteError::InvalidPath { .. }))
          && literal.as_ref().is_ok_and(|path| path.key() == Some("*"))
          && matches!(index, Err(RewriteError::InvalidPath { .. }))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Render every literal key class without changing its decoded value.
  #[test]
  fn exact_paths_render_literal_keys_canonically() -> Result<(), impl Debug> {
    let observed = [
      ("bare_1-key", "bare_1-key"),
      ("", "\"\""),
      ("with space", "\"with space\""),
      ("\"", "\"\\\"\""),
      ("\\", "\"\\\\\""),
      ("\n", "\"\\n\""),
      ("\r", "\"\\r\""),
      ("\t", "\"\\t\""),
      ("\u{0008}", "\"\\b\""),
      ("\u{000c}", "\"\\f\""),
      ("\u{0001}", "\"\\u0001\""),
      ("caf\u{e9}", "\"caf\u{e9}\""),
    ]
    .map(|(key, expected)| {
      let path = ExactPath::from_segments([key]);
      let rendered = path.to_string();
      let reparsed = ExactPath::parse(&rendered);
      (key, expected, path, rendered, reparsed)
    });
    ensure_that(observed, "canonical path spellings must round-trip every literal key", |cases| {
      cases.iter().all(|key_fields| {
        let (key, ref expected, _, ref rendered, ref reparsed) = *key_fields;
        rendered == expected && reparsed.as_ref().is_ok_and(|path| path.segments().eq([key]))
      })
    })
    .map(drop)
    .map_err(Box::new)
  }

  /// Keep public fieldless-enum rendering stable for diagnostics and command output.
  #[test]
  fn public_enum_display_is_stable() -> Result<(), impl Debug> {
    let fixture_kinds = [
      (TomlKind::Table, "Table"),
      (TomlKind::InlineTable, "InlineTable"),
      (TomlKind::Array, "Array"),
      (TomlKind::ArrayOfTables, "ArrayOfTables"),
      (TomlKind::String, "String"),
      (TomlKind::Integer, "Integer"),
      (TomlKind::Float, "Float"),
      (TomlKind::Boolean, "Boolean"),
      (TomlKind::DateTime, "DateTime"),
      (TomlKind::Invalid, "Invalid"),
    ];
    let fixture_outcomes = [
      (EditOutcome::Unchanged, "Unchanged"),
      (EditOutcome::Inserted, "Inserted"),
      (EditOutcome::Replaced, "Replaced"),
      (EditOutcome::Removed, "Removed"),
    ];
    ensure_that(
      (fixture_kinds, fixture_outcomes),
      "public enums must retain their stable variant-name rendering",
      |kinds_fields| {
        let (ref kinds, ref outcomes) = *kinds_fields;
        kinds.iter().all(|&(kind, expected)| kind.to_string() == expected)
          && outcomes.iter().all(|&(outcome, expected)| outcome.to_string() == expected)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Reject both parser and DOM diagnostics before a rewrite transaction is created.
  #[test]
  fn parse_rejects_syntax_and_semantic_diagnostics() -> Result<(), impl Debug> {
    ensure_that(
      (Rewrite::parse("value = [1 2]"), Rewrite::parse("value = 1\nvalue = 2\n")),
      "rewrites must reject syntax and semantic diagnostics with their native evidence",
      |syntax_fields| {
        let (ref syntax, ref semantic) = *syntax_fields;
        matches!(syntax, Err(RewriteError::SyntaxDiagnostics { .. }))
          && matches!(semantic, Err(RewriteError::SemanticDiagnostics { diagnostics }) if diagnostics.first().is_some_and(|diagnostic|
            diagnostic.kind() == SemanticDiagnosticKind::ConflictingKeys && diagnostic.range().is_some()
              && diagnostic.related_range().is_some() && !diagnostic.message().is_empty()))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Project every DOM diagnostic family into owned typed ranges and messages.
  #[test]
  fn semantic_diagnostic_projection_preserves_all_families() -> Result<(), impl Debug> {
    let fixture_cases = [
      (
        "missing =\nnext = 1\n",
        SemanticDiagnosticKind::UnexpectedSyntax,
        false,
        false,
        "the syntax was not expected here:",
      ),
      (
        "\"\\q\" = 1\n",
        SemanticDiagnosticKind::InvalidEscapeSequence,
        true,
        false,
        "the string contains invalid escape sequence(s)",
      ),
      (
        "value = 999999999999999999999999999999\n",
        SemanticDiagnosticKind::MalformedScalar,
        true,
        false,
        "the integer scalar could not be decoded:",
      ),
      (
        "a = 1\na = 2\n",
        SemanticDiagnosticKind::ConflictingKeys,
        true,
        true,
        "conflicting keys",
      ),
      (
        "a = 1\n[a.b]\n",
        SemanticDiagnosticKind::ExpectedTable,
        true,
        true,
        "expected table",
      ),
      (
        "a = 1\n[[a]]\n",
        SemanticDiagnosticKind::ExpectedArrayOfTables,
        true,
        true,
        "expected array of tables",
      ),
    ]
    .map(|(source, kind, nonempty, related, message)| {
      let observed = parse(source).map(|parsed| {
        let root = parsed.clone().into_dom();
        let diagnostics = root.validate();
        let projected = diagnostics
          .as_ref()
          .err()
          .map(|errors| errors.iter().cloned().map(SemanticDiagnostic::from_dom).collect::<Vec<_>>());
        (parsed, root, diagnostics, projected)
      });
      (kind, nonempty, related, message, observed)
    });
    ensure_that(
      fixture_cases,
      "semantic projection must preserve each diagnostic's message and exact range polarity",
      |cases| {
        cases.iter().all(|kind_fields| {
          let (ref kind, ref nonempty, ref related, ref message, ref observed) = *kind_fields;
          matches!(observed, &Ok((_, _, ref diagnostics, Some(ref projected))) if diagnostics.is_err()
            && projected.iter().find(|diagnostic| diagnostic.kind() == *kind).is_some_and(|diagnostic|
              diagnostic.range().is_some_and(|range|
                diagnostic.message().starts_with(message)
                && diagnostic.to_string() == diagnostic.message()
                && range.start() <= range.end()
                && (range.start() < range.end()) == *nonempty
                && diagnostic.related_range().is_some() == *related
                && diagnostic.related_range().is_none_or(|related_range| related_range.start() < related_range.end()))))
        })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Require owned rewrite failures to cross concurrent execution boundaries safely.
  #[test]
  fn rewrite_errors_are_send_and_sync() {
    require_send_sync::<RewriteError>();
  }

  /// Render an untouched non-ASCII document without changing any UTF-8 byte.
  #[test]
  fn untouched_and_non_ascii_documents_render_byte_for_byte() -> Result<(), impl Debug> {
    let source = "# caf\u{e9}\n\"\u{43a}\u{43b}\u{44e}\u{447}\" = \"\u{5024}\"\n";
    let observed = Rewrite::parse(source).map(|document| {
      let rendered = document.render();
      (document, rendered)
    });
    ensure_that(observed, "untouched UTF-8 bytes must be identical", |result| {
      result
        .as_ref()
        .is_ok_and(|rendered_document| rendered_document.1.as_ref().is_ok_and(|text| text == source))
    })
    .map(drop)
    .map_err(Box::new)
  }

  /// Forward `Display` through the same validated source transaction as `render`.
  #[test]
  fn display_matches_render_for_pending_rewrites() -> Result<(), impl Debug> {
    let observed = edit_document("value = 1\n", |document| {
      ValueFragment::parse("2").map(|replacement| {
        let edited = document.replace_value(&ExactPath::from_segments(["value"]), &replacement);
        (replacement, edited)
      })
    });
    ensure_that(
      observed,
      "Display must publish the same complete source as the fallible renderer",
      |result| {
        let &Ok((ref document, ref edited, ref rendered)) = result else {
          return false;
        };

        edited.as_ref().is_ok_and(|edit| matches!(edit.1, Ok(EditOutcome::Replaced)))
          && rendered.as_ref().is_ok_and(|text| document.to_string() == *text)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Return exact source slices and structural kinds for values and entries.
  #[test]
  fn values_and_entries_are_exact_and_typed() -> Result<(), impl Debug> {
    let observed = Rewrite::parse("# attached\nvalue  =  [1, 2] # trailing\nother = true\n").map(|document| {
      let path = ExactPath::from_segments(["value"]);
      let value = document.value_fragment(&path);
      let entry = document.entry_fragment(&path);
      (document, path, value, entry)
    });
    ensure_that(
      observed,
      "exact queries and fragments must preserve source spelling, trivia, and structural kinds",
      |result| {
        let &Ok((ref document, ref path, ref value, ref entry)) = result else {
          return false;
        };

        document
          .value(path)
          .is_ok_and(|view| view.text() == "[1, 2]" && view.kind() == TomlKind::Array)
          && document.entry(path).is_ok_and(|view| {
            view.text() == "value  =  [1, 2] # trailing" && view.kind() == TomlKind::Array && view.value().text() == "[1, 2]"
          })
          && value
            .as_ref()
            .is_ok_and(|fragment| fragment.as_str() == "[1, 2]" && fragment.kind() == TomlKind::Array)
          && entry.as_ref().is_ok_and(|fragment| {
            fragment.as_str().contains("# attached")
              && fragment.as_str().contains("# trailing")
              && fragment.key() == path
              && fragment.value().kind() == TomlKind::Array
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Classify every source-backed value family through the public exact-value query.
  #[test]
  fn value_queries_classify_scalar_and_composite_families() -> Result<(), impl Debug> {
    let observed = Rewrite::parse(
      "boolean = true\nstring = \"value\"\ninteger = 1\nfloat = 1.5\ndate = 1979-05-27T07:32:00Z\narray = [1]\ninline = \
       {}\n[regular]\nchild = 1\n[[items]]\nchild = 2\n",
    );
    ensure_that(
      observed,
      "exact value queries must preserve every structural TOML family",
      |result| {
        let Ok(ref document) = *result else {
          return false;
        };

        [
          ("boolean", TomlKind::Boolean),
          ("string", TomlKind::String),
          ("integer", TomlKind::Integer),
          ("float", TomlKind::Float),
          ("date", TomlKind::DateTime),
          ("array", TomlKind::Array),
          ("inline", TomlKind::InlineTable),
          ("regular", TomlKind::Table),
          ("items", TomlKind::ArrayOfTables),
        ]
        .iter()
        .all(|query_fields| {
          let (query, ref expected) = *query_fields;
          document
            .value(&ExactPath::from_segments([query]))
            .is_ok_and(|view| view.kind() == *expected)
        })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Reject document roots and table headers where a key/value entry is required.
  #[test]
  fn entry_operations_reject_non_entry_paths_without_mutation() -> Result<(), impl Debug> {
    let table = ExactPath::from_segments(["table"]);
    let observed = edit_document("[table]\nvalue = 1\n", |document| {
      document.remove_entry(&table, RemoveEmptyParents::Keep)
    });
    ensure_that(
      observed,
      "non-entry queries and removals must retain their typed failures without patches",
      |result| {
        let &Ok((ref document, ref removed, _)) = result else {
          return false;
        };

        matches!(document.entry(&ExactPath::default()), Err(RewriteError::MissingPath { .. }))
          && matches!(document.entry(&table), Err(RewriteError::UnsupportedPlacement { .. }))
          && matches!(removed, Err(RewriteError::UnsupportedPlacement { .. }))
          && document.patches().is_empty()
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Expose validated array-element and table-block metadata without reparsing at call sites.
  #[test]
  fn structural_fragments_expose_their_validated_metadata() -> Result<(), impl Debug> {
    let block_source = "# block\n[[items]]\nname = \"value\"\n";
    ensure_that(
      (
        ArrayElementFragment::parse("# leading\n\"value\" # trailing"),
        TableBlockFragment::parse(block_source),
      ),
      "structural fragments must retain complete source spelling and validated metadata",
      |element_fields| {
        let (ref element, ref block) = *element_fields;
        element.as_ref().is_ok_and(|fragment| {
          fragment.as_str() == "# leading\n\"value\" # trailing"
            && fragment.value().as_str() == "\"value\""
            && fragment.kind() == TomlKind::String
        }) && block.as_ref().is_ok_and(|fragment| {
          fragment.as_str() == block_source
            && fragment.kind() == TomlKind::ArrayOfTables
            && fragment.path() == &ExactPath::from_segments(["items"])
        })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Limit an extracted inline entry to its own key, value, and internal trivia.
  #[test]
  fn inline_entry_fragments_stay_inside_the_inline_table() -> Result<(), impl Debug> {
    let observed = Rewrite::parse("dependency = { version = \"2\", features = [\"std\"] }\n").map(|document| {
      let fragment = document.entry_fragment(&ExactPath::from_segments(["dependency", "version"]));
      (document, fragment)
    });
    ensure_that(
      observed,
      "inline extraction must exclude the outer entry and sibling fields",
      |result| {
        result.as_ref().is_ok_and(|entry_observation| {
          entry_observation
            .1
            .as_ref()
            .is_ok_and(|entry| entry.as_str() == "version = \"2\"")
        })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Replace only a value range and publish source, DOM, and patches transactionally.
  #[test]
  fn replace_value_preserves_neighboring_bytes_and_commits_transactionally() -> Result<(), impl Debug> {
    let source = "# keep\nvalue  =  1 # keep too\nneighbor = 2\n";
    let observed = edit_document(source, |document| {
      let fragments = [ValueFragment::parse("1"), ValueFragment::parse("\"caf\u{e9}\"")];
      let changes = match &fragments {
        &[Ok(ref unchanged), Ok(ref replacement)] => {
          let path = ExactPath::from_segments(["value"]);
          let first = document.replace_value(&path, unchanged);
          let initial_patches = document.patches().to_vec();
          let second = document.replace_value(&path, replacement);
          let rendered = document.render();
          let committed = document.commit();
          Some((first, initial_patches, second, rendered, committed))
        }
        _ => None,
      };
      (fragments, changes)
    });
    let expected = "# keep\nvalue  =  \"caf\u{e9}\" # keep too\nneighbor = 2\n";
    ensure_that(
      observed,
      "replacement and commit must publish source and DOM together while preserving neighboring bytes",
      |result| {
        let &Ok((ref document, (_, ref changes), ref rendered)) = result else {
          return false;
        };

        changes.as_ref().is_some_and(|first_fields| {
          let (ref first, ref initial_patches, ref second, ref before, ref committed) = *first_fields;
          matches!(first, Ok(EditOutcome::Unchanged))
            && initial_patches.is_empty()
            && matches!(second, Ok(EditOutcome::Replaced))
            && before.as_ref().is_ok_and(|text| text == expected)
            && committed.is_ok()
        }) && document.patches().is_empty()
          && document.source() == expected
          && rendered.as_ref().is_ok_and(|text| text == expected)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Leave committed source and queued patches intact when validation rejects a commit.
  #[test]
  fn failed_commit_retains_source_and_patches() -> Result<(), impl Debug> {
    let observed = edit_document("value = 1\n", |document| {
      let renamed = document
        .rename_keys("value", "bad key")
        .map(|updated| updated.patches().to_vec());
      let committed = document.commit();
      (renamed, committed)
    });
    ensure_that(
      observed,
      "rejected commit must retain committed source and pending patches",
      |result| {
        let &Ok((ref document, (ref renamed, ref committed), _)) = result else {
          return false;
        };

        renamed.is_ok()
          && matches!(committed, Err(RewriteError::SyntaxDiagnostics { .. }))
          && document.source() == "value = 1\n"
          && document.patches().len() == 1
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Insert entries at root, regular-table, and inline-table ownership boundaries.
  #[test]
  fn insert_and_upsert_cover_root_regular_and_inline_tables() -> Result<(), impl Debug> {
    let fixture_root = insert_document("[table]\nold = 1\n", &ExactPath::default(), "# new\nroot = true\n");
    let fixture_cases = [
      (
        "[table]\nold = 1\n\n[other]\nx = 2\n",
        "table",
        "[table]\nold = 1\nnew = 2\n\n[other]\nx = 2\n",
      ),
      ("value = { old = 1 }\n", "value", "value = { old = 1, new = 2 }\n"),
      ("value = {}\n", "value", "value = { new = 2 }\n"),
      ("value = {\n  old = 1,\n  }\n", "value", "value = {\n  old = 1,\n  new = 2\n  }\n"),
      ("value = {\n  old = 1\n  }\n", "value", "value = {\n  old = 1,\n  new = 2\n  }\n"),
    ]
    .map(|(source, parent, expected)| (expected, insert_document(source, &ExactPath::from_segments([parent]), "new = 2")));
    ensure_that(
      (fixture_root, fixture_cases),
      "entry insertion must preserve root, table, and inline ownership boundaries",
      |root_fields| {
        let (ref root, ref cases) = *root_fields;
        root.as_ref().is_ok_and(|inserted_fields| {
          let (_, ref inserted, ref rendered) = *inserted_fields;
          inserted.as_ref().is_ok_and(|edit| matches!(edit.1, Ok(EditOutcome::Inserted)))
            && rendered
              .as_ref()
              .is_ok_and(|text| text.starts_with("# new\nroot = true\n\n[table]"))
        }) && cases.iter().all(|expected_fields| {
          let (ref expected, ref result) = *expected_fields;
          matches!(result, &Ok((_, ref inserted, ref rendered)) if inserted
            .as_ref()
            .is_ok_and(|edit| matches!(edit.1, Ok(EditOutcome::Inserted)))
            && rendered.as_ref().is_ok_and(|text| text == expected))
        })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Distinguish duplicate, unchanged, replacement, and insertion outcomes at one table boundary.
  #[test]
  fn entry_upserts_and_table_creation_report_exact_outcomes() -> Result<(), impl Debug> {
    let observed = edit_document("root = 1\n[table]\nold = 1\n", |document| {
      let table = ExactPath::from_segments(["table"]);
      let creations = [document.create_tables(&ExactPath::default()), document.create_tables(&table)];
      let fragments = [
        EntryFragment::parse("old = 1"),
        EntryFragment::parse("old = 2"),
        EntryFragment::parse("new = true"),
      ];
      let edits = match &fragments {
        &[Ok(ref existing), Ok(ref replacement), Ok(ref inserted)] => Some([
          document.insert_entry(&table, existing),
          document.upsert_entry(&table, existing),
          document.upsert_entry(&table, replacement),
          document.upsert_entry(&table, inserted),
        ]),
        _ => None,
      };
      (creations, fragments, edits)
    });
    ensure_that(
      observed,
      "creation and upsert must report duplicate, unchanged, replaced, and inserted outcomes exactly",
      |result| {
        let &Ok((_, (ref creations, _, ref edits), ref rendered)) = result else {
          return false;
        };

        creations.iter().all(|outcome| matches!(outcome, Ok(EditOutcome::Unchanged)))
          && matches!(
            edits,
            Some([
              Err(RewriteError::AmbiguousMatches {
                count: 1,
                ..
              }),
              Ok(EditOutcome::Unchanged),
              Ok(EditOutcome::Replaced),
              Ok(EditOutcome::Inserted)
            ])
          )
          && rendered
            .as_ref()
            .is_ok_and(|text| text == "root = 1\n[table]\nold = 2\nnew = true\n")
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Replace and insert exact values while rejecting root replacement before mutation.
  #[test]
  fn value_upserts_replace_insert_and_reject_the_document_root() -> Result<(), impl Debug> {
    let fixture_cases = [
      (
        "[table]\nvalue = 1\n",
        ExactPath::from_segments(["table", "value"]),
        Some((EditOutcome::Replaced, "[table]\nvalue = 2\n")),
      ),
      (
        "[table]\nvalue = 1\n",
        ExactPath::from_segments(["table", "added"]),
        Some((EditOutcome::Inserted, "[table]\nvalue = 1\nadded = 2\n")),
      ),
      ("value = 1\n", ExactPath::default(), None),
    ]
    .map(|(source, path, expected)| {
      let fragment = ValueFragment::parse("2");
      let observed = edit_document(source, |document| {
        fragment.as_ref().ok().map(|value| document.upsert_value(&path, value))
      });
      (expected, fragment, observed)
    });
    ensure_that(
      fixture_cases,
      "value upserts must distinguish replacement, insertion, and atomic root rejection",
      |cases| {
        cases.iter().all(|expected_fields| {
          let (ref expected, ref fragment, ref result) = *expected_fields;
          fragment.is_ok()
            && matches!(result, &Ok((ref document, Some(ref outcome), ref rendered)) if
              expected.as_ref().is_some_and(|&(expected_outcome, text)|
                outcome.as_ref().is_ok_and(|actual| *actual == expected_outcome) && rendered.as_ref().is_ok_and(|actual| actual == text))
              || expected.is_none() && matches!(outcome, Err(RewriteError::UnsupportedPlacement { .. })) && document.patches().is_empty())
        })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Require a commit between parent-table creation and a dependent child insertion.
  #[test]
  fn explicit_parent_creation_requires_a_separate_committed_phase() -> Result<(), impl Debug> {
    let observed = edit_document("root = true\n", |document| {
      let parent = ExactPath::from_segments(["outer", "inner"]);
      let created = document.create_tables(&parent);
      let committed = document.commit();
      let inserted = EntryFragment::parse("value = 1").map(|fragment| {
        let outcome = document.insert_entry(&parent, &fragment);
        (fragment, outcome)
      });
      (created, committed, inserted)
    });
    ensure_that(
      observed,
      "a committed parent-table phase must support its dependent child insertion",
      |result| {
        let &Ok((_, (ref created, ref committed, ref inserted), ref rendered)) = result else {
          return false;
        };

        matches!(created, Ok(EditOutcome::Inserted))
          && committed.is_ok()
          && inserted.as_ref().is_ok_and(|edit| matches!(edit.1, Ok(EditOutcome::Inserted)))
          && rendered.as_ref().is_ok_and(|text| text.contains("[outer.inner]\nvalue = 1\n"))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Preserve the document boundary while rejecting non-table creation prefixes.
  #[test]
  fn table_creation_preserves_boundaries_and_rejects_non_table_prefixes() -> Result<(), impl Debug> {
    let parent = ExactPath::from_segments(["outer", "inner"]);
    let fixture_valid = [
      ("", "[outer]\n\n[outer.inner]\n"),
      ("root = true", "root = true\n\n[outer]\n\n[outer.inner]\n"),
    ]
    .map(|(source, expected)| (expected, edit_document(source, |document| document.create_tables(&parent))));
    let fixture_invalid = [
      ("outer = 1\n", TomlKind::Integer),
      ("outer = { value = 1 }\n", TomlKind::InlineTable),
    ]
    .map(|(source, kind)| (source, kind, edit_document(source, |document| document.create_tables(&parent))));
    ensure_that(
      (fixture_valid, fixture_invalid),
      "table creation must preserve boundaries and reject incompatible prefixes atomically",
      |valid_fields| {
        let (ref valid, ref invalid) = *valid_fields;
        valid.iter().all(|expected_fields| {
          let (ref expected, ref result) = *expected_fields;
          matches!(result, &Ok((_, Ok(EditOutcome::Inserted), ref rendered)) if rendered.as_ref().is_ok_and(|text| text == expected))
        }) && invalid.iter().all(|source_fields| {
          let (ref source, ref kind, ref result) = *source_fields;
          matches!(result, &Ok((ref document, ref created, ref rendered)) if
            matches!(created, Err(RewriteError::TypeMismatch { expected: "regular table", found, .. }) if found == kind)
            && document.patches().is_empty()
            && rendered.as_ref().is_ok_and(|text| text == source))
        })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Remove one entry with its attached comments while preserving blank-separated trivia.
  #[test]
  fn exact_removal_preserves_siblings_and_comment_boundaries() -> Result<(), impl Debug> {
    let observed = edit_document("# detached\n\n# attached\nremove = 1 # inline\nkeep = 2\n", |document| {
      document.remove_entry(&ExactPath::from_segments(["remove"]), RemoveEmptyParents::Keep)
    });
    ensure_that(
      observed,
      "removal must discard attached comments and retain blank-separated trivia and siblings",
      |result| {
        let &Ok((_, ref removed, ref rendered)) = result else {
          return false;
        };

        matches!(removed, Ok(EditOutcome::Removed)) && rendered.as_ref().is_ok_and(|text| text == "# detached\n\nkeep = 2\n")
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Remove every positional inline-table entry with a valid comma boundary.
  #[test]
  fn inline_table_removal_is_comma_aware_at_every_position() -> Result<(), impl Debug> {
    let fixture_cases = [
      ("first", "value = {middle=2,last=3}\n"),
      ("middle", "value = {first=1,last=3}\n"),
      ("last", "value = {first=1,middle=2}\n"),
    ]
    .map(|(selected, expected)| {
      (
        expected,
        edit_document("value = {first=1,middle=2,last=3}\n", |document| {
          document.remove_entry(&ExactPath::from_segments(["value", selected]), RemoveEmptyParents::Keep)
        }),
      )
    });
    let fixture_only = edit_document("value = {only=1}\n", |document| {
      document.remove_entry(&ExactPath::from_segments(["value", "only"]), RemoveEmptyParents::Prune)
    });
    ensure_that(
      (fixture_cases, fixture_only),
      "inline removal must preserve valid separators and retain an empty owning inline table",
      |cases_fields| {
        let (ref cases, ref only) = *cases_fields;
        cases.iter().all(|expected_fields| {
          let (ref expected, ref result) = *expected_fields;
          matches!(result, &Ok((_, Ok(EditOutcome::Removed), ref rendered)) if rendered.as_ref().is_ok_and(|text| text == expected))
        }) && only.as_ref().is_ok_and(|removed_fields| {
          let (_, ref removed, ref rendered) = *removed_fields;
          matches!(removed, Ok(EditOutcome::Removed)) && rendered.as_ref().is_ok_and(|text| text == "value = {}\n")
        })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Prune only concrete empty ancestors that have no retained comment ownership.
  #[test]
  fn removal_prunes_only_proven_empty_comment_free_parent_tables() -> Result<(), impl Debug> {
    let fixture_cases = [
      (
        "[outer]\n[outer.inner]\nremove = 1\n",
        ExactPath::from_segments(["outer", "inner", "remove"]),
        "",
      ),
      (
        "# retained table context\n[table]\nremove = 1\n",
        ExactPath::from_segments(["table", "remove"]),
        "# retained table context\n[table]\n",
      ),
    ]
    .map(|(source, path, expected)| {
      (
        expected,
        edit_document(source, |document| document.remove_entry(&path, RemoveEmptyParents::Prune)),
      )
    });
    ensure_that(
      fixture_cases,
      "pruning must remove empty ancestors while retaining parents that own comments",
      |cases| {
        cases.iter().all(|expected_fields| {
          let (ref expected, ref result) = *expected_fields;
          matches!(result, &Ok((_, Ok(EditOutcome::Removed), ref rendered)) if rendered.as_ref().is_ok_and(|text| text == expected))
        })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Reconcile array order, attached comments, detached slots, commas, and empty state.
  #[test]
  fn arrays_reconcile_elements_comments_order_and_empty_state() -> Result<(), impl Debug> {
    let values = ExactPath::from_segments(["values"]);
    let original_source = "values = [\n  # detached\n\n  # first\n  \"a\", # inline\n  \"b\",\n]\n";
    let fixture_reordered = edit_document(original_source, |document| reorder_array(document, &values));
    let fixture_empty = [
      ("values = [1, 2]\n", "values = []\n"),
      ("values = [\n  # retained\n\n  1,\n]\n", "values = [\n  # retained\n\n]\n"),
    ]
    .map(|(source, expected)| (expected, edit_document(source, |document| document.reconcile_array(&values, &[]))));
    let no_trailing_source = "values = [\n  1 # last\n]\n";
    let fixture_no_trailing = edit_document(no_trailing_source, |document| {
      document.array_elements(&values).map(|fragments| {
        let outcome = document.reconcile_array(&values, &fragments);
        (fragments, outcome)
      })
    });
    ensure_that((fixture_reordered, fixture_empty, fixture_no_trailing), "array reconciliation must preserve attachment, detached slots, commas, order, and empty-state trivia", |reordered_fields| { let (ref reordered, ref empty, ref no_trailing) = *reordered_fields;
      let &Ok((_, Ok((ref original, _, ref outcome)), Ok(ref text))) = reordered else { return false; };
      let [ref first, ref second] = *original.as_slice() else { return false; };
      let &Ok((_, Ok((_, ref trailing_outcome)), Ok(ref trailing_text))) = no_trailing else { return false; };
      first.as_str().contains("# first") && first.as_str().contains("# inline") && !first.as_str().contains("\"b\"")
        && !first.as_str().contains("# detached") && second.as_str() == "\"b\"" && matches!(outcome, Ok(EditOutcome::Replaced))
        && text.contains("\"b\",") && text.contains("# first") && text.contains("# inline")
        && matches!((text.find("# detached"), text.find("\"b\""), text.find("# first")), (Some(first_position), Some(second_position), Some(third_position)) if first_position < second_position && second_position < third_position)
        && empty.iter().all(|&(expected, ref result)| matches!(result, Ok((_, Ok(EditOutcome::Replaced), Ok(actual))) if actual == expected))
        && matches!(trailing_outcome, Ok(EditOutcome::Unchanged)) && trailing_text == no_trailing_source
    }).map(drop).map_err(Box::new)
  }

  /// Preserve one-line terminal-comma policy and untouched empty-array interior trivia.
  #[test]
  fn one_line_arrays_preserve_terminal_comma_style_and_empty_trivia() -> Result<(), impl Debug> {
    let values = ExactPath::from_segments(["values"]);
    let fixture_cases = [
      (
        "values = [1, 2,   ] # array\n",
        "values = [2, 1,   ] # array\n",
        EditOutcome::Replaced,
        2,
      ),
      ("values = [1, 2   ]\n", "values = [2, 1   ]\n", EditOutcome::Replaced, 2),
      ("values = [   ]\n", "values = [   ]\n", EditOutcome::Unchanged, 0),
    ]
    .map(|(source, expected, effect, count)| {
      let observed = edit_document(source, |document| reorder_array(document, &values));
      (expected, effect, count, observed)
    });
    ensure_that(
      fixture_cases,
      "array reconciliation must retain terminal comma style and empty-array interior trivia",
      |cases| {
        cases.iter().all(|expected_fields| {
          let (ref expected, ref effect, ref count, ref result) = *expected_fields;
          matches!(result, &Ok((_, Ok((ref original, _, Ok(ref outcome))), Ok(ref rendered))) if
            original.len() == *count && outcome == effect && rendered == expected)
        })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Reject incompatible reconciliation targets and fragments before queuing patches.
  #[test]
  fn reconciliation_rejects_wrong_types_and_paths_without_partial_mutation() -> Result<(), impl Debug> {
    let values = ExactPath::from_segments(["values"]);
    let fixture_arrays = [
      ("values = 1\n", TomlKind::Integer),
      ("[[values]]\nname = \"one\"\n", TomlKind::ArrayOfTables),
    ]
    .map(|(source, kind)| (kind, edit_document(source, |document| document.reconcile_array(&values, &[]))));
    let fixture_blocks = edit_document("[[items]]\nname = \"one\"\n", |document| {
      TableBlockFragment::parse("[[other]]\nname = \"two\"\n").map(|fragment| {
        let outcome = document.reconcile_table_blocks(&ExactPath::from_segments(["items"]), from_ref(&fragment));
        (fragment, outcome)
      })
    });
    ensure_that(
      (fixture_arrays, fixture_blocks),
      "wrong array kinds and table-block paths must fail without partial mutation",
      |arrays_fields| {
        let (ref arrays, ref blocks) = *arrays_fields;
        let &Ok((ref block_document, Ok((_, ref block_outcome)), _)) = blocks else {
          return false;
        };
        arrays.iter().all(|kind_fields| {
          let (ref kind, ref result) = *kind_fields;
          matches!(result, &Ok((ref document, ref outcome, _)) if document.patches().is_empty()
            && matches!(outcome, Err(RewriteError::TypeMismatch { expected: "inline array", found, .. }) if found == kind))
        }) && block_document.patches().is_empty()
          && matches!(
            block_outcome,
            Err(RewriteError::InvalidFragment {
              kind: FragmentKind::TableBlock,
              ..
            })
          )
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Extract and reorder complete table blocks without disturbing unrelated blocks.
  #[test]
  fn table_blocks_copy_reorder_and_remove_as_complete_units() -> Result<(), impl Debug> {
    let source = "# one\n[[items]]\nname = \"a\"\n\n# two\n[[items]]\nname = \"b\"\n\n[other]\nkeep = true\n";
    let items = ExactPath::from_segments(["items"]);
    let fixture_observed = edit_document(source, |document| {
      document.table_blocks(&items).map(|mut blocks| {
        let original = blocks.clone();
        let unchanged = document.reconcile_table_blocks(&items, &blocks);
        let unchanged_patches = document.patches().to_vec();
        blocks.reverse();
        let replaced = document.reconcile_table_blocks(&items, &blocks);
        (original, blocks, unchanged, unchanged_patches, replaced)
      })
    });
    let fixture_parsed = TableBlockFragment::parse("# block\n[[items]]\nname = \"x\"\n");
    ensure_that(
      (fixture_observed, fixture_parsed),
      "table blocks must retain exact paths, unchanged transactions, and independent reorder boundaries",
      |observed_fields| {
        let (ref observed, ref parsed) = *observed_fields;
        let &Ok((_, Ok((ref original, _, ref unchanged, ref patches, ref replaced)), Ok(ref text))) = observed else {
          return false;
        };
        let Ok(ref fragment) = *parsed else {
          return false;
        };
        original.len() == 2
          && matches!(unchanged, Ok(EditOutcome::Unchanged))
          && patches.is_empty()
          && matches!(replaced, Ok(EditOutcome::Replaced))
          && text.contains("[other]\nkeep = true")
          && matches!((text.find("name = \"b\""), text.find("name = \"a\"")), (Some(first), Some(second)) if first < second)
          && fragment.path() == &items
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Insert missing table blocks and leave an absent empty sequence unchanged.
  #[test]
  fn missing_table_block_reconciliation_distinguishes_empty_and_inserted() -> Result<(), impl Debug> {
    let items = ExactPath::from_segments(["items"]);
    let fixture_empty = edit_document("", |document| {
      TableBlockFragment::parse("[[items]]\nname = \"only\"\n").map(|fragment| {
        let outcome = document.reconcile_table_blocks(&items, from_ref(&fragment));
        (fragment, outcome)
      })
    });
    let fixture_missing = edit_document("root = true\n", |document| {
      let unchanged = document.reconcile_table_blocks(&items, &[]);
      let inserted = TableBlockFragment::parse("[[items]]\nname = \"new\"\n").map(|fragment| {
        let outcome = document.reconcile_table_blocks(&items, from_ref(&fragment));
        (fragment, outcome)
      });
      (unchanged, inserted)
    });
    let fixture_unterminated = edit_document("root = true", |document| {
      let fragments = [
        TableBlockFragment::parse("[[items]]\nname = \"first\""),
        TableBlockFragment::parse("[[items]]\nname = \"second\""),
      ];
      let outcome = match &fragments {
        &[Ok(ref first), Ok(ref second)] => Some(document.reconcile_table_blocks(&items, &[first.clone(), second.clone()])),
        _ => None,
      };
      (fragments, outcome)
    });
    ensure_that(
      (fixture_empty, fixture_missing, fixture_unterminated),
      "missing table blocks must preserve root and terminal-newline boundaries",
      |empty_fields| {
        let (ref empty, ref missing, ref unterminated) = *empty_fields;
        let &Ok((_, Ok((_, ref empty_outcome)), Ok(ref empty_text))) = empty else {
          return false;
        };
        let &Ok((_, (ref unchanged, Ok((_, ref inserted))), Ok(ref missing_text))) = missing else {
          return false;
        };
        let &Ok((_, (_, ref unterminated_outcome), Ok(ref unterminated_text))) = unterminated else {
          return false;
        };
        matches!(empty_outcome, Ok(EditOutcome::Inserted))
          && empty_text == "[[items]]\nname = \"only\"\n"
          && matches!(unchanged, Ok(EditOutcome::Unchanged))
          && matches!(inserted, Ok(EditOutcome::Inserted))
          && missing_text == "root = true\n\n[[items]]\nname = \"new\"\n"
          && matches!(unterminated_outcome, Some(Ok(EditOutcome::Inserted)))
          && unterminated_text == "root = true\n\n[[items]]\nname = \"first\"\n[[items]]\nname = \"second\""
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Keep detached inter-block and tail comments at their structural slots during reconciliation.
  #[test]
  fn table_blocks_keep_detached_comments_at_structural_slots() -> Result<(), impl Debug> {
    let source =
      "# attached one\n[[items]]\nname = \"a\"\n\n# detached between\n\n# attached two\n[[items]]\nname = \"b\"\n\n# detached tail\n";
    let items = ExactPath::from_segments(["items"]);
    let fixture_reordered = edit_document(source, |document| {
      document.table_blocks(&items).map(|mut blocks| {
        let original = blocks.clone();
        blocks.reverse();
        let outcome = document.reconcile_table_blocks(&items, &blocks);
        (original, blocks, outcome)
      })
    });
    let fixture_removed = edit_document(source, |document| document.reconcile_table_blocks(&items, &[]));
    ensure_that(
      (fixture_reordered, fixture_removed),
      "attached block comments must move with their blocks while detached comments retain their slots",
      |reordered_fields| {
        let (ref reordered, ref removed) = *reordered_fields;
        let &Ok((_, Ok((ref original, _, ref replaced)), Ok(ref reordered_text))) = reordered else {
          return false;
        };
        let [ref first, ref second] = *original.as_slice() else {
          return false;
        };
        let &Ok((_, ref removed_outcome, Ok(ref removed_text))) = removed else {
          return false;
        };
        first.as_str() == "# attached one\n[[items]]\nname = \"a\"\n"
          && second.as_str() == "# attached two\n[[items]]\nname = \"b\"\n"
          && matches!(replaced, Ok(EditOutcome::Replaced))
          && reordered_text
            == "# attached two\n[[items]]\nname = \"b\"\n\n# detached between\n\n# attached one\n[[items]]\nname = \"a\"\n\n# detached \
                tail\n"
          && matches!(removed_outcome, Ok(EditOutcome::Removed))
          && removed_text == "\n# detached between\n\n\n# detached tail\n"
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Treat an array-table parent and all strict descendants as one movable semantic element.
  #[test]
  fn array_table_elements_include_all_descendant_blocks() -> Result<(), impl Debug> {
    let source = "[[contracts.toml]]\nname = \"first\"\n[[contracts.toml.keys]]\nname = \"a\"\n\n[[contracts.toml]]\nname = \
                  \"second\"\n[[contracts.toml.keys]]\nname = \"b\"\n\n[other]\nkeep = true\n";
    let contracts = ExactPath::from_segments(["contracts", "toml"]);
    let observed = edit_document(source, |document| {
      document.table_blocks(&contracts).map(|mut blocks| {
        let original = blocks.clone();
        let reparsed = original.first().map(|first| TableBlockFragment::parse(first.as_str()));
        blocks.reverse();
        let outcome = document.reconcile_table_blocks(&contracts, &blocks);
        (original, reparsed, blocks, outcome)
      })
    });
    ensure_that(observed, "parent fragments must carry every descendant block through extraction, parsing, and reordering", |result| {
      let &Ok((_, Ok((ref original, Some(Ok(ref reparsed)), _, ref outcome)), Ok(ref text))) = result else { return false; };
      let [ref first, ref second] = *original.as_slice() else { return false; };
      first.as_str().contains("name = \"first\"") && first.as_str().contains("[[contracts.toml.keys]]")
        && first.as_str().contains("name = \"a\"") && !first.as_str().contains("name = \"second\"")
        && second.as_str().contains("name = \"second\"") && second.as_str().contains("name = \"b\"") && !second.as_str().contains("[other]")
        && reparsed.path() == &contracts && matches!(outcome, Ok(EditOutcome::Replaced)) && text.contains("[other]\nkeep = true")
        && matches!((text.find("name = \"second\""), text.find("name = \"b\""), text.find("name = \"first\"")), (Some(first_position), Some(second_position), Some(third_position)) if first_position < second_position && second_position < third_position)
    }).map(drop).map_err(Box::new)
  }

  /// Rebase only the root and descendant headers of a validated table block.
  #[test]
  fn table_blocks_rebase_root_and_descendant_headers_only() -> Result<(), impl Debug> {
    let source = "# contract\n[workspace.metadata.config.contract]\nkind = \"toml\"\n\n# \
                  key\n[[workspace.metadata.config.contract.keys]]\nname = \"version\"\n";
    let contracts = ExactPath::from_segments(["contracts"]);
    let observed = TableBlockFragment::parse(source).map(|block| {
      let unchanged = block.rebase(block.path());
      let rebased = block.rebase(&contracts);
      let rejected = block.rebase(&ExactPath::default());
      (block, unchanged, rebased, rejected)
    });
    ensure_that(
      observed,
      "rebasing must change only hierarchy headers and reject the document root",
      |result| {
        let &Ok((ref block, ref unchanged, ref rebased, ref rejected)) = result else {
          return false;
        };

        unchanged.as_ref().is_ok_and(|actual| actual == block)
          && rebased.as_ref().is_ok_and(|actual| {
            actual.path() == &contracts
              && actual.as_str() == "# contract\n[contracts]\nkind = \"toml\"\n\n# key\n[[contracts.keys]]\nname = \"version\"\n"
          })
          && matches!(rejected, Err(RewriteError::InvalidFragment { .. }))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Permit exact traversal through one array-table element and reject multiple matches.
  #[test]
  fn exact_operations_traverse_only_one_array_table_element() -> Result<(), impl Debug> {
    let parent = ExactPath::from_segments(["managed-children", "repositories"]);
    let branch = parent.child("branch");
    let name = parent.child("name");
    let source = "[[managed-children.repositories]]\nname = \"legacy\"\nbranch = \"old\"\n";
    let fixture_observed = edit_document(source, |document| {
      let initial = document.value(&branch).map(|view| (view.text().to_owned(), view.kind()));
      let replacement = ValueFragment::parse("\"strict\"");
      let replaced = replacement
        .as_ref()
        .ok()
        .map(|fragment| document.replace_value(&branch, fragment));
      let removed = document.remove_entry(&name, RemoveEmptyParents::Keep);
      let rendered = document.render();
      let committed = document.commit();
      let inserted = EntryFragment::parse("enabled = true").map(|fragment| {
        let outcome = document.insert_entry(&parent, &fragment);
        (fragment, outcome)
      });
      (initial, replacement, replaced, removed, rendered, committed, inserted)
    });
    let fixture_multiple = edit_document(
      "[[managed-children.repositories]]\nbranch = \"one\"\n\n[[managed-children.repositories]]\nbranch = \"two\"\n",
      |document| {
        let queried = document.value(&branch).map(|view| (view.text().to_owned(), view.kind()));
        let edited = ValueFragment::parse("\"strict\"").map(|fragment| {
          let outcome = document.replace_value(&branch, &fragment);
          (fragment, outcome)
        });
        (queried, edited)
      },
    );
    ensure_that(
      (fixture_observed, fixture_multiple),
      "exact array-table operations must retain unique traversal and reject ambiguity atomically",
      |observed_fields| {
        let (ref observed, ref multiple) = *observed_fields;
        let &Ok((
          _,
          (Ok((ref initial, _)), _, ref replaced, ref removed, Ok(ref rendered), ref committed, Ok((_, ref inserted))),
          Ok(ref final_render),
        )) = observed
        else {
          return false;
        };
        let &Ok((ref document, (ref queried, Ok((_, ref multiple_edit))), _)) = multiple else {
          return false;
        };
        initial == "\"old\""
          && matches!(replaced, Some(Ok(EditOutcome::Replaced)))
          && matches!(removed, Ok(EditOutcome::Removed))
          && rendered == "[[managed-children.repositories]]\nbranch = \"strict\"\n"
          && committed.is_ok()
          && matches!(inserted, Ok(EditOutcome::Inserted))
          && final_render.contains("branch = \"strict\"\nenabled = true\n")
          && matches!(
            queried,
            Err(RewriteError::AmbiguousMatches {
              count: 2,
              ..
            })
          )
          && document.patches().is_empty()
          && matches!(
            multiple_edit,
            Err(RewriteError::AmbiguousMatches {
              count: 2,
              ..
            })
          )
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Reject fragments containing extra values, entries, headers, or detached trivia.
  #[test]
  fn fragment_validation_rejects_wrong_shapes() -> Result<(), impl Debug> {
    let fixture_values = [ValueFragment::parse("1\nother = 2")];
    let fixture_entries = ["a = 1\nb = 2\n", "a = 1\n[table]\n", "# detached\n\na = 1\n"].map(EntryFragment::parse);
    let fixture_elements = ["1, 2", "", "# detached\n\n1"].map(ArrayElementFragment::parse);
    let fixture_blocks = [
      "[a]\nx = 1\n[b]\ny = 2\n",
      "root = 1\n",
      "root = 1\n[a]\nx = 1\n",
      "# detached\n\n[a]\nx = 1\n",
      "[[items]]\nname = \"one\"\n\n# detached tail\n",
    ]
    .map(TableBlockFragment::parse);
    ensure_that(
      (fixture_values, fixture_entries, fixture_elements, fixture_blocks),
      "fragment boundaries must reject extra structures and detached trivia",
      |values_fields| {
        let (ref values, ref entries, ref elements, ref blocks) = *values_fields;
        values
          .iter()
          .all(|result| matches!(result, Err(RewriteError::InvalidFragment { .. })))
          && entries
            .iter()
            .all(|result| matches!(result, Err(RewriteError::InvalidFragment { .. })))
          && elements
            .iter()
            .all(|result| matches!(result, Err(RewriteError::InvalidFragment { .. })))
          && blocks
            .iter()
            .all(|result| matches!(result, Err(RewriteError::InvalidFragment { .. })))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Preserve distinct typed errors for missing paths, type mismatches, queries, ambiguity, and
  /// ranges.
  #[test]
  fn query_and_mutation_failures_retain_typed_boundaries() -> Result<(), impl Debug> {
    let fixture_missing = Rewrite::parse("scalar = 1\n");
    let fixture_wrong_parent = edit_document("scalar = 1\n", |document| {
      EntryFragment::parse("child = true").map(|fragment| {
        let outcome = document.insert_entry(&ExactPath::from_segments(["scalar"]), &fragment);
        (fragment, outcome)
      })
    });
    let fixture_query = edit_document("value = 1\n", |document| {
      let renamed = document
        .rename_keys("[", "replacement")
        .map(|updated| updated.patches().to_vec());
      let invalid_glob = Keys::from(Key::new("["));
      let glob = document
        .root
        .find_all_matches(&invalid_glob, false)
        .map(Iterator::collect::<Vec<_>>);
      (renamed, invalid_glob, glob)
    });
    let fixture_ambiguous = edit_document(
      "[[items]]\nname = \"one\"\n\n[other]\nkeep = true\n\n[[items]]\nname = \"two\"\n",
      |document| {
        let items = ExactPath::from_segments(["items"]);
        document.table_blocks(&items).map(|blocks| {
          let outcome = document.reconcile_table_blocks(&items, &blocks);
          (blocks, outcome)
        })
      },
    );
    let fixture_invalid_range = raw_patch_document("value = 1\n", 0..100, "replacement");
    ensure_that(
      (
        fixture_missing, fixture_wrong_parent, fixture_query, fixture_ambiguous, fixture_invalid_range,
      ),
      "query and mutation failures must retain their distinct typed boundaries",
      |missing_fields| {
        let (ref missing, ref wrong_parent, ref query, ref ambiguous, ref invalid_range) = *missing_fields;
        missing.as_ref().is_ok_and(|document| {
          matches!(
            document.value(&ExactPath::from_segments(["missing"])),
            Err(RewriteError::MissingPath { .. })
          )
        }) && wrong_parent.as_ref().is_ok_and(|document_fields| {
          let (ref document, ref edited, _) = *document_fields;
          document.patches().is_empty()
            && edited
              .as_ref()
              .is_ok_and(|edit| matches!(edit.1, Err(RewriteError::TypeMismatch { .. })))
        }) && query.as_ref().is_ok_and(|&(_, (ref renamed, _, ref glob), _)| {
          matches!(renamed, Err(RewriteError::Query(QueryError::InvalidKey(_)))) && matches!(glob, Err(QueryError::InvalidGlob(_)))
        }) && ambiguous.as_ref().is_ok_and(|edited_fields| {
          let (_, ref edited, _) = *edited_fields;
          edited
            .as_ref()
            .is_ok_and(|edit| matches!(edit.1, Err(RewriteError::AmbiguousMatches { .. })))
        }) && invalid_range.as_ref().is_ok_and(|admitted_fields| {
          let (_, ref admitted, ref rendered) = *admitted_fields;
          admitted.is_ok() && matches!(rendered, Err(RewriteError::InvalidSourceRange { .. }))
        })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Reject overlapping key patches without appending any part of the failed request.
  #[test]
  fn overlapping_and_touching_patches_are_rejected_without_partial_addition() -> Result<(), impl Debug> {
    let observed = edit_document("[table]\nvalue = 1\n", |document| {
      let first = document.rename_keys("table", "first").map(|updated| updated.patches().to_vec());
      let second = document
        .rename_keys("table", "second")
        .map(|updated| updated.patches().to_vec());
      (first, second)
    });
    ensure_that(
      observed,
      "overlapping replacement must retain the accepted patch without partial addition",
      |result| {
        let &Ok((ref document, (ref first, ref second), _)) = result else {
          return false;
        };

        first.is_ok() && matches!(second, Err(RewriteError::Overlap)) && document.patches().len() == 1
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Reject adjacent and non-UTF-8 ranges without mutating committed render state.
  #[test]
  fn touching_and_non_utf8_ranges_fail_without_mutating_render_state() -> Result<(), impl Debug> {
    let fixture_touching = edit_document("ab = 1\n", |document| {
      let first = document.push_std_patch(0..1, "x".into());
      let second = document.push_std_patch(1..2, "y".into());
      (first, second)
    });
    let fixture_utf8 = raw_patch_document("\"\u{e9}\" = 1\n", 2..2, "");
    ensure_that(
      (fixture_touching, fixture_utf8),
      "touching and non-UTF-8 patches must retain committed source and accepted transaction state",
      |touching_fields| {
        let (ref touching, ref utf8) = *touching_fields;
        touching.as_ref().is_ok_and(|&(ref document, (ref first, ref second), _)| {
          first.is_ok() && matches!(second, Err(RewriteError::Overlap)) && document.patches().len() == 1
        }) && utf8.as_ref().is_ok_and(|document_fields| {
          let (ref document, ref admitted, ref rendered) = *document_fields;
          admitted.is_ok()
            && matches!(rendered, Err(RewriteError::InvalidUtf8Range { .. }))
            && document.source() == "\"\u{e9}\" = 1\n"
            && document.patches().len() == 1
        })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Require the rewrite constructor to own a document-root syntax node.
  #[test]
  fn non_root_nodes_are_rejected() -> Result<(), impl Debug> {
    let observed = parse("value = 1\n[table]\nchild = 2\n").map(|parsed| {
      let root = parsed.clone().into_dom();
      let value = root.get_key("value").map(Rewrite::new);
      let table = root.get_key("table").map(Rewrite::new);
      (parsed, root, value, table)
    });
    ensure_that(
      observed,
      "scalar and source-backed non-root nodes must reject rewrite ownership",
      |result| {
        let &Ok((ref parsed, _, ref value, ref table)) = result else {
          return false;
        };

        parsed.diagnostics().is_empty()
          && matches!(value, Some(Err(RewriteError::RootNodeExpected)))
          && matches!(table, Some(Err(RewriteError::RootNodeExpected)))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }
}
