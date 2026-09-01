//! Workspace-folder topology transitions shared by both server runtimes.

use lsp_types::DidChangeWorkspaceFoldersParams;
use lsp_types::Uri;
use thiserror::Error;
use url::Url;

use super::diagnostics::DiagnosticBatch;
use super::diagnostics::DiagnosticError;
use crate::lsp_ext::notification::DidChangeSchemaAssociationParams;
use crate::world::WorldError;
use crate::world::WorldState;

/// A typed workspace-folder transition failure.
#[derive(Debug, Error)]
pub(super) enum WorkspaceChangeError {
  /// A workspace URI cannot be represented as an absolute URL.
  #[error("workspace URI `{uri:?}` is unsupported")]
  UnsupportedWorkspaceUri {
    /// Rejected LSP URI.
    uri: Uri,
  },
  /// A topology or workspace initialization transition failed.
  #[error(transparent)]
  World(#[from] Box<WorldError>),
  /// Refreshing diagnostics for a moved document failed.
  #[error(transparent)]
  Diagnostic(#[from] DiagnosticError),
}

/// Client-visible output after one committed workspace-folder transition.
pub(super) struct WorkspaceChangeEffects {
  /// Current schema associations after redistribution.
  pub(super) associations: Vec<DidChangeSchemaAssociationParams>,
  /// Current diagnostics for every redistributed open document.
  pub(super) diagnostics:  Vec<DiagnosticBatch>,
}

/// Apply workspace-folder changes through local host capabilities.
///
/// # Errors
///
/// Returns [`WorkspaceChangeError`] when a URI, topology mutation, workspace initialization, or
/// diagnostics refresh fails.
macro_rules! define_workspace_future_family {
  (
    $workspace_change:ident,
    $finish_change:ident,
    $change_roots:ident,
    $collect_document_diagnostics:ident;
    $future:ident,
    $environment:path,
    $transport:ident,
    $schema_execution:path;
    [$($value_bound:path),*]
  ) => {
    /// Apply workspace-folder changes through this execution family's host capabilities.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceChangeError`] when a URI, topology mutation, workspace initialization,
    /// or diagnostics refresh fails.
    #[allow(
      clippy::single_call_fn,
      reason = "one workspace-folder transition per execution family, registered exactly once by its runtime family"
    )]
    pub(super) fn $workspace_change<E: $environment>(
      world: &WorldState<E, $transport<E>>,
      params: DidChangeWorkspaceFoldersParams,
    ) -> $future<'_, Result<WorkspaceChangeEffects, WorkspaceChangeError>> {
      Box::pin(async move {
        let change = prepare_change(params)?;
        let update = world.$change_roots(&change.removed, &change.added).await.map_err(Box::new)?;
        $finish_change(world, update.notifications, update.documents).await
      })
    }

    /// Deduplicate moved documents and collect stable post-mutation effects.
    #[allow(
      clippy::single_call_fn,
      reason = "the named continuation isolates moved-document deduplication and the post-mutation diagnostics sweep from the topology \
                mutation that produced them"
    )]
    fn $finish_change<E: $environment>(
      world: &WorldState<E, $transport<E>>,
      associations: Vec<DidChangeSchemaAssociationParams>,
      mut documents: Vec<Url>,
    ) -> $future<'_, Result<WorkspaceChangeEffects, WorkspaceChangeError>> {
      Box::pin(async move {
        documents.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        documents.dedup();
        let diagnostics = super::$collect_document_diagnostics(world, |document| {
          documents
            .binary_search_by(|candidate| candidate.as_str().cmp(document.as_str()))
            .is_ok()
        })
        .await?;
        Ok(WorkspaceChangeEffects {
          associations,
          diagnostics,
        })
      })
    }
  };
}

define_lsp_execution_families!(
  handler
  define_workspace_future_family;
  (workspace_change_local, workspace_change_concurrent),
  (finish_change_local, finish_change_concurrent),
  (change_roots_local, change_roots_concurrent),
  (
    collect_document_diagnostics_local,
    collect_document_diagnostics_concurrent
  ),
);

/// Validated root URLs for one workspace-folder event.
struct PreparedWorkspaceChange {
  /// Roots removed before additions are processed.
  removed: Vec<Url>,
  /// Roots added after removals are complete.
  added:   Vec<Url>,
}

/// Validate every workspace URI before applying topology mutations.
fn prepare_change(params: DidChangeWorkspaceFoldersParams) -> Result<PreparedWorkspaceChange, WorkspaceChangeError> {
  Ok(PreparedWorkspaceChange {
    removed: workspace_urls(params.event.removed)?,
    added:   workspace_urls(params.event.added)?,
  })
}

/// Convert all workspace folders into absolute URL identities.
fn workspace_urls(workspaces: Vec<lsp_types::WorkspaceFolder>) -> Result<Vec<Url>, WorkspaceChangeError> {
  workspaces
    .into_iter()
    .map(|workspace| {
      super::uri::to_url(&workspace.uri).ok_or(WorkspaceChangeError::UnsupportedWorkspaceUri {
        uri: workspace.uri
      })
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use std::str::FromStr as _;

  use futures::executor::block_on;
  use lsp_types::DidChangeWorkspaceFoldersParams;
  use lsp_types::Uri;
  use lsp_types::WorkspaceFolder;
  use lsp_types::WorkspaceFoldersChangeEvent;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_ok;
  use url::Url;

  #[cfg(not(target_arch = "wasm32"))]
  use crate::handlers::test_support::concurrent_world;
  use crate::handlers::test_support::ensure_no_client_effects;
  use crate::handlers::test_support::local_world;

  /// Construct one workspace-folder change event.
  fn change(removed: Vec<WorkspaceFolder>, added: Vec<WorkspaceFolder>) -> DidChangeWorkspaceFoldersParams {
    DidChangeWorkspaceFoldersParams {
      event: WorkspaceFoldersChangeEvent {
        added,
        removed,
      },
    }
  }

  /// Construct one workspace folder with checked wire URI syntax.
  fn folder(uri: &str, name: &str) -> Result<WorkspaceFolder, TestFailure> {
    Ok(WorkspaceFolder {
      uri:  ensure_ok(Uri::from_str(uri), "the workspace-folder URI fixture must parse")?,
      name: name.to_owned(),
    })
  }

  #[test]
  fn workspace_change_preparation_preserves_order_and_rejects_relative_references() -> Result<(), TestFailure> {
    let removed = folder("file:///workspace/old", "old")?;
    let added = folder("file:///workspace/new", "new")?;
    let prepared = ensure_ok(
      super::prepare_change(change(vec![removed], vec![added])),
      "absolute workspace-folder changes must prepare",
    )?;
    let expected_removed = ensure_ok(Url::parse("file:///workspace/old"), "the removed workspace URL fixture must parse")?;
    let expected_added = ensure_ok(Url::parse("file:///workspace/new"), "the added workspace URL fixture must parse")?;
    ensure(
      (prepared.removed, prepared.added) == (vec![expected_removed], vec![expected_added]),
      "workspace preparation must retain removed-then-added URL identity and order",
    )?;

    let relative = folder("workspace/relative", "relative")?;
    ensure(
      matches!(
        super::prepare_change(change(Vec::new(), vec![relative])),
        Err(super::WorkspaceChangeError::UnsupportedWorkspaceUri { .. })
      ),
      "a relative workspace URI reference must be rejected before topology mutation",
    )
  }

  #[test]
  fn empty_workspace_changes_are_no_ops_across_execution_families() -> Result<(), TestFailure> {
    block_on(async {
      let local_world = local_world()?;
      let local_effects = ensure_ok(
        super::workspace_change_local(&local_world, change(Vec::new(), Vec::new())).await,
        "an empty local workspace change must succeed",
      )?;
      ensure_no_client_effects(
        &local_effects.associations,
        &local_effects.diagnostics,
        "an empty local workspace change must publish no client effects",
      )?;

      #[cfg(not(target_arch = "wasm32"))]
      {
        let concurrent_world = concurrent_world()?;
        let concurrent_effects = ensure_ok(
          super::workspace_change_concurrent(&concurrent_world, change(Vec::new(), Vec::new())).await,
          "an empty concurrent workspace change must succeed",
        )?;
        ensure_no_client_effects(
          &concurrent_effects.associations,
          &concurrent_effects.diagnostics,
          "an empty concurrent workspace change must publish no client effects",
        )?;
      };
      Ok(())
    })
  }
}
