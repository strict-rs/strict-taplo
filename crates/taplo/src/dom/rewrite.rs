//! Source-preserving TOML queries and rewrites.
//!
//! [`Rewrite`] owns one parsed document and records non-overlapping source
//! patches against it.  Call [`Rewrite::commit`] between structural phases
//! when a later edit depends on a shape created by an earlier edit.

use core::fmt;
use std::cmp::Reverse;
use std::ops::Range;
use std::sync::Arc;

use rowan::TextRange;
use rowan::TextSize;
use thiserror::Error;

use super::FromSyntax;
use super::Keys;
use super::error::Error as DomError;
use super::from_syntax::keys_from_syntax;
use super::node::ArrayKind;
use super::node::DomNode;
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
  pub fn parse(path: &str) -> Result<Self, RewriteError> {
    if path.is_empty() {
      return Ok(Self::default());
    }

    let synthetic = format!("{path} = true\n");
    let parsed = parser::parse(&synthetic);
    if !parsed.errors.is_empty() {
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
      .map(|key| Arc::<str>::from(key.value()))
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
    Self::from_segments(self.segments.iter().cloned().chain(core::iter::once(segment.into())))
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
  /// An invalid DOM node retained by [`Rewrite::new`] compatibility.
  Invalid,
}

impl fmt::Display for TomlKind {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{self:?}")
  }
}

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

impl fmt::Display for EditOutcome {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{self:?}")
  }
}

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
  pub fn text(self) -> &'source str {
    self.text
  }

  /// Return the parsed structural kind.
  #[must_use]
  pub fn kind(self) -> TomlKind {
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
  pub fn text(self) -> &'source str {
    self.text
  }

  /// Return the parsed value kind.
  #[must_use]
  pub fn kind(self) -> TomlKind {
    self.value.kind
  }

  /// Return the entry's value view.
  #[must_use]
  pub fn value(self) -> ValueView<'source> {
    self.value
  }
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
  /// Parse and validate exactly one TOML value.
  ///
  /// # Errors
  ///
  /// Returns a typed syntax, semantic, or fragment-shape error when `source`
  /// does not contain exactly one value.
  pub fn parse(source: &str) -> Result<Self, RewriteError> {
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

  /// Return the exact value source.
  #[must_use]
  pub fn as_str(&self) -> &str {
    &self.source
  }

  /// Return the parsed structural kind.
  #[must_use]
  pub fn kind(&self) -> TomlKind {
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
  /// Parse and validate exactly one root key/value entry.
  ///
  /// # Errors
  ///
  /// Returns a typed syntax, semantic, or fragment-shape error when `source`
  /// is not exactly one entry plus its attached trivia.
  pub fn parse(source: &str) -> Result<Self, RewriteError> {
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

  /// Return the entry key relative to its parent.
  #[must_use]
  pub fn key(&self) -> &ExactPath {
    &self.key
  }

  /// Return the exact entry source including attached trivia.
  #[must_use]
  pub fn as_str(&self) -> &str {
    &self.source
  }

  /// Return the validated value fragment.
  #[must_use]
  pub fn value(&self) -> &ValueFragment {
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
  /// Parse and validate exactly one array element plus attached trivia.
  ///
  /// # Errors
  ///
  /// Returns a typed syntax, semantic, or fragment-shape error when `source`
  /// contains zero or multiple elements.
  pub fn parse(source: &str) -> Result<Self, RewriteError> {
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

  /// Return the fragment's source spelling.
  #[must_use]
  pub fn as_str(&self) -> &str {
    &self.source
  }

  /// Return the element value.
  #[must_use]
  pub fn value(&self) -> &ValueFragment {
    &self.value
  }

  /// Return the element value kind.
  #[must_use]
  pub fn kind(&self) -> TomlKind {
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

impl TableBlockFragment {
  /// Parse and validate exactly one complete table element.
  ///
  /// # Errors
  ///
  /// Returns a typed syntax, semantic, or fragment-shape error when `source`
  /// contains no root header, a second sibling/non-descendant header, or root
  /// entries outside the block.
  pub fn parse(source: &str) -> Result<Self, RewriteError> {
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

  /// Return the exact complete block source.
  #[must_use]
  pub fn as_str(&self) -> &str {
    &self.source
  }

  /// Return the block header path.
  #[must_use]
  pub fn path(&self) -> &ExactPath {
    &self.path
  }

  /// Return [`TomlKind::Table`] or [`TomlKind::ArrayOfTables`].
  #[must_use]
  pub fn kind(&self) -> TomlKind {
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
  /// This compatibility constructor retains the original behavior and does not
  /// recover parser diagnostics that were discarded before the DOM was built.
  /// New consumers should call [`Self::parse`].
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
    let parsed = parser::parse(source);
    if !parsed.errors.is_empty() {
      return Err(RewriteError::SyntaxDiagnostics {
        diagnostics: parsed.errors,
      });
    }
    let root = parsed.into_dom();
    if let Err(errors) = root.validate() {
      let diagnostics = errors.map(SemanticDiagnostic::from_dom).collect::<Vec<_>>();
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

  /// Add a compatibility patch such as [`Patch::RenameKeys`].
  ///
  /// # Errors
  ///
  /// Returns a DOM query error or overlap error when a requested key range
  /// cannot be patched safely.
  pub fn add(&mut self, patch: impl Into<Patch>) -> Result<&mut Self, RewriteError> {
    let patch = patch.into();
    match patch {
      Patch::RenameKeys {
        key,
        to,
      } => {
        let keys = key.parse::<Keys>()?;
        let ranges = self
          .root
          .find_all_matches(keys, false)?
          .filter_map(|(keys, _)| match keys.iter().last().cloned() {
            Some(dom::KeyOrIndex::Key(key)) => Some(key),
            _ => None,
          })
          .flat_map(|key| key.text_ranges().collect::<Vec<_>>())
          .collect::<Vec<_>>();

        self.validate_new_ranges(&ranges)?;

        for range in ranges {
          self.push_patch(range, PendingPatchKind::Replace(to.clone()))?;
        }
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

  /// Rename every key matched by the compatibility glob query.
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
    let Some(syntax) = node.syntax() else {
      return Err(RewriteError::UnsupportedPlacement {
        path:   path.clone(),
        reason: "the DOM value has no source provenance".into(),
      });
    };
    let range = text_range(syntax.text_range())?;
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
    let Some(syntax) = node.syntax() else {
      return Err(RewriteError::UnsupportedPlacement {
        path:   path.clone(),
        reason: "the DOM value has no source provenance".into(),
      });
    };
    let range = text_range(syntax.text_range())?;
    if self.slice(range.clone())? == fragment.as_str() {
      return Ok(EditOutcome::Unchanged);
    }
    self.push_std_patch(range, fragment.source.clone())?;
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
    if self.node(path).is_ok() {
      return self.replace_value(path, fragment);
    }
    let Some(parent) = path.parent() else {
      return Err(RewriteError::UnsupportedPlacement {
        path:   path.clone(),
        reason: "the document root cannot be replaced as an entry value".into(),
      });
    };
    let Some(key) = path.key() else {
      return Err(RewriteError::UnsupportedPlacement {
        path:   path.clone(),
        reason: "the document root has no entry key".into(),
      });
    };
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
    let node = self.node(path)?;
    let Node::Array(array) = &node else {
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
    let mut fragments = Vec::new();
    for value_node in syntax.children().filter(|child| child.kind() == SyntaxKind::VALUE) {
      fragments.push(self.array_element_fragment(syntax, &value_node)?);
    }
    Ok(fragments)
  }

  /// Reconcile an inline array to an ordered list of validated elements.
  ///
  /// # Errors
  ///
  /// Returns a missing-path, type, placement, range, or overlap error.
  pub fn reconcile_array(&mut self, path: &ExactPath, elements: &[ArrayElementFragment]) -> Result<EditOutcome, RewriteError> {
    let node = self.node(path)?;
    let Node::Array(array) = &node else {
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
    let interior = delimited_interior(syntax, SyntaxKind::BRACKET_START, SyntaxKind::BRACKET_END)?;
    let original = self.slice(interior.clone())?;
    let detached_comments = self.detached_array_comments(syntax)?;
    let replacement = render_array_elements(original, elements, &detached_comments);
    if original == replacement {
      return Ok(EditOutcome::Unchanged);
    }
    self.push_std_patch(interior, replacement.into())?;
    Ok(EditOutcome::Replaced)
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
      .map(|(position, (first, second))| {
        let slot = position.checked_add(1).ok_or(RewriteError::InvalidSourceRange {
          start: position,
          end:   usize::MAX,
        })?;
        Ok(TableBlockGap {
          slot,
          source: self.slice(first.block.end..second.block.start)?.into(),
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
    let mut cursor = 0usize;
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
      match &patch.kind {
        PendingPatchKind::Replace(replacement) => rendered.push_str(replacement),
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

  /// Resolve one literal key, crossing one unique AoT element when needed.
  fn exact_child(node: Node, segment: &str, path: &ExactPath) -> Result<Node, RewriteError> {
    match node {
      Node::Table(table) => table.get(segment).ok_or_else(|| RewriteError::MissingPath {
        path: path.clone()
      }),
      Node::Array(array) if array.kind() == ArrayKind::Tables => {
        let table = Self::unique_array_table_element(&array, path)?;
        Self::exact_child(table, segment, path)
      }
      _ => Err(RewriteError::MissingPath {
        path: path.clone()
      }),
    }
  }

  /// Resolve a table insertion parent, unwrapping one terminal AoT element.
  fn table_node(&self, path: &ExactPath) -> Result<Node, RewriteError> {
    let node = self.node(path)?;
    match node {
      Node::Array(array) if array.kind() == ArrayKind::Tables => Self::unique_array_table_element(&array, path),
      other => Ok(other),
    }
  }

  /// Resolve the sole table element in an array of tables.
  fn unique_array_table_element(array: &super::node::Array, path: &ExactPath) -> Result<Node, RewriteError> {
    let (count, element) = {
      let elements = array.items().read();
      (elements.len(), elements.first().cloned())
    };
    match (count, element) {
      (0, _) => Err(RewriteError::MissingPath {
        path: path.clone()
      }),
      (1, Some(element)) => Ok(element),
      (1, None) => Err(RewriteError::MissingPath {
        path: path.clone()
      }),
      _ => Err(RewriteError::AmbiguousMatches {
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
        let Some(key) = header.children().find(|child| child.kind() == SyntaxKind::KEY) else {
          return Err(RewriteError::InvalidFragment {
            kind:   FragmentKind::TableBlock,
            reason: "the table header has no key syntax".into(),
          });
        };
        let key = text_range(key.text_range())?;
        let start = self.attached_start(core.start)?;
        let kind = if header.kind() == SyntaxKind::TABLE_ARRAY_HEADER {
          TomlKind::ArrayOfTables
        } else {
          TomlKind::Table
        };
        Ok(HeaderRecord {
          path,
          kind,
          core: core.clone(),
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
      completed.push(HeaderRecord {
        path:  header.path.clone(),
        kind:  header.kind,
        core:  header.core.clone(),
        key:   header.key.clone(),
        block: header.block.start..end,
      });
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
    for (position, header) in headers.iter().enumerate().filter(|(_, header)| header.path == *path) {
      let following = headers.iter().skip(position.saturating_add(1));
      let end = following
        .take_while(|candidate| path.is_strict_parent_of(&candidate.path))
        .last()
        .map_or(header.block.end, |descendant| descendant.block.end);
      elements.push(HeaderRecord {
        path:  header.path.clone(),
        kind:  header.kind,
        core:  header.core.clone(),
        key:   header.key.clone(),
        block: header.block.start..end,
      });
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
    let parsed = parser::parse(&remaining);
    if !parsed.errors.is_empty() {
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
    let core = text_range(element.text_range())?;
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
    let value_source = self.slice(core.clone())?.trim();
    let node = Node::from_syntax(element.clone().into());
    let value_fragment = ValueFragment {
      source: value_source.into(),
      kind:   node_kind(&node),
    };
    let leading = self.slice(start..core.start)?;
    let trailing = self.slice(core.end..line_end)?;
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
    if let Some(comment) = &trailing_comment {
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
        let core = text_range(element.text_range())?;
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
        Ok(start..end)
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
    match prefix.rfind('\n') {
      Some(newline) => newline.checked_add(1).ok_or(RewriteError::InvalidSourceRange {
        start: newline,
        end:   usize::MAX,
      }),
      None => Ok(0),
    }
  }

  /// Return the byte offset immediately after the containing line ending.
  fn line_end(&self, offset: usize) -> Result<usize, RewriteError> {
    let suffix = self.source.get(offset..).ok_or(RewriteError::InvalidUtf8Range {
      start: offset,
      end:   self.source.len(),
    })?;
    match suffix.find('\n') {
      Some(relative) => offset
        .checked_add(relative)
        .and_then(|newline| newline.checked_add(1))
        .ok_or(RewriteError::InvalidSourceRange {
          start: offset,
          end:   usize::MAX,
        }),
      None => Ok(self.source.len()),
    }
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
    let start = TextSize::try_from(range.start).map_err(|_| RewriteError::InvalidSourceRange {
      start: range.start,
      end:   range.end,
    })?;
    let end = TextSize::try_from(range.end).map_err(|_| RewriteError::InvalidSourceRange {
      start: range.start,
      end:   range.end,
    })?;
    self.push_patch(TextRange::new(start, end), PendingPatchKind::Replace(replacement))
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
    self.render().map_err(|_| fmt::Error)?.fmt(formatter)
  }
}

/// Compatibility patch requests accepted by [`Rewrite::add`].
#[derive(Debug)]
pub enum Patch {
  /// Rename every key matched by the legacy glob query.
  RenameKeys {
    /// Legacy dotted/glob query.
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
  pub fn start(self) -> u32 {
    self.start
  }

  /// Return the exclusive end byte offset.
  #[must_use]
  pub fn end(self) -> u32 {
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
  /// Two keys conflict semantically.
  ConflictingKeys,
  /// A value used as a table was not a table.
  ExpectedTable,
  /// A value used as an array of tables was not an array of tables.
  ExpectedArrayOfTables,
  /// A compatibility DOM query failed.
  Query,
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
  pub fn kind(&self) -> SemanticDiagnosticKind {
    self.kind
  }

  /// Return the human-readable diagnostic message.
  #[must_use]
  pub fn message(&self) -> &str {
    &self.message
  }

  /// Return the primary source range, when available.
  #[must_use]
  pub fn range(&self) -> Option<DiagnosticRange> {
    self.range
  }

  /// Return the related source range, when available.
  #[must_use]
  pub fn related_range(&self) -> Option<DiagnosticRange> {
    self.related_range
  }

  /// Consume a DOM diagnostic while retaining no DOM-backed handles.
  fn from_dom(diagnostic: DomError) -> Self {
    let message = Arc::<str>::from(diagnostic.to_string());
    let (kind, range, related_range) = match diagnostic {
      DomError::UnexpectedSyntax {
        syntax,
      } => (
        SemanticDiagnosticKind::UnexpectedSyntax,
        Some(DiagnosticRange::from_text_range(syntax.text_range())),
        None,
      ),
      DomError::InvalidEscapeSequence {
        string,
      } => (
        SemanticDiagnosticKind::InvalidEscapeSequence,
        Some(DiagnosticRange::from_text_range(string.text_range())),
        None,
      ),
      DomError::ConflictingKeys {
        key,
        other,
      } => (
        SemanticDiagnosticKind::ConflictingKeys,
        key.text_ranges().next().map(DiagnosticRange::from_text_range),
        other.text_ranges().next().map(DiagnosticRange::from_text_range),
      ),
      DomError::ExpectedTable {
        not_table,
        required_by,
      } => (
        SemanticDiagnosticKind::ExpectedTable,
        not_table.text_ranges().next().map(DiagnosticRange::from_text_range),
        required_by.text_ranges().next().map(DiagnosticRange::from_text_range),
      ),
      DomError::ExpectedArrayOfTables {
        not_array_of_tables,
        required_by,
      } => (
        SemanticDiagnosticKind::ExpectedArrayOfTables,
        not_array_of_tables.text_ranges().next().map(DiagnosticRange::from_text_range),
        required_by.text_ranges().next().map(DiagnosticRange::from_text_range),
      ),
      DomError::Query(_) => (SemanticDiagnosticKind::Query, None, None),
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
  /// The parser reported one or more syntax diagnostics.
  #[error("the TOML source has syntax diagnostics: {diagnostics:?}")]
  SyntaxDiagnostics {
    /// All parser diagnostics in source order.
    diagnostics: Vec<parser::Error>,
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
  /// A compatibility DOM query failed.
  #[error("DOM query failed: {diagnostic}")]
  Dom {
    /// Owned diagnostic projection without DOM-backed syntax or key handles.
    diagnostic: SemanticDiagnostic,
  },
}

impl From<DomError> for RewriteError {
  fn from(diagnostic: DomError) -> Self {
    Self::Dom {
      diagnostic: SemanticDiagnostic::from_dom(diagnostic),
    }
  }
}

/// Backwards-compatible name for [`RewriteError`].
pub type Error = RewriteError;

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
  let start = usize::try_from(u32::from(range.start())).map_err(|_| RewriteError::InvalidSourceRange {
    start: usize::MAX,
    end:   usize::MAX,
  })?;
  let end = usize::try_from(u32::from(range.end())).map_err(|_| RewriteError::InvalidSourceRange {
    start,
    end: usize::MAX,
  })?;
  Ok(start..end)
}

/// Return the structural TOML kind represented by a DOM node.
fn node_kind(node: &Node) -> TomlKind {
  match node {
    Node::Table(table) => match table.kind() {
      TableKind::Inline => TomlKind::InlineTable,
      TableKind::Regular | TableKind::Pseudo => TomlKind::Table,
    },
    Node::Array(array) => match array.kind() {
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
  let Some(key) = entry.children().find(|child| child.kind() == SyntaxKind::KEY) else {
    return Err(RewriteError::InvalidFragment {
      kind:   FragmentKind::Entry,
      reason: "the entry has no key syntax".into(),
    });
  };
  Ok(ExactPath::from_segments(
    keys_from_syntax(&key.into()).map(|segment| Arc::<str>::from(segment.value())),
  ))
}

/// Extract a literal path from one table header.
fn path_from_header(header: &SyntaxNode) -> Result<ExactPath, RewriteError> {
  let Some(key) = header.children().find(|child| child.kind() == SyntaxKind::KEY) else {
    return Err(RewriteError::InvalidFragment {
      kind:   FragmentKind::TableBlock,
      reason: "the table header has no key syntax".into(),
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
      control if control.is_control() => rendered.push_str(&format!("\\u{:04X}", u32::from(control))),
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
    if let Some(comment) = &element.trailing_comment {
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
mod tests {
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;

  use super::ArrayElementFragment;
  use super::EditOutcome;
  use super::EntryFragment;
  use super::ExactPath;
  use super::RemoveEmptyParents;
  use super::Rewrite;
  use super::RewriteError;
  use super::SemanticDiagnosticKind;
  use super::TableBlockFragment;
  use super::TomlKind;
  use super::ValueFragment;
  use crate::parser::parse;

  /// Preserve the legacy constructor in focused compatibility tests.
  fn rewrite(source: &str) -> Result<Rewrite, TestFailure> {
    let parsed = parse(source);
    ensure(parsed.errors.is_empty(), "the rewrite fixture must parse cleanly")?;
    ensure_ok(Rewrite::new(parsed.into_dom()), "a parsed document root must be rewriteable")
  }

  /// Parse an exact path in tests without obscuring its failure context.
  fn path(source: &str) -> Result<ExactPath, TestFailure> {
    ensure_ok(ExactPath::parse(source), "the fixture path must be exact")
  }

  /// Require an owned public error boundary without making the DOM itself
  /// thread-safe.
  fn require_send_sync<T: Send + Sync>() {}

  #[test]
  fn rename_keys() -> Result<(), TestFailure> {
    let toml = "\n[table.middle.inner]\n[table.middle.inner.inner]\n";
    let expected = "\n[table_new.middle_new.inner_new]\n[table_new.middle_new.inner_new.inner2_new]\n";
    let mut patches = rewrite(toml)?;
    ensure_ok(patches.rename_keys("table", "table_new"), "the outer key must be renameable")?;
    ensure_ok(
      patches.rename_keys("table.middle", "middle_new"),
      "the middle key must be renameable",
    )?;
    ensure_ok(
      patches.rename_keys("table.middle.inner", "inner_new"),
      "the inner key must be renameable",
    )?;
    ensure_ok(
      patches.rename_keys("table.middle.inner.inner", "inner2_new"),
      "the deepest key must be renameable",
    )?;
    let rendered = ensure_ok(patches.render(), "legacy patches must render")?;
    ensure_eq(
      &rendered.as_str(),
      &expected,
      "different-length replacements must retain source order",
    )
  }

  #[test]
  fn rename_keys_array_of_tables() -> Result<(), TestFailure> {
    let toml = "\n[[table.middle.inner]]\n[[table.middle.inner]]\n[table.middle.inner.inner]\n";
    let expected =
      "\n[[table_new.middle_new.inner_new]]\n[[table_new.middle_new.inner_new]]\n[table_new.middle_new.inner_new.inner2_new]\n";
    let mut patches = rewrite(toml)?;
    ensure_ok(patches.rename_keys("table", "table_new"), "the array-table root must be renameable")?;
    ensure_ok(
      patches.rename_keys("table.middle", "middle_new"),
      "the array-table middle must be renameable",
    )?;
    ensure_ok(
      patches.rename_keys("table.middle.inner", "inner_new"),
      "the array-table inner must be renameable",
    )?;
    ensure_ok(
      patches.rename_keys("table.middle.inner.*.inner", "inner2_new"),
      "the nested array-table key must be renameable",
    )?;
    let rendered = ensure_ok(patches.render(), "legacy array-table patches must render")?;
    ensure_eq(&rendered.as_str(), &expected, "all matching array-table ranges must be rewritten")
  }

  #[test]
  fn exact_paths_are_literal_and_root_is_empty() -> Result<(), TestFailure> {
    let root = ExactPath::from_segments(core::iter::empty::<&str>());
    ensure(root.is_root(), "an empty segment sequence must identify the root")?;
    let quoted = path("patch.\"https://example.com/repo\"")?;
    ensure(
      quoted.segments().collect::<Vec<_>>() == ["patch", "https://example.com/repo"],
      "quoted punctuation must remain one literal segment",
    )?;
    ensure(
      matches!(ExactPath::parse("items.*"), Err(RewriteError::InvalidPath { .. })),
      "unquoted wildcard syntax must be rejected",
    )?;
    let literal_wildcard = path("items.\"*\"")?;
    ensure_eq(
      &literal_wildcard.key().unwrap_or(""),
      &"*",
      "a quoted wildcard character must remain an exact literal key",
    )?;
    ensure(
      matches!(ExactPath::parse("items[0]"), Err(RewriteError::InvalidPath { .. })),
      "array-index syntax must be rejected",
    )
  }

  #[test]
  fn parse_rejects_syntax_and_semantic_diagnostics() -> Result<(), TestFailure> {
    ensure(
      matches!(Rewrite::parse("value = [1 2]"), Err(RewriteError::SyntaxDiagnostics { .. })),
      "syntax diagnostics must reject a rewrite",
    )?;
    let semantic = Rewrite::parse("value = 1\nvalue = 2\n");
    ensure(
      matches!(semantic, Err(RewriteError::SemanticDiagnostics { .. })),
      "duplicate-key semantic diagnostics must reject a rewrite",
    )?;
    let diagnostics = match semantic {
      Err(RewriteError::SemanticDiagnostics {
        diagnostics,
      }) => diagnostics,
      _ => Vec::new(),
    };
    let diagnostic = ensure_some(diagnostics.first(), "duplicate keys must retain one owned semantic diagnostic")?;
    ensure(
      diagnostic.kind() == SemanticDiagnosticKind::ConflictingKeys,
      "the projected diagnostic must retain its stable category",
    )?;
    ensure(
      diagnostic.range().is_some(),
      "the projected diagnostic must retain its primary source range",
    )?;
    ensure(
      diagnostic.related_range().is_some(),
      "the projected diagnostic must retain its conflicting source range",
    )?;
    ensure(
      !diagnostic.message().is_empty(),
      "the projected diagnostic must retain its rendered message",
    )
  }

  #[test]
  fn rewrite_errors_are_send_sync_without_claiming_dom_is() -> Result<(), TestFailure> {
    require_send_sync::<RewriteError>();
    Ok(())
  }

  #[test]
  fn untouched_and_non_ascii_documents_render_byte_for_byte() -> Result<(), TestFailure> {
    let source = "# café\n\"ключ\" = \"値\"\n";
    let rewrite = ensure_ok(Rewrite::parse(source), "valid non-ASCII TOML must parse")?;
    let rendered = ensure_ok(rewrite.render(), "an untouched document must render")?;
    ensure_eq(&rendered.as_str(), &source, "untouched UTF-8 bytes must be identical")
  }

  #[test]
  fn values_and_entries_are_exact_and_typed() -> Result<(), TestFailure> {
    let source = "# attached\nvalue  =  [1, 2] # trailing\nother = true\n";
    let rewrite = ensure_ok(Rewrite::parse(source), "the query fixture must parse")?;
    let value_path = path("value")?;
    let value = ensure_ok(rewrite.value(&value_path), "the array value must be queryable")?;
    ensure_eq(&value.text(), &"[1, 2]", "value lookup must exclude key and trivia")?;
    ensure_eq(&value.kind(), &TomlKind::Array, "value lookup must expose the TOML kind")?;
    let entry = ensure_ok(rewrite.entry(&value_path), "the entry must be queryable")?;
    ensure_eq(
      &entry.text(),
      &"value  =  [1, 2] # trailing",
      "entry lookup must retain internal spacing and its inline comment",
    )?;
    let fragment = ensure_ok(rewrite.entry_fragment(&value_path), "the entry must be extractable")?;
    ensure(
      fragment.as_str().contains("# attached") && fragment.as_str().contains("# trailing"),
      "the extracted entry must retain attached comments",
    )
  }

  #[test]
  fn inline_entry_fragments_stay_inside_the_inline_table() -> Result<(), TestFailure> {
    let rewrite = ensure_ok(
      Rewrite::parse("dependency = { version = \"2\", features = [\"std\"] }\n"),
      "the inline dependency fixture must parse",
    )?;
    let fragment = ensure_ok(
      rewrite.entry_fragment(&path("dependency.version")?),
      "the nested inline entry must be extractable",
    )?;
    ensure_eq(
      &fragment.as_str(),
      &"version = \"2\"",
      "inline extraction must not capture the outer entry or sibling fields",
    )
  }

  #[test]
  fn replace_value_preserves_neighboring_bytes_and_commits_transactionally() -> Result<(), TestFailure> {
    let source = "# keep\nvalue  =  1 # keep too\nneighbor = 2\n";
    let mut rewrite = ensure_ok(Rewrite::parse(source), "the replacement fixture must parse")?;
    let replacement = ensure_ok(ValueFragment::parse("\"café\""), "the replacement must be a value")?;
    ensure_eq(
      &ensure_ok(
        rewrite.replace_value(&path("value")?, &replacement),
        "the value must be replaceable",
      )?,
      &EditOutcome::Replaced,
      "a different value must report replacement",
    )?;
    let expected = "# keep\nvalue  =  \"café\" # keep too\nneighbor = 2\n";
    let rendered = ensure_ok(rewrite.render(), "the replacement must render")?;
    ensure_eq(&rendered.as_str(), &expected, "only the value bytes may change")?;
    ensure_ok(rewrite.commit(), "the valid rendered document must commit")?;
    ensure(rewrite.patches().is_empty(), "a successful commit must clear pending patches")?;
    let committed = ensure_ok(rewrite.render(), "the committed document must render")?;
    ensure_eq(&committed.as_str(), &expected, "commit must replace the source and DOM together")
  }

  #[test]
  fn failed_commit_retains_source_and_patches() -> Result<(), TestFailure> {
    let mut rewrite = ensure_ok(Rewrite::parse("value = 1\n"), "the transaction fixture must parse")?;
    ensure_ok(rewrite.rename_keys("value", "bad key"), "legacy rename accepts source text")?;
    ensure(
      matches!(rewrite.commit(), Err(RewriteError::SyntaxDiagnostics { .. })),
      "an invalid complete result must fail commit",
    )?;
    ensure_eq(&rewrite.source(), &"value = 1\n", "failed commit must retain the committed source")?;
    ensure_eq(&rewrite.patches().len(), &1, "failed commit must retain pending patches")
  }

  #[test]
  fn insert_and_upsert_cover_root_regular_and_inline_tables() -> Result<(), TestFailure> {
    let mut root = ensure_ok(Rewrite::parse("[table]\nold = 1\n"), "the root insertion fixture must parse")?;
    let entry = ensure_ok(
      EntryFragment::parse("# new\nroot = true\n"),
      "the root entry fragment must validate",
    )?;
    ensure_ok(root.insert_entry(&ExactPath::default(), &entry), "a root entry must be insertable")?;
    let root_rendered = ensure_ok(root.render(), "the root insertion must render")?;
    ensure(
      root_rendered.starts_with("# new\nroot = true\n\n[table]"),
      "root insertion must precede table blocks",
    )?;

    let mut regular = ensure_ok(Rewrite::parse("[table]\nold = 1\n\n[other]\nx = 2\n"), "regular fixture must parse")?;
    let inserted = ensure_ok(EntryFragment::parse("new = 2"), "the regular entry must validate")?;
    ensure_ok(
      regular.insert_entry(&path("table")?, &inserted),
      "a regular-table entry must be insertable",
    )?;
    let regular_rendered = ensure_ok(regular.render(), "the regular insertion must render")?;
    ensure_eq(
      &regular_rendered.as_str(),
      &"[table]\nold = 1\nnew = 2\n\n[other]\nx = 2\n",
      "regular insertion must stay in the selected table block",
    )?;

    let mut inline = ensure_ok(Rewrite::parse("value = { old = 1 }\n"), "inline fixture must parse")?;
    let inline_entry = ensure_ok(EntryFragment::parse("new = 2"), "the inline entry must validate")?;
    ensure_ok(
      inline.insert_entry(&path("value")?, &inline_entry),
      "an inline-table entry must be insertable",
    )?;
    let inline_rendered = ensure_ok(inline.render(), "the inline insertion must render")?;
    ensure_eq(
      &inline_rendered.as_str(),
      &"value = { old = 1, new = 2 }\n",
      "inline insertion must preserve braces and spacing",
    )?;

    let mut multiline = ensure_ok(
      Rewrite::parse("value = {\n  old = 1,\n  }\n"),
      "the multiline inline-table fixture must parse",
    )?;
    ensure_ok(
      multiline.insert_entry(&path("value")?, &inline_entry),
      "a multiline inline-table entry must be insertable",
    )?;
    let multiline_rendered = ensure_ok(multiline.render(), "the multiline inline insertion must render")?;
    ensure_eq(
      &multiline_rendered.as_str(),
      &"value = {\n  old = 1,\n  new = 2\n  }\n",
      "multiline insertion must reuse the existing comma and closing indentation",
    )
  }

  #[test]
  fn explicit_parent_creation_requires_a_separate_committed_phase() -> Result<(), TestFailure> {
    let mut rewrite = ensure_ok(Rewrite::parse("root = true\n"), "the creation fixture must parse")?;
    ensure_eq(
      &ensure_ok(
        rewrite.create_tables(&path("outer.inner")?),
        "missing parent tables must be creatable",
      )?,
      &EditOutcome::Inserted,
      "missing tables must report insertion",
    )?;
    ensure_ok(rewrite.commit(), "the created table phase must commit")?;
    let entry = ensure_ok(EntryFragment::parse("value = 1"), "the dependent entry must validate")?;
    ensure_ok(
      rewrite.insert_entry(&path("outer.inner")?, &entry),
      "a dependent edit must use the reparsed table",
    )?;
    let rendered = ensure_ok(rewrite.render(), "the dependent edit must render")?;
    ensure(
      rendered.contains("[outer.inner]\nvalue = 1\n"),
      "the committed parent must accept a child entry",
    )
  }

  #[test]
  fn exact_removal_preserves_siblings_and_comment_boundaries() -> Result<(), TestFailure> {
    let source = "# detached\n\n# attached\nremove = 1 # inline\nkeep = 2\n";
    let mut rewrite = ensure_ok(Rewrite::parse(source), "the removal fixture must parse")?;
    ensure_ok(
      rewrite.remove_entry(&path("remove")?, RemoveEmptyParents::Keep),
      "the exact root entry must be removable",
    )?;
    let rendered = ensure_ok(rewrite.render(), "the removal must render")?;
    ensure_eq(
      &rendered.as_str(),
      &"# detached\n\nkeep = 2\n",
      "attached comments must be removed while blank-separated comments remain",
    )
  }

  #[test]
  fn removal_prunes_only_proven_empty_comment_free_parent_tables() -> Result<(), TestFailure> {
    let mut nested = ensure_ok(
      Rewrite::parse("[outer]\n[outer.inner]\nremove = 1\n"),
      "the nested prune fixture must parse",
    )?;
    ensure_ok(
      nested.remove_entry(&path("outer.inner.remove")?, RemoveEmptyParents::Prune),
      "the only nested entry must be removable with pruning",
    )?;
    let nested_rendered = ensure_ok(nested.render(), "the nested prune must render")?;
    ensure_eq(
      &nested_rendered.as_str(),
      &"",
      "every concrete ancestor proven empty after the removal must be pruned",
    )?;

    let mut commented = ensure_ok(
      Rewrite::parse("# retained table context\n[table]\nremove = 1\n"),
      "the commented prune fixture must parse",
    )?;
    ensure_ok(
      commented.remove_entry(&path("table.remove")?, RemoveEmptyParents::Prune),
      "the entry beneath a commented table must be removable",
    )?;
    let commented_rendered = ensure_ok(commented.render(), "the commented removal must render")?;
    ensure_eq(
      &commented_rendered.as_str(),
      &"# retained table context\n[table]\n",
      "a retained parent comment must prevent table pruning",
    )
  }

  #[test]
  fn arrays_reconcile_elements_comments_order_and_empty_state() -> Result<(), TestFailure> {
    let source = "values = [\n  # detached\n\n  # first\n  \"a\", # inline\n  \"b\",\n]\n";
    let mut rewrite = ensure_ok(Rewrite::parse(source), "the multiline array must parse")?;
    let values = path("values")?;
    let mut elements = ensure_ok(rewrite.array_elements(&values), "array elements must be extractable")?;
    ensure_eq(&elements.len(), &2, "the array must expose exactly two fragments")?;
    let first = ensure_some(elements.first(), "the first array fragment must exist")?;
    let second = ensure_some(elements.get(1), "the second array fragment must exist")?;
    ensure(
      first.as_str().contains("# first") && first.as_str().contains("# inline") && !first.as_str().contains("\"b\""),
      "the first fragment must retain only its own value and attached comments",
    )?;
    ensure(
      !first.as_str().contains("# detached"),
      "blank-separated comments must not attach to an element fragment",
    )?;
    ensure_eq(
      &second.as_str(),
      &"\"b\"",
      "the second fragment must not capture its preceding sibling",
    )?;
    elements.reverse();
    ensure_ok(rewrite.reconcile_array(&values, &elements), "array elements must be reorderable")?;
    let rendered = ensure_ok(rewrite.render(), "the reordered array must render")?;
    ensure(rendered.contains("\"b\","), "the second element must move first")?;
    ensure(
      rendered.contains("# first") && rendered.contains("# inline"),
      "attached comments must survive reconciliation",
    )?;
    let detached_position = rendered.find("# detached");
    let moved_first_position = rendered.find("\"b\"");
    let attached_position = rendered.find("# first");
    ensure(
      matches!(
        (detached_position, moved_first_position, attached_position),
        (Some(detached), Some(first_value), Some(attached)) if detached < first_value && first_value < attached
      ),
      "blank-separated comments must remain at their structural slot instead of moving with an element",
    )?;

    let mut empty = ensure_ok(Rewrite::parse("values = [1, 2]\n"), "the empty-array fixture must parse")?;
    ensure_ok(empty.reconcile_array(&values, &[]), "an array must reconcile to empty")?;
    let empty_rendered = ensure_ok(empty.render(), "the empty array must render")?;
    ensure_eq(
      &empty_rendered.as_str(),
      &"values = []\n",
      "empty reconciliation must retain the brackets",
    )?;

    let no_trailing_source = "values = [\n  1 # last\n]\n";
    let mut no_trailing = ensure_ok(Rewrite::parse(no_trailing_source), "the comment-without-comma fixture must parse")?;
    let no_trailing_elements = ensure_ok(
      no_trailing.array_elements(&values),
      "the final commented element must be extractable",
    )?;
    ensure_eq(
      &ensure_ok(
        no_trailing.reconcile_array(&values, &no_trailing_elements),
        "the unchanged final comment must reconcile",
      )?,
      &EditOutcome::Unchanged,
      "an inline comment must not manufacture a trailing comma",
    )?;
    let no_trailing_rendered = ensure_ok(no_trailing.render(), "the unchanged commented array must render")?;
    ensure_eq(
      &no_trailing_rendered.as_str(),
      &no_trailing_source,
      "a final inline comment without a comma must remain byte-identical",
    )
  }

  #[test]
  fn one_line_arrays_preserve_terminal_comma_style_and_empty_trivia() -> Result<(), TestFailure> {
    let values = path("values")?;
    let trailing_comma_source = "values = [1, 2,   ] # array\n";
    let mut trailing_comma = ensure_ok(
      Rewrite::parse(trailing_comma_source),
      "the one-line trailing-comma array must parse",
    )?;
    let mut trailing_comma_elements = ensure_ok(
      trailing_comma.array_elements(&values),
      "the trailing-comma elements must be extractable",
    )?;
    trailing_comma_elements.reverse();
    ensure_eq(
      &ensure_ok(
        trailing_comma.reconcile_array(&values, &trailing_comma_elements),
        "the trailing-comma elements must reconcile",
      )?,
      &EditOutcome::Replaced,
      "a reordered trailing-comma array must report replacement",
    )?;
    let trailing_comma_rendered = ensure_ok(trailing_comma.render(), "the trailing-comma array must render")?;
    ensure_eq(
      &trailing_comma_rendered.as_str(),
      &"values = [2, 1,   ] # array\n",
      "a one-line terminal comma, its following spaces, and the entry comment must survive reconciliation",
    )?;

    let mut no_comma = ensure_ok(Rewrite::parse("values = [1, 2   ]\n"), "the one-line no-comma array must parse")?;
    let mut no_comma_elements = ensure_ok(no_comma.array_elements(&values), "the no-comma elements must be extractable")?;
    no_comma_elements.reverse();
    ensure_ok(
      no_comma.reconcile_array(&values, &no_comma_elements),
      "the no-comma elements must reconcile",
    )?;
    let no_comma_rendered = ensure_ok(no_comma.render(), "the no-comma array must render")?;
    ensure_eq(
      &no_comma_rendered.as_str(),
      &"values = [2, 1   ]\n",
      "a one-line array without a terminal comma must not gain one",
    )?;

    let empty_source = "values = [   ]\n";
    let mut empty = ensure_ok(Rewrite::parse(empty_source), "the spaced empty array must parse")?;
    let empty_elements = ensure_ok(empty.array_elements(&values), "the empty array must expose no elements")?;
    ensure_eq(&empty_elements.len(), &0, "the empty array must expose zero fragments")?;
    ensure_eq(
      &ensure_ok(empty.reconcile_array(&values, &empty_elements), "the empty array must reconcile")?,
      &EditOutcome::Unchanged,
      "an already-empty array must remain unchanged",
    )?;
    let empty_rendered = ensure_ok(empty.render(), "the empty array must render")?;
    ensure_eq(
      &empty_rendered.as_str(),
      &empty_source,
      "an empty array's existing interior trivia must remain byte-identical",
    )
  }

  #[test]
  fn table_blocks_copy_reorder_and_remove_as_complete_units() -> Result<(), TestFailure> {
    let source = "# one\n[[items]]\nname = \"a\"\n\n# two\n[[items]]\nname = \"b\"\n\n[other]\nkeep = true\n";
    let mut rewrite = ensure_ok(Rewrite::parse(source), "the table-block fixture must parse")?;
    let items = path("items")?;
    let mut blocks = ensure_ok(rewrite.table_blocks(&items), "array-table blocks must be extractable")?;
    ensure_eq(&blocks.len(), &2, "both array-table blocks must be enumerated")?;
    blocks.reverse();
    ensure_ok(
      rewrite.reconcile_table_blocks(&items, &blocks),
      "array-table blocks must be reorderable",
    )?;
    let rendered = ensure_ok(rewrite.render(), "reordered blocks must render")?;
    let first_b = rendered.find("name = \"b\"");
    let first_a = rendered.find("name = \"a\"");
    ensure(
      matches!((first_b, first_a), (Some(left), Some(right)) if left < right),
      "the supplied block order must control the rendered order",
    )?;
    ensure(
      rendered.contains("[other]\nkeep = true"),
      "consumer-owned unrelated blocks must remain",
    )?;

    let parsed = ensure_ok(
      TableBlockFragment::parse("# block\n[[items]]\nname = \"x\"\n"),
      "a complete block must validate",
    )?;
    ensure_eq(parsed.path(), &items, "block parsing must retain the exact header path")
  }

  #[test]
  fn table_blocks_keep_detached_comments_at_structural_slots() -> Result<(), TestFailure> {
    let source =
      "# attached one\n[[items]]\nname = \"a\"\n\n# detached between\n\n# attached two\n[[items]]\nname = \"b\"\n\n# detached tail\n";
    let items = path("items")?;
    let mut reorder = ensure_ok(Rewrite::parse(source), "the detached-comment block fixture must parse")?;
    let mut blocks = ensure_ok(reorder.table_blocks(&items), "the table blocks must be extractable")?;
    ensure_eq(&blocks.len(), &2, "both table blocks must be enumerated")?;
    let first = ensure_some(blocks.first(), "the first table block must exist")?;
    let second = ensure_some(blocks.get(1), "the second table block must exist")?;
    ensure_eq(
      &first.as_str(),
      &"# attached one\n[[items]]\nname = \"a\"\n",
      "the first fragment must include its attached comment but exclude detached inter-block trivia",
    )?;
    ensure_eq(
      &second.as_str(),
      &"# attached two\n[[items]]\nname = \"b\"\n",
      "the second fragment must include its attached comment but exclude detached tail trivia",
    )?;
    blocks.reverse();
    ensure_ok(
      reorder.reconcile_table_blocks(&items, &blocks),
      "the detached-comment table blocks must reorder",
    )?;
    let reordered = ensure_ok(reorder.render(), "the reordered detached-comment blocks must render")?;
    ensure_eq(
      &reordered.as_str(),
      &"# attached two\n[[items]]\nname = \"b\"\n\n# detached between\n\n# attached one\n[[items]]\nname = \"a\"\n\n# detached tail\n",
      "attached comments must follow their blocks while detached inter-block and tail comments retain their slots",
    )?;

    let mut remove = ensure_ok(Rewrite::parse(source), "the detached-comment removal fixture must parse")?;
    ensure_eq(
      &ensure_ok(remove.reconcile_table_blocks(&items, &[]), "all table blocks must be removable")?,
      &EditOutcome::Removed,
      "removing every matching block must report removal",
    )?;
    let removed = ensure_ok(remove.render(), "the detached comments must render after block removal")?;
    ensure_eq(
      &removed.as_str(),
      &"\n# detached between\n\n\n# detached tail\n",
      "block removal must remove attached comments while preserving detached comment bytes",
    )
  }

  #[test]
  fn array_table_elements_include_all_descendant_blocks() -> Result<(), TestFailure> {
    let source = "[[contracts.toml]]\nname = \"first\"\n[[contracts.toml.keys]]\nname = \"a\"\n\n[[contracts.toml]]\nname = \
                  \"second\"\n[[contracts.toml.keys]]\nname = \"b\"\n\n[other]\nkeep = true\n";
    let mut rewrite = ensure_ok(Rewrite::parse(source), "the nested array-table fixture must parse")?;
    let contracts = path("contracts.toml")?;
    let mut blocks = ensure_ok(rewrite.table_blocks(&contracts), "complete parent elements must be extractable")?;
    ensure_eq(&blocks.len(), &2, "each matching parent header must produce one semantic element")?;
    let first = ensure_some(blocks.first(), "the first semantic parent element must exist")?;
    let second = ensure_some(blocks.get(1), "the second semantic parent element must exist")?;
    ensure(
      first.as_str().contains("name = \"first\"")
        && first.as_str().contains("[[contracts.toml.keys]]")
        && first.as_str().contains("name = \"a\"")
        && !first.as_str().contains("name = \"second\""),
      "the first fragment must include its descendants and stop at its sibling",
    )?;
    ensure(
      second.as_str().contains("name = \"second\"") && second.as_str().contains("name = \"b\"") && !second.as_str().contains("[other]"),
      "the second fragment must include descendants and stop at the first non-descendant",
    )?;

    let parsed = ensure_ok(
      TableBlockFragment::parse(first.as_str()),
      "a parent plus descendants must validate as one block",
    )?;
    ensure_eq(parsed.path(), &contracts, "nested block parsing must retain the root header path")?;

    blocks.reverse();
    ensure_ok(
      rewrite.reconcile_table_blocks(&contracts, &blocks),
      "complete parent elements must reconcile as ordered units",
    )?;
    let rendered = ensure_ok(rewrite.render(), "nested parent reordering must render")?;
    let second_position = rendered.find("name = \"second\"");
    let second_key_position = rendered.find("name = \"b\"");
    let first_position = rendered.find("name = \"first\"");
    ensure(
      matches!(
        (second_position, second_key_position, first_position),
        (Some(parent), Some(descendant), Some(next_parent)) if parent < descendant && descendant < next_parent
      ),
      "reordering must keep each descendant block with its owning parent",
    )?;
    ensure(
      rendered.contains("[other]\nkeep = true"),
      "non-descendant consumer blocks must remain in place",
    )
  }

  #[test]
  fn table_blocks_rebase_root_and_descendant_headers_only() -> Result<(), TestFailure> {
    let source = "# contract\n[workspace.metadata.config.contract]\nkind = \"toml\"\n\n# \
                  key\n[[workspace.metadata.config.contract.keys]]\nname = \"version\"\n";
    let expected = "# contract\n[contracts]\nkind = \"toml\"\n\n# key\n[[contracts.keys]]\nname = \"version\"\n";
    let block = ensure_ok(TableBlockFragment::parse(source), "the migration block must parse")?;
    let contracts = path("contracts")?;
    let rebased = ensure_ok(block.rebase(&contracts), "the complete hierarchy must be rebaseable")?;
    ensure_eq(rebased.path(), &contracts, "rebasing must expose the new root path")?;
    ensure_eq(
      &rebased.as_str(),
      &expected,
      "rebasing must change only root and descendant header paths",
    )?;
    ensure(
      matches!(block.rebase(&ExactPath::default()), Err(RewriteError::InvalidFragment { .. })),
      "a block cannot be rebased to the document root",
    )
  }

  #[test]
  fn exact_operations_traverse_only_one_array_table_element() -> Result<(), TestFailure> {
    let parent = path("managed-children.repositories")?;
    let branch = path("managed-children.repositories.branch")?;
    let name = path("managed-children.repositories.name")?;
    let source = "[[managed-children.repositories]]\nname = \"legacy\"\nbranch = \"old\"\n";
    let mut rewrite = ensure_ok(Rewrite::parse(source), "the standalone array-table block must parse")?;
    let branch_value = ensure_ok(rewrite.value(&branch), "a child beneath one array-table element must resolve")?;
    ensure_eq(
      &branch_value.text(),
      &"\"old\"",
      "unique array-table traversal must reach the exact child",
    )?;

    let replacement = ensure_ok(ValueFragment::parse("\"strict\""), "the replacement branch must validate")?;
    ensure_ok(
      rewrite.replace_value(&branch, &replacement),
      "a child beneath one array-table element must be replaceable",
    )?;
    ensure_ok(
      rewrite.remove_entry(&name, RemoveEmptyParents::Keep),
      "a child beneath one array-table element must be removable",
    )?;
    let rendered = ensure_ok(rewrite.render(), "the unique array-table edits must render")?;
    ensure_eq(
      &rendered.as_str(),
      &"[[managed-children.repositories]]\nbranch = \"strict\"\n",
      "exact edits must retain the array-table header while changing only selected children",
    )?;

    ensure_ok(rewrite.commit(), "the unique array-table edits must commit before insertion")?;
    let enabled = ensure_ok(EntryFragment::parse("enabled = true"), "the inserted child must validate")?;
    ensure_ok(
      rewrite.insert_entry(&parent, &enabled),
      "one array-table element must be a valid exact insertion parent",
    )?;
    let inserted = ensure_ok(rewrite.render(), "the array-table insertion must render")?;
    ensure(
      inserted.contains("branch = \"strict\"\nenabled = true\n"),
      "the inserted child must remain inside the unique array-table block",
    )?;

    let multiple_source = "[[managed-children.repositories]]\nbranch = \"one\"\n\n[[managed-children.repositories]]\nbranch = \"two\"\n";
    let mut multiple = ensure_ok(Rewrite::parse(multiple_source), "the multiple-element array table must parse")?;
    ensure(
      matches!(
        multiple.value(&branch),
        Err(RewriteError::AmbiguousMatches {
          count: 2,
          ..
        })
      ),
      "exact lookup through multiple array-table elements must be ambiguous",
    )?;
    ensure(
      matches!(
        multiple.replace_value(&branch, &replacement),
        Err(RewriteError::AmbiguousMatches {
          count: 2,
          ..
        })
      ),
      "exact mutation through multiple array-table elements must be ambiguous",
    )?;
    ensure(
      multiple.patches().is_empty(),
      "an ambiguous array-table mutation must leave no pending edit",
    )
  }

  #[test]
  fn fragment_validation_rejects_wrong_shapes() -> Result<(), TestFailure> {
    ensure(
      matches!(ValueFragment::parse("1\nother = 2"), Err(RewriteError::InvalidFragment { .. })),
      "a value fragment must reject a second entry",
    )?;
    ensure(
      matches!(EntryFragment::parse("a = 1\nb = 2\n"), Err(RewriteError::InvalidFragment { .. })),
      "an entry fragment must reject multiple entries",
    )?;
    ensure(
      matches!(ArrayElementFragment::parse("1, 2"), Err(RewriteError::InvalidFragment { .. })),
      "an array-element fragment must reject multiple elements",
    )?;
    ensure(
      matches!(
        ArrayElementFragment::parse("# detached\n\n1"),
        Err(RewriteError::InvalidFragment { .. })
      ),
      "an array-element fragment must reject blank-separated comments that are not attached trivia",
    )?;
    ensure(
      matches!(
        TableBlockFragment::parse("[a]\nx = 1\n[b]\ny = 2\n"),
        Err(RewriteError::InvalidFragment { .. })
      ),
      "a table-block fragment must reject multiple headers",
    )?;
    ensure(
      matches!(
        TableBlockFragment::parse("[[items]]\nname = \"one\"\n\n# detached tail\n"),
        Err(RewriteError::InvalidFragment { .. })
      ),
      "a table-block fragment must reject blank-separated tail comments that are not attached trivia",
    )
  }

  #[test]
  fn query_and_mutation_failures_retain_typed_boundaries() -> Result<(), TestFailure> {
    let rewrite = ensure_ok(Rewrite::parse("scalar = 1\n"), "the typed-error fixture must parse")?;
    ensure(
      matches!(rewrite.value(&path("missing")?), Err(RewriteError::MissingPath { .. })),
      "an absent exact value must return a missing-path error",
    )?;

    let entry = ensure_ok(EntryFragment::parse("child = true"), "the child entry must validate")?;
    let mut wrong_parent = ensure_ok(Rewrite::parse("scalar = 1\n"), "the wrong-parent fixture must parse")?;
    ensure(
      matches!(
        wrong_parent.insert_entry(&path("scalar")?, &entry),
        Err(RewriteError::TypeMismatch { .. })
      ),
      "insertion beneath a scalar must return a type mismatch",
    )?;
    ensure(
      wrong_parent.patches().is_empty(),
      "a type mismatch must not leave a pending source mutation",
    )?;

    let mut query = ensure_ok(Rewrite::parse("value = 1\n"), "the query-error fixture must parse")?;
    let query_diagnostic = match query.rename_keys("[", "replacement") {
      Err(RewriteError::Dom {
        diagnostic,
      }) => Some(diagnostic),
      _ => None,
    };
    let diagnostic = ensure_some(
      query_diagnostic.as_ref(),
      "a malformed compatibility query must retain an owned DOM diagnostic",
    )?;
    ensure(
      diagnostic.kind() == SemanticDiagnosticKind::Query,
      "a compatibility query failure must retain its stable category",
    )?;

    let mut ambiguous = ensure_ok(
      Rewrite::parse("[[items]]\nname = \"one\"\n\n[other]\nkeep = true\n\n[[items]]\nname = \"two\"\n"),
      "the non-contiguous array-table fixture must parse",
    )?;
    let items = path("items")?;
    let blocks = ensure_ok(ambiguous.table_blocks(&items), "both non-contiguous blocks must be queryable")?;
    ensure(
      matches!(
        ambiguous.reconcile_table_blocks(&items, &blocks),
        Err(RewriteError::AmbiguousMatches { .. })
      ),
      "non-contiguous exact table matches must return an ambiguity error",
    )?;

    let mut invalid_range = ensure_ok(Rewrite::parse("value = 1\n"), "the range-error fixture must parse")?;
    ensure_ok(
      invalid_range.push_std_patch(0..100, "replacement".into()),
      "an out-of-bounds range within Rowan coordinates may be queued",
    )?;
    ensure(
      matches!(invalid_range.render(), Err(RewriteError::InvalidSourceRange { .. })),
      "an out-of-bounds pending range must return an invalid-source-range error",
    )
  }

  #[test]
  fn overlapping_and_touching_patches_are_rejected_without_partial_addition() -> Result<(), TestFailure> {
    let mut patches = rewrite("[table]\nvalue = 1\n")?;
    ensure_ok(patches.rename_keys("table", "first"), "the first replacement must be accepted")?;
    ensure(
      matches!(patches.rename_keys("table", "second"), Err(RewriteError::Overlap)),
      "a second replacement over the same source range must be rejected",
    )?;
    ensure_eq(&patches.patches().len(), &1, "failed addition must not leave a partial patch")
  }

  #[test]
  fn touching_and_non_utf8_ranges_fail_without_mutating_render_state() -> Result<(), TestFailure> {
    let mut touching = ensure_ok(Rewrite::parse("ab = 1\n"), "the touching-range fixture must parse")?;
    ensure_ok(
      touching.push_std_patch(0..1, "x".into()),
      "the first internal patch must be accepted",
    )?;
    ensure(
      matches!(touching.push_std_patch(1..2, "y".into()), Err(RewriteError::Overlap)),
      "adjacent source ranges must be rejected as touching",
    )?;
    ensure_eq(&touching.patches().len(), &1, "a rejected touching range must not add a patch")?;

    let mut utf8 = ensure_ok(Rewrite::parse("\"é\" = 1\n"), "the UTF-8 range fixture must parse")?;
    ensure_ok(
      utf8.push_std_patch(2..2, "".into()),
      "a Rowan-representable byte offset may be queued",
    )?;
    ensure(
      matches!(utf8.render(), Err(RewriteError::InvalidUtf8Range { .. })),
      "render must reject a byte offset inside a UTF-8 code point",
    )?;
    ensure_eq(&utf8.source(), &"\"é\" = 1\n", "failed render must retain the original source")?;
    ensure_eq(&utf8.patches().len(), &1, "failed render must retain the pending transaction")
  }

  #[test]
  fn non_root_nodes_are_rejected() -> Result<(), TestFailure> {
    let parsed = parse("value = 1");
    ensure(parsed.errors.is_empty(), "the non-root fixture must parse cleanly")?;
    let value = ensure_ok(parsed.into_dom().try_get("value"), "the fixture value must exist")?;
    ensure(
      matches!(Rewrite::new(value), Err(RewriteError::RootNodeExpected)),
      "only a root syntax node may own a rewrite",
    )
  }
}
