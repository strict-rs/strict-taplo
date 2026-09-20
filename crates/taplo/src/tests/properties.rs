//! Strict property contracts spanning parsing, formatting, queries, and rewrites.

use core::fmt::Debug;
use std::str::FromStr as _;

use proptest::collection;
use proptest::strict::ensure_property;
use strict_test_support::ensure_that;

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
fn parse_format_reparse_is_stable() -> Result<(), impl Debug> {
  let values = collection::vec(i16::MIN..=i16::MAX, 0..24);
  ensure_property(
    &values,
    "generated documents parse, format, reparse, and format idempotently",
    |generated_values| {
      let source = generated_values
        .iter()
        .enumerate()
        .fold(String::new(), |mut document, (index, integer)| {
          document.push_str("key_");
          document.push_str(&index.to_string());
          document.push('=');
          document.push_str(&integer.to_string());
          document.push('\n');
          document
        });
      let initial = parse(&source);
      let fixture_formatted = formatter::format(&source, &Options::default()).map(|text| {
        let reparsed = parse(&text);
        let again = formatter::format(&text, &Options::default());
        (text, reparsed, again)
      });
      ensure_that(
        (generated_values, source, initial, fixture_formatted),
        "formatting must preserve syntax, semantics, and its fixed point",
        |observed| {
          observed.2.as_ref().is_ok_and(|parsed| parsed.diagnostics().is_empty())
            && observed.3.as_ref().is_ok_and(|formatted| {
              formatted
                .1
                .as_ref()
                .is_ok_and(|parsed| parsed.diagnostics().is_empty() && parsed.clone().into_dom().validate().is_ok())
                && formatted.2.as_ref().is_ok_and(|again| again == &formatted.0)
            })
        },
      )
      .map_err(Box::new)
    },
  )
  .map(drop)
}

/// Require decoded semantic paths to locate their generated source values exactly.
#[test]
fn query_paths_round_trip_generated_nested_tables() -> Result<(), impl Debug> {
  let input = (collection::vec(0_u8..16, 1..6), i64::MIN..=-1_i64);
  ensure_property(
    &input,
    "generated dotted paths round-trip through parsing and immutable DOM lookup",
    |(segments, value)| {
      let path = segments
        .iter()
        .map(|segment| format!("key_{segment}"))
        .collect::<Vec<_>>()
        .join(".");
      let source = format!("[{path}]\nleaf = {value}\n");
      let fixture_parsed = parse(&source);
      let full_path = Keys::from_str(&format!("{path}.leaf"));
      ensure_that(
        (segments, value, source, fixture_parsed, full_path),
        "generated paths must retain syntax, semantic validity, and their exact signed value",
        |observed| {
          let Ok(ref parsed) = observed.3 else {
            return false;
          };
          let Ok(ref keys) = observed.4 else {
            return false;
          };
          let root = parsed.clone().into_dom();
          let Some(node) = root.path(keys) else {
            return false;
          };
          let Some(integer) = node.as_integer() else {
            return false;
          };
          parsed.diagnostics().is_empty() && root.validate().is_ok() && integer.value() == IntegerValue::Negative(observed.1)
        },
      )
      .map_err(Box::new)
    },
  )
  .map(drop)
}

/// Require disjoint source rewrites to remain atomic, ordered, and idempotent.
#[test]
fn non_overlapping_rewrites_preserve_unedited_source() -> Result<(), impl Debug> {
  let values = (3_i64..10_000_i64, 3_i64..10_000_i64);
  ensure_property(
    &values,
    "disjoint replacements preserve intervening source and commit idempotently",
    |(first, second)| {
      let first_source = format!("-{first}");
      let second_source = format!("-{second}");
      let fragments = [ValueFragment::parse(&first_source), ValueFragment::parse(&second_source)];
      let expected = format!("first = {first_source}\n# retained between edits\nsecond = {second_source}\n");
      let observed = Rewrite::parse("first = 1\n# retained between edits\nsecond = 2\n").map(|mut rewrite| {
        let [ref first_fragment, ref second_fragment] = fragments;
        let first_edit = first_fragment
          .as_ref()
          .ok()
          .map(|fragment| rewrite.replace_value(&ExactPath::from_segments(["first"]), fragment));
        let second_edit = second_fragment
          .as_ref()
          .ok()
          .map(|fragment| rewrite.replace_value(&ExactPath::from_segments(["second"]), fragment));
        let rendered = rewrite.render();
        let first_commit = rewrite.commit();
        let committed_source = rewrite.source().to_owned();
        let repeated_commit = rewrite.commit();
        (
          rewrite,
          [first_edit, second_edit],
          rendered,
          first_commit,
          committed_source,
          repeated_commit,
        )
      });
      ensure_that(
        (fragments, expected, observed),
        "replacement, rendering, and both commits must preserve all unedited source",
        |subject| {
          let Ok(ref rewrite) = subject.2 else {
            return false;
          };

          rewrite.1.iter().all(|edit| matches!(edit, Some(Ok(EditOutcome::Replaced))))
            && rewrite.2.as_ref().is_ok_and(|rendered| rendered == &subject.1)
            && rewrite.3.is_ok()
            && rewrite.4 == subject.1
            && rewrite.5.is_ok()
            && rewrite.0.source() == subject.1
        },
      )
      .map_err(Box::new)
    },
  )
  .map(drop)
}
