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
use taplo::dom::Node;
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
    #[allow(clippy::single_call_fn, reason = "one prepare-rename entry point per execution family, registered exactly once by its runtime family")]
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
    #[allow(clippy::single_call_fn, reason = "one rename entry point per execution family, registered exactly once by its runtime family")]
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
  dom_node:   Option<(Keys, Node)>,
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
#[allow(
  clippy::single_call_fn,
  reason = "the name states the tolerant-parse rule that a header key outside a complete `[table]` or `[[array]]` header is not a rename \
            target, which the three-predicate expression alone does not convey"
)]
fn header_is_incomplete(query: &Query) -> bool {
  query.header_key().is_some() && !query.in_table_header() && !query.in_table_array_header()
}

#[cfg(test)]
mod tests {
  use std::fmt::Debug;

  use futures::executor::block_on;
  use lsp_types::Position;
  use lsp_types::PrepareRenameResponse;
  use lsp_types::Range;
  use lsp_types::RenameParams;
  use lsp_types::TextDocumentPositionParams;
  use lsp_types::WorkspaceEdit;
  use strict_test_support::ResultFailure;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_that;
  use taplo_lsp_async::Params;
  use taplo_lsp_async::util::Mapper;
  use url::Url;

  use super::prepare_document;
  use super::prepare_rename_concurrent;
  use super::prepare_rename_local;
  use super::rename_concurrent;
  use super::rename_document;
  use super::rename_local;
  use crate::handlers::test_support::FixtureFailure;
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
  fn document(source: &str, mapper_source: &str) -> Result<DocumentState, FixtureFailure> {
    let mut document = parse_document(source, "the rename fixture tree must build")?;
    document.mapper = ensure_ok(Mapper::new_utf16(mapper_source), "the rename mapper fixture must build")?;
    Ok(document)
  }

  /// Parse the rename document URL fixture.
  fn document_url() -> Result<Url, ResultFailure<url::ParseError>> {
    fixture_url("file:///workspace/file.toml", "the rename fixture URL must parse")
  }

  /// Decode one prepare-rename request through its public wire shape.
  fn prepare_params(document: &Url, line: u32, character: u32) -> Result<TextDocumentPositionParams, ResultFailure<serde_json::Error>> {
    position_params(document, line, character, "the prepare-rename request fixture must decode")
  }

  /// Decode one rename request through its public wire shape.
  fn rename_params(document: &Url, line: u32, character: u32, new_name: &str) -> Result<RenameParams, ResultFailure<serde_json::Error>> {
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

  #[test]
  fn prepare_and_rename_accept_identifiers_but_reject_other_positions() -> Result<(), impl Debug> {
    let observed = (|| {
      let source = "alpha = 1\n";
      let document = document(source, source)?;
      let url = document_url()?;
      let prepared = prepare_document(&document, Position::new(0, 1));
      let edit = rename_document(&document, &url, Position::new(0, 1), "renamed");
      let rejected = [Position::new(0, 7), Position::new(0, 8)].map(|position| {
        (
          position,
          prepare_document(&document, position),
          rename_document(&document, &url, position, "ignored"),
        )
      });
      Ok::<_, FixtureFailure>((document, url, prepared, edit, rejected))
    })();
    ensure_that(
      observed,
      "rename must preserve exact identifier edits and reject whitespace or primitive positions without edits",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        let Ok(Some(ref workspace)) = scenario.3 else {
          return false;
        };
        scenario.2
          == Ok(Some(PrepareRenameResponse::Range(Range::new(
            Position::new(0, 0),
            Position::new(0, 5),
          ))))
          && workspace
            .changes
            .as_ref()
            .and_then(|changes| changes.values().next())
            .and_then(|edits| edits.first())
            .is_some_and(|replacement| {
              replacement.new_text == "renamed" && replacement.range == Range::new(Position::new(0, 0), Position::new(0, 5))
            })
          && scenario
            .4
            .iter()
            .all(|position| matches!(position.1, Ok(None)) && matches!(position.2, Ok(None)))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn array_table_header_rename_targets_the_semantic_key_across_instances() -> Result<(), impl Debug> {
    let observed = (|| {
      let source = "[[products]]\nname = \"first\"\n[[products]]\nname = \"second\"\n";
      let document = document(source, source)?;
      let url = document_url()?;
      let edit = rename_document(&document, &url, Position::new(0, 3), "items");
      Ok::<_, FixtureFailure>((document, url, edit))
    })();
    ensure_that(
      observed,
      "array-table rename must omit item indices and replace every semantic key in rewrite order",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        let Ok(Some(ref edit)) = scenario.2 else {
          return false;
        };
        edit_observation(edit)
          == vec![
            (Range::new(Position::new(2, 2), Position::new(2, 10)), String::from("items")),
            (Range::new(Position::new(0, 2), Position::new(0, 10)), String::from("items")),
          ]
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn malformed_headers_and_missing_mapper_endpoints_do_not_return_partial_edits() -> Result<(), impl Debug> {
    let observed = (|| {
      let malformed_source = "[alpha.\n";
      let malformed = document(malformed_source, malformed_source)?;
      let unmappable = document("alpha = 1\n", "a")?;
      let url = document_url()?;
      let absent = rename_document(&malformed, &url, Position::new(0, 2), "renamed");
      let rejected = rename_document(&unmappable, &url, Position::new(0, 0), "renamed");
      Ok::<_, FixtureFailure>((malformed, unmappable, url, absent, rejected))
    })();
    ensure_that(
      observed,
      "malformed headers must return absence and unmappable endpoints must retain actionable typed errors without partial edits",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        matches!(scenario.3, Ok(None))
          && scenario.4.as_ref().is_err_and(|error| {
            error.code == -32603
              && error
                .details
                .as_ref()
                .and_then(serde_json::Value::as_str)
                .is_some_and(|detail| detail.contains("byte offset") && detail.contains("beyond source length"))
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn rename_handlers_preserve_absence_errors_and_execution_family_parity() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let document = document_url()?;
      let missing_document = fixture_url("file:///workspace/missing.toml", "the missing rename document URL must parse")?;
      let local = local_world()?;
      let concurrent = concurrent_world()?;
      let local_prepare_params = prepare_params(&document, 0, 2)?;
      let local_rename_params = rename_params(&document, 0, 2, "renamed")?;
      let concurrent_prepare_params = prepare_params(&document, 0, 2)?;
      let concurrent_rename_params = rename_params(&document, 0, 2, "renamed")?;
      let absent_params = prepare_params(&missing_document, 0, 0)?;
      let source = "[alpha]\nvalue = 1\n[alpha.child]\nvalue = 2\n";
      let installations = futures::join!(
        replace_local_document(&local, &document, source, "the local rename document must install"),
        replace_concurrent_document(&concurrent, &document, source, "the concurrent rename document must install"),
      );
      let local_prepare = prepare_rename_local(&local, Params::from(Some(local_prepare_params))).await;
      let local_edit = rename_local(&local, Params::from(Some(local_rename_params))).await;
      let concurrent_prepare = prepare_rename_concurrent(&concurrent, Params::from(Some(concurrent_prepare_params))).await;
      let concurrent_edit = rename_concurrent(&concurrent, Params::from(Some(concurrent_rename_params))).await;
      let absent = prepare_rename_local(&local, Params::from(Some(absent_params))).await;
      let rejected = rename_local(&local, Params::<RenameParams>::from(None)).await;
      Ok::<_, FixtureFailure>((
        document,
        local,
        concurrent,
        installations,
        (local_prepare, local_edit),
        (concurrent_prepare, concurrent_edit),
        absent,
        rejected,
      ))
    });
    ensure_that(
      observed,
      "rename families must preserve exact ordered edits, absence and parameter errors",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        let Ok(Some(ref edit)) = scenario.4.1 else {
          return false;
        };
        scenario.3.0.is_ok()
          && scenario.3.1.is_ok()
          && scenario.4.0
            == Ok(Some(PrepareRenameResponse::Range(Range::new(
              Position::new(0, 1),
              Position::new(0, 6),
            ))))
          && edit_observation(edit)
            == vec![
              (Range::new(Position::new(2, 1), Position::new(2, 6)), "renamed".into()),
              (Range::new(Position::new(0, 1), Position::new(0, 6)), "renamed".into()),
            ]
          && scenario.4 == scenario.5
          && matches!(scenario.6, Ok(None))
          && scenario.7.as_ref().is_err_and(|error| error.code == -32602)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }
}
