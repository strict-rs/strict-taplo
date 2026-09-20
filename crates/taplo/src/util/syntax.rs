//! Fallible operations for copying Taplo syntax into Rowan green-tree builders.
//!
//! These helpers preserve source node and token order while propagating Rowan builder protocol
//! failures instead of finalizing an invalid tree.

use rowan::BuildError;
use rowan::GreenNodeBuilder;
use rowan::NodeOrToken;

use crate::syntax::SyntaxNode;

/// Append a syntax subtree to a protocol-checked green builder.
///
/// # Errors
///
/// Returns [`BuildError`] when Rowan rejects a token or node construction
/// operation.
#[allow(
  clippy::single_call_fn,
  reason = "the recursive public helper preserves syntax order while propagating every Rowan builder protocol failure"
)]
pub fn add_all(node: &SyntaxNode, builder: &mut GreenNodeBuilder<'_>) -> Result<(), BuildError> {
  builder.start_node(node.kind().into());

  for child in node.children_with_tokens() {
    match child {
      NodeOrToken::Node(child_node) => add_all(&child_node, builder)?,
      NodeOrToken::Token(token) => builder.token(token.kind().into(), token.text())?,
    }
  }

  builder.finish_node()
}

#[cfg(test)]
/// Syntax-subtree copy contracts.
mod tests {
  use core::fmt::Debug;

  use rowan::GreenNodeBuilder;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_that;

  use super::add_all;
  use crate::parser::parse;

  #[test]
  fn subtree_copy_preserves_nested_tokens_and_the_callers_builder_frame() -> Result<(), impl Debug> {
    let source = "alpha = [1, { beta = true }]\n";
    let observed = ensure_ok(parse(source), "the copied syntax fixture must parse").map(|parsed| {
      let syntax = parsed.into_syntax();
      let mut root_builder = GreenNodeBuilder::new();
      let root_copy = add_all(&syntax, &mut root_builder);
      let copied = root_builder.finish();
      let mut nested_builder = GreenNodeBuilder::new();
      nested_builder.start_node(syntax.kind().into());
      let nested_copy = add_all(&syntax, &mut nested_builder);
      let enclosing = nested_builder.finish_node();
      let nested = nested_builder.finish();
      (syntax, root_copy, copied, nested_copy, enclosing, nested)
    });
    ensure_that(
      observed,
      "subtree copies must preserve exact source and caller-owned builder frames",
      |result| {
        let Ok(ref value) = *result else {
          return false;
        };

        value.1.is_ok()
          && value.2.as_ref().is_ok_and(|green| green.to_string() == source)
          && value.3.is_ok()
          && value.4.is_ok()
          && value.5.as_ref().is_ok_and(|green| green.to_string() == source)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }
}
