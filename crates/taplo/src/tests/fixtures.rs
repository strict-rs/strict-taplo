//! Deterministic runtime validation for the checked-in TOML fixture corpus.

use std::ffi::OsStr;
use std::fs::read_dir;
use std::fs::read_to_string;
use std::path::Path;
use std::path::PathBuf;

use strict_test_support::TestFailure;
use strict_test_support::ensure;
#[cfg(feature = "serde")]
use strict_test_support::ensure_eq;
use strict_test_support::ensure_ok;

#[cfg(feature = "serde")]
use crate::formatter;
#[cfg(feature = "serde")]
use crate::formatter::Options;
use crate::parser::parse;

/// Return sorted TOML fixture paths from one corpus directory.
fn fixture_paths(directory: &str) -> Result<Vec<PathBuf>, TestFailure> {
  let root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("../..")
    .join("test-data")
    .join(directory);
  let entries = ensure_ok(read_dir(root), "the TOML fixture corpus directory must be readable")?;
  let mut paths = Vec::new();

  for pending_entry in entries {
    let entry = ensure_ok(pending_entry, "every TOML fixture directory entry must be readable")?;
    let path = entry.path();
    if path.extension() == Some(OsStr::new("toml")) {
      paths.push(path);
    }
  }

  paths.sort();
  ensure(!paths.is_empty(), "the TOML fixture corpus must contain at least one fixture")?;
  Ok(paths)
}

/// Read one fixture as UTF-8 TOML source.
fn read_fixture(path: &Path) -> Result<String, TestFailure> {
  ensure_ok(read_to_string(path), "every TOML fixture must be readable as UTF-8 source")
}

/// Validate one corpus against its expected syntax-and-semantic cleanliness.
fn ensure_corpus(directory: &str, expect_clean: bool, failure_context: &'static str) -> Result<(), TestFailure> {
  let paths = fixture_paths(directory)?;
  let mut mismatches = Vec::new();

  for path in paths {
    let source = read_fixture(&path)?;
    let parsed = ensure_ok(parse(&source), "every corpus fixture must construct a lossless tree")?;
    let is_clean = [parsed.diagnostics().is_empty(), parsed.into_dom().validate().is_ok()] == [true, true];
    if is_clean != expect_clean {
      mismatches.push(path.display().to_string());
    }
  }

  if mismatches.is_empty() {
    Ok(())
  } else {
    Err(TestFailure::WasErr {
      context: failure_context,
      cause:   mismatches.join(", "),
    })
  }
}

/// Require every invalid fixture to produce syntax or semantic evidence of rejection.
#[test]
fn invalid_corpus_produces_syntax_or_semantic_diagnostics() -> Result<(), TestFailure> {
  ensure_corpus(
    "invalid",
    false,
    "every invalid TOML fixture must produce a syntax or semantic diagnostic",
  )
}

/// Require every valid fixture to remain clean through parsing and DOM validation.
#[test]
fn valid_corpus_is_syntax_and_semantically_clean() -> Result<(), TestFailure> {
  ensure_corpus(
    "valid",
    true,
    "every valid TOML fixture must remain free of syntax and semantic diagnostics",
  )
}

/// Require representative valid documents to preserve semantics through both rendering paths.
#[cfg(feature = "serde")]
#[test]
fn valid_corpus_formats_idempotently_and_round_trips_semantically() -> Result<(), TestFailure> {
  for path in fixture_paths("valid")? {
    let source = read_fixture(&path)?;
    let parsed = ensure_ok(parse(&source), "every valid round-trip fixture must parse")?;
    if !parsed.diagnostics().is_empty() {
      return Err(TestFailure::WasErr {
        context: "a valid round-trip fixture must have no syntax diagnostics",
        cause:   path.display().to_string(),
      });
    }
    let dom = parsed.into_dom();
    ensure(
      dom.validate().is_ok(),
      "a valid round-trip fixture must have no semantic diagnostics",
    )?;
    let expected_semantics = ensure_ok(
      serde_json::to_value(&dom),
      "a valid round-trip fixture must serialize its semantic value",
    )?;

    let formatted = ensure_ok(
      formatter::format(&source, &Options::default()),
      "a valid round-trip fixture must format",
    )?;
    let formatted_again = ensure_ok(
      formatter::format(&formatted, &Options::default()),
      "formatted corpus TOML must format a second time",
    )?;
    ensure_eq(
      &formatted_again,
      &formatted,
      "formatted corpus TOML must be a formatter fixed point",
    )?;

    let rendered = ensure_ok(
      dom.to_toml(false, false),
      "a valid semantic corpus value must render as standard TOML",
    )?;
    for (rendering, round_trip) in [("formatter", formatted), ("semantic renderer", rendered)] {
      let reparsed = ensure_ok(parse(&round_trip), "formatted or semantic-rendered corpus TOML must reparse")?;
      if !reparsed.diagnostics().is_empty() {
        return Err(TestFailure::WasErr {
          context: "formatted or semantic-rendered corpus TOML must remain syntactically valid",
          cause:   format!(
            "{} via {rendering}; diagnostics: {:?}; source:\n{round_trip}",
            path.display(),
            reparsed.diagnostics(),
          ),
        });
      }
      let reparsed_dom = reparsed.into_dom();
      ensure(
        reparsed_dom.validate().is_ok(),
        "formatted or semantic-rendered corpus TOML must remain semantically valid",
      )?;
      let actual_semantics = ensure_ok(
        serde_json::to_value(&reparsed_dom),
        "reparsed corpus TOML must serialize its semantic value",
      )?;
      ensure_eq(
        &actual_semantics,
        &expected_semantics,
        "both rendering paths must preserve the complete semantic TOML value",
      )?;
    }
  }

  Ok(())
}
