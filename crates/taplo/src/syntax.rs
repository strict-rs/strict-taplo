//! Syntax kinds, Rowan language integration, and the private Logos lexer.

use core::fmt;
use core::fmt::Debug;
use core::fmt::Formatter;
use core::ops::Range;

use logos::Logos;

/// A lossless Rowan syntax kind.
///
/// Known TOML token and node kinds are exposed as documented associated
/// constants. Unknown raw values remain representable so Rowan conversion is
/// total and never relies on assertions, casts, transmutes, or unsafe code.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SyntaxKind(
  /// Exact raw Rowan kind value, including values unknown to this Taplo version.
  u16,
);

/// Define stable associated constants and matching importable constants from one raw-kind table.
macro_rules! define_syntax_kinds {
  ($(($name:ident, $raw:literal, $documentation:literal)),+ $(,)?) => {
    impl SyntaxKind {
      $(
        #[doc = $documentation]
        pub const $name: Self = Self($raw);
      )+

      /// Return the stable name of a known kind.
      const fn name(self) -> Option<&'static str> {
        match self {
          $(
            Self::$name => Some(stringify!($name)),
          )+
          _ => None,
        }
      }
    }

    /// Named syntax-kind constants for imports and pattern matching.
    pub mod kind {
      use super::SyntaxKind;

      $(
        #[doc = $documentation]
        pub const $name: SyntaxKind = SyntaxKind::$name;
      )+
    }
  };
}

define_syntax_kinds!(
  (WHITESPACE, 0, "Spaces or horizontal tabs."),
  (NEWLINE, 1, "One or more LF or CRLF line endings."),
  (COMMENT, 2, "A comment extending to the end of its source line."),
  (IDENT, 3, "A bare or normalized TOML key identifier."),
  (IDENT_WITH_GLOB, 4, "A key identifier containing glob syntax used by query parsing."),
  (PERIOD, 5, "A period separating dotted-key components."),
  (COMMA, 6, "A comma separating array values or inline-table entries."),
  (EQ, 7, "An equals sign separating a key and value."),
  (STRING, 8, "A basic string."),
  (MULTI_LINE_STRING, 9, "A multiline basic string."),
  (STRING_LITERAL, 10, "A literal string."),
  (MULTI_LINE_STRING_LITERAL, 11, "A multiline literal string."),
  (INTEGER, 12, "A decimal integer."),
  (INTEGER_HEX, 13, "A hexadecimal integer."),
  (INTEGER_OCT, 14, "An octal integer."),
  (INTEGER_BIN, 15, "A binary integer."),
  (FLOAT, 16, "A floating-point value."),
  (BOOL, 17, "A Boolean value."),
  (DATE_TIME_OFFSET, 18, "An offset date-time value."),
  (DATE_TIME_LOCAL, 19, "A local date-time value."),
  (DATE, 20, "A local date value."),
  (TIME, 21, "A local time value."),
  (BRACKET_START, 22, "An opening square bracket."),
  (BRACKET_END, 23, "A closing square bracket."),
  (BRACE_START, 24, "An opening brace."),
  (BRACE_END, 25, "A closing brace."),
  (ERROR, 26, "Unrecognized or syntactically invalid source text."),
  (KEY, 27, "A composite dotted key node."),
  (VALUE, 28, "A composite value node."),
  (TABLE_HEADER, 29, "A regular table header node."),
  (TABLE_ARRAY_HEADER, 30, "An array-of-tables header node."),
  (ENTRY, 31, "A key-value entry node."),
  (ARRAY, 32, "An array node."),
  (INLINE_TABLE, 33, "An inline-table node."),
  (ROOT, 34, "The document root node."),
);

impl SyntaxKind {
  /// Construct a syntax kind from any raw Rowan value.
  #[allow(
    clippy::single_call_fn,
    reason = "the public constructor is the lossless raw-kind boundary used by Rowan's Language conversion"
  )]
  #[must_use]
  pub const fn from_raw(raw: u16) -> Self {
    Self(raw)
  }

  /// Return this kind's exact raw Rowan value.
  #[allow(
    clippy::single_call_fn,
    reason = "the public raw-kind extractor completes the lossless Rowan interoperability contract for external consumers"
  )]
  #[must_use]
  pub const fn raw(self) -> u16 {
    self.0
  }
}

impl Debug for SyntaxKind {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match self.name() {
      Some(name) => formatter.write_str(name),
      None => formatter.debug_tuple("SyntaxKind").field(&self.raw()).finish(),
    }
  }
}

impl From<SyntaxKind> for rowan::SyntaxKind {
  fn from(kind: SyntaxKind) -> Self {
    Self(kind.raw())
  }
}

/// Taplo's Rowan language marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Lang {}

impl rowan::Language for Lang {
  type Kind = SyntaxKind;

  fn kind_from_raw(raw: rowan::SyntaxKind) -> Self::Kind {
    SyntaxKind::from_raw(raw.0)
  }

  fn kind_to_raw(kind: Self::Kind) -> rowan::SyntaxKind {
    kind.into()
  }
}

/// A typed Rowan syntax node.
pub type SyntaxNode = rowan::SyntaxNode<Lang>;
/// A typed Rowan syntax token.
pub type SyntaxToken = rowan::SyntaxToken<Lang>;
/// A typed Rowan syntax node or token.
pub type SyntaxElement = rowan::NodeOrToken<SyntaxNode, SyntaxToken>;

/// Private Logos token vocabulary, intentionally separate from [`SyntaxKind`].
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
enum LexToken {
  /// Spaces or horizontal tabs.
  #[regex(r"([ \t])+")]
  Whitespace,
  /// One or more line endings.
  #[regex(r"(\n|\r\n)+")]
  Newline,
  /// A comment through the end of its source line.
  #[regex(r"#[^\n\r]*", allow_greedy = true)]
  Comment,
  /// A bare identifier.
  #[regex(r"[A-Za-z0-9_-]+", priority = 2)]
  Ident,
  /// A query-only identifier containing glob syntax.
  #[regex(r"[*?A-Za-z0-9_-]+", priority = 1)]
  IdentWithGlob,
  /// A period.
  #[token(".")]
  Period,
  /// A comma.
  #[token(",")]
  Comma,
  /// An equals sign.
  #[token("=")]
  Eq,
  /// A basic string.
  #[regex(r#"""#, lex_string)]
  String,
  /// A multiline basic string.
  #[regex(r#"""""#, lex_multi_line_string)]
  MultiLineString,
  /// A literal string.
  #[regex(r#"'"#, lex_string_literal)]
  StringLiteral,
  /// A multiline literal string.
  #[regex(r#"'''"#, lex_multi_line_string_literal)]
  MultiLineStringLiteral,
  /// A decimal integer.
  #[regex(r"[+-]?[0-9_]+", priority = 4)]
  Integer,
  /// A hexadecimal integer.
  #[regex(r"0x[0-9A-Fa-f_]+")]
  IntegerHex,
  /// An octal integer.
  #[regex(r"0o[0-7_]+")]
  IntegerOct,
  /// A binary integer.
  #[regex(r"0b(0|1|_)+")]
  IntegerBin,
  /// A floating-point number.
  #[regex(r"[-+]?([0-9_]+(\.[0-9_]+)?([eE][+-]?[0-9_]+)?|nan|inf)", priority = 3)]
  Float,
  /// A Boolean.
  #[regex(r"true|false")]
  Bool,
  /// An offset date-time.
  #[regex(r#"(?:[1-9]\d\d\d-(?:(?:0[1-9]|1[0-2])-(?:0[1-9]|1\d|2[0-8])|(?:0[13-9]|1[0-2])-(?:29|30)|(?:0[13578]|1[02])-31)|(?:[1-9]\d(?:0[48]|[2468][048]|[13579][26])|(?:[2468][048]|[13579][26])00)-02-29)(?:T|t| )(?:[01]\d|2[0-3]):[0-5]\d:[0-5]\d(?:(?:\.|,)\d+)?(?:[Zz]|[+-][01]\d:[0-5]\d)"#)]
  DateTimeOffset,
  /// A local date-time.
  #[regex(r#"(?:[1-9]\d\d\d-(?:(?:0[1-9]|1[0-2])-(?:0[1-9]|1\d|2[0-8])|(?:0[13-9]|1[0-2])-(?:29|30)|(?:0[13578]|1[02])-31)|(?:[1-9]\d(?:0[48]|[2468][048]|[13579][26])|(?:[2468][048]|[13579][26])00)-02-29)(?:T|t| )(?:[01]\d|2[0-3]):[0-5]\d:[0-5]\d(?:(?:\.|,)\d+)?"#)]
  DateTimeLocal,
  /// A local date.
  #[regex(r#"(?:[1-9]\d\d\d-(?:(?:0[1-9]|1[0-2])-(?:0[1-9]|1\d|2[0-8])|(?:0[13-9]|1[0-2])-(?:29|30)|(?:0[13578]|1[02])-31)|(?:[1-9]\d(?:0[48]|[2468][048]|[13579][26])|(?:[2468][048]|[13579][26])00)-02-29)"#)]
  Date,
  /// A local time.
  #[regex(r#"(?:[01]\d|2[0-3]):[0-5]\d:[0-5]\d(?:(?:\.|,)\d+)?"#)]
  Time,
  /// An opening square bracket.
  #[token("[")]
  BracketStart,
  /// A closing square bracket.
  #[token("]")]
  BracketEnd,
  /// An opening brace.
  #[token("{")]
  BraceStart,
  /// A closing brace.
  #[token("}")]
  BraceEnd,
}

impl LexToken {
  /// Map every private lexer token to its public lossless syntax kind.
  const fn syntax_kind(self) -> SyntaxKind {
    match self {
      Self::Whitespace => SyntaxKind::WHITESPACE,
      Self::Newline => SyntaxKind::NEWLINE,
      Self::Comment => SyntaxKind::COMMENT,
      Self::Ident => SyntaxKind::IDENT,
      Self::IdentWithGlob => SyntaxKind::IDENT_WITH_GLOB,
      Self::Period => SyntaxKind::PERIOD,
      Self::Comma => SyntaxKind::COMMA,
      Self::Eq => SyntaxKind::EQ,
      Self::String => SyntaxKind::STRING,
      Self::MultiLineString => SyntaxKind::MULTI_LINE_STRING,
      Self::StringLiteral => SyntaxKind::STRING_LITERAL,
      Self::MultiLineStringLiteral => SyntaxKind::MULTI_LINE_STRING_LITERAL,
      Self::Integer => SyntaxKind::INTEGER,
      Self::IntegerHex => SyntaxKind::INTEGER_HEX,
      Self::IntegerOct => SyntaxKind::INTEGER_OCT,
      Self::IntegerBin => SyntaxKind::INTEGER_BIN,
      Self::Float => SyntaxKind::FLOAT,
      Self::Bool => SyntaxKind::BOOL,
      Self::DateTimeOffset => SyntaxKind::DATE_TIME_OFFSET,
      Self::DateTimeLocal => SyntaxKind::DATE_TIME_LOCAL,
      Self::Date => SyntaxKind::DATE,
      Self::Time => SyntaxKind::TIME,
      Self::BracketStart => SyntaxKind::BRACKET_START,
      Self::BracketEnd => SyntaxKind::BRACKET_END,
      Self::BraceStart => SyntaxKind::BRACE_START,
      Self::BraceEnd => SyntaxKind::BRACE_END,
    }
  }
}

/// Crate-private wrapper that keeps [`LexToken`] out of public APIs.
pub(crate) struct SyntaxLexer<'source> {
  /// Logos lexer over the private token vocabulary.
  inner: logos::Lexer<'source, LexToken>,
}

impl<'source> SyntaxLexer<'source> {
  /// Create a lexer for one source string.
  #[allow(
    clippy::single_call_fn,
    reason = "the constructor keeps the private Logos vocabulary behind the parser-facing lexer wrapper"
  )]
  pub(crate) fn new(source: &'source str) -> Self {
    Self {
      inner: LexToken::lexer(source),
    }
  }

  /// Advance to the next token, mapping it explicitly into [`SyntaxKind`].
  pub(crate) fn next(&mut self) -> Option<SyntaxKind> {
    self.inner.next().map(|result| match result {
      Ok(token) => token.syntax_kind(),
      Err(()) => SyntaxKind::ERROR,
    })
  }

  /// Borrow the current token's source text.
  pub(crate) fn slice(&self) -> &'source str {
    self.inner.slice()
  }

  /// Return the current token's byte range.
  pub(crate) fn span(&self) -> Range<usize> {
    self.inner.span()
  }

  /// Borrow unlexed source after the current token.
  pub(crate) fn remainder(&self) -> &'source str {
    self.inner.remainder()
  }
}

/// Advance `total_len` by one UTF-8 character length without overflow.
const fn advance_len(total_len: &mut usize, character: char) -> bool {
  let Some(next) = total_len.checked_add(character.len_utf8()) else {
    return false;
  };
  *total_len = next;
  true
}

/// Advance `quote_count` by one without overflow.
const fn advance_quote_count(quote_count: &mut usize) -> bool {
  let Some(next) = quote_count.checked_add(1) else {
    return false;
  };
  *quote_count = next;
  true
}

/// Scan a basic string through its unescaped terminator.
#[allow(
  clippy::single_call_fn,
  reason = "the named Logos callback owns byte-checked escape parity and basic-string termination"
)]
fn lex_string(lexer: &mut logos::Lexer<'_, LexToken>) -> bool {
  let mut escaped = false;
  let mut total_len = 0;

  for character in lexer.remainder().chars() {
    if !advance_len(&mut total_len, character) {
      return false;
    }

    if character == '\\' {
      escaped = !escaped;
      continue;
    }

    if character == '"' && !escaped {
      lexer.bump(total_len);
      return true;
    }

    escaped = false;
  }

  false
}

/// Scan a multiline basic string through its complete quote run.
#[allow(
  clippy::single_call_fn,
  reason = "the named Logos callback owns multiline basic-string escape parity and legal closing quote runs"
)]
fn lex_multi_line_string(lexer: &mut logos::Lexer<'_, LexToken>) -> bool {
  let mut total_len = 0;
  let mut quote_count = 0;
  let mut escaped = false;
  let mut quotes_found = false;

  for character in lexer.remainder().chars() {
    if quotes_found && character != '"' {
      if quote_count >= 6 {
        return false;
      }
      lexer.bump(total_len);
      return true;
    }
    if quotes_found {
      if !advance_quote_count(&mut quote_count) || !advance_len(&mut total_len, character) {
        return false;
      }
      continue;
    }

    if !advance_len(&mut total_len, character) {
      return false;
    }

    if character == '\\' {
      quote_count = 0;
      escaped = !escaped;
      continue;
    }

    if character == '"' && !escaped {
      if !advance_quote_count(&mut quote_count) {
        return false;
      }
    } else {
      quote_count = 0;
    }

    if quote_count == 3 {
      quotes_found = true;
    }

    escaped = false;
  }

  if !quotes_found || quote_count >= 6 {
    return false;
  }

  lexer.bump(total_len);
  true
}

/// Scan a literal string through its closing quote.
#[allow(
  clippy::single_call_fn,
  reason = "the named Logos callback owns UTF-8-safe literal-string termination without escape semantics"
)]
fn lex_string_literal(lexer: &mut logos::Lexer<'_, LexToken>) -> bool {
  let mut total_len = 0;

  for character in lexer.remainder().chars() {
    if !advance_len(&mut total_len, character) {
      return false;
    }

    if character == '\'' {
      lexer.bump(total_len);
      return true;
    }
  }

  false
}

/// Scan a multiline literal string through its complete quote run.
#[allow(
  clippy::single_call_fn,
  reason = "the named Logos callback owns multiline literal-string content quotes and terminator bounds"
)]
fn lex_multi_line_string_literal(lexer: &mut logos::Lexer<'_, LexToken>) -> bool {
  let mut total_len = 0;
  let mut quote_count = 0;
  let mut quotes_found = false;

  for character in lexer.remainder().chars() {
    if quotes_found {
      if character != '\'' {
        lexer.bump(total_len);
        return true;
      }
      if quote_count > 4 || !advance_quote_count(&mut quote_count) || !advance_len(&mut total_len, character) {
        return false;
      }
      continue;
    }

    if !advance_len(&mut total_len, character) {
      return false;
    }

    if character == '\'' {
      if !advance_quote_count(&mut quote_count) {
        return false;
      }
    } else {
      quote_count = 0;
    }

    if quote_count == 3 {
      quotes_found = true;
    }
  }

  if !quotes_found {
    return false;
  }

  lexer.bump(total_len);
  true
}

#[cfg(test)]
/// Raw-kind stability and private lexer behavior contracts.
mod tests {
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;

  use super::SyntaxKind;
  use super::SyntaxLexer;

  /// Collect the public kinds produced by the private lexer.
  fn kinds(source: &str) -> Vec<SyntaxKind> {
    let mut lexer = SyntaxLexer::new(source);
    let mut kinds = Vec::new();
    while let Some(kind) = lexer.next() {
      kinds.push(kind);
    }
    kinds
  }

  /// Require one isolated source fragment to produce exactly one kind.
  fn ensure_single_kind(source: &str, expected: SyntaxKind, message: &'static str) -> Result<(), TestFailure> {
    ensure(kinds(source) == vec![expected], message)
  }

  /// Preserve every known raw assignment and round-trip unknown Rowan kinds without loss.
  #[test]
  fn raw_kind_conversion_is_total_and_stable() -> Result<(), TestFailure> {
    let known = [
      SyntaxKind::WHITESPACE,
      SyntaxKind::NEWLINE,
      SyntaxKind::COMMENT,
      SyntaxKind::IDENT,
      SyntaxKind::IDENT_WITH_GLOB,
      SyntaxKind::PERIOD,
      SyntaxKind::COMMA,
      SyntaxKind::EQ,
      SyntaxKind::STRING,
      SyntaxKind::MULTI_LINE_STRING,
      SyntaxKind::STRING_LITERAL,
      SyntaxKind::MULTI_LINE_STRING_LITERAL,
      SyntaxKind::INTEGER,
      SyntaxKind::INTEGER_HEX,
      SyntaxKind::INTEGER_OCT,
      SyntaxKind::INTEGER_BIN,
      SyntaxKind::FLOAT,
      SyntaxKind::BOOL,
      SyntaxKind::DATE_TIME_OFFSET,
      SyntaxKind::DATE_TIME_LOCAL,
      SyntaxKind::DATE,
      SyntaxKind::TIME,
      SyntaxKind::BRACKET_START,
      SyntaxKind::BRACKET_END,
      SyntaxKind::BRACE_START,
      SyntaxKind::BRACE_END,
      SyntaxKind::ERROR,
      SyntaxKind::KEY,
      SyntaxKind::VALUE,
      SyntaxKind::TABLE_HEADER,
      SyntaxKind::TABLE_ARRAY_HEADER,
      SyntaxKind::ENTRY,
      SyntaxKind::ARRAY,
      SyntaxKind::INLINE_TABLE,
      SyntaxKind::ROOT,
    ];
    ensure(
      known.map(SyntaxKind::raw)
        == [
          0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34,
        ],
      "every known syntax kind must retain its historical raw assignment",
    )?;

    let unknown = SyntaxKind::from_raw(u16::MAX);
    ensure(unknown.raw() == u16::MAX, "unknown raw syntax kinds must round-trip unchanged")?;
    ensure(
      format!("{unknown:?}").contains("65535"),
      "unknown syntax-kind diagnostics must retain the raw value",
    )
  }

  /// Map every lexical token class explicitly into its public syntax kind.
  #[test]
  fn private_lexer_maps_every_token_class() -> Result<(), TestFailure> {
    ensure_single_kind(" \t", SyntaxKind::WHITESPACE, "horizontal whitespace must remain one token")?;
    ensure_single_kind("\r\n\n", SyntaxKind::NEWLINE, "CRLF and LF runs must remain newline tokens")?;
    ensure_single_kind("# comment", SyntaxKind::COMMENT, "comments must extend through line content")?;
    ensure_single_kind("bare-key", SyntaxKind::IDENT, "bare keys must remain identifiers")?;
    ensure_single_kind(
      "*?key",
      SyntaxKind::IDENT_WITH_GLOB,
      "glob key segments must retain their query kind",
    )?;
    ensure_single_kind(".", SyntaxKind::PERIOD, "period punctuation must retain its kind")?;
    ensure_single_kind(",", SyntaxKind::COMMA, "comma punctuation must retain its kind")?;
    ensure_single_kind("=", SyntaxKind::EQ, "equals punctuation must retain its kind")?;
    ensure_single_kind("\"value\"", SyntaxKind::STRING, "basic strings must retain their kind")?;
    ensure_single_kind(
      "\"\"\"value\"\"\"",
      SyntaxKind::MULTI_LINE_STRING,
      "multiline basic strings must retain their kind",
    )?;
    ensure_single_kind("'value'", SyntaxKind::STRING_LITERAL, "literal strings must retain their kind")?;
    ensure_single_kind(
      "'''value'''",
      SyntaxKind::MULTI_LINE_STRING_LITERAL,
      "multiline literal strings must retain their kind",
    )?;
    ensure_single_kind("+12_345", SyntaxKind::INTEGER, "decimal integers must retain their kind")?;
    ensure_single_kind("0xCA_FE", SyntaxKind::INTEGER_HEX, "hexadecimal integers must retain their kind")?;
    ensure_single_kind("0o7_55", SyntaxKind::INTEGER_OCT, "octal integers must retain their kind")?;
    ensure_single_kind("0b10_01", SyntaxKind::INTEGER_BIN, "binary integers must retain their kind")?;
    ensure_single_kind("-1.25e+3", SyntaxKind::FLOAT, "finite floats must retain their kind")?;
    ensure_single_kind("inf", SyntaxKind::FLOAT, "special floats must retain their kind")?;
    ensure_single_kind("false", SyntaxKind::BOOL, "Booleans must retain their kind")?;
    ensure_single_kind(
      "1979-05-27T07:32:00Z",
      SyntaxKind::DATE_TIME_OFFSET,
      "offset date-times must retain their kind",
    )?;
    ensure_single_kind(
      "1979-05-27 07:32:00",
      SyntaxKind::DATE_TIME_LOCAL,
      "local date-times must retain their kind",
    )?;
    ensure_single_kind("1979-05-27", SyntaxKind::DATE, "local dates must retain their kind")?;
    ensure_single_kind("07:32:00.5", SyntaxKind::TIME, "local times must retain their kind")?;
    ensure_single_kind("[", SyntaxKind::BRACKET_START, "opening brackets must retain their kind")?;
    ensure_single_kind("]", SyntaxKind::BRACKET_END, "closing brackets must retain their kind")?;
    ensure_single_kind("{", SyntaxKind::BRACE_START, "opening braces must retain their kind")?;
    ensure_single_kind("}", SyntaxKind::BRACE_END, "closing braces must retain their kind")?;

    ensure(
      kinds("key = [true, \"\u{503c}\"] # comment\r\n")
        == [
          SyntaxKind::IDENT,
          SyntaxKind::WHITESPACE,
          SyntaxKind::EQ,
          SyntaxKind::WHITESPACE,
          SyntaxKind::BRACKET_START,
          SyntaxKind::BOOL,
          SyntaxKind::COMMA,
          SyntaxKind::WHITESPACE,
          SyntaxKind::STRING,
          SyntaxKind::BRACKET_END,
          SyntaxKind::WHITESPACE,
          SyntaxKind::COMMENT,
          SyntaxKind::NEWLINE,
        ],
      "mixed Unicode TOML must preserve every token boundary and CRLF ending",
    )
  }

  /// Distinguish escaped quotes from valid multiline basic-string terminators.
  #[test]
  fn multiline_basic_strings_respect_escape_parity() -> Result<(), TestFailure> {
    ensure_single_kind(
      "\"\"\"value\\\\\"\"\"",
      SyntaxKind::MULTI_LINE_STRING,
      "an even backslash run must not escape the multiline terminator",
    )?;
    ensure(
      kinds("\"\"\"value\\\"\"\"").contains(&SyntaxKind::ERROR),
      "an odd backslash run must escape one quote and leave the source unterminated",
    )?;
    ensure_single_kind(
      "\"\"\"prefix\"\"\\\\\"suffix\"\"\"",
      SyntaxKind::MULTI_LINE_STRING,
      "backslashes must break a preceding quote run before a later terminator",
    )
  }

  /// Accept legal content-quote runs and reject six-quote multiline terminators.
  #[test]
  fn multiline_quote_runs_accept_only_toml_terminators() -> Result<(), TestFailure> {
    ensure_single_kind(
      "\"\"\"value\"\"\"\"\"",
      SyntaxKind::MULTI_LINE_STRING,
      "multiline basic strings may end with two content quotes before the terminator",
    )?;
    ensure_single_kind(
      "'''value'''''",
      SyntaxKind::MULTI_LINE_STRING_LITERAL,
      "multiline literal strings may end with two content quotes before the terminator",
    )?;
    ensure(
      kinds("\"\"\"value\"\"\"\"\"\"").contains(&SyntaxKind::ERROR),
      "a six-quote multiline basic closing run must be rejected",
    )?;
    ensure(
      kinds("\"\"\"value\"\"\"\"\"\"tail").contains(&SyntaxKind::ERROR),
      "content after a six-quote multiline basic run must not turn the invalid run into a terminator",
    )?;
    ensure(
      kinds("'''value''''''").contains(&SyntaxKind::ERROR),
      "a six-quote multiline literal closing run must be rejected",
    )
  }

  /// Produce error tokens for unterminated strings and unrecognized source bytes.
  #[test]
  fn lexer_reports_unterminated_and_unrecognized_input() -> Result<(), TestFailure> {
    let unterminated = kinds("value = \"unterminated\\");
    ensure(
      unterminated.contains(&SyntaxKind::ERROR),
      "an unterminated escaped string must retain an error token",
    )?;
    ensure(
      kinds("value = 'unterminated").contains(&SyntaxKind::ERROR),
      "an unterminated literal string must retain an error token",
    )?;
    ensure(
      kinds("value = \"\"\"unterminated").contains(&SyntaxKind::ERROR),
      "an unterminated multiline basic string must retain an error token",
    )?;
    ensure(
      kinds("value = '''unterminated").contains(&SyntaxKind::ERROR),
      "an unterminated multiline literal string must retain an error token",
    )?;
    ensure(
      kinds("value = @")
        == [
          SyntaxKind::IDENT,
          SyntaxKind::WHITESPACE,
          SyntaxKind::EQ,
          SyntaxKind::WHITESPACE,
          SyntaxKind::ERROR,
        ],
      "an unrecognized byte must map to the public error kind",
    )
  }
}
