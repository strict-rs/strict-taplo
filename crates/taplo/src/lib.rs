//! # About
//!
//! The main purpose of the library is to provide tools for analyzing TOML data where the
//! layout must be preserved and the original position of every parsed token must be known. It can
//! also format TOML documents.
//!
//! It uses [Rowan](::rowan) for the syntax tree, and every character is preserved from the input,
//! including all comments and white space.
//!
//! A [DOM](dom) can be constructed for data-oriented analysis where each node wraps a part of the
//! syntax tree with additional information and functionality.
//!
//! # Features
//!
//! - **serde**: Support for [serde](https://serde.rs) serialization of the DOM nodes.
//! - **schema**: Enable JSON-schema generation for formatter configuration.
//!
//! # Usage
//!
//! A TOML document has to be parsed with [parse](parser::parse) first, it
//! will build a syntax tree that can be traversed.
//!
//! If there were no syntax errors during parsing, then a [`dom::Node`]
//! can be constructed. It will build a DOM tree and validate the TOML document according
//! to the specification. A DOM tree can be constructed even with syntax errors present, however
//! parts of it might be missing.
//!
//! ```
//! use strict_test_support::TestFailure;
//! use strict_test_support::ensure;
//! use strict_test_support::ensure_ok;
//! use taplo::parser::parse;
//!
//! # fn main() -> Result<(), TestFailure> {
//! const SOURCE: &str = "value = 1
//! value = 2
//!
//! [table]
//! string = 'some string'";
//!
//! let parse_result = ensure_ok(parse(SOURCE), "the syntax tree must build")?;
//!
//! // Check for syntax errors.
//! // These are not carried over to DOM errors.
//! ensure(
//!   parse_result.diagnostics().is_empty(),
//!   "the source must not produce syntax diagnostics",
//! )?;
//!
//! let root_node = parse_result.into_dom();
//!
//! // Check for semantic errors.
//! // In this example "value" is a duplicate key.
//! ensure(
//!   root_node.validate().is_err(),
//!   "the duplicate key must produce a semantic diagnostic",
//! )
//! # }
//! ```

use std::collections::HashMap as StandardHashMap;
use std::collections::HashSet as StandardHashSet;
use std::collections::hash_map::RandomState;

/// Immutable semantic TOML values, diagnostics, queries, rendering, and source-preserving rewrites.
pub mod dom;
/// Source-preserving formatting for parsed syntax trees and semantic DOM values.
pub mod formatter;
/// Lossless TOML parsing with ordered recoverable diagnostics and fatal builder failures.
pub mod parser;
/// Stable syntax kinds, typed Rowan aliases, and the private Logos-backed lexer.
pub mod syntax;
/// Escape decoding, syntax-tree operations, character validation, and source-range helpers.
pub mod util;

pub use rowan;

/// Workspace hash map using the standard randomized hasher.
pub type HashMap<K, V> = StandardHashMap<K, V, RandomState>;
/// Workspace hash set using the standard randomized hasher.
pub type HashSet<V> = StandardHashSet<V, RandomState>;

#[cfg(test)]
/// Shared panic-free syntax and DOM construction for sibling core tests.
pub mod test_support;

#[cfg(test)]
/// Cross-module parser, formatter, fixture, and property contracts.
mod tests;
