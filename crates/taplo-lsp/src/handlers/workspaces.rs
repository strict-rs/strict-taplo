//! Workspace-folder topology updates with open-document redistribution.

use lsp_async_stub::Context;
use lsp_async_stub::Params;
use lsp_types::DidChangeWorkspaceFoldersParams;
use taplo_common::environment::Environment;

use super::update_configuration;
use crate::diagnostics;
use crate::world::World;
use crate::world::send_association_notifications;

pub async fn workspace_change<E: Environment>(context: Context<World<E>>, params: Params<DidChangeWorkspaceFoldersParams>) {
  let Some(params) = params.optional() else {
    return;
  };
  let mut moved_documents = Vec::new();

  for removed in params.event.removed {
    let Some(root) = crate::uri::to_url(&removed.uri) else {
      continue;
    };
    moved_documents.extend(context.remove_workspace_root(&root).await);
  }

  let init_config = context.init_config.load_full();
  let default_config = context.default_config.load_full();
  let mut notifications = Vec::new();
  for added in params.event.added {
    let Some(root) = crate::uri::to_url(&added.uri) else {
      continue;
    };
    let (workspace, moved) = context.add_workspace_root(root).await;
    moved_documents.extend(moved);
    let mut workspace = workspace.write().await;
    workspace.schemas.cache().set_cache_path(init_config.cache_path.clone());
    match workspace.initialize(&context.env, &default_config).await {
      Ok(mut current) => notifications.append(&mut current),
      Err(error) => tracing::error!(%error, "failed to initialize workspace"),
    }
  }

  for handle in context.all_workspace_handles().await {
    notifications.extend(handle.read().await.association_notifications());
  }
  send_association_notifications(context.clone(), notifications).await;
  moved_documents.sort_by(|left, right| left.as_str().cmp(right.as_str()));
  moved_documents.dedup();
  for document in moved_documents {
    diagnostics::publish_diagnostics(context.clone(), document).await;
  }

  update_configuration(context).await;
}
