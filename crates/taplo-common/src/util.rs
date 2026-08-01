use std::borrow::Cow;
use std::hash::Hash;
use std::hash::Hasher;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use globset::Glob;
use globset::GlobSet;
use percent_encoding::percent_decode_str;
use serde_json::Value;
use thiserror::Error;

/// A failure while compiling an include/exclude glob rule.
#[derive(Debug, Error)]
pub enum GlobRuleError {
  /// One glob expression or the resulting set was invalid.
  #[error("invalid glob rule")]
  InvalidGlob(#[from] globset::Error),
}

/// Compiled include/exclude path matching rules.
#[derive(Debug, Clone)]
pub struct GlobRule {
  /// Expressions a path must match to be included.
  include: GlobSet,
  /// Expressions that remove an otherwise included path.
  exclude: GlobSet,
}

/// Compile one set of glob expressions into a matcher.
///
/// An empty expression set yields a matcher that matches nothing.
fn compile_globs(patterns: impl IntoIterator<Item = impl AsRef<str>>) -> Result<GlobSet, globset::Error> {
  let globs = patterns
    .into_iter()
    .map(|pattern| Glob::new(pattern.as_ref()))
    .collect::<Result<Vec<_>, _>>()?;
  GlobSet::new(globs)
}

impl GlobRule {
  /// Compile one include set and one exclude set into a matching rule.
  ///
  /// An empty include set matches nothing; an empty exclude set removes nothing.
  ///
  /// # Errors
  ///
  /// Returns [`GlobRuleError`] when an expression is not a valid glob or the resulting set
  /// cannot be built.
  pub fn new(
    include: impl IntoIterator<Item = impl AsRef<str>>,
    exclude: impl IntoIterator<Item = impl AsRef<str>>,
  ) -> Result<Self, GlobRuleError> {
    Ok(Self {
      include: compile_globs(include)?,
      exclude: compile_globs(exclude)?,
    })
  }

  /// Return whether a path is included by this rule.
  ///
  /// A path matches when it matches the include set and no exclude expression, so excludes
  /// always win over includes.
  #[must_use]
  pub fn is_match(&self, text: impl AsRef<Path>) -> bool {
    if !self.include.is_match(text.as_ref()) {
      return false;
    }

    !self.exclude.is_match(text.as_ref())
  }
}

/// A shared JSON value usable as a hash-map key or deduplication key.
///
/// [`serde_json::Value`] is not [`Hash`], so schema deduplication wraps it here; hashing is
/// delegated to [`HashValue`] and equality to the underlying value.
#[derive(Debug, Eq)]
pub struct ArcHashValue(pub Arc<Value>);

impl Hash for ArcHashValue {
  fn hash<H: Hasher>(&self, state: &mut H) {
    HashValue(&self.0).hash(state);
  }
}

impl PartialEq for ArcHashValue {
  fn eq(&self, other: &Self) -> bool {
    self.0 == other.0
  }
}

/// A borrowed JSON value that hashes structurally.
///
/// Objects hash their entries in iteration order, so two structurally equal values only hash
/// alike when their key order agrees — which holds for values produced by the same decoder.
#[derive(Debug, Eq)]
pub struct HashValue<'v>(pub &'v Value);

impl PartialEq for HashValue<'_> {
  fn eq(&self, other: &Self) -> bool {
    self.0 == other.0
  }
}

impl Hash for HashValue<'_> {
  fn hash<H: Hasher>(&self, state: &mut H) {
    match *self.0 {
      Value::Null => 0.hash(state),
      Value::Bool(ref boolean) => boolean.hash(state),
      Value::Number(ref number) => number.hash(state),
      Value::String(ref string) => string.hash(state),
      Value::Array(ref array) => {
        for element in array {
          HashValue(element).hash(state);
        }
      }
      Value::Object(ref object) => {
        for (key, element) in object {
          key.hash(state);
          HashValue(element).hash(state);
        }
      }
    }
  }
}

/// Conversion of a host path into Taplo's comparable path model.
///
/// Configuration globs, schema associations, and cache keys are all compared as text, so every
/// path that reaches those comparisons is normalized first.
pub trait Normalize {
  /// Normalizing in the context of Taplo the following:
  ///
  /// - replaces `\` with `/` on windows
  /// - decodes all percent-encoded characters
  #[must_use]
  fn normalize(self) -> Self;
}

impl Normalize for PathBuf {
  fn normalize(self) -> Self {
    self.to_str().map(|source| (*normalize_str(source)).into()).unwrap_or(self)
  }
}

/// Normalize percent escapes and platform separators in one path-like string.
#[allow(
  clippy::single_call_fn,
  reason = "string normalization keeps percent decoding and platform separator rules reusable at the path normalization boundary"
)]
pub(crate) fn normalize_str(source: &str) -> Cow<'_, str> {
  let Some(percent_decoded) = percent_decode_str(source).decode_utf8().ok() else {
    return source.into();
  };

  if cfg!(windows) {
    percent_decoded.replace('\\', "/").into()
  } else {
    percent_decoded
  }
}
