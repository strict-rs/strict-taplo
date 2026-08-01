//! Strict property contracts spanning parsing, formatting, queries, and rewrites.

use std::str::FromStr as _;

use proptest::collection;
use proptest::strict::ensure_property;
use strict_test_support::TestFailure;
use strict_test_support::ensure;
use strict_test_support::ensure_eq;
use strict_test_support::ensure_ok;
use strict_test_support::ensure_some;

use crate::dom::Keys;
use crate::dom::node::IntegerValue;
use crate::dom::rewrite::EditOutcome;
use crate::dom::rewrite::ExactPath;
use crate::dom::rewrite::Rewrite;
use crate::dom::rewrite::ValueFragment;
use crate::formatter;
use crate::formatter::Options;
use crate::parser::parse;

/// Require formatting to preserve valid generated documents and reach a fixed point.
#[test]
fn parse_format_reparse_is_stable() -> Result<(), TestFailure> {
  let values = collection::vec(i16::MIN..=i16::MAX, 0..24);
  ensure_property(
    &values,
    "valid generated entries parse, format, reparse, and format idempotently",
    |generated_values| {
      let source = generated_values
        .iter()
        .enumerate()
        .fold(String::new(), |mut generated_source, (index, integer)| {
          generated_source.push_str("key_");
          generated_source.push_str(&index.to_string());
          generated_source.push('=');
          generated_source.push_str(&integer.to_string());
          generated_source.push('\n');
          generated_source
        });
      let initial = ensure_ok(parse(&source), "the generated document syntax tree must construct")?;
      ensure(
        initial.diagnostics().is_empty(),
        "the generated document must be syntactically valid",
      )?;

      let formatted = ensure_ok(
        formatter::format(&source, &Options::default()),
        "the generated document must format",
      )?;
      let reparsed = ensure_ok(parse(&formatted), "the formatted generated document syntax tree must construct")?;
      ensure(reparsed.diagnostics().is_empty(), "formatting must preserve syntactic validity")?;
      ensure(reparsed.into_dom().validate().is_ok(), "formatting must preserve semantic validity")?;

      let formatted_again = ensure_ok(
        formatter::format(&formatted, &Options::default()),
        "the formatted document must format a second time",
      )?;
      ensure_eq(
        &formatted_again,
        &formatted,
        "a successfully formatted document must be a formatter fixed point",
      )
    },
  )
}

/// Require decoded semantic paths to locate their generated source values exactly.
#[test]
fn query_paths_round_trip_generated_nested_tables() -> Result<(), TestFailure> {
  let input = (collection::vec(0_u8..16, 1..6), i64::MIN..=-1_i64);
  ensure_property(
    &input,
    "generated dotted paths round-trip through parser and immutable DOM lookup",
    |(segments, value)| {
      let path = segments
        .iter()
        .map(|segment| format!("key_{segment}"))
        .collect::<Vec<_>>()
        .join(".");
      let source = format!("[{path}]\nleaf = {value}\n");
      let parsed = ensure_ok(parse(&source), "the generated nested document must construct")?;
      ensure(
        parsed.diagnostics().is_empty(),
        "the generated nested document must be syntactically valid",
      )?;
      let root = parsed.into_dom();
      ensure(root.validate().is_ok(), "the generated nested document must be semantically valid")?;

      let full_path = ensure_ok(
        Keys::from_str(&format!("{path}.leaf")),
        "the generated exact semantic path must parse",
      )?;
      let node = ensure_some(root.path(&full_path), "the decoded generated path must locate its leaf")?;
      let integer = ensure_some(node.as_integer(), "the generated path must retain its integer node kind")?;
      ensure_eq(
        &integer.value(),
        &IntegerValue::Negative(value),
        "the generated semantic path must retain the exact decoded value",
      )
    },
  )
}

/// Require disjoint source rewrites to remain atomic, ordered, and idempotent.
#[test]
fn non_overlapping_rewrites_preserve_unedited_source() -> Result<(), TestFailure> {
  let values = (3_i64..10_000_i64, 3_i64..10_000_i64);
  ensure_property(
    &values,
    "non-overlapping replacements preserve intervening source and commit idempotently",
    |(first, second)| {
      let first_source = format!("-{first}");
      let second_source = format!("-{second}");
      let first_fragment = ensure_ok(
        ValueFragment::parse(&first_source),
        "the first generated replacement must be a TOML value",
      )?;
      let second_fragment = ensure_ok(
        ValueFragment::parse(&second_source),
        "the second generated replacement must be a TOML value",
      )?;
      let first_path = ExactPath::from_segments(["first"]);
      let second_path = ExactPath::from_segments(["second"]);
      let mut rewrite = ensure_ok(
        Rewrite::parse("first = 1\n# retained between edits\nsecond = 2\n"),
        "the rewrite fixture must parse",
      )?;

      ensure_eq(
        &ensure_ok(
          rewrite.replace_value(&first_path, &first_fragment),
          "the first disjoint replacement must queue",
        )?,
        &EditOutcome::Replaced,
        "the first generated value must replace its original",
      )?;
      ensure_eq(
        &ensure_ok(
          rewrite.replace_value(&second_path, &second_fragment),
          "the second disjoint replacement must queue",
        )?,
        &EditOutcome::Replaced,
        "the second generated value must replace its original",
      )?;

      let expected = format!("first = {first_source}\n# retained between edits\nsecond = {second_source}\n");
      let rendered = ensure_ok(rewrite.render(), "the two disjoint replacements must render atomically")?;
      ensure_eq(
        &rendered,
        &expected,
        "rendering must preserve every byte outside the two value ranges",
      )?;
      ensure_ok(rewrite.commit(), "the two disjoint replacements must commit atomically")?;
      ensure_eq(
        &rewrite.source(),
        &expected.as_str(),
        "commit must publish exactly the rendered source",
      )?;
      ensure_ok(rewrite.commit(), "repeating a clean commit must remain successful")?;
      ensure_eq(&rewrite.source(), &expected.as_str(), "repeating a clean commit must be idempotent")
    },
  )
}
