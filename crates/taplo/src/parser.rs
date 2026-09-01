//! Lossless TOML parsing into protocol-checked Rowan syntax trees.
//!
//! Recoverable grammar failures become ordered [`crate::parser::Diagnostic`] values while Rowan
//! builder protocol failures return [`crate::parser::ParseFailure`]. Composite values use
//! heap-owned frames so deeply nested arrays and inline tables do not recurse through the Rust call
//! stack.

use std::error::Error as StandardError;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fmt::Result as FormattingResult;
use std::mem::replace;
use std::ops::Range;

use rowan::BuildError;
use rowan::GreenNode;
use rowan::GreenNodeBuilder;
use rowan::TextRange;
use rowan::TextSize;

use crate::dom;
use crate::syntax::SyntaxKind;
use crate::syntax::SyntaxLexer;
use crate::syntax::SyntaxNode;
use crate::syntax::kind::ARRAY;
use crate::syntax::kind::BOOL;
use crate::syntax::kind::BRACE_END;
use crate::syntax::kind::BRACE_START;
use crate::syntax::kind::BRACKET_END;
use crate::syntax::kind::BRACKET_START;
use crate::syntax::kind::COMMA;
use crate::syntax::kind::COMMENT;
use crate::syntax::kind::DATE;
use crate::syntax::kind::DATE_TIME_LOCAL;
use crate::syntax::kind::DATE_TIME_OFFSET;
use crate::syntax::kind::ENTRY;
use crate::syntax::kind::EQ;
use crate::syntax::kind::ERROR;
use crate::syntax::kind::FLOAT;
use crate::syntax::kind::IDENT;
use crate::syntax::kind::IDENT_WITH_GLOB;
use crate::syntax::kind::INLINE_TABLE;
use crate::syntax::kind::INTEGER;
use crate::syntax::kind::INTEGER_BIN;
use crate::syntax::kind::INTEGER_HEX;
use crate::syntax::kind::INTEGER_OCT;
use crate::syntax::kind::KEY;
use crate::syntax::kind::MULTI_LINE_STRING;
use crate::syntax::kind::MULTI_LINE_STRING_LITERAL;
use crate::syntax::kind::NEWLINE;
use crate::syntax::kind::PERIOD;
use crate::syntax::kind::ROOT;
use crate::syntax::kind::STRING;
use crate::syntax::kind::STRING_LITERAL;
use crate::syntax::kind::TABLE_ARRAY_HEADER;
use crate::syntax::kind::TABLE_HEADER;
use crate::syntax::kind::TIME;
use crate::syntax::kind::VALUE;
use crate::syntax::kind::WHITESPACE;
use crate::util::CharacterPolicy;
use crate::util::EscapeError;
use crate::util::check_escape;
use crate::util::validate_characters;

/// A recoverable syntax diagnostic produced while parsing.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct Diagnostic {
  /// The source span associated with the diagnostic.
  range: TextRange,

  /// Human-readable diagnostic message.
  message: String,
}

impl Diagnostic {
  /// Source span associated with this diagnostic.
  #[allow(
    clippy::single_call_fn,
    reason = "the public accessor is the stable projection used when formatter recovery ranges are collected"
  )]
  #[must_use]
  pub const fn range(&self) -> TextRange {
    self.range
  }

  /// Human-readable diagnostic message.
  #[must_use]
  pub fn message(&self) -> &str {
    &self.message
  }
}

impl Display for Diagnostic {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> FormattingResult {
    write!(
      formatter,
      "{} ({}..{})",
      self.message,
      u32::from(self.range.start()),
      u32::from(self.range.end())
    )
  }
}
impl StandardError for Diagnostic {}

/// Fatal syntax-tree construction failure.
#[derive(Debug, Clone, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ParseFailure {
  /// Rowan rejected a builder operation or final tree shape.
  #[error("failed to construct the syntax tree: {0}")]
  Build(#[from] BuildError),
}

/// Parse a TOML document into a [Rowan green tree](rowan::GreenNode).
///
/// The parsing will not stop at unexpected or invalid tokens.
/// Instead errors will be collected with their character offsets and lengths,
/// and the invalid token(s) will have the `ERROR` kind in the final tree.
///
/// The parser will also validate comment and string contents, looking for
/// invalid escape sequences and invalid characters.
/// These will also be reported as syntax errors.
///
/// This does not check for semantic errors such as duplicate keys.
///
/// # Errors
///
/// Returns [`crate::parser::ParseFailure`] when Rowan cannot construct the lossless syntax
/// tree. Recoverable TOML syntax problems remain available through
/// [`Parse::diagnostics`].
pub fn parse(source: &str) -> Result<Parse, ParseFailure> {
  Parser::new(source).parse()
}

/// A hand-written parser that uses the Logos lexer
/// to tokenize the source, then constructs
/// a Rowan green tree from them.
pub(crate) struct Parser<'p> {
  /// Whether lexer whitespace is copied immediately instead of exposed to the active production.
  skip_whitespace:    bool,
  /// Whether query-only glob identifiers and bracket-separated key segments are accepted.
  key_pattern_syntax: bool,
  /// Current significant token retained until a production consumes or reclassifies it.
  current_token:      Option<SyntaxKind>,

  /// Tokens that delimit the active recovery scopes and must not be consumed by a first error.
  recovery_tokens: Vec<SyntaxKind>,

  /// Private lossless token source.
  lexer:       SyntaxLexer<'p>,
  /// Protocol-checked Rowan construction adapter.
  builder:     SyntaxTreeBuilder<'p>,
  /// Ordered recoverable syntax diagnostics.
  diagnostics: Vec<Diagnostic>,
}

impl Parser<'_> {
  /// Required for patch syntax
  /// and key matches.
  ///
  /// It allows a part of glob syntax in identifiers as well.
  pub(crate) fn parse_key_only(mut self) -> Result<Parse, ParseFailure> {
    self.key_pattern_syntax = true;
    let result = self.with_node(KEY, Self::parse_key);
    Self::complete(result)?;

    Ok(Parse {
      green_node:  self.builder.finish()?,
      diagnostics: self.diagnostics,
    })
  }
}

/// This is just a convenience type during parsing.
/// It allows using "?", making the code cleaner.
type ParserResult<T> = Result<T, ParserControl>;

/// Internal distinction between recoverable syntax control flow and fatal
/// Rowan construction failures.
#[derive(Debug)]
enum ParserControl {
  /// Recoverable syntax mismatch already represented by a diagnostic.
  Syntax,
  /// Fatal Rowan builder failure.
  Build(BuildError),
}

impl From<BuildError> for ParserControl {
  fn from(source: BuildError) -> Self {
    Self::Build(source)
  }
}

/// Private typed adapter around Rowan's protocol-checked builder.
#[derive(Default)]
struct SyntaxTreeBuilder<'cache> {
  /// Rowan builder that owns the in-progress green tree.
  inner: GreenNodeBuilder<'cache>,
}

impl SyntaxTreeBuilder<'_> {
  /// Start one syntax node.
  fn start_node(&mut self, kind: SyntaxKind) {
    self.inner.start_node(kind.into());
  }

  /// Append one lossless token.
  fn token(&mut self, kind: SyntaxKind, text: &str) -> Result<(), BuildError> {
    self.inner.token(kind.into(), text)
  }

  /// Finish the current syntax node.
  fn finish_node(&mut self) -> Result<(), BuildError> {
    self.inner.finish_node()
  }

  /// Complete the sole syntax-tree root.
  fn finish(self) -> Result<GreenNode, BuildError> {
    self.inner.finish()
  }
}

/// One heap-stacked value frame with its active phase and caller-owned cleanup.
struct ValueFrame {
  /// Current scalar or composite parsing phase.
  phase:         ValuePhase,
  /// Syntax nodes the caller expects this frame to close.
  completion:    ValueCompletion,
  /// Recovery-token length to restore after this frame finishes.
  recovery_base: usize,
}

/// Active grammar phase for one iterative value frame.
enum ValuePhase {
  /// Inspect and consume one scalar or open a composite value.
  Value,
  /// Continue an array after its opening bracket or latest child.
  Array(ArrayState),
  /// Continue an inline table after its opening brace or latest entry.
  InlineTable(InlineTableState),
}

impl ValuePhase {
  /// Return the caller-owned nodes for a child scheduled by this composite phase.
  const fn child_completion(&self) -> Option<ValueCompletion> {
    match *self {
      Self::Value => None,
      Self::Array(_) => Some(ValueCompletion::ArrayItem),
      Self::InlineTable(_) => Some(ValueCompletion::InlineEntry),
    }
  }

  /// Return whether completing this phase must close a composite syntax node.
  const fn owns_syntax_node(&self) -> bool {
    match *self {
      Self::Value => false,
      Self::Array(_) | Self::InlineTable(_) => true,
    }
  }
}

/// Syntax-node completion responsibility inherited from a frame's caller.
#[derive(Clone, Copy)]
enum ValueCompletion {
  /// The surrounding parser owns any enclosing `VALUE` node.
  Root,
  /// Close the `VALUE` wrapper opened for one array item.
  ArrayItem,
  /// Close an inline-table value and then its containing `ENTRY`.
  InlineEntry,
}

/// Outcome of one successful iterative value-frame transition.
enum ValueTransition {
  /// Revisit the same frame after consuming or opening structure.
  Requeue,
  /// Revisit the parent only after a newly opened child frame completes.
  ChildScheduled,
  /// Close this frame's composite and caller-owned syntax nodes.
  Complete,
}

/// Separator state for one active array frame.
struct ArrayState {
  /// Whether the array has not encountered a value or comma yet.
  first:      bool,
  /// Whether the most recently consumed structural token was a comma.
  comma_last: bool,
}

/// Separator state for one active inline-table frame.
struct InlineTableState {
  /// Whether the next significant token must be a comma or closing brace.
  expect_comma_or_end: bool,
}

impl<'p> Parser<'p> {
  /// Construct a parser with ordinary TOML key syntax and an empty recovery stack.
  pub(crate) fn new(source: &'p str) -> Self {
    Parser {
      current_token:      None,
      skip_whitespace:    true,
      key_pattern_syntax: false,
      recovery_tokens:    Vec::new(),
      lexer:              SyntaxLexer::new(source),
      builder:            SyntaxTreeBuilder::default(),
      diagnostics:        Vec::default(),
    }
  }

  /// Parse a complete document root and publish its diagnostics with the finished tree.
  fn parse(mut self) -> Result<Parse, ParseFailure> {
    let result = self.with_node(ROOT, Self::parse_root);
    Self::complete(result)?;

    Ok(Parse {
      green_node:  self.builder.finish()?,
      diagnostics: self.diagnostics,
    })
  }

  /// Record a recoverable mismatch and consume a non-boundary token as `ERROR`.
  fn error(&mut self, message: &str) -> ParserResult<()> {
    let diagnostic = Diagnostic {
      range:   self.current_range(),
      message: message.into(),
    };

    let same_error = self.diagnostics.last().is_some_and(|existing| existing == &diagnostic);

    if same_error {
      Self::recover(self.token_as(ERROR))?;
    } else {
      self.add_diagnostic(&diagnostic);
      if self.current_token.is_some_and(|token| !self.is_recovery_token(token)) {
        Self::recover(self.token_as(ERROR))?;
      }
    }

    Err(ParserControl::Syntax)
  }

  /// Record a recoverable mismatch without consuming the current recovery-boundary token.
  fn report_error(&mut self, message: &str) -> ParserResult<()> {
    self.add_diagnostic(&Diagnostic {
      range:   self.current_range(),
      message: message.into(),
    });
    Err(ParserControl::Syntax)
  }

  /// Append a diagnostic unless it exactly duplicates the immediately preceding diagnostic.
  fn add_diagnostic(&mut self, diagnostic: &Diagnostic) {
    if self
      .diagnostics
      .last()
      .is_some_and(|last_diagnostic| last_diagnostic == diagnostic)
    {
      return;
    }

    self.diagnostics.push(diagnostic.clone());
  }

  /// Ignore a recoverable syntax result while retaining fatal builder failure.
  const fn recover(result: ParserResult<()>) -> ParserResult<()> {
    match result {
      Ok(()) | Err(ParserControl::Syntax) => Ok(()),
      Err(source @ ParserControl::Build(_)) => Err(source),
    }
  }

  /// Accept recoverable syntax control flow at a public parse boundary.
  const fn complete(result: ParserResult<()>) -> Result<(), ParseFailure> {
    match result {
      Ok(()) | Err(ParserControl::Syntax) => Ok(()),
      Err(ParserControl::Build(source)) => Err(ParseFailure::Build(source)),
    }
  }

  /// Run one operation inside a balanced syntax node.
  fn with_node<T>(&mut self, kind: SyntaxKind, operation: impl FnOnce(&mut Self) -> ParserResult<T>) -> ParserResult<T> {
    self.builder.start_node(kind);
    let result = operation(self);
    let finish_result = self.builder.finish_node().map_err(ParserControl::from);

    match result {
      Err(source @ ParserControl::Build(_)) => Err(source),
      completed_operation => {
        finish_result?;
        completed_operation
      }
    }
  }

  /// Convert the current lexer span into Rowan's coordinate space without truncation or panic.
  fn current_range(&self) -> TextRange {
    Self::range(self.lexer.span())
  }

  /// Saturate diagnostic coordinates that cannot be represented by Rowan's 32-bit offsets.
  fn text_size(offset: usize) -> TextSize {
    TextSize::try_from(offset).unwrap_or_else(|_| TextSize::new(u32::MAX))
  }

  /// Convert a lexer span into Rowan's coordinate space without truncation or panic.
  #[allow(
    clippy::single_call_fn,
    reason = "the named conversion keeps paired span endpoints on one saturating Rowan-coordinate policy"
  )]
  fn range(span: Range<usize>) -> TextRange {
    TextRange::new(Self::text_size(span.start), Self::text_size(span.end))
  }

  /// Construct a zero-width diagnostic range relative to the current lexer token.
  fn relative_range(&self, relative_offset: usize) -> TextRange {
    let absolute_offset = self.lexer.span().start.saturating_add(relative_offset);
    let offset = Self::text_size(absolute_offset);
    TextRange::new(offset, offset)
  }

  /// Record one diagnostic at an offset within the current lexer token.
  fn add_relative_error(&mut self, relative_offset: usize, message: &str) {
    self.add_diagnostic(&Diagnostic {
      range:   self.relative_range(relative_offset),
      message: message.into(),
    });
  }

  /// Run `operation` with exact recovery delimiters, restoring the enclosing scope afterward.
  fn with_recovery_tokens<T>(&mut self, tokens: &[SyntaxKind], operation: impl FnOnce(&mut Self) -> T) -> T {
    let previous_len = self.recovery_tokens.len();
    self.recovery_tokens.extend_from_slice(tokens);
    let result = operation(self);
    self.recovery_tokens.truncate(previous_len);
    result
  }

  /// Whether `token` belongs to any active recovery scope.
  fn is_recovery_token(&self, token: SyntaxKind) -> bool {
    self.recovery_tokens.contains(&token)
  }

  /// Append one synthetic or losslessly copied token through the checked builder.
  fn insert_token(&mut self, kind: SyntaxKind, text: &str) -> ParserResult<()> {
    self.builder.token(kind, text).map_err(Into::into)
  }

  /// Consume the required token or record the supplied mismatch, including unexpected EOF.
  fn must_token_or(&mut self, kind: SyntaxKind, message: &str) -> ParserResult<()> {
    match self.get_token() {
      Ok(token) => {
        if kind == token {
          self.token()
        } else {
          self.error(message)
        }
      }
      Err(ParserControl::Syntax) => {
        self.add_diagnostic(&Diagnostic {
          range:   self.current_range(),
          message: "unexpected EOF".into(),
        });
        Err(ParserControl::Syntax)
      }
      Err(source @ ParserControl::Build(_)) => Err(source),
    }
  }

  /// Append the current token without stepping into trailing whitespace.
  fn add_token(&mut self) -> ParserResult<()> {
    match self.get_token() {
      Err(ParserControl::Syntax) => Err(ParserControl::Syntax),
      Err(source @ ParserControl::Build(_)) => Err(source),
      Ok(token) => {
        self.builder.token(token, self.lexer.slice())?;
        self.current_token = None;
        Ok(())
      }
    }
  }

  /// Append the current token under its lexical syntax kind and advance.
  fn token(&mut self) -> ParserResult<()> {
    match self.get_token() {
      Err(source) => Err(source),
      Ok(token) => self.token_as(token),
    }
  }

  /// This function implicitly calls `step`,
  /// it was definitely not a good design decision
  /// but changing this behaviour involves a
  /// different syntax tree and breakages down the line.
  fn token_as(&mut self, kind: SyntaxKind) -> ParserResult<()> {
    self.token_as_no_step(kind)?;
    self.step()?;
    Ok(())
  }

  /// Append the current token under a parser-selected kind without advancing.
  fn token_as_no_step(&mut self, kind: SyntaxKind) -> ParserResult<()> {
    match self.get_token() {
      Err(ParserControl::Syntax) => return Err(ParserControl::Syntax),
      Err(source @ ParserControl::Build(_)) => return Err(source),
      Ok(_) => {
        self.builder.token(kind, self.lexer.slice())?;
      }
    }

    Ok(())
  }

  /// Record every invalid source offset produced by one character validator.
  fn record_character_errors(&mut self, result: Result<(), Vec<usize>>, message: &str) {
    if let Err(error_indices) = result {
      for error_index in error_indices {
        self.add_relative_error(error_index, message);
      }
    }
  }

  /// Record every invalid escape offset while preserving tolerant parsing.
  fn record_escape_errors(&mut self, result: Result<(), Vec<EscapeError>>) {
    if let Err(errors) = result {
      for error in errors {
        self.add_relative_error(error.offset(), "invalid escape sequence");
      }
    }
  }

  /// Validate and append one comment token.
  #[allow(
    clippy::single_call_fn,
    reason = "the helper keeps comment character diagnostics coupled to lossless comment-token insertion"
  )]
  fn append_comment(&mut self, token: SyntaxKind) -> ParserResult<()> {
    let validation = validate_characters(self.lexer.slice(), CharacterPolicy::CommentOrLiteral);
    self.record_character_errors(validation, "invalid character in comment");
    self.insert_token(token, self.lexer.slice())
  }

  /// Advance through comments and optionally hidden whitespace to the next significant token.
  fn step(&mut self) -> ParserResult<()> {
    self.current_token = None;
    while let Some(token) = self.lexer.next() {
      match token {
        COMMENT => self.append_comment(token)?,
        WHITESPACE if self.skip_whitespace => self.insert_token(token, self.lexer.slice())?,
        ERROR => {
          self.insert_token(token, self.lexer.slice())?;
          self.add_diagnostic(&Diagnostic {
            range:   self.current_range(),
            message: "unexpected token".into(),
          });
        }
        _ => {
          self.current_token = Some(token);
          break;
        }
      }
    }
    Ok(())
  }

  /// Return the current significant token, advancing the lexer when necessary.
  fn get_token(&mut self) -> ParserResult<SyntaxKind> {
    if self.current_token.is_none() {
      self.step()?;
    }

    self.current_token.ok_or(ParserControl::Syntax)
  }

  /// Close the active root entry wrapper, if any.
  fn finish_root_entry(&mut self, entry_started: &mut bool) -> ParserResult<()> {
    if *entry_started {
      self.builder.finish_node()?;
      *entry_started = false;
    }
    Ok(())
  }

  /// Parse one document header line after enforcing root line boundaries.
  #[allow(
    clippy::single_call_fn,
    reason = "the helper owns the root transition from an optional entry wrapper into regular or array-table header parsing"
  )]
  fn parse_root_header_line(&mut self, entry_started: &mut bool, not_newline: &mut bool) -> ParserResult<()> {
    self.finish_root_entry(entry_started)?;
    if *not_newline {
      Self::recover(self.error("expected new line"))?;
      return Ok(());
    }
    *not_newline = true;

    let result = if self.lexer.remainder().starts_with('[') {
      self.with_recovery_tokens(&[NEWLINE], |parser| {
        parser.with_node(TABLE_ARRAY_HEADER, Self::parse_table_array_header)
      })
    } else {
      self.with_recovery_tokens(&[NEWLINE], |parser| parser.with_node(TABLE_HEADER, Self::parse_table_header))
    };
    Self::recover(result)
  }

  /// Consume one document newline and close the preceding entry wrapper.
  #[allow(
    clippy::single_call_fn,
    reason = "the helper keeps newline consumption and entry-wrapper closure synchronized at the document grammar boundary"
  )]
  fn parse_root_newline(&mut self, entry_started: &mut bool, not_newline: &mut bool) -> ParserResult<()> {
    *not_newline = false;
    self.finish_root_entry(entry_started)?;
    Self::recover(self.token())
  }

  /// Parse one ordinary document entry after enforcing root line boundaries.
  #[allow(
    clippy::single_call_fn,
    reason = "the helper owns manual entry-wrapper lifecycle and newline recovery for the root grammar's ordinary-entry branch"
  )]
  fn parse_root_entry(&mut self, entry_started: &mut bool, not_newline: &mut bool) -> ParserResult<()> {
    if *not_newline {
      Self::recover(self.error("expected new line"))?;
      return Ok(());
    }
    self.finish_root_entry(entry_started)?;
    *not_newline = true;
    self.builder.start_node(ENTRY);
    *entry_started = true;
    let result = self.with_recovery_tokens(&[NEWLINE], Self::parse_entry);
    Self::recover(result)
  }

  /// Parse the document-level sequence of entries and table headers.
  #[allow(
    clippy::single_call_fn,
    reason = "the grammar production owns root line boundaries and manually balanced entry wrappers"
  )]
  fn parse_root(&mut self) -> ParserResult<()> {
    // Ensure we have newlines between entries
    let mut not_newline = false;

    // We want to make sure that an entry spans the
    // entire line, so we start/close its node manually.
    let mut entry_started = false;

    loop {
      let token = match self.get_token() {
        Ok(token) => token,
        Err(ParserControl::Syntax) => break,
        Err(source @ ParserControl::Build(_)) => return Err(source),
      };

      match token {
        BRACKET_START => self.parse_root_header_line(&mut entry_started, &mut not_newline)?,
        NEWLINE => self.parse_root_newline(&mut entry_started, &mut not_newline)?,
        _ => self.parse_root_entry(&mut entry_started, &mut not_newline)?,
      }
    }
    self.finish_root_entry(&mut entry_started)?;

    Ok(())
  }

  /// Parse one regular table header and recover within its key boundary.
  #[allow(
    clippy::single_call_fn,
    reason = "the grammar production keeps regular-header delimiters and key recovery explicit"
  )]
  fn parse_table_header(&mut self) -> ParserResult<()> {
    self.must_token_or(BRACKET_START, r#"expected "[""#)?;
    let key = self.with_node(KEY, Self::parse_key);
    Self::recover(key)?;
    self.must_token_or(BRACKET_END, r#"expected "]""#)?;

    Ok(())
  }

  /// Parse one array-of-tables header without consuming source after its second closing bracket.
  #[allow(
    clippy::single_call_fn,
    reason = "the grammar production brackets the whitespace-mode transition required by array-table headers"
  )]
  fn parse_table_array_header(&mut self) -> ParserResult<()> {
    self.skip_whitespace = false;
    let result = self.parse_table_array_header_inner();
    self.skip_whitespace = true;

    match result {
      Ok(()) => self.step(),
      Err(source) => Err(source),
    }
  }

  /// Parse an array-table header without stepping past its second closing bracket.
  fn parse_table_array_header_inner(&mut self) -> ParserResult<()> {
    self.must_token_or(BRACKET_START, r#"expected "[[""#)?;
    self.must_token_or(BRACKET_START, r#"expected "[[""#)?;
    self.skip_whitespace = true;
    let key = self.with_node(KEY, Self::parse_key);
    Self::recover(key)?;
    self.skip_whitespace = false;
    let first_close = self.must_token_or(BRACKET_END, r#"expected "]]""#);
    Self::recover(first_close)?;

    let token = self.get_token()?;
    if token == BRACKET_END {
      self.token_as_no_step(token)
    } else {
      self.error(r#"expected "]]"#)
    }
  }

  /// Parse one key, equals delimiter, and iteratively parsed value.
  #[allow(
    clippy::single_call_fn,
    reason = "the grammar production owns the balanced KEY and VALUE wrappers for one entry"
  )]
  fn parse_entry(&mut self) -> ParserResult<()> {
    self.with_node(KEY, Self::parse_key)?;
    self.must_token_or(EQ, r#"expected "=""#)?;
    self.with_node(VALUE, Self::parse_value)?;

    Ok(())
  }

  /// Return the next key token or finish a complete key at end of input.
  #[allow(
    clippy::single_call_fn,
    reason = "the helper distinguishes a complete key boundary from an incomplete trailing period when the lexer reaches EOF"
  )]
  fn next_key_token(&mut self, after_period: bool) -> ParserResult<Option<SyntaxKind>> {
    match self.get_token() {
      Ok(token) => Ok(Some(token)),
      Err(ParserControl::Syntax) if after_period => self.error("unexpected end of input").map(|()| None),
      Err(ParserControl::Syntax) => Ok(None),
      Err(source @ ParserControl::Build(_)) => Err(source),
    }
  }

  /// Consume one key period while rejecting adjacent periods.
  #[allow(
    clippy::single_call_fn,
    reason = "the helper owns the dotted-key separator state transition and its adjacent-period diagnostic"
  )]
  fn consume_key_period(&mut self, after_period: &mut bool) -> ParserResult<()> {
    if *after_period {
      return self.error(r#"unexpected ".""#);
    }
    self.token()?;
    *after_period = true;
    Ok(())
  }

  /// Parse the identifier required after one consumed key period.
  #[allow(
    clippy::single_call_fn,
    reason = "the helper keeps the trailing-period requirement and its non-consuming recovery diagnostic together"
  )]
  fn consume_identifier_after_period(&mut self, after_period: &mut bool) -> ParserResult<()> {
    match self.parse_ident() {
      Ok(()) => {}
      Err(ParserControl::Syntax) => return self.report_error("expected identifier"),
      Err(source @ ParserControl::Build(_)) => return Err(source),
    }
    *after_period = false;
    Ok(())
  }

  /// Parse a dotted key, including query-only glob and bracket syntax when enabled.
  fn parse_key(&mut self) -> ParserResult<()> {
    match self.parse_ident() {
      Ok(()) => {}
      Err(ParserControl::Syntax) => return self.report_error("expected identifier"),
      Err(source @ ParserControl::Build(_)) => return Err(source),
    }

    let mut after_period = false;
    loop {
      let Some(token) = self.next_key_token(after_period)? else {
        return Ok(());
      };

      match token {
        PERIOD => self.consume_key_period(&mut after_period)?,
        BRACKET_START if self.key_pattern_syntax => {
          self.parse_bracketed_key_segment()?;
          after_period = false;
        }
        _ if after_period => self.consume_identifier_after_period(&mut after_period)?,
        _ if self.key_pattern_syntax => return self.error("unexpected identifier"),
        _ => break,
      }
    }

    Ok(())
  }

  /// Parse one bracket-delimited query-key segment.
  #[allow(
    clippy::single_call_fn,
    reason = "the helper isolates query-only bracket delimiters and their identifier recovery from ordinary dotted-key parsing"
  )]
  fn parse_bracketed_key_segment(&mut self) -> ParserResult<()> {
    self.step()?;
    match self.parse_ident() {
      Ok(()) => {}
      Err(ParserControl::Syntax) => return self.error("expected identifier"),
      Err(source @ ParserControl::Build(_)) => return Err(source),
    }
    if self.get_token()? != BRACKET_END {
      self.error(r#"expected "]""#)?;
    }
    self.step()
  }

  /// Split one floating-point token into dotted identifier segments.
  #[allow(
    clippy::single_call_fn,
    reason = "the helper preserves the query-key normalization that emits periods between float-token segments"
  )]
  fn insert_float_key_segments(&mut self) -> ParserResult<()> {
    let segments = self.lexer.slice().split('.').map(ToOwned::to_owned).collect::<Vec<_>>();
    for (index, segment) in segments.into_iter().enumerate() {
      if index != 0 {
        self.insert_token(PERIOD, ".")?;
      }
      self.insert_token(IDENT, &segment)?;
    }
    self.step()
  }

  /// Parse or normalize one key identifier while preserving its original token text.
  fn parse_ident(&mut self) -> ParserResult<()> {
    let token = self.get_token()?;
    match token {
      IDENT => self.token(),
      IDENT_WITH_GLOB => {
        if self.key_pattern_syntax {
          self.token_as(IDENT)
        } else {
          self.error("expected identifier")
        }
      }
      INTEGER_HEX | INTEGER_BIN | INTEGER_OCT | BOOL | DATE => self.token_as(IDENT),
      INTEGER => {
        if self.lexer.slice().starts_with('+') {
          Err(ParserControl::Syntax)
        } else {
          self.token_as(IDENT)
        }
      }
      STRING_LITERAL => {
        let validation = validate_characters(self.lexer.slice(), CharacterPolicy::CommentOrLiteral);
        self.record_character_errors(validation, "invalid control character in string literal");
        self.token_as(IDENT)
      }
      STRING => {
        let character_validation = validate_characters(self.lexer.slice(), CharacterPolicy::BasicString);
        self.record_character_errors(character_validation, "invalid character in string");
        let escape_validation = check_escape(self.lexer.slice());
        self.record_escape_errors(escape_validation);
        self.token_as(IDENT)
      }
      FLOAT => {
        if self.lexer.slice().starts_with('0') {
          self.error("zero-padded numbers are not allowed")
        } else if self.lexer.slice().starts_with('+') {
          Err(ParserControl::Syntax)
        } else {
          self.insert_float_key_segments()
        }
      }
      _ => self.error("expected identifier"),
    }
  }

  /// Schedule one child value frame or close an invalid child-scheduling state.
  #[allow(
    clippy::single_call_fn,
    reason = "the helper owns parent requeue ordering and child completion metadata for the iterative value-frame stack"
  )]
  fn schedule_child_value_frame(
    &mut self,
    frame: ValueFrame,
    frames: &mut Vec<ValueFrame>,
    child_recovery_base: usize,
  ) -> ParserResult<bool> {
    let Some(completion) = frame.phase.child_completion() else {
      self.complete_value_frame(&frame)?;
      return Ok(false);
    };
    frames.push(frame);
    frames.push(ValueFrame {
      phase: ValuePhase::Value,
      completion,
      recovery_base: child_recovery_base,
    });
    Ok(true)
  }

  /// Parse one scalar or arbitrarily nested composite through heap-owned value frames.
  #[allow(
    clippy::single_call_fn,
    reason = "the grammar production is the sole owner of iterative frame scheduling and recovery-stack restoration"
  )]
  fn parse_value(&mut self) -> ParserResult<()> {
    let mut frames = vec![ValueFrame {
      phase:         ValuePhase::Value,
      completion:    ValueCompletion::Root,
      recovery_base: self.recovery_tokens.len(),
    }];
    let mut had_syntax_error = false;

    while let Some(mut frame) = frames.pop() {
      let child_recovery_base = self.recovery_tokens.len();
      match self.advance_value_frame(&mut frame) {
        Ok(ValueTransition::Requeue) => frames.push(frame),
        Ok(ValueTransition::ChildScheduled) => {
          let child_scheduled = self.schedule_child_value_frame(frame, &mut frames, child_recovery_base)?;
          had_syntax_error = had_syntax_error || !child_scheduled;
        }
        Ok(ValueTransition::Complete) => self.complete_value_frame(&frame)?,
        Err(ParserControl::Syntax) => {
          had_syntax_error = true;
          self.complete_value_frame(&frame)?;
        }
        Err(source @ ParserControl::Build(_)) => return Err(source),
      }
    }

    if had_syntax_error {
      Err(ParserControl::Syntax)
    } else {
      Ok(())
    }
  }

  /// Advance whichever scalar or composite phase is active in `frame`.
  fn advance_value_frame(&mut self, frame: &mut ValueFrame) -> ParserResult<ValueTransition> {
    let phase = replace(&mut frame.phase, ValuePhase::Value);
    match phase {
      ValuePhase::Value => self.begin_value(&mut frame.phase),
      ValuePhase::Array(mut state) => {
        let result = self.continue_array(&mut state);
        frame.phase = ValuePhase::Array(state);
        result
      }
      ValuePhase::InlineTable(mut state) => {
        let result = self.continue_inline_table(&mut state);
        frame.phase = ValuePhase::InlineTable(state);
        result
      }
    }
  }

  /// Close the syntax nodes owned by a completed or recoverably failed frame.
  fn complete_value_frame(&mut self, frame: &ValueFrame) -> ParserResult<()> {
    if frame.phase.owns_syntax_node() {
      self.builder.finish_node()?;
    }
    match frame.completion {
      ValueCompletion::Root => {}
      ValueCompletion::ArrayItem => self.builder.finish_node()?,
      ValueCompletion::InlineEntry => {
        self.builder.finish_node()?;
        self.builder.finish_node()?;
      }
    }
    self.recovery_tokens.truncate(frame.recovery_base);
    Ok(())
  }

  /// Consume one scalar token or turn the current frame into a composite phase.
  fn begin_value(&mut self, phase: &mut ValuePhase) -> ParserResult<ValueTransition> {
    let token = match self.get_token() {
      Ok(token) => token,
      Err(ParserControl::Syntax) => return self.error("expected value").map(|()| ValueTransition::Complete),
      Err(source @ ParserControl::Build(_)) => return Err(source),
    };

    self.dispatch_value_token(token, phase)
  }

  /// Dispatch one recognized value token to its semantic transition owner.
  fn dispatch_value_token(&mut self, token: SyntaxKind, phase: &mut ValuePhase) -> ParserResult<ValueTransition> {
    match token {
      BOOL | DATE_TIME_OFFSET | DATE_TIME_LOCAL | DATE | TIME => self.parse_boolean_or_date(),
      INTEGER => self.parse_decimal_integer(),
      INTEGER_BIN => self.parse_radix_integer(2),
      INTEGER_HEX => self.parse_radix_integer(16),
      INTEGER_OCT => self.parse_radix_integer(8),
      FLOAT => self.parse_float(),
      STRING | MULTI_LINE_STRING | STRING_LITERAL | MULTI_LINE_STRING_LITERAL => self.parse_string(token),
      BRACKET_START | BRACE_START => self.open_composite(token, phase),
      _ => self.error("expected value").map(|()| ValueTransition::Complete),
    }
  }

  /// Consume a Boolean or already classified date/time token.
  fn parse_boolean_or_date(&mut self) -> ParserResult<ValueTransition> {
    self.token()?;
    Ok(ValueTransition::Complete)
  }

  /// Validate and consume one decimal integer token.
  fn parse_decimal_integer(&mut self) -> ParserResult<ValueTransition> {
    let source = self.lexer.slice();
    self.validate_decimal_number(source, source, "zero-padded integers are not allowed")?;
    Ok(ValueTransition::Complete)
  }

  /// Validate and consume one non-decimal integer token.
  fn parse_radix_integer(&mut self, radix: u32) -> ParserResult<ValueTransition> {
    if check_underscores(self.lexer.slice(), radix) {
      self.token()?;
    } else {
      self.error("invalid underscores")?;
    }
    Ok(ValueTransition::Complete)
  }

  /// Validate and consume one floating-point token.
  fn parse_float(&mut self) -> ParserResult<ValueTransition> {
    let source = self.lexer.slice();
    let integer_part = source.split(['.', 'e', 'E']).next().unwrap_or(source);
    self.validate_decimal_number(integer_part, source, "zero-padded numbers are not allowed")?;
    Ok(ValueTransition::Complete)
  }

  /// Validate and consume one basic or literal string token.
  fn parse_string(&mut self, token: SyntaxKind) -> ParserResult<ValueTransition> {
    match token {
      STRING_LITERAL => {
        let validation = validate_characters(self.lexer.slice(), CharacterPolicy::CommentOrLiteral);
        self.record_character_errors(validation, "invalid control character in string literal");
      }
      MULTI_LINE_STRING_LITERAL => {
        let validation = validate_characters(self.lexer.slice(), CharacterPolicy::MultilineLiteralString);
        self.record_character_errors(validation, "invalid character in string");
      }
      STRING | MULTI_LINE_STRING => {
        let character_validation = if token == STRING {
          validate_characters(self.lexer.slice(), CharacterPolicy::BasicString)
        } else {
          validate_characters(self.lexer.slice(), CharacterPolicy::MultilineBasicString)
        };
        self.record_character_errors(character_validation, "invalid character in string");
        let escape_validation = check_escape(self.lexer.slice());
        self.record_escape_errors(escape_validation);
      }
      _ => return self.error("expected string").map(|()| ValueTransition::Complete),
    }
    self.token()?;
    Ok(ValueTransition::Complete)
  }

  /// Open one array or inline-table frame.
  fn open_composite(&mut self, token: SyntaxKind, phase: &mut ValuePhase) -> ParserResult<ValueTransition> {
    match token {
      BRACKET_START => {
        self.builder.start_node(ARRAY);
        self.must_token_or(BRACKET_START, r#"expected "[""#)?;
        *phase = ValuePhase::Array(ArrayState {
          first:      true,
          comma_last: false,
        });
      }
      BRACE_START => {
        self.builder.start_node(INLINE_TABLE);
        self.must_token_or(BRACE_START, r#"expected "{""#)?;
        *phase = ValuePhase::InlineTable(InlineTableState {
          expect_comma_or_end: false,
        });
      }
      _ => return self.error("expected composite value").map(|()| ValueTransition::Complete),
    }
    Ok(ValueTransition::Requeue)
  }

  /// Validate decimal zero-padding and underscore placement before consuming a token.
  fn validate_decimal_number(&mut self, integer_part: &str, source: &str, zero_padding_message: &'static str) -> ParserResult<()> {
    if is_zero_padded(integer_part) {
      self.error(zero_padding_message)
    } else if !check_underscores(source, 10) {
      self.error("invalid underscores")
    } else {
      self.token()
    }
  }

  /// Consume one array structural token or open its next child value wrapper.
  fn continue_array(&mut self, state: &mut ArrayState) -> ParserResult<ValueTransition> {
    let token = match self.get_token() {
      Ok(token) => token,
      Err(ParserControl::Syntax) => return self.report_error("unexpected EOF").map(|()| ValueTransition::Complete),
      Err(source @ ParserControl::Build(_)) => return Err(source),
    };

    match token {
      BRACKET_END => {
        self.add_token()?;
        Ok(ValueTransition::Complete)
      }
      NEWLINE => {
        self.token()?;
        Ok(ValueTransition::Requeue)
      }
      COMMA => {
        if state.first || state.comma_last {
          Self::recover(self.error(r#"unexpected ",""#))?;
        } else {
          self.token()?;
        }
        state.first = false;
        state.comma_last = true;
        Ok(ValueTransition::Requeue)
      }
      _ => {
        if !state.comma_last && !state.first {
          Self::recover(self.report_error(r#"expected ",""#))?;
        }

        self.recovery_tokens.extend_from_slice(&[COMMA, BRACKET_END, NEWLINE]);
        self.builder.start_node(VALUE);
        state.first = false;
        state.comma_last = false;
        Ok(ValueTransition::ChildScheduled)
      }
    }
  }

  /// Consume one inline-table structural token or open its next entry.
  fn continue_inline_table(&mut self, state: &mut InlineTableState) -> ParserResult<ValueTransition> {
    let token = match self.get_token() {
      Ok(token) => token,
      Err(ParserControl::Syntax) => return self.report_error(r#"expected "}""#).map(|()| ValueTransition::Complete),
      Err(source @ ParserControl::Build(_)) => return Err(source),
    };

    match token {
      BRACE_END => {
        self.add_token()?;
        Ok(ValueTransition::Complete)
      }
      WHITESPACE | NEWLINE | COMMENT => {
        self.add_token()?;
        Ok(ValueTransition::Requeue)
      }
      COMMA => {
        if state.expect_comma_or_end {
          self.add_token()?;
        } else {
          Self::recover(self.error(r#"unexpected ",""#))?;
        }
        state.expect_comma_or_end = false;
        Ok(ValueTransition::Requeue)
      }
      _ => self.schedule_inline_entry(state),
    }
  }

  /// Open one inline-table entry and leave its value for a child frame.
  fn schedule_inline_entry(&mut self, state: &mut InlineTableState) -> ParserResult<ValueTransition> {
    if state.expect_comma_or_end {
      Self::recover(self.report_error(r#"expected "," or "}""#))?;
    }

    self.recovery_tokens.extend_from_slice(&[COMMA, BRACE_END]);
    self.builder.start_node(ENTRY);
    let key = self.with_node(KEY, Self::parse_key);
    Self::recover(key)?;
    let equals = self.must_token_or(EQ, r#"expected "=""#);
    Self::recover(equals)?;
    self.builder.start_node(VALUE);

    state.expect_comma_or_end = true;
    Ok(ValueTransition::ChildScheduled)
  }
}

/// Return whether one signed or unsigned decimal component has forbidden leading zeroes.
#[allow(
  clippy::single_call_fn,
  reason = "the named predicate owns the signed and unsigned leading-zero rule shared by integer and float validation"
)]
fn is_zero_padded(source: &str) -> bool {
  (source.starts_with('0') && source != "0") || (source.starts_with("+0") && source != "+0") || (source.starts_with("-0") && source != "-0")
}

/// Return whether every underscore separates two digits valid for `radix`.
fn check_underscores(source: &str, radix: u32) -> bool {
  if source.starts_with('_') || source.ends_with('_') {
    return false;
  }

  let mut last_char = '\0';

  for character in source.chars() {
    if character == '_' && !last_char.is_digit(radix) {
      return false;
    }
    if !character.is_digit(radix) && last_char == '_' {
      return false;
    }
    last_char = character;
  }

  true
}

/// A successfully constructed lossless syntax tree and its recoverable diagnostics.
#[derive(Debug, Clone)]
pub struct Parse {
  /// Constructed immutable Rowan tree.
  green_node:  GreenNode,
  /// Ordered recoverable syntax diagnostics.
  diagnostics: Vec<Diagnostic>,
}

impl Parse {
  /// Borrow the constructed immutable green tree.
  #[must_use]
  pub const fn green(&self) -> &GreenNode {
    &self.green_node
  }

  /// Borrow the ordered recoverable syntax diagnostics.
  #[must_use]
  pub fn diagnostics(&self) -> &[Diagnostic] {
    &self.diagnostics
  }

  /// Consume this parse and return its immutable green tree.
  #[must_use]
  pub fn into_green(self) -> GreenNode {
    self.green_node
  }

  /// Turn the parse into a syntax node.
  #[must_use]
  pub fn into_syntax(self) -> SyntaxNode {
    SyntaxNode::new_root(self.into_green())
  }

  /// Turn the parse into a DOM tree.
  ///
  /// Any semantic errors that occur will be collected
  /// in the returned DOM node.
  #[must_use]
  pub fn into_dom(self) -> dom::node::Node {
    dom::node_from_syntax(self.into_syntax().into())
  }
}

#[cfg(test)]
/// Recovery, losslessness, progress, and deep composite parser contracts.
mod tests {
  use rowan::TextRange;
  use rowan::TextSize;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;

  use super::Diagnostic;
  use super::Parser;
  use super::ParserControl;
  use super::parse;
  use crate::dom::Node;
  use crate::syntax::SyntaxKind;
  use crate::syntax::kind::BRACKET_END;
  use crate::syntax::kind::COMMA;
  use crate::syntax::kind::DATE;
  use crate::syntax::kind::ENTRY;
  use crate::syntax::kind::ERROR;
  use crate::syntax::kind::IDENT;
  use crate::syntax::kind::NEWLINE;
  use crate::syntax::kind::ROOT;
  use crate::syntax::kind::TIME;
  use crate::syntax::kind::VALUE;

  /// Count diagnostics with one exact message.
  fn diagnostic_count(source: &str, message: &str) -> Result<usize, TestFailure> {
    let parsed = ensure_ok(parse(source), "the parser must construct the diagnostic fixture tree")?;
    Ok(
      parsed
        .diagnostics()
        .iter()
        .filter(|diagnostic| diagnostic.message() == message)
        .count(),
    )
  }

  /// Count syntax nodes and tokens of one kind.
  fn syntax_kind_count(source: &str, kind: SyntaxKind) -> Result<usize, TestFailure> {
    let parsed = ensure_ok(parse(source), "the parser must construct the syntax-kind fixture tree")?;
    Ok(
      parsed
        .into_syntax()
        .descendants_with_tokens()
        .filter(|element| element.kind() == kind)
        .count(),
    )
  }

  /// Verify one missing composite separator while retaining the following item.
  fn ensure_missing_separator_recovery(
    invalid: &str,
    valid: &str,
    diagnostic: &str,
    retained_kind: SyntaxKind,
    retained_count: usize,
  ) -> Result<(), TestFailure> {
    ensure_eq(
      &diagnostic_count(invalid, diagnostic)?,
      &1,
      "one missing composite separator must be reported",
    )?;
    ensure_eq(
      &syntax_kind_count(invalid, retained_kind)?,
      &retained_count,
      "the containing value and every following composite item must remain parsed",
    )?;
    ensure_eq(
      &diagnostic_count(valid, diagnostic)?,
      &0,
      "a valid composite separator must not be diagnosed",
    )
  }

  /// Preserve both array values while diagnosing a missing separator exactly once.
  #[test]
  fn missing_array_comma_preserves_following_value() -> Result<(), TestFailure> {
    ensure_missing_separator_recovery("a = [1 2]", "a = [1, 2]", r#"expected ",""#, VALUE, 3)
  }

  /// Preserve both inline entries while diagnosing a missing separator exactly once.
  #[test]
  fn missing_inline_table_comma_preserves_following_entry() -> Result<(), TestFailure> {
    ensure_missing_separator_recovery("a = { b = 1 c = 2 }", "a = { b = 1, c = 2 }", r#"expected "," or "}""#, ENTRY, 3)
  }

  /// Consume only an extra comma and retain the following array value or inline entry.
  #[test]
  fn consecutive_commas_consume_only_the_offending_delimiter() -> Result<(), TestFailure> {
    let array = "a = [1,,2]";
    ensure_eq(
      &syntax_kind_count(array, ERROR)?,
      &1,
      "one consecutive array comma must become one error token",
    )?;
    ensure_eq(
      &syntax_kind_count(array, VALUE)?,
      &3,
      "the value following the bad array comma must remain parsed",
    )?;

    let inline = "a = { b = 1,, c = 2 }";
    ensure_eq(
      &syntax_kind_count(inline, ERROR)?,
      &1,
      "one consecutive inline-table comma must become one error token",
    )?;
    ensure_eq(
      &syntax_kind_count(inline, ENTRY)?,
      &3,
      "the entry following the bad inline-table comma must remain parsed",
    )
  }

  /// Preserve structural delimiters and subsequent entries when a value is missing or invalid.
  #[test]
  fn recovery_boundaries_survive_missing_values() -> Result<(), TestFailure> {
    let closing_boundary = "a = { b = }\nc = 2";
    let closing_syntax = ensure_ok(parse(closing_boundary), "the closing-boundary fixture tree must construct")?.into_syntax();
    ensure(
      closing_syntax.to_string().contains("}\nc = 2"),
      "a closing brace and following entry must survive value recovery",
    )?;
    ensure_eq(
      &closing_syntax
        .descendants_with_tokens()
        .filter(|element| element.kind() == ERROR)
        .count(),
      &0,
      "a structural recovery boundary must not be rewritten as an error token",
    )?;

    let invalid_value = "a = nope\nb = 2";
    ensure_eq(
      &syntax_kind_count(invalid_value, ERROR)?,
      &1,
      "a bare identifier in value position must become an error token",
    )?;
    ensure_eq(
      &syntax_kind_count(invalid_value, ENTRY)?,
      &2,
      "a non-boundary invalid value must not consume the following entry",
    )
  }

  /// Restore nested recovery-token scopes by length without deleting an outer duplicate.
  #[test]
  fn nested_recovery_scopes_restore_outer_tokens() -> Result<(), TestFailure> {
    let mut parser = Parser::new("");
    parser.with_recovery_tokens(&[COMMA], |outer| -> Result<(), TestFailure> {
      ensure(outer.is_recovery_token(COMMA), "the outer recovery token must be active")?;
      outer.with_recovery_tokens(&[COMMA, NEWLINE], |inner| -> Result<(), TestFailure> {
        ensure(inner.is_recovery_token(COMMA), "a duplicate inner recovery token must be active")?;
        ensure(inner.is_recovery_token(NEWLINE), "the inner-only recovery token must be active")
      })?;
      ensure(
        outer.is_recovery_token(COMMA),
        "leaving the inner scope must retain the outer duplicate",
      )?;
      ensure(
        !outer.is_recovery_token(NEWLINE),
        "leaving the inner scope must remove its unique token",
      )
    })?;
    ensure(
      !parser.is_recovery_token(COMMA),
      "leaving the outer scope must restore the empty recovery stack",
    )
  }

  /// Force token progress after an identical recoverable failure repeats at one range.
  #[test]
  fn repeated_same_range_recovery_forces_progress() -> Result<(), TestFailure> {
    let mut parser = Parser::new("]");
    parser.builder.start_node(ROOT);
    let token = parser.get_token().ok();
    ensure(token == Some(BRACKET_END), "the fixture must lex as a closing delimiter")?;

    parser.with_recovery_tokens(&[BRACKET_END], |scoped| -> Result<(), TestFailure> {
      ensure(
        matches!(scoped.error("expected value"), Err(ParserControl::Syntax)),
        "the first failure must remain recoverable at the active boundary",
      )?;
      ensure(
        matches!(scoped.error("expected value"), Err(ParserControl::Syntax)),
        "the repeated failure must remain recoverable after forcing progress",
      )
    })?;

    ensure_eq(&parser.diagnostics.len(), &1, "the same range and message must be recorded once")?;
    ensure(
      parser.get_token().is_err(),
      "the second same-range failure must consume the boundary and advance",
    )
  }

  /// Suppress only adjacent diagnostics whose range and message are both identical.
  #[test]
  fn error_collection_suppresses_only_exact_adjacent_duplicates() -> Result<(), TestFailure> {
    let mut parser = Parser::new("");
    let first = Diagnostic {
      range:   TextRange::new(TextSize::new(0), TextSize::new(1)),
      message: "first".into(),
    };
    let distinct_message = Diagnostic {
      range:   first.range,
      message: "second".into(),
    };
    let distinct_range = Diagnostic {
      range:   TextRange::new(TextSize::new(1), TextSize::new(2)),
      message: "first".into(),
    };

    parser.add_diagnostic(&first);
    parser.add_diagnostic(&first);
    parser.add_diagnostic(&distinct_message);
    parser.add_diagnostic(&distinct_range);

    ensure(
      parser.diagnostics == [first, distinct_message, distinct_range],
      "only an exact adjacent duplicate may be suppressed",
    )
  }

  /// Normalize every supported query-key segment and reject malformed boundaries.
  #[test]
  fn query_key_parsing_normalizes_globs_brackets_and_float_shaped_segments() -> Result<(), TestFailure> {
    let parsed = ensure_ok(
      Parser::new("root.*[0].1.2").parse_key_only(),
      "a mixed query-key path must construct a syntax tree",
    )?;
    ensure(
      parsed.diagnostics().is_empty(),
      "glob, bracket, numeric, and float-shaped query-key segments must all be accepted",
    )?;
    let identifiers = parsed
      .into_syntax()
      .descendants_with_tokens()
      .filter(|element| element.kind() == IDENT)
      .map(|element| element.to_string())
      .collect::<Vec<_>>();
    ensure(
      identifiers == ["root", "*", "0", "1", "2"],
      "query-only syntax must normalize every semantic segment into ordered identifiers",
    )?;

    let adjacent_periods = ensure_ok(
      Parser::new("root..child").parse_key_only(),
      "an adjacent-period query key must remain a recoverable parse",
    )?;
    ensure_eq(
      &adjacent_periods
        .diagnostics()
        .iter()
        .filter(|diagnostic| diagnostic.message() == r#"unexpected ".""#)
        .count(),
      &1,
      "adjacent periods must produce one exact boundary diagnostic",
    )?;

    let malformed_bracket = ensure_ok(
      Parser::new("root[0.child").parse_key_only(),
      "a malformed bracket query key must remain a recoverable parse",
    )?;
    ensure(
      malformed_bracket
        .diagnostics()
        .iter()
        .any(|diagnostic| diagnostic.message() == r#"expected "]""#),
      "a bracket segment without its closing delimiter must be rejected",
    )?;

    for (source, message) in [
      ("root.", "unexpected end of input"),
      ("root[]", "expected identifier"),
      ("root child", "unexpected identifier"),
      ("+1", "expected identifier"),
      ("+1.2", "expected identifier"),
    ] {
      let rejected = ensure_ok(
        Parser::new(source).parse_key_only(),
        "an invalid query-key boundary must remain a recoverable parse",
      )?;
      ensure(
        rejected.diagnostics().iter().any(|diagnostic| diagnostic.message() == message),
        "each invalid query-key boundary must retain its specific diagnostic",
      )?;
    }

    let zero_padded = ensure_ok(
      Parser::new("01.2").parse_key_only(),
      "a zero-padded float-shaped key must remain a recoverable parse",
    )?;
    ensure(
      zero_padded
        .diagnostics()
        .iter()
        .any(|diagnostic| diagnostic.message() == "zero-padded numbers are not allowed"),
      "zero-padded float-shaped keys must not be normalized into semantic path segments",
    )
  }

  /// Preserve valid header/value classification while diagnosing malformed structural boundaries.
  #[test]
  fn headers_and_lexical_overlaps_preserve_structure_and_diagnostics() -> Result<(), TestFailure> {
    let valid = "date = 1979-05-27\ntime = 07:32:00.5\n[[items]]\nname = \"first\"\n";
    ensure_eq(
      &diagnostic_count(valid, "expected new line")?,
      &0,
      "valid entries and array-table headers must retain their line boundaries",
    )?;
    ensure_eq(
      &syntax_kind_count(valid, DATE)?,
      &1,
      "a local date value must retain its date syntax kind",
    )?;
    ensure_eq(
      &syntax_kind_count(valid, TIME)?,
      &1,
      "a fractional local time value must retain its time syntax kind",
    )?;

    ensure(
      diagnostic_count("value = 1 [table]\n", "expected new line")? > 0,
      "a header following an entry on the same line must be rejected at the line boundary",
    )?;
    ensure_eq(
      &diagnostic_count("[[items]\n", r#"expected "]]"#)?,
      &1,
      "an array-table header must require its second closing bracket",
    )?;

    let invalid_numbers = "decimal = 01\nfloat = 01.5\npositive = +01\nnegative = -01\nhex = 0x_1\nbinary = 0b1__0\n";
    ensure_eq(
      &diagnostic_count(invalid_numbers, "zero-padded integers are not allowed")?,
      &3,
      "unsigned and signed zero-padded integers must share the integer-specific diagnostic",
    )?;
    ensure_eq(
      &diagnostic_count(invalid_numbers, "zero-padded numbers are not allowed")?,
      &1,
      "a zero-padded float must retain its numeric diagnostic",
    )?;
    ensure_eq(
      &diagnostic_count(invalid_numbers, "invalid underscores")?,
      &2,
      "radix values must reject leading and adjacent underscores independently",
    )?;
    ensure_eq(
      &diagnostic_count("positive = +0\nnegative = -0\n", "zero-padded integers are not allowed")?,
      &0,
      "the exact signed zero values must remain valid rather than being mistaken for padding",
    )
  }

  /// Parse, preserve, and freeze one deeply nested valid or invalid composite.
  fn ensure_deep_composite(source: &str, valid: bool, expected_node: impl FnOnce(&Node) -> bool) -> Result<(), TestFailure> {
    let parsed = ensure_ok(parse(source), "a deeply nested composite must construct a recoverable tree")?;
    ensure(
      parsed.diagnostics().is_empty() == valid,
      "deep-composite diagnostics must distinguish valid and unterminated input",
    )?;
    let rendered = parsed.green().to_string();
    ensure_eq(
      &rendered.as_str(),
      &source,
      "iterative deep-composite parsing and recovery must remain lossless",
    )?;
    let dom = parsed.into_dom();
    ensure(
      dom.get_key("value").is_some_and(|value| expected_node(&value)),
      "deep-composite parsing must freeze into the expected tolerant DOM kind",
    )
  }

  /// Parse and freeze a deeply nested valid array through heap-owned frames.
  #[test]
  fn deeply_nested_arrays_use_heap_frames() -> Result<(), TestFailure> {
    let depth = 10_000;
    let source = format!("value = {}0{}\n", "[".repeat(depth), "]".repeat(depth));
    ensure_deep_composite(&source, true, Node::is_array)
  }

  /// Terminate and freeze a deeply nested unterminated array with ordered diagnostics.
  #[test]
  fn deeply_nested_invalid_arrays_terminate_with_diagnostics() -> Result<(), TestFailure> {
    let depth = 10_000;
    let source = format!("value = {}0\n", "[".repeat(depth));
    ensure_deep_composite(&source, false, Node::is_array)
  }

  /// Parse and freeze deeply nested inline tables without call-stack recursion.
  #[test]
  fn deeply_nested_inline_tables_use_heap_frames_and_freeze() -> Result<(), TestFailure> {
    let depth = 10_000;
    let source = format!("value = {}0{}\n", "{ nested = ".repeat(depth), " }".repeat(depth));
    ensure_deep_composite(&source, true, Node::is_table)
  }

  /// Recover and freeze deeply nested unterminated inline tables without stalling.
  #[test]
  fn deeply_nested_invalid_inline_tables_terminate_and_freeze() -> Result<(), TestFailure> {
    let depth = 10_000;
    let source = format!("value = {}0\n", "{ nested = ".repeat(depth));
    ensure_deep_composite(&source, false, Node::is_table)
  }
}
