//! Local-future JSON-RPC/LSP server routing with typed request and notification registration.

pub mod rpc;
pub mod util;

use std::collections::HashMap;
use std::io;
use std::mem;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::task::Waker;

use async_trait::async_trait;
use futures::Future;
use futures::FutureExt;
use futures::SinkExt;
use futures::channel::oneshot;
use futures::future::FusedFuture;
use futures::future::LocalBoxFuture;
use futures::lock::Mutex as AsyncMutex;
use futures::sink::Sink;
use handler::ErasedNotificationHandler;
use handler::ErasedRequestHandler;
use handler::RequestOutcome;
use lsp_types::NumberOrString;
use lsp_types::notification::Notification;
use lsp_types::notification::{
  self,
};
use lsp_types::request as request_types;
use lsp_types::request::Request;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::Instrument;

mod handler;

#[cfg(any(feature = "tokio-stdio", feature = "tokio-tcp"))]
pub mod listen;

/// Shared cancellation state for one active inbound request.
#[derive(Debug, Clone, Default)]
struct Cancellation {
  /// Whether cancellation was requested.
  cancelled: Arc<AtomicBool>,
  /// Waker registered by the corresponding token.
  waker:     Arc<Mutex<Option<Waker>>>,
}

impl Cancellation {
  /// Construct a token observing this cancellation state.
  fn token(&self) -> CancelToken {
    CancelToken {
      cancelled: self.cancelled.clone(),
      waker_set: Arc::new(AtomicBool::new(false)),
      waker:     self.waker.clone(),
    }
  }

  /// Mark the task cancelled and wake its waiter if registered.
  fn cancel(&self) {
    self.cancelled.store(true, Ordering::SeqCst);
    if let Some(waker) = lock_recovering_poison(&self.waker).take() {
      waker.wake();
    }
  }
}

/// Acquire a mutex guard while preserving recoverable state after a poisoned lock.
fn lock_recovering_poison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
  mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Future that resolves when its inbound request is cancelled.
#[derive(Debug, Clone)]
pub struct CancelToken {
  /// Shared cancellation flag.
  cancelled: Arc<AtomicBool>,
  /// Whether this token has registered its waker.
  waker_set: Arc<AtomicBool>,
  /// Shared cancellation waker slot.
  waker:     Arc<Mutex<Option<Waker>>>,
}

impl CancelToken {
  /// Whether cancellation has been requested.
  #[must_use]
  pub fn is_cancelled(&self) -> bool {
    self.cancelled.load(Ordering::SeqCst)
  }

  /// Adapt cancellation into an RPC request-cancelled error.
  pub fn as_err(&mut self) -> CancelTokenErr<'_> {
    CancelTokenErr(self)
  }
}

impl Future for CancelToken {
  type Output = ();

  fn poll(self: std::pin::Pin<&mut Self>, context: &mut std::task::Context<'_>) -> Poll<Self::Output> {
    let this = self.as_ref().get_ref();
    if this.is_cancelled() {
      return Poll::Ready(());
    }

    let mut waker = lock_recovering_poison(&this.waker);
    if this.is_cancelled() {
      return Poll::Ready(());
    }
    *waker = Some(context.waker().clone());
    this.waker_set.store(true, Ordering::SeqCst);
    Poll::Pending
  }
}

impl FusedFuture for CancelToken {
  fn is_terminated(&self) -> bool {
    self.is_cancelled()
  }
}

/// Cancellation future adapted to the LSP request-cancelled error.
pub struct CancelTokenErr<'t>(&'t mut CancelToken);

impl Future for CancelTokenErr<'_> {
  type Output = Result<(), rpc::Error>;

  fn poll(self: std::pin::Pin<&mut Self>, context: &mut std::task::Context<'_>) -> Poll<Self::Output> {
    match self.get_mut().0.poll_unpin(context) {
      Poll::Ready(()) => Poll::Ready(Err(rpc::Error::request_cancelled())),
      Poll::Pending => Poll::Pending,
    }
  }
}

impl FusedFuture for CancelTokenErr<'_> {
  fn is_terminated(&self) -> bool {
    self.0.is_terminated()
  }
}

/// Public response-writing consumer contract.
#[async_trait(?Send)]
pub trait ResponseWriter: Sized {
  /// Write one typed response.
  async fn write_response<R: Serialize>(mut self, response: &rpc::Response<R>) -> Result<(), io::Error>;
}

/// Public outbound request/notification consumer contract.
#[async_trait(?Send)]
pub trait RequestWriter {
  /// Send an outbound request and wait for its typed response.
  async fn write_request<R: Request<Params = P>, P: Serialize + DeserializeOwned + core::fmt::Debug>(
    &mut self,
    params: Option<R::Params>,
  ) -> Result<rpc::Response<R::Result>, io::Error>;

  /// Send an outbound notification.
  async fn write_notification<N: Notification<Params = P>, P: Serialize + DeserializeOwned + core::fmt::Debug>(
    &mut self,
    params: Option<N::Params>,
  ) -> Result<(), io::Error>;

  /// Cancel the currently awaited outbound request, if any.
  async fn cancel(&mut self) -> Result<(), io::Error>;
}

/// Deferred local tasks owned by a single handler invocation.
type DeferredTasks = Rc<AsyncMutex<Vec<LocalBoxFuture<'static, ()>>>>;

/// Handler context carrying world state, cancellation, outbound writing, and deferred work.
#[derive(Clone)]
pub struct Context<W: Clone> {
  /// Mutable local server session state.
  session:         Rc<AsyncMutex<SessionState>>,
  /// Cancellation token for the current inbound request.
  cancel_token:    CancelToken,
  /// ID of an outbound request currently awaited by this context clone.
  last_request_id: Option<rpc::RequestId>,
  /// Local serialized access to the response transport.
  writer:          Rc<AsyncMutex<Box<dyn MessageWriter>>>,
  /// User world state.
  world:           W,
  /// Work deferred until handler completion and, for requests, successful response output.
  deferred:        DeferredTasks,
}

impl<W: Clone> std::ops::Deref for Context<W> {
  type Target = W;

  fn deref(&self) -> &Self::Target {
    &self.world
  }
}

impl<W: Clone> Context<W> {
  /// Whether initialization completed successfully.
  pub async fn is_initialized(&self) -> bool {
    self.session.lock().await.initialized
  }

  /// Whether a successful shutdown response has been sent.
  pub async fn is_shutting_down(&self) -> bool {
    self.session.lock().await.shutting_down
  }

  /// Borrow the user world state.
  #[must_use]
  pub fn world(&self) -> &W {
    &self.world
  }

  /// Mutably borrow this invocation's cancellation token.
  pub fn cancel_token(&mut self) -> &mut CancelToken {
    &mut self.cancel_token
  }

  /// Defer a local future until the handler completes.
  ///
  /// Request-deferred work runs only after its response is sent successfully. Notification
  /// work runs after its handler completes.
  pub async fn defer<F: Future<Output = ()> + 'static>(&self, future: F) {
    self.deferred.lock().await.push(future.boxed_local());
  }
}

#[async_trait(?Send)]
impl<W: Clone> RequestWriter for Context<W> {
  #[tracing::instrument(level = tracing::Level::TRACE, skip(self))]
  async fn write_request<R: Request<Params = P>, P: Serialize + DeserializeOwned + core::fmt::Debug>(
    &mut self,
    params: Option<R::Params>,
  ) -> Result<rpc::Response<R::Result>, io::Error> {
    let params = serialize_optional(params)?;
    let (id, receiver) = {
      let mut session = self.session.lock().await;
      let request_id = session.next_request_id;
      session.next_request_id = request_id
        .checked_add(1)
        .ok_or_else(|| io::Error::other("outbound request ID space exhausted"))?;
      let id = NumberOrString::Number(request_id);
      let (sender, receiver) = oneshot::channel();
      session.pending_requests.insert(id.clone(), sender);
      (id, receiver)
    };

    let message = request_message(R::METHOD, Some(id.clone()), params);
    self.last_request_id = Some(id.clone());
    let send_result = self
      .writer
      .lock()
      .await
      .send(message)
      .instrument(tracing::debug_span!("sending request", ?id))
      .await;
    if let Err(error) = send_result {
      self.session.lock().await.pending_requests.remove(&id);
      self.last_request_id = None;
      return Err(error);
    }

    let response = receiver.await;
    self.last_request_id = None;
    let response = response.map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "outbound request response channel was dropped"))?;
    tracing::trace!(response = ?response, "received response");

    let result = response
      .result
      .map(serde_json::from_value)
      .transpose()
      .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(rpc::Response {
      jsonrpc: response.jsonrpc,
      id: response.id,
      result,
      error: response.error,
    })
  }

  #[tracing::instrument(level = tracing::Level::TRACE, skip(self))]
  async fn write_notification<N: Notification<Params = P>, P: Serialize + DeserializeOwned + core::fmt::Debug>(
    &mut self,
    params: Option<N::Params>,
  ) -> Result<(), io::Error> {
    let message = request_message(N::METHOD, None, serialize_optional(params)?);
    self.writer.lock().await.send(message).await
  }

  async fn cancel(&mut self) -> Result<(), io::Error> {
    if let Some(id) = Option::take(&mut self.last_request_id) {
      self
        .write_notification::<notification::Cancel, _>(Some(lsp_types::CancelParams {
          id,
        }))
        .await
    } else {
      Ok(())
    }
  }
}

/// A transport sink suitable for local server response and outbound message writing.
pub trait MessageWriter: Sink<rpc::Message, Error = io::Error> + Unpin {}
impl<T: Sink<rpc::Message, Error = io::Error> + Unpin> MessageWriter for T {}

/// Mutable state for one local JSON-RPC session.
struct SessionState {
  /// Next numeric ID for an outbound request.
  next_request_id:  i32,
  /// Whether a successful initialize response has been sent.
  initialized:      bool,
  /// Whether an initialize request is currently active.
  initializing:     bool,
  /// Whether a successful shutdown response has been sent.
  shutting_down:    bool,
  /// Active inbound request cancellation states.
  active_tasks:     HashMap<rpc::RequestId, Cancellation>,
  /// Pending outbound requests awaiting client responses.
  pending_requests: HashMap<rpc::RequestId, oneshot::Sender<rpc::Response<serde_json::Value>>>,
}

impl SessionState {
  /// Start one inbound task unless its request ID is already active.
  fn start_task(&mut self, id: rpc::RequestId) -> Option<CancelToken> {
    if self.active_tasks.contains_key(&id) {
      return None;
    }
    let cancellation = Cancellation::default();
    let token = cancellation.token();
    self.active_tasks.insert(id, cancellation);
    Some(token)
  }

  /// Remove a normally completed task without cancelling its retained token clones.
  fn complete_task(&mut self, id: &rpc::RequestId) {
    self.active_tasks.remove(id);
    tracing::trace!(?id, "task completed");
  }

  /// Remove and cancel one active task.
  fn cancel_task(&mut self, id: &rpc::RequestId) {
    if let Some(cancellation) = self.active_tasks.remove(id) {
      cancellation.cancel();
      tracing::trace!(?id, "task cancelled");
    }
  }
}

/// Local JSON-RPC/LSP server with immutable kind-separated handler registries.
pub struct Server<W: Clone> {
  /// Mutable local session state.
  session:               Rc<AsyncMutex<SessionState>>,
  /// Typed request handlers keyed by protocol-owned method identifiers.
  request_handlers:      HashMap<&'static str, Rc<dyn ErasedRequestHandler<W>>>,
  /// Typed notification handlers keyed by protocol-owned method identifiers.
  notification_handlers: HashMap<&'static str, Rc<dyn ErasedNotificationHandler<W>>>,
}

/// Source-compatible builder name for the consuming registration surface.
pub type ServerBuilder<W> = Server<W>;

impl<W: Clone + 'static> Default for Server<W> {
  fn default() -> Self {
    Self::new()
  }
}

impl<W: Clone + 'static> Server<W> {
  /// Construct an unregistered server that also serves as its consuming builder.
  #[must_use]
  pub fn new() -> Self {
    Self {
      session:               Rc::new(AsyncMutex::new(SessionState {
        next_request_id:  0,
        initialized:      false,
        initializing:     false,
        shutting_down:    false,
        active_tasks:     HashMap::new(),
        pending_requests: HashMap::new(),
      })),
      request_handlers:      HashMap::new(),
      notification_handlers: HashMap::new(),
    }
  }

  /// Register a typed notification handler under `N::METHOD`.
  #[must_use]
  pub fn on_notification<N, F>(mut self, handler: fn(Context<W>, Params<N::Params>) -> F) -> Self
  where
    N: Notification + 'static,
    N::Params: DeserializeOwned + 'static,
    F: Future<Output = ()> + 'static,
  {
    self
      .notification_handlers
      .insert(N::METHOD, Rc::new(handler::NotificationHandler::<N, _, _>::new(handler)));
    tracing::info!(method = N::METHOD, "registered notification handler");
    self
  }

  /// Register a typed request handler under `R::METHOD`.
  #[must_use]
  pub fn on_request<R, F>(mut self, handler: fn(Context<W>, Params<R::Params>) -> F) -> Self
  where
    R: Request + 'static,
    R::Params: DeserializeOwned + 'static,
    R::Result: Serialize + 'static,
    F: Future<Output = Result<R::Result, rpc::Error>> + 'static,
  {
    self
      .request_handlers
      .insert(R::METHOD, Rc::new(handler::RequestHandler::<R, _, _>::new(handler)));
    tracing::info!(method = R::METHOD, "registered request handler");
    self
  }

  /// Finish registration. The server is already fully constructed, so this is identity.
  #[must_use]
  pub fn build(self) -> Self {
    self
  }

  /// Classify and handle one JSON-RPC wire message.
  // `+ use<W, T>` ensures the returned future owns all captured local state and not `&self`.
  pub fn handle_message<T: MessageWriter + Clone + 'static>(
    &self,
    world: W,
    message: rpc::Message,
    writer: T,
  ) -> impl Future<Output = Result<(), io::Error>> + use<W, T> {
    let session = self.session.clone();
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
      match (message.method, message.id) {
        (Some(method), Some(id)) => {
          let request = InboundRequest {
            jsonrpc: message.jsonrpc,
            method,
            id,
            params: message.params,
          };
          Self::handle_request(session, world, request, request_handler, writer).await
        }
        (Some(method), None) => {
          let notification = InboundNotification {
            jsonrpc: message.jsonrpc,
            method,
            params: message.params,
          };
          Self::handle_notification(session, world, notification, notification_handler, writer).await
        }
        (None, Some(id)) => {
          if message.jsonrpc != "2.0" {
            tracing::warn!(?id, "ignoring response with unsupported JSON-RPC version");
            return Ok(());
          }
          Self::handle_response(session, rpc::Response {
            jsonrpc: message.jsonrpc,
            id,
            result: message.result,
            error: message.error,
          })
          .await;
          Ok(())
        }
        (None, None) => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "JSON-RPC message has neither method nor ID",
        )),
      }
    }
  }

  /// Whether a successful shutdown response has been sent.
  pub async fn is_shutting_down(&self) -> bool {
    self.session.lock().await.shutting_down
  }

  /// Deliver one client response to a pending outbound request.
  async fn handle_response(session: Rc<AsyncMutex<SessionState>>, response: rpc::Response<serde_json::Value>) {
    let sender = session.lock().await.pending_requests.remove(&response.id);
    match sender {
      Some(sender) => {
        if sender.send(response).is_err() {
          tracing::warn!("outbound response receiver was dropped");
        }
      }
      None => tracing::warn!(?response, "ignoring response with unknown request ID"),
    }
  }

  /// Route one concrete-ID request through lifecycle gates and its request-only registry.
  async fn handle_request<T: MessageWriter + Clone + 'static>(
    session: Rc<AsyncMutex<SessionState>>,
    world: W,
    request: InboundRequest,
    handler: Option<Rc<dyn ErasedRequestHandler<W>>>,
    mut writer: T,
  ) -> Result<(), io::Error> {
    if request.jsonrpc != "2.0" {
      tracing::warn!(id = ?request.id, "request uses unsupported JSON-RPC version");
      return writer
        .send(error_message(
          request.id,
          error_with_text(rpc::Error::invalid_request(), "only JSON-RPC version 2.0 is accepted"),
        ))
        .await;
    }

    let is_initialize = request.method == request_types::Initialize::METHOD;
    let is_shutdown = request.method == request_types::Shutdown::METHOD;
    {
      let mut state = session.lock().await;
      let lifecycle_error = if state.shutting_down {
        Some(error_with_text(rpc::Error::invalid_request(), "server is shutting down"))
      } else if is_initialize && (state.initialized || state.initializing) {
        Some(error_with_text(
          rpc::Error::invalid_request(),
          "server is already initialized or initializing",
        ))
      } else if !state.initialized && !is_initialize {
        Some(rpc::Error::server_not_initialized())
      } else {
        None
      };
      if let Some(error) = lifecycle_error {
        drop(state);
        return writer.send(error_message(request.id, error)).await;
      }
      if is_initialize {
        state.initializing = true;
      }
    }

    let cancel_token = {
      let mut state = session.lock().await;
      let token = state.start_task(request.id.clone());
      if token.is_none() && is_initialize {
        state.initializing = false;
      }
      token
    };
    let Some(cancel_token) = cancel_token else {
      return writer
        .send(error_message(
          request.id,
          error_with_text(rpc::Error::invalid_request(), "request ID is already active"),
        ))
        .await;
    };

    let context = Context {
      session: session.clone(),
      cancel_token,
      last_request_id: None,
      writer: Rc::new(AsyncMutex::new(Box::new(writer.clone()))),
      world,
      deferred: Default::default(),
    };
    let outcome = match (handler, is_shutdown) {
      (Some(handler), _) => {
        handler
          .handle(context.clone(), request.params)
          .instrument(tracing::trace_span!(
              "request handler",
              method = %request.method
          ))
          .await
      }
      (None, true) => RequestOutcome::Success(serde_json::Value::Null),
      (None, false) => RequestOutcome::RpcError(rpc::Error::method_not_found()),
    };
    let handler_succeeded = matches!(outcome, RequestOutcome::Success(_));
    let response = outcome_message(request.id.clone(), outcome);
    if let Err(error) = writer.send(response).await {
      let mut state = session.lock().await;
      state.complete_task(&request.id);
      if is_initialize {
        state.initializing = false;
      }
      return Err(error);
    }

    {
      let mut state = session.lock().await;
      if is_initialize {
        state.initializing = false;
        if handler_succeeded {
          state.initialized = true;
        }
      }
      if is_shutdown && handler_succeeded {
        state.shutting_down = true;
      }
    }
    run_deferred(&context, &request.method).await;
    session.lock().await.complete_task(&request.id);
    Ok(())
  }

  /// Route one notification through cancellation or its notification-only registry.
  async fn handle_notification<T: MessageWriter + Clone + 'static>(
    session: Rc<AsyncMutex<SessionState>>,
    world: W,
    notification: InboundNotification,
    handler: Option<Rc<dyn ErasedNotificationHandler<W>>>,
    writer: T,
  ) -> Result<(), io::Error> {
    if notification.jsonrpc != "2.0" {
      tracing::warn!(
          method = %notification.method,
          "ignoring notification with unsupported JSON-RPC version"
      );
      return Ok(());
    }

    if notification.method == notification::Cancel::METHOD {
      if let Some(params) = notification.params
        && let Ok(cancel) = serde_json::from_value::<lsp_types::CancelParams>(params)
      {
        session.lock().await.cancel_task(&cancel.id);
      }
      return Ok(());
    }

    let Some(handler) = handler else {
      tracing::warn!(
          method = %notification.method,
          "no notification handler registered"
      );
      return Ok(());
    };
    let context = Context {
      session,
      cancel_token: Cancellation::default().token(),
      last_request_id: None,
      writer: Rc::new(AsyncMutex::new(Box::new(writer))),
      world,
      deferred: Default::default(),
    };
    handler
      .handle(context.clone(), notification.params)
      .instrument(tracing::trace_span!(
          "notification handler",
          method = %notification.method
      ))
      .await;
    run_deferred(&context, &notification.method).await;
    Ok(())
  }
}

/// Concrete request data established at wire classification.
struct InboundRequest {
  /// JSON-RPC version.
  jsonrpc: String,
  /// Method identifier.
  method:  String,
  /// Concrete request ID.
  id:      rpc::RequestId,
  /// Raw optional parameters.
  params:  Option<serde_json::Value>,
}

/// Concrete notification data established at wire classification.
struct InboundNotification {
  /// JSON-RPC version.
  jsonrpc: String,
  /// Method identifier.
  method:  String,
  /// Raw optional parameters.
  params:  Option<serde_json::Value>,
}

/// Run and drain deferred work for one completed invocation.
async fn run_deferred<W: Clone>(context: &Context<W>, method: &str) {
  let deferred = mem::take(&mut *context.deferred.lock().await);
  for task in deferred {
    task.instrument(tracing::trace_span!("deferred task", %method)).await;
  }
}

/// Serialize optional typed parameters into JSON without panicking.
fn serialize_optional<T: Serialize>(value: Option<T>) -> Result<Option<serde_json::Value>, io::Error> {
  value
    .map(serde_json::to_value)
    .transpose()
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Build a request-or-notification wire message from already serialized parameters.
fn request_message(method: &str, id: Option<rpc::RequestId>, params: Option<serde_json::Value>) -> rpc::Message {
  rpc::Message {
    jsonrpc: "2.0".into(),
    method: Some(method.into()),
    id,
    params,
    result: None,
    error: None,
  }
}

/// Attach text detail to an RPC error without fallible serialization.
fn error_with_text(mut error: rpc::Error, detail: impl Into<String>) -> rpc::Error {
  error.data = Some(serde_json::Value::String(detail.into()));
  error
}

/// Build an error response for one concrete request ID.
fn error_message(id: rpc::RequestId, error: rpc::Error) -> rpc::Message {
  rpc::Message {
    jsonrpc: "2.0".into(),
    method:  None,
    id:      Some(id),
    params:  None,
    result:  None,
    error:   Some(error),
  }
}

/// Convert an erased handler outcome into a same-ID response message.
fn outcome_message(id: rpc::RequestId, outcome: RequestOutcome) -> rpc::Message {
  match outcome {
    RequestOutcome::Success(result) => rpc::Message {
      jsonrpc: "2.0".into(),
      method:  None,
      id:      Some(id),
      params:  None,
      result:  Some(result),
      error:   None,
    },
    RequestOutcome::RpcError(error) => error_message(id, error),
    RequestOutcome::InvalidParams(detail) => error_message(id, error_with_text(rpc::Error::invalid_params(), detail)),
    RequestOutcome::SerializationFailure(detail) => error_message(id, error_with_text(rpc::Error::internal_error(), detail)),
  }
}

/// Wrapper around optional typed handler parameters.
pub struct Params<P>(Option<P>);

impl<P> Params<P> {
  /// Return optional parameters unchanged.
  #[must_use]
  pub fn optional(self) -> Option<P> {
    self.0
  }

  /// Require parameters or return the standard invalid-params error.
  pub fn required(self) -> Result<P, rpc::Error> {
    self
      .0
      .ok_or_else(|| error_with_text(rpc::Error::invalid_params(), "params are required"))
  }
}

impl<P> From<Option<P>> for Params<P> {
  fn from(params: Option<P>) -> Self {
    Self(params)
  }
}

#[cfg(test)]
mod tests {
  use std::cell::Cell;
  use std::io;
  use std::pin::Pin;
  use std::rc::Rc;
  use std::sync::Arc;
  use std::sync::Mutex;
  use std::task::Context as TaskContext;
  use std::task::Poll;

  use futures::Sink;
  use futures::channel::oneshot;
  use futures::executor::LocalPool;
  use futures::future::FusedFuture;
  use futures::task::LocalSpawnExt;
  use lsp_types::NumberOrString;
  use lsp_types::notification::Notification;
  use lsp_types::notification::{
    self,
  };
  use lsp_types::request::Request;
  use serde::Deserialize;
  use serde::Deserializer;
  use serde::Serialize;
  use serde::Serializer;
  use serde::ser::Error as _;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;

  use super::CancelToken;
  use super::Cancellation;
  use super::Context;
  use super::MessageWriter;
  use super::Params;
  use super::RequestWriter;
  use super::Server;
  use super::ServerBuilder;
  use super::lock_recovering_poison;
  use super::request_message;
  use crate::rpc;

  /// Mutable state behind the controllable test transport.
  #[derive(Default)]
  struct SinkState {
    /// Successfully sent wire messages.
    messages: Vec<rpc::Message>,
    /// Whether subsequent sends should fail.
    fail:     bool,
  }

  /// Cloneable local sink with observable messages and injected write failure.
  #[derive(Clone, Default)]
  struct TestSink {
    /// Shared transport state.
    state: Arc<Mutex<SinkState>>,
  }

  impl TestSink {
    /// Return all successfully sent messages.
    fn messages(&self) -> Vec<rpc::Message> {
      lock_recovering_poison(&self.state).messages.clone()
    }

    /// Remove recorded messages.
    fn clear(&self) {
      lock_recovering_poison(&self.state).messages.clear();
    }

    /// Enable or disable injected send failure.
    fn set_failure(&self, fail: bool) {
      lock_recovering_poison(&self.state).fail = fail;
    }
  }

  impl Sink<rpc::Message> for TestSink {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, _context: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
      Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, message: rpc::Message) -> Result<(), Self::Error> {
      let mut state = lock_recovering_poison(&self.state);
      if state.fail {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "injected writer failure"))
      } else {
        state.messages.push(message);
        Ok(())
      }
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
      Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _context: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
      Poll::Ready(Ok(()))
    }
  }

  /// Serializable request/notification fixture parameters.
  #[derive(Clone, Debug, Deserialize, Serialize)]
  struct TestParams {
    /// Observable input value.
    value: u32,
  }

  /// Serializable successful request result.
  #[derive(Clone, Debug, Deserialize, Serialize)]
  struct TestResult {
    /// Echoed output value.
    value: u32,
  }

  /// Result whose serializer deliberately reports a typed serialization error.
  #[derive(Clone, Debug)]
  struct FailingResult;

  impl Serialize for FailingResult {
    fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
    where
      S: Serializer,
    {
      Err(S::Error::custom("injected result serialization failure"))
    }
  }

  impl<'de> Deserialize<'de> for FailingResult {
    fn deserialize<D>(_deserializer: D) -> Result<Self, D::Error>
    where
      D: Deserializer<'de>,
    {
      Ok(Self)
    }
  }

  /// Initialize protocol fixture.
  struct InitializeRequest;
  impl Request for InitializeRequest {
    type Params = TestParams;
    type Result = TestResult;
    const METHOD: &'static str = "initialize";
  }

  /// Ordinary request fixture.
  struct BasicRequest;
  impl Request for BasicRequest {
    type Params = TestParams;
    type Result = TestResult;
    const METHOD: &'static str = "test/basic";
  }

  /// Request fixture sharing its method with a notification fixture.
  struct SharedRequest;
  impl Request for SharedRequest {
    type Params = TestParams;
    type Result = TestResult;
    const METHOD: &'static str = "test/shared";
  }

  /// Request fixture whose handler returns an RPC error.
  struct ErrorRequest;
  impl Request for ErrorRequest {
    type Params = TestParams;
    type Result = TestResult;
    const METHOD: &'static str = "test/error";
  }

  /// Request fixture whose successful result cannot be serialized.
  struct SerializationRequest;
  impl Request for SerializationRequest {
    type Params = TestParams;
    type Result = FailingResult;
    const METHOD: &'static str = "test/serialization";
  }

  /// Long-running cancellable request fixture.
  struct CancellableRequest;
  impl Request for CancellableRequest {
    type Params = TestParams;
    type Result = TestResult;
    const METHOD: &'static str = "test/cancellable";
  }

  /// Shutdown protocol fixture.
  struct ShutdownRequest;
  impl Request for ShutdownRequest {
    type Params = ();
    type Result = ();
    const METHOD: &'static str = "shutdown";
  }

  /// Shared-method notification fixture.
  struct SharedNotification;
  impl Notification for SharedNotification {
    type Params = TestParams;
    const METHOD: &'static str = "test/shared";
  }

  /// Notification-only fixture.
  struct NoticeOnly;
  impl Notification for NoticeOnly {
    type Params = TestParams;
    const METHOD: &'static str = "test/notice-only";
  }

  /// Request-only method represented as a notification input in polarity tests.
  struct BasicNotification;
  impl Notification for BasicNotification {
    type Params = TestParams;
    const METHOD: &'static str = "test/basic";
  }

  /// Cloneable observable world used by handlers.
  #[derive(Clone, Default)]
  struct TestWorld {
    /// Request handler invocation count.
    requests:      Rc<Cell<usize>>,
    /// Notification handler invocation count.
    notifications: Rc<Cell<usize>>,
    /// Deferred task invocation count.
    deferred:      Rc<Cell<usize>>,
    /// Tokens retained for cancellation assertions.
    tokens:        Arc<Mutex<Vec<CancelToken>>>,
  }

  impl TestWorld {
    /// Increment the request count without overflow.
    fn record_request(&self) {
      self.requests.set(self.requests.get().saturating_add(1));
    }

    /// Increment the notification count without overflow.
    fn record_notification(&self) {
      self.notifications.set(self.notifications.get().saturating_add(1));
    }
  }

  async fn initialize_handler(context: Context<TestWorld>, params: Params<TestParams>) -> Result<TestResult, rpc::Error> {
    let params = params.required()?;
    context.record_request();
    Ok(TestResult {
      value: params.value
    })
  }

  async fn basic_handler(context: Context<TestWorld>, params: Params<TestParams>) -> Result<TestResult, rpc::Error> {
    let params = params.required()?;
    context.record_request();
    Ok(TestResult {
      value: params.value
    })
  }

  async fn error_handler(context: Context<TestWorld>, params: Params<TestParams>) -> Result<TestResult, rpc::Error> {
    let _ = params.required()?;
    context.record_request();
    Err(rpc::Error::content_modified())
  }

  async fn serialization_handler(context: Context<TestWorld>, params: Params<TestParams>) -> Result<FailingResult, rpc::Error> {
    let _ = params.required()?;
    context.record_request();
    Ok(FailingResult)
  }

  async fn notification_handler(context: Context<TestWorld>, params: Params<TestParams>) {
    if params.required().is_ok() {
      context.record_notification();
    }
  }

  async fn deferred_handler(context: Context<TestWorld>, params: Params<TestParams>) -> Result<TestResult, rpc::Error> {
    let params = params.required()?;
    context.record_request();
    let deferred = context.world().deferred.clone();
    context
      .defer(async move {
        deferred.set(deferred.get().saturating_add(1));
      })
      .await;
    Ok(TestResult {
      value: params.value
    })
  }

  async fn deferred_notification(context: Context<TestWorld>, params: Params<TestParams>) {
    if params.required().is_err() {
      return;
    }
    context.record_notification();
    let deferred = context.world().deferred.clone();
    context
      .defer(async move {
        deferred.set(deferred.get().saturating_add(1));
      })
      .await;
  }

  async fn cancellable_handler(mut context: Context<TestWorld>, params: Params<TestParams>) -> Result<TestResult, rpc::Error> {
    let params = params.required()?;
    context.record_request();
    let token = context.cancel_token().clone();
    lock_recovering_poison(&context.world().tokens).push(token);
    context.cancel_token().await;
    Ok(TestResult {
      value: params.value
    })
  }

  async fn token_observing_handler(mut context: Context<TestWorld>, params: Params<TestParams>) -> Result<TestResult, rpc::Error> {
    let params = params.required()?;
    context.record_request();
    let token = context.cancel_token().clone();
    lock_recovering_poison(&context.world().tokens).push(token);
    Ok(TestResult {
      value: params.value
    })
  }

  async fn shutdown_handler(_context: Context<TestWorld>, _params: Params<()>) -> Result<(), rpc::Error> {
    Ok(())
  }

  async fn failing_initialize_handler(_context: Context<TestWorld>, _params: Params<TestParams>) -> Result<TestResult, rpc::Error> {
    Err(rpc::Error::internal_error())
  }

  async fn failing_shutdown_handler(_context: Context<TestWorld>, _params: Params<()>) -> Result<(), rpc::Error> {
    Err(rpc::Error::internal_error())
  }

  fn params(value: u32) -> Option<serde_json::Value> {
    Some(serde_json::json!({ "value": value }))
  }

  fn id(value: i32) -> rpc::RequestId {
    NumberOrString::Number(value)
  }

  fn response_error_code(message: &rpc::Message) -> Option<i32> {
    message.error.as_ref().map(|error| error.code)
  }

  fn last_message(sink: &TestSink) -> Result<rpc::Message, TestFailure> {
    ensure_some(sink.messages().last().cloned(), "the server must emit a response message")
  }

  fn initialize(server: &Server<TestWorld>, world: &TestWorld, sink: &TestSink) -> Result<(), TestFailure> {
    ensure_ok(
      futures::executor::block_on(server.handle_message(
        world.clone(),
        request_message("initialize", Some(id(1)), params(1)),
        sink.clone(),
      )),
      "initialization must dispatch",
    )?;
    ensure(
      futures::executor::block_on(async { server.session.lock().await.initialized }),
      "successful initialization must transition the session",
    )?;
    sink.clear();
    Ok(())
  }

  #[test]
  fn builder_preserves_kind_separated_registration_and_dispatch() -> Result<(), TestFailure> {
    let server: ServerBuilder<TestWorld> = Server::new()
      .on_request::<InitializeRequest, _>(initialize_handler)
      .on_request::<SharedRequest, _>(basic_handler)
      .on_notification::<SharedNotification, _>(notification_handler)
      .on_notification::<NoticeOnly, _>(notification_handler)
      .on_request::<BasicRequest, _>(basic_handler);
    let server = server.build();
    let world = TestWorld::default();
    let sink = TestSink::default();
    initialize(&server, &world, &sink)?;

    ensure_ok(
      futures::executor::block_on(server.handle_message(
        world.clone(),
        request_message("test/shared", Some(id(2)), params(7)),
        sink.clone(),
      )),
      "the shared request must dispatch",
    )?;
    ensure_eq(&world.requests.get(), &2, "initialize plus shared request count")?;
    ensure_eq(
      &world.notifications.get(),
      &0,
      "a request must not invoke the same-method notification",
    )?;
    let response = last_message(&sink)?;
    ensure(
      response.id == Some(id(2)) && response.result == Some(serde_json::json!({ "value": 7 })),
      "the request response must retain its ID and typed result",
    )?;

    sink.clear();
    ensure_ok(
      futures::executor::block_on(server.handle_message(world.clone(), request_message("test/shared", None, params(8)), sink.clone())),
      "the shared notification must dispatch",
    )?;
    ensure_eq(
      &world.notifications.get(),
      &1,
      "the notification must invoke only its notification handler",
    )?;
    ensure(sink.messages().is_empty(), "a valid notification must emit no response")?;

    ensure_ok(
      futures::executor::block_on(server.handle_message(
        world.clone(),
        request_message("test/notice-only", Some(id(3)), params(9)),
        sink.clone(),
      )),
      "a request to a notification-only method must be handled as an error",
    )?;
    ensure(
      response_error_code(&last_message(&sink)?) == Some(-32601),
      "notification-only request error code",
    )?;

    sink.clear();
    ensure_ok(
      futures::executor::block_on(server.handle_message(
        world.clone(),
        request_message(BasicNotification::METHOD, None, params(10)),
        sink.clone(),
      )),
      "a notification to a request-only method must be ignored",
    )?;
    ensure_eq(
      &world.requests.get(),
      &2,
      "the request-only handler must not be invoked by a notification",
    )?;
    ensure(sink.messages().is_empty(), "a request-only notification must emit no response")
  }

  #[test]
  fn request_outcomes_preserve_ids_and_do_not_invoke_invalid_handlers() -> Result<(), TestFailure> {
    let server = Server::new()
      .on_request::<InitializeRequest, _>(initialize_handler)
      .on_request::<BasicRequest, _>(basic_handler)
      .on_request::<ErrorRequest, _>(error_handler)
      .on_request::<SerializationRequest, _>(serialization_handler)
      .build();
    let world = TestWorld::default();
    let sink = TestSink::default();
    initialize(&server, &world, &sink)?;

    ensure_ok(
      futures::executor::block_on(server.handle_message(
        world.clone(),
        request_message("test/basic", Some(id(2)), Some(serde_json::json!("bad"))),
        sink.clone(),
      )),
      "invalid request parameters must produce a response",
    )?;
    let invalid = last_message(&sink)?;
    ensure(
      invalid.id == Some(id(2)) && response_error_code(&invalid) == Some(-32602),
      "invalid params must retain the concrete request ID",
    )?;
    ensure_eq(&world.requests.get(), &1, "invalid params must not invoke the typed handler")?;

    sink.clear();
    ensure_ok(
      futures::executor::block_on(server.handle_message(
        world.clone(),
        request_message("test/error", Some(id(3)), params(3)),
        sink.clone(),
      )),
      "a typed RPC error must produce a response",
    )?;
    let rpc_error = last_message(&sink)?;
    ensure(
      rpc_error.id == Some(id(3)) && response_error_code(&rpc_error) == Some(-32801),
      "typed handler errors must retain the same request ID",
    )?;

    sink.clear();
    ensure_ok(
      futures::executor::block_on(server.handle_message(
        world.clone(),
        request_message("test/serialization", Some(id(4)), params(4)),
        sink.clone(),
      )),
      "result serialization failure must produce an internal error",
    )?;
    let serialization = last_message(&sink)?;
    ensure(
      serialization.id == Some(id(4))
        && response_error_code(&serialization) == Some(-32603)
        && serialization
          .error
          .as_ref()
          .and_then(|error| error.data.as_ref())
          .is_some_and(|data| data.as_str() == Some("injected result serialization failure")),
      "result serialization failure must retain ID and actionable detail",
    )
  }

  #[test]
  fn lifecycle_transitions_only_after_successful_results_and_writes() -> Result<(), TestFailure> {
    let world = TestWorld::default();
    let sink = TestSink::default();
    let uninitialized = Server::new()
      .on_request::<InitializeRequest, _>(initialize_handler)
      .on_request::<BasicRequest, _>(basic_handler)
      .build();
    ensure_ok(
      futures::executor::block_on(uninitialized.handle_message(
        world.clone(),
        request_message("test/basic", Some(id(1)), params(1)),
        sink.clone(),
      )),
      "a pre-initialize request must receive a lifecycle error",
    )?;
    ensure(
      response_error_code(&last_message(&sink)?) == Some(-32002),
      "pre-initialize request error code",
    )?;

    sink.clear();
    ensure_ok(
      futures::executor::block_on(uninitialized.handle_message(
        world.clone(),
        request_message("shutdown", Some(id(2)), None),
        sink.clone(),
      )),
      "shutdown before initialization must receive a lifecycle error",
    )?;
    ensure(
      !futures::executor::block_on(uninitialized.is_shutting_down()),
      "shutdown before initialization must not transition state",
    )?;

    let failed_initialize = Server::new()
      .on_request::<InitializeRequest, _>(failing_initialize_handler)
      .build();
    sink.clear();
    ensure_ok(
      futures::executor::block_on(failed_initialize.handle_message(
        world.clone(),
        request_message("initialize", Some(id(3)), params(3)),
        sink.clone(),
      )),
      "a failed initialize handler must still emit its RPC error",
    )?;
    ensure(
      !futures::executor::block_on(async { failed_initialize.session.lock().await.initialized }),
      "an initialize handler error must not transition state",
    )?;

    let writer_failed_initialize = Server::new().on_request::<InitializeRequest, _>(initialize_handler).build();
    let failing_sink = TestSink::default();
    failing_sink.set_failure(true);
    let writer_result = futures::executor::block_on(writer_failed_initialize.handle_message(
      world.clone(),
      request_message("initialize", Some(id(4)), params(4)),
      failing_sink,
    ));
    ensure(writer_result.is_err(), "initialize response writer failure must propagate")?;
    ensure(
      !futures::executor::block_on(async { writer_failed_initialize.session.lock().await.initialized }),
      "initialize writer failure must not transition state",
    )?;

    let server = Server::new()
      .on_request::<InitializeRequest, _>(initialize_handler)
      .on_request::<ShutdownRequest, _>(shutdown_handler)
      .on_request::<BasicRequest, _>(basic_handler)
      .build();
    let live_sink = TestSink::default();
    initialize(&server, &world, &live_sink)?;
    ensure_ok(
      futures::executor::block_on(server.handle_message(
        world.clone(),
        request_message("initialize", Some(id(5)), params(5)),
        live_sink.clone(),
      )),
      "duplicate initialize must receive an invalid-request response",
    )?;
    ensure(
      response_error_code(&last_message(&live_sink)?) == Some(-32600),
      "duplicate initialize error code",
    )?;

    live_sink.clear();
    ensure_ok(
      futures::executor::block_on(server.handle_message(world.clone(), request_message("shutdown", Some(id(6)), None), live_sink.clone())),
      "successful shutdown must dispatch",
    )?;
    ensure(
      futures::executor::block_on(server.is_shutting_down()),
      "successful shutdown response must transition state",
    )?;
    ensure_ok(
      futures::executor::block_on(server.handle_message(world, request_message("test/basic", Some(id(7)), params(7)), live_sink.clone())),
      "a post-shutdown request must receive an invalid-request response",
    )?;
    ensure(
      response_error_code(&last_message(&live_sink)?) == Some(-32600),
      "post-shutdown request error code",
    )
  }

  #[test]
  fn failed_shutdown_and_writer_failure_do_not_run_request_deferred_work() -> Result<(), TestFailure> {
    let world = TestWorld::default();
    let sink = TestSink::default();
    let failed_shutdown = Server::new()
      .on_request::<InitializeRequest, _>(initialize_handler)
      .on_request::<ShutdownRequest, _>(failing_shutdown_handler)
      .build();
    initialize(&failed_shutdown, &world, &sink)?;
    ensure_ok(
      futures::executor::block_on(failed_shutdown.handle_message(
        world.clone(),
        request_message("shutdown", Some(id(2)), None),
        sink.clone(),
      )),
      "a failed shutdown handler must emit its error",
    )?;
    ensure(
      !futures::executor::block_on(failed_shutdown.is_shutting_down()),
      "a shutdown handler error must not transition state",
    )?;

    let deferred_server = Server::new()
      .on_request::<InitializeRequest, _>(initialize_handler)
      .on_request::<BasicRequest, _>(deferred_handler)
      .on_notification::<SharedNotification, _>(deferred_notification)
      .build();
    let deferred_sink = TestSink::default();
    initialize(&deferred_server, &world, &deferred_sink)?;
    deferred_sink.set_failure(true);
    let failed_write = futures::executor::block_on(deferred_server.handle_message(
      world.clone(),
      request_message("test/basic", Some(id(3)), params(3)),
      deferred_sink.clone(),
    ));
    ensure(failed_write.is_err(), "request response writer failure must propagate")?;
    ensure_eq(&world.deferred.get(), &0, "request-deferred work must not run after a failed send")?;

    deferred_sink.set_failure(false);
    ensure_ok(
      futures::executor::block_on(deferred_server.handle_message(
        world.clone(),
        request_message("test/basic", Some(id(4)), params(4)),
        deferred_sink.clone(),
      )),
      "request deferred work must run after successful output",
    )?;
    ensure_eq(
      &world.deferred.get(),
      &1,
      "request-deferred work must run once after response output",
    )?;
    ensure_ok(
      futures::executor::block_on(deferred_server.handle_message(
        world.clone(),
        request_message("test/shared", None, params(5)),
        deferred_sink,
      )),
      "notification deferred work must run after handler completion",
    )?;
    ensure_eq(&world.deferred.get(), &2, "notification-deferred work must run after its handler")
  }

  #[test]
  fn duplicate_ids_and_cancellation_affect_only_the_matching_active_task() -> Result<(), TestFailure> {
    let server = Server::new()
      .on_request::<InitializeRequest, _>(initialize_handler)
      .on_request::<CancellableRequest, _>(cancellable_handler)
      .build();
    let world = TestWorld::default();
    let sink = TestSink::default();
    initialize(&server, &world, &sink)?;

    let mut pool = LocalPool::new();
    let first = server.handle_message(
      world.clone(),
      request_message("test/cancellable", Some(id(9)), params(9)),
      sink.clone(),
    );
    ensure_ok(
      pool.spawner().spawn_local(async move {
        let _ = first.await;
      }),
      "the first cancellable request must spawn locally",
    )?;
    pool.run_until_stalled();
    ensure_eq(&world.requests.get(), &2, "initialize plus one active cancellable handler must run")?;
    let token = ensure_some(
      lock_recovering_poison(&world.tokens).first().cloned(),
      "the active handler must expose its cancellation token",
    )?;

    ensure_ok(
      pool.run_until(server.handle_message(
        world.clone(),
        request_message("test/cancellable", Some(id(9)), params(10)),
        sink.clone(),
      )),
      "a duplicate active request ID must receive an error response",
    )?;
    ensure_eq(&world.requests.get(), &2, "a duplicate active ID must not invoke a second handler")?;
    ensure(
      response_error_code(&last_message(&sink)?) == Some(-32600),
      "duplicate active request ID error code",
    )?;

    ensure_ok(
      pool.run_until(server.handle_message(
        world.clone(),
        request_message(notification::Cancel::METHOD, None, None),
        sink.clone(),
      )),
      "missing cancellation params must be ignored",
    )?;
    ensure_ok(
      pool.run_until(server.handle_message(
        world.clone(),
        request_message(
          notification::Cancel::METHOD,
          None,
          Some(serde_json::json!({ "id": { "bad": true } })),
        ),
        sink.clone(),
      )),
      "malformed cancellation params must be ignored",
    )?;
    ensure_ok(
      pool.run_until(server.handle_message(
        world.clone(),
        request_message(notification::Cancel::METHOD, None, Some(serde_json::json!({ "id": 99 }))),
        sink.clone(),
      )),
      "unknown cancellation IDs must be ignored",
    )?;
    ensure(
      !token.is_cancelled(),
      "missing, malformed, and unknown cancellations must leave the active task pending",
    )?;

    ensure_ok(
      pool.run_until(server.handle_message(
        world,
        request_message(notification::Cancel::METHOD, None, Some(serde_json::json!({ "id": 9 }))),
        sink,
      )),
      "valid cancellation must dispatch",
    )?;
    pool.run_until_stalled();
    ensure(
      token.is_cancelled() && token.is_terminated(),
      "valid cancellation must terminate only the matching token",
    )
  }

  #[test]
  fn normal_completion_removes_task_without_cancelling_retained_token() -> Result<(), TestFailure> {
    let server = Server::new()
      .on_request::<InitializeRequest, _>(initialize_handler)
      .on_request::<BasicRequest, _>(token_observing_handler)
      .build();
    let world = TestWorld::default();
    let sink = TestSink::default();
    initialize(&server, &world, &sink)?;
    ensure_ok(
      futures::executor::block_on(server.handle_message(world.clone(), request_message("test/basic", Some(id(2)), params(2)), sink)),
      "the token-observing request must complete",
    )?;
    let token = ensure_some(
      lock_recovering_poison(&world.tokens).first().cloned(),
      "the completed handler must expose its retained token",
    )?;
    ensure(
      !token.is_cancelled() && !token.is_terminated(),
      "normal completion must remove task ownership without cancelling the token",
    )?;
    ensure(
      futures::executor::block_on(async { server.session.lock().await.active_tasks.is_empty() }),
      "normal completion must remove the active task entry",
    )
  }

  #[test]
  fn outbound_request_failures_are_fallible_and_do_not_wrap_ids() -> Result<(), TestFailure> {
    let server = Server::<TestWorld>::new().build();
    let sink = TestSink::default();
    let world = TestWorld::default();
    let mut context = Context {
      session:         server.session.clone(),
      cancel_token:    Cancellation::default().token(),
      last_request_id: None,
      writer:          Rc::new(futures::lock::Mutex::new(Box::new(sink.clone()) as Box<dyn MessageWriter>)),
      world:           world.clone(),
      deferred:        Default::default(),
    };
    futures::executor::block_on(async {
      context.session.lock().await.next_request_id = i32::MAX;
    });
    let exhausted = futures::executor::block_on(context.write_request::<BasicRequest, _>(Some(TestParams {
      value: 1
    })));
    let exhausted_error = ensure_some(exhausted.err(), "ID exhaustion must return an error")?;
    ensure(exhausted_error.kind() == io::ErrorKind::Other, "ID exhaustion error kind")?;
    ensure(sink.messages().is_empty(), "ID exhaustion must not emit a wrapped request ID")?;

    futures::executor::block_on(async {
      context.session.lock().await.next_request_id = 0;
    });
    let mut dropped_context = context.clone();
    let (result_sender, result_receiver) = oneshot::channel();
    let mut pool = LocalPool::new();
    ensure_ok(
      pool.spawner().spawn_local(async move {
        let result = dropped_context
          .write_request::<BasicRequest, _>(Some(TestParams {
            value: 2
          }))
          .await;
        let _ = result_sender.send(result);
      }),
      "the outbound request must spawn locally",
    )?;
    pool.run_until_stalled();
    pool.run_until(async {
      context.session.lock().await.pending_requests.clear();
    });
    let dropped = ensure_ok(pool.run_until(result_receiver), "the outbound result channel must remain connected")?;
    let dropped_error = ensure_some(dropped.err(), "a dropped response receiver must fail")?;
    ensure(
      dropped_error.kind() == io::ErrorKind::BrokenPipe,
      "dropped response channel error kind",
    )?;

    let mut malformed_context = context;
    let (malformed_sender, malformed_receiver) = oneshot::channel();
    ensure_ok(
      pool.spawner().spawn_local(async move {
        let result = malformed_context
          .write_request::<BasicRequest, _>(Some(TestParams {
            value: 3
          }))
          .await;
        let _ = malformed_sender.send(result);
      }),
      "the malformed-response request must spawn locally",
    )?;
    pool.run_until_stalled();
    let outbound = last_message(&sink)?;
    let outbound_id = ensure_some(outbound.id, "the outbound request must carry an ID")?;
    pool.run_until(super::Server::<TestWorld>::handle_response(server.session.clone(), rpc::Response {
      jsonrpc: "2.0".into(),
      id:      outbound_id,
      result:  Some(serde_json::json!("not a TestResult")),
      error:   None,
    }));
    let malformed = ensure_ok(
      pool.run_until(malformed_receiver),
      "the malformed response result channel must remain connected",
    )?;
    let malformed_error = ensure_some(malformed.err(), "malformed response deserialization must return an error")?;
    ensure(
      malformed_error.kind() == io::ErrorKind::InvalidData,
      "malformed response error kind",
    )
  }

  #[test]
  fn malformed_wire_data_and_empty_response_results_are_typed_errors() -> Result<(), TestFailure> {
    let server = Server::<TestWorld>::new().build();
    let invalid = rpc::Message {
      jsonrpc: "2.0".into(),
      method:  None,
      id:      None,
      params:  None,
      result:  None,
      error:   None,
    };
    let wire_error = futures::executor::block_on(server.handle_message(TestWorld::default(), invalid, TestSink::default()));
    let wire_error = ensure_some(wire_error.err(), "invalid wire data must return an error")?;
    ensure(wire_error.kind() == io::ErrorKind::InvalidData, "invalid wire data error kind")?;

    let response = rpc::Response::<TestResult> {
      jsonrpc: "2.0".into(),
      id:      id(1),
      result:  None,
      error:   None,
    };
    let response_error = ensure_some(response.into_result().err(), "a response without result or error must be rejected")?;
    ensure_eq(&response_error.code, &-32603, "empty response internal-error code")
  }
}
