//! Local WebAssembly LSP binding and validated JavaScript output transport.

use std::fmt;
use std::io;
use std::io::ErrorKind;
use std::pin::Pin;
use std::rc::Rc;
use std::task::Context;
use std::task::Poll;

use futures::Sink;
use futures::SinkExt as _;
use js_sys::Function;
use js_sys::Promise;
use js_sys::Reflect;
use taplo_lsp::world::LocalWorld;
use taplo_lsp_async::LocalServer;
use taplo_lsp_async::MessageWriterError;
use taplo_lsp_async::rpc;
use taplo_lsp_async::rpc::Message;
use thiserror::Error as ThisError;
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::prelude::JsError;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen_futures::future_to_promise;

use crate::LocalWasmFuture;
use crate::WasmEnvironment;
use crate::js_error;
use crate::js_error_message;

/// Local LSP state exported to JavaScript.
#[wasm_bindgen]
pub struct TaploWasmLsp {
  /// Shared current-thread LSP state retained by exported promises.
  inner: Rc<TaploWasmLspInner>,
}

impl fmt::Debug for TaploWasmLsp {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.debug_struct("TaploWasmLsp").finish_non_exhaustive()
  }
}

/// Current-thread server state with a lifetime independent from one JavaScript method call.
struct TaploWasmLspInner {
  /// Current-thread protocol server.
  server:        LocalServer<LocalWorld<WasmEnvironment>>,
  /// Shared local world.
  world:         LocalWorld<WasmEnvironment>,
  /// Validated JavaScript output callback.
  lsp_interface: WasmLspInterface,
}

/// Construct exported state around one fully initialized local server.
pub(super) fn new(
  server: LocalServer<LocalWorld<WasmEnvironment>>,
  world: LocalWorld<WasmEnvironment>,
  lsp_interface: WasmLspInterface,
) -> TaploWasmLsp {
  TaploWasmLsp {
    inner: Rc::new(TaploWasmLspInner {
      server,
      world,
      lsp_interface,
    }),
  }
}

#[wasm_bindgen]
impl TaploWasmLsp {
  /// Submit one JSON-RPC message to the local server.
  ///
  /// # Errors
  ///
  /// Returns [`JsError`] when the JavaScript value cannot be decoded, a
  /// malformed client response cannot legally receive a response, or the
  /// local server/output boundary fails.
  pub fn send(&self, javascript_message: JsValue) -> Promise {
    let inner = Rc::clone(&self.inner);
    future_to_promise(async move {
      match inner.send(javascript_message).await {
        Ok(()) => Ok(JsValue::undefined()),
        Err(error) => Err(JsValue::from(error)),
      }
    })
  }
}

impl TaploWasmLspInner {
  /// Decode and submit one message while retaining typed Rust failures.
  fn send(&self, javascript_message: JsValue) -> LocalWasmFuture<'_, Result<(), JsError>> {
    Box::pin(async move {
      let wire_value = serde_wasm_bindgen::from_value(javascript_message).map_err(js_error)?;
      let decoded_message = match rpc::decode_value(wire_value) {
        Ok(decoded_message) => decoded_message,
        Err(error) => {
          let response = error.into_response().map_err(js_error)?;
          return self.lsp_interface.clone().send(response).await.map_err(js_error);
        }
      };
      self
        .server
        .handle_message(Rc::clone(&self.world), decoded_message, self.lsp_interface.clone())
        .await
        .map_err(js_error)
    })
  }
}

/// A JavaScript LSP output-interface initialization failure.
#[derive(Debug, ThisError)]
pub(super) enum WasmLspInterfaceError {
  /// Reading the callback property threw.
  #[error("failed to read JavaScript LSP callback `js_on_message`: {message}")]
  CallbackProperty {
    /// Stable JavaScript exception text.
    message: String,
  },
  /// The required callback is absent.
  #[error("required JavaScript LSP callback `js_on_message` is missing")]
  MissingCallback,
  /// The required property is not callable.
  #[error("JavaScript LSP property `js_on_message` is not a function")]
  InvalidCallback,
}

/// Validated JavaScript output callback used as a local message sink.
#[derive(Clone)]
pub(super) struct WasmLspInterface {
  /// Required output callback.
  on_message: Function,
}

impl TryFrom<JsValue> for WasmLspInterface {
  type Error = WasmLspInterfaceError;

  fn try_from(javascript_interface: JsValue) -> Result<Self, Self::Error> {
    let callback = Reflect::get(&javascript_interface, &JsValue::from_str("js_on_message")).map_err(|error| {
      WasmLspInterfaceError::CallbackProperty {
        message: js_error_message(&error),
      }
    })?;
    if callback.is_null() || callback.is_undefined() {
      return Err(WasmLspInterfaceError::MissingCallback);
    }
    Ok(Self {
      on_message: callback
        .dyn_ref::<Function>()
        .cloned()
        .ok_or(WasmLspInterfaceError::InvalidCallback)?,
    })
  }
}

impl Sink<Message> for WasmLspInterface {
  type Error = MessageWriterError;

  fn poll_ready(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
    Poll::Ready(Ok(()))
  }

  fn start_send(self: Pin<&mut Self>, message: Message) -> Result<(), Self::Error> {
    let javascript_message = match serde_wasm_bindgen::to_value(&message) {
      Ok(javascript_message) => javascript_message,
      Err(_serialization_error) => {
        return Err(MessageWriterError::from(io::Error::from(ErrorKind::InvalidData)));
      }
    };
    let returned = match self.on_message.call1(&JsValue::null(), &javascript_message) {
      Ok(returned) => returned,
      Err(_javascript_error) => {
        return Err(MessageWriterError::from(io::Error::from(ErrorKind::BrokenPipe)));
      }
    };
    drop(returned);
    Ok(())
  }

  taplo_lsp_async::implement_message_writer_readiness!();
}
