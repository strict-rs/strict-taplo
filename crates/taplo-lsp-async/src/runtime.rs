//! Local and concurrent server families generated from one protocol adapter.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::marker::PhantomData;
use std::mem;
use std::ops::Deref;
use std::rc::Rc;
use std::sync::Arc;

use futures::Future;
use futures::FutureExt as _;
use futures::SinkExt as _;
use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::future::Either;
use futures::future::LocalBoxFuture;
use futures::future::ready;
use futures::future::select;
use futures::lock::Mutex as AsyncMutex;
use futures::sink::Sink;
use lsp_types::notification;
use lsp_types::notification::Notification;
use lsp_types::request::Request;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::Instrument as _;

use crate::CancellationState as _;
use crate::ConcurrentCancelToken;
use crate::ConcurrentCancellation;
use crate::ConcurrentMutationStorage;
use crate::Inbound;
use crate::InboundNotification;
use crate::InboundRequest;
use crate::LocalCancelToken;
use crate::LocalCancellation;
use crate::LocalMutationStorage;
use crate::MessageSchedule;
use crate::MessageWriterError;
use crate::MutationBarrier;
use crate::NotificationDisposition;
use crate::Params;
use crate::RequestKind;
use crate::RequestOutcome;
use crate::ServerError;
use crate::SessionState;
use crate::classify_message;
use crate::error_message;
use crate::outcome_message;
use crate::request_message;
use crate::rpc;
use crate::schedule_message;
use crate::serialize_optional;

/// Define one kind-safe request and notification adapter family.
macro_rules! define_handler_family {
  (
    erased_request = $erased_request:ident,
    erased_notification = $erased_notification:ident,
    request_handler = $request_handler:ident,
    notification_handler = $notification_handler:ident,
    context = $context:ident,
    boxed_future = $boxed_future:ident,
    box_method = $box_method:ident,
    trait_bounds = [$($trait_bound:path),*],
    value_bounds = [$($value_bound:path),*],
    world_bounds = [$($world_bound:path),*],
  ) => {
    /// Kind-safe erased request handler for this execution model.
    trait $erased_request<W: Clone>: 'static $(+ $trait_bound)* {
      /// Deserialize parameters, invoke the typed handler, and serialize its
      /// result.
      fn handle(
        &self,
        context: $context<W>,
        params: Option<serde_json::Value>,
      ) -> $boxed_future<'static, RequestOutcome>;
    }

    /// Kind-safe erased notification handler for this execution model.
    trait $erased_notification<W: Clone>: 'static $(+ $trait_bound)* {
      /// Deserialize parameters and invoke the typed notification handler.
      fn handle(
        &self,
        context: $context<W>,
        params: Option<serde_json::Value>,
      ) -> $boxed_future<'static, Result<(), ServerError>>;
    }

    /// Typed request-handler adapter for this execution model.
    struct $request_handler<R, F, W>
    where
      R: Request,
      F: Future<Output = Result<R::Result, rpc::RpcError>>,
      W: Clone,
    {
      /// Registered typed handler function.
      function: fn($context<W>, Params<R::Params>) -> F,
      /// Adapter type ownership without storing values.
      marker:   PhantomData<fn() -> R>,
    }

    impl<R, F, W> $request_handler<R, F, W>
    where
      R: Request,
      F: Future<Output = Result<R::Result, rpc::RpcError>>,
      W: Clone,
    {
      /// Construct a typed request-handler adapter.
      #[allow(
        clippy::single_call_fn,
        reason = "the typed request-handler constructor names the registration-to-erasure boundary"
      )]
      fn new(function: fn($context<W>, Params<R::Params>) -> F) -> Self {
        Self {
          function,
          marker: PhantomData,
        }
      }
    }

    impl<R, F, W> $erased_request<W> for $request_handler<R, F, W>
    where
      R: Request + 'static,
      R::Params: DeserializeOwned $(+ $value_bound)* + 'static,
      R::Result: Serialize $(+ $value_bound)* + 'static,
      F: Future<Output = Result<R::Result, rpc::RpcError>> $(+ $value_bound)* + 'static,
      W: Clone $(+ $world_bound)* + 'static,
    {
      fn handle(
        &self,
        context: $context<W>,
        params: Option<serde_json::Value>,
      ) -> $boxed_future<'static, RequestOutcome> {
        let function = self.function;
        async move {
          invoke_request(deserialize_params(params).map(|params| function(context, params))).await
        }
        .$box_method()
      }
    }

    /// Typed notification-handler adapter for this execution model.
    struct $notification_handler<N, F, W>
    where
      N: Notification,
      F: Future<Output = Result<(), ServerError>>,
      W: Clone,
    {
      /// Registered typed handler function.
      function: fn($context<W>, Params<N::Params>) -> F,
      /// Adapter type ownership without storing values.
      marker:   PhantomData<fn() -> N>,
    }

    impl<N, F, W> $notification_handler<N, F, W>
    where
      N: Notification,
      F: Future<Output = Result<(), ServerError>>,
      W: Clone,
    {
      /// Construct a typed notification-handler adapter.
      #[allow(
        clippy::single_call_fn,
        reason = "the typed notification-handler constructor names the registration-to-erasure boundary"
      )]
      fn new(function: fn($context<W>, Params<N::Params>) -> F) -> Self {
        Self {
          function,
          marker: PhantomData,
        }
      }
    }

    impl<N, F, W> $erased_notification<W> for $notification_handler<N, F, W>
    where
      N: Notification + 'static,
      N::Params: DeserializeOwned $(+ $value_bound)* + 'static,
      F: Future<Output = Result<(), ServerError>> $(+ $value_bound)* + 'static,
      W: Clone $(+ $world_bound)* + 'static,
    {
      fn handle(
        &self,
        context: $context<W>,
        params: Option<serde_json::Value>,
      ) -> $boxed_future<'static, Result<(), ServerError>> {
        let function = self.function;
        async move {
          invoke_notification(deserialize_params(params).map(|params| function(context, params)))
            .await
        }
        .$box_method()
      }
    }
  };
}

define_handler_family!(
  erased_request = LocalErasedRequestHandler,
  erased_notification = LocalErasedNotificationHandler,
  request_handler = LocalRequestHandler,
  notification_handler = LocalNotificationHandler,
  context = LocalContext,
  boxed_future = LocalBoxFuture,
  box_method = boxed_local,
  trait_bounds = [],
  value_bounds = [],
  world_bounds = [],
);

define_handler_family!(
  erased_request = ConcurrentErasedRequestHandler,
  erased_notification = ConcurrentErasedNotificationHandler,
  request_handler = ConcurrentRequestHandler,
  notification_handler = ConcurrentNotificationHandler,
  context = ConcurrentContext,
  boxed_future = BoxFuture,
  box_method = boxed,
  trait_bounds = [Send, Sync],
  value_bounds = [Send],
  world_bounds = [Send, Sync],
);

/// Deserialize optional raw parameters for either handler family.
fn deserialize_params<P: DeserializeOwned>(params: Option<serde_json::Value>) -> Result<Params<P>, String> {
  params
    .map(serde_json::from_value)
    .transpose()
    .map(Params::from)
    .map_err(|error| error.to_string())
}

/// Invoke a request handler after parameter deserialization.
async fn invoke_request<R, F>(future: Result<F, String>) -> RequestOutcome
where
  R: Serialize,
  F: Future<Output = Result<R, rpc::RpcError>>,
{
  let handler_future = match future {
    Ok(handler_future) => handler_future,
    Err(detail) => return RequestOutcome::InvalidParams(detail),
  };
  match handler_future.await {
    Ok(handler_result) => match serde_json::to_value(handler_result) {
      Ok(serialized_result) => RequestOutcome::Success(serialized_result),
      Err(error) => RequestOutcome::SerializationFailure(error.to_string()),
    },
    Err(error) => RequestOutcome::RpcError(error),
  }
}

/// Invoke a notification handler only after successful parameter deserialization.
async fn invoke_notification<F>(future: Result<F, String>) -> Result<(), ServerError>
where
  F: Future<Output = Result<(), ServerError>>,
{
  match future {
    Ok(handler_future) => handler_future.await,
    Err(error) => {
      tracing::warn!(?error, "invalid notification parameters");
      Ok(())
    }
  }
}

/// Ordering lane selected for a registered notification handler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NotificationOrdering {
  /// Dispatch independently of ordered state mutations.
  Independent,
  /// Serialize dispatch through the mutation barrier.
  Mutation,
}

/// Generate public notification registrars from one ordering-aware method shape.
macro_rules! define_notification_registrations {
  (
    context = $context:ident,
    world = $world:ident,
    value_bounds = $value_bounds:tt,
    $(
      $(#[$attribute:meta])*
      $method:ident => $ordering:ident;
    )+
  ) => {
    $(
      define_notification_registrations!(
        @method
        context = $context,
        world = $world,
        value_bounds = $value_bounds,
        $(#[$attribute])*
        $method => $ordering
      );
    )+
  };
  (
    @method
    context = $context:ident,
    world = $world:ident,
    value_bounds = [$($value_bound:path),*],
    $(#[$attribute:meta])*
    $method:ident => $ordering:ident
  ) => {
      $(#[$attribute])*
      #[must_use]
      pub fn $method<N, F>(
        self,
        handler: fn($context<$world>, Params<N::Params>) -> F,
      ) -> Self
      where
        N: Notification + 'static,
        N::Params: DeserializeOwned $(+ $value_bound)* + 'static,
        F: Future<Output = Result<(), ServerError>> $(+ $value_bound)* + 'static,
      {
        self.with_notification::<N, F>(
          handler,
          NotificationOrdering::$ordering,
        )
      }
  };
}

/// Define one server family while preserving its ownership and future bounds.
macro_rules! define_runtime_family {
  (local) => {
    define_runtime_family!(
      @implement
      family = (LocalContext, LocalServer, LocalMessageWriter, LocalDeferredTasks, Rc, LocalBoxFuture, boxed_local),
      state = (LocalCancellation, LocalCancelToken, LocalMutationStorage),
      handlers = (LocalErasedRequestHandler, LocalErasedNotificationHandler, LocalRequestHandler, LocalNotificationHandler),
      behavior = (run_local_deferred, [], [], []),
    );
  };
  (concurrent) => {
    define_runtime_family!(
      @implement
      family = (ConcurrentContext, ConcurrentServer, ConcurrentMessageWriter, ConcurrentDeferredTasks, Arc, BoxFuture, boxed),
      state = (ConcurrentCancellation, ConcurrentCancelToken, ConcurrentMutationStorage),
      handlers = (ConcurrentErasedRequestHandler, ConcurrentErasedNotificationHandler, ConcurrentRequestHandler, ConcurrentNotificationHandler),
      behavior = (run_concurrent_deferred, [Send], [Send], [Send, Sync]),
    );
  };
  (
    @implement
    family = ($context:ident, $server:ident, $writer:ident, $deferred:ident, $shared:ident, $boxed_future:ident, $box_method:ident),
    state = ($cancellation:ident, $cancel_token:ident, $mutation_storage:ident),
    handlers = ($erased_request:ident, $erased_notification:ident, $request_handler:ident, $notification_handler:ident),
    behavior = ($run_deferred:ident, [$($writer_bound:path),*], [$($value_bound:path),*], [$($world_bound:path),*]),
  ) => {
    /// Response transport accepted by this server family.
    pub trait $writer:
      Sink<rpc::Message, Error = MessageWriterError> + Unpin $(+ $writer_bound)*
    {
    }

    impl<T> $writer for T where
      T: Sink<rpc::Message, Error = MessageWriterError> + Unpin $(+ $writer_bound)*
    {
    }

    /// Deferred tasks owned by one handler invocation.
    type $deferred =
      $shared<AsyncMutex<Vec<$boxed_future<'static, Result<(), ServerError>>>>>;

    /// Handler context for this execution model.
    #[derive(Clone)]
    pub struct $context<W: Clone> {
      /// Mutable server session state.
      session:         $shared<AsyncMutex<SessionState<$cancellation>>>,
      /// Cancellation token for the current inbound request.
      cancel_token:    $cancel_token,
      /// ID of an outbound request currently awaited by this context clone.
      last_request_id: Option<rpc::RequestId>,
      /// Serialized access to the response transport.
      writer:          $shared<AsyncMutex<Box<dyn $writer>>>,
      /// User world state.
      world:           W,
      /// Work deferred until handler completion.
      deferred:        $deferred,
    }

    impl<W: Clone> fmt::Debug for $context<W> {
      fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct(stringify!($context)).finish_non_exhaustive()
      }
    }

    impl<W: Clone> Deref for $context<W> {
      type Target = W;

      fn deref(&self) -> &Self::Target {
        &self.world
      }
    }

    impl<W: Clone $(+ $world_bound)* + 'static> $context<W> {
      /// Return whether initialization completed successfully.
      pub fn is_initialized(&self) -> $boxed_future<'_, bool> {
        async move { self.session.lock().await.is_initialized() }.$box_method()
      }

      /// Return whether a successful shutdown response has been sent.
      pub fn is_shutting_down(&self) -> $boxed_future<'_, bool> {
        async move { self.session.lock().await.is_shutting_down() }.$box_method()
      }

      /// Borrow the user world state.
      #[must_use]
      pub const fn world(&self) -> &W {
        &self.world
      }

      /// Mutably borrow this invocation's cancellation token.
      pub const fn cancel_token(&mut self) -> &mut $cancel_token {
        &mut self.cancel_token
      }

      /// Defer a future until the handler completes.
      ///
      /// Request-deferred work runs only after its response is sent
      /// successfully. Notification work runs after its handler completes.
      pub fn defer<F>(&self, future: F) -> $boxed_future<'_, ()>
      where
        F: Future<Output = Result<(), ServerError>> $(+ $value_bound)* + 'static,
      {
        async move {
          self.deferred.lock().await.push(future.$box_method());
        }
        .$box_method()
      }

      /// Send an outbound request and await its typed response.
      ///
      /// # Errors
      ///
      /// Returns [`ServerError`] when serialization, transport, response
      /// delivery, or response deserialization fails.
      pub fn write_request<R>(
        &mut self,
        params: Option<R::Params>,
      ) -> $boxed_future<'_, Result<rpc::Response<R::Result>, ServerError>>
      where
        R: Request + 'static,
        R::Params: Serialize + DeserializeOwned + fmt::Debug $(+ $value_bound)*,
        R::Result: DeserializeOwned,
      {
        async move {
          let serialized_params = serialize_optional(params, "outbound request parameters")?;
          let (request_id, receiver) = {
            let mut session = self.session.lock().await;
            let request_id = session.next_outbound_id()?;
            let (sender, receiver) = oneshot::channel();
            if session.pending_requests.insert(request_id.clone(), sender).is_some() {
              return Err(ServerError::RequestIdExhausted);
            }
            drop(session);
            (request_id, receiver)
          };

          let message = request_message(R::METHOD, Some(request_id.clone()), serialized_params);
          self.last_request_id = Some(request_id.clone());
          if let Err(error) = self.writer.lock().await.send(message).await {
            let removed = self
              .session
              .lock()
              .await
              .pending_requests
              .remove(&request_id)
              .is_some();
            tracing::trace!(id = ?request_id, removed, "discarded outbound request after transport failure");
            self.last_request_id = None;
            return Err(ServerError::Transport(error));
          }

          let response = receiver.await;
          self.last_request_id = None;
          let response = response.map_err(|_closed| ServerError::ResponseChannelClosed)?;
          let response_result = response
            .result
            .map(serde_json::from_value)
            .transpose()
            .map_err(ServerError::ResponseDeserialization)?;
          Ok(rpc::Response {
            jsonrpc: response.jsonrpc,
            id: response.id,
            result: response_result,
            error: response.error,
          })
        }
        .$box_method()
      }

      /// Send an outbound notification.
      ///
      /// # Errors
      ///
      /// Returns [`ServerError`] when parameter serialization or transport
      /// output fails.
      pub fn write_notification<N>(
        &mut self,
        params: Option<N::Params>,
      ) -> $boxed_future<'_, Result<(), ServerError>>
      where
        N: Notification + 'static,
        N::Params: Serialize + DeserializeOwned + fmt::Debug $(+ $value_bound)*,
      {
        async move {
          let message = request_message(
            N::METHOD,
            None,
            serialize_optional(params, "outbound notification parameters")?,
          );
          self
            .writer
            .lock()
            .await
            .send(message)
            .await
            .map_err(ServerError::Transport)
        }
        .$box_method()
      }

      /// Cancel the currently awaited outbound request, if any.
      ///
      /// # Errors
      ///
      /// Returns [`ServerError`] if the cancellation notification cannot be
      /// written.
      pub fn cancel(&mut self) -> $boxed_future<'_, Result<(), ServerError>> {
        async move {
          if let Some(request_id) = Option::take(&mut self.last_request_id) {
            self
              .write_notification::<notification::Cancel>(Some(
                lsp_types::CancelParams {
                  id: request_id,
                },
              ))
              .await
          } else {
            Ok(())
          }
        }
        .$box_method()
      }
    }

    /// JSON-RPC/LSP server with kind-separated handler registries.
    pub struct $server<W: Clone $(+ $world_bound)*> {
      /// Mutable session state.
      session: $shared<AsyncMutex<SessionState<$cancellation>>>,
      /// Typed request handlers keyed by method.
      request_handlers:
        HashMap<&'static str, $shared<dyn $erased_request<W>>>,
      /// Typed notification handlers keyed by method.
      notification_handlers:
        HashMap<&'static str, $shared<dyn $erased_notification<W>>>,
      /// Notification methods serialized through the ordered mutation lane.
      mutation_methods: HashSet<&'static str>,
      /// Issued and committed mutation revisions.
      mutation_barrier: MutationBarrier<$mutation_storage>,
    }

    impl<W: Clone $(+ $world_bound)*> fmt::Debug for $server<W> {
      fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
          .debug_struct(stringify!($server))
          .field("request_handler_count", &self.request_handlers.len())
          .field("notification_handler_count", &self.notification_handlers.len())
          .field("mutation_method_count", &self.mutation_methods.len())
          .finish_non_exhaustive()
      }
    }

    impl<W: Clone $(+ $world_bound)* + 'static> Default for $server<W> {
      fn default() -> Self {
        Self::new()
      }
    }

    impl<W: Clone $(+ $world_bound)* + 'static> $server<W> {
      /// Construct an unregistered server.
      #[must_use]
      #[allow(
        clippy::single_call_fn,
        reason = "the public runtime constructor remains an explicit server-creation API alongside Default"
      )]
      pub fn new() -> Self {
        Self {
          session: $shared::new(AsyncMutex::new(SessionState::new())),
          request_handlers: HashMap::new(),
          notification_handlers: HashMap::new(),
          mutation_methods: HashSet::new(),
          mutation_barrier: MutationBarrier::default(),
        }
      }

      define_notification_registrations!(
        context = $context,
        world = W,
        value_bounds = [$($value_bound),*],
        /// Register a typed notification handler under `N::METHOD`.
        on_notification => Independent;
        /// Register a typed notification on the ordered state-mutation lane.
        on_mutation_notification => Mutation;
      );

      /// Register one notification handler and atomically replace its ordering
      /// classification.
      fn with_notification<N, F>(
        mut self,
        handler: fn($context<W>, Params<N::Params>) -> F,
        ordering: NotificationOrdering,
      ) -> Self
      where
        N: Notification + 'static,
        N::Params: DeserializeOwned $(+ $value_bound)* + 'static,
        F: Future<Output = Result<(), ServerError>> $(+ $value_bound)* + 'static,
      {
        let replaced = self
          .notification_handlers
          .insert(
            N::METHOD,
            $shared::new($notification_handler::<N, _, _>::new(handler)),
          )
          .is_some();
        let previous_ordering = if self.mutation_methods.remove(N::METHOD) {
          NotificationOrdering::Mutation
        } else {
          NotificationOrdering::Independent
        };
        let newly_classified =
          ordering == NotificationOrdering::Mutation
            && self.mutation_methods.insert(N::METHOD);
        tracing::debug!(
          method = N::METHOD,
          replaced,
          newly_classified,
          ?previous_ordering,
          ?ordering,
          "registered notification handler"
        );
        self
      }

      /// Register a typed request handler under `R::METHOD`.
      #[must_use]
      pub fn on_request<R, F>(
        mut self,
        handler: fn($context<W>, Params<R::Params>) -> F,
      ) -> Self
      where
        R: Request + 'static,
        R::Params: DeserializeOwned $(+ $value_bound)* + 'static,
        R::Result: Serialize $(+ $value_bound)* + 'static,
        F: Future<Output = Result<R::Result, rpc::RpcError>> $(+ $value_bound)* + 'static,
      {
        let replaced = self
          .request_handlers
          .insert(
            R::METHOD,
            $shared::new($request_handler::<R, _, _>::new(handler)),
          )
          .is_some();
        tracing::debug!(method = R::METHOD, replaced, "registered request handler");
        self
      }

      /// Classify and handle one JSON-RPC wire message.
      ///
      /// # Errors
      ///
      /// Returns [`ServerError`] when lifecycle validation, handler execution,
      /// response delivery, or outbound-response routing fails.
      pub fn handle_message<T>(
        &self,
        world: W,
        message: rpc::Message,
        writer: T,
      ) -> $boxed_future<'static, Result<(), ServerError>>
      where
        T: $writer + Clone + 'static,
      {
        let session = $shared::clone(&self.session);
        let mutation_barrier = self.mutation_barrier.clone();
        let schedule =
          schedule_message(&message, &self.mutation_methods, &mutation_barrier);
        let request_handler = message
          .method
          .as_deref()
          .and_then(|method| self.request_handlers.get(method))
          .cloned();
        let notification_handler = message
          .method
          .as_deref()
          .and_then(|method| self.notification_handlers.get(method))
          .cloned();

        async move {
          let inbound = classify_message(message);
          let _mutation_ticket = match schedule {
            MessageSchedule::Request {
              prior_revision,
            } => {
              mutation_barrier.wait_for(prior_revision).await;
              None
            }
            MessageSchedule::Mutation {
              ticket,
            } => {
              let ticket = ticket?;
              ticket.wait_turn().await;
              Some(ticket)
            }
            MessageSchedule::Independent => None,
          };
          match inbound {
            Inbound::Request(request) => {
              Self::handle_request(
                session,
                world,
                request,
                request_handler,
                writer,
              )
              .await
            }
            Inbound::Notification(notification) => {
              Self::handle_notification(
                session,
                world,
                notification,
                notification_handler,
                writer,
              )
              .await
            }
            Inbound::Response(response) => {
              Self::handle_response(session, response).await
            }
            Inbound::InvalidRequest(response) => writer
              .clone()
              .send(response)
              .await
              .map_err(ServerError::Transport),
            Inbound::InvalidResponse(error) => Err(error),
          }
        }
        .$box_method()
      }

      /// Return whether a successful shutdown response has been sent.
      pub fn is_shutting_down(&self) -> $boxed_future<'_, bool> {
        async move { self.session.lock().await.is_shutting_down() }.$box_method()
      }

      /// Deliver one client response to a pending outbound request.
      #[allow(
        clippy::single_call_fn,
        reason = "response handling remains isolated to own pending-request removal and response-channel completion"
      )]
      fn handle_response(
        session: $shared<AsyncMutex<SessionState<$cancellation>>>,
        response: rpc::Response<serde_json::Value>,
      ) -> $boxed_future<'static, Result<(), ServerError>> {
        async move {
          let response_contract = if response.jsonrpc != "2.0" {
            Err(ServerError::InvalidResponseVersion {
              id:      response.id.clone(),
              version: response.jsonrpc.clone(),
            })
          } else if response.result.is_some() == response.error.is_some() {
            Err(ServerError::InvalidResponseShape {
              id: response.id.clone(),
            })
          } else {
            Ok(())
          };
          if let Err(error) = response_contract {
            let removed = session
              .lock()
              .await
              .pending_requests
              .remove(&response.id)
              .is_some();
            tracing::trace!(id = ?response.id, removed, "discarded malformed outbound response");
            return Err(error);
          }
          let sender = session.lock().await.pending_requests.remove(&response.id);
          match sender {
            Some(sender) => {
              if sender.send(response).is_err() {
                tracing::warn!("outbound response receiver was dropped");
              }
            }
            None => {
              tracing::warn!(?response, "ignoring response with unknown request ID");
            }
          }
          Ok(())
        }
        .$box_method()
      }

      /// Route one concrete-ID request through lifecycle gates.
      #[allow(
        clippy::single_call_fn,
        reason = "request handling remains isolated to own lifecycle, cancellation, and typed response dispatch"
      )]
      fn handle_request<T>(
        session: $shared<AsyncMutex<SessionState<$cancellation>>>,
        world: W,
        request: InboundRequest,
        handler: Option<$shared<dyn $erased_request<W>>>,
        mut writer: T,
      ) -> $boxed_future<'static, Result<(), ServerError>>
      where
        T: $writer + Clone + 'static,
      {
        async move {
          if request.jsonrpc != "2.0" {
            return writer
              .send(error_message(
                request.id,
                rpc::RpcError::invalid_request()
                  .with_details("only JSON-RPC version 2.0 is accepted"),
              ))
              .await
              .map_err(ServerError::Transport);
          }

          let begin = session.lock().await.begin_request(&request);
          let (kind, cancel_token) = match begin {
            Ok(begin) => begin,
            Err(error) => {
              return writer
                .send(error_message(request.id, error))
                .await
                .map_err(ServerError::Transport);
            }
          };
          let context = $context {
            session: $shared::clone(&session),
            cancel_token,
            last_request_id: None,
            writer: $shared::new(AsyncMutex::new(Box::new(writer.clone()))),
            world,
            deferred: Default::default(),
          };
          let outcome_future: $boxed_future<'static, RequestOutcome> =
            match (handler, kind) {
              (Some(handler), _) => handler
                .handle(context.clone(), request.params)
                .instrument(
                  tracing::trace_span!(
                    "request handler",
                    method = ?request.method
                  ),
                )
                .$box_method(),
              (None, RequestKind::Shutdown) => {
                ready(RequestOutcome::Success(serde_json::Value::Null))
                  .$box_method()
              }
              (None, RequestKind::Initialize | RequestKind::Ordinary) => {
                ready(RequestOutcome::RpcError(rpc::RpcError::method_not_found()))
                  .$box_method()
              }
            };
          let outcome = if kind == RequestKind::Ordinary {
            match select(outcome_future, context.cancel_token.clone()).await {
              Either::Left((outcome, _cancel)) => outcome,
              Either::Right(((), _handler)) => {
                RequestOutcome::RpcError(rpc::RpcError::request_cancelled())
              }
            }
          } else {
            outcome_future.await
          };
          let handler_succeeded = matches!(outcome, RequestOutcome::Success(_));
          if let Err(error) = writer
            .send(outcome_message(request.id.clone(), outcome))
            .await
          {
            session.lock().await.abort_request(&request.id, kind);
            return Err(ServerError::Transport(error));
          }
          session.lock().await.commit_request(kind, handler_succeeded);
          let deferred_result = $run_deferred(&context, &request.method).await;
          session.lock().await.finish_request(&request.id);
          deferred_result
        }
        .$box_method()
      }

      /// Route one notification through cancellation or the notification
      /// registry.
      #[allow(
        clippy::single_call_fn,
        reason = "notification handling remains isolated to own cancellation, lifecycle, and terminal-notification semantics"
      )]
      fn handle_notification<T>(
        session: $shared<AsyncMutex<SessionState<$cancellation>>>,
        world: W,
        notification: InboundNotification,
        handler: Option<$shared<dyn $erased_notification<W>>>,
        writer: T,
      ) -> $boxed_future<'static, Result<(), ServerError>>
      where
        T: $writer + Clone + 'static,
      {
        async move {
          let disposition = session
            .lock()
            .await
            .prepare_notification(&notification)?;
          if disposition == NotificationDisposition::Handled {
            return Ok(());
          }
          let Some(handler) = handler else {
            tracing::warn!(
              method = ?notification.method,
              "no notification handler registered"
            );
            return Ok(());
          };
          let context = $context {
            session,
            cancel_token: $cancellation::default().token(),
            last_request_id: None,
            writer: $shared::new(AsyncMutex::new(Box::new(writer))),
            world,
            deferred: Default::default(),
          };
          handler
            .handle(context.clone(), notification.params)
            .instrument(
              tracing::trace_span!(
                "notification handler",
                method = ?notification.method
              ),
            )
            .await?;
          $run_deferred(&context, &notification.method).await
        }
        .$box_method()
      }

    }

    /// Run and drain deferred work for one completed invocation.
    fn $run_deferred<'context, W: Clone $(+ $world_bound)* + 'static>(
      context: &'context $context<W>,
      method: &'context str,
    ) -> $boxed_future<'context, Result<(), ServerError>> {
      async move {
        let deferred = mem::take(&mut *context.deferred.lock().await);
        for task in deferred {
          task
            .instrument(tracing::trace_span!("deferred task", ?method))
            .await?;
        }
        Ok(())
      }
      .$box_method()
    }
  };
}

define_runtime_family!(local);

define_runtime_family!(concurrent);

#[cfg(test)]
mod tests {
  use futures::executor::block_on;
  use futures::future::Ready;
  use futures::future::ready;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_contains;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;

  use super::RequestOutcome;
  use super::ServerError;
  use super::deserialize_params;
  use super::invoke_notification;
  use super::invoke_request;
  use super::rpc;
  use crate::SerializationFailureFixture;

  #[test]
  fn parameter_deserialization_preserves_absence_values_and_typed_rejection() -> Result<(), TestFailure> {
    let absent = ensure_some(deserialize_params::<u64>(None).ok(), "absent optional parameters must deserialize")?;
    ensure(
      absent.optional().is_none(),
      "absent wire parameters must remain absent for the typed handler",
    )?;

    let present = ensure_some(
      deserialize_params::<u64>(Some(serde_json::json!(41))).ok(),
      "a compatible wire value must deserialize",
    )?;
    ensure(
      present.optional() == Some(41_u64),
      "a compatible wire value must reach the typed handler unchanged",
    )?;

    let rejection = ensure_some(
      deserialize_params::<u64>(Some(serde_json::json!("not-a-number"))).err(),
      "an incompatible wire value must return its decoder failure",
    )?;
    ensure_contains(
      &rejection,
      "invalid type",
      "parameter rejection must retain the serde type mismatch",
    )
  }

  #[test]
  fn request_invocation_distinguishes_success_rpc_parameter_and_serialization_outcomes() -> Result<(), TestFailure> {
    let success = block_on(invoke_request::<u64, _>(Ok(ready(Ok(73_u64)))));
    ensure(
      matches!(success, RequestOutcome::Success(value) if value == serde_json::json!(73)),
      "a successful typed handler result must become its JSON value",
    )?;

    let rpc_error = rpc::RpcError::invalid_request().with_details("fixture request rejected");
    let rejected = block_on(invoke_request::<u64, _>(Ok(ready(Err(rpc_error.clone())))));
    ensure(
      matches!(rejected, RequestOutcome::RpcError(error) if error == rpc_error),
      "a typed RPC handler failure must cross the erasure boundary unchanged",
    )?;

    let invalid_params: Result<Ready<Result<u64, rpc::RpcError>>, String> = Err(String::from("fixture params rejected"));
    let invalid = block_on(invoke_request(invalid_params));
    ensure(
      matches!(
        invalid,
        RequestOutcome::InvalidParams(detail) if detail == "fixture params rejected"
      ),
      "parameter decoding failure must remain distinct from handler execution",
    )?;

    let unserializable = block_on(invoke_request::<SerializationFailureFixture, _>(Ok(ready(Ok(
      SerializationFailureFixture,
    )))));
    ensure(
      matches!(
        unserializable,
        RequestOutcome::SerializationFailure(detail)
          if detail.contains("fixture value cannot be serialized")
      ),
      "successful values that cannot serialize must become an internal response failure",
    )
  }

  #[test]
  fn notification_invocation_runs_valid_handlers_propagates_failures_and_ignores_bad_params() -> Result<(), TestFailure> {
    ensure_ok(
      block_on(invoke_notification(Ok(ready(Ok(()))))),
      "a valid notification handler must complete",
    )?;

    let handler_failure = block_on(invoke_notification(Ok(ready(Err(ServerError::ExitBeforeShutdown)))));
    ensure(
      matches!(handler_failure, Err(ServerError::ExitBeforeShutdown)),
      "a notification handler failure must cross the erased boundary",
    )?;

    let invalid_params: Result<Ready<Result<(), ServerError>>, String> = Err(String::from("fixture notification params rejected"));
    ensure_ok(
      block_on(invoke_notification(invalid_params)),
      "invalid notification parameters must be ignored before handler execution",
    )
  }
}
