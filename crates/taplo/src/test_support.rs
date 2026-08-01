//! Shared panic-free parser projections for core behavior tests.

use strict_test_support::TestFailure;
use strict_test_support::ensure_ok;

use crate::dom::Node;
use crate::parser::parse;
use crate::syntax::SyntaxNode;

/// Parse one source fixture and retain its lossless syntax tree.
pub(crate) fn parse_syntax(source: &str, context: &'static str) -> Result<SyntaxNode, TestFailure> {
  Ok(ensure_ok(parse(source), context)?.into_syntax())
}

/// Parse one source fixture and publish its immutable semantic DOM.
pub(crate) fn parse_dom(source: &str, context: &'static str) -> Result<Node, TestFailure> {
  Ok(ensure_ok(parse(source), context)?.into_dom())
}
