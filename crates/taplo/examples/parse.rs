//! Demonstrates the distinction between fatal tree construction failures and recoverable TOML
//! diagnostics.

use taplo::parser::ParseFailure;
use taplo::parser::parse;

/// Parse one document, inspect its recoverable diagnostics, and freeze its DOM.
///
/// # Errors
///
/// Returns [`ParseFailure`] when the lossless syntax tree cannot be constructed.
fn main() -> Result<(), ParseFailure> {
  const SOURCE: &str = "value = 1
value = 2

[table]
string = 'some string'";

  let parse_result = parse(SOURCE)?;

  // Recoverable syntax diagnostics are available separately from a fatal tree-construction
  // failure.
  let _syntax_diagnostics = parse_result.diagnostics();

  // DOM construction remains available for syntactically imperfect documents.
  let _root_node = parse_result.into_dom();

  Ok(())
}
