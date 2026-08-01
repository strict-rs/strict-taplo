//! Serde serialization and detached-value deserialization for the semantic DOM.
//!
//! Source diagnostics reject serialization rather than being erased. Deserialization accepts
//! only value shapes representable by TOML and constructs immutable detached wrappers.

use core::fmt;
use core::fmt::Formatter;
use std::collections::VecDeque;
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use serde::Serializer;
use serde::de::Error as DeserializeError;
use serde::de::MapAccess;
use serde::de::SeqAccess;
use serde::de::Unexpected;
use serde::de::Visitor;
use serde::ser::Error;
use serde::ser::SerializeMap as _;
use serde::ser::SerializeSeq as _;

use super::node::ArenaEntries;
use super::node::ArenaEntry;
use super::node::ArrayInner;
use super::node::ArrayKind;
use super::node::BoolInner;
use super::node::DomArena;
use super::node::FloatInner;
use super::node::IntegerInner;
use super::node::IntegerValue;
use super::node::Node;
use super::node::NodeId as ArenaNodeId;
use super::node::NodeSeed;
use super::node::StrInner;
use super::node::TableInner;
use super::node::TableKind;
use crate::dom::node::Key;

impl Serialize for Node {
  fn serialize<S>(&self, ser: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    if !self.errors().is_empty() {
      return Err(Error::custom("a node with semantic diagnostics cannot be serialized"));
    }

    match *self {
      Self::Table(ref table) => {
        let entries = table.entries();
        let mut map = ser.serialize_map(Some(entries.len()))?;

        if entries.iter().any(|(key, _)| !key.errors().is_empty()) {
          return Err(Error::custom("a key with semantic diagnostics cannot be serialized"));
        }
        for (key, entry) in &entries {
          map.serialize_entry(key.value(), &entry)?;
        }

        map.end()
      }
      Self::Array(ref array) => {
        let items = array.items();
        let mut seq = ser.serialize_seq(Some(items.len()))?;
        for element in &items {
          seq.serialize_element(&element)?;
        }
        seq.end()
      }
      Self::Bool(ref boolean) => ser.serialize_bool(boolean.value()),
      Self::Str(ref string) => ser.serialize_str(string.value()),
      Self::Integer(ref integer) => match integer.value() {
        IntegerValue::Negative(number) => ser.serialize_i64(number),
        IntegerValue::Positive(number) => ser.serialize_u64(number),
      },
      Self::Float(ref float) => ser.serialize_f64(float.value()),
      Self::Date(ref date_time) => ser.serialize_str(&date_time.value().to_string()),
      Self::Invalid(_) => Err(Error::custom("invalid node cannot be serialized")),
    }
  }
}

/// Detached TOML-compatible value collected before one iterative arena publication.
enum DetachedNode {
  /// Boolean value.
  Bool(bool),
  /// Signedness-preserving integer.
  Integer(IntegerValue),
  /// Floating-point value.
  Float(f64),
  /// String value.
  String(Arc<str>),
  /// Ordered array values.
  Array(Vec<Self>),
  /// Ordered table entries.
  Table(Vec<(Key, Self)>),
}

/// One detached arena record whose child identities are already allocated.
enum DetachedRecord {
  /// Boolean record.
  Bool(bool),
  /// Integer record.
  Integer(IntegerValue),
  /// Floating-point record.
  Float(f64),
  /// String record.
  String(Arc<str>),
  /// Array record.
  Array {
    /// Semantic array representation.
    kind:  ArrayKind,
    /// Child record identities.
    items: Vec<ArenaNodeId>,
  },
  /// Table record.
  Table(Vec<(Key, ArenaNodeId)>),
}

/// Preallocated detached records awaiting direct immutable publication.
struct DetachedPublication {
  /// Root arena identity.
  root:    ArenaNodeId,
  /// Every identity and record in parent-before-child construction order.
  records: Vec<(ArenaNodeId, DetachedRecord)>,
}

#[derive(Clone, Copy)]
/// Serde visitor that constructs detached TOML-compatible intermediate values.
struct TomlVisitor;

/// Generate one infallible scalar visitor over the detached value model.
macro_rules! visit_detached_scalar {
  ($method:ident, $value:ident : $value_type:ty => $detached:expr) => {
    fn $method<E>(self, $value: $value_type) -> Result<Self::Value, E>
    where
      E: DeserializeError,
    {
      Ok($detached)
    }
  };
}

impl<'de> Visitor<'de> for TomlVisitor {
  type Value = DetachedNode;

  fn expecting(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    write!(formatter, "a TOML value")
  }

  visit_detached_scalar!(visit_bool, boolean: bool => DetachedNode::Bool(boolean));

  fn visit_i64<E>(self, integer: i64) -> Result<Self::Value, E>
  where
    E: DeserializeError,
  {
    Ok(DetachedNode::Integer(if integer.is_negative() {
      IntegerValue::Negative(integer)
    } else {
      IntegerValue::Positive(u64::try_from(integer).map_err(E::custom)?)
    }))
  }

  visit_detached_scalar!(visit_u64, integer: u64 => DetachedNode::Integer(IntegerValue::Positive(integer)));
  visit_detached_scalar!(visit_f64, float: f64 => DetachedNode::Float(float));
  visit_detached_scalar!(visit_str, text: &str => DetachedNode::String(Arc::from(text)));

  fn visit_bytes<E>(self, bytes: &[u8]) -> Result<Self::Value, E>
  where
    E: DeserializeError,
  {
    Err(DeserializeError::invalid_type(Unexpected::Bytes(bytes), &self))
  }

  fn visit_none<E>(self) -> Result<Self::Value, E>
  where
    E: DeserializeError,
  {
    Err(DeserializeError::invalid_type(Unexpected::Option, &self))
  }

  fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
  where
    D: serde::Deserializer<'de>,
  {
    deserializer.deserialize_any(self)
  }

  fn visit_unit<E>(self) -> Result<Self::Value, E>
  where
    E: DeserializeError,
  {
    Err(DeserializeError::invalid_type(Unexpected::Unit, &self))
  }

  fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
  where
    D: serde::Deserializer<'de>,
  {
    deserializer.deserialize_any(self)
  }

  fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
  where
    A: SeqAccess<'de>,
  {
    let mut items = Vec::new();
    while let Some(node) = seq.next_element::<DetachedNode>()? {
      items.push(node);
    }
    Ok(DetachedNode::Array(items))
  }

  fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
  where
    A: MapAccess<'de>,
  {
    let mut entries = Vec::new();
    while let Some((key, node)) = map.next_entry::<String, DetachedNode>()? {
      entries.push((Key::new(key), node));
    }
    Ok(DetachedNode::Table(entries))
  }
}

impl<'de> Deserialize<'de> for DetachedNode {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: serde::Deserializer<'de>,
  {
    deserializer.deserialize_any(TomlVisitor)
  }
}

/// Allocate detached records iteratively so publication never follows the host call stack.
fn detached_records(root: DetachedNode) -> Result<DetachedPublication, &'static str> {
  let root_id = ArenaNodeId::pending(0);
  let mut next_index = 1_usize;
  let mut records = Vec::new();
  let mut pending = Vec::from([(root_id.clone(), root)]);
  while let Some((id, node)) = pending.pop() {
    let record = match node {
      DetachedNode::Bool(boolean) => DetachedRecord::Bool(boolean),
      DetachedNode::Integer(integer) => DetachedRecord::Integer(integer),
      DetachedNode::Float(float) => DetachedRecord::Float(float),
      DetachedNode::String(string) => DetachedRecord::String(string),
      DetachedNode::Array(items) => {
        let kind = if !items.is_empty() && items.iter().all(|element| matches!(element, DetachedNode::Table(_))) {
          ArrayKind::Tables
        } else {
          ArrayKind::Inline
        };
        let mut item_ids = Vec::with_capacity(items.len());
        let mut item_tasks = Vec::with_capacity(items.len());
        for element in items {
          let item_id = ArenaNodeId::pending(next_index);
          next_index = next_index.checked_add(1).ok_or("detached arena identity space is exhausted")?;
          item_ids.push(item_id.clone());
          item_tasks.push((item_id, element));
        }
        pending.extend(item_tasks.into_iter().rev());
        DetachedRecord::Array {
          kind,
          items: item_ids,
        }
      }
      DetachedNode::Table(entries) => {
        let mut entry_ids = Vec::with_capacity(entries.len());
        let mut entry_tasks = Vec::with_capacity(entries.len());
        for (key, entry) in entries {
          let entry_id = ArenaNodeId::pending(next_index);
          next_index = next_index.checked_add(1).ok_or("detached arena identity space is exhausted")?;
          entry_ids.push((key, entry_id.clone()));
          entry_tasks.push((entry_id, entry));
        }
        pending.extend(entry_tasks.into_iter().rev());
        DetachedRecord::Table(entry_ids)
      }
    };
    records.push((id, record));
  }
  Ok(DetachedPublication {
    root: root_id,
    records,
  })
}

/// Convert one detached record into its immutable arena representation.
fn publish_detached_record(detached_record: DetachedRecord) -> NodeSeed {
  match detached_record {
    DetachedRecord::Bool(boolean) => NodeSeed::Bool {
      inner: Arc::new(BoolInner {
        diagnostics: Arc::default(),
        syntax:      None,
        value:       boolean,
      }),
    },
    DetachedRecord::Integer(integer) => NodeSeed::Integer {
      inner: Arc::new(IntegerInner {
        diagnostics: Arc::default(),
        syntax:      None,
        repr:        super::node::IntegerRepr::Dec,
        value:       integer,
      }),
    },
    DetachedRecord::Float(float) => NodeSeed::Float {
      inner: Arc::new(FloatInner {
        diagnostics: Arc::default(),
        syntax:      None,
        value:       float,
      }),
    },
    DetachedRecord::String(string) => NodeSeed::Str {
      inner: Arc::new(StrInner {
        diagnostics: Arc::default(),
        syntax:      None,
        value:       string,
      }),
    },
    DetachedRecord::Array {
      kind,
      items,
    } => NodeSeed::Array {
      inner: Arc::new(ArrayInner {
        diagnostics: Arc::default(),
        syntax: None,
        kind,
      }),
      items: items.into(),
    },
    DetachedRecord::Table(table_entries) => {
      let all = table_entries
        .into_iter()
        .map(|(key, node)| ArenaEntry {
          key,
          node,
        })
        .collect::<Vec<_>>();
      let lookup = all
        .iter()
        .enumerate()
        .map(|(entry_index, entry)| (entry.key.clone(), entry_index))
        .collect();
      NodeSeed::Table {
        inner:   Arc::new(TableInner {
          diagnostics: Arc::default(),
          syntax:      None,
          kind:        TableKind::Regular,
        }),
        entries: Arc::new(ArenaEntries {
          lookup,
          all: all.into(),
        }),
      }
    }
  }
}

/// Publish detached records directly into one immutable arena.
fn publish_detached(publication: DetachedPublication) -> Node {
  for (id, record) in publication.records {
    id.initialize(publish_detached_record(record));
  }
  DomArena::publish(&publication.root, Arc::default(), VecDeque::new())
}

impl<'de> Deserialize<'de> for Node {
  fn deserialize<D>(de: D) -> Result<Self, D::Error>
  where
    D: serde::Deserializer<'de>,
  {
    let detached = DetachedNode::deserialize(de)?;
    Ok(publish_detached(detached_records(detached).map_err(D::Error::custom)?))
  }
}

#[cfg(test)]
/// Serde boundary tests for supported detached values and rejected non-TOML shapes.
mod tests {
  use serde::de::Visitor as _;
  use serde::de::value::Error as ValueError;
  use serde::de::value::StrDeserializer;
  use serde_json::json;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;

  use super::Node;
  use super::TomlVisitor;
  use crate::parser::parse;

  /// Round-trip supported JSON shapes and render detached values as valid TOML.
  #[test]
  fn detached_json_values_round_trip_and_render_valid_toml() -> Result<(), TestFailure> {
    let expected = json!({
      "can't\n": true,
      "count": 3,
      "nested": {
        "items": ["one", "two"],
      },
    });
    let node = ensure_ok(
      serde_json::from_value::<Node>(expected.clone()),
      "supported JSON values must construct a detached DOM",
    )?;
    let observed = ensure_ok(serde_json::to_value(&node), "a valid detached DOM must serialize back to JSON")?;
    ensure_eq(
      &observed,
      &expected,
      "detached JSON conversion must preserve every supported value and key",
    )?;
    let rendered = ensure_ok(node.to_toml(false, false), "a detached DOM with a non-bare key must render")?;
    ensure(
      rendered.contains("\"can't\\n\" = true"),
      "a detached key containing a newline must use valid escaped basic-key syntax",
    )
  }

  /// Reject serialization whenever malformed source or conflicts make semantics ambiguous.
  #[test]
  fn invalid_and_conflicting_source_cannot_serialize() -> Result<(), TestFailure> {
    let malformed = ensure_ok(
      parse("value = 999999999999999999999999999999\n"),
      "the malformed scalar fixture tree must construct",
    )?
    .into_dom();
    ensure(
      serde_json::to_value(malformed).is_err(),
      "a malformed semantic scalar must not serialize with a fabricated value",
    )?;

    let conflicting = ensure_ok(parse("value = 1\nvalue = 2\n"), "the conflicting-key fixture tree must construct")?.into_dom();
    ensure(
      serde_json::to_value(conflicting).is_err(),
      "conflicting semantic keys must not serialize as an ambiguous object",
    )
  }

  /// Reject JSON null because TOML has no equivalent semantic value.
  #[test]
  fn unsupported_json_null_is_rejected() -> Result<(), TestFailure> {
    ensure(
      serde_json::from_value::<Node>(serde_json::Value::Null).is_err(),
      "JSON null has no TOML semantic value and must be rejected",
    )
  }

  /// Report enum input through Serde's default typed visitor rejection.
  #[test]
  fn enum_input_reports_its_type_and_the_toml_expectation() -> Result<(), TestFailure> {
    let error = ensure_some(
      TomlVisitor.visit_enum(StrDeserializer::<ValueError>::new("Variant")).err(),
      "Serde enum access must be rejected by the TOML visitor",
    )?;
    ensure_eq(
      &error.to_string(),
      &"invalid type: enum, expected a TOML value".to_owned(),
      "the default visitor contract must identify both the unsupported input and expected TOML domain",
    )
  }
}
