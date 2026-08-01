//! Compare Taplo parsing, formatting, validation, and optional Serde conversion against
//! representative TOML input.

use std::hint::black_box;

use criterion::Criterion;
use criterion::criterion_group;
use criterion::criterion_main;
#[cfg(feature = "serde")]
use taplo::dom::Node;
use taplo::formatter::Options;
use taplo::formatter::format;
use taplo::formatter::format_syntax;
use taplo::parser::Parse;
use taplo::parser::parse;

/// Register parser, DOM construction, validation, and comparison benchmarks.
#[allow(
  clippy::single_call_fn,
  reason = "the parsing benchmark family remains a named Criterion target grouping comparable parse workloads"
)]
fn parsing(criterion: &mut Criterion) -> &mut Criterion {
  let source = include_str!("../../../test-data/example.toml");
  criterion
    .bench_function("parse taplo syntax", |bencher| bencher.iter(|| parse(black_box(source))))
    .bench_function("parse taplo dom", |bencher| {
      bencher.iter(|| parse(black_box(source)).map(Parse::into_dom));
    })
    .bench_function("parse taplo dom and validate", |bencher| {
      bencher.iter(|| parse(black_box(source)).map(Parse::into_dom).map(|root| root.validate()));
    })
    .bench_function("parse toml-rs", |bencher| {
      bencher.iter(|| toml::from_str::<toml::Value>(black_box(source)));
    })
}

/// Register formatting benchmarks for retained syntax and raw source input.
#[allow(
  clippy::single_call_fn,
  reason = "the formatting benchmark family remains a named Criterion target comparing retained-syntax and source entry points"
)]
fn formatting(criterion: &mut Criterion) -> &mut Criterion {
  let source = include_str!("../../../test-data/example.toml");

  let syntax = parse(source).map(Parse::into_syntax);
  criterion
    .bench_function("format syntax", |bencher| {
      bencher.iter(|| {
        syntax
          .as_ref()
          .map(|node| format_syntax(&black_box(node.clone()), &Options::default()))
      });
    })
    .bench_function("parse and format", |bencher| {
      bencher.iter(|| format(black_box(source), &Options::default()));
    })
}

#[cfg(feature = "serde")]
/// Register JSON-to-DOM conversion followed by detached TOML rendering.
#[allow(
  clippy::single_call_fn,
  reason = "the conversion benchmark remains a named Criterion target for the optional Serde-to-DOM pipeline"
)]
fn conversion(criterion: &mut Criterion) -> &mut Criterion {
  let source = include_str!("../../../test-data/example.toml");
  let parsed_json = toml::from_str::<serde_json::Value>(source).map_err(|error| error.to_string());

  criterion.bench_function("convert from JSON", |bencher| {
    bencher.iter(|| {
      parsed_json.as_ref().map_err(Clone::clone).and_then(|json_document| {
        serde_json::from_value::<Node>(black_box(json_document.clone()))
          .map_err(|error| error.to_string())
          .and_then(|node| node.to_toml(false, false).map_err(|error| error.to_string()))
      })
    });
  })
}

#[cfg(feature = "serde")]
criterion_group!(benches, parsing, formatting, conversion);
#[cfg(not(feature = "serde"))]
criterion_group!(benches, parsing, formatting);
criterion_main!(benches);
