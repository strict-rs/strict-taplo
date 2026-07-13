use std::cmp::Reverse;
use std::ops::Range;
use std::sync::Arc;

use rowan::TextRange;
use thiserror::Error;

use super::Keys;
use super::node::DomNode;
use super::node::Node;
use crate::dom;
use crate::syntax::SyntaxKind;

#[derive(Debug)]
pub struct Rewrite {
  root:    Node,
  source:  String,
  patches: Vec<PendingPatch>,
}

impl Rewrite {
  pub fn new(root: Node) -> Result<Self, Error> {
    let Some(syntax) = root.syntax().and_then(|syntax| syntax.as_node()) else {
      return Err(Error::RootNodeExpected);
    };
    if syntax.kind() != SyntaxKind::ROOT {
      return Err(Error::RootNodeExpected);
    }
    let source = syntax.to_string();

    Ok(Self {
      root,
      source,
      patches: Default::default(),
    })
  }

  pub fn add(&mut self, patch: impl Into<Patch>) -> Result<&mut Self, Error> {
    let patch = patch.into();
    match patch {
      Patch::RenameKeys {
        key,
        to,
      } => {
        let keys = key.parse::<Keys>()?;
        let nodes = self.root.find_all_matches(keys, false)?;

        for (keys, _) in nodes {
          let key = match keys.iter().last().cloned() {
            Some(dom::KeyOrIndex::Key(k)) => k,
            _ => continue,
          };

          for range in key.text_ranges() {
            self.check_overlap(range)?;

            self.patches.push(PendingPatch {
              range,
              kind: PendingPatchKind::Replace(to.clone()),
            })
          }
        }
      }
    }

    self.patches.sort_by_key(|patch| Reverse(patch.range.start()));

    Ok(self)
  }

  pub fn patches(&self) -> &[PendingPatch] {
    &self.patches
  }

  fn check_overlap(&self, range: TextRange) -> Result<(), Error> {
    for patch in self.patches() {
      if patch.range.contains_range(range)
        || range.contains_range(patch.range)
        || patch.range.contains(range.start())
        || patch.range.contains(range.end())
      {
        return Err(Error::Overlap);
      }
    }

    Ok(())
  }
}

impl Rewrite {
  pub fn rename_keys(&mut self, key: &str, to: &str) -> Result<&mut Self, Error> {
    self.add(Patch::RenameKeys {
      key: key.into(),
      to:  to.into(),
    })
  }
}

impl core::fmt::Display for Rewrite {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let mut s = self.source.clone();

    for patch in &self.patches {
      match &patch.kind {
        PendingPatchKind::Replace(to) => {
          let Some(range) = std_range(patch.range) else {
            return Err(std::fmt::Error);
          };
          s.replace_range(range, to);
        }
      }
    }

    s.fmt(f)
  }
}

#[derive(Debug)]
pub enum Patch {
  RenameKeys { key: Arc<str>, to: Arc<str> },
}

#[derive(Debug)]
pub struct PendingPatch {
  pub range: TextRange,
  pub kind:  PendingPatchKind,
}

#[derive(Debug)]
#[non_exhaustive]
pub enum PendingPatchKind {
  Replace(Arc<str>),
}

#[derive(Debug, Error)]
pub enum Error {
  #[error("only the root node can be patched")]
  RootNodeExpected,
  #[error("expected table")]
  ExpectedTable,
  #[error("new patches would overlap with existing ones")]
  Overlap,
  #[error("{0}")]
  Dom(#[from] dom::error::Error),
}

fn std_range(range: TextRange) -> Option<Range<usize>> {
  let start = usize::try_from(u32::from(range.start())).ok()?;
  let end = usize::try_from(u32::from(range.end())).ok()?;
  Some(start..end)
}

#[cfg(test)]
mod tests {
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;

  use super::Error;
  use super::Rewrite;
  use crate::parser::parse;

  fn rewrite(source: &str) -> Result<Rewrite, TestFailure> {
    let parsed = parse(source);
    ensure(parsed.errors.is_empty(), "the rewrite fixture must parse cleanly")?;
    ensure_ok(Rewrite::new(parsed.into_dom()), "a parsed document root must be rewriteable")
  }

  #[test]
  fn rename_keys() -> Result<(), TestFailure> {
    let toml = r#"
[table.middle.inner]
[table.middle.inner.inner]
"#;

    let expected_toml = r#"
[table_new.middle_new.inner_new]
[table_new.middle_new.inner_new.inner2_new]
"#;

    let mut patches = rewrite(toml)?;

    ensure_ok(patches.rename_keys("table", "table_new"), "the outer key must be renameable")?;
    ensure_ok(
      patches.rename_keys("table.middle", "middle_new"),
      "the middle key must be renameable",
    )?;
    ensure_ok(
      patches.rename_keys("table.middle.inner", "inner_new"),
      "the inner key must be renameable",
    )?;
    ensure_ok(
      patches.rename_keys("table.middle.inner.inner", "inner2_new"),
      "the deepest key must be renameable",
    )?;

    let rendered = patches.to_string();
    ensure_eq(
      &rendered.as_str(),
      &expected_toml,
      "different-length replacements must render in descending source order",
    )
  }

  #[test]
  fn rename_keys_array_of_tables() -> Result<(), TestFailure> {
    let toml = r#"
[[table.middle.inner]]
[[table.middle.inner]]
[table.middle.inner.inner]
"#;

    let expected_toml = r#"
[[table_new.middle_new.inner_new]]
[[table_new.middle_new.inner_new]]
[table_new.middle_new.inner_new.inner2_new]
"#;

    let mut patches = rewrite(toml)?;

    ensure_ok(
      patches.rename_keys("table", "table_new"),
      "the array-table root key must be renameable",
    )?;
    ensure_ok(
      patches.rename_keys("table.middle", "middle_new"),
      "the array-table middle key must be renameable",
    )?;
    ensure_ok(
      patches.rename_keys("table.middle.inner", "inner_new"),
      "the array-table inner key must be renameable",
    )?;
    ensure_ok(
      patches.rename_keys("table.middle.inner.*.inner", "inner2_new"),
      "the nested array-table key must be renameable",
    )?;

    let rendered = patches.to_string();
    ensure_eq(
      &rendered.as_str(),
      &expected_toml,
      "all matching array-table ranges must be rewritten",
    )
  }

  #[test]
  fn overlapping_replacements_are_rejected() -> Result<(), TestFailure> {
    let mut patches = rewrite("[table]\nvalue = 1\n")?;
    ensure_ok(patches.rename_keys("table", "first"), "the first replacement must be accepted")?;
    ensure(
      matches!(patches.rename_keys("table", "second"), Err(Error::Overlap)),
      "a second replacement over the same source range must be rejected",
    )
  }

  #[test]
  fn non_root_nodes_are_rejected() -> Result<(), TestFailure> {
    let parsed = parse("value = 1");
    ensure(parsed.errors.is_empty(), "the non-root fixture must parse cleanly")?;
    let value = ensure_ok(parsed.into_dom().try_get("value"), "the fixture value must exist")?;
    ensure(
      matches!(Rewrite::new(value), Err(Error::RootNodeExpected)),
      "only a root syntax node may own a rewrite",
    )
  }
}
