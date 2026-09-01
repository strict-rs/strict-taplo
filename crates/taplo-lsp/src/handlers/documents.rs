//! Open-document lifecycle transitions shared by both server runtimes.

use lsp_types::DidChangeTextDocumentParams;
use lsp_types::DidCloseTextDocumentParams;
use lsp_types::DidOpenTextDocumentParams;
use lsp_types::PublishDiagnosticsParams;
use lsp_types::Uri;
use taplo_lsp_async::Params;
use thiserror::Error;
use url::Url;

use super::diagnostics::DiagnosticBatch;
use super::diagnostics::DiagnosticError;
use super::diagnostics::cleared_diagnostics;
#[cfg(not(target_arch = "wasm32"))]
use super::diagnostics::collect_diagnostics_concurrent;
use super::diagnostics::collect_diagnostics_local;
use super::diagnostics::excluded_diagnostics;
use crate::lsp_ext::notification::DidChangeSchemaAssociationParams;
use crate::world::DocumentDisposition;
use crate::world::WorldError;
use crate::world::WorldState;

/// A typed failure while applying one document notification.
#[derive(Debug, Error)]
pub(super) enum DocumentNotificationError {
  /// Required notification parameters were omitted.
  #[error("document notification parameters are required")]
  MissingParameters,
  /// A document URI cannot be represented by Taplo's URL model.
  #[error("document URI `{uri:?}` is unsupported")]
  UnsupportedUri {
    /// Rejected LSP URI.
    uri: Uri,
  },
  /// A full-sync document change omitted its replacement text.
  #[error("full-sync document change omitted its replacement text")]
  MissingContentChange,
  /// The world transition failed.
  #[error(transparent)]
  World(#[from] Box<WorldError>),
  /// Diagnostics could not be collected.
  #[error(transparent)]
  Diagnostic(#[from] DiagnosticError),
  /// An included document disappeared before its diagnostics snapshot.
  #[error("included document `{document}` has no current diagnostics snapshot")]
  MissingDocumentSnapshot {
    /// Document expected to remain installed.
    document: Url,
  },
}

/// Client-visible output resulting from one committed document mutation.
pub(super) struct DocumentEffects {
  /// Updated schema associations.
  pub(super) associations: Vec<DidChangeSchemaAssociationParams>,
  /// Replacement diagnostics for the affected document.
  pub(super) diagnostics:  DiagnosticBatch,
}

/// Parse and install an opened document.
///
/// # Errors
///
/// Returns [`DocumentNotificationError`] when parameters, URI conversion, parsing, state
/// mutation, or diagnostic collection fails.
macro_rules! define_document_future_family {
  (
    $document_open:ident,
    $document_change:ident,
    $document_close:ident,
    $replace_document:ident,
    $world_replace_document:ident,
    $world_close_document:ident,
    $collect_diagnostics:ident;
    $future:ident,
    $environment:path,
    $transport:ident,
    $schema_execution:path;
    [$($value_bound:path),*]
  ) => {
    /// Parse and install an opened document.
    ///
    /// # Errors
    ///
    /// Returns [`DocumentNotificationError`] when parameters, URI conversion, parsing, state
    /// mutation, or diagnostic collection fails.
    #[allow(
      clippy::single_call_fn,
      reason = "one document-open transition per execution family, registered exactly once on its runtime family's ordered mutation lane"
    )]
    pub(super) fn $document_open<E: $environment>(
      world: &WorldState<E, $transport<E>>,
      params: Params<DidOpenTextDocumentParams>,
    ) -> $future<'_, Result<DocumentEffects, DocumentNotificationError>> {
      Box::pin(async move {
        let parameters = params.optional().ok_or(DocumentNotificationError::MissingParameters)?;
        let uri = parameters.text_document.uri;
        let document = document_url(&uri)?;
        $replace_document(world, document, parameters.text_document.text).await
      })
    }

    /// Parse and install the complete replacement text from a full-sync change.
    ///
    /// # Errors
    ///
    /// Returns [`DocumentNotificationError`] when parameters, replacement text, URI conversion,
    /// parsing, state mutation, or diagnostic collection fails.
    #[allow(
      clippy::single_call_fn,
      reason = "one full-sync document-change transition per execution family, registered exactly once on its runtime family's ordered \
                mutation lane"
    )]
    pub(super) fn $document_change<E: $environment>(
      world: &WorldState<E, $transport<E>>,
      params: Params<DidChangeTextDocumentParams>,
    ) -> $future<'_, Result<DocumentEffects, DocumentNotificationError>> {
      Box::pin(async move {
        let mut parameters = params.optional().ok_or(DocumentNotificationError::MissingParameters)?;
        let change = parameters
          .content_changes
          .pop()
          .ok_or(DocumentNotificationError::MissingContentChange)?;
        let uri = parameters.text_document.uri;
        let document = document_url(&uri)?;
        $replace_document(world, document, change.text).await
      })
    }

    /// Remove a closed document and clear its diagnostics.
    ///
    /// # Errors
    ///
    /// Returns [`DocumentNotificationError`] when parameters, URI conversion, state mutation, or
    /// diagnostic construction fails.
    #[allow(
      clippy::single_call_fn,
      reason = "one document-close transition per execution family, registered exactly once on its runtime family's ordered mutation lane"
    )]
    pub(super) fn $document_close<E: $environment>(
      world: &WorldState<E, $transport<E>>,
      params: Params<DidCloseTextDocumentParams>,
    ) -> $future<'_, Result<DocumentEffects, DocumentNotificationError>> {
      Box::pin(async move {
        let parameters = params.optional().ok_or(DocumentNotificationError::MissingParameters)?;
        let document = document_url(&parameters.text_document.uri)?;
        let associations = world.$world_close_document(&document).await.map_err(Box::new)?;
        Ok(DocumentEffects {
          associations,
          diagnostics: cleared_diagnostics(&document)?,
        })
      })
    }

    /// Apply the shared replacement transition and derive its client-visible effects.
    fn $replace_document<E: $environment>(
      world: &WorldState<E, $transport<E>>,
      document: Url,
      source: String,
    ) -> $future<'_, Result<DocumentEffects, DocumentNotificationError>> {
      Box::pin(async move {
        let update = world.$world_replace_document(&document, &source).await.map_err(Box::new)?;
        let diagnostics = match update.disposition {
          DocumentDisposition::Included => {
            $collect_diagnostics(world, &document)
              .await?
              .ok_or_else(|| DocumentNotificationError::MissingDocumentSnapshot {
                document: document.clone(),
              })?
          }
          DocumentDisposition::Excluded => excluded_diagnostics(&document)?,
        };
        Ok(DocumentEffects {
          associations: update.notifications,
          diagnostics,
        })
      })
    }
  };
}

define_lsp_execution_families!(
  handler
  define_document_future_family;
  (document_open_local, document_open_concurrent),
  (document_change_local, document_change_concurrent),
  (document_close_local, document_close_concurrent),
  (replace_document_local, replace_document_concurrent),
  (replace_document, replace_document_concurrent),
  (close_document, close_document_concurrent),
  (collect_diagnostics_local, collect_diagnostics_concurrent),
);

/// Convert one LSP URI into the URL representation used by workspace ownership.
fn document_url(uri: &Uri) -> Result<Url, DocumentNotificationError> {
  super::uri::to_url(uri).ok_or_else(|| DocumentNotificationError::UnsupportedUri {
    uri: uri.clone()
  })
}

/// Convert a diagnostics batch into the standard publish-notification payload.
pub(super) fn publish_params(batch: DiagnosticBatch) -> PublishDiagnosticsParams {
  PublishDiagnosticsParams {
    uri:         batch.uri,
    diagnostics: batch.diagnostics,
    version:     None,
  }
}

#[cfg(test)]
mod tests {
  use std::str::FromStr as _;
  use std::sync::Arc;

  use futures::executor::block_on;
  use lsp_types::DidChangeTextDocumentParams;
  use lsp_types::DidOpenTextDocumentParams;
  use lsp_types::TextDocumentItem;
  use lsp_types::Uri;
  use lsp_types::VersionedTextDocumentIdentifier;
  use serde_json::json;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_ok;
  use taplo_common::config::Config;
  use taplo_lsp_async::Params;
  use url::Url;

  use super::DocumentNotificationError;
  use super::document_change_local;
  use super::document_open_local;
  use super::document_url;
  use crate::handlers::test_support::local_world;
  use crate::world::DocumentDisposition;

  /// Parse one wire document URI.
  fn uri(value: &str) -> Result<Uri, TestFailure> {
    ensure_ok(Uri::from_str(value), "the document-notification URI fixture must parse")
  }

  #[test]
  fn document_uri_and_change_validation_reject_missing_content_before_mutation() -> Result<(), TestFailure> {
    let absolute = uri("file:///workspace/document.toml")?;
    ensure(
      ensure_ok(document_url(&absolute), "the absolute document URI must convert to a URL")?
        == ensure_ok(
          Url::parse("file:///workspace/document.toml"),
          "the absolute document URL fixture must parse",
        )?,
      "an absolute document URI must retain its URL identity",
    )?;
    ensure(
      matches!(
        document_url(&uri("workspace/relative.toml")?),
        Err(DocumentNotificationError::UnsupportedUri { .. })
      ),
      "a relative document URI reference must be rejected before state mutation",
    )?;

    let world = local_world()?;
    let missing_change = DidChangeTextDocumentParams {
      text_document:   VersionedTextDocumentIdentifier {
        uri:     absolute,
        version: 2,
      },
      content_changes: Vec::new(),
    };
    ensure(
      matches!(
        block_on(document_change_local(&world, Params::from(Some(missing_change)))),
        Err(DocumentNotificationError::MissingContentChange)
      ),
      "a full-sync change without replacement text must retain its typed validation failure",
    )?;
    ensure(
      matches!(
        block_on(document_change_local(&world, Params::<DidChangeTextDocumentParams>::from(None),)),
        Err(DocumentNotificationError::MissingParameters)
      ),
      "a change without parameters must retain its distinct typed validation failure",
    )
  }

  #[test]
  fn excluded_document_open_retains_state_and_publishes_one_explanatory_hint() -> Result<(), TestFailure> {
    block_on(async {
      let world = local_world()?;
      world.set_default_config(Arc::new(Config {
        include: Some(vec![String::from("included.toml")]),
        ..Config::default()
      }));
      let schema_disabled = json!({
        "schema": {
          "enabled": false,
          "catalogs": []
        }
      });
      drop(ensure_ok(
        world.apply_configuration_values_local(Some(&schema_disabled), &[]).await,
        "the excluded-document configuration must commit",
      )?);

      let effects = ensure_ok(
        document_open_local(
          &world,
          Params::from(Some(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
              uri:         uri("file:///workspace/excluded.toml")?,
              language_id: String::from("toml"),
              version:     1,
              text:        String::from("value = 1\n"),
            },
          })),
        )
        .await,
        "an excluded document must remain a successful open transition",
      )?;
      let association_facts = effects.associations.first().map(|association| {
        (
          association.document_uri.as_str(),
          association.schema_uri.is_none(),
          association.meta.is_none(),
        )
      });
      ensure(
        (effects.associations.len(), association_facts) == (1, Some(("file:///workspace/excluded.toml", true, true))),
        "an excluded open must publish one null effective-schema notification that clears stale client state",
      )?;
      let diagnostic_message = effects
        .diagnostics
        .diagnostics
        .first()
        .map(|diagnostic| diagnostic.message.as_str());
      ensure(
        (effects.diagnostics.diagnostics.len(), diagnostic_message) == (1, Some("this document has been excluded")),
        "an excluded open must publish exactly one explanatory hint",
      )?;
      let dispositions = world.open_document_dispositions().await;
      ensure(
        dispositions.len() == 1,
        "an excluded open must remain represented in the world document set",
      )?;
      ensure(
        dispositions
          .first()
          .is_some_and(|disposition_entry| disposition_entry.1 == DocumentDisposition::Excluded),
        "the retained excluded document must expose its exclusion disposition",
      )
    })
  }
}
