//! Typed rendering from immutable semantic DOM values into TOML source.
//!
//! Source-backed scalars retain their original spelling. Detached values use explicit quoting
//! and numeric policies, while malformed or semantically ambiguous nodes fail before emission.

use std::fmt::Display;
use std::fmt::Error as FormattingError;
use std::fmt::Write;

use thiserror::Error;

use super::Keys;
use super::Node;
use super::node::Array;
use super::node::ArrayKind;
use super::node::Bool;
use super::node::DateTime;
use super::node::Float;
use super::node::Integer;
use super::node::IntegerRepr;
use super::node::Invalid;
use super::node::InvalidReason;
use super::node::Str;
use super::node::Table;
use super::node::TableKind;
use crate::syntax::SyntaxElement;
use crate::util::escape;

/// A failure while rendering an immutable DOM as TOML.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum RenderError {
  /// A malformed source node has no valid semantic TOML representation.
  #[error("cannot render an invalid DOM node: {reason:?}")]
  InvalidNode {
    /// Typed reason the source node is invalid.
    reason: InvalidReason,
  },
  /// An integer representation is incompatible with its decoded signedness.
  #[error("cannot render a negative integer using {representation:?} notation")]
  NegativeNonDecimal {
    /// Requested non-decimal representation.
    representation: IntegerRepr,
  },
  /// Recoverable semantic diagnostics prevent an unambiguous TOML rendering.
  #[error("cannot render a DOM containing {count} semantic diagnostic(s)")]
  SemanticDiagnostics {
    /// Number of diagnostics found during the render preflight.
    count: usize,
  },
  /// The destination formatter rejected a write.
  #[error("the TOML destination rejected formatted output")]
  Formatting,
}

impl From<FormattingError> for RenderError {
  fn from(_: FormattingError) -> Self {
    Self::Formatting
  }
}

/// Render one validated semantic value into the supplied TOML destination.
pub(super) fn render(node: &Node, formatter: &mut impl Write, inline: bool, prefer_single_quote: bool) -> Result<(), RenderError> {
  ensure_renderable(node)?;
  render_tasks(formatter, node.clone(), inline, prefer_single_quote)
}

/// Reject malformed nodes and ambiguous semantic state before emitting bytes.
#[allow(
  clippy::single_call_fn,
  reason = "the named preflight keeps the reject-before-emit contract separate from the rendering program"
)]
fn ensure_renderable(root: &Node) -> Result<(), RenderError> {
  let mut diagnostic_count = 0_usize;
  let mut pending = vec![root.clone()];
  while let Some(node) = pending.pop() {
    if let Node::Invalid(ref invalid) = node {
      return Err(RenderError::InvalidNode {
        reason: invalid.reason().clone(),
      });
    }
    diagnostic_count = diagnostic_count.saturating_add(node.errors().len());
    match node {
      Node::Table(table) => {
        let key_diagnostic_count = table
          .entries()
          .iter()
          .map(|(key, _)| key.errors().len())
          .fold(0_usize, usize::saturating_add);
        diagnostic_count = diagnostic_count.saturating_add(key_diagnostic_count);
        pending.extend(table.entries().iter().rev().map(|(_, child)| child));
      }
      Node::Array(array) => pending.extend(array.items().iter().rev()),
      Node::Bool(_) | Node::Str(_) | Node::Integer(_) | Node::Float(_) | Node::Date(_) | Node::Invalid(_) => {}
    }
  }

  if diagnostic_count == 0 {
    Ok(())
  } else {
    Err(RenderError::SemanticDiagnostics {
      count: diagnostic_count
    })
  }
}

/// Orthogonal inline and header-emission choices for one rendering task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RenderMode {
  /// Whether containers use inline delimiters.
  inline:      bool,
  /// Whether a standard table emits its own header.
  emit_header: bool,
}

impl RenderMode {
  /// Inline container representation.
  const INLINE: Self = Self {
    inline:      true,
    emit_header: false,
  };
  /// Standard header-based container representation.
  const STANDARD: Self = Self {
    inline:      false,
    emit_header: true,
  };
  /// Standard table content whose owning array-table header was already emitted.
  const SUPPRESS_HEADER: Self = Self {
    inline:      false,
    emit_header: false,
  };
}

/// One owned instruction in the depth-independent TOML rendering program.
#[derive(Debug)]
enum RenderTask {
  /// Render one semantic node under the supplied path and representation.
  Node {
    /// Node to render.
    node: Node,
    /// Semantic path written before values or inside headers.
    keys: Keys,
    /// Composite representation selected for this node.
    mode: RenderMode,
  },
  /// Write one static separator or delimiter.
  Static(&'static str),
  /// Write one owned header.
  Header(String),
}

/// Execute a depth-independent TOML rendering program.
#[allow(
  clippy::single_call_fn,
  reason = "the named driver owns the depth-independent task loop distinct from the renderability preflight"
)]
fn render_tasks(formatter: &mut impl Write, root: Node, inline: bool, prefer_single_quote: bool) -> Result<(), RenderError> {
  let root_mode = if inline {
    RenderMode::INLINE
  } else {
    RenderMode::STANDARD
  };
  let mut pending = vec![RenderTask::Node {
    node: root,
    keys: Keys::empty(),
    mode: root_mode,
  }];
  while let Some(task) = pending.pop() {
    match task {
      RenderTask::Node {
        node,
        keys,
        mode: node_mode,
      } => queue_or_render_node(formatter, node, &keys, node_mode, prefer_single_quote, &mut pending)?,
      RenderTask::Static(text) => formatter.write_str(text)?,
      RenderTask::Header(header) => formatter.write_str(&header)?,
    }
  }
  Ok(())
}

/// Render a scalar immediately or queue one composite node's children in reverse output order.
#[allow(
  clippy::single_call_fn,
  reason = "the named dispatcher is the single node-variant routing point of the rendering program"
)]
fn queue_or_render_node(
  formatter: &mut impl Write,
  node: Node,
  keys: &Keys,
  mode: RenderMode,
  prefer_single_quote: bool,
  pending: &mut Vec<RenderTask>,
) -> Result<(), RenderError> {
  match node {
    Node::Table(table) => queue_table(formatter, &table, keys, mode, pending),
    Node::Array(array) => queue_array(formatter, &array, keys, mode, pending),
    Node::Bool(boolean) => render_bool(formatter, &boolean, keys),
    Node::Str(string) => {
      write_assignment(formatter, keys)?;
      render_string(formatter, &string, prefer_single_quote)
    }
    Node::Integer(integer) => render_integer(formatter, &integer, keys),
    Node::Float(float) => render_float(formatter, &float, keys),
    Node::Date(date_time) => render_date(formatter, &date_time, keys),
    Node::Invalid(invalid) => render_invalid(&invalid),
  }
}

/// Render one Boolean from retained syntax or its semantic value.
#[allow(
  clippy::single_call_fn,
  reason = "the named renderer keeps Boolean source-spelling preservation beside its sibling scalar renderers"
)]
fn render_bool(formatter: &mut impl Write, boolean: &Bool, keys: &Keys) -> Result<(), RenderError> {
  write_assignment(formatter, keys)?;
  render_source_or_value(formatter, boolean.syntax(), boolean.value())?;
  Ok(())
}

/// Render one integer with source-spelling and representation preservation.
#[allow(
  clippy::single_call_fn,
  reason = "the named renderer keeps integer representation and signedness policy beside its sibling renderers"
)]
fn render_integer(formatter: &mut impl Write, integer: &Integer, keys: &Keys) -> Result<(), RenderError> {
  write_assignment(formatter, keys)?;
  if let Some(syntax) = integer.syntax() {
    write!(formatter, "{syntax}")?;
    return Ok(());
  }
  match integer.representation() {
    IntegerRepr::Dec => write!(formatter, "{}", integer.value())?,
    representation @ (IntegerRepr::Bin | IntegerRepr::Oct | IntegerRepr::Hex) => {
      let Some(unsigned) = integer.value().as_positive() else {
        return Err(RenderError::NegativeNonDecimal {
          representation,
        });
      };
      match representation {
        IntegerRepr::Bin => write!(formatter, "{unsigned:#b}")?,
        IntegerRepr::Oct => write!(formatter, "{unsigned:#o}")?,
        IntegerRepr::Hex => write!(formatter, "{unsigned:#X}")?,
        IntegerRepr::Dec => {}
      }
    }
  }
  Ok(())
}

/// Render one float from retained syntax or its semantic value.
#[allow(
  clippy::single_call_fn,
  reason = "the named renderer keeps float source-spelling preservation beside its sibling scalar renderers"
)]
fn render_float(formatter: &mut impl Write, float: &Float, keys: &Keys) -> Result<(), RenderError> {
  write_assignment(formatter, keys)?;
  if let Some(syntax) = float.syntax() {
    write!(formatter, "{syntax}")?;
  } else {
    write_float(formatter, float.value())?;
  }
  Ok(())
}

/// Render one date/time from retained syntax or its semantic value.
#[allow(
  clippy::single_call_fn,
  reason = "the named renderer keeps date/time source-spelling preservation beside its sibling scalar renderers"
)]
fn render_date(formatter: &mut impl Write, date_time: &DateTime, keys: &Keys) -> Result<(), RenderError> {
  write_assignment(formatter, keys)?;
  render_source_or_value(formatter, date_time.syntax(), date_time.value())?;
  Ok(())
}

/// Render retained source spelling when present, otherwise a semantic scalar value.
fn render_source_or_value(formatter: &mut impl Write, syntax: Option<&SyntaxElement>, scalar: impl Display) -> Result<(), FormattingError> {
  match syntax {
    Some(source) => write!(formatter, "{source}"),
    None => write!(formatter, "{scalar}"),
  }
}

/// Reject an invalid semantic node without fabricating TOML.
#[allow(
  clippy::single_call_fn,
  reason = "the named renderer keeps malformed-node rejection on the same dispatch table as every emitting renderer"
)]
fn render_invalid(invalid: &Invalid) -> Result<(), RenderError> {
  Err(RenderError::InvalidNode {
    reason: invalid.reason().clone(),
  })
}

/// Write a non-empty assignment path.
fn write_assignment(formatter: &mut impl Write, keys: &Keys) -> Result<(), FormattingError> {
  if !keys.is_empty() {
    formatter.write_str(keys.dotted())?;
    formatter.write_str(" = ")?;
  }
  Ok(())
}

/// Queue inline children in source order with separators between them.
fn queue_inline_children(pending: &mut Vec<RenderTask>, children: impl DoubleEndedIterator<Item = (Node, Keys)> + ExactSizeIterator) {
  for (index, (node, keys)) in children.enumerate().rev() {
    pending.push(RenderTask::Node {
      node,
      keys,
      mode: RenderMode::INLINE,
    });
    if index > 0 {
      pending.push(RenderTask::Static(", "));
    }
  }
}

/// Queue one inline or header-based table.
#[allow(
  clippy::single_call_fn,
  reason = "the named queuer owns inline-versus-header table layout and scalar/composite entry ordering"
)]
fn queue_table(
  formatter: &mut impl Write,
  table: &Table,
  keys: &Keys,
  mode: RenderMode,
  pending: &mut Vec<RenderTask>,
) -> Result<(), RenderError> {
  let entries = table.entries();
  if table.kind() == TableKind::Inline || mode.inline {
    write_assignment(formatter, keys)?;
    formatter.write_str("{ ")?;
    pending.push(RenderTask::Static(" }"));
    queue_inline_children(pending, entries.iter().map(|(key, child)| (child, Keys::from(key))));
    return Ok(());
  }

  if !keys.is_empty() && mode.emit_header {
    formatter.write_char('[')?;
    formatter.write_str(keys.dotted())?;
    formatter.write_str("]\n")?;
  }
  let mut scalar_entries = Vec::new();
  let mut composite_entries = Vec::new();
  for (key, child) in &entries {
    let header_based = child
      .as_table()
      .is_some_and(|child_table| child_table.kind() != TableKind::Inline)
      || child
        .as_array()
        .is_some_and(|child_array| child_array.kind() == ArrayKind::Tables);
    if header_based {
      composite_entries.push((key, child));
    } else {
      scalar_entries.push((key, child));
    }
  }
  for (key, child) in composite_entries.into_iter().rev() {
    pending.push(RenderTask::Node {
      node: child,
      keys: keys.join(key),
      mode: RenderMode::STANDARD,
    });
  }
  for (key, child) in scalar_entries.into_iter().rev() {
    pending.push(RenderTask::Static("\n"));
    pending.push(RenderTask::Node {
      node: child,
      keys: Keys::from(key),
      mode: RenderMode::STANDARD,
    });
  }
  Ok(())
}

/// Queue one inline array or array-of-tables.
#[allow(
  clippy::single_call_fn,
  reason = "the named queuer owns inline-versus-array-of-tables layout and per-element header emission"
)]
fn queue_array(
  formatter: &mut impl Write,
  array: &Array,
  keys: &Keys,
  mode: RenderMode,
  pending: &mut Vec<RenderTask>,
) -> Result<(), RenderError> {
  let items = array.items();
  if array.kind() == ArrayKind::Inline || mode.inline {
    write_assignment(formatter, keys)?;
    formatter.write_str("[ ")?;
    pending.push(RenderTask::Static(" ]"));
    queue_inline_children(pending, items.iter().map(|child| (child, Keys::empty())));
    return Ok(());
  }

  let header = format!("[[{}]]\n", keys.dotted());
  for child in items.iter().rev() {
    pending.push(RenderTask::Node {
      node: child,
      keys: keys.clone(),
      mode: RenderMode::SUPPRESS_HEADER,
    });
    pending.push(RenderTask::Header(header.clone()));
  }
  Ok(())
}

/// Render one string from retained syntax or detached quoting policy.
#[allow(
  clippy::single_call_fn,
  reason = "the helper keeps source-spelling preservation and detached quote selection at the string emission boundary"
)]
fn render_string(formatter: &mut impl Write, string: &Str, prefer_single_quote: bool) -> Result<(), RenderError> {
  if let Some(syntax) = string.syntax() {
    write!(formatter, "{syntax}")?;
    return Ok(());
  }

  let escaped = escape(string.value());
  if prefer_single_quote && escaped == string.value() && !string.value().contains('\'') {
    write!(formatter, "'{}'", string.value())?;
  } else {
    write!(formatter, r#""{escaped}""#)?;
  }
  Ok(())
}

/// Write one detached floating-point value using TOML-preserving syntax.
#[allow(
  clippy::single_call_fn,
  reason = "the named scalar writer preserves TOML spellings for integral finite values, infinities, and signed NaN"
)]
fn write_float(formatter: &mut impl Write, number: f64) -> Result<(), FormattingError> {
  if number.is_nan() {
    return formatter.write_str(if number.is_sign_negative() { "-nan" } else { "nan" });
  }
  if number.is_infinite() {
    return formatter.write_str(if number.is_sign_negative() { "-inf" } else { "inf" });
  }

  let rendered = number.to_string();
  formatter.write_str(&rendered)?;
  if !rendered.contains('.') && !rendered.contains('e') && !rendered.contains('E') {
    formatter.write_str(".0")?;
  }
  Ok(())
}

#[cfg(test)]
/// Rendering contracts for source-backed, detached, invalid, and destination-failure cases.
mod tests {
  use std::fmt::Error as FormattingError;
  use std::fmt::Result as FormattingResult;
  use std::fmt::Write;
  use std::sync::Arc;

  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;

  use super::RenderError;
  use crate::dom::Keys;
  use crate::dom::Node;
  use crate::dom::node::FloatInner;
  use crate::dom::node::IntegerInner;
  use crate::dom::node::IntegerRepr;
  use crate::dom::node::IntegerValue;
  use crate::dom::node::StrInner;
  use crate::parser::parse;

  /// Parse one syntax-clean DOM fixture.
  fn clean_dom(source: &str) -> Result<Node, TestFailure> {
    let parsed = ensure_ok(parse(source), "the renderer fixture tree must construct")?;
    ensure(parsed.diagnostics().is_empty(), "the renderer fixture syntax must be clean")?;
    Ok(parsed.into_dom())
  }

  /// Resolve the source-backed scalar targeted by a renderer behavior test.
  #[allow(
    clippy::single_call_fn,
    reason = "the helper keeps fixture-path parsing and absence reporting out of the quote-preservation assertion"
  )]
  fn path(root: &Node, dotted: &str) -> Result<Node, TestFailure> {
    let keys = ensure_ok(dotted.parse::<Keys>(), "the renderer fixture path must parse")?;
    ensure_some(root.path(&keys), "the renderer fixture path must exist")
  }

  /// Preserve the exact quoting form retained by a source-backed scalar.
  #[test]
  fn source_backed_scalar_preserves_its_representation() -> Result<(), TestFailure> {
    let root = clean_dom("value = 'literal'\n")?;
    let rendered = ensure_ok(path(&root, "value")?.to_toml(true, false), "a valid scalar must render")?;
    ensure_eq(
      &rendered,
      &"'literal'".to_owned(),
      "a source-backed scalar must preserve its exact quote representation",
    )
  }

  /// Render and destroy a deeply nested inline table without consuming call-stack depth.
  #[test]
  fn deeply_nested_inline_table_renders_and_drops_iteratively() -> Result<(), TestFailure> {
    let shallow_source = "value = { nested = 0 }\n";
    let shallow = clean_dom(shallow_source)?;
    ensure_eq(
      &ensure_ok(shallow.to_toml(false, false), "a shallow inline table must render")?,
      &shallow_source.to_owned(),
      "the iterative renderer must retain the ordinary inline-table representation",
    )?;

    let depth = 10_000;
    let source = format!("value = {}0{}\n", "{ nested = ".repeat(depth), " }".repeat(depth));
    let root = clean_dom(&source)?;
    let rendered = ensure_ok(
      root.to_toml(false, false),
      "a deeply nested semantic tree must render without recursive calls",
    )?;
    ensure(
      rendered == source,
      "depth-independent rendering must preserve the complete nested inline-table value",
    )
  }

  /// Keep inline-table assignments local and separated inside consecutive array-table elements.
  #[test]
  fn array_table_inline_children_render_as_local_assignments() -> Result<(), TestFailure> {
    let source = "\
[[products]]
name = \"hammer\"
dimensions = { length = 12, width = 4 }

[[products]]
name = \"nail\"
dimensions = { length = 2, width = 1 }
";
    let expected = "\
[[products]]
name = \"hammer\"
dimensions = { length = 12, width = 4 }
[[products]]
name = \"nail\"
dimensions = { length = 2, width = 1 }
";
    let rendered = ensure_ok(
      clean_dom(source)?.to_toml(false, false),
      "array-table elements with inline children must render",
    )?;
    ensure_eq(
      &rendered,
      &expected.to_owned(),
      "inline children must use local assignment keys with a terminating newline",
    )?;
    let reparsed = ensure_ok(parse(&rendered), "rendered consecutive array-table elements must reparse")?;
    ensure(
      [reparsed.diagnostics().is_empty(), reparsed.into_dom().validate().is_ok()] == [true, true],
      "rendered consecutive array-table elements must remain valid TOML",
    )
  }

  /// Keep a child usable after the caller releases its original root handle.
  #[test]
  fn child_handle_survives_root_drop() -> Result<(), TestFailure> {
    let root = clean_dom("parent = { child = 7 }\n")?;
    let child = path(&root, "parent.child")?;
    drop(root);
    let rendered = ensure_ok(
      child.to_toml(true, false),
      "a child handle must retain the immutable arena after its root is released",
    )?;
    ensure_eq(
      &rendered,
      &"7".to_owned(),
      "a surviving child handle must retain its decoded value and source spelling",
    )
  }

  /// Select literal or basic quoting for detached strings without producing invalid TOML.
  #[test]
  fn detached_string_uses_only_valid_quote_forms() -> Result<(), TestFailure> {
    let string = |value: &str| {
      Node::from(StrInner {
        diagnostics: Arc::default(),
        syntax:      None,
        value:       Arc::from(value),
      })
    };

    ensure_eq(
      &ensure_ok(string("simple").to_toml(true, true), "a simple detached string must render")?,
      &"'simple'".to_owned(),
      "single quotes may be preferred when the literal remains valid",
    )?;
    ensure_eq(
      &ensure_ok(
        string("can't").to_toml(true, true),
        "an apostrophe-containing detached string must render",
      )?,
      &r#""can't""#.to_owned(),
      "an apostrophe must force basic-string quoting",
    )
  }

  /// Render detached finite and special floats with unambiguous TOML floating-point syntax.
  #[test]
  fn detached_floats_retain_float_syntax() -> Result<(), TestFailure> {
    let float = |value| {
      Node::from(FloatInner {
        diagnostics: Arc::default(),
        syntax: None,
        value,
      })
    };

    for (value, expected) in [
      (1.0, "1.0"),
      (-0.0, "-0.0"),
      (f64::INFINITY, "inf"),
      (f64::NEG_INFINITY, "-inf"),
      (f64::NAN, "nan"),
    ] {
      ensure_eq(
        &ensure_ok(float(value).to_toml(true, false), "a detached float must render")?,
        &expected.to_owned(),
        "detached floating-point values must retain TOML floating-point syntax",
      )?;
    }
    Ok(())
  }

  /// Surface malformed nodes, incompatible integer representations, and conflicts as typed errors.
  #[test]
  fn malformed_source_and_unsupported_integer_are_typed_errors() -> Result<(), TestFailure> {
    let invalid = clean_dom("value = 999999999999999999999999999999\n")?;
    ensure(
      matches!(invalid.to_toml(false, false), Err(RenderError::InvalidNode { .. })),
      "malformed source must not be omitted or rendered with a fabricated value",
    )?;

    let negative_hex = Node::from(IntegerInner {
      diagnostics: Arc::default(),
      syntax:      None,
      repr:        IntegerRepr::Hex,
      value:       IntegerValue::Negative(-1),
    });
    ensure(
      matches!(
        negative_hex.to_toml(true, false),
        Err(RenderError::NegativeNonDecimal {
          representation: IntegerRepr::Hex,
        })
      ),
      "a negative detached non-decimal integer must be rejected explicitly",
    )?;

    let conflicting = clean_dom("value = 1\nvalue = 2\n")?;
    ensure(
      matches!(
        conflicting.to_toml(false, false),
        Err(RenderError::SemanticDiagnostics {
          count
        }) if count > 0
      ),
      "ambiguous semantic state must be rejected before TOML emission",
    )
  }

  /// Preserve a destination writer failure as the renderer's typed formatting error.
  #[test]
  fn destination_failure_is_not_erased() -> Result<(), TestFailure> {
    /// Writer fixture that rejects every attempted output byte.
    struct RejectingWriter;

    impl Write for RejectingWriter {
      fn write_str(&mut self, _: &str) -> FormattingResult {
        Err(FormattingError)
      }
    }

    let root = clean_dom("value = true\n")?;
    ensure(
      matches!(root.to_toml_fmt(&mut RejectingWriter, false, false), Err(RenderError::Formatting)),
      "the caller must receive a typed destination failure",
    )
  }
}
