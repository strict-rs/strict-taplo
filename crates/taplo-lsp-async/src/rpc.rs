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
  /// use strict_test_support::{PredicateFailure, ensure_that};
  /// use taplo_lsp_async::rpc::{Message, MessageId, Request};
  ///
  /// fn main() -> Result<(), PredicateFailure<Result<Message, serde_json::Error>>> {
  ///   ensure_that(
  ///     Request::new()
  ///       .with_method("workspace/example")
  ///       .with_id(Some(NumberOrString::Number(7)))
  ///       .with_params(Some(serde_json::json!({ "enabled": true })))
  ///       .try_into_message(),
  ///     "the wire request must retain its method and identifier",
  ///     |outcome| outcome.as_ref().is_ok_and(|message| {
  ///       message.id == MessageId::Value(NumberOrString::Number(7))
  ///         && message.method.as_deref() == Some("workspace/example")
  ///     }),
  ///   ).map(drop)
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
  use strict_test_support::ComparisonFailure;
  use strict_test_support::PredicateFailure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_that;

  use super::InvalidResponseDecode;
  use super::InvalidServerCode;
  use super::Message;
  use super::MessageDecodeError;
  use super::MessageId;
  use super::Request;
  use super::Response;
  use super::RpcError;
  use super::decode_slice;
  use super::decode_value;
  use crate::SerializationFailureFixture;

  /// Full wire-family decoding results, including both typed presence states.
  #[derive(Debug)]
  struct WireFamilies {
    /// Requests, notifications, successful responses, and failed responses.
    messages: [Result<Message, MessageDecodeError>; 4],
    /// Explicit null and missing successful-response fields.
    typed:    [Result<Response<serde_json::Value>, serde_json::Error>; 2],
  }

  #[test]
  fn decodes_each_supported_wire_family() -> Result<(), Box<PredicateFailure<WireFamilies>>> {
    let observed = WireFamilies {
      messages: [
        decode_slice(br#"{"jsonrpc":"2.0","id":7,"method":"fixture","params":{"value":1}}"#),
        decode_slice(br#"{"jsonrpc":"2.0","method":"fixture"}"#),
        decode_slice(br#"{"jsonrpc":"2.0","id":"fixture","result":null}"#),
        decode_slice(br#"{"jsonrpc":"2.0","id":8,"error":{"code":-32601,"message":"missing","data":null}}"#),
      ],
      typed:    [
        serde_json::from_slice(br#"{"jsonrpc":"2.0","id":9,"result":null}"#),
        serde_json::from_slice(br#"{"jsonrpc":"2.0","id":9}"#),
      ],
    };
    ensure_that(
      observed,
      "wire decoding must retain every message family and distinguish null from absent results",
      |observations| {
        let [ref request, ref notification, ref response, ref error] = observations.messages;
        let [ref typed_null, ref typed_absent] = observations.typed;
        request
          .as_ref()
          .is_ok_and(|message| message.method.as_deref() == Some("fixture") && message.id == MessageId::Value(NumberOrString::Number(7)))
          && notification
            .as_ref()
            .is_ok_and(|message| message.method.as_deref() == Some("fixture") && message.id == MessageId::Missing)
          && response.as_ref().is_ok_and(|message| {
            message.method.is_none()
              && message.id == MessageId::Value(NumberOrString::String("fixture".into()))
              && message.result == Some(serde_json::Value::Null)
          })
          && error
            .as_ref()
            .is_ok_and(|message| message.result.is_none() && message.error.is_some())
          && typed_null
            .as_ref()
            .is_ok_and(|message| message.result == Some(serde_json::Value::Null))
          && typed_absent.as_ref().is_ok_and(|message| message.result.is_none())
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Original wire messages and their presence-aware serialized forms.
  #[derive(Debug)]
  struct PresenceObservations {
    /// Direct and optional identifier conversions.
    identifiers: [MessageId; 3],
    /// Notification, request, response, and null-ID message subjects.
    messages:    [Message; 4],
    /// Serialized notification and null-ID object.
    serialized:  [Result<serde_json::Value, serde_json::Error>; 2],
  }

  #[test]
  fn message_identifiers_preserve_presence_and_classification_contracts() -> Result<(), Box<PredicateFailure<PresenceObservations>>> {
    let request_id = NumberOrString::String("request".into());
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
      id: MessageId::Value(request_id.clone()),
      ..Message::default()
    };
    let null_id = Message {
      jsonrpc: "2.0".into(),
      id: MessageId::Null,
      ..Message::default()
    };
    let observed = PresenceObservations {
      identifiers: [
        MessageId::from(request_id.clone()),
        MessageId::from_optional(Some(request_id.clone())),
        MessageId::from_optional(None),
      ],
      serialized:  [serde_json::to_value(&notification), serde_json::to_value(&null_id)],
      messages:    [notification, request, response, null_id],
    };
    ensure_that(
      observed,
      "wire identifiers must retain presence, classification, and serialized absence",
      |observations| {
        let [
          ref observed_notification,
          ref observed_request,
          ref observed_response,
          ref observed_null,
        ] = observations.messages;
        let [ref notification_wire, ref null_wire] = observations.serialized;
        observations.identifiers
          == [
            MessageId::Value(request_id.clone()),
            MessageId::Value(request_id.clone()),
            MessageId::Missing,
          ]
          && (
            observed_notification.is_notification(),
            observed_notification.is_response(),
            observed_request.is_notification(),
            observed_request.is_response(),
            observed_response.is_notification(),
            observed_response.is_response(),
            observed_null.is_notification(),
            observed_null.is_response(),
          ) == (true, false, false, false, false, true, false, false)
          && notification_wire.as_ref().is_ok_and(|wire| wire.get("id").is_none())
          && null_wire
            .as_ref()
            .is_ok_and(|wire| wire.get("id") == Some(&serde_json::Value::Null))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Native decoding failure and its consumed protocol conversion boundary.
  #[derive(Debug, thiserror::Error)]
  enum DecodeResponseFailure {
    /// Decoding unexpectedly succeeded or used a different failure category.
    #[error(transparent)]
    Decode(#[from] PredicateFailure<Result<Message, MessageDecodeError>>),
    /// The expected decoding category produced an incorrect protocol response.
    #[error(transparent)]
    Response(#[from] PredicateFailure<Result<Message, InvalidResponseDecode>>),
    /// A successful message has no decoder error to convert.
    #[error("a successfully decoded message cannot produce a decoder-error response: {message:?}")]
    UnexpectedMessage {
      /// Original successful message.
      message: Message,
    },
  }

  /// Check the decoder category before the protocol owner consumes its native error.
  fn decoded_error_response(
    outcome: Result<Message, MessageDecodeError>,
    context: &'static str,
    predicate: impl FnOnce(&Result<Message, MessageDecodeError>) -> bool,
  ) -> Result<Result<Message, InvalidResponseDecode>, Box<DecodeResponseFailure>> {
    let checked = ensure_that(outcome, context, predicate)
      .map_err(DecodeResponseFailure::from)
      .map_err(Box::new)?;
    match checked {
      Err(error) => Ok(error.into_response()),
      Ok(message) => Err(Box::new(DecodeResponseFailure::UnexpectedMessage {
        message,
      })),
    }
  }

  #[test]
  fn syntax_errors_return_null_id_parse_responses() -> Result<(), Box<DecodeResponseFailure>> {
    let response = decoded_error_response(
      decode_slice(br#"{"jsonrpc":"2.0""#),
      "invalid JSON must retain its native parser category",
      |outcome| matches!(*outcome, Err(MessageDecodeError::Parse { .. })),
    )?;
    ensure_that(
      response,
      "a parse failure must become a null-ID response with the standard parse code",
      |outcome| {
        outcome
          .as_ref()
          .is_ok_and(|message| message.id == MessageId::Null && message.error.as_ref().is_some_and(|error| error.code == -32700))
      },
    )
    .map(drop)
    .map_err(DecodeResponseFailure::from)
    .map_err(Box::new)
  }

  /// All request-side and response-side conversion outcomes retained together.
  type DirectionObservations = [Result<Result<Message, InvalidResponseDecode>, Box<DecodeResponseFailure>>; 3];

  #[test]
  fn malformed_messages_preserve_request_response_direction() -> Result<(), Box<PredicateFailure<DirectionObservations>>> {
    let observations = [
      decoded_error_response(
        decode_value(serde_json::json!({ "jsonrpc": "2.0", "id": "recoverable", "method": 17 })),
        "a malformed request must retain the request-side category",
        |outcome| matches!(*outcome, Err(MessageDecodeError::InvalidRequest { .. })),
      ),
      decoded_error_response(
        decode_value(serde_json::json!({ "jsonrpc": "2.0", "id": 11, "error": "not an error object" })),
        "a malformed response must retain the response-side category",
        |outcome| matches!(*outcome, Err(MessageDecodeError::InvalidResponse { .. })),
      ),
      decoded_error_response(
        decode_value(serde_json::json!({ "jsonrpc": "2.0", "id": [], "method": 17 })),
        "an invalid request ID must remain a request-side failure",
        |outcome| matches!(*outcome, Err(MessageDecodeError::InvalidRequest { .. })),
      ),
    ];
    ensure_that(observations, "request failures must reply with recovered IDs while malformed responses retain their no-reply direction", |observed| {
      let [ref request, ref response, ref invalid_id] = *observed;
      matches!(*request, Ok(Ok(ref message)) if message.id == MessageId::Value(NumberOrString::String("recoverable".into())) && message.error.as_ref().is_some_and(|error| error.code == -32600))
        && matches!(*response, Ok(Err(ref error)) if error.id == NumberOrString::Number(11))
        && matches!(*invalid_id, Ok(Ok(ref message)) if message.id == MessageId::Null)
    }).map(drop).map_err(Box::new)
  }

  /// Owned request payload conversion preserving serde failures.
  type ConvertedRequest = Result<Request<Vec<u32>>, serde_json::Error>;

  /// Owned response payload conversion preserving serde failures.
  type ConvertedResponse = Result<Response<Vec<u32>>, serde_json::Error>;

  /// All typed builder and parameter-conversion results.
  #[derive(Debug)]
  struct BuilderObservations {
    /// Complete serialized request.
    request:         Result<Message, serde_json::Error>,
    /// Compatible and incompatible request payload conversions.
    request_params:  [ConvertedRequest; 2],
    /// Compatible and incompatible response payload conversions.
    response_params: [ConvertedResponse; 2],
  }

  #[test]
  fn request_and_response_builders_preserve_typed_wire_contracts() -> Result<(), Box<PredicateFailure<BuilderObservations>>> {
    let observations = BuilderObservations {
      request:         Request::new()
        .with_method("fixture")
        .with_id(Some(NumberOrString::Number(7)))
        .with_params(Some(serde_json::json!({"enabled": true})))
        .try_into_message(),
      request_params:  [
        Request::new()
          .with_method("fixture/owned")
          .with_params(Some(serde_json::json!([1, 2])))
          .try_into_params(),
        Request::new()
          .with_method("fixture/invalid")
          .with_params(Some(serde_json::json!({"value": 1})))
          .try_into_params(),
      ],
      response_params: [
        Response::success(serde_json::json!([3, 4])).try_into_params(),
        Response::success(serde_json::json!({"value": 1})).try_into_params(),
      ],
    };
    ensure_that(
      observations,
      "typed builders must preserve complete compatible payloads and native conversion failures",
      |observed| {
        let [ref accepted_request, ref rejected_request] = observed.request_params;
        let [ref accepted_response, ref rejected_response] = observed.response_params;
        observed.request.as_ref().is_ok_and(|message| {
          message.method.as_deref() == Some("fixture")
            && message.id == MessageId::Value(NumberOrString::Number(7))
            && message.params == Some(serde_json::json!({"enabled": true}))
        }) && accepted_request
          .as_ref()
          .is_ok_and(|request| request.params == Some(vec![1, 2]))
          && accepted_response
            .as_ref()
            .is_ok_and(|response| response.result == Some(vec![3, 4]))
          && rejected_request.is_err()
          && rejected_response.is_err()
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Serialized notification and both response channel shapes.
  type WireShapes = (
    Result<serde_json::Value, serde_json::Error>,
    [Result<Message, serde_json::Error>; 2],
  );

  #[test]
  fn typed_builders_preserve_notification_and_response_wire_shapes() -> Result<(), Box<PredicateFailure<WireShapes>>> {
    ensure_that(
      (
        serde_json::to_value(
          Request::<serde_json::Value>::new()
            .with_method("fixture/notify")
            .with_params(None),
        ),
        [
          Response::success(17)
            .with_request_id(NumberOrString::String("request".into()))
            .try_into_message(),
          Response::<()>::error(RpcError::method_not_found())
            .with_request_id(NumberOrString::Number(8))
            .try_into_message(),
        ],
      ),
      "typed builders must preserve notification omission and the sole response channel",
      |observed| {
        let (ref notification, [ref success, ref error]) = *observed;
        notification
          .as_ref()
          .is_ok_and(|value| *value == serde_json::json!({"jsonrpc": "2.0", "method": "fixture/notify"}))
          && success.as_ref().is_ok_and(|message| {
            message.id == MessageId::Value(NumberOrString::String("request".into()))
              && message.result == Some(serde_json::json!(17))
              && message.error.is_none()
          })
          && error.as_ref().is_ok_and(|message| {
            message.id == MessageId::Value(NumberOrString::Number(8))
              && message.result.is_none()
              && message.error == Some(RpcError::method_not_found())
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Serialization and response-channel extraction outcomes.
  type RejectedBuilders = ([Result<Message, serde_json::Error>; 2], [Result<u32, RpcError>; 3]);

  #[test]
  fn builders_reject_serialization_and_invalid_response_channels() -> Result<(), Box<PredicateFailure<RejectedBuilders>>> {
    let expected_error = RpcError::method_not_found().with_details("missing fixture");
    let observed = (
      [
        Request::new()
          .with_method("fixture")
          .with_params(Some(SerializationFailureFixture))
          .try_into_message(),
        Response::success(SerializationFailureFixture).try_into_message(),
      ],
      [
        Response::<u32> {
          jsonrpc: "2.0".into(),
          id:      NumberOrString::Number(1),
          result:  None,
          error:   None,
        }
        .into_result(),
        Response {
          jsonrpc: "2.0".into(),
          id:      NumberOrString::Number(2),
          result:  Some(17_u32),
          error:   Some(RpcError::internal_error()),
        }
        .into_result(),
        Response::<u32>::error(expected_error.clone())
          .with_request_id(NumberOrString::Number(3))
          .into_result(),
      ],
    );
    ensure_that(
      observed,
      "builders must retain serialization failures, reject missing or dual channels, and propagate sole RPC errors",
      |observations| {
        let (ref serialization, [ref empty, ref ambiguous, ref propagated]) = *observations;
        serialization.iter().all(Result::is_err)
          && empty.as_ref().is_err_and(|error| error.code == -32603)
          && *ambiguous == Err(RpcError::internal_error().with_details("response contains both result and error"))
          && *propagated == Err(expected_error.clone())
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Complete typed response conversions and error-constructor outputs.
  type ConstructorObservations = ([Response<u32>; 2], [RpcError; 2], [RpcError; 8]);

  #[test]
  fn result_conversion_and_error_constructors_preserve_typed_protocol_contracts()
  -> Result<(), Box<ComparisonFailure<ConstructorObservations, ConstructorObservations>>> {
    let custom = RpcError::new("Custom failure");
    let customized = custom
      .clone()
      .with_code(73)
      .with_details(serde_json::json!({"reason": "fixture"}));
    let observed = (
      [
        Response::from(Ok::<u32, RpcError>(17)),
        Response::from(Err::<u32, RpcError>(RpcError::request_cancelled())),
      ],
      [custom, customized],
      [
        RpcError::parse(),
        RpcError::invalid_request(),
        RpcError::method_not_found(),
        RpcError::invalid_params(),
        RpcError::internal_error(),
        RpcError::server_not_initialized(),
        RpcError::request_cancelled(),
        RpcError::content_modified(),
      ],
    );
    let expected = (
      [
        Response {
          jsonrpc: "2.0".into(),
          id:      NumberOrString::Number(0),
          result:  Some(17),
          error:   None,
        },
        Response {
          jsonrpc: "2.0".into(),
          id:      NumberOrString::Number(0),
          result:  None,
          error:   Some(RpcError {
            code:    -32800,
            message: "Request cancelled".into(),
            details: None,
          }),
        },
      ],
      [
        RpcError {
          code:    0,
          message: "Custom failure".into(),
          details: None,
        },
        RpcError {
          code:    73,
          message: "Custom failure".into(),
          details: Some(serde_json::json!({"reason": "fixture"})),
        },
      ],
      [
        (-32700, "Parse error"),
        (-32600, "Invalid request"),
        (-32601, "Method not found"),
        (-32602, "Invalid params"),
        (-32603, "Internal error"),
        (-32002, "Server not initialized"),
        (-32800, "Request cancelled"),
        (-32801, "Content modified"),
      ]
      .map(|(code, message)| RpcError {
        code,
        message: message.into(),
        details: None,
      }),
    );
    ensure_eq(
      observed,
      expected,
      "constructors must preserve complete response channels and standard or customized RPC errors",
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Native reserved-range constructor results at both boundary polarities.
  type ServerCodes = [Result<RpcError, InvalidServerCode>; 4];

  #[test]
  fn server_error_codes_accept_only_the_reserved_range() -> Result<(), Box<ComparisonFailure<ServerCodes, ServerCodes>>> {
    ensure_eq(
      [
        RpcError::server(-32099),
        RpcError::server(-32000),
        RpcError::server(-32100),
        RpcError::server(-31999),
      ],
      [
        Ok(RpcError {
          code:    -32099,
          message: "Server error".into(),
          details: None,
        }),
        Ok(RpcError {
          code:    -32000,
          message: "Server error".into(),
          details: None,
        }),
        Err(InvalidServerCode {
          code: -32100
        }),
        Err(InvalidServerCode {
          code: -31999
        }),
      ],
      "server-code constructors must preserve accepted and rejected range boundaries",
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Original RPC error, rendered description, and native wire serialization.
  type ErrorContract = (RpcError, String, Result<serde_json::Value, serde_json::Error>);

  #[test]
  fn rpc_errors_implement_the_standard_error_contract_without_fabricated_sources() -> Result<(), Box<PredicateFailure<ErrorContract>>> {
    let error = RpcError::internal_error().with_details("diagnostic detail");
    let rendered = error.to_string();
    let serialized = serde_json::to_value(&error);
    ensure_that(
      (error, rendered, serialized),
      "RPC errors must preserve their description and data member without inventing a causal source",
      |observed| {
        let (ref native, ref description, ref wire) = *observed;
        description == "RPC error (-32603): Internal error"
          && wire
            .as_ref()
            .is_ok_and(|value| value.get("data") == Some(&serde_json::json!("diagnostic detail")) && value.get("details").is_none())
          && StdError::source(native).is_none()
      },
    )
    .map(drop)
    .map_err(Box::new)
  }
}
