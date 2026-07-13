use lsp_async_stub::Context;
use lsp_async_stub::Params;
use lsp_async_stub::rpc::Error;
use lsp_async_stub::util::LspExt;
use lsp_async_stub::util::Mapper;
use lsp_types::DocumentSymbol;
use lsp_types::DocumentSymbolParams;
use lsp_types::DocumentSymbolResponse;
use lsp_types::SymbolKind;
use taplo::dom::Node;
use taplo::rowan::TextRange;
use taplo::util::join_ranges;
use taplo_common::environment::Environment;

use crate::world::DocumentState;
use crate::world::World;

#[tracing::instrument(skip_all)]
pub(crate) async fn document_symbols<E: Environment>(
  context: Context<World<E>>,
  params: Params<DocumentSymbolParams>,
) -> Result<Option<DocumentSymbolResponse>, Error> {
  let p = params.required()?;

  let Some(document_uri) = crate::uri::to_url(&p.text_document.uri) else {
    return Ok(None);
  };

  let Some(snapshot) = context.document_snapshot(&document_uri).await else {
    return Ok(None);
  };

  Ok(Some(DocumentSymbolResponse::Nested(create_symbols(&snapshot.document))))
}

pub(crate) fn create_symbols(doc: &DocumentState) -> Vec<DocumentSymbol> {
  let mapper = &doc.mapper;
  let mut symbols: Vec<DocumentSymbol> = Vec::new();

  let dom = doc.dom.clone();

  let root_table = dom.as_table().unwrap();
  let entries = root_table.entries().read();

  for (key, entry) in entries.iter() {
    symbols_for_value(ensure_non_empty_key(key.value().to_string()), None, entry, mapper, &mut symbols);
  }

  symbols
}

#[allow(deprecated)]
fn symbols_for_value(name: String, key_range: Option<TextRange>, node: &Node, mapper: &Mapper, symbols: &mut Vec<DocumentSymbol>) {
  let own_range = mapper.range(join_ranges(node.text_ranges(true))).unwrap();

  let range = if let Some(key_r) = key_range {
    mapper.range(key_r.cover(join_ranges(node.text_ranges(true)))).unwrap()
  } else {
    own_range
  };

  let selection_range = key_range.map_or(own_range, |r| mapper.range(r).unwrap());

  match node {
    Node::Bool(_) => symbols.push(DocumentSymbol {
      name,
      kind: SymbolKind::BOOLEAN,
      range: range.into_lsp(),
      selection_range: selection_range.into_lsp(),
      detail: None,
      deprecated: None,
      tags: Default::default(),
      children: None,
    }),
    Node::Str(_) => symbols.push(DocumentSymbol {
      name,
      kind: SymbolKind::STRING,
      range: range.into_lsp(),
      selection_range: selection_range.into_lsp(),
      detail: None,
      deprecated: None,
      tags: Default::default(),
      children: None,
    }),
    Node::Integer(_) | Node::Float(_) => symbols.push(DocumentSymbol {
      name,
      kind: SymbolKind::NUMBER,
      range: range.into_lsp(),
      selection_range: selection_range.into_lsp(),
      detail: None,
      deprecated: None,
      tags: Default::default(),
      children: None,
    }),
    Node::Date(_) => symbols.push(DocumentSymbol {
      name,
      kind: SymbolKind::FIELD,
      range: range.into_lsp(),
      selection_range: selection_range.into_lsp(),
      detail: None,
      deprecated: None,
      tags: Default::default(),
      children: None,
    }),
    Node::Array(arr) => symbols.push(DocumentSymbol {
      name,
      kind: SymbolKind::ARRAY,
      range: range.into_lsp(),
      selection_range: selection_range.into_lsp(),
      detail: None,
      deprecated: None,
      tags: Default::default(),
      children: {
        let mut child_symbols = Vec::with_capacity(arr.items().read().len());
        let items = arr.items().read();

        for (i, c) in items.iter().enumerate() {
          symbols_for_value(i.to_string(), None, c, mapper, &mut child_symbols);
        }

        Some(child_symbols)
      },
    }),
    Node::Table(t) => {
      symbols.push(DocumentSymbol {
        name,
        kind: SymbolKind::OBJECT,
        range: range.into_lsp(),
        selection_range: selection_range.into_lsp(),
        detail: None,
        deprecated: None,
        tags: Default::default(),
        children: {
          let mut child_symbols = Vec::with_capacity(t.entries().read().len());
          let entries = t.entries().read();
          for (key, entry) in entries.iter() {
            symbols_for_value(
              ensure_non_empty_key(key.value().to_string()),
              None,
              entry,
              mapper,
              &mut child_symbols,
            );
          }

          Some(child_symbols)
        },
      });
    }
    Node::Invalid(_) => {}
  }
}

fn ensure_non_empty_key(s: String) -> String {
  if s.is_empty() { r"''".into() } else { s }
}
