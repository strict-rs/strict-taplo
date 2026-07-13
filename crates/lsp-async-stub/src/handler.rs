//! Typed request and notification handlers erased behind kind-specific local-future traits.

use std::marker::PhantomData;

use futures::Future;
use futures::FutureExt;
use futures::future::LocalBoxFuture;
use lsp_types::notification::Notification;
use lsp_types::request::Request;
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::Context;
use super::Params;
use crate::rpc;

/// Result of invoking an erased request handler before its JSON-RPC response is written.
pub(crate) enum RequestOutcome {
  /// Typed handler success serialized to JSON.
  Success(serde_json::Value),
  /// Typed handler failure already represented as an RPC error.
  RpcError(rpc::Error),
  /// Request parameters could not be deserialized.
  InvalidParams(String),
  /// A successful typed result could not be serialized.
  SerializationFailure(String),
}

/// Kind-safe erased request handler.
pub(crate) trait ErasedRequestHandler<W: Clone> {
  /// Deserialize parameters, invoke the typed handler, and serialize its result locally.
  fn handle(&self, context: Context<W>, params: Option<serde_json::Value>) -> LocalBoxFuture<'static, RequestOutcome>;
}

/// Kind-safe erased notification handler.
pub(crate) trait ErasedNotificationHandler<W: Clone> {
  /// Deserialize parameters and invoke the typed notification handler locally.
  fn handle(&self, context: Context<W>, params: Option<serde_json::Value>) -> LocalBoxFuture<'static, ()>;
}

/// Typed request handler adapter.
pub(crate) struct RequestHandler<R, F, W>
where
  R: Request,
  F: Future<Output = Result<R::Result, rpc::Error>>,
  W: Clone,
{
  /// Registered typed handler function.
  function: fn(Context<W>, Params<R::Params>) -> F,
  /// Adapter type ownership without storing values.
  marker:   PhantomData<fn() -> R>,
}

impl<R, F, W> RequestHandler<R, F, W>
where
  R: Request,
  F: Future<Output = Result<R::Result, rpc::Error>>,
  W: Clone,
{
  /// Construct a typed request handler adapter.
  pub(crate) fn new(function: fn(Context<W>, Params<R::Params>) -> F) -> Self {
    Self {
      function,
      marker: PhantomData,
    }
  }
}

impl<R, F, W> ErasedRequestHandler<W> for RequestHandler<R, F, W>
where
  R: Request + 'static,
  R::Params: DeserializeOwned + 'static,
  R::Result: Serialize + 'static,
  F: Future<Output = Result<R::Result, rpc::Error>> + 'static,
  W: Clone + 'static,
{
  fn handle(&self, context: Context<W>, params: Option<serde_json::Value>) -> LocalBoxFuture<'static, RequestOutcome> {
    let function = self.function;
    async move {
      let params = match params.map(serde_json::from_value).transpose() {
        Ok(params) => params,
        Err(error) => return RequestOutcome::InvalidParams(error.to_string()),
      };

      match function(context, Params::from(params)).await {
        Ok(result) => match serde_json::to_value(result) {
          Ok(result) => RequestOutcome::Success(result),
          Err(error) => RequestOutcome::SerializationFailure(error.to_string()),
        },
        Err(error) => RequestOutcome::RpcError(error),
      }
    }
    .boxed_local()
  }
}

/// Typed notification handler adapter.
pub(crate) struct NotificationHandler<N, F, W>
where
  N: Notification,
  F: Future<Output = ()>,
  W: Clone,
{
  /// Registered typed handler function.
  function: fn(Context<W>, Params<N::Params>) -> F,
  /// Adapter type ownership without storing values.
  marker:   PhantomData<fn() -> N>,
}

impl<N, F, W> NotificationHandler<N, F, W>
where
  N: Notification,
  F: Future<Output = ()>,
  W: Clone,
{
  /// Construct a typed notification handler adapter.
  pub(crate) fn new(function: fn(Context<W>, Params<N::Params>) -> F) -> Self {
    Self {
      function,
      marker: PhantomData,
    }
  }
}

impl<N, F, W> ErasedNotificationHandler<W> for NotificationHandler<N, F, W>
where
  N: Notification + 'static,
  N::Params: DeserializeOwned + 'static,
  F: Future<Output = ()> + 'static,
  W: Clone + 'static,
{
  fn handle(&self, context: Context<W>, params: Option<serde_json::Value>) -> LocalBoxFuture<'static, ()> {
    let function = self.function;
    async move {
      match params.map(serde_json::from_value).transpose() {
        Ok(params) => function(context, Params::from(params)).await,
        Err(error) => {
          tracing::warn!(%error, "invalid notification parameters");
        }
      }
    }
    .boxed_local()
  }
}
