//! TOML document to syntax tree parsing.

use logos::Lexer;
use logos::Logos;
use rowan::GreenNode;
use rowan::GreenNodeBuilder;
use rowan::TextRange;
use rowan::TextSize;

use crate::dom::FromSyntax;
use crate::dom::{
  self,
};
use crate::syntax::SyntaxKind;
use crate::syntax::SyntaxKind::*;
use crate::syntax::SyntaxNode;
use crate::util::allowed_chars;
use crate::util::check_escape;

#[macro_use]
mod macros;

/// A syntax error that can occur during parsing.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct Error {
  /// The span of the error.
  pub range: TextRange,

  /// Human-friendly error message.
  pub message: String,
}

impl core::fmt::Display for Error {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{} ({:?})", self.message, self.range)
  }
}
impl std::error::Error for Error {}

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
pub fn parse(source: &str) -> Parse {
  Parser::new(source).parse()
}

/// A hand-written parser that uses the Logos lexer
/// to tokenize the source, then constructs
/// a Rowan green tree from them.
pub(crate) struct Parser<'p> {
  skip_whitespace:    bool,
  // Allow glob patterns as keys and using [] instead of dots.
  key_pattern_syntax: bool,
  current_token:      Option<SyntaxKind>,

  /// Tokens that delimit the active recovery scopes and must not be consumed by a first error.
  recovery_tokens: Vec<SyntaxKind>,

  lexer:   Lexer<'p, SyntaxKind>,
  builder: GreenNodeBuilder<'p>,
  errors:  Vec<Error>,
}

impl Parser<'_> {
  /// Required for patch syntax
  /// and key matches.
  ///
  /// It allows a part of glob syntax in identifiers as well.
  pub(crate) fn parse_key_only(mut self) -> Parse {
    self.key_pattern_syntax = true;
    let _ = with_node!(self.builder, KEY, self.parse_key());

    Parse {
      green_node: self.builder.finish(),
      errors:     self.errors,
    }
  }
}

/// This is just a convenience type during parsing.
/// It allows using "?", making the code cleaner.
type ParserResult<T> = Result<T, ()>;

// FIXME(recursion)
// Deeply nested structures cause stack overflow,
// this probably has to be rewritten into a state machine
// that contains minimal function calls.
impl<'p> Parser<'p> {
  pub(crate) fn new(source: &'p str) -> Self {
    Parser {
      current_token:      None,
      skip_whitespace:    true,
      key_pattern_syntax: false,
      recovery_tokens:    Vec::new(),
      lexer:              SyntaxKind::lexer(source),
      builder:            Default::default(),
      errors:             Default::default(),
    }
  }

  fn parse(mut self) -> Parse {
    let _ = with_node!(self.builder, ROOT, self.parse_root());

    Parse {
      green_node: self.builder.finish(),
      errors:     self.errors,
    }
  }

  fn error(&mut self, message: &str) -> ParserResult<()> {
    let err = Error {
      range:   self.current_range(),
      message: message.into(),
    };

    let same_error = self.errors.last().map(|e| e.range == err.range).unwrap_or(false);

    if !same_error {
      self.add_error(&err);
      if self.current_token.is_some_and(|token| !self.is_recovery_token(token)) {
        let _ = self.token_as(ERROR);
      }
    } else {
      let _ = self.token_as(ERROR);
    }

    Err(())
  }

  // report error without consuming the current the token
  fn report_error(&mut self, message: &str) -> ParserResult<()> {
    self.add_error(&Error {
      range:   self.current_range(),
      message: message.into(),
    });
    Err(())
  }

  fn add_error(&mut self, e: &Error) {
    if self.errors.last().is_some_and(|last_error| last_error == e) {
      return;
    }

    self.errors.push(e.clone());
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
  fn range(span: std::ops::Range<usize>) -> TextRange {
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
    self.add_error(&Error {
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

  fn insert_token(&mut self, kind: SyntaxKind, s: &str) {
    self.builder.token(kind.into(), s)
  }

  fn must_token_or(&mut self, kind: SyntaxKind, message: &str) -> ParserResult<()> {
    match self.get_token() {
      Ok(t) => {
        if kind == t {
          self.token()
        } else {
          self.error(message)
        }
      }
      Err(_) => {
        self.add_error(&Error {
          range:   self.current_range(),
          message: "unexpected EOF".into(),
        });
        Err(())
      }
    }
  }

  // This is the same as `token` but won't consume trailing whitespace.
  fn add_token(&mut self) -> ParserResult<()> {
    match self.get_token() {
      Err(_) => Err(()),
      Ok(token) => {
        self.builder.token(token.into(), self.lexer.slice());
        self.current_token = None;
        Ok(())
      }
    }
  }

  fn token(&mut self) -> ParserResult<()> {
    match self.get_token() {
      Err(_) => Err(()),
      Ok(token) => self.token_as(token),
    }
  }

  /// This function implicitly calls `step`,
  /// it was definitely not a good design decision
  /// but changing this behaviour involves a
  /// different syntax tree and breakages down the line.
  fn token_as(&mut self, kind: SyntaxKind) -> ParserResult<()> {
    self.token_as_no_step(kind)?;
    self.step();
    Ok(())
  }

  fn token_as_no_step(&mut self, kind: SyntaxKind) -> ParserResult<()> {
    match self.get_token() {
      Err(_) => return Err(()),
      Ok(_) => {
        self.builder.token(kind.into(), self.lexer.slice());
      }
    }

    Ok(())
  }

  fn step(&mut self) {
    self.current_token = None;
    while let Some(token) = self.lexer.next() {
      // logos 0.16 yields `Err(())` for unrecognized input; map it to the ERROR
      // node kind so error-tolerant parsing still produces a tree plus a syntax error.
      let token = token.unwrap_or(ERROR);
      match token {
        COMMENT => {
          match allowed_chars::comment(self.lexer.slice()) {
            Ok(_) => {}
            Err(err_indices) => {
              for e in err_indices {
                self.add_relative_error(e, "invalid character in comment");
              }
            }
          };

          self.insert_token(token, self.lexer.slice());
        }
        WHITESPACE => {
          if self.skip_whitespace {
            self.insert_token(token, self.lexer.slice());
          } else {
            self.current_token = Some(token);
            break;
          }
        }
        ERROR => {
          self.insert_token(token, self.lexer.slice());
          self.add_error(&Error {
            range:   self.current_range(),
            message: "unexpected token".into(),
          })
        }
        _ => {
          self.current_token = Some(token);
          break;
        }
      }
    }
  }

  fn get_token(&mut self) -> ParserResult<SyntaxKind> {
    if self.current_token.is_none() {
      self.step();
    }

    self.current_token.ok_or(())
  }

  fn parse_root(&mut self) -> ParserResult<()> {
    // Ensure we have newlines between entries
    let mut not_newline = false;

    // We want to make sure that an entry spans the
    // entire line, so we start/close its node manually.
    let mut entry_started = false;

    while let Ok(token) = self.get_token() {
      match token {
        BRACKET_START => {
          if entry_started {
            self.builder.finish_node();
            entry_started = false;
          }

          if not_newline {
            let _ = self.error("expected new line");
            continue;
          }

          not_newline = true;

          if self.lexer.remainder().starts_with('[') {
            let _ = self.with_recovery_tokens(&[NEWLINE], |parser| {
              with_node!(parser.builder, TABLE_ARRAY_HEADER, parser.parse_table_array_header())
            });
          } else {
            let _ = self.with_recovery_tokens(&[NEWLINE], |parser| {
              with_node!(parser.builder, TABLE_HEADER, parser.parse_table_header())
            });
          }
        }
        NEWLINE => {
          not_newline = false;
          if entry_started {
            self.builder.finish_node();
            entry_started = false;
          }
          let _ = self.token();
        }
        _ => {
          if not_newline {
            let _ = self.error("expected new line");
            continue;
          }
          if entry_started {
            self.builder.finish_node();
          }
          not_newline = true;
          self.builder.start_node(ENTRY.into());
          entry_started = true;
          let _ = self.with_recovery_tokens(&[NEWLINE], Self::parse_entry);
        }
      }
    }
    if entry_started {
      self.builder.finish_node();
    }

    Ok(())
  }

  fn parse_table_header(&mut self) -> ParserResult<()> {
    self.must_token_or(BRACKET_START, r#"expected "[""#)?;
    let _ = with_node!(self.builder, KEY, self.parse_key());
    self.must_token_or(BRACKET_END, r#"expected "]""#)?;

    Ok(())
  }

  fn parse_table_array_header(&mut self) -> ParserResult<()> {
    self.skip_whitespace = false;
    self.must_token_or(BRACKET_START, r#"expected "[[""#)?;
    self.must_token_or(BRACKET_START, r#"expected "[[""#)?;
    self.skip_whitespace = true;
    let _ = with_node!(self.builder, KEY, self.parse_key());
    self.skip_whitespace = false;
    let _ = self.must_token_or(BRACKET_END, r#"expected "]]""#);

    // Hack in order to avoid calling `step` after
    // the second closing bracket.
    let token = self.get_token()?;
    match token {
      BRACKET_END => {
        self.token_as_no_step(token)?;
      }
      _ => {
        self.error(r#"expected "]]"#)?;
      }
    }
    self.skip_whitespace = true;

    self.step();

    Ok(())
  }

  fn parse_entry(&mut self) -> ParserResult<()> {
    with_node!(self.builder, KEY, self.parse_key())?;
    self.must_token_or(EQ, r#"expected "=""#)?;
    with_node!(self.builder, VALUE, self.parse_value())?;

    Ok(())
  }

  fn parse_key(&mut self) -> ParserResult<()> {
    if self.parse_ident().is_err() {
      return self.report_error("expected identifier");
    }

    let mut after_period = false;
    loop {
      let t = match self.get_token() {
        Ok(token) => token,
        Err(_) => {
          if !after_period {
            return Ok(());
          }
          return self.error("unexpected end of input");
        }
      };

      match t {
        PERIOD => {
          if after_period {
            return self.error(r#"unexpected ".""#);
          } else {
            self.token()?;
            after_period = true;
          }
        }
        BRACKET_START if self.key_pattern_syntax => {
          self.step();

          match self.parse_ident() {
            Ok(_) => {}
            Err(_) => return self.error("expected identifier"),
          }

          let token = self.get_token()?;

          if !matches!(token, BRACKET_END) {
            self.error(r#"expected "]""#)?;
          }
          self.step();
          after_period = false;
        }
        _ => {
          if after_period {
            match self.parse_ident() {
              Ok(_) => {}
              Err(_) => return self.report_error("expected identifier"),
            }
            after_period = false;
          } else if self.key_pattern_syntax {
            return self.error("unexpected identifier");
          } else {
            break;
          }
        }
      };
    }

    Ok(())
  }

  fn parse_ident(&mut self) -> ParserResult<()> {
    let t = self.get_token()?;
    match t {
      IDENT => self.token(),
      IDENT_WITH_GLOB => {
        if self.key_pattern_syntax {
          self.token_as(IDENT)
        } else {
          self.error("expected identifier")
        }
      }
      INTEGER_HEX | INTEGER_BIN | INTEGER_OCT => self.token_as(IDENT),
      INTEGER => {
        if self.lexer.slice().starts_with('+') {
          Err(())
        } else {
          self.token_as(IDENT)
        }
      }
      STRING_LITERAL => {
        match allowed_chars::string_literal(self.lexer.slice()) {
          Ok(_) => {}
          Err(err_indices) => {
            for e in err_indices {
              self.add_relative_error(e, "invalid control character in string literal");
            }
          }
        };

        self.token_as(IDENT)
      }
      STRING => {
        match allowed_chars::string(self.lexer.slice()) {
          Ok(_) => {}
          Err(err_indices) => {
            for e in err_indices {
              self.add_relative_error(e, "invalid character in string");
            }
          }
        };

        match check_escape(self.lexer.slice()) {
          Ok(_) => self.token_as(IDENT),
          Err(err_indices) => {
            for e in err_indices {
              self.add_relative_error(e, "invalid escape sequence");
            }

            // We proceed normally even if
            // the string contains invalid escapes.
            // It shouldn't affect the rest of the parsing.
            self.token_as(IDENT)
          }
        }
      }
      FLOAT => {
        if self.lexer.slice().starts_with('0') {
          self.error("zero-padded numbers are not allowed")
        } else if self.lexer.slice().starts_with('+') {
          Err(())
        } else {
          for (i, s) in self.lexer.slice().split('.').enumerate() {
            if i != 0 {
              self.insert_token(PERIOD, ".");
            }

            self.insert_token(IDENT, s);
          }
          self.step();
          Ok(())
        }
      }
      BOOL => self.token_as(IDENT),
      DATE => self.token_as(IDENT),
      _ => self.error("expected identifier"),
    }
  }

  fn parse_value(&mut self) -> ParserResult<()> {
    let t = match self.get_token() {
      Ok(t) => t,
      Err(_) => return self.error("expected value"),
    };

    match t {
      BOOL | DATE_TIME_OFFSET | DATE_TIME_LOCAL | DATE | TIME => self.token(),
      INTEGER => {
        // This is probably a logos bug or a priority issue,
        // for some reason "1979-05-27" gets lexed as INTEGER.
        if !self.lexer.slice().starts_with('-') && self.lexer.slice().contains('-') {
          return self.token_as(DATE);
        }

        // FIXME: probably another logos bug.
        if self.lexer.slice().contains(':') {
          return self.token_as(TIME);
        }

        // This could've been done more elegantly probably.
        if (self.lexer.slice().starts_with('0') && self.lexer.slice() != "0")
          || (self.lexer.slice().starts_with("+0") && self.lexer.slice() != "+0")
          || (self.lexer.slice().starts_with("-0") && self.lexer.slice() != "-0")
        {
          self.error("zero-padded integers are not allowed")
        } else if !check_underscores(self.lexer.slice(), 10) {
          self.error("invalid underscores")
        } else {
          self.token()
        }
      }
      INTEGER_BIN => {
        if !check_underscores(self.lexer.slice(), 2) {
          self.error("invalid underscores")
        } else {
          self.token()
        }
      }
      INTEGER_HEX => {
        if !check_underscores(self.lexer.slice(), 16) {
          self.error("invalid underscores")
        } else {
          self.token()
        }
      }
      INTEGER_OCT => {
        if !check_underscores(self.lexer.slice(), 8) {
          self.error("invalid underscores")
        } else {
          self.token()
        }
      }
      FLOAT => {
        // FIXME: probably another logos bug.
        if self.lexer.slice().contains(':') {
          return self.token_as(TIME);
        }

        let int_slice = self.lexer.slice().split(['.', 'e', 'E']).next().unwrap_or(self.lexer.slice());

        if (int_slice.starts_with('0') && int_slice != "0")
          || (int_slice.starts_with("+0") && int_slice != "+0")
          || (int_slice.starts_with("-0") && int_slice != "-0")
        {
          self.error("zero-padded numbers are not allowed")
        } else if !check_underscores(self.lexer.slice(), 10) {
          self.error("invalid underscores")
        } else {
          self.token()
        }
      }
      STRING_LITERAL => {
        match allowed_chars::string_literal(self.lexer.slice()) {
          Ok(_) => {}
          Err(err_indices) => {
            for e in err_indices {
              self.add_relative_error(e, "invalid control character in string literal");
            }
          }
        };
        self.token()
      }
      MULTI_LINE_STRING_LITERAL => {
        match allowed_chars::multi_line_string_literal(self.lexer.slice()) {
          Ok(_) => {}
          Err(err_indices) => {
            for e in err_indices {
              self.add_relative_error(e, "invalid character in string");
            }
          }
        };
        self.token()
      }
      STRING => {
        match allowed_chars::string(self.lexer.slice()) {
          Ok(_) => {}
          Err(err_indices) => {
            for e in err_indices {
              self.add_relative_error(e, "invalid character in string");
            }
          }
        };

        match check_escape(self.lexer.slice()) {
          Ok(_) => self.token(),
          Err(err_indices) => {
            for e in err_indices {
              self.add_relative_error(e, "invalid escape sequence");
            }

            // We proceed normally even if
            // the string contains invalid escapes.
            // It shouldn't affect the rest of the parsing.
            self.token()
          }
        }
      }
      MULTI_LINE_STRING => {
        match allowed_chars::multi_line_string(self.lexer.slice()) {
          Ok(_) => {}
          Err(err_indices) => {
            for e in err_indices {
              self.add_relative_error(e, "invalid character in string");
            }
          }
        };

        match check_escape(self.lexer.slice()) {
          Ok(_) => self.token(),
          Err(err_indices) => {
            for e in err_indices {
              self.add_relative_error(e, "invalid escape sequence");
            }

            // We proceed normally even if
            // the string contains invalid escapes.
            // It shouldn't affect the rest of the parsing.
            self.token()
          }
        }
      }
      BRACKET_START => {
        with_node!(self.builder, ARRAY, self.parse_array())
      }
      BRACE_START => {
        with_node!(self.builder, INLINE_TABLE, self.parse_inline_table())
      }
      _ => self.error("expected value"),
    }
  }

  fn parse_inline_table(&mut self) -> ParserResult<()> {
    // https://github.com/toml-lang/toml/blob/78fcf9dd7eab7acfbaf147c684b649477e7bdd9c/toml.abnf#L238
    self.must_token_or(BRACE_START, r#"expected "{""#)?;

    let mut expect_comma_or_end = false;

    loop {
      let t = match self.get_token() {
        Ok(t) => t,
        Err(_) => return self.report_error(r#"expected "}""#),
      };

      match t {
        BRACE_END => {
          self.add_token()?;
          break;
        }
        WHITESPACE | NEWLINE | COMMENT => {
          self.add_token()?;
        }
        COMMA => {
          if !expect_comma_or_end {
            let _ = self.error(r#"unexpected ",""#);
          } else {
            self.add_token()?;
          }
          expect_comma_or_end = false;
        }
        _ => {
          if expect_comma_or_end {
            let _ = self.report_error(r#"expected "," or "}""#);
          }
          let _ = self.with_recovery_tokens(&[COMMA, BRACE_END], |parser| {
            with_node!(parser.builder, ENTRY, parser.parse_entry())
          });
          expect_comma_or_end = true;
        }
      }
    }
    Ok(())
  }

  fn parse_array(&mut self) -> ParserResult<()> {
    self.must_token_or(BRACKET_START, r#"expected "[""#)?;

    let mut first = true;
    let mut comma_last = false;
    loop {
      let t = match self.get_token() {
        Ok(t) => t,
        Err(_) => {
          let _ = self.report_error("unexpected EOF");
          return Err(());
        }
      };

      match t {
        BRACKET_END => break self.add_token()?,
        NEWLINE => {
          self.token()?;
          continue; // as if it wasn't there, so it doesn't count as a first token
        }
        COMMA => {
          if first || comma_last {
            let _ = self.error(r#"unexpected ",""#);
          } else {
            self.token()?;
          }
          comma_last = true;
        }
        _ => {
          if !comma_last && !first {
            let _ = self.report_error(r#"expected ",""#);
          }
          let _ = self.with_recovery_tokens(&[COMMA, BRACKET_END, NEWLINE], |parser| {
            with_node!(parser.builder, VALUE, parser.parse_value())
          });
          comma_last = false;
        }
      }

      first = false;
    }
    Ok(())
  }
}

fn check_underscores(s: &str, radix: u32) -> bool {
  if s.starts_with('_') || s.ends_with('_') {
    return false;
  }

  let mut last_char = '\0';

  for c in s.chars() {
    if c == '_' && !last_char.is_digit(radix) {
      return false;
    }
    if !c.is_digit(radix) && last_char == '_' {
      return false;
    }
    last_char = c;
  }

  true
}

/// The final results of a parsing.
/// It contains the green tree, and
/// the errors that occurred during parsing.
#[derive(Debug, Clone)]
pub struct Parse {
  pub green_node: GreenNode,
  pub errors:     Vec<Error>,
}

impl Parse {
  /// Turn the parse into a syntax node.
  pub fn into_syntax(self) -> SyntaxNode {
    SyntaxNode::new_root(self.green_node)
  }

  /// Turn the parse into a DOM tree.
  ///
  /// Any semantic errors that occur will be collected
  /// in the returned DOM node.
  pub fn into_dom(self) -> dom::node::Node {
    dom::Node::from_syntax(self.into_syntax().into())
  }
}

#[cfg(test)]
mod tests {
  use rowan::TextRange;
  use rowan::TextSize;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;

  use super::Error;
  use super::Parser;
  use super::parse;
  use crate::syntax::SyntaxKind::BRACKET_END;
  use crate::syntax::SyntaxKind::COMMA;
  use crate::syntax::SyntaxKind::ENTRY;
  use crate::syntax::SyntaxKind::ERROR;
  use crate::syntax::SyntaxKind::NEWLINE;
  use crate::syntax::SyntaxKind::ROOT;
  use crate::syntax::SyntaxKind::VALUE;

  fn error_count(source: &str, message: &str) -> usize {
    parse(source).errors.iter().filter(|error| error.message == message).count()
  }

  fn syntax_kind_count(source: &str, kind: crate::syntax::SyntaxKind) -> usize {
    parse(source)
      .into_syntax()
      .descendants_with_tokens()
      .filter(|element| element.kind() == kind)
      .count()
  }

  #[test]
  fn missing_array_comma_preserves_following_value() -> Result<(), TestFailure> {
    let invalid = "a = [1 2]";
    ensure_eq(
      &error_count(invalid, r#"expected ",""#),
      &1,
      "one missing array separator must be reported",
    )?;
    ensure_eq(
      &syntax_kind_count(invalid, VALUE),
      &3,
      "the entry value and both array values must remain parsed",
    )?;

    let valid = "a = [1, 2]";
    ensure_eq(
      &error_count(valid, r#"expected ",""#),
      &0,
      "a valid array separator must not be diagnosed",
    )
  }

  #[test]
  fn missing_inline_table_comma_preserves_following_entry() -> Result<(), TestFailure> {
    let invalid = "a = { b = 1 c = 2 }";
    ensure_eq(
      &error_count(invalid, r#"expected "," or "}""#),
      &1,
      "one missing inline-table separator must be reported",
    )?;
    ensure_eq(
      &syntax_kind_count(invalid, ENTRY),
      &3,
      "the outer entry and both inline-table entries must remain parsed",
    )?;

    let valid = "a = { b = 1, c = 2 }";
    ensure_eq(
      &error_count(valid, r#"expected "," or "}""#),
      &0,
      "a valid inline-table separator must not be diagnosed",
    )
  }

  #[test]
  fn consecutive_commas_consume_only_the_offending_delimiter() -> Result<(), TestFailure> {
    let array = "a = [1,,2]";
    ensure_eq(
      &syntax_kind_count(array, ERROR),
      &1,
      "one consecutive array comma must become one error token",
    )?;
    ensure_eq(
      &syntax_kind_count(array, VALUE),
      &3,
      "the value following the bad array comma must remain parsed",
    )?;

    let inline = "a = { b = 1,, c = 2 }";
    ensure_eq(
      &syntax_kind_count(inline, ERROR),
      &1,
      "one consecutive inline-table comma must become one error token",
    )?;
    ensure_eq(
      &syntax_kind_count(inline, ENTRY),
      &3,
      "the entry following the bad inline-table comma must remain parsed",
    )
  }

  #[test]
  fn recovery_boundaries_survive_missing_values() -> Result<(), TestFailure> {
    let closing_boundary = "a = { b = }\nc = 2";
    let closing_syntax = parse(closing_boundary).into_syntax();
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
      &syntax_kind_count(invalid_value, ERROR),
      &1,
      "a bare identifier in value position must become an error token",
    )?;
    ensure_eq(
      &syntax_kind_count(invalid_value, ENTRY),
      &2,
      "a non-boundary invalid value must not consume the following entry",
    )
  }

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

  #[test]
  fn repeated_same_range_recovery_forces_progress() -> Result<(), TestFailure> {
    let mut parser = Parser::new("]");
    parser.builder.start_node(ROOT.into());
    let token = parser.get_token().ok();
    ensure(token == Some(BRACKET_END), "the fixture must lex as a closing delimiter")?;

    parser.with_recovery_tokens(&[BRACKET_END], |scoped| {
      let _ = scoped.error("expected value");
      let _ = scoped.error("expected value");
    });

    ensure_eq(&parser.errors.len(), &1, "the same range and message must be recorded once")?;
    ensure(
      parser.get_token().is_err(),
      "the second same-range failure must consume the boundary and advance",
    )
  }

  #[test]
  fn error_collection_suppresses_only_exact_adjacent_duplicates() -> Result<(), TestFailure> {
    let mut parser = Parser::new("");
    let first = Error {
      range:   TextRange::new(TextSize::new(0), TextSize::new(1)),
      message: "first".into(),
    };
    let distinct_message = Error {
      range:   first.range,
      message: "second".into(),
    };
    let distinct_range = Error {
      range:   TextRange::new(TextSize::new(1), TextSize::new(2)),
      message: "first".into(),
    };

    parser.add_error(&first);
    parser.add_error(&first);
    parser.add_error(&distinct_message);
    parser.add_error(&distinct_range);

    ensure(
      parser.errors == [first, distinct_message, distinct_range],
      "only an exact adjacent duplicate may be suppressed",
    )
  }
}
