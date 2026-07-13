//! Identifier-only prepare-rename and complete, all-or-nothing workspace edits.

use std::collections::HashMap;

use lsp_async_stub::Context;
use lsp_async_stub::Params;
use lsp_async_stub::rpc::Error;
use lsp_types::Position;
use lsp_types::PrepareRenameResponse;
use lsp_types::RenameParams;
use lsp_types::TextDocumentPositionParams;
use lsp_types::TextEdit;
use lsp_types::WorkspaceEdit;
use taplo::dom::KeyOrIndex;
use taplo::dom::Keys;
use taplo::dom::rewrite::Rewrite;
use taplo::syntax::SyntaxKind;
use taplo_common::environment::Environment;

use crate::query::Query;
use crate::query::lookup_keys;
use crate::world::DocumentState;
use crate::world::World;

#[tracing::instrument(skip_all)]
pub async fn prepare_rename<E: Environment>(
  context: Context<World<E>>,
  params: Params<TextDocumentPositionParams>,
) -> Result<Option<PrepareRenameResponse>, Error> {
  let params = params.required()?;
  let Some(document_uri) = crate::uri::to_url(&params.text_document.uri) else {
    return Ok(None);
  };
  let Some(snapshot) = context.document_snapshot(&document_uri).await else {
    return Ok(None);
  };
  prepare_document(&snapshot.document, params.position)
}

/// Prepare identifier-only rename for one immutable document snapshot.
fn prepare_document(document: &DocumentState, position: Position) -> Result<Option<PrepareRenameResponse>, Error> {
  let Some(offset) = document.mapper.offset(crate::uri::from_lsp_position(position)) else {
    return Ok(None);
  };
  let query = Query::at(&document.dom, offset);
  if header_is_incomplete(&query) {
    return Ok(None);
  }
  let Some(position) = query.first_matching(|position| position.syntax.kind() == SyntaxKind::IDENT) else {
    return Ok(None);
  };
  let Some(range) = crate::uri::to_lsp_range(&document.mapper, position.syntax.text_range()) else {
    return Ok(None);
  };
  Ok(Some(PrepareRenameResponse::Range(range)))
}

#[tracing::instrument(skip_all)]
pub async fn rename<E: Environment>(context: Context<World<E>>, params: Params<RenameParams>) -> Result<Option<WorkspaceEdit>, Error> {
  let params = params.required()?;
  let Some(document_uri) = crate::uri::to_url(&params.text_document_position.text_document.uri) else {
    return Ok(None);
  };
  let Some(snapshot) = context.document_snapshot(&document_uri).await else {
    return Ok(None);
  };
  rename_document(
    &snapshot.document,
    &document_uri,
    params.text_document_position.position,
    &params.new_name,
  )
}

/// Build one all-or-nothing rename edit from an immutable document snapshot.
fn rename_document(
  document: &DocumentState,
  document_uri: &url::Url,
  position: Position,
  new_name: &str,
) -> Result<Option<WorkspaceEdit>, Error> {
  let Some(offset) = document.mapper.offset(crate::uri::from_lsp_position(position)) else {
    return Ok(None);
  };
  let query = Query::at(&document.dom, offset);
  if header_is_incomplete(&query) {
    return Ok(None);
  }
  let Some(position) = query.first_matching(|position| position.syntax.kind() == SyntaxKind::IDENT) else {
    return Ok(None);
  };
  let Some((keys, _)) = &position.dom_node else {
    return Ok(None);
  };
  let mut keys = keys.clone();
  if query.header_key().is_some() {
    let Some(index) = query.header_identifier_index(&position.syntax) else {
      return Ok(None);
    };
    keys = lookup_keys(document.dom.clone(), &Keys::new(keys.into_iter().take(index.saturating_add(1))));
  }
  if matches!(keys.iter().last(), Some(KeyOrIndex::Index(_))) {
    keys = keys.skip_right(1);
  }

  let mut rewrite = Rewrite::new(document.dom.clone())
    .map_err(|error| Error::internal_error().with_data(format!("failed to initialize rename rewrite: {error}")))?;
  rewrite
    .rename_keys(keys.dotted(), new_name)
    .map_err(|error| Error::internal_error().with_data(format!("failed to rename key: {error}")))?;

  let Some(uri) = crate::uri::to_uri(document_uri) else {
    return Err(Error::internal_error().with_data("document URL is not representable by LSP"));
  };
  let mut edits = Vec::with_capacity(rewrite.patches().len());
  for patch in rewrite.patches() {
    let taplo::dom::rewrite::PendingPatchKind::Replace(replacement) = &patch.kind else {
      return Err(Error::internal_error().with_data("unsupported rewrite patch kind"));
    };
    let Some(range) = crate::uri::to_lsp_range(&document.mapper, patch.range) else {
      return Err(Error::internal_error().with_data("rename range is not representable by LSP"));
    };
    edits.push(TextEdit {
      range,
      new_text: replacement.to_string(),
    });
  }

  Ok(Some(WorkspaceEdit {
    changes: Some(HashMap::from([(uri, edits)])),
    ..WorkspaceEdit::default()
  }))
}

/// Whether tolerant syntax exposes a header key without a complete enclosing header.
fn header_is_incomplete(query: &Query) -> bool {
  query.header_key().is_some() && !query.in_table_header() && !query.in_table_array_header()
}

#[cfg(test)]
mod tests {
  use lsp_async_stub::util::Mapper;
  use lsp_types::Position;
  use lsp_types::PrepareRenameResponse;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use url::Url;

  use super::prepare_document;
  use super::rename_document;
  use crate::world::DocumentState;

  /// Build a parsed document with a mapper that may deliberately represent a different source.
  fn document(source: &str, mapper_source: &str) -> DocumentState {
    let parse = taplo::parser::parse(source);
    let dom = parse.clone().into_dom();
    DocumentState {
      parse,
      dom,
      mapper: Mapper::new_utf16(mapper_source, false),
    }
  }

  /// Parse the rename document URL fixture.
  fn document_url() -> Result<Url, TestFailure> {
    ensure_ok(Url::parse("file:///workspace/file.toml"), "the rename fixture URL must parse")
  }

  #[test]
  fn prepare_and_rename_accept_identifiers_but_reject_other_positions() -> Result<(), TestFailure> {
    let source = "alpha = 1\n";
    let document = document(source, source);
    let prepared = ensure_some(
      ensure_ok(
        prepare_document(&document, Position::new(0, 1)),
        "identifier prepare-rename must be fallible without failing",
      )?,
      "a real identifier must be prepare-renameable",
    )?;
    ensure(
      prepared == PrepareRenameResponse::Range(lsp_types::Range::new(Position::new(0, 0), Position::new(0, 5))),
      "prepare-rename must return the exact identifier range",
    )?;

    let edit = ensure_some(
      ensure_ok(
        rename_document(&document, &document_url()?, Position::new(0, 1), "renamed"),
        "identifier rename must succeed",
      )?,
      "a real identifier must produce a workspace edit",
    )?;
    let changes = ensure_some(edit.changes.as_ref(), "rename changes must exist")?;
    let edits = ensure_some(changes.values().next(), "the document edits must exist")?;
    let replacement = ensure_some(edits.first(), "the key replacement must exist")?;
    ensure_eq(
      &replacement.new_text.as_str(),
      &"renamed",
      "rename must use the requested replacement text",
    )?;
    ensure(
      replacement.range == lsp_types::Range::new(Position::new(0, 0), Position::new(0, 5)),
      "rename must map the full identifier range",
    )?;

    for position in [Position::new(0, 7), Position::new(0, 8)] {
      ensure(
        ensure_ok(
          prepare_document(&document, position),
          "a nonidentifier prepare request must remain fallible",
        )?
        .is_none(),
        "whitespace and primitive positions must not prepare rename",
      )?;
      ensure(
        ensure_ok(
          rename_document(&document, &document_url()?, position, "ignored"),
          "a nonidentifier rename request must remain fallible",
        )?
        .is_none(),
        "whitespace and primitive positions must not produce edits",
      )?;
    }
    Ok(())
  }

  #[test]
  fn malformed_headers_and_missing_mapper_endpoints_do_not_return_partial_edits() -> Result<(), TestFailure> {
    let malformed_source = "[alpha.\n";
    let malformed = document(malformed_source, malformed_source);
    ensure(
      ensure_ok(
        rename_document(&malformed, &document_url()?, Position::new(0, 2), "renamed"),
        "a malformed header rename request must not panic",
      )?
      .is_none(),
      "a malformed header without a renameable DOM path must produce no edit",
    )?;

    let source = "alpha = 1\n";
    let unmappable = document(source, "a");
    let result = rename_document(&unmappable, &document_url()?, Position::new(0, 0), "renamed");
    let error = ensure_some(result.err(), "an unmappable replacement endpoint must return a typed error")?;
    ensure_eq(&error.code, &-32603, "mapping failure must be reported as an internal RPC error")?;
    ensure(
      error
        .data
        .as_ref()
        .and_then(serde_json::Value::as_str)
        .is_some_and(|detail| detail.contains("range is not representable")),
      "mapping failure must carry actionable context rather than a partial edit",
    )
  }
}
