//! Profile lossless parsing and immutable DOM construction against the representative TOML corpus.

use std::hint::black_box;

use criterion::Criterion;
use criterion::criterion_group;
use criterion::criterion_main;
#[cfg(unix)]
use pprof::criterion::Output;
#[cfg(unix)]
use pprof::criterion::PProfProfiler;
use taplo::parser::Parse;
use taplo::parser::parse;

/// Register the lossless syntax-parsing benchmark and return its Criterion handle.
#[allow(
  clippy::single_call_fn,
  reason = "the named syntax benchmark is a distinct Criterion registration target for parser-only profiling"
)]
fn syntax(criterion: &mut Criterion) -> &mut Criterion {
  let source = include_str!("../../../test-data/example.toml");
  criterion.bench_function("parse-toml", |bencher| bencher.iter(|| parse(black_box(source))))
}

/// Register the parsing-plus-DOM benchmark and return its Criterion handle.
#[allow(
  clippy::single_call_fn,
  reason = "the named DOM benchmark is a distinct Criterion registration target for semantic-tree profiling"
)]
fn dom(criterion: &mut Criterion) -> &mut Criterion {
  let source = include_str!("../../../test-data/example.toml");
  criterion.bench_function("toml-dom", |bencher| bencher.iter(|| parse(black_box(source)).map(Parse::into_dom)))
}

#[cfg(unix)]
criterion_group!(
    name = benches;
    config = Criterion::default().with_profiler(PProfProfiler::new(100, Output::Flamegraph(None)));
    targets = syntax, dom
);

#[cfg(not(unix))]
criterion_group!(
    name = benches;
    config = Criterion::default();
    targets = syntax, dom
);

criterion_main!(benches);
