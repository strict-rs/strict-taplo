use std::sync::Arc;

use lsp_async_stub::Context;
use lsp_async_stub::Params;
use lsp_async_stub::rpc::Error;
use lsp_types::CompletionOptions;
use lsp_types::DocumentLinkOptions;
use lsp_types::FoldingRangeProviderCapability;
use lsp_types::HoverProviderCapability;
use lsp_types::InitializeParams;
use lsp_types::InitializeResult;
use lsp_types::InitializedParams;
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
use lsp_types::WorkDoneProgressOptions;
use lsp_types::WorkspaceFoldersServerCapabilities;
use lsp_types::WorkspaceServerCapabilities;
use taplo_common::environment::Environment;

use super::semantic_tokens;
use super::update_configuration;
use crate::World;
use crate::config::InitConfig;
use crate::world::send_association_notifications;

#[tracing::instrument(skip_all)]
pub async fn initialize<E: Environment>(context: Context<World<E>>, params: Params<InitializeParams>) -> Result<InitializeResult, Error> {
  let p = params.required()?;

  if let Some(init_opts) = p.initialization_options {
    match serde_json::from_value::<InitConfig>(init_opts) {
      Ok(c) => context.init_config.store(Arc::new(c)),
      Err(error) => {
        tracing::error!(%error, "invalid initialization options");
      }
    }
  }

  if let Some(workspaces) = p.workspace_folders {
    let init_config = context.init_config.load();
    let mut notifications = Vec::new();

    for workspace in workspaces {
      let Some(ws_url) = crate::uri::to_url(&workspace.uri) else {
        continue;
      };
      let (workspace, _) = context.add_workspace_root(ws_url).await;
      let mut workspace = workspace.write().await;
      workspace.schemas.cache().set_cache_path(init_config.cache_path.clone());
      match workspace.initialize(&context.env, &context.default_config.load()).await {
        Ok(mut current) => notifications.append(&mut current),
        Err(error) => tracing::error!(?error, "failed to initialize workspace"),
      }
    }
    let notification_context = context.clone();
    context
      .defer(async move {
        send_association_notifications(notification_context, notifications).await;
      })
      .await;
  }

  Ok(InitializeResult {
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
          token_modifiers: semantic_tokens::TokenModifier::MODIFIERS.into(),
        },
        full: Some(SemanticTokensFullOptions::Bool(true)),
        range: Some(false),
      })),
      rename_provider: Some(OneOf::Right(RenameOptions {
        prepare_provider:           Some(true),
        work_done_progress_options: Default::default(),
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
        work_done_progress_options: Default::default(),
      }),
      ..Default::default()
    },
    server_info:     Some(ServerInfo {
      name:    "Taplo".into(),
      version: Some(env!("CARGO_PKG_VERSION").into()),
    }),
    offset_encoding: None,
  })
}

#[tracing::instrument(skip_all)]
pub async fn initialized<E: Environment>(context: Context<World<E>>, _params: Params<InitializedParams>) {
  context.env.spawn_local(update_configuration(context.clone()));
}
