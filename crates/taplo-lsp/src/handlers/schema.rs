//! Manual schema association queries and state transitions.

use serde_json::Map;
use serde_json::Value;
use taplo_common::schema::associations::SchemaAssociation;
use taplo_common::schema::associations::priority;
use taplo_common::schema::associations::source;
use taplo_lsp_async::Params;
use taplo_lsp_async::rpc::RpcError;

use crate::lsp_ext::notification;
use crate::lsp_ext::notification::AssociateSchemaParams;
use crate::lsp_ext::request::AssociatedSchemaParams;
use crate::lsp_ext::request::AssociatedSchemaResponse;
use crate::lsp_ext::request::ListSchemasParams;
use crate::lsp_ext::request::ListSchemasResponse;
use crate::lsp_ext::request::SchemaInfo;
use crate::world::ManualAssociationRule;
use crate::world::ManualAssociationUpdate;
use crate::world::WorldState;

/// Generate manual-schema operations over one concrete execution family.
macro_rules! define_schema_handler_future_family {
  (
    $list_schemas:ident,
    $associate_schema:ident,
    $associated_schema:ident,
    $list_schema_associations:ident,
    $world_associate_schema:ident,
    $world_associated_schema:ident;
    $future:ident,
    $environment:path,
    $transport:ident,
    $schema_execution:path;
    [$($value_bound:path),*]
  ) => {
    /// List every non-document association visible to a document's workspace.
    pub(super) fn $list_schemas<E: $environment>(
      world: &WorldState<E, $transport<E>>,
      params: Params<ListSchemasParams>,
    ) -> $future<'_, Result<ListSchemasResponse, RpcError>> {
      Box::pin(async move {
        let parameters = params.required()?;
        Ok(ListSchemasResponse {
          schemas: world
            .$list_schema_associations(&parameters.document_uri)
            .await
            .into_iter()
            .map(|association| SchemaInfo {
              url:  association.url,
              meta: association.meta,
            })
            .collect(),
        })
      })
    }

    /// Validate and commit one manual schema association.
    pub(super) fn $associate_schema<E: $environment>(
      world: &WorldState<E, $transport<E>>,
      params: Params<AssociateSchemaParams>,
    ) -> $future<'_, Result<ManualAssociationUpdate, RpcError>> {
      Box::pin(async move {
        let parameters = params.required()?;
        let association = SchemaAssociation {
          priority: parameters.priority.unwrap_or(priority::MAX),
          url:      parameters.schema_uri,
          meta:     {
            let mut metadata = match parameters.meta {
              Some(Value::Object(metadata)) => metadata,
              Some(_) | None => Map::new(),
            };
            drop(metadata.insert(String::from("source"), Value::String(String::from(source::MANUAL))));
            Value::Object(metadata)
          },
        };
        let rule = match parameters.rule {
          notification::AssociationRule::Glob(pattern) => ManualAssociationRule::Glob(pattern),
          notification::AssociationRule::Regex(pattern) => ManualAssociationRule::Regex(pattern),
          notification::AssociationRule::Url(document) => ManualAssociationRule::Url(document),
        };
        world
          .$world_associate_schema(rule, association)
          .await
          .map_err(|error| RpcError::internal_error().with_details(error.to_string()))
      })
    }

    /// Return the highest-priority schema association selected for one document.
    pub(super) fn $associated_schema<E: $environment>(
      world: &WorldState<E, $transport<E>>,
      params: Params<AssociatedSchemaParams>,
    ) -> $future<'_, Result<AssociatedSchemaResponse, RpcError>> {
      Box::pin(async move {
        let parameters = params.required()?;
        Ok(AssociatedSchemaResponse {
          schema: world
            .$world_associated_schema(&parameters.document_uri)
            .await
            .map(|association| SchemaInfo {
              url:  association.url,
              meta: association.meta,
            }),
        })
      })
    }
  };
}

define_lsp_execution_families!(
  handler
  define_schema_handler_future_family;
  (list_schemas_local, list_schemas_concurrent),
  (associate_schema_local, associate_schema_concurrent),
  (associated_schema_local, associated_schema_concurrent),
  (
    list_schema_associations,
    list_schema_associations_concurrent
  ),
  (associate_schema, associate_schema_concurrent),
  (associated_schema, associated_schema_concurrent),
);
