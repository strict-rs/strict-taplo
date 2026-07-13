use std::hint::black_box;

use criterion::Criterion;
use criterion::criterion_group;
use criterion::criterion_main;
#[cfg(unix)]
use pprof::criterion::Output;
#[cfg(unix)]
use pprof::criterion::PProfProfiler;
use taplo::parser::parse;

pub fn syntax(c: &mut Criterion) {
  let source = include_str!("../../../test-data/example.toml");
  c.bench_function("parse-toml", |b| b.iter(|| parse(black_box(source))));
}

pub fn dom(c: &mut Criterion) {
  let source = include_str!("../../../test-data/example.toml");
  c.bench_function("toml-dom", |b| b.iter(|| parse(black_box(source)).into_dom()));
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
