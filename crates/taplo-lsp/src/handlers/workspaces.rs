use super::update_configuration;
use crate::world::{WorkspaceState, World};
use lsp_async_stub::{Context, Params};
use lsp_types::DidChangeWorkspaceFoldersParams;
use taplo_common::environment::Environment;

pub async fn workspace_change<E: Environment>(
    context: Context<World<E>>,
    params: Params<DidChangeWorkspaceFoldersParams>,
) {
    let p = match params.optional() {
        None => return,
        Some(p) => p,
    };

    let mut workspaces = context.workspaces.write().await;
    let init_config = context.init_config.load();

    for removed in p.event.removed {
        if let Some(url) = crate::uri::to_url(&removed.uri) {
            workspaces.shift_remove(&url);
        }
    }

    for added in p.event.added {
        let Some(added_url) = crate::uri::to_url(&added.uri) else {
            continue;
        };
        let ws = workspaces
            .entry(added_url.clone())
            .or_insert(WorkspaceState::new(context.env.clone(), added_url));

        ws.schemas
            .cache()
            .set_cache_path(init_config.cache_path.clone());

        if let Err(error) = ws.initialize(context.clone(), &context.env).await {
            tracing::error!(?error, "failed to initialize workspace");
        }
    }

    drop(workspaces);
    update_configuration(context).await;
}
