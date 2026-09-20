//! Shared panic-free parser projections for core behavior tests.

use strict_test_support::ResultFailure;
use strict_test_support::ensure_ok;

use crate::dom::Node;
use crate::parser::ParseFailure;
use crate::parser::parse;
/// Parse one source fixture and publish its immutable semantic DOM.
pub(crate) fn parse_dom(source: &str, context: &'static str) -> Result<Node, ResultFailure<ParseFailure>> {
  Ok(ensure_ok(parse(source), context)?.into_dom())
}
