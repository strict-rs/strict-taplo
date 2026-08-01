//! Source-preserving TOML formatting.
//!
//! The formatting can be done on documents that might
//! contain invalid syntax. In that case the invalid part is skipped.

use std::cmp;
use std::collections::VecDeque;
use std::iter::FromIterator;
use std::iter::repeat_n;
use std::mem::take;
use std::ops::Range;
use std::rc::Rc;

use itertools::Itertools as _;
use rowan::GreenNode;
use rowan::NodeOrToken;
use rowan::TextRange;
#[cfg(feature = "schema")]
use schemars::JsonSchema;
#[cfg(feature = "serde")]
use serde::Deserialize;
#[cfg(feature = "serde")]
use serde::Serialize;

use crate::dom;
use crate::dom::Keys;
use crate::dom::Node;
use crate::parser::Diagnostic as ParseDiagnostic;
use crate::parser::ParseFailure;
use crate::parser::parse;
use crate::syntax::SyntaxElement;
use crate::syntax::SyntaxNode;
use crate::syntax::SyntaxToken;
use crate::syntax::kind::ARRAY;
use crate::syntax::kind::BRACE_END;
use crate::syntax::kind::BRACE_START;
use crate::syntax::kind::BRACKET_END;
use crate::syntax::kind::BRACKET_START;
use crate::syntax::kind::COMMA;
use crate::syntax::kind::COMMENT;
use crate::syntax::kind::ENTRY;
use crate::syntax::kind::INLINE_TABLE;
use crate::syntax::kind::KEY;
use crate::syntax::kind::NEWLINE;
use crate::syntax::kind::ROOT;
use crate::syntax::kind::TABLE_ARRAY_HEADER;
use crate::syntax::kind::TABLE_HEADER;
use crate::syntax::kind::VALUE;
use crate::syntax::kind::WHITESPACE;
use crate::util::overlaps;

#[macro_use]
/// Formatter option type generation and textual update parsing.
mod macros;

#[derive(Debug, Clone, Default)]
/// Scoped formatter options based on text ranges.
pub struct ScopedOptions(
  /// Ordered source ranges paired with the partial options active inside each range.
  Vec<(TextRange, OptionsIncomplete)>,
);

impl FromIterator<(TextRange, OptionsIncomplete)> for ScopedOptions {
  fn from_iter<T: IntoIterator<Item = (TextRange, OptionsIncomplete)>>(iter: T) -> Self {
    Self(Vec::from_iter(iter))
  }
}

create_options!(
  /// All the formatting options.
  #[derive(Debug, Clone, Eq, PartialEq)]
  #[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
  pub struct Options {
    /// Align entries vertically.
    ///
    /// Entries that have table headers, comments,
    /// or blank lines between them are not aligned.
    pub align_entries: bool,

    /// Align consecutive comments after entries and items vertically.
    ///
    /// This applies to comments that are after entries or array items.
    pub align_comments: bool,

    /// If `align_comments` is true, apply the alignment in cases where
    /// there's only one comment.
    pub align_single_comments: bool,

    /// Put trailing commas for multiline
    /// arrays.
    pub array_trailing_comma: bool,

    /// Automatically expand arrays to multiple lines once they
    /// exceed the configured `column_width`.
    pub array_auto_expand: bool,

    /// Expand values (e.g.) inside inline tables
    /// where possible.
    pub inline_table_expand: bool,

    /// Automatically collapse arrays if they
    /// fit in one line.
    ///
    /// The array won't be collapsed if it
    /// contains a comment.
    pub array_auto_collapse: bool,

    /// Omit whitespace padding inside single-line arrays.
    pub compact_arrays: bool,

    /// Omit whitespace padding inside inline tables.
    pub compact_inline_tables: bool,

    /// Omit whitespace around `=`.
    pub compact_entries: bool,

    /// Target maximum column width after which
    /// arrays are expanded into new lines.
    ///
    /// This is best-effort and might not be accurate.
    pub column_width: usize,

    /// Indent subtables if they come in order.
    pub indent_tables: bool,

    /// Indent entries under tables.
    pub indent_entries: bool,

    /// Indentation to use, should be tabs or spaces
    /// but technically could be anything.
    pub indent_string: String,

    /// Add trailing newline to the source.
    pub trailing_newline: bool,

    /// Alphabetically reorder keys that are not separated by blank lines.
    pub reorder_keys: bool,

    /// Alphabetically reorder array values that are not separated by blank lines.
    pub reorder_arrays: bool,

    /// Alphabetically reorder inline table values.
    pub reorder_inline_tables: bool,

    /// The maximum amount of consecutive blank lines allowed.
    pub allowed_blank_lines: usize,

    /// Use CRLF line endings
    pub crlf: bool,
  }
);

/// Failure to interpret one textual formatter option.
#[derive(Debug, Clone, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum OptionParseError {
  /// The option name is not part of [`Options`].
  #[error("unknown formatting option `{0}`")]
  InvalidOption(String),
  /// The option value cannot be parsed as the option's declared type.
  #[error("invalid value `{input}` for formatting option `{key}`; expected `{expected}`: {reason}")]
  InvalidValue {
    /// Formatter option name.
    key:      String,
    /// Rejected textual value.
    input:    String,
    /// Rust type expected by the option.
    expected: &'static str,
    /// Owned parse-failure explanation.
    reason:   String,
  },
}

/// Typed failure from a source-parsing or path-scoped formatting entry point.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FormatError {
  /// The source syntax tree could not be constructed.
  #[error(transparent)]
  Parse(#[from] ParseFailure),
  /// A path scope could not be interpreted or queried.
  #[error(transparent)]
  Query(#[from] dom::error::QueryError),
  /// A detached DOM value could not be rendered as TOML.
  #[error(transparent)]
  Render(#[from] dom::RenderError),
}

impl Default for Options {
  fn default() -> Self {
    Self {
      align_entries:         false,
      align_comments:        true,
      align_single_comments: true,
      array_trailing_comma:  true,
      array_auto_expand:     true,
      array_auto_collapse:   true,
      compact_arrays:        true,
      compact_inline_tables: false,
      compact_entries:       false,
      column_width:          80,
      indent_tables:         false,
      indent_entries:        false,
      inline_table_expand:   true,
      trailing_newline:      true,
      allowed_blank_lines:   2,
      indent_string:         "  ".into(),
      reorder_keys:          false,
      reorder_arrays:        false,
      reorder_inline_tables: false,
      crlf:                  false,
    }
  }
}

impl Options {
  /// Return the configured logical line ending.
  const fn newline(&self) -> &'static str {
    if self.crlf { "\r\n" } else { "\n" }
  }

  /// Yield the requested number of line endings, capped by the blank-line policy.
  fn newlines(&self, count: usize) -> impl Iterator<Item = &'static str> {
    repeat_n(self.newline(), usize::min(count, self.allowed_blank_lines.saturating_add(1)))
  }

  /// Return whether the observed trailing-comment count should be aligned.
  const fn should_align_comments(&self, comment_count: usize) -> bool {
    (comment_count != 1 || self.align_single_comments) && self.align_comments
  }
}

#[derive(Debug, Clone)]
/// Formatting state inherited by nested syntax nodes.
struct Context {
  /// Current semantic indentation depth.
  indent_level:    usize,
  /// Whether the surrounding layout requires a multiline representation.
  force_multiline: bool,
  /// Syntax-diagnostic ranges that must be copied without reformatting.
  errors:          Rc<[TextRange]>,
  /// Range-scoped partial option updates.
  scopes:          Rc<ScopedOptions>,
}

impl Default for Context {
  fn default() -> Self {
    Self {
      indent_level:    Default::default(),
      force_multiline: Default::default(),
      errors:          Rc::from([]),
      scopes:          Rc::default(),
    }
  }
}

impl Context {
  /// Update options based on the text range.
  fn update_options(&self, options: &mut Options, range: TextRange) {
    for scoped in &self.scopes.0 {
      if scoped.0.contains_range(range) {
        options.update(scoped.1.clone());
      }
    }
  }

  /// Return whether a syntax range touches a parser diagnostic and must remain source-preserved.
  fn error_at(&self, range: TextRange) -> bool {
    for error_range in self.errors.iter().copied() {
      if overlaps(range, error_range) {
        return true;
      }
    }

    false
  }

  /// Yield one configured indentation unit for each active indentation level.
  fn indent<'o>(&self, opts: &'o Options) -> impl Iterator<Item = &'o str> {
    repeat_n(opts.indent_string.as_ref(), self.indent_level)
  }
}

/// Formats a parsed TOML green tree.
#[must_use]
pub fn format_green(green: GreenNode, options: &Options) -> String {
  format_syntax(&SyntaxNode::new_root(green), options)
}

/// Parse then format a TOML document, skipping ranges that contain syntax diagnostics.
///
/// # Errors
///
/// Returns [`FormatError::Parse`] when the lossless syntax tree cannot be
/// constructed.
pub fn format(src: &str, options: &Options) -> Result<String, FormatError> {
  let parsed = parse(src)?;

  let context = Context {
    errors: parsed.diagnostics().iter().map(ParseDiagnostic::range).collect(),
    ..Context::default()
  };

  Ok(format_impl(&parsed.into_syntax(), options, &context))
}

/// Formats a parsed TOML syntax tree.
#[allow(
  clippy::single_call_fn,
  reason = "the public syntax-tree entry point preserves the infallible post-construction formatting contract"
)]
#[must_use]
pub fn format_syntax(node: &SyntaxNode, options: &Options) -> String {
  let mut formatted = format_impl(node, options, &Context::default());

  formatted = formatted.trim_end().into();

  if options.trailing_newline {
    formatted += options.newline();
  }

  formatted
}

/// Formats a DOM root node with given scopes.
///
/// **This doesn't check errors of the DOM.**
///
/// # Errors
///
/// Returns [`FormatError::Render`] when a detached DOM cannot be represented
/// as TOML.
pub fn format_with_scopes(dom: &Node, options: &Options, errors: &[TextRange], scopes: ScopedOptions) -> Result<String, FormatError> {
  let context = Context {
    scopes: Rc::new(scopes),
    errors: errors.into(),
    ..Context::default()
  };

  let mut formatted = format_dom(dom, options, &context)?;

  formatted = formatted.trim_end().into();

  if options.trailing_newline {
    formatted += options.newline();
  }

  Ok(formatted)
}

/// Format a DOM root with options selected by semantic key paths.
///
/// Each path is resolved against the DOM and its options are applied to every
/// matched node range. The caller supplies syntax-error ranges; this function
/// does not independently reject a DOM that already carries semantic errors.
///
/// # Errors
///
/// Returns [`FormatError::Query`] when a scope is invalid or cannot be queried,
/// or [`FormatError::Render`] when a detached DOM cannot be represented as
/// TOML.
#[allow(
  clippy::single_call_fn,
  reason = "the public semantic-path entry point preserves a distinct query-and-render contract for downstream formatters"
)]
pub fn format_with_path_scopes<I, S>(dom: &Node, options: &Options, errors: &[TextRange], scopes: I) -> Result<String, FormatError>
where
  I: IntoIterator<Item = (S, OptionsIncomplete)>,
  S: AsRef<str>,
{
  let mut context = Context {
    errors: errors.into(),
    ..Context::default()
  };

  let mut scoped_ranges = Vec::new();

  for (scope, opts) in scopes {
    let keys: Keys = scope.as_ref().parse()?;
    let matched = dom.find_all_matches(&keys, false)?;

    for (_, node) in matched {
      scoped_ranges.extend(node.text_ranges(true).map(|text_range| (text_range, opts.clone())));
    }
  }

  context.scopes = Rc::new(ScopedOptions::from_iter(scoped_ranges));

  let mut formatted = format_dom(dom, options, &context)?;

  formatted = formatted.trim_end().into();

  if options.trailing_newline {
    formatted += options.newline();
  }

  Ok(formatted)
}

/// Format a syntax root and normalize exactly one configured trailing line ending.
fn format_impl(node: &SyntaxNode, options: &Options, context: &Context) -> String {
  let mut formatted = if node.kind() == ROOT {
    RootFormatter::new(options, context).format(node)
  } else {
    node.to_string()
  };

  if let Some(without_newline) = formatted.strip_suffix("\r\n").or_else(|| formatted.strip_suffix('\n')) {
    formatted = without_newline.to_owned();
  }

  if options.trailing_newline {
    formatted += options.newline();
  }

  formatted
}

/// Format a syntax-backed DOM root, preserving a detached DOM through its TOML representation.
fn format_dom(dom: &Node, options: &Options, context: &Context) -> Result<String, FormatError> {
  dom.syntax().and_then(|syntax| syntax.clone().into_node()).map_or_else(
    || dom.to_toml(false, false).map_err(FormatError::from),
    |syntax| Ok(format_impl(&syntax, options, context)),
  )
}

/// Deferred key/value row used for ordering, width expansion, and vertical alignment.
struct FormattedEntry {
  /// The value node used when an over-width entry must be reformatted as multiline.
  value_syntax: Option<SyntaxNode>,
  /// Rendered key spelling.
  key:          String,
  /// Eagerly normalized key segments used for deterministic ordering.
  cleaned_key:  Vec<String>,
  /// Rendered value text without a trailing comment.
  value:        String,
  /// Optional trailing entry comment.
  comment:      Option<String>,
}

impl FormattedEntry {
  /// Borrow normalized key segments used by ordering implementations.
  fn cleaned_key(&self) -> &[String] {
    &self.cleaned_key
  }

  /// Append the rendered entry to `formatted` using the active separator policy.
  fn append_to(&self, formatted: &mut String, options: &Options) {
    formatted.push_str(&self.key);
    if options.compact_entries {
      formatted.push('=');
    } else {
      formatted.push_str(" = ");
    }
    formatted.push_str(&self.value);
  }
}

impl PartialEq for FormattedEntry {
  fn eq(&self, other: &Self) -> bool {
    self.cleaned_key().eq(other.cleaned_key())
  }
}

impl Eq for FormattedEntry {}

impl PartialOrd for FormattedEntry {
  fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
    Some(self.cmp(other))
  }
}

impl Ord for FormattedEntry {
  fn cmp(&self, other: &Self) -> cmp::Ordering {
    self.cleaned_key().cmp(other.cleaned_key())
  }
}

/// A rendered formatter value plus its optional trailing comment.
struct FormattedValue {
  /// Rendered TOML text.
  text:             String,
  /// Comment that belongs after the rendered text.
  trailing_comment: Option<String>,
}

impl FormattedValue {
  /// Construct a rendered value.
  const fn new(text: String, trailing_comment: Option<String>) -> Self {
    Self {
      text,
      trailing_comment,
    }
  }

  /// Append the rendered text to `formatted`.
  fn append_to(&self, formatted: &mut String) {
    formatted.push_str(&self.text);
  }

  /// Clone the trailing comment for a parent rendering context.
  fn trailing_comment(&self) -> Option<String> {
    self.trailing_comment.clone()
  }
}

/// Flush deferred document comments using the active indentation and newline policy.
fn flush_comments(comments: &mut Vec<String>, formatted: &mut String, context: &Context, options: &Options) {
  for (index, comment) in take(comments).into_iter().enumerate() {
    if index != 0 {
      *formatted += options.newline();
    }
    formatted.extend(context.indent(options));
    *formatted += &comment;
  }
}

/// Apply table and entry indentation policy for one newly encountered header.
#[allow(
  clippy::single_call_fn,
  reason = "the helper keeps table-prefix history and entry indentation coordinated at the header transition boundary"
)]
fn update_table_indentation(context: &mut Context, options: &Options, header: &SyntaxNode, history: &mut Vec<(Keys, usize)>) {
  if options.indent_entries && context.indent_level == 0 {
    context.indent_level = 1;
  }

  let Some(key) = header.first_child().map(|key_node| Keys::from_syntax(&key_node.into())) else {
    return;
  };
  if options.indent_tables {
    context.indent_level = table_indent_level(history, &key, usize::from(options.indent_entries));
  }
  history.push((key, context.indent_level));
}

/// Stateful document-root formatter coordinating deferred groups and scoped
/// policy.
struct RootFormatter<'options> {
  /// Repository-level options used to reset each source scope.
  options:           &'options Options,
  /// Rendered document accumulated in source order.
  formatted:         String,
  /// Deferred entries awaiting ordering and alignment.
  entries:           Vec<FormattedEntry>,
  /// Source newlines already represented by deferred content.
  skipped_newlines:  usize,
  /// Deferred comments awaiting indentation context.
  comments:          Vec<String>,
  /// Mutable indentation and parser-error context.
  context:           Context,
  /// Previously encountered table paths and their indentation.
  table_history:     Vec<(Keys, usize)>,
  /// Newlines detached from their original syntax token.
  dangling_newlines: usize,
  /// Options active at the current source element.
  scoped_options:    Options,
}

impl<'options> RootFormatter<'options> {
  /// Construct the state owner for one document.
  fn new(options: &'options Options, context: &Context) -> Self {
    Self {
      options,
      formatted: String::new(),
      entries: Vec::new(),
      skipped_newlines: 0,
      comments: Vec::new(),
      context: context.clone(),
      table_history: Vec::new(),
      dangling_newlines: 0,
      scoped_options: options.clone(),
    }
  }

  /// Format every direct root child and flush deferred state.
  fn format(mut self, root: &SyntaxNode) -> String {
    for element in root.children_with_tokens() {
      self.element(element);
    }
    flush_comments(&mut self.comments, &mut self.formatted, &self.context, &self.scoped_options);
    flush_entries(&mut self.entries, &mut self.formatted, &self.scoped_options, &self.context);
    self.formatted
  }

  /// Format one direct root child or preserve it when it intersects a parser
  /// diagnostic.
  fn element(&mut self, element: SyntaxElement) {
    if self.context.error_at(element.text_range()) {
      self.formatted.push_str(&element.to_string());
      return;
    }
    let range = element.text_range();
    match element {
      NodeOrToken::Node(node) => self.node(&node, range),
      NodeOrToken::Token(token) => self.token(&token),
    }
  }

  /// Apply the root transition owned by one syntax node.
  fn node(&mut self, node: &SyntaxNode, range: TextRange) {
    match node.kind() {
      TABLE_ARRAY_HEADER | TABLE_HEADER => self.header(node, range),
      ENTRY => self.entry(node, range),
      _ => self.formatted.push_str(&node.to_string()),
    }
  }

  /// Flush a pending entry group and establish one following source-line boundary.
  fn flush_entry_boundary(&mut self) {
    let had_entries = !self.entries.is_empty();
    flush_entries(&mut self.entries, &mut self.formatted, &self.scoped_options, &self.context);
    if had_entries {
      self.formatted.push_str(self.scoped_options.newline());
      self.skipped_newlines = 0;
    }
  }

  /// Flush earlier groups and render one table header.
  fn header(&mut self, header_node: &SyntaxNode, range: TextRange) {
    self.flush_entry_boundary();

    self.scoped_options = self.options.clone();
    self.context.update_options(&mut self.scoped_options, range);
    update_table_indentation(&mut self.context, &self.scoped_options, header_node, &mut self.table_history);

    let mut header_context = self.context.clone();
    if self.scoped_options.indent_entries {
      header_context.indent_level = header_context.indent_level.saturating_sub(1);
    }
    let had_comments = !self.comments.is_empty();
    flush_comments(&mut self.comments, &mut self.formatted, &header_context, &self.scoped_options);
    if had_comments {
      self.formatted.push_str(self.scoped_options.newline());
      self.skipped_newlines = 0;
    }

    let header = format_table_header(header_node, &self.scoped_options, &header_context);
    let trailing_comment = header.trailing_comment();
    if self.scoped_options.indent_tables {
      self.formatted.extend(header_context.indent(&self.scoped_options));
    }
    header.append_to(&mut self.formatted);
    if let Some(comment) = trailing_comment {
      self.formatted.push(' ');
      self.formatted.push_str(&comment);
    }
  }

  /// Queue one key/value entry after applying its scoped options.
  fn entry(&mut self, entry_node: &SyntaxNode, range: TextRange) {
    self.scoped_options = self.options.clone();
    self.context.update_options(&mut self.scoped_options, range);

    let had_comments = !self.comments.is_empty();
    flush_comments(&mut self.comments, &mut self.formatted, &self.context, &self.scoped_options);
    if had_comments {
      self.formatted.push_str(self.scoped_options.newline());
      self.skipped_newlines = 0;
    }

    self.entries.push(format_entry(entry_node, &self.scoped_options, &self.context));
    self.skipped_newlines = self.skipped_newlines.saturating_add(1);
  }

  /// Apply one root-level token transition.
  fn token(&mut self, token: &SyntaxToken) {
    match token.kind() {
      NEWLINE => self.newline(token),
      COMMENT => self.comment(token),
      WHITESPACE => {}
      _ => self.formatted.push_str(token.text()),
    }
  }

  /// Apply one newline token, including dangling-newline recovery.
  fn newline(&mut self, token: &SyntaxToken) {
    let mut newline_count = token.text().newline_count();
    if let Some(dangling_count) = dangling_newlines(token) {
      self.dangling_newlines = self.dangling_newlines.saturating_add(dangling_count);
      return;
    }
    newline_count = newline_count.saturating_add(self.dangling_newlines);
    self.dangling_newlines = 0;

    if newline_count > 1 {
      flush_comments(&mut self.comments, &mut self.formatted, &self.context, &self.scoped_options);
      flush_entries(&mut self.entries, &mut self.formatted, &self.scoped_options, &self.context);
      self.skipped_newlines = 0;
    }
    self.formatted.extend(
      self
        .scoped_options
        .newlines(newline_count.saturating_sub(self.skipped_newlines)),
    );
  }

  /// Defer one comment after flushing any entry group that precedes it.
  fn comment(&mut self, token: &SyntaxToken) {
    self.flush_entry_boundary();
    self.comments.push(token.text().to_owned());
    self.skipped_newlines = self.skipped_newlines.saturating_add(1);
  }
}

/// Determine the indentation level using the indentation history.
///
/// The latest key that is a strict prefix is used and indented. If none is found, the default
/// indentation is used.
#[allow(
  clippy::single_call_fn,
  reason = "the named policy isolates prefix-history lookup from root rendering and keeps fallback indentation explicit"
)]
fn table_indent_level(history: &[(Keys, usize)], current_key: &Keys, default_indent: usize) -> usize {
  history
    .iter()
    .rev()
    .find_map(|history_entry| {
      (current_key.contains(&history_entry.0) && current_key != &history_entry.0).then_some(history_entry.1.saturating_add(1))
    })
    .unwrap_or(default_indent)
}

/// Reformat one entry value when any rendered line exceeds the configured width.
#[allow(
  clippy::single_call_fn,
  reason = "the helper owns the width calculation and multiline re-rendering decision for one deferred entry"
)]
fn expand_overwidth_entry(entry: &mut FormattedEntry, indent_chars_count: usize, options: &Options, context: &Context) {
  let comment_chars_count = entry
    .comment
    .as_ref()
    .map_or(0, |comment| comment.chars().count().saturating_add(1));
  let line_count = entry.value.split('\n').count();

  for (line_index, line) in entry.value.split('\n').enumerate() {
    let mut chars_count = line.chars().count();
    if line_index == 0 {
      chars_count = chars_count.saturating_add(indent_chars_count);
      chars_count = chars_count.saturating_add(entry.key.chars().count());
      chars_count = chars_count.saturating_add(if options.compact_entries { 1 } else { 3 });
    }
    if line_index.saturating_add(1) == line_count {
      chars_count = chars_count.saturating_add(comment_chars_count);
    }
    if chars_count <= options.column_width {
      continue;
    }

    let mut multiline_context = context.clone();
    multiline_context.force_multiline = true;
    let Some(value_syntax) = entry.value_syntax.as_ref() else {
      return;
    };
    let formatted_value = format_value(value_syntax, options, &multiline_context);
    entry.value.clear();
    if let Some(trailing_comment) = formatted_value.trailing_comment() {
      entry.comment = Some(trailing_comment);
    }
    formatted_value.append_to(&mut entry.value);
    return;
  }
}

/// Flush deferred entries into the formatted string.
fn flush_entries(entry_group: &mut Vec<FormattedEntry>, formatted: &mut String, options: &Options, context: &Context) {
  if options.reorder_keys {
    entry_group.sort();
  }

  let indent_chars_count = context.indent_level.saturating_mul(options.indent_string.chars().count());

  // We check for too long lines, and try to expand them if possible.
  // We don't take vertical alignment into account for simplicity.
  if options.array_auto_expand {
    for entry in entry_group.iter_mut() {
      expand_overwidth_entry(entry, indent_chars_count, options, context);
    }
  }

  let mut comment_count: usize = 0;
  // Transform the entries into generic rows that can be aligned.
  let rows = take(entry_group)
    .into_iter()
    .map(|entry| {
      let mut row = Vec::with_capacity(5);

      row.push(context.indent(options).collect::<String>());
      row.push(entry.key);
      row.push("=".to_owned());
      row.push(entry.value);
      if let Some(comment) = entry.comment {
        row.push(comment);
        comment_count = comment_count.saturating_add(1);
      }

      row
    })
    .collect::<Vec<_>>();

  let align_comments = options.should_align_comments(comment_count);
  *formatted += &format_rows(
    if !options.align_entries && !align_comments {
      0..0
    } else if !options.align_entries && align_comments {
      3..usize::MAX
    } else if options.align_entries && !align_comments {
      0..3
    } else {
      0..usize::MAX
    },
    if options.compact_entries {
      3..usize::MAX
    } else {
      1..usize::MAX
    },
    &rows,
    options.newline(),
    " ",
  );
}

/// Format one entry into deferred key, value, and comment columns.
fn format_entry(node: &SyntaxNode, options: &Options, context: &Context) -> FormattedEntry {
  let mut key = String::new();
  let mut rendered_value = String::new();
  let mut comment = None;
  let mut value_syntax = None;

  for syntax_element in node.children_with_tokens() {
    match syntax_element {
      NodeOrToken::Node(child_node) => match child_node.kind() {
        KEY => {
          format_key(&child_node, &mut key, options, context);
        }
        VALUE => {
          value_syntax = Some(child_node.clone());
          let formatted_value = format_value(&child_node, options, context);
          if let Some(trailing_comment) = formatted_value.trailing_comment() {
            comment = Some(trailing_comment);
          }
          formatted_value.append_to(&mut rendered_value);
        }
        _ => rendered_value.push_str(&child_node.to_string()),
      },
      NodeOrToken::Token(token) => {
        if token.kind() == COMMENT {
          comment = Some(token.text().into());
        }
      }
    }
  }

  let cleaned_key = key.replace(['\'', '"'], "").split('.').map(ToOwned::to_owned).collect();

  FormattedEntry {
    value_syntax,
    key,
    cleaned_key,
    value: rendered_value,
    comment,
  }
}

/// Append a key without whitespace around its identifiers and periods.
fn format_key(node: &SyntaxNode, formatted: &mut String, _options: &Options, _context: &Context) {
  // Idents and periods without whitespace
  for syntax_element in node.children_with_tokens() {
    match syntax_element {
      NodeOrToken::Node(_) => {}
      NodeOrToken::Token(token) => match token.kind() {
        WHITESPACE | NEWLINE => {}
        _ => {
          *formatted += token.text();
        }
      },
    }
  }
}

/// Format one value and return any trailing comment to its owning entry or array item.
fn format_value(node: &SyntaxNode, options: &Options, context: &Context) -> FormattedValue {
  let mut rendered_value = String::new();
  let mut comment = None;

  let mut scoped_options = options.clone();
  context.update_options(&mut scoped_options, node.text_range());

  for syntax_element in node.children_with_tokens() {
    match syntax_element {
      NodeOrToken::Node(child_node) => match child_node.kind() {
        ARRAY => {
          let formatted = ArrayFormatter::new(&child_node, &scoped_options, context).format(&child_node);
          if let Some(trailing_comment) = formatted.trailing_comment() {
            comment = Some(trailing_comment);
          }
          formatted.append_to(&mut rendered_value);
        }
        INLINE_TABLE => {
          let formatted = format_inline_table(&child_node, &scoped_options, context);
          if let Some(trailing_comment) = formatted.trailing_comment() {
            comment = Some(trailing_comment);
          }
          formatted.append_to(&mut rendered_value);
        }
        _ => rendered_value.push_str(&child_node.to_string()),
      },
      NodeOrToken::Token(token) => match token.kind() {
        NEWLINE | WHITESPACE => {}
        COMMENT => {
          comment = Some(token.text().into());
        }
        _ => {
          rendered_value = token.text().into();
        }
      },
    }
  }

  FormattedValue::new(rendered_value, comment)
}

/// Format one inline table without transferring its trailing comment into the value text.
#[allow(
  clippy::single_call_fn,
  reason = "the named phase owns inline-table spacing, deterministic child ordering, and trailing-comment separation"
)]
fn format_inline_table(node: &SyntaxNode, options: &Options, context: &Context) -> FormattedValue {
  let mut formatted = String::new();
  let mut comment = None;

  let mut inline_context = context.clone();
  if inline_context.force_multiline {
    inline_context.force_multiline = options.inline_table_expand;
  }

  let child_count = node.children().count();

  if node.children().count() == 0 {
    formatted = "{}".into();
  }

  let mut sorted_children = options.reorder_inline_tables.then(|| {
    node
      .children()
      .sorted_unstable_by(|x, y| x.to_string().cmp(&y.to_string()))
      .collect::<VecDeque<_>>()
  });

  let mut node_index: usize = 0;
  for syntax_element in node.children_with_tokens() {
    match syntax_element {
      NodeOrToken::Node(child_node) => {
        if node_index != 0 {
          formatted += ", ";
        }

        let child = if options.reorder_inline_tables {
          sorted_children.as_mut().and_then(VecDeque::pop_front).unwrap_or(child_node)
        } else {
          child_node
        };

        let entry = format_entry(&child, options, &inline_context);
        entry.append_to(&mut formatted, options);

        node_index = node_index.saturating_add(1);
      }
      NodeOrToken::Token(token) => match token.kind() {
        BRACE_START => {
          if child_count == 0 {
            // We're only interested in trailing comments.
            continue;
          }

          formatted += "{";
          if !options.compact_inline_tables {
            formatted += " ";
          }
        }
        BRACE_END => {
          if child_count == 0 {
            // We're only interested in trailing comments.
            continue;
          }

          if !options.compact_inline_tables {
            formatted += " ";
          }
          formatted += "}";
        }
        WHITESPACE | COMMA => {}
        COMMENT => {
          comment = Some(token.text().into());
        }
        _ => formatted += token.text(),
      },
    }
  }

  FormattedValue::new(formatted, comment)
}
/// Return whether the source-backed array currently spans multiple lines.
#[allow(
  clippy::single_call_fn,
  reason = "the predicate names the source-shape input to array expansion and collapse policy"
)]
fn is_array_multiline(node: &SyntaxNode) -> bool {
  node.descendants_with_tokens().any(|n| n.kind() == NEWLINE)
}

/// Return whether an array has no comments that would be displaced by collapsing it.
#[allow(
  clippy::single_call_fn,
  reason = "the predicate makes comment preservation an explicit guard on automatic array collapse"
)]
fn can_collapse_array(node: &SyntaxNode) -> bool {
  !node.descendants_with_tokens().any(|n| n.kind() == COMMENT)
}

/// Flush deferred array values using the active single-line or multiline layout.
fn flush_array_values(
  value_group: &mut Vec<(String, Option<String>)>,
  comma_group: &mut Vec<bool>,
  formatted: &mut String,
  context: &Context,
  options: &Options,
  multiline: bool,
) {
  if options.reorder_arrays {
    value_group.sort_unstable_by(|left, right| left.0.cmp(&right.0));
  }

  for (has_comma, value_pair) in take(comma_group).into_iter().zip(value_group.iter_mut()) {
    if has_comma {
      value_pair.0 += ",";
    }
  }

  if !multiline {
    for (index, (rendered_value, comment)) in take(value_group).into_iter().enumerate() {
      if index != 0 {
        *formatted += " ";
      }
      *formatted += &rendered_value;
      if let Some(trailing_comment) = comment {
        *formatted += " ";
        *formatted += &trailing_comment;
      }
    }
    return;
  }

  let mut comment_count: usize = 0;
  let rows = take(value_group)
    .into_iter()
    .map(|(rendered_value, comment)| {
      let mut row = Vec::with_capacity(5);
      row.push(context.indent(options).collect::<String>());
      row.push(rendered_value);
      if let Some(trailing_comment) = comment {
        row.push(trailing_comment);
        comment_count = comment_count.saturating_add(1);
      }
      row
    })
    .collect::<Vec<_>>();

  let align_comments = options.should_align_comments(comment_count);
  *formatted += &format_rows(
    if align_comments { 0..usize::MAX } else { 0..0 },
    1..usize::MAX,
    &rows,
    options.newline(),
    " ",
  );
}

/// Attach a same-line comment to the final deferred array value.
#[allow(
  clippy::single_call_fn,
  reason = "the helper isolates trailing-comment ownership from standalone array-comment rendering"
)]
fn attach_trailing_array_comment(value_group: &mut [(String, Option<String>)], comment: &str) {
  if let Some(last_value) = value_group.last_mut() {
    last_value.1 = Some(comment.to_owned());
  }
}

/// Stateful owner of one array's layout, deferred values, and structural output.
#[derive(Debug)]
struct ArrayFormatter<'format> {
  /// Active array formatting options.
  options:                &'format Options,
  /// Context inherited by the array delimiters.
  context:                &'format Context,
  /// Context inherited by array values and standalone comments.
  inner_context:          Context,
  /// Whether the result uses a line per value.
  multiline:              bool,
  /// Rendered output accumulated so far.
  formatted:              String,
  /// Deferred rendered values and their same-line comments.
  values:                 Vec<(String, Option<String>)>,
  /// Deferred comma decisions corresponding one-to-one with `values`.
  commas:                 Vec<bool>,
  /// Source newlines accounted for by deferred values.
  skip_newlines:          usize,
  /// Newlines separated from their successor by whitespace.
  dangling_newline_count: usize,
  /// Number of value nodes already consumed.
  child_index:            usize,
  /// Number of child nodes in the source array.
  child_count:            usize,
}

impl<'format> ArrayFormatter<'format> {
  /// Construct formatting state and decide the array's final layout mode.
  fn new(node: &SyntaxNode, options: &'format Options, context: &'format Context) -> Self {
    let source_multiline = is_array_multiline(node) || context.force_multiline;
    let multiline = if can_collapse_array(node) && options.array_auto_collapse && !context.force_multiline {
      false
    } else {
      source_multiline
    };
    let mut inner_context = context.clone();
    if multiline {
      inner_context.indent_level = inner_context.indent_level.saturating_add(1);
    }

    Self {
      options,
      context,
      inner_context,
      multiline,
      formatted: String::new(),
      values: Vec::new(),
      commas: Vec::new(),
      skip_newlines: 0,
      dangling_newline_count: 0,
      child_index: 0,
      child_count: node.children().count(),
    }
  }

  /// Consume every syntax element and return the complete rendered value.
  fn format(mut self, node: &SyntaxNode) -> FormattedValue {
    for syntax_element in node.children_with_tokens() {
      match syntax_element {
        NodeOrToken::Node(child_node) if child_node.kind() == VALUE => self.value(&child_node),
        NodeOrToken::Node(child_node) => self.structural_node(&child_node),
        NodeOrToken::Token(token) => self.token(&token),
      }
    }
    self.finish()
  }

  /// Defer one formatted array value and its eventual comma.
  fn value(&mut self, child_node: &SyntaxNode) {
    if self.multiline && self.formatted.ends_with('[') {
      self.formatted += self.options.newline();
    }

    let formatted_value = format_value(child_node, self.options, &self.inner_context);
    let mut rendered_value = String::new();
    formatted_value.append_to(&mut rendered_value);
    let has_comma = self.child_index.saturating_add(1) < self.child_count || (self.multiline && self.options.array_trailing_comma);
    self.commas.push(has_comma);
    self.values.push((rendered_value, formatted_value.trailing_comment()));
    self.skip_newlines = self.skip_newlines.saturating_add(1);
    self.child_index = self.child_index.saturating_add(1);
  }

  /// Preserve an unexpected structural child after committing deferred values.
  fn structural_node(&mut self, child_node: &SyntaxNode) {
    self.flush();
    self.formatted.push_str(&child_node.to_string());
  }

  /// Dispatch one array punctuation or trivia token.
  fn token(&mut self, token: &SyntaxToken) {
    match token.kind() {
      BRACKET_START => self.opening_bracket(),
      BRACKET_END => self.closing_bracket(),
      NEWLINE => self.newline(token),
      COMMENT => self.comment(token),
      _ => {}
    }
  }

  /// Render the opening bracket and optional compact-mode padding.
  fn opening_bracket(&mut self) {
    self.formatted.push('[');
    if !self.options.compact_arrays && !self.multiline {
      self.formatted.push(' ');
    }
  }

  /// Commit deferred values and render the closing bracket at its outer indentation.
  fn closing_bracket(&mut self) {
    self.flush();
    if self.multiline && !self.formatted.ends_with('\n') {
      self.formatted += self.options.newline();
    }
    if self.multiline {
      self.formatted.extend(self.context.indent(self.options));
    }
    if !self.multiline && !self.options.compact_arrays {
      self.formatted.push(' ');
    }
    self.formatted.push(']');
  }

  /// Reconcile one source newline token with deferred values and blank-line policy.
  fn newline(&mut self, token: &SyntaxToken) {
    if !self.multiline {
      return;
    }

    let mut newline_count = token.text().newline_count();
    if let Some(dangling_count) = dangling_newlines(token) {
      self.dangling_newline_count = self.dangling_newline_count.saturating_add(dangling_count);
      return;
    }
    newline_count = newline_count.saturating_add(self.dangling_newline_count);
    self.dangling_newline_count = 0;

    if newline_count > 1 {
      self.flush();
      self.skip_newlines = 0;
    }
    self
      .formatted
      .extend(self.options.newlines(newline_count.saturating_sub(self.skip_newlines)));
  }

  /// Attach a same-line comment or render one standalone array comment.
  fn comment(&mut self, token: &SyntaxToken) {
    let newline_before = token
      .siblings_with_tokens(rowan::Direction::Prev)
      .skip(1)
      .find(|sibling| sibling.kind() != WHITESPACE)
      .is_some_and(|sibling| sibling.kind() == NEWLINE);

    if !newline_before && !self.values.is_empty() {
      attach_trailing_array_comment(&mut self.values, token.text());
      return;
    }

    let had_values = !self.values.is_empty();
    self.flush();
    if had_values {
      self.formatted += self.options.newline();
      self.skip_newlines = 0;
    }

    if self.formatted.ends_with('[') {
      self.formatted.push(' ');
    } else {
      self.formatted.extend(self.inner_context.indent(self.options));
    }
    self.formatted += token.text();
  }

  /// Commit every deferred value using the selected array layout.
  fn flush(&mut self) {
    flush_array_values(
      &mut self.values, &mut self.commas, &mut self.formatted, &self.inner_context, self.options, self.multiline,
    );
  }

  /// Normalize a syntax-less empty array and publish the rendered value.
  fn finish(mut self) -> FormattedValue {
    if self.formatted.is_empty() {
      self.formatted = "[]".into();
    }
    FormattedValue::new(self.formatted, None)
  }
}

/// Format one table header while returning its trailing comment separately.
#[allow(
  clippy::single_call_fn,
  reason = "the named phase isolates table-header key rendering from document-level comment alignment"
)]
fn format_table_header(node: &SyntaxNode, options: &Options, context: &Context) -> FormattedValue {
  let mut formatted = String::new();
  let mut comment = None;

  for syntax_element in node.children_with_tokens() {
    match syntax_element {
      NodeOrToken::Node(child_node) => {
        format_key(&child_node, &mut formatted, options, context);
      }
      NodeOrToken::Token(token) => match token.kind() {
        WHITESPACE | NEWLINE => {}
        COMMENT => {
          comment = Some(token.text().to_owned());
        }
        _ => formatted += token.text(),
      },
    }
  }

  FormattedValue::new(formatted, comment)
}

/// Count logical line-feed characters in formatted source.
trait NewlineCount {
  /// Return the number of logical line-feed characters.
  fn newline_count(&self) -> usize;
}

impl NewlineCount for &str {
  fn newline_count(&self) -> usize {
    self.chars().filter(|character| character == &'\n').count()
  }
}

/// Align selected row columns and join the rows with the configured separators.
fn format_rows<R, S>(align_range: Range<usize>, separator_range: Range<usize>, rows: &[R], newline: &str, separator: &str) -> String
where
  R: AsRef<[S]>,
  S: AsRef<str>,
{
  let mut out = String::new();

  // We currently don't support vertical alignment of complex data.
  let can_align = rows
    .iter()
    .flat_map(|row| row.as_ref().iter())
    .all(|column| !column.as_ref().contains('\n'));

  let diff_widths = |range: Range<usize>, target_row: &R| -> usize {
    let mut max_width = 0_usize;

    for candidate_row in rows {
      let row_len = candidate_row.as_ref().len();

      let row_range = cmp::min(range.start, row_len.saturating_sub(1))..cmp::min(range.end, row_len);

      let width = candidate_row
        .as_ref()
        .get(row_range)
        .unwrap_or(&[])
        .iter()
        .map(|column| column.as_ref().chars().count())
        .fold(0, usize::saturating_add);
      max_width = cmp::max(max_width, width);
    }

    let row_width = target_row
      .as_ref()
      .get(range)
      .unwrap_or(&[])
      .iter()
      .map(|column| column.as_ref().chars().count())
      .fold(0, usize::saturating_add);

    max_width.saturating_sub(row_width)
  };

  for (row_idx, row) in rows.iter().enumerate() {
    if row_idx != 0 {
      out += newline;
    }

    let mut last_align_idx = 0_usize;

    for (column_index, column) in row.as_ref().iter().enumerate() {
      if column_index > separator_range.start && column_index <= separator_range.end.saturating_add(1) && column_index < row.as_ref().len()
      {
        out += separator;
      }

      out += column.as_ref();

      if can_align
        && align_range.start <= column_index
        && align_range.end > column_index
        && column_index.saturating_add(1) < row.as_ref().len()
      {
        let next_column_index = column_index.saturating_add(1);
        let diff = diff_widths(last_align_idx..next_column_index, row);
        out.extend(repeat_n(" ", diff));
        last_align_idx = next_column_index;
      }
    }
  }

  out
}

/// Special handling of blank lines.
///
/// A design decision was made in the parser that newline (LF) characters
/// and whitespace (" ", and \t) are part of separate tokens.
///
/// Generally we count the amount of blank lines by counting LF characters in a token,
/// however if any of the consecutive blank lines contain empty characters,
/// this way of counting becomes unreliable.
///
/// So we check if the newlines are followed by whitespace,
/// then newlines again, and return the count here,
/// and we can add these values up.
fn dangling_newlines(token: &SyntaxToken) -> Option<usize> {
  let newline_count = token.text().newline_count();

  token
    .next_sibling_or_token()
    .filter(|next| next.kind() == WHITESPACE)
    .and_then(|next| next.next_sibling_or_token())
    .filter(|next_next| next_next.kind() == NEWLINE)
    .map(|_| newline_count)
}
