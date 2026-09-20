//! Cross-module parser and formatter behavior tests.

use strict_test_support::PredicateFailure;
use strict_test_support::ensure_that;

use crate::parser::Parse;
use crate::parser::ParseFailure;
use crate::parser::parse;

/// A complete parse result retained when a valid-input contract fails.
type ValidParseFailure = PredicateFailure<Result<Parse, ParseFailure>>;

/// Runtime validation of the checked-in valid and invalid TOML corpora.
mod fixtures;
/// Exact formatter behavior and option-policy contracts.
mod formatter;
/// Generated-input invariants spanning parsing, formatting, queries, and rewrites.
mod properties;

/// Accept local-time values inside inline arrays without diagnostics.
#[test]
fn time_in_arrays() -> Result<(), ValidParseFailure> {
  let src = "
    a = [00:00:01, 02:03:04]
    ";

  ensure_that(parse(src), "valid times in arrays must not produce diagnostics", |parsed| {
    parsed.as_ref().is_ok_and(|value| value.diagnostics().is_empty())
  })
  .map(drop)
}

/// Accept trailing comments on regular and array-of-tables headers.
#[test]
fn comments_after_tables() -> Result<(), ValidParseFailure> {
  let src = "
[[array]] # foo
[table] # foo
";
  ensure_that(parse(src), "comments after table headers must not produce diagnostics", |parsed| {
    parsed.as_ref().is_ok_and(|value| value.diagnostics().is_empty())
  })
  .map(drop)
}

/// Treat date-shaped tokens as legal table and entry keys.
#[test]
fn dates_in_table_keys() -> Result<(), ValidParseFailure> {
  let src = "
[2024-01-01]
2024-01-01 = true

[[2024-01-02]]
2024-01-01 = true
";
  ensure_that(parse(src), "dates used as table keys must not produce diagnostics", |parsed| {
    parsed.as_ref().is_ok_and(|value| value.diagnostics().is_empty())
  })
  .map(drop)
}

/// Accept the supported multiline inline-table form with a trailing comma.
#[test]
fn inline_table_with_linebreaks_and_trailing_comma() -> Result<(), ValidParseFailure> {
  let src = r#"
cooldowns = { 
    foo = "foo",
    bar = "bar",
}
"#;
  ensure_that(
    parse(src),
    "supported multiline inline tables must not produce diagnostics",
    |parsed| parsed.as_ref().is_ok_and(|value| value.diagnostics().is_empty()),
  )
  .map(drop)
}
