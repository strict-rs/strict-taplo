//! Initialization planning and execution-model-specific workspace setup.

use std::sync::Arc;

use lsp_types::CompletionOptions;
use lsp_types::DocumentLinkOptions;
use lsp_types::FoldingRangeProviderCapability;
use lsp_types::HoverProviderCapability;
use lsp_types::InitializeParams;
use lsp_types::InitializeResult;
use lsp_types::OneOf;
use lsp_types::RenameOptions;
use lsp_types::SemanticTokensFullOptions;
use lsp_types::SemanticTokensLegend;
use lsp_types::SemanticTokensOptions;
use lsp_types::SemanticTokensServerCapabilities;
use lsp_types::ServerCapabilities;
use lsp_types::ServerInfo;
use lsp_types::TextDocumentSyncCapability;
use lsp_types::TextDocumentSyncKind;
use lsp_types::Uri;
use lsp_types::WorkDoneProgressOptions;
use lsp_types::WorkspaceFoldersServerCapabilities;
use lsp_types::WorkspaceServerCapabilities;
use thiserror::Error;
use url::Url;

use super::semantic_tokens;
use crate::config::InitConfig;
use crate::lsp_ext::notification::DidChangeSchemaAssociationParams;
use crate::world::WorldError;
use crate::world::WorldState;

/// A typed initialization failure before the protocol adapter maps it to JSON-RPC.
#[derive(Debug, Error)]
pub(super) enum InitializationError {
  /// Initialization options do not match [`InitConfig`].
  #[error("invalid initialization options")]
  Options {
    /// Underlying JSON shape failure.
    #[source]
    source: serde_json::Error,
  },
  /// A workspace URI cannot be represented as an absolute URL.
  #[error("workspace URI `{uri:?}` is unsupported")]
  UnsupportedWorkspaceUri {
    /// Rejected LSP URI.
    uri: Uri,
  },
  /// World construction or workspace initialization failed.
  #[error(transparent)]
  World(#[from] Box<WorldError>),
}

/// Successful initialization plus output deferred until after the response is written.
pub(super) struct InitializationEffects {
  /// Standard LSP initialization response.
  pub(super) result:       InitializeResult,
  /// Schema associations established while initializing workspace folders.
  pub(super) associations: Vec<DidChangeSchemaAssociationParams>,
}

/// Initialize local workspace state from client parameters.
///
/// # Errors
///
/// Returns [`InitializationError`] when options, workspace URIs, or local workspace
/// initialization are invalid.
macro_rules! define_initialization_future_family {
  (
    $initialize:ident,
    $initialize_roots:ident;
    $future:ident,
    $environment:path,
    $transport:ident,
    $schema_execution:path;
    [$($value_bound:path),*]
  ) => {
    /// Initialize workspace state from client parameters.
    ///
    /// # Errors
    ///
    /// Returns [`InitializationError`] when options, workspace URIs, or workspace initialization
    /// are invalid.
    #[allow(
      clippy::single_call_fn,
      reason = "one initialization transition per execution family, registered exactly once by its runtime family"
    )]
    pub(super) fn $initialize<E: $environment>(
      world: &WorldState<E, $transport<E>>,
      params: InitializeParams,
    ) -> $future<'_, Result<InitializationEffects, InitializationError>> {
      Box::pin(async move {
        let prepared = prepare_initialization(params)?;
        let associations = world
          .$initialize_roots(prepared.init_config, prepared.workspace_roots)
          .await
          .map_err(Box::new)?;
        Ok(InitializationEffects {
          result: initialization_result(),
          associations,
        })
      })
    }
  };
}

define_lsp_execution_families!(
  handler
  define_initialization_future_family;
  (initialize_local, initialize_concurrent),
  (initialize_roots_local, initialize_roots_concurrent),
);

/// Validated initialization inputs independent of execution model.
struct PreparedInitialization {
  /// Initialization configuration.
  init_config:     Arc<InitConfig>,
  /// Absolute workspace roots in client order.
  workspace_roots: Vec<Url>,
}

/// Decode initialization options and workspace roots before mutating world state.
fn prepare_initialization(params: InitializeParams) -> Result<PreparedInitialization, InitializationError> {
  let init_config = match params.initialization_options {
    Some(options) => serde_json::from_value(options).map_err(|source| InitializationError::Options {
      source,
    })?,
    None => InitConfig::default(),
  };
  let mut workspace_roots = Vec::new();
  for workspace in params.workspace_folders.unwrap_or_default() {
    workspace_roots.push(
      super::uri::to_url(&workspace.uri).ok_or(InitializationError::UnsupportedWorkspaceUri {
        uri: workspace.uri
      })?,
    );
  }
  Ok(PreparedInitialization {
    init_config: Arc::new(init_config),
    workspace_roots,
  })
}

/// Construct the capabilities shared by local and concurrent servers.
fn initialization_result() -> InitializeResult {
  InitializeResult {
    capabilities:    ServerCapabilities {
      workspace: Some(WorkspaceServerCapabilities {
        workspace_folders: Some(WorkspaceFoldersServerCapabilities {
          supported:            Some(true),
          change_notifications: Some(OneOf::Left(true)),
        }),
        ..Default::default()
      }),
      text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
      semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(SemanticTokensOptions {
        work_done_progress_options: WorkDoneProgressOptions {
          work_done_progress: false.into(),
        },
        legend: SemanticTokensLegend {
          token_types:     semantic_tokens::TokenType::LEGEND.into(),
          token_modifiers: Vec::new(),
        },
        full: Some(SemanticTokensFullOptions::Bool(true)),
        range: Some(false),
      })),
      rename_provider: Some(OneOf::Right(RenameOptions {
        prepare_provider:           Some(true),
        work_done_progress_options: WorkDoneProgressOptions {
          work_done_progress: false.into(),
        },
      })),
      folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
      document_symbol_provider: Some(OneOf::Left(true)),
      document_formatting_provider: Some(OneOf::Left(true)),
      hover_provider: Some(HoverProviderCapability::Simple(true)),
      completion_provider: Some(CompletionOptions {
        resolve_provider: Some(false),
        trigger_characters: Some(vec![".".into(), "=".into(), "[".into(), "{".into(), ",".into(), "\"".into()]),
        ..Default::default()
      }),
      document_link_provider: Some(DocumentLinkOptions {
        resolve_provider:           None,
        work_done_progress_options: WorkDoneProgressOptions {
          work_done_progress: false.into(),
        },
      }),
      ..Default::default()
    },
    server_info:     Some(ServerInfo {
      name:    "Taplo".into(),
      version: Some(env!("CARGO_PKG_VERSION").into()),
    }),
    offset_encoding: None,
  }
}

#[cfg(test)]
mod tests {
  use std::path::Path;

  use lsp_types::InitializeParams;
  use lsp_types::SemanticTokensServerCapabilities;
  use lsp_types::TextDocumentSyncCapability;
  use lsp_types::TextDocumentSyncKind;
  use serde_json::json;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_ok;

  use super::InitializationError;
  use super::initialization_result;
  use super::prepare_initialization;

  /// Decode initialization parameters through their real wire representation.
  fn parameters(value: serde_json::Value) -> Result<InitializeParams, TestFailure> {
    ensure_ok(serde_json::from_value(value), "the initialization parameter fixture must decode")
  }

  #[test]
  fn initialization_preparation_preserves_options_and_workspace_order() -> Result<(), TestFailure> {
    let prepared = ensure_ok(
      prepare_initialization(parameters(json!({
        "processId": null,
        "capabilities": {},
        "initializationOptions": {
          "cachePath": "/workspace/cache",
          "configurationSection": "taplo"
        },
        "workspaceFolders": [
          {"uri": "file:///workspace/first", "name": "first"},
          {"uri": "file:///workspace/second", "name": "second"}
        ]
      }))?),
      "valid initialization options and folders must prepare",
    )?;
    let workspace_roots = prepared.workspace_roots.iter().map(url::Url::as_str).collect::<Vec<_>>();
    ensure(
      (
        prepared.init_config.cache_path.as_deref(),
        prepared.init_config.configuration_section.as_str(),
        workspace_roots,
      ) == (Some(Path::new("/workspace/cache")), "taplo", vec![
        "file:///workspace/first", "file:///workspace/second",
      ]),
      "initialization preparation must preserve host options and client workspace order",
    )?;

    let defaults = ensure_ok(
      prepare_initialization(parameters(json!({
        "processId": null,
        "capabilities": {}
      }))?),
      "omitted initialization options and folders must use defaults",
    )?;
    ensure(
      (
        defaults.init_config.cache_path.as_ref(),
        defaults.init_config.configuration_section.as_str(),
        defaults.workspace_roots.is_empty(),
      ) == (None, "evenBetterToml", true),
      "omitted initialization values must retain the public defaults",
    )
  }

  #[test]
  fn initialization_preparation_rejects_invalid_options_and_relative_workspace_uris() -> Result<(), TestFailure> {
    ensure(
      matches!(
        prepare_initialization(parameters(json!({
          "processId": null,
          "capabilities": {},
          "initializationOptions": 7
        }))?),
        Err(InitializationError::Options { .. })
      ),
      "non-object initialization options must retain their typed decode source",
    )?;
    ensure(
      matches!(
        prepare_initialization(parameters(json!({
          "processId": null,
          "capabilities": {},
          "workspaceFolders": [
            {"uri": "workspace/relative", "name": "relative"}
          ]
        }))?),
        Err(InitializationError::UnsupportedWorkspaceUri { .. })
      ),
      "a relative workspace URI reference must be rejected before world initialization",
    )
  }

  #[test]
  fn initialization_capabilities_advertise_the_complete_shared_protocol() -> Result<(), TestFailure> {
    let result = initialization_result();
    let server_identity = result
      .server_info
      .as_ref()
      .map(|server| (server.name.as_str(), server.version.as_deref()));
    ensure(
      server_identity == Some(("Taplo", Some(env!("CARGO_PKG_VERSION")))),
      "initialization must identify the Taplo server and current package version",
    )?;
    ensure(
      result.capabilities.text_document_sync == Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
      "initialization must advertise full document synchronization",
    )?;
    let semantic_token_facts = match result.capabilities.semantic_tokens_provider {
      Some(SemanticTokensServerCapabilities::SemanticTokensOptions(ref options)) => Some((
        options.legend.token_modifiers.len(),
        options.legend.token_types.len(),
        options.range,
      )),
      Some(SemanticTokensServerCapabilities::SemanticTokensRegistrationOptions(_)) | None => None,
    };
    ensure(
      semantic_token_facts == Some((0, 2, Some(false))),
      "initialization must advertise exactly the implemented semantic token types without ghost modifiers",
    )?;
    ensure(
      result
        .capabilities
        .workspace
        .and_then(|workspace| workspace.workspace_folders)
        .is_some_and(|folders| folders.supported == Some(true)),
      "initialization must advertise dynamic workspace-folder support",
    )
  }
}
