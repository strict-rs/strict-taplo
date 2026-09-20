//! TOML basic-string escape encoding, decoding, and ordered typed validation.
//!
//! A private Logos vocabulary recognizes escapes without replacing the main TOML lexer. Public
//! helpers preserve UTF-8 byte offsets and distinguish unsupported, incomplete, and invalid
//! Unicode forms.

use core::fmt;
use core::fmt::Display;
use core::fmt::Formatter;
use std::char::from_u32;

use logos::Lexer;
use logos::Logos;
use thiserror::Error;

/// Escaping based on:
///
/// \b         - backspace       (U+0008)
/// \t         - tab             (U+0009)
/// \n         - linefeed        (U+000A)
/// \f         - form feed       (U+000C)
/// \r         - carriage return (U+000D)
/// \"         - quote           (U+0022)
/// \\         - backslash       (U+005C)
/// \uXXXX     - unicode         (U+XXXX)
/// \UXXXXXXXX - unicode         (U+XXXXXXXX)
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Escape {
  /// Backspace escape.
  #[token(r#"\b"#)]
  Backspace,

  /// Horizontal-tab escape.
  #[token(r#"\t"#)]
  Tab,

  /// Escaped physical newline and surrounding whitespace.
  #[regex(r#"(\\\n[ \t]*)|(\\\r\n[ \t]*)"#)]
  Newline,

  /// Line-feed escape.
  #[token(r#"\n"#)]
  LineFeed,

  /// Form-feed escape.
  #[token(r#"\f"#)]
  FormFeed,

  /// Carriage-return escape.
  #[token(r#"\r"#)]
  CarriageReturn,

  /// Double-quote escape.
  #[token(r#"\""#)]
  Quote,

  /// Backslash escape.
  #[token(r#"\\"#)]
  Backslash,

  /// Four-digit Unicode escape.
  // Same thing repeated 4 times, but the {n} repetition syntax is not supported by Logos.
  #[regex(r#"\\u[0-9A-Fa-f_][0-9A-Fa-f_][0-9A-Fa-f_][0-9A-Fa-f_]"#)]
  Unicode,

  /// Eight-digit Unicode escape.
  // Same thing repeated 8 times, but the {n} repetition syntax is not supported by Logos.
  #[regex(r#"\\U[0-9A-Fa-f_][0-9A-Fa-f_][0-9A-Fa-f_][0-9A-Fa-f_][0-9A-Fa-f_][0-9A-Fa-f_][0-9A-Fa-f_][0-9A-Fa-f_]"#)]
  UnicodeLarge,

  /// Backslash followed by an unsupported escape character.
  // Catch-all for an unrecognized escape (`\` + any char); lower priority than the
  // specific escape tokens above so those win when both match the same two characters.
  #[regex(r#"\\."#, priority = 2)]
  Unknown,
}

/// Category of a TOML basic-string escape failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscapeErrorKind {
  /// The escape name is not part of TOML's supported vocabulary.
  UnknownSequence,
  /// A final backslash has no escape name or continuation line.
  TrailingBackslash,
  /// Unicode digits could not be decoded.
  InvalidUnicodeDigits,
  /// A decoded Unicode integer is not a scalar value.
  InvalidUnicodeScalar,
  /// A lexer token did not contain its required Unicode prefix and payload.
  InvalidTokenBoundary,
}

impl EscapeErrorKind {
  /// Return the stable reader-facing description of this failure category.
  #[must_use]
  pub const fn as_str(self) -> &'static str {
    match self {
      Self::UnknownSequence => "unknown escape sequence",
      Self::TrailingBackslash => "trailing backslash",
      Self::InvalidUnicodeDigits => "invalid Unicode escape digits",
      Self::InvalidUnicodeScalar => "invalid Unicode scalar value",
      Self::InvalidTokenBoundary => "invalid Unicode escape token",
    }
  }
}

impl Display for EscapeErrorKind {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter.write_str(self.as_str())
  }
}

/// Typed TOML basic-string escape failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("{kind} at byte offset {offset}")]
pub struct EscapeError {
  /// Byte offset within the decoded string body.
  offset: usize,
  /// Stable failure category.
  kind:   EscapeErrorKind,
}

impl EscapeError {
  /// Return the byte offset within the decoded string body.
  #[must_use]
  pub const fn offset(self) -> usize {
    self.offset
  }

  /// Return the stable failure category.
  #[must_use]
  pub const fn kind(self) -> EscapeErrorKind {
    self.kind
  }
}

/// Escape values in a given string.
#[must_use]
pub fn escape(source: &str) -> String {
  let mut escaped = String::with_capacity(source.len());

  for character in source.chars() {
    match character {
      '\u{0008}' => escaped.push_str(r"\b"),
      '\u{0009}' => escaped.push_str(r"\t"),
      '\u{000A}' => escaped.push_str(r"\n"),
      '\u{000C}' => escaped.push_str(r"\f"),
      '\u{000D}' => escaped.push_str(r"\r"),
      '\u{0022}' => escaped.push_str(r#"\""#),
      '\u{005C}' => escaped.push_str(r"\\"),
      '\u{0000}'..='\u{0007}' | '\u{000B}' | '\u{000E}'..='\u{001F}' | '\u{007F}' => {
        let code_point = u32::from(character);
        escaped.push_str(r"\u00");
        escaped.push(hexadecimal_digit((code_point >> 4) & 0x0F));
        escaped.push(hexadecimal_digit(code_point & 0x0F));
      }
      _ => {
        escaped.push(character);
      }
    }
  }

  escaped
}

/// Render one four-bit value as an uppercase hexadecimal digit.
fn hexadecimal_digit(nibble: u32) -> char {
  char::from_digit(nibble, 16).map_or('0', |digit| digit.to_ascii_uppercase())
}

/// Decode every supported TOML basic-string escape sequence.
///
/// # Errors
///
/// Returns [`EscapeError`] when an explicit escape sequence is unsupported or
/// does not encode a Unicode scalar value.
pub fn unescape(source: &str) -> Result<String, EscapeError> {
  let mut new_s = String::with_capacity(source.len());
  let mut lexer: Lexer<'_, Escape> = Lexer::new(source);

  while let Some(token_result) = lexer.next() {
    // Unrecognized non-escape input is passed through verbatim.
    let Ok(escape) = token_result else {
      if lexer.slice() == "\\" {
        return Err(EscapeError {
          offset: lexer.span().start,
          kind:   EscapeErrorKind::TrailingBackslash,
        });
      }
      new_s.push_str(lexer.slice());
      continue;
    };
    match escape {
      Escape::Backspace => new_s.push('\u{0008}'),
      Escape::Tab => new_s.push('\u{0009}'),
      Escape::LineFeed => new_s.push('\u{000A}'),
      Escape::FormFeed => new_s.push('\u{000C}'),
      Escape::CarriageReturn => new_s.push('\u{000D}'),
      Escape::Quote => new_s.push('\u{0022}'),
      Escape::Backslash => new_s.push('\u{005C}'),
      Escape::Newline => {}
      Escape::Unicode | Escape::UnicodeLarge => {
        new_s.push(decode_unicode(lexer.slice(), lexer.span().start)?);
      }
      Escape::Unknown => {
        return Err(EscapeError {
          offset: lexer.span().start,
          kind:   EscapeErrorKind::UnknownSequence,
        });
      }
    }
  }

  new_s.push_str(lexer.remainder());
  Ok(new_s)
}

/// Validate escapes without allocating a decoded string.
///
/// # Errors
///
/// Returns every typed invalid escape in source order.
pub fn check_escape(source: &str) -> Result<(), Vec<EscapeError>> {
  let mut lexer: Lexer<'_, Escape> = Lexer::new(source);
  let mut invalid = Vec::new();

  while let Some(token_result) = lexer.next() {
    // Unrecognized non-escape input is not an escape error.
    let Ok(escape) = token_result else {
      if lexer.slice() == "\\" {
        invalid.push(EscapeError {
          offset: lexer.span().start,
          kind:   EscapeErrorKind::TrailingBackslash,
        });
      }
      continue;
    };
    match escape {
      Escape::Backspace
      | Escape::Tab
      | Escape::LineFeed
      | Escape::FormFeed
      | Escape::CarriageReturn
      | Escape::Quote
      | Escape::Backslash
      | Escape::Newline => {}
      Escape::Unicode | Escape::UnicodeLarge => {
        if let Err(error) = decode_unicode(lexer.slice(), lexer.span().start) {
          invalid.push(error);
        }
      }
      Escape::Unknown => invalid.push(EscapeError {
        offset: lexer.span().start,
        kind:   EscapeErrorKind::UnknownSequence,
      }),
    }
  }

  if invalid.is_empty() { Ok(()) } else { Err(invalid) }
}

/// Decode the Unicode payload of one lexer-proven escape token.
fn decode_unicode(token: &str, offset: usize) -> Result<char, EscapeError> {
  let digits = token
    .strip_prefix("\\u")
    .or_else(|| token.strip_prefix("\\U"))
    .ok_or(EscapeError {
      offset,
      kind: EscapeErrorKind::InvalidTokenBoundary,
    })?;
  let code_point = u32::from_str_radix(digits, 16).map_err(|_parse_error| EscapeError {
    offset,
    kind: EscapeErrorKind::InvalidUnicodeDigits,
  })?;
  from_u32(code_point).ok_or(EscapeError {
    offset,
    kind: EscapeErrorKind::InvalidUnicodeScalar,
  })
}

#[cfg(test)]
/// Escape round-trip, continuation, offset, and failure-ordering contracts.
mod tests {
  use core::fmt::Debug;

  use strict_test_support::ComparisonFailure;
  use strict_test_support::PredicateFailure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_that;

  use super::EscapeError;
  use super::EscapeErrorKind;
  use super::check_escape;
  use super::decode_unicode;
  use super::escape;
  use super::unescape;

  /// A native string-decoding outcome.
  type Decoded = Result<String, EscapeError>;
  /// Exact decoding comparisons, including every result in a scenario.
  type DecodeComparison<const N: usize> = ComparisonFailure<[Decoded; N], [Decoded; N]>;

  /// Round-trip every supported control escape and preserve ordinary source text.
  #[test]
  fn escapes_and_decodes_supported_characters() -> Result<(), PredicateFailure<(String, Decoded)>> {
    let original = "\u{0000}\u{0007}\u{0008}\t\n\u{000b}\u{000c}\r\u{001f}\"\\plain\u{007f}";
    let encoded = escape(original);
    let decoded = unescape(&encoded);
    ensure_that(
      (encoded, decoded),
      "escaping must preserve the exact value and encode every forbidden control",
      |observed| {
        observed.0 == r#"\u0000\u0007\b\t\n\u000B\f\r\u001F\"\\plain\u007F"# && observed.1.as_ref().is_ok_and(|value| value == original)
      },
    )
    .map(drop)
  }

  /// Decode Unicode escapes together with a whitespace-trimming CRLF continuation.
  #[test]
  fn decodes_unicode_and_line_continuations() -> Result<(), DecodeComparison<1>> {
    ensure_eq(
      [unescape("caf\\u00E9\\\r\n  au\\U00000020lait")],
      [Ok(String::from("caf\u{e9}au lait"))],
      "Unicode escapes and CRLF continuation must retain Unicode scalar values",
    )
    .map(drop)
  }

  /// Elide physical LF and CRLF continuations without consuming ordinary whitespace.
  #[test]
  fn line_continuations_elide_following_indentation_only() -> Result<(), impl Debug> {
    ensure_eq(
      [
        unescape("left\\\n \t right"),
        unescape("left\\\r\n\t  right"),
        unescape("left  right"),
        unescape(r"left\n  right"),
      ],
      [
        Ok(String::from("leftright")),
        Ok(String::from("leftright")),
        Ok(String::from("left  right")),
        Ok(String::from("left\n  right")),
      ],
      "physical continuations must elide indentation while ordinary spaces and escaped newlines remain",
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Locate an unsupported escape by its UTF-8 byte offset.
  #[test]
  fn reports_unknown_escape_at_byte_offset() -> Result<(), DecodeComparison<1>> {
    ensure_eq(
      [unescape("\u{e9}\\q")],
      [Err(EscapeError {
        offset: 2,
        kind:   EscapeErrorKind::UnknownSequence,
      })],
      "unknown escapes must retain their UTF-8 byte offset and native category",
    )
    .map(drop)
  }

  /// Classify and locate a terminal backslash consistently in decoding and validation.
  #[test]
  fn reports_a_trailing_backslash_in_decode_and_validation() -> Result<(), impl Debug> {
    ensure_that(
      (unescape("value\\"), check_escape("value\\")),
      "decoding and validation must reject the same final backslash exactly once",
      |observed| {
        let expected = EscapeError {
          offset: 5,
          kind:   EscapeErrorKind::TrailingBackslash,
        };
        observed.0 == Err(expected) && observed.1 == Err(vec![expected])
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Keep malformed digits, non-scalar Unicode, and token-boundary failures distinct.
  #[test]
  fn distinguishes_invalid_unicode_failures() -> Result<(), impl Debug> {
    ensure_that(
      ([unescape("\\u____"), unescape("\\uD800")], decode_unicode("00E9", 7)),
      "Unicode failures must distinguish invalid digits, scalars, and supplied token boundaries",
      |observed| {
        observed.0
          == [
            Err(EscapeError {
              offset: 0,
              kind:   EscapeErrorKind::InvalidUnicodeDigits,
            }),
            Err(EscapeError {
              offset: 0,
              kind:   EscapeErrorKind::InvalidUnicodeScalar,
            }),
          ]
          && observed.1
            == Err(EscapeError {
              offset: 7,
              kind:   EscapeErrorKind::InvalidTokenBoundary,
            })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Return every escape failure in source order with its original byte coordinate.
  #[test]
  fn collects_escape_failures_in_source_order() -> Result<(), impl Debug> {
    ensure_eq(
      check_escape("\\q ok \\u____ \\z"),
      Err(vec![
        EscapeError {
          offset: 0,
          kind:   EscapeErrorKind::UnknownSequence,
        },
        EscapeError {
          offset: 6,
          kind:   EscapeErrorKind::InvalidUnicodeDigits,
        },
        EscapeError {
          offset: 13,
          kind:   EscapeErrorKind::UnknownSequence,
        },
      ]),
      "all escape failures must remain ordered and precisely located",
    )
    .map(drop)
    .map_err(Box::new)
  }
}
