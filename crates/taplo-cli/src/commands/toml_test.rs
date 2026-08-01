use serde::Serialize;
use serde::ser::Error as _;
use serde::ser::SerializeMap as _;
use serde::ser::SerializeSeq as _;
use taplo::dom::Node;
use taplo::dom::node::DateTimeValue;
use taplo::parser;
use taplo_common::environment::LocalEnvironment;
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;

use crate::CliError;
use crate::CliFailure;
use crate::LocalCommandFuture;
use crate::Taplo;

/// Decode TOML-test input and emit its canonical JSON representation.
pub(super) fn execute_toml_test<E: LocalEnvironment>(taplo: &Taplo<E>) -> LocalCommandFuture<'_, Result<(), CliError>> {
  Box::pin(async move {
    let mut source = String::new();
    let bytes_read = taplo.env.stdin().read_to_string(&mut source).await?;
    tracing::trace!(bytes_read, "read TOML-test input from standard input");

    let parse = parser::parse(&source)?;

    if !parse.diagnostics().is_empty() {
      let diagnostics = parse
        .diagnostics()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
      taplo.env.stderr().write_all(diagnostics.as_bytes()).await?;
      return Err(CliFailure::InvalidTomlTestInput.into());
    }
    let dom = parse.into_dom();

    if let Err(errors) = dom.validate() {
      let diagnostics = errors.into_iter().map(|error| error.to_string()).collect::<Vec<_>>().join("\n");
      taplo.env.stderr().write_all(diagnostics.as_bytes()).await?;
      return Err(CliFailure::InvalidTomlTestInput.into());
    }

    let output = serde_json::to_vec(&TomlTestValue::new(&dom))?;
    let mut stdout = taplo.env.stdout();
    stdout.write_all(&output).await?;
    stdout.flush().await?;

    Ok(())
  })
}

/// Scalar type labels required by the TOML conformance JSON format.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum TomlTestType {
  /// String scalar.
  String,
  /// Integer scalar.
  Integer,
  /// Floating-point scalar.
  Float,
  /// Boolean scalar.
  Bool,
  /// Offset date-time scalar.
  DateTime,
  /// Local date-time scalar.
  #[serde(rename = "datetime-local")]
  DateTimeLocal,
  /// Local date scalar.
  #[serde(rename = "date-local")]
  DateLocal,
  /// Local time scalar.
  #[serde(rename = "time-local")]
  TimeLocal,
}

impl TomlTestType {
  /// Classify one scalar node, returning `None` for containers and invalid nodes.
  fn of(node: &Node) -> Option<Self> {
    match *node {
      Node::Bool(_) => Some(Self::Bool),
      Node::Integer(_) => Some(Self::Integer),
      Node::Float(_) => Some(Self::Float),
      Node::Str(_) => Some(Self::String),
      Node::Date(ref date_value) => match date_value.value() {
        DateTimeValue::OffsetDateTime(_) => Some(Self::DateTime),
        DateTimeValue::LocalDateTime(_) => Some(Self::DateTimeLocal),
        DateTimeValue::Date(_) => Some(Self::DateLocal),
        DateTimeValue::Time(_) => Some(Self::TimeLocal),
      },
      Node::Array(_) | Node::Table(_) | Node::Invalid(_) => None,
    }
  }
}

/// Borrowed DOM node serialized through the TOML conformance JSON contract.
#[derive(Debug)]
struct TomlTestValue<'a> {
  /// Scalar label, absent for containers and invalid nodes.
  r#type: Option<TomlTestType>,
  /// Semantic node being serialized.
  node:   &'a Node,
}

impl<'a> TomlTestValue<'a> {
  /// Wrap one DOM node in the TOML conformance JSON representation.
  fn new(node: &'a Node) -> Self {
    Self {
      r#type: TomlTestType::of(node),
      node,
    }
  }
}

impl Serialize for TomlTestValue<'_> {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: serde::Serializer,
  {
    match self.r#type {
      Some(scalar_type) => self.serialize_scalar(serializer, scalar_type),
      None => self.serialize_container(serializer),
    }
  }
}

impl TomlTestValue<'_> {
  /// Serialize one scalar through the TOML-test type/value envelope.
  fn serialize_scalar<S>(&self, serializer: S, scalar_type: TomlTestType) -> Result<S::Ok, S::Error>
  where
    S: serde::Serializer,
  {
    let mut map = serializer.serialize_map(Some(2))?;
    map.serialize_entry("type", &scalar_type)?;
    let scalar_value = match *self.node {
      Node::Str(ref string) => string.value().to_owned(),
      Node::Float(ref float_value) if float_value.value().is_nan() => String::from("nan"),
      Node::Float(ref float_value) if float_value.value().is_infinite() => float_value
        .syntax()
        .map_or_else(|| float_value.value().to_string(), ToString::to_string),
      Node::Bool(_) | Node::Integer(_) | Node::Float(_) | Node::Date(_) => serde_json::to_string(&self.node).map_err(S::Error::custom)?,
      Node::Table(_) | Node::Array(_) | Node::Invalid(_) => {
        return Err(S::Error::custom("a scalar TOML test label requires a scalar node"));
      }
    };
    map.serialize_entry("value", &scalar_value)?;
    map.end()
  }

  /// Serialize one container recursively through TOML-test values.
  fn serialize_container<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: serde::Serializer,
  {
    match *self.node {
      Node::Array(ref array) => {
        let items = array.items();
        let mut sequence = serializer.serialize_seq(Some(items.len()))?;
        for child_node in items {
          sequence.serialize_element(&TomlTestValue::new(&child_node))?;
        }
        sequence.end()
      }
      Node::Table(ref table) => {
        let entries = table.entries();
        let mut map = serializer.serialize_map(Some(entries.len()))?;
        for (key, child_node) in &entries {
          map.serialize_entry(key.value(), &TomlTestValue::new(&child_node))?;
        }
        map.end()
      }
      Node::Invalid(_) => Err(S::Error::custom("cannot serialize an invalid TOML node")),
      Node::Bool(_) | Node::Str(_) | Node::Integer(_) | Node::Float(_) | Node::Date(_) => {
        Err(S::Error::custom("unexpected scalar TOML test value"))
      }
    }
  }
}
