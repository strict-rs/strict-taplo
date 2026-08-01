//! Modern nested document-symbol request handling.

use lsp_types::DocumentSymbolParams;
use lsp_types::Range;
use lsp_types::SymbolKind;
use taplo::dom::Node;
use taplo::rowan::TextRange;
use taplo::util::try_join_ranges;
use taplo_lsp_async::Params;
use taplo_lsp_async::rpc::RpcError;
use taplo_lsp_async::util::MappingError;

use crate::lsp_ext::request::ModernDocumentSymbol;
use crate::world::DocumentState;
use crate::world::WorldState;

/// Resolve modern nested symbols for one open document.
///
/// # Errors
///
/// Returns a JSON-RPC error when parameters are missing, source coordinates
/// cannot be mapped, or the captured document generation becomes stale.
macro_rules! define_document_symbol_future_family {
  (
    $document_symbols:ident,
    $document_snapshot_for_uri:ident,
    $ensure_current_snapshot:ident;
    $future:ident,
    $environment:path,
    $transport:ident,
    $schema_execution:path;
    [$($value_bound:path),*]
  ) => {
    /// Resolve modern nested symbols for one open document.
    ///
    /// # Errors
    ///
    /// Returns a JSON-RPC error when parameters are missing, source coordinates cannot be mapped,
    /// or the captured document generation becomes stale.
    pub(super) fn $document_symbols<E: $environment>(
      world: &WorldState<E, $transport<E>>,
      params: Params<DocumentSymbolParams>,
    ) -> $future<'_, Result<Option<Vec<ModernDocumentSymbol>>, RpcError>> {
      Box::pin(async move {
        let parameters = params.required()?;

        current_document_snapshot!(
          super::$document_snapshot_for_uri(world, &parameters.text_document.uri) => (document_uri, snapshot)
        );

        let symbols = create_symbols(&snapshot.document).map_err(|error| super::uri::mapping_rpc_error(&error))?;
        super::$ensure_current_snapshot(world, &document_uri, &snapshot).await?;
        Ok(Some(symbols))
      })
    }
  };
}

define_checked_document_handler_execution_families!(
  define_document_symbol_future_family;
  (document_symbols_local, document_symbols_concurrent),
);

/// Build modern nested symbols from one immutable document state.
///
/// # Errors
///
/// Returns [`MappingError`] when a semantic source range cannot be represented
/// by the document's negotiated LSP coordinate model.
fn create_symbols(doc: &DocumentState) -> Result<Vec<ModernDocumentSymbol>, MappingError> {
  let dom = doc.dom.clone();
  let Some(root_table) = dom.as_table() else {
    return Ok(Vec::new());
  };
  let pending = root_table
    .entries()
    .iter()
    .rev()
    .map(|(key, entry)| SymbolTask::Visit {
      name:      ensure_non_empty_key(key.value().to_owned()),
      key_range: key.text_ranges().next(),
      node:      entry,
    })
    .collect::<Vec<_>>();
  SymbolCollector::new(doc, pending).collect()
}

/// Owns the depth-independent traversal and output assembly for one document.
struct SymbolCollector<'document> {
  /// Immutable document that owns the source-to-LSP coordinate mapper.
  document: &'document DocumentState,
  /// Completed symbols in traversal order.
  symbols:  Vec<ModernDocumentSymbol>,
  /// Pending visit and composite-assembly tasks.
  pending:  Vec<SymbolTask>,
}

impl<'document> SymbolCollector<'document> {
  /// Create a collector from its root-level visit tasks.
  const fn new(document: &'document DocumentState, pending: Vec<SymbolTask>) -> Self {
    Self {
      document,
      symbols: Vec::new(),
      pending,
    }
  }

  /// Complete all pending tasks in source order.
  ///
  /// # Errors
  ///
  /// Returns [`MappingError`] when a semantic source range cannot be represented by LSP
  /// coordinates.
  fn collect(mut self) -> Result<Vec<ModernDocumentSymbol>, MappingError> {
    while let Some(task) = self.pending.pop() {
      match task {
        SymbolTask::Visit {
          name,
          key_range,
          node,
        } => self.visit(name, key_range, node)?,
        SymbolTask::Finish {
          name,
          kind,
          range,
          selection_range,
          children_start,
        } => self.finish(name, kind, range, selection_range, children_start),
      }
    }
    Ok(self.symbols)
  }

  /// Visit one semantic value and either emit it or schedule its composite children.
  ///
  /// # Errors
  ///
  /// Returns [`MappingError`] when the value's source or selection range cannot be represented by
  /// LSP coordinates.
  fn visit(&mut self, name: String, key_range: Option<TextRange>, node: Node) -> Result<(), MappingError> {
    let Some(node_range) = try_join_ranges(node.text_ranges(true)) else {
      return Ok(());
    };
    let own_range = self.document.mapper.range(node_range)?;
    let range = key_range.map_or(Ok(own_range), |source_key_range| {
      self.document.mapper.range(source_key_range.cover(node_range))
    })?;
    let selection_range = key_range.map_or(Ok(own_range), |source_key_range| self.document.mapper.range(source_key_range))?;

    match node {
      Node::Bool(_) => self
        .symbols
        .push(symbol(name, SymbolKind::BOOLEAN, range, selection_range, None)),
      Node::Str(_) => self
        .symbols
        .push(symbol(name, SymbolKind::STRING, range, selection_range, None)),
      Node::Integer(_) | Node::Float(_) => self
        .symbols
        .push(symbol(name, SymbolKind::NUMBER, range, selection_range, None)),
      Node::Date(_) => self.symbols.push(symbol(name, SymbolKind::FIELD, range, selection_range, None)),
      Node::Array(array) => {
        self.schedule_finish(name, SymbolKind::ARRAY, range, selection_range);
        self
          .pending
          .extend(array.items().iter().enumerate().rev().map(|(index, child)| SymbolTask::Visit {
            name:      index.to_string(),
            key_range: None,
            node:      child,
          }));
      }
      Node::Table(table) => {
        self.schedule_finish(name, SymbolKind::OBJECT, range, selection_range);
        self
          .pending
          .extend(table.entries().iter().rev().map(|(key, child)| SymbolTask::Visit {
            name:      ensure_non_empty_key(key.value().to_owned()),
            key_range: key.text_ranges().next(),
            node:      child,
          }));
      }
      Node::Invalid(_) => {}
    }
    Ok(())
  }

  /// Schedule assembly of a composite after its direct children.
  fn schedule_finish(&mut self, name: String, kind: SymbolKind, range: Range, selection_range: Range) {
    self.pending.push(SymbolTask::Finish {
      name,
      kind,
      range,
      selection_range,
      children_start: self.symbols.len(),
    });
  }

  /// Assemble one composite from the children emitted since its visit task.
  fn finish(&mut self, name: String, kind: SymbolKind, range: Range, selection_range: Range, children_start: usize) {
    let mut children = Vec::new();
    while self.symbols.len() > children_start {
      let Some(child) = self.symbols.pop() else {
        break;
      };
      children.push(child);
    }
    children.reverse();
    self.symbols.push(symbol(name, kind, range, selection_range, Some(children)));
  }
}

/// One heap-backed document-symbol traversal step.
enum SymbolTask {
  /// Visit one semantic value.
  Visit {
    /// Displayed symbol name.
    name:      String,
    /// Source range of the owning key, when present.
    key_range: Option<TextRange>,
    /// Semantic value to classify.
    node:      Node,
  },
  /// Assemble one composite after all direct children have completed.
  Finish {
    /// Displayed symbol name.
    name:            String,
    /// LSP symbol kind.
    kind:            SymbolKind,
    /// Full source range.
    range:           Range,
    /// Preferred editor selection range.
    selection_range: Range,
    /// Output index at which direct children begin.
    children_start:  usize,
  },
}

/// Construct one modern nested document symbol.
const fn symbol(
  name: String,
  kind: SymbolKind,
  range: Range,
  selection_range: Range,
  children: Option<Vec<ModernDocumentSymbol>>,
) -> ModernDocumentSymbol {
  ModernDocumentSymbol {
    name,
    kind,
    range,
    selection_range,
    detail: None,
    tags: None,
    children,
  }
}

/// Substitute a valid TOML representation for an empty decoded key.
fn ensure_non_empty_key(key_text: String) -> String {
  if key_text.is_empty() { "''".into() } else { key_text }
}

#[cfg(test)]
mod tests {
  use std::iter::repeat_n;

  use lsp_types::Position;
  use lsp_types::Range;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;

  use super::create_symbols;
  use crate::handlers::test_support::parse_document;
  use crate::lsp_ext::request::ModernDocumentSymbol;

  /// Find one named symbol in a symbol slice.
  fn named<'symbols>(symbols: &'symbols [ModernDocumentSymbol], name: &str) -> Option<&'symbols ModernDocumentSymbol> {
    symbols.iter().find(|symbol| symbol.name == name)
  }

  #[test]
  fn nested_symbols_select_their_source_keys() -> Result<(), TestFailure> {
    let document = parse_document(
      "parent.child = 1\n[table]\nvalue = true\n",
      "the document-symbol fixture must parse",
    )?;
    let symbols = ensure_ok(create_symbols(&document), "document symbols must map")?;
    let parent = ensure_some(named(&symbols, "parent"), "the dotted parent symbol must exist")?;
    ensure(
      parent.selection_range == Range::new(Position::new(0, 0), Position::new(0, 6)),
      "an implicit dotted-key table must select its real parent key",
    )?;
    let parent_children = ensure_some(parent.children.as_deref(), "the dotted parent symbol must retain nested children")?;
    let child = ensure_some(named(parent_children, "child"), "the dotted child symbol must exist")?;
    ensure(
      child.selection_range == Range::new(Position::new(0, 7), Position::new(0, 12)),
      "a dotted child must select only its own key segment",
    )?;

    let table = ensure_some(named(&symbols, "table"), "the table symbol must exist")?;
    ensure(
      table.selection_range == Range::new(Position::new(1, 1), Position::new(1, 6)),
      "a table symbol must select the key inside its header delimiters",
    )
  }

  #[test]
  fn heap_backed_symbol_walk_preserves_deep_array_nesting() -> Result<(), TestFailure> {
    let depth = 512;
    let source = format!("root = {}0{}\n", "[".repeat(depth), "]".repeat(depth));
    let document = parse_document(&source, "the deeply nested document-symbol fixture must parse")?;
    let symbols = ensure_ok(create_symbols(&document), "the heap-backed symbol walk must complete")?;
    let mut level = ensure_some(named(&symbols, "root"), "the root array symbol must exist")?;
    for expected_name in repeat_n("0", depth) {
      let children = ensure_some(level.children.as_deref(), "every nested array level must retain its child")?;
      level = ensure_some(
        named(children, expected_name),
        "each nested array child must preserve its numeric name",
      )?;
    }
    ensure_eq(
      &level.name,
      &String::from("0"),
      "the deepest scalar must remain reachable after iterative traversal",
    )
  }
}
