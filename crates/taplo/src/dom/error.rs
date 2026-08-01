//! Typed failures produced by DOM construction and semantic queries.
//!
//! [`crate::dom::error::Diagnostic`] retains source-anchored problems encountered while
//! constructing the immutable DOM, whereas [`QueryError`] reports failures to interpret or follow a
//! caller-supplied path.

use thiserror::Error;

use super::node::Key;
use super::node::MalformedScalar;
use crate::parser::Diagnostic as ParseDiagnostic;
use crate::parser::ParseFailure;
use crate::syntax::SyntaxElement;

/// A recoverable semantic problem discovered while constructing the DOM.
#[derive(Debug, Clone, Error)]
pub enum Diagnostic {
  /// Syntax appeared in a semantic position where it is not valid.
  #[error("the syntax was not expected here: {syntax:#?}")]
  UnexpectedSyntax {
    /// Source node or token that could not be assigned a valid semantic role.
    syntax: SyntaxElement,
  },
  /// A key contains an invalid escape sequence.
  #[error("the string contains invalid escape sequence(s)")]
  InvalidEscapeSequence {
    /// Source-backed quoted key token containing the rejected escape sequence.
    string: SyntaxElement,
  },
  /// A lexically classified scalar could not be decoded.
  #[error(transparent)]
  MalformedScalar(#[from] MalformedScalar),
  /// Two entries resolve to the same semantic key.
  #[error("conflicting keys")]
  ConflictingKeys {
    /// Later key occurrence whose definition conflicts with an existing entry.
    key:   Key,
    /// Previously resolved key that already owns the same semantic path.
    other: Key,
  },
  /// A table path traversed a non-table value.
  #[error("expected table")]
  ExpectedTable {
    /// Previously resolved key whose value cannot be traversed as a table.
    not_table:   Key,
    /// Later path segment that requires `not_table` to resolve to a table.
    required_by: Key,
  },
  /// An array-of-tables path traversed another value kind.
  #[error("expected array of tables")]
  ExpectedArrayOfTables {
    /// Previously resolved key whose value cannot accept another table element.
    not_array_of_tables: Key,
    /// Later path segment that requires an array-of-tables value at that key.
    required_by:         Key,
  },
}

/// A failure while interpreting or following a DOM query.
#[derive(Debug, Clone, Error)]
pub enum QueryError {
  /// The requested key or array index does not exist.
  #[error("the key or index was not found")]
  NotFound,
  /// A glob expression is not valid.
  #[error("invalid glob pattern: {0}")]
  InvalidGlob(#[from] globset::Error),
  /// A textual key query produced recoverable parser diagnostics.
  #[error("the given key is invalid: {0}")]
  InvalidKey(ParseDiagnostic),
  /// Rowan could not construct the syntax tree for a textual key query.
  #[error("the key syntax tree could not be constructed: {0}")]
  Parse(#[from] ParseFailure),
}
