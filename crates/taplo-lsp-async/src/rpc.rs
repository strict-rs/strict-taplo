//! JSON-RPC wire values and standard LSP error constructors.

use lsp_types::NumberOrString;
use serde::de::DeserializeOwned;
use serde::de::Error as DeserializeError;
use thiserror::Error as ThisError;

/// A JSON-RPC request identifier.
pub type RequestId = NumberOrString;

/// Presence-aware JSON-RPC wire identifier.
///
/// Requests and ordinary responses carry [`Self::Value`], notifications omit
/// the field through [`Self::Missing`], and protocol-error responses use
/// [`Self::Null`] when no valid request identifier can be recovered.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum MessageId {
  /// The `id` field is absent.
  #[default]
  Missing,
  /// The `id` field is explicitly JSON `null`.
  Null,
  /// The `id` field contains a valid LSP request identifier.
  Value(RequestId),
}

impl MessageId {
  /// Return whether the identifier field is absent.
  #[must_use]
  pub const fn is_missing(&self) -> bool {
    matches!(self, Self::Missing)
  }

  /// Return whether the identifier contains a concrete request ID.
  #[must_use]
  pub const fn is_value(&self) -> bool {
    matches!(self, Self::Value(_))
  }

  /// Convert an optional request ID into a wire identifier.
  #[must_use]
  pub fn from_optional(id: Option<RequestId>) -> Self {
    id.map_or(Self::Missing, Self::Value)
  }
}

impl From<RequestId> for MessageId {
  fn from(id: RequestId) -> Self {
    Self::Value(id)
  }
}

impl serde::Serialize for MessageId {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: serde::Serializer,
  {
    match *self {
      Self::Missing | Self::Null => serializer.serialize_none(),
      Self::Value(ref request_id) => request_id.serialize(serializer),
    }
  }
}

impl<'de> serde::Deserialize<'de> for MessageId {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: serde::Deserializer<'de>,
  {
    let wire_id = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
    if wire_id.is_null() {
      Ok(Self::Null)
    } else {
      serde_json::from_value(wire_id)
        .map(Self::Value)
        .map_err(DeserializeError::custom)
    }
  }
}

/// Deserialize a present JSON field without treating JSON `null` as absence.
fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
  D: serde::Deserializer<'de>,
  T: serde::Deserialize<'de>,
{
  <T as serde::Deserialize>::deserialize(deserializer).map(Some)
}

/// A complete JSON-RPC wire message before request/notification/response classification.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
  /// JSON-RPC protocol version.
  pub jsonrpc: String,
  /// Request or notification method.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub method:  Option<String>,
  /// Request or response identifier.
  #[serde(default, skip_serializing_if = "MessageId::is_missing")]
  pub id:      MessageId,
  /// Request or notification parameters.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub params:  Option<serde_json::Value>,
  /// Successful response value.
  #[serde(
    default,
    deserialize_with = "deserialize_present",
    skip_serializing_if = "Option::is_none"
  )]
  pub result:  Option<serde_json::Value>,
  /// Failed response value.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub error:   Option<RpcError>,
}

impl Message {
  /// Return whether this message is a notification.
  #[must_use]
  pub const fn is_notification(&self) -> bool {
    self.method.is_some() && self.id.is_missing()
  }

  /// Return whether this message is a response.
  #[must_use]
  pub const fn is_response(&self) -> bool {
    self.method.is_none() && self.id.is_value()
  }
}

/// A typed JSON-RPC request or notification value.
#[derive(Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request<T = ()> {
  /// JSON-RPC protocol version.
  pub jsonrpc: String,
  /// Request or notification method.
  pub method:  String,
  /// Optional request identifier; absent for notifications.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub id:      Option<RequestId>,
  /// Optional typed parameters.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub params:  Option<T>,
}

impl<T> Request<T> {
  /// Construct an empty JSON-RPC 2.0 request builder.
  #[must_use]
  pub fn new() -> Self {
    Self {
      jsonrpc: "2.0".into(),
      method:  String::new(),
      id:      None,
      params:  None,
    }
  }

  /// Set the request method.
  #[must_use]
  pub fn with_method(self, method: impl Into<String>) -> Self {
    Self {
      method: method.into(),
      ..self
    }
  }

  /// Set the optional request identifier.
  #[must_use]
  pub fn with_id(self, id: Option<RequestId>) -> Self {
    Self {
      id,
      ..self
    }
  }

  /// Set the optional typed parameters.
  #[must_use]
  pub fn with_params(self, params: Option<T>) -> Self {
    Self {
      params,
      ..self
    }
  }

  /// Serialize this typed request into a wire message.
  ///
  /// ```
  /// use lsp_types::NumberOrString;
  /// use strict_test_support::{TestFailure, ensure, ensure_ok};
  /// use taplo_lsp_async::rpc::{MessageId, Request};
  ///
  /// fn main() -> Result<(), TestFailure> {
  ///   let message = ensure_ok(
  ///     Request::new()
  ///       .with_method("workspace/example")
  ///       .with_id(Some(NumberOrString::Number(7)))
  ///       .with_params(Some(serde_json::json!({ "enabled": true })))
  ///       .try_into_message(),
  ///     "the typed request must serialize",
  ///   )?;
  ///   ensure(
  ///     message.id == MessageId::Value(NumberOrString::Number(7))
  ///       && message.method.as_deref() == Some("workspace/example"),
  ///     "the wire request must retain its method and identifier",
  ///   )
  /// }
  /// ```
  ///
  /// # Errors
  ///
  /// Returns the serializer error when present parameters cannot be represented as JSON.
  pub fn try_into_message(self) -> Result<Message, serde_json::Error>
  where
    T: serde::Serialize,
  {
    Ok(Message {
      jsonrpc: self.jsonrpc,
      method:  Some(self.method),
      id:      MessageId::from_optional(self.id),
      params:  self.params.map(serde_json::to_value).transpose()?,
      result:  None,
      error:   None,
    })
  }
  /// Deserialize this request's parameters into another owned type.
  ///
  /// # Errors
  ///
  /// Returns the deserializer error when the raw parameters do not match `P`.
  pub fn try_into_params<P: DeserializeOwned>(self) -> Result<Request<P>, serde_json::Error>
  where
    T: Into<serde_json::Value>,
  {
    Ok(Request {
      id:      self.id,
      jsonrpc: self.jsonrpc,
      method:  self.method,
      params:  self.params.map(|params| serde_json::from_value(params.into())).transpose()?,
    })
  }
}

/// A typed JSON-RPC response.
#[derive(Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(bound(deserialize = "R: serde::Deserialize<'de>"))]
pub struct Response<R = ()> {
  /// JSON-RPC protocol version.
  pub jsonrpc: String,
  /// ID of the corresponding request.
  pub id:      RequestId,
  /// Optional successful response value.
  #[serde(
    default,
    deserialize_with = "deserialize_present",
    skip_serializing_if = "Option::is_none"
  )]
  pub result:  Option<R>,
  /// Optional failed response value.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub error:   Option<RpcError>,
}

impl<R> Response<R> {
  /// Replace the response's request identifier.
  #[must_use]
  pub fn with_request_id(self, id: RequestId) -> Self {
    Self {
      id,
      ..self
    }
  }

  /// Extract a successful result or its JSON-RPC error.
  ///
  /// # Errors
  ///
  /// Returns the response error, or an internal error when the response
  /// contains neither channel or both channels.
  pub fn into_result(self) -> Result<R, RpcError> {
    match (self.result, self.error) {
      (Some(response_result), None) => Ok(response_result),
      (None, Some(error)) => Err(error),
      (None, None) => Err(RpcError::internal_error().with_details("response contains neither result nor error")),
      (Some(_response_result), Some(_error)) => Err(RpcError::internal_error().with_details("response contains both result and error")),
    }
  }

  /// Serialize this typed response into a wire message.
  ///
  /// # Errors
  ///
  /// Returns the serializer error when a successful result cannot be represented as JSON.
  pub fn try_into_message(self) -> Result<Message, serde_json::Error>
  where
    R: serde::Serialize,
  {
    Ok(Message {
      jsonrpc: self.jsonrpc,
      method:  None,
      id:      MessageId::Value(self.id),
      params:  None,
      result:  self.result.map(serde_json::to_value).transpose()?,
      error:   self.error,
    })
  }

  /// Construct a successful response with the caller-replaceable default request ID.
  #[must_use]
  #[allow(
    clippy::single_call_fn,
    reason = "the named success constructor preserves the typed response-building API used by Result conversion"
  )]
  pub fn success(response_result: R) -> Self {
    Self {
      jsonrpc: "2.0".into(),
      id:      NumberOrString::Number(0),
      result:  Some(response_result),
      error:   None,
    }
  }

  /// Deserialize this response's successful value into another owned type.
  ///
  /// # Errors
  ///
  /// Returns the deserializer error when the raw result does not match `P`.
  pub fn try_into_params<P: DeserializeOwned>(self) -> Result<Response<P>, serde_json::Error>
  where
    R: Into<serde_json::Value>,
  {
    Ok(Response {
      jsonrpc: self.jsonrpc,
      id:      self.id,
      result:  self.result.map(Into::into).map(serde_json::from_value).transpose()?,
      error:   self.error,
    })
  }

  /// Construct a failed response with the caller-replaceable default request ID.
  #[must_use]
  pub fn error(error: RpcError) -> Self {
    Self {
      jsonrpc: "2.0".into(),
      id:      NumberOrString::Number(0),
      result:  None,
      error:   Some(error),
    }
  }
}

/// Failure while decoding one JSON-RPC body into a wire message.
#[derive(Debug, ThisError)]
pub enum MessageDecodeError {
  /// The body is not syntactically valid JSON.
  #[error("JSON-RPC body is not valid JSON: {source}")]
  Parse {
    /// Underlying JSON parser failure.
    #[source]
    source: serde_json::Error,
  },
  /// The body is valid JSON but not a supported JSON-RPC message object.
  #[error("invalid JSON-RPC request: {source}")]
  InvalidRequest {
    /// Recoverable request identifier, or JSON `null`.
    id:     MessageId,
    /// Underlying message decoder failure.
    #[source]
    source: serde_json::Error,
  },
  /// The body appears to be a client response but its typed shape is invalid.
  #[error("invalid JSON-RPC response {id:?}: {source}")]
  InvalidResponse {
    /// Recovered concrete response identifier.
    id:     RequestId,
    /// Underlying message decoder failure.
    #[source]
    source: serde_json::Error,
  },
}

impl MessageDecodeError {
  /// Convert a request-side protocol failure into its JSON-RPC error response.
  ///
  /// # Errors
  ///
  /// Returns [`InvalidResponseDecode`] for a malformed client response because
  /// JSON-RPC forbids responding to a response.
  pub fn into_response(self) -> Result<Message, InvalidResponseDecode> {
    let (response_id, rpc_error) = match self {
      Self::Parse {
        source,
      } => (MessageId::Null, RpcError::parse().with_details(source.to_string())),
      Self::InvalidRequest {
        id: request_id,
        source,
      } => (request_id, RpcError::invalid_request().with_details(source.to_string())),
      Self::InvalidResponse {
        id: response_id,
        source,
      } => {
        return Err(InvalidResponseDecode {
          id: response_id,
          source,
        });
      }
    };
    Ok(Message {
      jsonrpc: "2.0".into(),
      method:  None,
      id:      response_id,
      params:  None,
      result:  None,
      error:   Some(rpc_error),
    })
  }
}

/// A malformed client response that must not receive another response.
#[derive(Debug, ThisError)]
#[error("invalid JSON-RPC response {id:?}: {source}")]
pub struct InvalidResponseDecode {
  /// Recovered concrete response identifier.
  pub id:     RequestId,
  /// Underlying message decoder failure.
  #[source]
  pub source: serde_json::Error,
}

/// Decode one framed JSON-RPC body.
///
/// # Errors
///
/// Returns [`MessageDecodeError::Parse`] for invalid JSON and
/// [`MessageDecodeError::InvalidRequest`] for a valid JSON value that does not
/// satisfy the supported message shape.
#[allow(
  clippy::single_call_fn,
  reason = "the byte-slice decoder is the framed transport boundary and delegates parsed-value classification separately"
)]
pub fn decode_slice(body: &[u8]) -> Result<Message, MessageDecodeError> {
  let wire_message = serde_json::from_slice(body).map_err(|source| MessageDecodeError::Parse {
    source,
  })?;
  decode_value(wire_message)
}

/// Decode one already-parsed JSON value into a wire message.
///
/// # Errors
///
/// Returns [`MessageDecodeError::InvalidRequest`] when the value does not
/// satisfy the supported message shape.
#[allow(
  clippy::single_call_fn,
  reason = "the parsed-value decoder is an independently callable JSON-RPC boundary that preserves identifier recovery semantics"
)]
pub fn decode_value(wire_message: serde_json::Value) -> Result<Message, MessageDecodeError> {
  let recovered_id = recover_response_id(&wire_message);
  let appears_to_be_response = wire_message.as_object().is_some_and(|object| !object.contains_key("method"));
  serde_json::from_value(wire_message).map_err(|source| match (appears_to_be_response, recovered_id) {
    (true, MessageId::Value(response_id)) => MessageDecodeError::InvalidResponse {
      id: response_id,
      source,
    },
    (_, request_id) => MessageDecodeError::InvalidRequest {
      id: request_id,
      source,
    },
  })
}

/// Recover a valid response identifier from a malformed request object.
#[allow(
  clippy::single_call_fn,
  reason = "identifier recovery isolates malformed-response classification from general message decoding"
)]
fn recover_response_id(wire_message: &serde_json::Value) -> MessageId {
  let Some(wire_id) = wire_message.as_object().and_then(|object| object.get("id")) else {
    return MessageId::Null;
  };
  if wire_id.is_null() {
    MessageId::Null
  } else {
    serde_json::from_value(wire_id.clone()).map_or(MessageId::Null, MessageId::Value)
  }
}

impl<E, R> From<Result<R, E>> for Response<R>
where
  E: Into<RpcError>,
{
  fn from(outcome: Result<R, E>) -> Self {
    match outcome {
      Ok(response_result) => Self::success(response_result),
      Err(error) => Self {
        jsonrpc: "2.0".into(),
        id:      NumberOrString::Number(0),
        result:  None,
        error:   Some(error.into()),
      },
    }
  }
}

/// A typed JSON-RPC error response body.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize, ThisError)]
#[error("RPC error ({code}): {message}")]
pub struct RpcError {
  /// JSON-RPC numeric error code.
  pub code:    i32,
  /// Reader-facing error summary.
  pub message: String,
  /// Optional structured protocol details.
  #[serde(rename = "data")]
  pub details: Option<serde_json::Value>,
}

impl RpcError {
  /// Construct a custom error with code zero.
  #[must_use]
  #[allow(
    clippy::single_call_fn,
    reason = "the named constructor is the custom-error entry point paired with with_code and with_details"
  )]
  pub fn new(message: impl Into<String>) -> Self {
    Self {
      code:    0,
      message: message.into(),
      details: None,
    }
  }

  /// Replace the error code.
  #[must_use]
  pub const fn with_code(mut self, code: i32) -> Self {
    self.code = code;
    self
  }

  /// Attach structured protocol details to the error.
  #[must_use]
  pub fn with_details(mut self, details: impl Into<serde_json::Value>) -> Self {
    self.details = Some(details.into());
    self
  }

  /// Construct the standard parse error.
  #[must_use]
  #[allow(
    clippy::single_call_fn,
    reason = "the named parse-error constructor exposes the standard JSON-RPC code and message as a stable protocol API"
  )]
  pub fn parse() -> Self {
    Self::standard(-32700, "Parse error")
  }

  /// Construct the standard invalid-request error.
  #[must_use]
  pub fn invalid_request() -> Self {
    Self::standard(-32600, "Invalid request")
  }

  /// Construct the standard method-not-found error.
  #[must_use]
  pub fn method_not_found() -> Self {
    Self::standard(-32601, "Method not found")
  }

  /// Construct the standard invalid-parameters error.
  #[must_use]
  pub fn invalid_params() -> Self {
    Self::standard(-32602, "Invalid params")
  }

  /// Construct the standard internal error.
  #[must_use]
  pub fn internal_error() -> Self {
    Self::standard(-32603, "Internal error")
  }

  /// Construct the LSP server-not-initialized error.
  #[must_use]
  #[allow(
    clippy::single_call_fn,
    reason = "the named server-not-initialized constructor exposes the standard LSP code and message as a stable protocol API"
  )]
  pub fn server_not_initialized() -> Self {
    Self::standard(-32002, "Server not initialized")
  }

  /// Construct the LSP request-cancelled error.
  #[must_use]
  pub fn request_cancelled() -> Self {
    Self::standard(-32800, "Request cancelled")
  }

  /// Construct the LSP content-modified error.
  #[must_use]
  #[allow(
    clippy::single_call_fn,
    reason = "the named content-modified constructor exposes the standard LSP code and message as a stable protocol API"
  )]
  pub fn content_modified() -> Self {
    Self::standard(-32801, "Content modified")
  }

  /// Construct a reserved JSON-RPC server error.
  ///
  /// # Errors
  ///
  /// Returns [`InvalidServerCode`] unless `code` lies in `-32099..=-32000`.
  pub fn server(code: i32) -> Result<Self, InvalidServerCode> {
    if (-32099..=-32000).contains(&code) {
      Ok(Self::standard(code, "Server error"))
    } else {
      Err(InvalidServerCode {
        code,
      })
    }
  }

  /// Construct an error with no detail.
  fn standard(code: i32, message: &str) -> Self {
    Self {
      code,
      message: message.into(),
      details: None,
    }
  }
}

/// A code outside JSON-RPC's reserved server-error range.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ThisError)]
#[error("server error code {code} lies outside -32099..=-32000")]
pub struct InvalidServerCode {
  /// The rejected code.
  pub code: i32,
}

#[cfg(test)]
mod tests {
  use std::error::Error as StdError;

  use lsp_types::NumberOrString;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;

  use super::Message;
  use super::MessageDecodeError;
  use super::MessageId;
  use super::Request;
  use super::Response;
  use super::RpcError;
  use super::decode_slice;
  use super::decode_value;
  use crate::SerializationFailureFixture;

  /// Require one parsed JSON value to fail the supported wire-message contract.
  fn decode_failure(wire_message: serde_json::Value, context: &'static str) -> Result<MessageDecodeError, TestFailure> {
    ensure_some(decode_value(wire_message).err(), context)
  }

  /// Construct a malformed request with one recoverable string identifier.
  #[allow(
    clippy::single_call_fn,
    reason = "the malformed-request fixture names the recoverable-ID decoding contract exercised by the protocol-error test"
  )]
  fn malformed_request() -> serde_json::Value {
    serde_json::json!({
      "jsonrpc": "2.0",
      "id": "recoverable",
      "method": 17
    })
  }

  /// Construct a malformed response with one recoverable numeric identifier.
  #[allow(
    clippy::single_call_fn,
    reason = "the malformed-response fixture names the response-classification contract exercised by the no-reply test"
  )]
  fn malformed_response() -> serde_json::Value {
    serde_json::json!({
      "jsonrpc": "2.0",
      "id": 11,
      "error": "not an error object"
    })
  }

  #[test]
  fn decodes_each_supported_wire_family() -> Result<(), TestFailure> {
    let request = ensure_ok(
      decode_slice(br#"{"jsonrpc":"2.0","id":7,"method":"fixture","params":{"value":1}}"#),
      "a valid request must decode",
    )?;
    ensure(
      (request.method.as_deref(), request.id) == (Some("fixture"), MessageId::Value(NumberOrString::Number(7))),
      "a request must retain its method and concrete ID",
    )?;

    let notification = ensure_ok(
      decode_slice(br#"{"jsonrpc":"2.0","method":"fixture"}"#),
      "a valid notification must decode",
    )?;
    ensure(
      (notification.method.as_deref(), notification.id) == (Some("fixture"), MessageId::Missing),
      "a notification must preserve an absent ID",
    )?;

    let response = ensure_ok(
      decode_slice(br#"{"jsonrpc":"2.0","id":"fixture","result":null}"#),
      "a valid response must decode",
    )?;
    ensure(
      (response.method, response.id, response.result)
        == (
          None,
          MessageId::Value(NumberOrString::String("fixture".into())),
          Some(serde_json::Value::Null),
        ),
      "a response must retain its ID and explicit null result",
    )?;

    let error_response = ensure_ok(
      decode_slice(br#"{"jsonrpc":"2.0","id":8,"error":{"code":-32601,"message":"missing","data":null}}"#),
      "a valid error response must decode",
    )?;
    ensure(
      (error_response.result.is_some(), error_response.error.is_some()) == (false, true),
      "an absent result field must remain distinct from an explicit null result",
    )?;

    let typed_null = ensure_ok(
      serde_json::from_slice::<Response<serde_json::Value>>(br#"{"jsonrpc":"2.0","id":9,"result":null}"#),
      "a typed response with an explicit null result must deserialize",
    )?;
    let typed_absent = ensure_ok(
      serde_json::from_slice::<Response<serde_json::Value>>(br#"{"jsonrpc":"2.0","id":9}"#),
      "a typed response with an absent result must deserialize",
    )?;
    ensure(
      (typed_null.result, typed_absent.result) == (Some(serde_json::Value::Null), None),
      "typed responses must preserve the wire distinction between explicit null and absence",
    )
  }

  #[test]
  fn message_identifiers_preserve_presence_and_classification_contracts() -> Result<(), TestFailure> {
    let request_id = NumberOrString::String("request".into());
    ensure(
      (
        MessageId::from(request_id.clone()),
        MessageId::from_optional(Some(request_id.clone())),
        MessageId::from_optional(None),
      ) == (
        MessageId::Value(request_id.clone()),
        MessageId::Value(request_id.clone()),
        MessageId::Missing,
      ),
      "direct and optional request identifiers must preserve concrete and absent presence states",
    )?;

    let notification = Message {
      jsonrpc: "2.0".into(),
      method: Some("fixture/notify".into()),
      ..Message::default()
    };
    let request = Message {
      id: MessageId::Value(request_id.clone()),
      ..notification.clone()
    };
    let response = Message {
      jsonrpc: "2.0".into(),
      id: MessageId::Value(request_id),
      ..Message::default()
    };
    let null_id = Message {
      jsonrpc: "2.0".into(),
      id: MessageId::Null,
      ..Message::default()
    };
    ensure(
      (
        notification.is_notification(),
        notification.is_response(),
        request.is_notification(),
        request.is_response(),
        response.is_notification(),
        response.is_response(),
        null_id.is_notification(),
        null_id.is_response(),
      ) == (true, false, false, false, false, true, false, false),
      "message classification must distinguish notifications, requests, responses, and null protocol identifiers",
    )?;

    let notification_wire = ensure_ok(
      serde_json::to_value(notification),
      "a notification with a missing identifier must serialize",
    )?;
    let null_wire = ensure_ok(
      serde_json::to_value(null_id),
      "a protocol message with a null identifier must serialize",
    )?;
    ensure(
      (notification_wire.get("id"), null_wire.get("id")) == (None, Some(&serde_json::Value::Null)),
      "missing identifiers must be omitted while null identifiers remain explicit on the wire",
    )
  }

  #[test]
  fn syntax_errors_return_null_id_parse_responses() -> Result<(), TestFailure> {
    let error = ensure_some(decode_slice(br#"{"jsonrpc":"2.0""#).err(), "invalid JSON must fail decoding")?;
    ensure(
      matches!(&error, MessageDecodeError::Parse { .. }),
      "invalid JSON must retain the parse-error category",
    )?;
    let response = ensure_ok(error.into_response(), "a parse failure must produce an error response")?;
    ensure(response.id == MessageId::Null, "a parse response must use a JSON null ID")?;
    let rpc_error = ensure_some(response.error.as_ref(), "the parse response must contain an error")?;
    ensure_eq(&rpc_error.code, &-32700, "the parse response must use the standard code")
  }

  #[test]
  fn malformed_messages_preserve_request_response_direction() -> Result<(), TestFailure> {
    let request_error = decode_failure(malformed_request(), "a non-string method must fail decoding")?;
    ensure(
      matches!(&request_error, MessageDecodeError::InvalidRequest { .. }),
      "a malformed request must retain the request-side category",
    )?;
    let response = ensure_ok(request_error.into_response(), "a malformed request must produce an error response")?;
    ensure(
      response.id == MessageId::Value(NumberOrString::String("recoverable".into())),
      "a valid request ID must be echoed in the error response",
    )?;
    let rpc_error = ensure_some(response.error.as_ref(), "the invalid-request response must contain an error")?;
    ensure_eq(&rpc_error.code, &-32600, "the invalid-request response must use the standard code")?;

    let response_error = decode_failure(malformed_response(), "a malformed response must fail decoding")?;
    ensure(
      matches!(&response_error, MessageDecodeError::InvalidResponse { .. }),
      "a malformed client response must retain the response-side category",
    )?;
    let invalid_response = ensure_some(
      response_error.into_response().err(),
      "JSON-RPC must not create a response to a malformed response",
    )?;
    ensure(
      invalid_response.id == NumberOrString::Number(11),
      "the malformed response must retain its concrete ID for diagnostics",
    )?;

    let invalid_id_error = decode_failure(
      serde_json::json!({
        "jsonrpc": "2.0",
        "id": [],
        "method": 17
      }),
      "a malformed request with an invalid ID must fail decoding",
    )?;
    let invalid_id_response = ensure_ok(
      invalid_id_error.into_response(),
      "a malformed request with no recoverable ID must produce an error response",
    )?;
    ensure(
      invalid_id_response.id == MessageId::Null,
      "an invalid request ID must recover as JSON null rather than an invented concrete ID",
    )
  }

  #[test]
  fn request_and_response_builders_preserve_typed_wire_contracts() -> Result<(), TestFailure> {
    let request = ensure_ok(
      Request::new()
        .with_method("fixture")
        .with_id(Some(NumberOrString::Number(7)))
        .with_params(Some(serde_json::json!({
          "enabled": true
        })))
        .try_into_message(),
      "a serializable typed request must become a wire message",
    )?;
    ensure(
      (request.method.as_deref(), request.id, request.params)
        == (
          Some("fixture"),
          MessageId::Value(NumberOrString::Number(7)),
          Some(serde_json::json!({
            "enabled": true
          })),
        ),
      "the request builder must preserve its method, ID, and structured parameters",
    )?;

    let request_params = ensure_ok(
      Request::new()
        .with_method("fixture/owned")
        .with_params(Some(serde_json::json!([1, 2])))
        .try_into_params::<Vec<u32>>(),
      "request parameters must convert into an independently owned typed payload",
    )?;
    ensure(
      request_params.params == Some(vec![1, 2]),
      "request parameter conversion must preserve the complete owned payload",
    )?;

    let response_params = ensure_ok(
      Response::success(serde_json::json!([3, 4])).try_into_params::<Vec<u32>>(),
      "response results must convert into an independently owned typed payload",
    )?;
    ensure(
      response_params.result == Some(vec![3, 4]),
      "response result conversion must preserve the complete owned payload",
    )?;

    ensure(
      Request::new()
        .with_method("fixture/invalid")
        .with_params(Some(serde_json::json!({
          "value": 1
        })))
        .try_into_params::<Vec<u32>>()
        .is_err(),
      "request parameter conversion must reject an incompatible owned payload",
    )?;
    ensure(
      Response::success(serde_json::json!({
        "value": 1
      }))
      .try_into_params::<Vec<u32>>()
      .is_err(),
      "response result conversion must reject an incompatible owned payload",
    )
  }

  #[test]
  fn typed_builders_preserve_notification_and_response_wire_shapes() -> Result<(), TestFailure> {
    let notification = ensure_ok(
      serde_json::to_value(
        Request::<serde_json::Value>::new()
          .with_method("fixture/notify")
          .with_params(None),
      ),
      "a typed notification must serialize",
    )?;
    ensure(
      notification
        == serde_json::json!({
          "jsonrpc": "2.0",
          "method": "fixture/notify"
        }),
      "a typed notification must omit both the request ID and absent parameters",
    )?;

    let success = ensure_ok(
      Response::success(17)
        .with_request_id(NumberOrString::String("request".into()))
        .try_into_message(),
      "a successful response builder must serialize",
    )?;
    ensure(
      (success.id, success.result, success.error)
        == (
          MessageId::Value(NumberOrString::String("request".into())),
          Some(serde_json::json!(17)),
          None,
        ),
      "a successful response builder must preserve its ID and sole result channel",
    )?;

    let error = ensure_ok(
      Response::<()>::error(RpcError::method_not_found())
        .with_request_id(NumberOrString::Number(8))
        .try_into_message(),
      "an error response builder must serialize",
    )?;
    let error_code = error.error.as_ref().map(|rpc_error| rpc_error.code);
    ensure(
      (error.id, error.result, error_code) == (MessageId::Value(NumberOrString::Number(8)), None, Some(-32601)),
      "an error response builder must preserve its ID and sole error channel",
    )
  }

  #[test]
  fn builders_reject_serialization_and_invalid_response_channels() -> Result<(), TestFailure> {
    ensure(
      Request::new()
        .with_method("fixture")
        .with_params(Some(SerializationFailureFixture))
        .try_into_message()
        .is_err(),
      "a request builder must propagate parameter serialization failure",
    )?;
    ensure(
      Response::success(SerializationFailureFixture).try_into_message().is_err(),
      "a response builder must propagate result serialization failure",
    )?;

    let empty = Response::<u32> {
      jsonrpc: "2.0".into(),
      id:      NumberOrString::Number(1),
      result:  None,
      error:   None,
    };
    let empty_error = ensure_some(empty.into_result().err(), "a response without success or error data must fail")?;
    ensure_eq(
      &empty_error.code,
      &RpcError::internal_error().code,
      "an empty response must use the internal-error category",
    )?;

    let ambiguous = Response {
      jsonrpc: "2.0".into(),
      id:      NumberOrString::Number(2),
      result:  Some(17_u32),
      error:   Some(RpcError::internal_error()),
    };
    let ambiguous_error = ensure_some(
      ambiguous.into_result().err(),
      "a response containing both success and error channels must fail",
    )?;
    ensure(
      (ambiguous_error.code, ambiguous_error.details)
        == (
          RpcError::internal_error().code,
          Some(serde_json::Value::String("response contains both result and error".into())),
        ),
      "an ambiguous response must identify the invalid dual-channel shape",
    )?;

    let expected_error = RpcError::method_not_found().with_details("missing fixture");
    let propagated = ensure_some(
      Response::<u32>::error(expected_error.clone())
        .with_request_id(NumberOrString::Number(3))
        .into_result()
        .err(),
      "a sole response error channel must propagate",
    )?;
    ensure(
      propagated == expected_error,
      "single-channel extraction must preserve the complete JSON-RPC error body",
    )
  }

  #[test]
  fn result_conversion_and_error_constructors_preserve_typed_protocol_contracts() -> Result<(), TestFailure> {
    let success: Response<u32> = Result::<u32, RpcError>::Ok(17).into();
    let failure: Response<u32> = Result::<u32, RpcError>::Err(RpcError::request_cancelled()).into();
    ensure(
      (success.result, success.error, failure.result, failure.error) == (Some(17), None, None, Some(RpcError::request_cancelled())),
      "result conversion must map success and failure into exactly one response channel",
    )?;

    let custom = RpcError::new("Custom failure");
    ensure(
      (custom.code, custom.message.as_str(), custom.details.as_ref()) == (0, "Custom failure", None),
      "a custom RPC error must begin with code zero and no structured details",
    )?;
    let customized = custom.with_code(73).with_details(serde_json::json!({
      "reason": "fixture"
    }));
    ensure(
      (customized.code, customized.message.as_str(), customized.details.as_ref())
        == (
          73,
          "Custom failure",
          Some(&serde_json::json!({
            "reason": "fixture"
          })),
        ),
      "custom RPC error builders must preserve the message while replacing code and attaching structured details",
    )?;

    let standard_errors = [
      RpcError::parse(),
      RpcError::invalid_request(),
      RpcError::method_not_found(),
      RpcError::invalid_params(),
      RpcError::internal_error(),
      RpcError::server_not_initialized(),
      RpcError::request_cancelled(),
      RpcError::content_modified(),
    ];
    let standard_contracts = standard_errors
      .iter()
      .map(|error| (error.code, error.message.as_str(), error.details.is_none()))
      .collect::<Vec<_>>();
    ensure(
      standard_contracts
        == vec![
          (-32700, "Parse error", true),
          (-32600, "Invalid request", true),
          (-32601, "Method not found", true),
          (-32602, "Invalid params", true),
          (-32603, "Internal error", true),
          (-32002, "Server not initialized", true),
          (-32800, "Request cancelled", true),
          (-32801, "Content modified", true),
        ],
      "every named protocol constructor must preserve its standard code, message, and detail-free shape",
    )
  }

  #[test]
  fn server_error_codes_accept_only_the_reserved_range() -> Result<(), TestFailure> {
    for code in [-32099, -32000] {
      let error = ensure_ok(
        RpcError::server(code),
        "each inclusive JSON-RPC server-code boundary must be accepted",
      )?;
      ensure_eq(&error.code, &code, "the server-error constructor must preserve an accepted code")?;
    }
    for code in [-32100, -31999] {
      let error = ensure_some(
        RpcError::server(code).err(),
        "a code outside the JSON-RPC server range must be rejected",
      )?;
      ensure_eq(&error.code, &code, "the invalid-code error must preserve the rejected value")?;
    }
    Ok(())
  }

  #[test]
  fn rpc_errors_implement_the_standard_error_contract_without_fabricated_sources() -> Result<(), TestFailure> {
    let error = RpcError::internal_error().with_details("diagnostic detail");
    ensure_eq(
      &error.to_string(),
      &String::from("RPC error (-32603): Internal error"),
      "the standard error contract must preserve the stable reader-facing RPC description",
    )?;
    let serialized = ensure_ok(serde_json::to_value(&error), "an RPC error with structured details must serialize")?;
    let wire_details = ensure_some(
      serialized.get("data"),
      "structured RPC details must use the JSON-RPC data member on the wire",
    )?;
    ensure_eq(
      wire_details,
      &serde_json::json!("diagnostic detail"),
      "the JSON-RPC data member must preserve the structured detail",
    )?;
    ensure(
      serialized.get("details").is_none(),
      "the contextual Rust field name must not leak into the JSON-RPC wire object",
    )?;
    ensure(
      StdError::source(&error).is_none(),
      "structured RPC details must remain protocol metadata rather than a fabricated causal error",
    )
  }
}
