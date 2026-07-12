//! Manual schema association queries and updates.

use crate::{
    diagnostics::publish_diagnostics,
    lsp_ext::{
        notification::{self, AssociateSchemaParams},
        request::{
            AssociatedSchemaParams, AssociatedSchemaResponse, ListSchemasParams,
            ListSchemasResponse, SchemaInfo,
        },
    },
    world::{send_association_notifications, World},
};
use lsp_async_stub::{rpc::Error, Context, Params};
use serde_json::json;
use taplo_common::{
    environment::Environment,
    schema::associations::{priority, source, AssociationRule, SchemaAssociation},
};

#[tracing::instrument(skip_all)]
pub async fn list_schemas<E: Environment>(
    context: Context<World<E>>,
    params: Params<ListSchemasParams>,
) -> Result<ListSchemasResponse, Error> {
    let params = params.required()?;
    let workspace = context.workspace_for_document(&params.document_uri).await;
    let workspace = workspace.read().await;
    let associations = workspace.schemas.associations().read();
    Ok(ListSchemasResponse {
        schemas: associations
            .iter()
            .filter(|(rule, _)| !matches!(rule, AssociationRule::Url(..)))
            .map(|(_, association)| SchemaInfo {
                url: association.url.clone(),
                meta: association.meta.clone(),
            })
            .collect(),
    })
}

#[tracing::instrument(skip_all)]
pub async fn associate_schema<E: Environment>(
    context: Context<World<E>>,
    params: Params<AssociateSchemaParams>,
) {
    let Ok(params) = params.required() else {
        return;
    };
    let association = SchemaAssociation {
        priority: params.priority.unwrap_or(priority::MAX),
        url: params.schema_uri,
        meta: {
            let mut meta = params.meta.unwrap_or_else(|| json!({}));
            if !meta.is_object() {
                meta = json!({});
            }
            meta["source"] = source::MANUAL.into();
            meta
        },
    };

    let mut notifications = Vec::new();
    let mut diagnostic_document = None;
    match params.rule {
        notification::AssociationRule::Glob(glob) => {
            let rule = match AssociationRule::glob(&glob) {
                Ok(rule) => rule,
                Err(error) => {
                    tracing::error!(%error, schema_uri = %association.url, "invalid schema glob");
                    return;
                }
            };
            for handle in context.all_workspace_handles().await {
                let workspace = handle.write().await;
                workspace
                    .schemas
                    .associations()
                    .add(rule.clone(), association.clone());
                notifications.extend(workspace.association_notifications());
            }
        }
        notification::AssociationRule::Regex(regex) => {
            let rule = match AssociationRule::regex(&regex) {
                Ok(rule) => rule,
                Err(error) => {
                    tracing::error!(%error, schema_uri = %association.url, "invalid schema regex");
                    return;
                }
            };
            for handle in context.all_workspace_handles().await {
                let workspace = handle.write().await;
                workspace
                    .schemas
                    .associations()
                    .add(rule.clone(), association.clone());
                notifications.extend(workspace.association_notifications());
            }
        }
        notification::AssociationRule::Url(document_uri) => {
            let handle = context.workspace_for_document(&document_uri).await;
            let workspace = handle.write().await;
            workspace
                .schemas
                .associations()
                .retain(|(rule, existing)| match rule {
                    AssociationRule::Url(url) => {
                        url != &document_uri || existing.meta["source"] != source::MANUAL
                    }
                    _ => true,
                });
            workspace
                .schemas
                .associations()
                .add(AssociationRule::Url(document_uri.clone()), association);
            notifications.extend(workspace.association_notifications());
            diagnostic_document = Some(document_uri);
        }
    }

    if let Some(document) = diagnostic_document {
        publish_diagnostics(context.clone(), document).await;
    }
    send_association_notifications(context, notifications).await;
}

#[tracing::instrument(skip_all)]
pub async fn associated_schema<E: Environment>(
    context: Context<World<E>>,
    params: Params<AssociatedSchemaParams>,
) -> Result<AssociatedSchemaResponse, Error> {
    let params = params.required()?;
    let workspace = context.workspace_for_document(&params.document_uri).await;
    let workspace = workspace.read().await;
    Ok(AssociatedSchemaResponse {
        schema: workspace
            .schemas
            .associations()
            .association_for(&params.document_uri)
            .map(|association| SchemaInfo {
                url: association.url,
                meta: association.meta,
            }),
    })
}
