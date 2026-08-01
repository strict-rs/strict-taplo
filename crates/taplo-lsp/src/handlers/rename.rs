//! Identifier-only prepare-rename and complete, all-or-nothing workspace edits.

use std::collections::HashMap;

use lsp_types::Position;
use lsp_types::PrepareRenameResponse;
use lsp_types::RenameParams;
use lsp_types::TextDocumentPositionParams;
use lsp_types::TextEdit;
use lsp_types::WorkspaceEdit;
use taplo::dom::KeyOrIndex;
use taplo::dom::Keys;
use taplo::dom::rewrite::PendingPatchKind;
use taplo::dom::rewrite::Rewrite;
use taplo::syntax::SyntaxKind;
use taplo::syntax::SyntaxNode;
use taplo::syntax::SyntaxToken;
use taplo_lsp_async::Params;
use taplo_lsp_async::rpc::RpcError;
use url::Url;

use crate::query::Query;
use crate::query::lookup_keys;
use crate::world::DocumentState;
use crate::world::WorldState;

/// Prepare identifier-only rename for one current document snapshot.
///
/// # Errors
///
/// Returns an RPC error when request parameters, source coordinates, or snapshot freshness cannot
/// be validated.
macro_rules! define_rename_future_family {
  (
    $prepare_rename:ident,
    $rename:ident,
    $document_snapshot_for_uri:ident,
    $ensure_current_snapshot:ident;
    $future:ident,
    $environment:path,
    $transport:ident,
    $schema_execution:path;
    [$($value_bound:path),*]
  ) => {
    /// Prepare identifier-only rename for one current document snapshot.
    ///
    /// # Errors
    ///
    /// Returns an RPC error when request parameters, source coordinates, or snapshot freshness
    /// cannot be validated.
    pub(super) fn $prepare_rename<E: $environment>(
      world: &WorldState<E, $transport<E>>,
      params: Params<TextDocumentPositionParams>,
    ) -> $future<'_, Result<Option<PrepareRenameResponse>, RpcError>> {
      Box::pin(async move {
        let parameters = params.required()?;
        current_document_snapshot!(
          super::$document_snapshot_for_uri(world, &parameters.text_document.uri) => (document_uri, snapshot)
        );
        let response = prepare_document(&snapshot.document, parameters.position)?;
        super::$ensure_current_snapshot(world, &document_uri, &snapshot).await?;
        Ok(response)
      })
    }

    /// Build an all-or-nothing identifier rename edit for one current document snapshot.
    ///
    /// # Errors
    ///
    /// Returns an RPC error when request parameters, source coordinates, rewrite construction, or
    /// snapshot freshness cannot be validated.
    pub(super) fn $rename<E: $environment>(
      world: &WorldState<E, $transport<E>>,
      params: Params<RenameParams>,
    ) -> $future<'_, Result<Option<WorkspaceEdit>, RpcError>> {
      Box::pin(async move {
        let parameters = params.required()?;
        current_document_snapshot!(
          super::$document_snapshot_for_uri(world, &parameters.text_document_position.text_document.uri) => (document_uri, snapshot)
        );
        let edit = rename_document(
          &snapshot.document,
          &document_uri,
          parameters.text_document_position.position,
          &parameters.new_name,
        )?;
        super::$ensure_current_snapshot(world, &document_uri, &snapshot).await?;
        Ok(edit)
      })
    }
  };
}

define_checked_document_handler_execution_families!(
  define_rename_future_family;
  (prepare_rename_local, prepare_rename_concurrent),
  (rename_local, rename_concurrent),
);

/// Owned cursor facts shared by prepare-rename and rename execution.
struct RenameTarget {
  /// Identifier selected at the cursor.
  syntax:     SyntaxToken,
  /// Narrowest semantic node that owns the identifier.
  dom_node:   Option<(Keys, taplo::dom::Node)>,
  /// Table-header key containing the identifier, when present.
  header_key: Option<SyntaxNode>,
}

/// Classify one cursor as a complete identifier rename target.
fn rename_target(document: &DocumentState, cursor_position: Position) -> Result<Option<RenameTarget>, RpcError> {
  let offset = document
    .mapper
    .offset(cursor_position)
    .map_err(|error| super::uri::mapping_rpc_error(&error))?;
  let query = Query::at(&document.dom, offset);
  if header_is_incomplete(&query) {
    return Ok(None);
  }
  let Some(position_info) = query.first_matching(|position| position.syntax.kind() == SyntaxKind::IDENT) else {
    return Ok(None);
  };
  Ok(Some(RenameTarget {
    syntax:     position_info.syntax.clone(),
    dom_node:   position_info.dom_node.clone(),
    header_key: query.header_key(),
  }))
}

/// Prepare identifier-only rename for one immutable document snapshot.
fn prepare_document(document: &DocumentState, cursor_position: Position) -> Result<Option<PrepareRenameResponse>, RpcError> {
  let Some(target) = rename_target(document, cursor_position)? else {
    return Ok(None);
  };
  let range =
    super::uri::to_lsp_range(&document.mapper, target.syntax.text_range()).map_err(|error| super::uri::mapping_rpc_error(&error))?;
  Ok(Some(PrepareRenameResponse::Range(range)))
}

/// Build one all-or-nothing rename edit from an immutable document snapshot.
fn rename_document(
  document: &DocumentState,
  document_uri: &Url,
  cursor_position: Position,
  new_name: &str,
) -> Result<Option<WorkspaceEdit>, RpcError> {
  let Some(target) = rename_target(document, cursor_position)? else {
    return Ok(None);
  };
  let Some(position_node) = target.dom_node.as_ref() else {
    return Ok(None);
  };
  let mut target_keys = position_node.0.clone();
  if target.header_key.is_some() {
    let Some(index) = Query::header_identifier_index(&target.syntax) else {
      return Ok(None);
    };
    target_keys = lookup_keys(
      document.dom.clone(),
      &Keys::new(target_keys.into_iter().take(index.saturating_add(1))),
    );
  }
  if matches!(target_keys.iter().last(), Some(&KeyOrIndex::Index(_))) {
    target_keys = target_keys.skip_right(1);
  }

  let mut rewrite = Rewrite::new(document.dom.clone())
    .map_err(|error| RpcError::internal_error().with_details(format!("failed to initialize rename rewrite: {error}")))?;
  let renamed_rewrite = rewrite
    .rename_keys(target_keys.dotted(), new_name)
    .map_err(|error| RpcError::internal_error().with_details(format!("failed to rename key: {error}")))?;

  let Some(uri) = super::uri::to_uri(document_uri) else {
    return Err(RpcError::internal_error().with_details("document URL is not representable by LSP"));
  };
  let mut edits = Vec::with_capacity(renamed_rewrite.patches().len());
  for patch in renamed_rewrite.patches() {
    let PendingPatchKind::Replace(ref replacement) = patch.kind else {
      return Err(RpcError::internal_error().with_details("unsupported rewrite patch kind"));
    };
    let range = super::uri::to_lsp_range(&document.mapper, patch.range).map_err(|error| super::uri::mapping_rpc_error(&error))?;
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
  use std::future::Future;

  use futures::executor::block_on;
  use lsp_types::Position;
  use lsp_types::PrepareRenameResponse;
  use lsp_types::Range;
  use lsp_types::RenameParams;
  use lsp_types::TextDocumentPositionParams;
  use lsp_types::WorkspaceEdit;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use taplo_lsp_async::Params;
  use taplo_lsp_async::rpc::RpcError;
  use taplo_lsp_async::util::Mapper;
  use url::Url;

  use super::prepare_document;
  use super::prepare_rename_concurrent;
  use super::prepare_rename_local;
  use super::rename_concurrent;
  use super::rename_document;
  use super::rename_local;
  use crate::handlers::test_support::concurrent_world;
  use crate::handlers::test_support::local_world;
  use crate::handlers::test_support::parse_document;
  use crate::handlers::test_support::position_params;
  use crate::handlers::test_support::rename_params as decode_rename_params;
  use crate::handlers::test_support::replace_concurrent_document;
  use crate::handlers::test_support::replace_local_document;
  use crate::handlers::test_support::url as fixture_url;
  use crate::world::DocumentState;

  /// Build a parsed document with a mapper that may deliberately represent a different source.
  fn document(source: &str, mapper_source: &str) -> Result<DocumentState, TestFailure> {
    let mut document = parse_document(source, "the rename fixture tree must build")?;
    document.mapper = ensure_ok(Mapper::new_utf16(mapper_source), "the rename mapper fixture must build")?;
    Ok(document)
  }

  /// Parse the rename document URL fixture.
  fn document_url() -> Result<Url, TestFailure> {
    fixture_url("file:///workspace/file.toml", "the rename fixture URL must parse")
  }

  /// Decode one prepare-rename request through its public wire shape.
  fn prepare_params(document: &Url, line: u32, character: u32) -> Result<TextDocumentPositionParams, TestFailure> {
    position_params(document, line, character, "the prepare-rename request fixture must decode")
  }

  /// Decode one rename request through its public wire shape.
  fn rename_params(document: &Url, line: u32, character: u32, new_name: &str) -> Result<RenameParams, TestFailure> {
    decode_rename_params(document, line, character, new_name, "the rename request fixture must decode")
  }

  /// Project one workspace edit into its ordered replacement ranges and texts.
  fn edit_observation(edit: &WorkspaceEdit) -> Vec<(Range, String)> {
    edit
      .changes
      .iter()
      .flat_map(|changes| changes.values())
      .flatten()
      .map(|text_edit| (text_edit.range, text_edit.new_text.clone()))
      .collect()
  }

  /// Resolve one prepare/rename request pair through a selected execution family.
  async fn rename_outputs(
    prepare: impl Future<Output = Result<Option<PrepareRenameResponse>, RpcError>>,
    rename: impl Future<Output = Result<Option<WorkspaceEdit>, RpcError>>,
    prepare_execution: &'static str,
    prepare_presence: &'static str,
    rename_execution: &'static str,
    rename_presence: &'static str,
  ) -> Result<(PrepareRenameResponse, WorkspaceEdit), TestFailure> {
    let prepared = ensure_some(ensure_ok(prepare.await, prepare_execution)?, prepare_presence)?;
    let edit = ensure_some(ensure_ok(rename.await, rename_execution)?, rename_presence)?;
    Ok((prepared, edit))
  }

  /// Execute one rename scenario through every selected handler family.
  macro_rules! rename_family_outputs {
    ($document:ident, $line:literal, $character:literal, $new_name:literal; $(($world:ident, $prepare:path, $rename:path, $prepare_execution:literal, $prepare_presence:literal, $rename_execution:literal, $rename_presence:literal)),+ $(,)?) => {
      (
        $(
          rename_outputs(
            $prepare(
              &$world,
              Params::from(Some(prepare_params(&$document, $line, $character)?)),
            ),
            $rename(
              &$world,
              Params::from(Some(rename_params(
                &$document,
                $line,
                $character,
                $new_name,
              )?)),
            ),
            $prepare_execution,
            $prepare_presence,
            $rename_execution,
            $rename_presence,
          ).await?
        ),+
      )
    };
  }

  #[test]
  fn prepare_and_rename_accept_identifiers_but_reject_other_positions() -> Result<(), TestFailure> {
    let source = "alpha = 1\n";
    let document = document(source, source)?;
    let prepared = ensure_some(
      ensure_ok(
        prepare_document(&document, Position::new(0, 1)),
        "identifier prepare-rename must be fallible without failing",
      )?,
      "a real identifier must be prepare-renameable",
    )?;
    ensure(
      prepared == PrepareRenameResponse::Range(Range::new(Position::new(0, 0), Position::new(0, 5))),
      "prepare-rename must return the exact identifier range",
    )?;

    let edit = ensure_some(
      ensure_ok(
        rename_document(&document, &document_url()?, Position::new(0, 1), "renamed"),
        "identifier rename must succeed",
      )?,
      "a real identifier must produce a workspace edit",
    )?;
    let edits = ensure_some(
      edit
        .changes
        .as_ref()
        .and_then(|document_changes| document_changes.values().next()),
      "rename changes and document edits must exist",
    )?;
    let replacement = ensure_some(edits.first(), "the key replacement must exist")?;
    ensure_eq(
      &replacement.new_text.as_str(),
      &"renamed",
      "rename must use the requested replacement text",
    )?;
    ensure(
      replacement.range == Range::new(Position::new(0, 0), Position::new(0, 5)),
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
  fn array_table_header_rename_targets_the_semantic_key_across_instances() -> Result<(), TestFailure> {
    let source = "[[products]]\nname = \"first\"\n[[products]]\nname = \"second\"\n";
    let document = document(source, source)?;
    let edit = ensure_some(
      ensure_ok(
        rename_document(&document, &document_url()?, Position::new(0, 3), "items"),
        "array-table header rename must succeed",
      )?,
      "a complete array-table header must produce a workspace edit",
    )?;
    ensure(
      edit_observation(&edit)
        == vec![
          (Range::new(Position::new(2, 2), Position::new(2, 10)), String::from("items")),
          (Range::new(Position::new(0, 2), Position::new(0, 10)), String::from("items")),
        ],
      "array-table header rename must remove runtime item indices and replace every semantic key occurrence in rewrite order",
    )
  }

  #[test]
  fn malformed_headers_and_missing_mapper_endpoints_do_not_return_partial_edits() -> Result<(), TestFailure> {
    let malformed_source = "[alpha.\n";
    let malformed = document(malformed_source, malformed_source)?;
    ensure(
      ensure_ok(
        rename_document(&malformed, &document_url()?, Position::new(0, 2), "renamed"),
        "a malformed header rename request must not panic",
      )?
      .is_none(),
      "a malformed header without a renameable DOM path must produce no edit",
    )?;

    let source = "alpha = 1\n";
    let unmappable = document(source, "a")?;
    let result = rename_document(&unmappable, &document_url()?, Position::new(0, 0), "renamed");
    let error = ensure_some(result.err(), "an unmappable replacement endpoint must return a typed error")?;
    ensure_eq(&error.code, &-32603, "mapping failure must be reported as an internal RPC error")?;
    let detail = ensure_some(
      error.details.as_ref().and_then(serde_json::Value::as_str),
      "mapping failure must retain its textual RPC detail",
    )?;
    ensure(
      [detail.contains("byte offset"), detail.contains("beyond source length")] == [true, true],
      "mapping failure must carry actionable context rather than a partial edit",
    )
  }

  #[test]
  fn rename_handlers_preserve_absence_errors_and_execution_family_parity() -> Result<(), TestFailure> {
    block_on(async {
      let document = document_url()?;
      let source = "[alpha]\nvalue = 1\n[alpha.child]\nvalue = 2\n";

      let local = local_world()?;
      let concurrent = concurrent_world()?;
      let (local_installation, concurrent_installation) = futures::join!(
        replace_local_document(&local, &document, source, "the local rename document must install",),
        replace_concurrent_document(&concurrent, &document, source, "the concurrent rename document must install",),
      );
      local_installation?;
      concurrent_installation?;

      let ((local_prepare, local_edit), (concurrent_prepare, concurrent_edit)) = rename_family_outputs!(
        document,
        0,
        2,
        "renamed";
        (
          local,
          prepare_rename_local,
          rename_local,
          "local prepare-rename must execute",
          "a local identifier must prepare rename",
          "local rename must execute",
          "a local identifier must produce a workspace edit"
        ),
        (
          concurrent,
          prepare_rename_concurrent,
          rename_concurrent,
          "concurrent prepare-rename must execute",
          "a concurrent identifier must prepare rename",
          "concurrent rename must execute",
          "a concurrent identifier must produce a workspace edit"
        ),
      );
      ensure(
        local_prepare == PrepareRenameResponse::Range(Range::new(Position::new(0, 1), Position::new(0, 6))),
        "local prepare-rename must retain the exact selected identifier range",
      )?;
      let local_observation = edit_observation(&local_edit);
      ensure(
        local_observation
          == vec![
            (Range::new(Position::new(2, 1), Position::new(2, 6)), "renamed".into()),
            (Range::new(Position::new(0, 1), Position::new(0, 6)), "renamed".into()),
          ],
        "local rename must replace every exact occurrence in descending source order without editing the dotted child segment",
      )?;

      let missing_document = fixture_url("file:///workspace/missing.toml", "the missing rename document URL must parse")?;
      ensure(
        ensure_ok(
          prepare_rename_local(&local, Params::from(Some(prepare_params(&missing_document, 0, 0)?))).await,
          "prepare-rename for an unopened document must remain an absent success",
        )?
        .is_none(),
        "prepare-rename must not fabricate state for an unopened document",
      )?;
      let missing_params = ensure_some(
        rename_local(&local, Params::<RenameParams>::from(None)).await.err(),
        "rename without parameters must return a typed invalid-params error",
      )?;
      ensure_eq(
        &missing_params.code,
        &-32602,
        "rename without parameters must retain the standard invalid-params code",
      )?;

      ensure(
        (concurrent_prepare, edit_observation(&concurrent_edit)) == (local_prepare, local_observation),
        "local and concurrent rename families must preserve identical prepare and workspace-edit observations",
      )
    })
  }
}
