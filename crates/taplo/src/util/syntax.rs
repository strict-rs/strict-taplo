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
  use rowan::GreenNodeBuilder;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_ok;

  use super::add_all;
  use crate::syntax::SyntaxNode;
  use crate::test_support::parse_syntax;

  #[test]
  fn subtree_copy_preserves_nested_tokens_and_the_callers_builder_frame() -> Result<(), TestFailure> {
    let source = "alpha = [1, { beta = true }]\n";
    let syntax = parse_syntax(source, "the copied syntax fixture must parse")?;

    let mut root_builder = GreenNodeBuilder::new();
    ensure_ok(
      add_all(&syntax, &mut root_builder),
      "copying a complete syntax tree into an empty builder must succeed",
    )?;
    let copied = ensure_ok(root_builder.finish(), "the copied syntax root must satisfy the builder protocol")?;
    ensure(
      SyntaxNode::new_root(copied).to_string() == source,
      "syntax copying must preserve every nested node and token in source order",
    )?;

    let mut nested_builder = GreenNodeBuilder::new();
    nested_builder.start_node(syntax.kind().into());
    ensure_ok(
      add_all(&syntax, &mut nested_builder),
      "copying beneath an existing caller frame must succeed",
    )?;
    ensure_ok(
      nested_builder.finish_node(),
      "the caller must retain ownership of its enclosing builder frame",
    )?;
    let nested = ensure_ok(nested_builder.finish(), "the enclosing copied syntax tree must remain finishable")?;
    ensure(
      SyntaxNode::new_root(nested).to_string() == source,
      "an enclosing caller frame must not duplicate, drop, or reorder copied token text",
    )
  }
}
