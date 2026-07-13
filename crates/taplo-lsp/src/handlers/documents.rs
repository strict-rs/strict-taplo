//! Open-document lifecycle and document-owned schema association routing.

use lsp_async_stub::Context;
use lsp_async_stub::Params;
use lsp_async_stub::RequestWriter;
use lsp_async_stub::util::Mapper;
use lsp_types::Diagnostic;
use lsp_types::DiagnosticSeverity;
use lsp_types::DidChangeTextDocumentParams;
use lsp_types::DidCloseTextDocumentParams;
use lsp_types::DidOpenTextDocumentParams;
use lsp_types::DidSaveTextDocumentParams;
use lsp_types::PublishDiagnosticsParams;
use lsp_types::Uri;
use lsp_types::notification;
use taplo_common::environment::Environment;

use crate::diagnostics;
use crate::world::DocumentState;
use crate::world::WorkspaceState;
use crate::world::World;
use crate::world::send_association_notifications;

/// Parse one full document replacement outside every workspace lock.
fn parse_document(source: &str) -> DocumentState {
  let parse = taplo::parser::parse(source);
  let mapper = Mapper::new_utf16(source, false);
  let dom = parse.clone().into_dom();
  DocumentState {
    parse,
    dom,
    mapper,
  }
}

/// Replace one open document and its document-derived associations, or remove it if excluded.
fn replace_document<E: Environment>(
  workspace: &mut WorkspaceState<E>,
  environment: &E,
  document_url: &url::Url,
  document: DocumentState,
) -> Option<Vec<crate::lsp_ext::notification::DidChangeSchemaAssociationParams>> {
  if workspace.document_is_excluded(environment, document_url) {
    remove_document(workspace, document_url);
    return None;
  }

  workspace.documents.insert(document_url.clone(), document.clone());
  workspace.schemas.associations().add_from_document(document_url, &document.dom);
  Some(workspace.association_notifications())
}

/// Remove one open document and only the associations derived from its current source.
fn remove_document<E: Environment>(workspace: &mut WorkspaceState<E>, document_url: &url::Url) {
  workspace.documents.remove(document_url);
  workspace.schemas.associations().remove_from_document(document_url);
}

/// Publish the single hint that represents the live excluded-document contract.
async fn publish_excluded<E: Environment>(context: &mut Context<World<E>>, document_uri: Uri) {
  let result = context
    .write_notification::<notification::PublishDiagnostics, _>(Some(PublishDiagnosticsParams {
      uri:         document_uri,
      diagnostics: vec![Diagnostic {
        range: Default::default(),
        severity: Some(DiagnosticSeverity::HINT),
        source: Some("Even Better TOML".into()),
        message: "this document has been excluded".into(),
        ..Diagnostic::default()
      }],
      version:     None,
    }))
    .await;
  if let Err(error) = result {
    tracing::error!(%error, "failed to publish excluded-document diagnostic");
  }
}

#[tracing::instrument(skip_all)]
pub(crate) async fn document_open<E: Environment>(mut context: Context<World<E>>, params: Params<DidOpenTextDocumentParams>) {
  let Some(params) = params.optional() else {
    return;
  };
  let Some(document_url) = crate::uri::to_url(&params.text_document.uri) else {
    return;
  };

  let document = parse_document(&params.text_document.text);
  let workspace = context.workspace_for_document(&document_url).await;
  let notifications = {
    let mut workspace = workspace.write().await;
    replace_document(&mut workspace, &context.env, &document_url, document)
  };

  let Some(notifications) = notifications else {
    publish_excluded(&mut context, params.text_document.uri).await;
    return;
  };
  send_association_notifications(context.clone(), notifications).await;
  diagnostics::publish_diagnostics(context, document_url).await;
}

#[tracing::instrument(skip_all)]
pub(crate) async fn document_change<E: Environment>(mut context: Context<World<E>>, params: Params<DidChangeTextDocumentParams>) {
  let Some(mut params) = params.optional() else {
    return;
  };
  let Some(change) = params.content_changes.pop() else {
    return;
  };
  let Some(document_url) = crate::uri::to_url(&params.text_document.uri) else {
    return;
  };

  let document = parse_document(&change.text);
  let workspace = context.workspace_for_document(&document_url).await;
  let notifications = {
    let mut workspace = workspace.write().await;
    replace_document(&mut workspace, &context.env, &document_url, document)
  };

  let Some(notifications) = notifications else {
    publish_excluded(&mut context, params.text_document.uri).await;
    return;
  };
  send_association_notifications(context.clone(), notifications).await;
  diagnostics::publish_diagnostics(context, document_url).await;
}

#[tracing::instrument(skip_all)]
pub(crate) async fn document_save<E: Environment>(_context: Context<World<E>>, _params: Params<DidSaveTextDocumentParams>) {}

#[tracing::instrument(skip_all)]
pub(crate) async fn document_close<E: Environment>(context: Context<World<E>>, params: Params<DidCloseTextDocumentParams>) {
  let Some(params) = params.optional() else {
    return;
  };
  let Some(document_url) = crate::uri::to_url(&params.text_document.uri) else {
    return;
  };

  let workspace = context.workspace_for_document(&document_url).await;
  {
    let mut workspace = workspace.write().await;
    remove_document(&mut workspace, &document_url);
  }
  diagnostics::clear_diagnostics(context, document_url).await;
}

#[cfg(test)]
mod tests {
  use std::path::Path;

  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use taplo_common::config::Config;
  use taplo_common::schema::associations::AssociationRule;
  use taplo_common::schema::associations::SchemaAssociation;
  use taplo_common::schema::associations::priority;
  use taplo_common::schema::associations::source;
  use url::Url;

  use super::parse_document;
  use super::remove_document;
  use super::replace_document;
  use crate::test_support::TestEnvironment;
  use crate::test_support::ensure_anyhow;
  use crate::world::WorkspaceRoot;
  use crate::world::WorkspaceState;

  /// Parse one document URL fixture.
  fn url(value: &str) -> Result<Url, TestFailure> {
    ensure_ok(Url::parse(value), "the document lifecycle fixture URL must parse")
  }

  /// Count associations owned by one source kind.
  fn source_count(workspace: &WorkspaceState<TestEnvironment>, expected: &str) -> usize {
    workspace
      .schemas
      .associations()
      .read()
      .iter()
      .filter(|(_, association)| association.meta["source"] == expected)
      .count()
  }

  #[test]
  fn replacement_refreshes_derived_ownership_and_preserves_manual_associations() -> Result<(), TestFailure> {
    let environment = TestEnvironment::default();
    let mut workspace = WorkspaceState::new(environment.clone(), WorkspaceRoot::Detached, None);
    let document_url = url("file:///workspace/file.toml")?;
    workspace
      .schemas
      .associations()
      .add(AssociationRule::Url(document_url.clone()), SchemaAssociation {
        url:      url("https://example.com/manual.json")?,
        meta:     serde_json::json!({ "source": source::MANUAL }),
        priority: priority::MAX,
      });

    let notifications = ensure_some(
      replace_document(
        &mut workspace,
        &environment,
        &document_url,
        parse_document("#:schema https://example.com/directive.json\nvalue = 1\n"),
      ),
      "an included open document must be stored",
    )?;
    ensure(
      !notifications.is_empty(),
      "the inserted document must participate in its own association notifications",
    )?;
    ensure_eq(
      &source_count(&workspace, source::DIRECTIVE),
      &1,
      "opening a directive document must establish derived ownership",
    )?;
    ensure_eq(
      &source_count(&workspace, source::MANUAL),
      &1,
      "document refresh must preserve a manual URL association",
    )?;

    let _ = replace_document(
      &mut workspace,
      &environment,
      &document_url,
      parse_document("\"$schema\" = \"https://example.com/field.json\"\nvalue = 2\n"),
    );
    ensure_eq(
      &source_count(&workspace, source::DIRECTIVE),
      &0,
      "change must remove the previous directive",
    )?;
    ensure_eq(
      &source_count(&workspace, source::SCHEMA_FIELD),
      &1,
      "change must install the current schema field",
    )?;
    let snapshot = ensure_some(
      workspace.document_snapshot(&document_url),
      "the changed document snapshot must exist",
    )?;
    ensure_eq(
      &ensure_ok(
        serde_json::to_value(snapshot.document.dom),
        "the changed document DOM must serialize",
      )?,
      &serde_json::json!({ "$schema": "https://example.com/field.json", "value": 2 }),
      "change must replace parse and DOM state rather than retaining the old source",
    )?;

    let _ = replace_document(&mut workspace, &environment, &document_url, parse_document("value = 3\n"));
    ensure_eq(
      &source_count(&workspace, source::SCHEMA_FIELD),
      &0,
      "removing the schema field must remove its prior derived association",
    )?;
    ensure_eq(
      &source_count(&workspace, source::MANUAL),
      &1,
      "plain document changes must still preserve manual ownership",
    )
  }

  #[test]
  fn disablement_hides_but_retains_derived_state_and_close_removes_only_that_state() -> Result<(), TestFailure> {
    let environment = TestEnvironment::default();
    let mut workspace = WorkspaceState::new(environment.clone(), WorkspaceRoot::Detached, None);
    workspace.config.schema.enabled = false;
    let document_url = url("file:///workspace/file.toml")?;
    workspace
      .schemas
      .associations()
      .add(AssociationRule::Url(document_url.clone()), SchemaAssociation {
        url:      url("https://example.com/manual.json")?,
        meta:     serde_json::json!({ "source": source::MANUAL }),
        priority: priority::MAX,
      });

    let notifications = ensure_some(
      replace_document(
        &mut workspace,
        &environment,
        &document_url,
        parse_document("#:schema https://example.com/directive.json\nvalue = 1\n"),
      ),
      "schema disablement must not discard document ownership",
    )?;
    ensure_eq(
      &source_count(&workspace, source::DIRECTIVE),
      &1,
      "disabled validation must retain the internal directive association",
    )?;
    let notification = ensure_some(
      notifications
        .iter()
        .find(|notification| notification.document_uri == document_url),
      "the disabled document must still emit an ownership notification",
    )?;
    ensure(
      notification.schema_uri.is_none(),
      "disabled schema exposure must publish no active schema",
    )?;

    remove_document(&mut workspace, &document_url);
    ensure(
      workspace.document_snapshot(&document_url).is_none(),
      "close must remove retained document state",
    )?;
    ensure_eq(
      &source_count(&workspace, source::DIRECTIVE),
      &0,
      "close must remove the document-derived directive",
    )?;
    ensure_eq(
      &source_count(&workspace, source::MANUAL),
      &1,
      "close must preserve a manual association for the same URL",
    )
  }

  #[test]
  fn exclusion_removes_file_state_but_never_applies_to_non_file_urls() -> Result<(), TestFailure> {
    let environment = TestEnvironment::default();
    let mut workspace = WorkspaceState::new(environment.clone(), WorkspaceRoot::Detached, None);
    let file_url = url("file:///workspace/excluded.toml")?;
    let _ = replace_document(
      &mut workspace,
      &environment,
      &file_url,
      parse_document("#:schema https://example.com/old.json\nvalue = 1\n"),
    );

    let mut excluding = Config {
      include: Some(vec!["**/included.toml".into()]),
      ..Config::default()
    };
    ensure_anyhow(
      excluding.prepare(&environment, Path::new("/workspace")),
      "the exclusion rule fixture must prepare",
    )?;
    workspace.taplo_config = excluding;
    ensure(
      replace_document(&mut workspace, &environment, &file_url, parse_document("value = 2\n")).is_none(),
      "an excluded file replacement must signal exclusion",
    )?;
    ensure(
      workspace.document_snapshot(&file_url).is_none(),
      "an excluded file must not remain open",
    )?;
    ensure_eq(
      &source_count(&workspace, source::DIRECTIVE),
      &0,
      "exclusion must remove prior document-derived associations",
    )?;

    let non_file = url("untitled:document")?;
    ensure(
      replace_document(&mut workspace, &environment, &non_file, parse_document("value = 3\n")).is_some(),
      "filesystem glob exclusion must not apply to a non-file URL",
    )?;
    ensure(
      workspace.document_snapshot(&non_file).is_some(),
      "the non-file document must remain stored",
    )
  }
}
