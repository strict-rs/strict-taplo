//! Global and root-scoped LSP configuration updates associated by captured root keys.

use crate::world::{send_association_notifications, WorkspaceHandle, World};
use lsp_async_stub::{Context, Params, RequestWriter};
use lsp_types::{
    request::WorkspaceConfiguration, ConfigurationItem, ConfigurationParams,
    DidChangeConfigurationParams,
};
use serde_json::Value;
use std::sync::Arc;
use taplo_common::environment::Environment;

#[tracing::instrument(skip_all)]
pub async fn configuration_change<E: Environment>(
    context: Context<World<E>>,
    params: Params<DidChangeConfigurationParams>,
) {
    let Some(params) = params.optional() else {
        return;
    };
    let handles = context.all_workspace_handles().await;
    let mut affected = Vec::new();
    for handle in handles {
        let mut workspace = handle.write().await;
        match workspace.config.update_from_json(&params.settings) {
            Ok(()) => push_unique_handle(&mut affected, handle.clone()),
            Err(error) => tracing::error!(%error, "invalid configuration"),
        }
    }
    reinitialize(context, affected).await;
}

#[tracing::instrument(skip_all)]
pub async fn update_configuration<E: Environment>(mut context: Context<World<E>>) {
    let init_config = context.init_config.load();
    let mut captured_roots = Vec::new();
    for root in context.rooted_workspace_urls().await {
        let Some(scope_uri) = crate::uri::to_uri(&root) else {
            tracing::warn!(%root, "workspace root is not representable as an LSP URI");
            continue;
        };
        captured_roots.push((
            root,
            ConfigurationItem {
                scope_uri: Some(scope_uri),
                section: Some(init_config.configuration_section.clone()),
            },
        ));
    }

    let mut items = Vec::with_capacity(captured_roots.len().saturating_add(1));
    items.push(ConfigurationItem {
        scope_uri: None,
        section: Some(init_config.configuration_section.clone()),
    });
    items.extend(captured_roots.iter().map(|(_, item)| item.clone()));

    let response = context
        .write_request::<WorkspaceConfiguration, _>(Some(ConfigurationParams { items }))
        .await;
    let response = match response {
        Ok(response) => match response.into_result() {
            Ok(values) => values,
            Err(error) => {
                tracing::error!(%error, "invalid configuration response");
                return;
            }
        },
        Err(error) => {
            tracing::error!(%error, "failed to fetch configuration");
            return;
        }
    };

    let affected = apply_configuration_response(&context, &captured_roots, &response).await;
    reinitialize(context, affected).await;
}

/// Apply configuration values by global position and captured root identity.
async fn apply_configuration_response<E: Environment>(
    world: &World<E>,
    captured_roots: &[(url::Url, ConfigurationItem)],
    response: &[Value],
) -> Vec<WorkspaceHandle<E>> {
    let mut affected = Vec::new();
    match response.first() {
        Some(global) if global.is_object() => {
            for handle in world.all_workspace_handles().await {
                let mut workspace = handle.write().await;
                match workspace.config.update_from_json(global) {
                    Ok(()) => push_unique_handle(&mut affected, handle.clone()),
                    Err(error) => tracing::error!(%error, "invalid global configuration"),
                }
            }
        }
        Some(_) => tracing::warn!("ignoring non-object global configuration"),
        None => tracing::warn!("configuration response omitted the global value"),
    }

    for (index, (root, _)) in captured_roots.iter().enumerate() {
        let response_index = index.saturating_add(1);
        let Some(value) = response.get(response_index) else {
            tracing::warn!(%root, "configuration response omitted a scoped value");
            continue;
        };
        if !value.is_object() {
            tracing::warn!(%root, "ignoring non-object scoped configuration");
            continue;
        }
        let Some(handle) = world.rooted_workspace(root).await else {
            tracing::warn!(%root, "workspace was removed while configuration was pending");
            continue;
        };
        let mut workspace = handle.write().await;
        match workspace.config.update_from_json(value) {
            Ok(()) => push_unique_handle(&mut affected, handle.clone()),
            Err(error) => tracing::error!(%error, %root, "invalid scoped configuration"),
        }
    }

    let expected = captured_roots.len().saturating_add(1);
    if response.len() > expected {
        tracing::warn!(
            surplus = response.len().saturating_sub(expected),
            "ignoring surplus configuration response values"
        );
    }
    affected
}

/// Add a handle once by allocation identity.
fn push_unique_handle<E: Environment>(
    handles: &mut Vec<WorkspaceHandle<E>>,
    candidate: WorkspaceHandle<E>,
) {
    if !handles
        .iter()
        .any(|existing| Arc::ptr_eq(existing, &candidate))
    {
        handles.push(candidate);
    }
}

/// Reinitialize affected workspaces and send all resulting notifications after unlocking them.
async fn reinitialize<E: Environment>(
    context: Context<World<E>>,
    handles: Vec<WorkspaceHandle<E>>,
) {
    let default_config = context.default_config.load_full();
    let mut notifications = Vec::new();
    for handle in handles {
        let mut workspace = handle.write().await;
        match workspace.initialize(&context.env, &default_config).await {
            Ok(mut current) => notifications.append(&mut current),
            Err(error) => tracing::error!(%error, "failed to update workspace"),
        }
    }
    send_association_notifications(context, notifications).await;
}

#[cfg(test)]
mod tests {
    use super::apply_configuration_response;
    use crate::{create_world, test_support::TestEnvironment};
    use lsp_types::ConfigurationItem;
    use strict_test_support::{ensure, ensure_eq, ensure_ok, TestFailure};
    use url::Url;

    /// Parse one workspace URL fixture.
    fn url(value: &str) -> Result<Url, TestFailure> {
        ensure_ok(Url::parse(value), "the configuration fixture URL must parse")
    }

    /// Pair one captured root with the wire item that would have been sent for it.
    fn captured(root: Url) -> (Url, ConfigurationItem) {
        (
            root,
            ConfigurationItem {
                scope_uri: None,
                section: Some("evenBetterToml".into()),
            },
        )
    }

    #[test]
    fn global_and_scoped_values_apply_by_current_and_captured_identity(
    ) -> Result<(), TestFailure> {
        let world = create_world(TestEnvironment::default());
        let first_root = url("file:///workspace/first/")?;
        let second_root = url("file:///workspace/second/")?;

        futures::executor::block_on(async {
            let (first, _) = world.add_workspace_root(first_root.clone()).await;
            let captured_roots = vec![captured(first_root.clone())];
            let (second, _) = world.add_workspace_root(second_root).await;
            let response = vec![
                serde_json::json!({ "schema": { "enabled": false } }),
                serde_json::json!({ "completion": { "maxKeys": 9 } }),
            ];
            let affected = apply_configuration_response(&world, &captured_roots, &response).await;
            ensure_eq(
                &affected.len(),
                &3,
                "global configuration must affect detached and both current roots exactly once",
            )?;

            let detached = world
                .workspace_for_document(&url("file:///outside/file.toml")?)
                .await;
            ensure(
                !detached.read().await.config.schema.enabled,
                "the global value must apply to detached state",
            )?;
            ensure(
                !first.read().await.config.schema.enabled,
                "the global value must apply to a captured root",
            )?;
            ensure_eq(
                &first.read().await.config.completion.max_keys,
                &9,
                "the scoped value must apply to its captured root",
            )?;
            ensure(
                !second.read().await.config.schema.enabled,
                "a root added after capture must still receive the global object",
            )?;
            ensure_eq(
                &second.read().await.config.completion.max_keys,
                &5,
                "a root added after capture must not receive another root's scoped object",
            )
        })
    }

    #[test]
    fn malformed_missing_surplus_and_stale_values_leave_unrelated_state_unchanged(
    ) -> Result<(), TestFailure> {
        let world = create_world(TestEnvironment::default());
        let live_root = url("file:///workspace/live/")?;
        let stale_root = url("file:///workspace/stale/")?;

        futures::executor::block_on(async {
            let (live, _) = world.add_workspace_root(live_root.clone()).await;
            let _ = world.add_workspace_root(stale_root.clone()).await;
            let captured_roots = vec![captured(live_root), captured(stale_root.clone())];
            let _ = world.remove_workspace_root(&stale_root).await;

            let malformed = vec![
                serde_json::json!("not an object"),
                serde_json::json!("also not an object"),
                serde_json::json!({ "completion": { "maxKeys": 99 } }),
                serde_json::json!({ "completion": { "maxKeys": 100 } }),
            ];
            let affected = apply_configuration_response(&world, &captured_roots, &malformed).await;
            ensure(
                affected.is_empty(),
                "non-object and stale scoped values must affect no live workspace",
            )?;
            ensure_eq(
                &live.read().await.config.completion.max_keys,
                &5,
                "malformed and stale values must leave unrelated live configuration unchanged",
            )?;

            let missing = vec![serde_json::json!({ "syntax": { "semanticTokens": false } })];
            let affected = apply_configuration_response(
                &world,
                &[captured(url("file:///workspace/live/")?)],
                &missing,
            )
            .await;
            ensure_eq(
                &affected.len(),
                &2,
                "a valid global-only response must affect detached and the current root",
            )?;
            ensure_eq(
                &live.read().await.config.completion.max_keys,
                &5,
                "a missing scoped value must not alter the root's scoped completion setting",
            )
        })
    }
}
