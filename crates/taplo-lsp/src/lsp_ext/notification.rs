use lsp_types::notification::Notification;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use url::Url;

/// Notification emitted when a message should also be written to the client's output channel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageWithOutput {}

/// Severity of a message written to the client's output channel.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageKind {
  /// Informational message.
  Info,
  /// Warning message.
  Warn,
  /// Error message.
  Error,
}

/// Parameters for [`MessageWithOutput`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageWithOutputParams {
  /// Severity of the message.
  pub kind:    MessageKind,
  /// Human-readable message text.
  pub message: String,
}

impl Notification for MessageWithOutput {
  type Params = MessageWithOutputParams;
  const METHOD: &'static str = "taplo/messageWithOutput";
}

/// Rule used to associate a schema with documents.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AssociationRule {
  /// Glob pattern matched against document paths.
  Glob(String),
  /// Regular expression matched against document identifiers.
  Regex(String),
  /// Exact document URL.
  Url(Url),
}

/// Notification that installs a schema association.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssociateSchema {}

/// Parameters for [`AssociateSchema`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssociateSchemaParams {
  /// Optional document that owns the association.
  pub document_uri: Option<Url>,
  /// Schema URL to associate.
  pub schema_uri:   Url,
  /// Matching rule for the association.
  pub rule:         AssociationRule,
  /// Optional precedence used when multiple rules match.
  pub priority:     Option<usize>,
  /// Optional client-defined schema metadata.
  pub meta:         Option<Value>,
}

impl Notification for AssociateSchema {
  type Params = AssociateSchemaParams;
  const METHOD: &'static str = "taplo/associateSchema";
}

/// Notification emitted when a document's effective schema association changes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DidChangeSchemaAssociation {}

/// Parameters for [`DidChangeSchemaAssociation`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DidChangeSchemaAssociationParams {
  /// Document whose association changed.
  pub document_uri: Url,
  /// Newly associated schema, or `None` when the association was removed.
  pub schema_uri:   Option<Url>,
  /// Optional metadata attached to the effective association.
  pub meta:         Option<Value>,
}

impl Notification for DidChangeSchemaAssociation {
  type Params = DidChangeSchemaAssociationParams;
  const METHOD: &'static str = "taplo/didChangeSchemaAssociation";
}
