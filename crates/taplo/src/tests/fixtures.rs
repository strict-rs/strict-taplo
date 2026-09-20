//! Deterministic runtime validation for the checked-in TOML fixture corpus.

use core::fmt::Debug;
use std::ffi::OsStr;
use std::fs::read_dir;
use std::fs::read_to_string;
use std::io::Error as IoError;
use std::path::Path;
use std::path::PathBuf;

use strict_test_support::ensure_that;

#[cfg(feature = "serde")]
use crate::dom::Diagnostic;
#[cfg(feature = "serde")]
use crate::dom::Node;
#[cfg(feature = "serde")]
use crate::formatter;
#[cfg(feature = "serde")]
use crate::formatter::Options;
#[cfg(feature = "serde")]
use crate::parser::Parse;
#[cfg(feature = "serde")]
use crate::parser::ParseFailure;
use crate::parser::parse;

/// Return sorted native directory outcomes for every TOML fixture.
fn fixture_paths(directory: &str) -> Result<Vec<Result<PathBuf, IoError>>, IoError> {
  let root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("../..")
    .join("test-data")
    .join(directory);
  read_dir(root).map(|entries| {
    let mut paths = entries
      .filter_map(|entry| match entry {
        Ok(value) => {
          let path = value.path();
          (path.extension() == Some(OsStr::new("toml"))).then_some(Ok(path))
        }
        Err(error) => Some(Err(error)),
      })
      .collect::<Vec<_>>();
    paths.sort_by(|left, right| left.as_ref().ok().cmp(&right.as_ref().ok()));
    paths
  })
}

/// Read one fixture as UTF-8 TOML source without erasing the native I/O error.
fn read_fixture(path: &Path) -> Result<String, IoError> {
  read_to_string(path)
}

/// Validate the entire corpus while retaining every path, read, parse, and diagnostic.
fn ensure_corpus(directory: &str, expect_clean: bool, failure_context: &'static str) -> Result<impl Debug, impl Debug> {
  let observe = |path: PathBuf| {
    let document = read_fixture(&path).map(|source| {
      let parsed = parse(&source).map(|parsed| {
        let dom = parsed.clone().into_dom();
        let validation = dom.validate();
        (parsed, dom, validation)
      });
      (source, parsed)
    });
    (path, document)
  };
  let corpus = fixture_paths(directory).map(|paths| paths.into_iter().map(|pending| pending.map(observe)).collect::<Vec<_>>());
  ensure_that(corpus, failure_context, |observed| {
    let Ok(ref fixtures) = *observed else {
      return false;
    };
    !fixtures.is_empty()
      && fixtures.iter().all(|fixture| {
        let &Ok((_, Ok((_, Ok((ref parsed, _, ref validation)))))) = fixture else {
          return false;
        };
        (parsed.diagnostics().is_empty() && validation.is_ok()) == expect_clean
      })
  })
  .map_err(Box::new)
}

/// Require every invalid fixture to produce syntax or semantic evidence of rejection.
#[test]
fn invalid_corpus_produces_syntax_or_semantic_diagnostics() -> Result<(), impl Debug> {
  ensure_corpus(
    "invalid",
    false,
    "every invalid fixture must produce syntax or semantic diagnostics",
  )
  .map(drop)
}

/// Require every valid fixture to remain clean through parsing and DOM validation.
#[test]
fn valid_corpus_is_syntax_and_semantically_clean() -> Result<(), impl Debug> {
  ensure_corpus(
    "valid",
    true,
    "every valid fixture must remain free of syntax and semantic diagnostics",
  )
  .map(drop)
}

/// Parse, DOM, validation, and JSON semantic observations for one rendering.
#[cfg(feature = "serde")]
type SemanticDocument = (
  Parse,
  Node,
  Result<(), Vec<Diagnostic>>,
  Result<serde_json::Value, serde_json::Error>,
);

/// Observe each independent document-validity channel without consuming earlier evidence.
#[cfg(feature = "serde")]
fn semantic_document(source: &str) -> Result<SemanticDocument, ParseFailure> {
  parse(source).map(|parsed| {
    let dom = parsed.clone().into_dom();
    let validation = dom.validate();
    let semantics = serde_json::to_value(&dom);
    (parsed, dom, validation, semantics)
  })
}

/// Require representative valid documents to preserve semantics through both rendering paths.
#[cfg(feature = "serde")]
#[test]
fn valid_corpus_formats_idempotently_and_round_trips_semantically() -> Result<(), impl Debug> {
  let fixture_round_trip = |source: String| {
    let initial = semantic_document(&source).map(|initial| {
      let formatted = formatter::format(&source, &Options::default()).map(|text| {
        let again = formatter::format(&text, &Options::default());
        let reparsed = semantic_document(&text);
        (text, again, reparsed)
      });
      let rendered = initial.1.to_toml(false, false).map(|text| {
        let reparsed = semantic_document(&text);
        (text, reparsed)
      });
      (initial, formatted, rendered)
    });
    (source, initial)
  };
  let observe = |path: PathBuf| {
    let document = read_fixture(&path).map(fixture_round_trip);
    (path, document)
  };
  let corpus = fixture_paths("valid").map(|paths| paths.into_iter().map(|pending| pending.map(observe)).collect::<Vec<_>>());
  ensure_that(
    corpus,
    "every valid corpus fixture must preserve validity, semantics, and formatting idempotence",
    |observed| {
      let Ok(ref fixtures) = *observed else {
        return false;
      };
      !fixtures.is_empty()
        && fixtures.iter().all(|fixture| {
          let &Ok((_, Ok((_, Ok(ref round_trip))))) = fixture else {
            return false;
          };
          let &(ref initial, Ok(ref formatting), Ok(ref rendering)) = round_trip else {
            return false;
          };
          let Ok(ref expected) = initial.3 else {
            return false;
          };
          let Ok(ref formatted) = formatting.2 else {
            return false;
          };
          let Ok(ref rendered) = rendering.1 else {
            return false;
          };
          initial.0.diagnostics().is_empty()
            && initial.2.is_ok()
            && formatting.1.as_ref().is_ok_and(|again| again == &formatting.0)
            && formatted.0.diagnostics().is_empty()
            && formatted.2.is_ok()
            && formatted.3.as_ref().is_ok_and(|actual| actual == expected)
            && rendered.0.diagnostics().is_empty()
            && rendered.2.is_ok()
            && rendered.3.as_ref().is_ok_and(|actual| actual == expected)
        })
    },
  )
  .map(drop)
  .map_err(Box::new)
}
