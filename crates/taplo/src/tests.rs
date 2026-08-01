//! Cross-module parser and formatter behavior tests.

use strict_test_support::TestFailure;
use strict_test_support::ensure;
use strict_test_support::ensure_ok;

use crate::parser::parse;

/// Runtime validation of the checked-in valid and invalid TOML corpora.
mod fixtures;
/// Exact formatter behavior and option-policy contracts.
mod formatter;
/// Generated-input invariants spanning parsing, formatting, queries, and rewrites.
mod properties;

/// Accept local-time values inside inline arrays without diagnostics.
#[test]
fn time_in_arrays() -> Result<(), TestFailure> {
  let src = "
    a = [00:00:01, 02:03:04]
    ";

  let parsed = ensure_ok(parse(src), "the time-array syntax tree must construct")?;
  ensure(
    parsed.diagnostics().is_empty(),
    "valid times in arrays must not produce diagnostics",
  )
}

/// Accept trailing comments on regular and array-of-tables headers.
#[test]
fn comments_after_tables() -> Result<(), TestFailure> {
  let src = "
[[array]] # foo
[table] # foo
";
  let parsed = ensure_ok(parse(src), "the table-comment syntax tree must construct")?;
  ensure(
    parsed.diagnostics().is_empty(),
    "comments after table headers must not produce diagnostics",
  )
}

/// Treat date-shaped tokens as legal table and entry keys.
#[test]
fn dates_in_table_keys() -> Result<(), TestFailure> {
  let src = "
[2024-01-01]
2024-01-01 = true

[[2024-01-02]]
2024-01-01 = true
";
  let parsed = ensure_ok(parse(src), "the date-key syntax tree must construct")?;
  ensure(
    parsed.diagnostics().is_empty(),
    "dates used as table keys must not produce diagnostics",
  )
}

/// Accept the supported multiline inline-table form with a trailing comma.
#[test]
fn inline_table_with_linebreaks_and_trailing_comma() -> Result<(), TestFailure> {
  let src = r#"
cooldowns = { 
    foo = "foo",
    bar = "bar",
}
"#;
  let parsed = ensure_ok(parse(src), "the multiline inline-table syntax tree must construct")?;
  ensure(
    parsed.diagnostics().is_empty(),
    "supported multiline inline tables must not produce diagnostics",
  )
}
