//! Protocol adapters shared by the local and concurrent server families.

#[cfg(not(target_arch = "wasm32"))]
use taplo_common::environment::ConcurrentEnvironment;
use taplo_common::environment::LocalEnvironment;
#[cfg(not(target_arch = "wasm32"))]
use taplo_lsp_async::ConcurrentServer;
use taplo_lsp_async::LocalServer;

#[cfg(not(target_arch = "wasm32"))]
use crate::world::ConcurrentWorld;
use crate::world::LocalWorld;

/// Define the protocol adapter for one execution model.
macro_rules! define_runtime_family_impl {
  (
    family $module:ident {
      capability = $environment:ident;
      protocol = ($context:ident, $server:ident);
      state = ($world:ident, $future:ident);
    }
  ) => {
    mod $module {
      //! Execution-model-specific registration around shared protocol transitions.

      use taplo_lsp_async::$context;
      use taplo_lsp_async::$server;
      use taplo_lsp_async::Params;
      use taplo_lsp_async::ServerError;
      use taplo_lsp_async::rpc::RpcError;
      use lsp_types::DidChangeConfigurationParams;
      use lsp_types::DidChangeTextDocumentParams;
      use lsp_types::DidChangeWorkspaceFoldersParams;
      use lsp_types::DidCloseTextDocumentParams;
      use lsp_types::DidOpenTextDocumentParams;
      use lsp_types::DidSaveTextDocumentParams;
      use lsp_types::InitializedParams;
      use lsp_types::notification;
      use lsp_types::notification::Notification;
      use lsp_types::request;
      use taplo_common::environment::$environment;

      use crate::$future;
      use crate::handlers::diagnostics::DiagnosticBatch;
      use crate::handlers;
      use crate::handlers::$module as family_handlers;
      use crate::handlers::ConfigurationEffects;
      use crate::handlers::DocumentEffects;
      use crate::handlers::WorkspaceChangeEffects;
      use crate::lsp_ext;
      use crate::world::$world;

      /// Build the complete request and notification registry.
      pub(super) fn create_server<E: $environment>() -> $server<$world<E>> {
        $server::new()
          .on_request::<request::Initialize, _>(initialize::<E>)
          .on_request::<request::FoldingRangeRequest, _>(folding_ranges::<E>)
          .on_request::<lsp_ext::request::ModernDocumentSymbolRequest, _>(document_symbols::<E>)
          .on_request::<request::Formatting, _>(format::<E>)
          .on_request::<request::Completion, _>(completion::<E>)
          .on_request::<request::HoverRequest, _>(hover::<E>)
          .on_request::<request::DocumentLinkRequest, _>(links::<E>)
          .on_request::<request::SemanticTokensFullRequest, _>(semantic_tokens::<E>)
          .on_request::<request::PrepareRenameRequest, _>(prepare_rename::<E>)
          .on_request::<request::Rename, _>(rename::<E>)
          .on_mutation_notification::<notification::Initialized, _>(initialized::<E>)
          .on_mutation_notification::<notification::DidOpenTextDocument, _>(document_open::<E>)
          .on_mutation_notification::<notification::DidChangeTextDocument, _>(document_change::<E>)
          .on_mutation_notification::<notification::DidSaveTextDocument, _>(document_save::<E>)
          .on_mutation_notification::<notification::DidCloseTextDocument, _>(document_close::<E>)
          .on_mutation_notification::<notification::DidChangeConfiguration, _>(
            configuration_change::<E>,
          )
          .on_mutation_notification::<notification::DidChangeWorkspaceFolders, _>(
            workspace_change::<E>,
          )
          .on_request::<lsp_ext::request::ConvertToJsonRequest, _>(convert_to_json::<E>)
          .on_request::<lsp_ext::request::ConvertToTomlRequest, _>(convert_to_toml::<E>)
          .on_request::<lsp_ext::request::ListSchemasRequest, _>(list_schemas::<E>)
          .on_request::<lsp_ext::request::AssociatedSchemaRequest, _>(associated_schema::<E>)
          .on_mutation_notification::<lsp_ext::notification::AssociateSchema, _>(
            associate_schema::<E>,
          )
      }

      /// Map an internal handler failure into a JSON-RPC internal error.
      fn rpc_error(error: impl std::fmt::Display) -> RpcError {
        RpcError::internal_error().with_details(error.to_string())
      }

      /// Report a notification failure after its typed transition reaches the protocol edge.
      fn report_notification_error(method: &'static str, error: impl std::fmt::Display) {
        tracing::error!(%error, %method, "notification handling failed");
      }

      /// Send all association notifications in deterministic order.
      fn send_associations<E: $environment>(
        context: &mut $context<$world<E>>,
        associations: Vec<lsp_ext::notification::DidChangeSchemaAssociationParams>,
      ) -> $future<'_, Result<(), ServerError>> {
        Box::pin(async move {
          for association in associations {
            context
              .write_notification::<lsp_ext::notification::DidChangeSchemaAssociation>(Some(
                association,
              ))
              .await?;
          }
          Ok(())
        })
      }

      /// Send one diagnostics replacement.
      fn send_diagnostics<E: $environment>(
        context: &mut $context<$world<E>>,
        diagnostics: DiagnosticBatch,
      ) -> $future<'_, Result<(), ServerError>> {
        Box::pin(async move {
          context
            .write_notification::<notification::PublishDiagnostics>(Some(
              handlers::publish_params(diagnostics),
            ))
            .await
        })
      }

      /// Send every output produced by a document mutation.
      fn send_document_effects<E: $environment>(
        context: &mut $context<$world<E>>,
        effects: DocumentEffects,
      ) -> $future<'_, Result<(), ServerError>> {
        Box::pin(async move {
          send_associations(context, effects.associations).await?;
          send_diagnostics(context, effects.diagnostics).await
        })
      }

      /// Send the association and diagnostics vectors shared by broad world transitions.
      fn send_world_effects<E: $environment>(
        context: &mut $context<$world<E>>,
        associations: Vec<lsp_ext::notification::DidChangeSchemaAssociationParams>,
        diagnostics: Vec<DiagnosticBatch>,
      ) -> $future<'_, Result<(), ServerError>> {
        Box::pin(async move {
          send_associations(context, associations).await?;
          for batch in diagnostics {
            send_diagnostics(context, batch).await?;
          }
          Ok(())
        })
      }

      /// Send every output produced by a workspace topology mutation.
      fn send_workspace_effects<E: $environment>(
        context: &mut $context<$world<E>>,
        effects: WorkspaceChangeEffects,
      ) -> $future<'_, Result<(), ServerError>> {
        send_world_effects(context, effects.associations, effects.diagnostics)
      }

      /// Send every output produced by a configuration mutation.
      fn send_configuration_effects<E: $environment>(
        context: &mut $context<$world<E>>,
        effects: ConfigurationEffects,
      ) -> $future<'_, Result<(), ServerError>> {
        send_world_effects(context, effects.associations, effects.diagnostics)
      }

      /// Pull configuration from the client and atomically apply the captured response.
      fn refresh_configuration<'operation, E: $environment>(
        context: &'operation mut $context<$world<E>>,
        method: &'static str,
      ) -> $future<'operation, Result<(), ServerError>> {
        Box::pin(async move {
          let request = match family_handlers::configuration_request(context.world().as_ref()).await {
            Ok(request) => request,
            Err(error) => {
              report_notification_error(method, error);
              return Ok(());
            }
          };
          let response = context
            .write_request::<request::WorkspaceConfiguration>(Some(request.params.clone()))
            .await?;
          let response = match response.into_result() {
            Ok(response) => response,
            Err(error) => {
              report_notification_error(method, error);
              return Ok(());
            }
          };
          let effects = match family_handlers::apply_configuration_response(
            context.world().as_ref(),
            &request,
            response,
          )
          .await
          {
            Ok(effects) => effects,
            Err(error) => {
              report_notification_error(method, error);
              return Ok(());
            }
          };
          send_configuration_effects(context, effects).await
        })
      }

      /// Handle initialization and defer notifications until after its response.
      fn initialize<E: $environment>(
        context: $context<$world<E>>,
        params: Params<lsp_types::InitializeParams>,
      ) -> $future<'static, Result<lsp_types::InitializeResult, RpcError>> {
        Box::pin(async move {
          let params = params.required()?;
          let effects = family_handlers::initialize(context.world().as_ref(), params)
            .await
            .map_err(rpc_error)?;
          let mut output = context.clone();
          context
            .defer(async move {
              send_associations(&mut output, effects.associations).await
            })
            .await;
          Ok(effects.result)
        })
      }

      /// Start the client-configuration exchange after initialization.
      fn initialized<E: $environment>(
        context: $context<$world<E>>,
        _params: Params<InitializedParams>,
      ) -> $future<'static, Result<(), ServerError>> {
        Box::pin(async move {
          let mut context = context;
          refresh_configuration(&mut context, notification::Initialized::METHOD).await
        })
      }

      /// Handle an opened document.
      fn document_open<E: $environment>(
        mut context: $context<$world<E>>,
        params: Params<DidOpenTextDocumentParams>,
      ) -> $future<'static, Result<(), ServerError>> {
        Box::pin(async move {
          match family_handlers::document_open(context.world().as_ref(), params).await {
            Ok(effects) => send_document_effects(&mut context, effects).await,
            Err(error) => {
              report_notification_error(notification::DidOpenTextDocument::METHOD, error);
              Ok(())
            }
          }
        })
      }

      /// Handle a full-sync document change.
      fn document_change<E: $environment>(
        mut context: $context<$world<E>>,
        params: Params<DidChangeTextDocumentParams>,
      ) -> $future<'static, Result<(), ServerError>> {
        Box::pin(async move {
          match family_handlers::document_change(context.world().as_ref(), params).await {
            Ok(effects) => send_document_effects(&mut context, effects).await,
            Err(error) => {
              report_notification_error(notification::DidChangeTextDocument::METHOD, error);
              Ok(())
            }
          }
        })
      }

      /// Preserve the full-sync save notification as an ordered no-op.
      fn document_save<E: $environment>(
        _context: $context<$world<E>>,
        _params: Params<DidSaveTextDocumentParams>,
      ) -> $future<'static, Result<(), ServerError>> {
        Box::pin(async { Ok(()) })
      }

      /// Handle a closed document.
      fn document_close<E: $environment>(
        mut context: $context<$world<E>>,
        params: Params<DidCloseTextDocumentParams>,
      ) -> $future<'static, Result<(), ServerError>> {
        Box::pin(async move {
          match family_handlers::document_close(context.world().as_ref(), params).await {
            Ok(effects) => send_document_effects(&mut context, effects).await,
            Err(error) => {
              report_notification_error(notification::DidCloseTextDocument::METHOD, error);
              Ok(())
            }
          }
        })
      }

      /// Apply pushed client configuration.
      fn configuration_change<E: $environment>(
        mut context: $context<$world<E>>,
        params: Params<DidChangeConfigurationParams>,
      ) -> $future<'static, Result<(), ServerError>> {
        Box::pin(async move {
          let result = match params.optional() {
            Some(params) => family_handlers::configuration_change(context.world().as_ref(), params)
              .await
              .map_err(|error| error.to_string()),
            None => Err("configuration notification parameters are required".into()),
          };
          match result {
            Ok(effects) => send_configuration_effects(&mut context, effects).await,
            Err(error) => {
              report_notification_error(notification::DidChangeConfiguration::METHOD, error);
              Ok(())
            }
          }
        })
      }

      /// Apply workspace topology changes and refresh scoped client configuration.
      fn workspace_change<E: $environment>(
        mut context: $context<$world<E>>,
        params: Params<DidChangeWorkspaceFoldersParams>,
      ) -> $future<'static, Result<(), ServerError>> {
        Box::pin(async move {
          let Some(params) = params.optional() else {
            report_notification_error(
              notification::DidChangeWorkspaceFolders::METHOD,
              "workspace notification parameters are required",
            );
            return Ok(());
          };
          match family_handlers::workspace_change(context.world().as_ref(), params).await {
            Ok(effects) => {
              send_workspace_effects(&mut context, effects).await?;
              refresh_configuration(
                &mut context,
                notification::DidChangeWorkspaceFolders::METHOD,
              )
              .await
            }
            Err(error) => {
              report_notification_error(notification::DidChangeWorkspaceFolders::METHOD, error);
              Ok(())
            }
          }
        })
      }

      /// Convert TOML to JSON without consulting world state.
      fn convert_to_json<E: $environment>(
        _context: $context<$world<E>>,
        params: Params<lsp_ext::request::ConvertToJsonParams>,
      ) -> $future<'static, Result<lsp_ext::request::ConvertToJsonResponse, RpcError>> {
        Box::pin(async move { handlers::convert_to_json(params).await })
      }

      /// Convert JSON to TOML without consulting world state.
      fn convert_to_toml<E: $environment>(
        _context: $context<$world<E>>,
        params: Params<lsp_ext::request::ConvertToTomlParams>,
      ) -> $future<'static, Result<lsp_ext::request::ConvertToTomlResponse, RpcError>> {
        Box::pin(async move { handlers::convert_to_toml(params).await })
      }

      /// List schema associations visible to one document.
      fn list_schemas<E: $environment>(
        context: $context<$world<E>>,
        params: Params<lsp_ext::request::ListSchemasParams>,
      ) -> $future<'static, Result<lsp_ext::request::ListSchemasResponse, RpcError>> {
        Box::pin(async move {
          family_handlers::list_schemas(context.world().as_ref(), params).await
        })
      }

      /// Return the selected schema association for one document.
      fn associated_schema<E: $environment>(
        context: $context<$world<E>>,
        params: Params<lsp_ext::request::AssociatedSchemaParams>,
      ) -> $future<'static, Result<lsp_ext::request::AssociatedSchemaResponse, RpcError>> {
        Box::pin(async move {
          family_handlers::associated_schema(context.world().as_ref(), params).await
        })
      }

      /// Commit one manual schema association and publish its effects.
      fn associate_schema<E: $environment>(
        mut context: $context<$world<E>>,
        params: Params<lsp_ext::notification::AssociateSchemaParams>,
      ) -> $future<'static, Result<(), ServerError>> {
        Box::pin(async move {
          match family_handlers::associate_schema(context.world().as_ref(), params).await {
            Ok(update) => {
              send_associations(&mut context, update.notifications).await?;
              if let Some(document) = update.diagnostic_document {
                match family_handlers::collect_diagnostics(context.world().as_ref(), &document).await {
                  Ok(Some(diagnostics)) => {
                    send_diagnostics(&mut context, diagnostics).await?;
                  }
                  Ok(None) => {}
                  Err(error) => {
                    report_notification_error(lsp_ext::notification::AssociateSchema::METHOD, error);
                  }
                }
              }
              Ok(())
            }
            Err(error) => {
              report_notification_error(lsp_ext::notification::AssociateSchema::METHOD, error);
              Ok(())
            }
          }
        })
      }

      /// Handle folding ranges.
      fn folding_ranges<E: $environment>(
        context: $context<$world<E>>,
        params: Params<lsp_types::FoldingRangeParams>,
      ) -> $future<'static, Result<Option<Vec<lsp_types::FoldingRange>>, RpcError>> {
        Box::pin(async move {
          family_handlers::folding_ranges(context.world().as_ref(), params).await
        })
      }

      /// Handle document symbols.
      fn document_symbols<E: $environment>(
        context: $context<$world<E>>,
        params: Params<lsp_types::DocumentSymbolParams>,
      ) -> $future<'static, Result<Option<Vec<lsp_ext::request::ModernDocumentSymbol>>, RpcError>> {
        Box::pin(async move {
          family_handlers::document_symbols(context.world().as_ref(), params).await
        })
      }

      /// Handle document formatting.
      fn format<E: $environment>(
        context: $context<$world<E>>,
        params: Params<lsp_types::DocumentFormattingParams>,
      ) -> $future<'static, Result<Option<Vec<lsp_types::TextEdit>>, RpcError>> {
        Box::pin(async move {
          family_handlers::format(context.world().as_ref(), params).await
        })
      }

      /// Handle schema completion.
      fn completion<E: $environment>(
        context: $context<$world<E>>,
        params: Params<lsp_types::CompletionParams>,
      ) -> $future<'static, Result<Option<lsp_types::CompletionResponse>, RpcError>> {
        Box::pin(async move {
          family_handlers::completion(context.world().as_ref(), params).await
        })
      }

      /// Handle schema hover.
      fn hover<E: $environment>(
        context: $context<$world<E>>,
        params: Params<lsp_types::HoverParams>,
      ) -> $future<'static, Result<Option<lsp_types::Hover>, RpcError>> {
        Box::pin(async move {
          family_handlers::hover(context.world().as_ref(), params).await
        })
      }

      /// Handle schema documentation links.
      fn links<E: $environment>(
        context: $context<$world<E>>,
        params: Params<lsp_types::DocumentLinkParams>,
      ) -> $future<'static, Result<Option<Vec<lsp_types::DocumentLink>>, RpcError>> {
        Box::pin(async move {
          family_handlers::links(context.world().as_ref(), params).await
        })
      }

      /// Handle semantic tokens.
      fn semantic_tokens<E: $environment>(
        context: $context<$world<E>>,
        params: Params<lsp_types::SemanticTokensParams>,
      ) -> $future<'static, Result<Option<lsp_types::SemanticTokensResult>, RpcError>> {
        Box::pin(async move {
          family_handlers::semantic_tokens(context.world().as_ref(), params).await
        })
      }

      /// Handle prepare-rename.
      fn prepare_rename<E: $environment>(
        context: $context<$world<E>>,
        params: Params<lsp_types::TextDocumentPositionParams>,
      ) -> $future<'static, Result<Option<lsp_types::PrepareRenameResponse>, RpcError>> {
        Box::pin(async move {
          family_handlers::prepare_rename(context.world().as_ref(), params).await
        })
      }

      /// Handle rename.
      fn rename<E: $environment>(
        context: $context<$world<E>>,
        params: Params<lsp_types::RenameParams>,
      ) -> $future<'static, Result<Option<lsp_types::WorkspaceEdit>, RpcError>> {
        Box::pin(async move {
          family_handlers::rename(context.world().as_ref(), params).await
        })
      }
    }
  };
}

define_runtime_family_impl!(
  family local {
    capability = LocalEnvironment;
    protocol = (LocalContext, LocalServer);
    state = (LocalWorld, LocalFuture);
  }
);

#[cfg(not(target_arch = "wasm32"))]
define_runtime_family_impl!(
  family concurrent {
    capability = ConcurrentEnvironment;
    protocol = (ConcurrentContext, ConcurrentServer);
    state = (ConcurrentWorld, ConcurrentFuture);
  }
);

/// Construct the current-thread server from the local runtime family.
pub(super) fn create_local_server<E: LocalEnvironment>() -> LocalServer<LocalWorld<E>> {
  local::create_server()
}

/// Construct the native multi-threaded server from the concurrent runtime family.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn create_concurrent_server<E: ConcurrentEnvironment>() -> ConcurrentServer<ConcurrentWorld<E>> {
  concurrent::create_server()
}

#[cfg(test)]
mod tests {
  use std::pin::Pin;
  use std::sync::Arc;
  use std::task::Context;
  use std::task::Poll;

  use futures::Sink;
  use futures::future::join;
  use lsp_types::NumberOrString;
  use lsp_types::notification;
  use lsp_types::notification::Notification as _;
  use lsp_types::request;
  use lsp_types::request::Request as _;
  use parking_lot::Mutex;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use taplo_common::schema::transport::local_http_client;
  use taplo_lsp_async::MessageWriterError;
  use taplo_lsp_async::rpc;
  use url::Url;

  use crate::lsp_ext::notification as extension_notification;
  use crate::lsp_ext::request as extension_request;
  use crate::world::TestEnvironment;

  /// Document exercised through every registered document request family.
  const DOCUMENT_URI: &str = "file:///workspace/document.toml";

  /// Successful schema associated with the runtime document.
  const SCHEMA_URI: &str = "file:///workspace/schema.json";

  /// Writer retaining every protocol response and server notification.
  #[derive(Clone, Debug, Default)]
  struct CapturedWriter {
    /// Ordered protocol output.
    messages: Arc<Mutex<Vec<rpc::Message>>>,
  }

  impl CapturedWriter {
    /// Clone all messages emitted so far.
    fn messages(&self) -> Vec<rpc::Message> {
      self.messages.lock().clone()
    }
  }

  impl Sink<rpc::Message> for CapturedWriter {
    type Error = MessageWriterError;

    fn poll_ready(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
      taplo_lsp_async::message_writer_ready()
    }

    fn start_send(self: Pin<&mut Self>, message: rpc::Message) -> Result<(), Self::Error> {
      self.messages.lock().push(message);
      Ok(())
    }

    taplo_lsp_async::implement_message_writer_readiness!();
  }

  /// Construct one inbound request.
  fn request_message(id: i32, method: &str, params: Option<serde_json::Value>) -> rpc::Message {
    rpc::Message {
      jsonrpc: "2.0".into(),
      method: Some(method.into()),
      id: rpc::MessageId::Value(NumberOrString::Number(id)),
      params,
      result: None,
      error: None,
    }
  }

  /// Construct one inbound notification.
  fn notification_message(method: &str, params: Option<serde_json::Value>) -> rpc::Message {
    rpc::Message {
      jsonrpc: "2.0".into(),
      method: Some(method.into()),
      id: rpc::MessageId::Missing,
      params,
      result: None,
      error: None,
    }
  }

  /// Construct one client response to a server-initiated request.
  fn response_message(id: i32, result: serde_json::Value) -> rpc::Message {
    rpc::Message {
      jsonrpc: "2.0".into(),
      method:  None,
      id:      rpc::MessageId::Value(NumberOrString::Number(id)),
      params:  None,
      result:  Some(result),
      error:   None,
    }
  }

  /// Extract the successful result emitted for one request ID.
  fn successful_result(writer: &CapturedWriter, id: i32) -> Result<serde_json::Value, TestFailure> {
    let response = ensure_some(
      writer
        .messages()
        .into_iter()
        .find(|message| (message.method.is_none(), &message.id) == (true, &rpc::MessageId::Value(NumberOrString::Number(id)))),
      "the registered request must emit its correlated response",
    )?;
    ensure(response.error.is_none(), "a valid registered request must not emit an RPC error")?;
    ensure_some(response.result, "a successful registered request must carry a result channel")
  }

  /// Extract the ordered configuration items from one server request.
  fn configuration_items<'message>(
    message: &'message rpc::Message,
    context: &'static str,
  ) -> Result<&'message [serde_json::Value], TestFailure> {
    let items = ensure_some(
      message
        .params
        .as_ref()
        .and_then(|params| params.get("items"))
        .and_then(serde_json::Value::as_array),
      context,
    )?;
    Ok(items)
  }

  /// Exercise the complete Taplo handler registry through one execution family.
  macro_rules! runtime_registry_behavior {
    (
      $test:ident,
      server = $server:path,
      world = $world:path,
      clone = $clone:path
    ) => {
      #[test]
      fn $test() -> Result<(), TestFailure> {
        let environment = TestEnvironment::default();
        environment.insert_file(
          "/workspace/schema.json",
          br#"{
            "type": "object",
            "properties": {
              "name": {
                "title": "Name",
                "type": "string",
                "description": "The configured name.",
                "x-taplo": {
                  "links": {
                    "key": "https://example.com/name"
                  }
                }
              },
              "values": {
                "type": "array",
                "items": {
                  "type": "integer"
                }
              },
              "enabled": {
                "type": "boolean",
                "description": "Whether the feature is enabled."
              }
            }
          }"#
            .to_vec(),
        );
        let http = ensure_ok(local_http_client(), "the runtime HTTP client must initialize")?;
        let world = ensure_ok(
          $world(environment.clone(), http),
          "the runtime world must initialize",
        )?;
        let runtime = ensure_ok(
          tokio::runtime::Builder::new_current_thread().enable_all().build(),
          "the runtime behavior executor must initialize",
        )?;
        let server = $server();
        let writer = CapturedWriter::default();
        let route = |message: rpc::Message, context: &'static str| {
          ensure_ok(
            runtime.block_on(server.handle_message($clone(&world), message, writer.clone())),
            context,
          )
        };
        let exchange = |
          notification: rpc::Message,
          response: rpc::Message,
          notification_context: &'static str,
          response_context: &'static str,
        | -> Result<(), TestFailure> {
          let (notification_result, response_result) = runtime.block_on(join(
            server.handle_message($clone(&world), notification, writer.clone()),
            server.handle_message($clone(&world), response, writer.clone()),
          ));
          ensure_ok(notification_result, notification_context)?;
          ensure_ok(response_result, response_context)
        };

        route(
          request_message(
            0,
            request::Initialize::METHOD,
            Some(serde_json::json!({
              "processId": null,
              "rootUri": null,
              "capabilities": {},
              "workspaceFolders": []
            })),
          ),
          "the registered server must initialize",
        )?;
        let initialization = successful_result(&writer, 0)?;
        ensure(
          initialization.pointer("/capabilities/textDocumentSync").is_some(),
          "initialization must advertise full document synchronization",
        )?;
        exchange(
          notification_message(notification::Initialized::METHOD, Some(serde_json::json!({}))),
          response_message(
            0,
            serde_json::json!([{
              "schema": {
                "catalogs": []
              }
            }]),
          ),
          "the initialized notification must complete its client-configuration exchange",
          "the initial client-configuration response must reach its pending request",
        )?;
        exchange(
          notification_message(
            notification::DidChangeWorkspaceFolders::METHOD,
            Some(serde_json::json!({
              "event": {
                "added": [{
                  "uri": "file:///workspace",
                  "name": "workspace"
                }],
                "removed": []
              }
            })),
          ),
          response_message(
            1,
            serde_json::json!([
              {
                "schema": {
                  "catalogs": []
                }
              },
              {
                "schema": {
                  "catalogs": []
                }
              }
            ]),
          ),
          "the workspace transition must complete its scoped client-configuration exchange",
          "the scoped client-configuration response must reach its pending request",
        )?;
        let emitted_messages = writer.messages();
        let mut configuration_requests = emitted_messages
          .iter()
          .filter(|message| message.method.as_deref() == Some(request::WorkspaceConfiguration::METHOD));
        let initial_configuration = ensure_some(
          configuration_requests.next(),
          "initialization must issue one client-configuration request",
        )?;
        let scoped_configuration = ensure_some(
          configuration_requests.next(),
          "workspace mutation must issue one client-configuration request",
        )?;
        ensure(
          configuration_requests.next().is_none(),
          "the two lifecycle transitions must issue exactly two client-configuration requests",
        )?;
        let initial_items = configuration_items(
          initial_configuration,
          "the initial configuration request must carry its ordered item vector",
        )?;
        let initial_global = ensure_some(
          initial_items.first(),
          "the initial configuration request must carry its global item",
        )?;
        let scoped_items = configuration_items(
          scoped_configuration,
          "the workspace configuration request must carry its ordered item vector",
        )?;
        let scoped_global = ensure_some(
          scoped_items.first(),
          "the workspace configuration request must retain its global item",
        )?;
        let scoped_root = ensure_some(
          scoped_items.get(1),
          "the workspace configuration request must carry the added root item",
        )?;
        ensure(
          (
            initial_items.len(),
            initial_global.get("section"),
            initial_global.get("scopeUri"),
            scoped_items.len(),
            scoped_global.get("section"),
            scoped_global.get("scopeUri"),
            scoped_root.get("section"),
            scoped_root.get("scopeUri"),
          ) == (
            1,
            Some(&serde_json::json!("evenBetterToml")),
            None,
            2,
            Some(&serde_json::json!("evenBetterToml")),
            None,
            Some(&serde_json::json!("evenBetterToml")),
            Some(&serde_json::json!("file:///workspace")),
          ),
          "initialization must request global configuration and workspace mutation must add only the new root scope",
        )?;

        route(
          notification_message(
            notification::DidOpenTextDocument::METHOD,
            Some(serde_json::json!({
              "textDocument": {
                "uri": DOCUMENT_URI,
                "languageId": "toml",
                "version": 1,
                "text": "name=\"taplo\"\nvalues = [1, 2]\n"
              }
            })),
          ),
          "the registered document-open transition must complete",
        )?;
        ensure(
          writer
            .messages()
            .iter()
            .any(|message| message.method.as_deref() == Some(notification::PublishDiagnostics::METHOD)),
          "opening a document must publish its replacement diagnostics",
        )?;

        let document = serde_json::json!({
          "textDocument": {
            "uri": DOCUMENT_URI
          }
        });
        let positioned = serde_json::json!({
          "textDocument": {
            "uri": DOCUMENT_URI
          },
          "position": {
            "line": 0,
            "character": 1
          }
        });
        let registered_requests = [
          (1, request::FoldingRangeRequest::METHOD, document.clone()),
          (
            2,
            extension_request::ModernDocumentSymbolRequest::METHOD,
            document.clone(),
          ),
          (
            3,
            request::Formatting::METHOD,
            serde_json::json!({
              "textDocument": {
                "uri": DOCUMENT_URI
              },
              "options": {
                "tabSize": 2,
                "insertSpaces": true
              }
            }),
          ),
          (4, request::Completion::METHOD, positioned.clone()),
          (5, request::HoverRequest::METHOD, positioned.clone()),
          (6, request::DocumentLinkRequest::METHOD, document.clone()),
          (
            7,
            request::SemanticTokensFullRequest::METHOD,
            document.clone(),
          ),
          (8, request::PrepareRenameRequest::METHOD, positioned.clone()),
          (
            9,
            request::Rename::METHOD,
            serde_json::json!({
              "textDocument": {
                "uri": DOCUMENT_URI
              },
              "position": {
                "line": 0,
                "character": 1
              },
              "newName": "renamed"
            }),
          ),
          (
            10,
            extension_request::ConvertToJsonRequest::METHOD,
            serde_json::json!({
              "text": "name = \"taplo\"\n"
            }),
          ),
          (
            11,
            extension_request::ConvertToTomlRequest::METHOD,
            serde_json::json!({
              "text": "{\"name\":\"taplo\"}"
            }),
          ),
          (
            12,
            extension_request::ListSchemasRequest::METHOD,
            serde_json::json!({
              "documentUri": DOCUMENT_URI
            }),
          ),
          (
            13,
            extension_request::AssociatedSchemaRequest::METHOD,
            serde_json::json!({
              "documentUri": DOCUMENT_URI
            }),
          ),
        ];
        for (id, method, params) in registered_requests {
          route(
            request_message(id, method, Some(params)),
            "each registered document and conversion request must complete",
          )?;
          drop(successful_result(&writer, id)?);
        }
        let listed_schemas_before = successful_result(&writer, 12)?;
        ensure(
          listed_schemas_before.get("schemas").and_then(serde_json::Value::as_array).is_some(),
          "schema listing must serialize its catalog collection",
        )?;
        ensure(
          successful_result(&writer, 13)?.get("schema") == Some(&serde_json::Value::Null),
          "associated-schema lookup must report no effective schema before association",
        )?;
        ensure(
          successful_result(&writer, 4)?.is_null(),
          "completion without an effective schema must remain absent",
        )?;
        ensure(
          successful_result(&writer, 5)?.is_null(),
          "hover without an effective schema must remain absent",
        )?;
        ensure(
          successful_result(&writer, 6)?.is_null(),
          "disabled standalone document links must remain absent without an effective schema",
        )?;
        ensure(
          successful_result(&writer, 3)?.as_array().is_some_and(|edits| !edits.is_empty()),
          "formatting must return the edit for the unformatted opened document",
        )?;
        ensure(
          successful_result(&writer, 9)?
            .pointer("/changes")
            .is_some(),
          "rename must return a workspace edit for the selected identifier",
        )?;
        ensure(
          successful_result(&writer, 10)?
            .get("text")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| text.contains("\"name\"")),
          "TOML-to-JSON conversion must return converted text",
        )?;
        ensure(
          successful_result(&writer, 11)?
            .get("text")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| text.contains("name")),
          "JSON-to-TOML conversion must return converted text",
        )?;

        let document_url = ensure_ok(
          Url::parse(DOCUMENT_URI),
          "the runtime document URL must parse",
        )?;
        let schema_url = ensure_ok(
          Url::parse(SCHEMA_URI),
          "the runtime schema URL must parse",
        )?;
        let association = extension_notification::AssociateSchemaParams {
          document_uri: Some(document_url.clone()),
          schema_uri:   schema_url,
          rule:         extension_notification::AssociationRule::Url(document_url),
          priority:     None,
          meta:         Some(serde_json::json!({
            "name": "fixture"
          })),
        };
        route(
          notification_message(
            extension_notification::AssociateSchema::METHOD,
            Some(ensure_ok(
              serde_json::to_value(association),
              "the schema-association parameters must serialize",
            )?),
          ),
          "the registered manual-schema transition must complete",
        )?;
        ensure(
          writer.messages().iter().any(|message| {
            message.method.as_deref()
              == Some(extension_notification::DidChangeSchemaAssociation::METHOD)
          }),
          "manual association must publish its effective-schema notification",
        )?;

        for (id, method) in [
          (14, extension_request::ListSchemasRequest::METHOD),
          (15, extension_request::AssociatedSchemaRequest::METHOD),
        ] {
          route(
            request_message(
              id,
              method,
              Some(serde_json::json!({
                "documentUri": DOCUMENT_URI
              })),
            ),
            "schema queries must observe the committed manual association",
          )?;
        }
        ensure(
          successful_result(&writer, 14)? == listed_schemas_before,
          "an exact-document association must not alter the catalog schema listing",
        )?;
        ensure(
          successful_result(&writer, 15)?
            .pointer("/schema/url")
            == Some(&serde_json::json!(SCHEMA_URI)),
          "associated-schema lookup must select the committed manual association",
        )?;

        for (id, method, params) in [
          (
            16,
            request::Completion::METHOD,
            serde_json::json!({
              "textDocument": {
                "uri": DOCUMENT_URI
              },
              "position": {
                "line": 2,
                "character": 0
              }
            }),
          ),
          (17, request::HoverRequest::METHOD, positioned.clone()),
          (18, request::DocumentLinkRequest::METHOD, document.clone()),
        ] {
          route(
            request_message(id, method, Some(params)),
            "schema-backed requests must observe the committed manual association",
          )?;
        }
        ensure(
          successful_result(&writer, 16)?
            .as_array()
            .is_some_and(|items| {
              items.iter().any(|item| {
                (
                  item.get("label"),
                  item.pointer("/documentation/value"),
                ) == (
                  Some(&serde_json::json!("enabled")),
                  Some(&serde_json::json!("Whether the feature is enabled.")),
                )
              })
            }),
          "schema-backed completion must offer the missing property with its documentation",
        )?;
        ensure(
          successful_result(&writer, 17)?
            .pointer("/contents/value")
            == Some(&serde_json::json!(
              "[Name](https://example.com/name)\n\nThe configured name."
            )),
          "schema-backed hover must embed the key link while standalone links are disabled",
        )?;
        ensure(
          successful_result(&writer, 18)?.is_null(),
          "an associated schema must not override the disabled standalone-link policy",
        )?;

        route(
          notification_message(
            notification::DidChangeConfiguration::METHOD,
            Some(serde_json::json!({
              "settings": {
                "schema": {
                  "catalogs": [],
                  "links": true
                }
              }
            })),
          ),
          "standalone schema links must be enabled through the public configuration transition",
        )?;
        for (id, method, params) in [
          (19, request::HoverRequest::METHOD, positioned.clone()),
          (20, request::DocumentLinkRequest::METHOD, document.clone()),
        ] {
          route(
            request_message(id, method, Some(params)),
            "schema-backed requests must observe the committed link policy",
          )?;
        }
        ensure(
          successful_result(&writer, 19)?
            .pointer("/contents/value")
            == Some(&serde_json::json!("The configured name.")),
          "standalone-link mode must keep the external link out of hover documentation",
        )?;
        ensure(
          successful_result(&writer, 20)?
            .as_array()
            .is_some_and(|links| {
              links.iter().any(|link| {
                (
                  link.get("target"),
                  link.pointer("/range/start/line"),
                ) == (
                  Some(&serde_json::json!("https://example.com/name")),
                  Some(&serde_json::json!(0)),
                )
              })
            }),
          "schema-backed document links must target the configured key documentation",
        )?;

        route(
          notification_message(
            notification::DidChangeTextDocument::METHOD,
            Some(serde_json::json!({
              "textDocument": {
                "uri": DOCUMENT_URI,
                "version": 2
              },
              "contentChanges": [{
                "text": "name = 7\n"
              }]
            })),
          ),
          "the registered full-sync document change must complete",
        )?;
        route(
          notification_message(
            notification::DidSaveTextDocument::METHOD,
            Some(serde_json::json!({
              "textDocument": {
                "uri": DOCUMENT_URI
              }
            })),
          ),
          "the registered save notification must preserve its ordered no-op",
        )?;
        route(
          notification_message(
            notification::DidChangeConfiguration::METHOD,
            Some(serde_json::json!({
              "settings": {}
            })),
          ),
          "pushed client configuration must apply through the registry",
        )?;

        for method in [
          notification::DidOpenTextDocument::METHOD,
          notification::DidChangeTextDocument::METHOD,
          notification::DidCloseTextDocument::METHOD,
          notification::DidChangeConfiguration::METHOD,
          notification::DidChangeWorkspaceFolders::METHOD,
          extension_notification::AssociateSchema::METHOD,
        ] {
          route(
            notification_message(method, None),
            "missing notification parameters must be contained at the notification boundary",
          )?;
        }

        route(
          notification_message(
            notification::DidCloseTextDocument::METHOD,
            Some(serde_json::json!({
              "textDocument": {
                "uri": DOCUMENT_URI
              }
            })),
          ),
          "the registered document-close transition must complete",
        )?;
        let cleared_diagnostics = writer
          .messages()
          .into_iter()
          .rev()
          .find(|message| message.method.as_deref() == Some(notification::PublishDiagnostics::METHOD))
          .and_then(|message| message.params);
        ensure(
          cleared_diagnostics
            .as_ref()
            .and_then(|params| params.get("diagnostics"))
            .and_then(serde_json::Value::as_array)
            .is_some_and(Vec::is_empty),
          "closing the document must publish an empty diagnostics replacement",
        )?;

        route(
          request_message(99, request::Shutdown::METHOD, None),
          "the registered server must complete shutdown",
        )?;
        ensure(
          successful_result(&writer, 99)?.is_null(),
          "shutdown must emit the standard null result",
        )?;
        route(
          notification_message(notification::Exit::METHOD, None),
          "exit must succeed after the shutdown response",
        )
      }
    };
  }

  runtime_registry_behavior!(
    local_registry_routes_protocol_behavior,
    server = crate::create_local_server,
    world = crate::create_local_world,
    clone = std::rc::Rc::clone
  );

  #[cfg(not(target_arch = "wasm32"))]
  runtime_registry_behavior!(
    concurrent_registry_routes_protocol_behavior,
    server = crate::create_concurrent_server,
    world = crate::create_concurrent_world,
    clone = Arc::clone
  );
}
