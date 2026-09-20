//! Schema-backed completion classification and focused completion construction.

use std::borrow::Cow;
use std::sync::Arc;

use lsp_types::CompletionItem;
use lsp_types::CompletionItemKind;
use lsp_types::CompletionParams;
use lsp_types::CompletionResponse;
use lsp_types::CompletionTextEdit;
use lsp_types::Documentation;
use lsp_types::InsertTextFormat;
use lsp_types::MarkupContent;
use lsp_types::MarkupKind;
use lsp_types::Range;
use lsp_types::TextEdit;
use serde_json::Value;
use taplo::dom::Keys;
use taplo::dom::Node;
use taplo::dom::RenderError;
use taplo::dom::node::IntegerValue;
use taplo::dom::node::TableKind;
use taplo::rowan::TextRange;
use taplo_common::schema::ValueExt as _;
use taplo_common::schema::ext::SchemaExtensionError;
use taplo_common::schema::ext::schema_ext_of;
use taplo_lsp_async::Params;
use taplo_lsp_async::rpc::RpcError;
use taplo_lsp_async::util::Mapper;
use thiserror::Error as ThisError;
use url::Url;

use crate::query::Query;
use crate::query::lookup_keys;
use crate::world::DocumentSnapshot;
use crate::world::SchemaExecution as _;
use crate::world::WorldState;

/// A failed completion projection from schema data.
#[derive(Debug, ThisError)]
enum CompletionError {
  /// Taplo-specific schema metadata is malformed.
  #[error(transparent)]
  SchemaExtension(#[from] SchemaExtensionError),
  /// A JSON Schema value cannot be decoded as a TOML semantic value.
  #[error("schema completion value cannot be represented as TOML")]
  Decode {
    /// Underlying semantic decoding failure.
    #[source]
    source: serde_json::Error,
  },
  /// A decoded TOML semantic value cannot be encoded for lossless verification.
  #[error("schema completion value cannot be serialized for lossless verification")]
  Encode {
    /// Underlying semantic encoding failure.
    #[source]
    source: serde_json::Error,
  },
  /// Decoding a schema completion value would change its meaning.
  #[error("schema completion value loses data during TOML conversion")]
  Lossy {
    /// Original JSON Schema value.
    original:   Box<Value>,
    /// Value produced by the TOML semantic round trip.
    round_trip: Box<Value>,
  },
  /// A decoded semantic value cannot be rendered as TOML.
  #[error(transparent)]
  Render(#[from] RenderError),
}

impl From<CompletionError> for RpcError {
  fn from(error: CompletionError) -> Self {
    Self::internal_error().with_details(error.to_string())
  }
}

/// Semantic completion operation selected once from cursor-relative syntax and DOM state.
#[derive(Clone, Debug, Eq, PartialEq)]
enum CompletionTarget {
  /// Replace or extend one standard table header.
  TableHeader {
    /// Number of header segments already present.
    prefix_length: usize,
    /// Optional source range occupied by the current header key.
    replacement:   Option<Range>,
    /// Semantic node currently represented by the header.
    current_path:  Keys,
  },
  /// Replace or extend one array-of-tables header.
  ArrayTableHeader {
    /// Number of header segments already present.
    prefix_length: usize,
    /// Optional source range occupied by the current header key.
    replacement:   Option<Range>,
  },
  /// Insert a new entry on a trivia-only line.
  EmptyLine {
    /// Schema lookup path of the containing table.
    lookup_path: Keys,
  },
  /// Complete an entry key that may already have an equals sign.
  EntryKey {
    /// Schema lookup path of the key's semantic parent.
    lookup_path:   Keys,
    /// Number of key segments already present.
    prefix_length: usize,
    /// Optional source range occupied by the current key.
    replacement:   Option<Range>,
    /// Whether the entry already owns an equals sign and value boundary.
    has_equals:    bool,
  },
  /// Insert another entry inside an inline table.
  InlineTableEntry {
    /// Schema lookup path of the inline table.
    lookup_path: Keys,
  },
  /// Complete the value of one entry or array item.
  EntryValue {
    /// Schema lookup path of the value.
    lookup_path:  Keys,
    /// Optional source range replaced by the selected value.
    replacement:  Option<Range>,
    /// Whether literal-string quoting should be preferred.
    single_quote: bool,
  },
  /// Replace an incomplete standalone key with a complete entry snippet.
  StandaloneKey {
    /// Schema lookup path of the key's semantic parent.
    lookup_path: Keys,
    /// Optional source range occupied by the incomplete key.
    replacement: Option<Range>,
  },
}

/// One iterative work item for the completion-specific semantic projection.
enum CompletionInstanceTask {
  /// Project one semantic DOM node.
  Visit(Node),
  /// Assemble one table from projected children in source order.
  FinishTable(Vec<String>),
  /// Assemble one array while preserving this many child positions.
  FinishArray(usize),
}

/// Project a tolerant semantic DOM into the JSON instance used by schema traversal.
///
/// Invalid table values are omitted because they have no trustworthy property value. Invalid or
/// non-finite array values become JSON `null` so every later array index remains stable.
fn completion_instance(root: &Node) -> Value {
  let mut tasks = vec![CompletionInstanceTask::Visit(root.clone())];
  let mut values = Vec::new();

  while let Some(task) = tasks.pop() {
    match task {
      CompletionInstanceTask::Visit(node) => match node {
        Node::Table(table) => {
          let entries = table
            .entries()
            .iter()
            .filter(|entry| entry.0.errors().is_empty())
            .map(|(key, child)| (key.value().to_owned(), child))
            .collect::<Vec<_>>();
          let keys = entries.iter().map(|entry| entry.0.clone()).collect();
          tasks.push(CompletionInstanceTask::FinishTable(keys));
          tasks.extend(entries.into_iter().rev().map(|entry| CompletionInstanceTask::Visit(entry.1)));
        }
        Node::Array(array) => {
          let items = array.items().iter().collect::<Vec<_>>();
          tasks.push(CompletionInstanceTask::FinishArray(items.len()));
          tasks.extend(items.into_iter().rev().map(CompletionInstanceTask::Visit));
        }
        Node::Bool(boolean) => values.push(Some(Value::Bool(boolean.value()))),
        Node::Str(string) => values.push(Some(Value::String(string.value().to_owned()))),
        Node::Integer(integer) => values.push(Some(Value::Number(match integer.value() {
          IntegerValue::Negative(negative) => negative.into(),
          IntegerValue::Positive(positive) => positive.into(),
        }))),
        Node::Float(float) => values.push(serde_json::Number::from_f64(float.value()).map(Value::Number)),
        Node::Date(date_time) => values.push(Some(Value::String(date_time.value().to_string()))),
        Node::Invalid(_) => values.push(None),
      },
      CompletionInstanceTask::FinishTable(keys) => {
        let mut projected = Vec::with_capacity(keys.len());
        for key in keys.into_iter().rev() {
          projected.push((key, values.pop().flatten()));
        }
        let table = projected
          .into_iter()
          .rev()
          .filter_map(|(key, projected_value)| Some((key, projected_value?)))
          .collect::<serde_json::Map<_, _>>();
        values.push(Some(Value::Object(table)));
      }
      CompletionInstanceTask::FinishArray(item_count) => {
        let mut array = Vec::with_capacity(item_count);
        for _ in 0..item_count {
          array.push(values.pop().flatten().unwrap_or(Value::Null));
        }
        array.reverse();
        values.push(Some(Value::Array(array)));
      }
    }
  }

  values.pop().flatten().unwrap_or(Value::Null)
}

impl CompletionTarget {
  /// Classify one cursor query into a complete semantic completion operation.
  fn classify(query: &Query, root: &Node, mapper: &Mapper) -> Result<Option<Self>, RpcError> {
    if query.in_table_header() {
      return Ok(Some(Self::TableHeader {
        prefix_length: query.header_keys().len(),
        replacement:   project_nonempty_range(mapper, query.header_key().map(|key| key.text_range()))?,
        current_path:  query.dom_node().map_or_else(Keys::empty, |node_entry| node_entry.0.clone()),
      }));
    }

    if query.in_table_array_header() {
      return Ok(Some(Self::ArrayTableHeader {
        prefix_length: query.header_keys().len(),
        replacement:   project_nonempty_range(mapper, query.header_key().map(|key| key.text_range()))?,
      }));
    }

    if query.empty_line() {
      let parent = query.parent_table_or_array_table(root);
      return Ok(Some(Self::EmptyLine {
        lookup_path: lookup_keys(root.clone(), &parent.0),
      }));
    }

    if query.in_entry_keys() {
      let mut parent_path = query
        .dom_node()
        .map_or_else(|| query.parent_table_or_array_table(root).0, |node_entry| node_entry.0.clone());
      let entry_keys = query.entry_keys();
      parent_path = parent_path.skip_right(entry_keys.len());
      return Ok(Some(Self::EntryKey {
        lookup_path:   lookup_keys(root.clone(), &parent_path),
        prefix_length: entry_keys.len(),
        replacement:   project_nonempty_range(mapper, query.entry_key().map(|key| key.text_range()))?,
        has_equals:    query.entry_has_eq(),
      }));
    }

    if query.in_entry_value() {
      let Some(node_entry) = query.dom_node() else {
        return Ok(None);
      };
      let path = &node_entry.0;
      if query.in_inline_table() {
        return Ok(Some(Self::InlineTableEntry {
          lookup_path: lookup_keys(root.clone(), path),
        }));
      }

      let lookup_path = if query.is_inline() {
        lookup_keys(root.clone(), path)
      } else {
        let parent = query.parent_table_or_array_table(root);
        lookup_keys(root.clone(), &parent.0.extend(query.entry_keys()))
      };
      let replacement = if query.in_array() {
        None
      } else {
        project_range(mapper, query.entry_value().map(|value_syntax| value_syntax.text_range()))?
      };
      return Ok(Some(Self::EntryValue {
        lookup_path,
        replacement,
        single_quote: query.is_single_quote_value(),
      }));
    }

    let mut parent_path = query
      .dom_node()
      .map_or_else(|| query.parent_table_or_array_table(root).0, |node_entry| node_entry.0.clone());
    let entry_keys = query.entry_keys();
    parent_path = parent_path.skip_right(entry_keys.len());
    Ok(Some(Self::StandaloneKey {
      lookup_path: lookup_keys(root.clone(), &parent_path),
      replacement: project_range(mapper, entry_keys.all_text_range())?,
    }))
  }
}

/// Project one optional syntax range into checked LSP coordinates.
fn project_range(mapper: &Mapper, range: Option<TextRange>) -> Result<Option<Range>, RpcError> {
  range
    .map(|source_range| super::uri::to_lsp_range(mapper, source_range))
    .transpose()
    .map_err(|error| super::uri::mapping_rpc_error(&error))
}

/// Project one optional nonempty syntax range into checked LSP coordinates.
fn project_nonempty_range(mapper: &Mapper, range: Option<TextRange>) -> Result<Option<Range>, RpcError> {
  project_range(mapper, range.filter(|source_range| !source_range.is_empty()))
}

/// Produce schema-backed completion items for one current document snapshot.
///
/// # Errors
///
/// Returns an RPC error when request parameters, source coordinates, schema traversal, completion
/// rendering, or snapshot freshness cannot be validated.
macro_rules! define_completion_future_family {
  (
    $completion:ident,
    $complete_target:ident,
    $completion_schemas:ident,
    $document_snapshot_for_uri:ident,
    $current_snapshot_response:ident;
    $future:ident,
    $environment:path,
    $transport:ident,
    $schema_execution:path;
    [$($value_bound:path),*]
  ) => {
    /// Produce schema-backed completion items for one current document snapshot.
    ///
    /// # Errors
    ///
    /// Returns an RPC error when request parameters, source coordinates, schema traversal,
    /// completion rendering, or snapshot freshness cannot be validated.
    #[allow(clippy::single_call_fn, reason = "one completion entry point per execution family, registered exactly once by its runtime family")]
    pub(super) fn $completion<E: $environment>(
      world: &WorldState<E, $transport<E>>,
      params: Params<CompletionParams>,
    ) -> $future<'_, Result<Option<CompletionResponse>, RpcError>> {
      Box::pin(async move {
        let parameters = params.required()?;

        current_document_snapshot!(
          super::$document_snapshot_for_uri(world, &parameters.text_document_position.text_document.uri) => (document_uri, snapshot)
        );

        if !snapshot.config.schema.enabled {
          return super::$current_snapshot_response(world, &document_uri, &snapshot, None).await;
        }

        let document = &snapshot.document;
        let Some(schema_association) = snapshot.schemas.associations().association_for(&document_uri) else {
          return super::$current_snapshot_response(world, &document_uri, &snapshot, None).await;
        };

        let offset = document
          .mapper
          .offset(parameters.text_document_position.position)
          .map_err(|error| super::uri::mapping_rpc_error(&error))?;
        let query = Query::at(&document.dom, offset);
        let Some(target) = CompletionTarget::classify(&query, &document.dom, &document.mapper)? else {
          return super::$current_snapshot_response(world, &document_uri, &snapshot, None).await;
        };
        let instance = completion_instance(&document.dom);
        let completions = $complete_target(&snapshot, &schema_association.url, &instance, &target).await?;
        super::$current_snapshot_response(world, &document_uri, &snapshot, Some(CompletionResponse::Array(completions))).await
      })
    }

    /// Build completions for one already-classified semantic target.
    #[allow(clippy::single_call_fn, reason = "the named step keeps the exhaustive `CompletionTarget` dispatch separate from the snapshot, association, and cursor classification that precedes it")]
    fn $complete_target<'operation, E: $environment>(
      snapshot: &'operation DocumentSnapshot<$transport<E>>,
      schema_url: &'operation Url,
      instance: &'operation Value,
      target: &'operation CompletionTarget,
    ) -> $future<'operation, Result<Vec<CompletionItem>, RpcError>> {
      Box::pin(async move {
        match *target {
          CompletionTarget::TableHeader {
            prefix_length,
            ref replacement,
            ref current_path,
          } => {
            let schemas = $completion_schemas(snapshot, schema_url, instance, &Keys::empty(), prefix_length).await?;
            table_header_completions(&snapshot.document.dom, schemas, replacement.as_ref(), current_path)
          }
          CompletionTarget::ArrayTableHeader {
            prefix_length,
            ref replacement,
          } => {
            let schemas = $completion_schemas(snapshot, schema_url, instance, &Keys::empty(), prefix_length).await?;
            array_table_header_completions(schemas, replacement.as_ref())
          }
          CompletionTarget::EmptyLine {
            ref lookup_path,
          }
          | CompletionTarget::InlineTableEntry {
            ref lookup_path,
          } => {
            let schemas = $completion_schemas(snapshot, schema_url, instance, lookup_path, 0).await?;
            new_entry_completions(&snapshot.document.dom, schemas)
          }
          CompletionTarget::EntryKey {
            ref lookup_path,
            prefix_length,
            ref replacement,
            has_equals,
          } => {
            let schemas = $completion_schemas(snapshot, schema_url, instance, lookup_path, prefix_length).await?;
            entry_key_completions(schemas, replacement.as_ref(), has_equals)
          }
          CompletionTarget::EntryValue {
            ref lookup_path,
            ref replacement,
            single_quote,
          } => {
            let schemas = $completion_schemas(snapshot, schema_url, instance, lookup_path, 0).await?;
            entry_value_completions(schemas, replacement.as_ref(), single_quote)
          }
          CompletionTarget::StandaloneKey {
            ref lookup_path,
            ref replacement,
          } => {
            let schemas = $completion_schemas(snapshot, schema_url, instance, lookup_path, 0).await?;
            standalone_key_completions(&snapshot.document.dom, schemas, replacement.as_ref())
          }
        }
      })
    }

    /// Resolve schema candidates through the shared completion error boundary.
    fn $completion_schemas<'operation, E: $environment>(
      snapshot: &'operation DocumentSnapshot<$transport<E>>,
      schema_url: &'operation Url,
      instance: &'operation Value,
      lookup_path: &'operation Keys,
      prefix_length: usize,
    ) -> $future<'operation, Result<Vec<(Keys, Keys, Arc<Value>)>, RpcError>> {
      Box::pin(async move {
        <$schema_execution>::possible_schemas_from(
          &snapshot.schemas,
          schema_url,
          instance,
          lookup_path,
          completion_depth(prefix_length, snapshot.config.completion.max_keys),
        )
        .await
        .map_err(|error| RpcError::internal_error().with_details(error.to_string()))
      })
    }
  };
}

define_response_document_handler_execution_families!(
  define_completion_future_family;
  (completion_local, completion_concurrent),
  (complete_target_local, complete_target_concurrent),
  (completion_schemas_local, completion_schemas_concurrent),
);

/// Return whether a schema branch permits an object.
#[allow(
  clippy::single_call_fn,
  reason = "the name carries the domain rule that an absent, null, scalar, or union `type` can still admit a table header, which the \
            filter closure would otherwise state as three unexplained JSON probes"
)]
fn accepts_object(schema: &Value) -> bool {
  let schema_type = schema.get("type");
  schema_type.is_none_or(Value::is_null)
    || schema_type.is_some_and(|candidate| candidate == "object")
    || schema_type
      .and_then(Value::as_array)
      .is_some_and(|types| types.iter().any(|candidate| candidate == "object"))
}

/// Return whether a schema branch permits an array whose items are objects.
#[allow(
  clippy::single_call_fn,
  reason = "the name carries the domain rule that `[[array of tables]]` requires an array whose item schema is absent, null, or an \
            object, keeping that TOML-specific contract out of the filter closure"
)]
fn accepts_array_of_objects(schema: &Value) -> bool {
  schema.get("type").is_some_and(|schema_type| schema_type == "array")
    && schema
      .get("items")
      .and_then(Value::as_object)
      .and_then(|items| items.get("type"))
      .is_none_or(|item_type| item_type.is_null() || item_type == "object")
}

/// Return whether a completion path is absent or represented only by a pseudo table.
fn missing_or_pseudo(root: &Node, path: &Keys) -> bool {
  root
    .path(path)
    .is_none_or(|node| node.as_table().is_some_and(|table| table.kind() == TableKind::Pseudo))
}

/// Build one replacement edit covering an existing source range.
const fn replacement_text_edit(replacement_range: Range, new_text: String) -> CompletionTextEdit {
  CompletionTextEdit::Edit(TextEdit {
    range: replacement_range,
    new_text,
  })
}

/// Build one schema-backed table-header completion.
fn header_completion(path: &Keys, schema: &Value, replacement: Option<&Range>) -> Result<CompletionItem, CompletionError> {
  let text = path.to_string();
  Ok(CompletionItem {
    label: text.clone(),
    kind: Some(CompletionItemKind::STRUCT),
    documentation: documentation(schema)?,
    insert_text: Some(text.clone()),
    text_edit: replacement.map(|replacement_range| replacement_text_edit(*replacement_range, text)),
    ..Default::default()
  })
}

/// Complete one standard table header.
fn table_header_completions(
  root: &Node,
  schemas: Vec<(Keys, Keys, Arc<Value>)>,
  replacement: Option<&Range>,
  current_path: &Keys,
) -> Result<Vec<CompletionItem>, RpcError> {
  schemas
    .into_iter()
    .filter(|schema_entry| accepts_object(&schema_entry.2))
    .filter(|schema_entry| {
      current_path == &schema_entry.0
        || root
          .path(&schema_entry.0)
          .is_none_or(|node| node.as_table().is_some_and(|table| table.kind() == TableKind::Pseudo))
    })
    .map(|(full_path, _, schema)| header_completion(&full_path, &schema, replacement))
    .collect::<Result<Vec<_>, CompletionError>>()
    .map_err(RpcError::from)
}

/// Complete one array-of-tables header.
fn array_table_header_completions(
  schemas: Vec<(Keys, Keys, Arc<Value>)>,
  replacement: Option<&Range>,
) -> Result<Vec<CompletionItem>, RpcError> {
  schemas
    .into_iter()
    .filter(|schema_entry| accepts_array_of_objects(&schema_entry.2))
    .map(|(full_path, _, schema)| header_completion(&full_path, &schema, replacement))
    .collect::<Result<Vec<_>, CompletionError>>()
    .map_err(RpcError::from)
}

/// Insert complete entry snippets beneath one table-like schema path.
fn new_entry_completions(root: &Node, schemas: Vec<(Keys, Keys, Arc<Value>)>) -> Result<Vec<CompletionItem>, RpcError> {
  schemas
    .into_iter()
    .filter(|schema_entry| missing_or_pseudo(root, &schema_entry.0))
    .map(|(_, relative_path, schema)| {
      Ok(CompletionItem {
        label: relative_path.to_string(),
        kind: Some(CompletionItemKind::VARIABLE),
        documentation: documentation(&schema)?,
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        insert_text: Some(new_entry_snippet(&relative_path, &schema, false)?),
        ..Default::default()
      })
    })
    .collect::<Result<Vec<_>, CompletionError>>()
    .map_err(RpcError::from)
}

/// Complete one partially written entry key.
fn entry_key_completions(
  schemas: Vec<(Keys, Keys, Arc<Value>)>,
  replacement: Option<&Range>,
  has_equals: bool,
) -> Result<Vec<CompletionItem>, RpcError> {
  schemas
    .into_iter()
    .map(|(_, relative_path, schema)| {
      let mut text = if has_equals {
        relative_path.to_string()
      } else {
        new_entry_snippet(&relative_path, &schema, false)?
      };
      if has_equals {
        text.push(' ');
      }
      Ok(CompletionItem {
        label: relative_path.to_string(),
        kind: Some(CompletionItemKind::VARIABLE),
        documentation: documentation(&schema)?,
        text_edit: replacement.map(|replacement_range| replacement_text_edit(*replacement_range, text.clone())),
        insert_text: Some(text),
        insert_text_format: if has_equals {
          None
        } else {
          Some(InsertTextFormat::SNIPPET)
        },
        ..Default::default()
      })
    })
    .collect::<Result<Vec<_>, CompletionError>>()
    .map_err(RpcError::from)
}

/// Complete one entry or array value.
fn entry_value_completions(
  schemas: Vec<(Keys, Keys, Arc<Value>)>,
  replacement: Option<&Range>,
  single_quote: bool,
) -> Result<Vec<CompletionItem>, RpcError> {
  let mut completions = Vec::new();
  for (_, _, schema) in schemas {
    add_value_completions(&schema, replacement, &mut completions, single_quote)?;
  }
  Ok(completions)
}

/// Replace an incomplete standalone key with complete entry snippets.
fn standalone_key_completions(
  root: &Node,
  schemas: Vec<(Keys, Keys, Arc<Value>)>,
  replacement: Option<&Range>,
) -> Result<Vec<CompletionItem>, RpcError> {
  schemas
    .into_iter()
    .filter(|schema_entry| missing_or_pseudo(root, &schema_entry.0))
    .map(|(_, relative_path, schema)| {
      let text = new_entry_snippet(&relative_path, &schema, false)?;
      Ok(CompletionItem {
        label: relative_path.to_string(),
        kind: Some(CompletionItemKind::VARIABLE),
        documentation: documentation(&schema)?,
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        insert_text: Some(text.clone()),
        text_edit: replacement.map(|replacement_range| replacement_text_edit(*replacement_range, text)),
        ..Default::default()
      })
    })
    .collect::<Result<Vec<_>, CompletionError>>()
    .map_err(RpcError::from)
}

/// Resolve nonempty completion documentation from Taplo extensions or JSON Schema.
///
/// # Errors
///
/// Returns [`CompletionError::SchemaExtension`] when Taplo-specific schema metadata is malformed.
fn documentation(schema: &Value) -> Result<Option<Documentation>, CompletionError> {
  Ok(
    schema_ext_of(schema)?
      .and_then(|ext| ext.docs)
      .and_then(|docs| docs.main)
      .or_else(|| schema.get("description").and_then(Value::as_str).map(ToOwned::to_owned))
      .filter(|documentation_text| !documentation_text.is_empty())
      .map(markdown_documentation),
  )
}

/// Wrap one owned Markdown string in the LSP documentation representation.
const fn markdown_documentation(content: String) -> Documentation {
  Documentation::MarkupContent(MarkupContent {
    kind:  MarkupKind::Markdown,
    value: content,
  })
}

/// Compute the maximum schema traversal depth without overflowing.
const fn completion_depth(prefix_length: usize, max_keys: usize) -> usize {
  prefix_length.saturating_add(max_keys)
}

/// Convert one JSON Schema candidate into losslessly equivalent TOML source.
///
/// # Errors
///
/// Returns a typed completion error when decoding, verification, or TOML rendering fails.
fn schema_value_to_toml(schema_value: &Value, single_quote: bool) -> Result<Option<(String, CompletionItemKind)>, CompletionError> {
  if schema_value.is_null() {
    return Ok(None);
  }

  let node = serde_json::from_value::<Node>(schema_value.clone()).map_err(|source| CompletionError::Decode {
    source,
  })?;
  let round_trip = serde_json::to_value(&node).map_err(|source| CompletionError::Encode {
    source,
  })?;
  if round_trip != *schema_value {
    return Err(CompletionError::Lossy {
      original:   Box::new(schema_value.clone()),
      round_trip: Box::new(round_trip),
    });
  }
  let kind = if node.as_table().is_some() {
    CompletionItemKind::STRUCT
  } else {
    CompletionItemKind::VALUE
  };
  Ok(Some((node.to_toml(true, single_quote)?, kind)))
}

/// Build one concrete-value completion with an optional replacement edit.
fn value_completion(
  text: String,
  kind: CompletionItemKind,
  documentation_text: Option<String>,
  replacement: Option<&Range>,
) -> CompletionItem {
  CompletionItem {
    label: text.clone(),
    kind: Some(kind),
    documentation: documentation_text
      .filter(|content| !content.is_empty())
      .map(markdown_documentation),
    insert_text: Some(text.clone()),
    text_edit: replacement.map(|replacement_range| replacement_text_edit(*replacement_range, text)),
    ..Default::default()
  }
}

/// Build one snippet completion with an optional replacement edit.
fn snippet_completion(label: &str, text: &str, documentation_text: Option<String>, replacement: Option<&Range>) -> CompletionItem {
  CompletionItem {
    label: label.into(),
    kind: Some(CompletionItemKind::VALUE),
    documentation: documentation_text
      .filter(|content| !content.is_empty())
      .map(markdown_documentation),
    insert_text: Some(text.into()),
    insert_text_format: Some(InsertTextFormat::SNIPPET),
    text_edit: replacement.map(|replacement_range| replacement_text_edit(*replacement_range, text.into())),
    ..Default::default()
  }
}

/// Select the first losslessly representable explicit schema value in precedence order.
#[allow(
  clippy::single_call_fn,
  reason = "the named selector owns the const-before-default precedence and its skip-on-unrepresentable fallthrough, so the caller's enum \
            and type-derived branches stay one readable step each"
)]
fn explicit_value_completion(
  schema: &Value,
  candidates: [(&str, Option<String>); 2],
  schema_docs: Option<&str>,
  replacement: Option<&Range>,
  single_quote: bool,
) -> Result<Option<CompletionItem>, CompletionError> {
  for (field, candidate_docs) in candidates {
    if let Some(candidate_value) = schema.get(field)
      && let Some((text, kind)) = schema_value_to_toml(candidate_value, single_quote)?
    {
      return Ok(Some(value_completion(
        text,
        kind,
        candidate_docs.or_else(|| schema_docs.map(str::to_owned)),
        replacement,
      )));
    }
  }
  Ok(None)
}

/// Add enum, const, default, or type-derived value completions for one schema branch.
///
/// # Errors
///
/// Returns a typed completion error when schema extensions or candidate TOML values cannot be
/// decoded losslessly.
#[allow(
  clippy::single_call_fn,
  reason = "the named builder owns the complete enum-then-const-or-default-then-type precedence for one schema branch, leaving its caller \
            to iterate branches without repeating that ordering"
)]
fn add_value_completions(
  schema: &Value,
  replacement: Option<&Range>,
  completions: &mut Vec<CompletionItem>,
  single_quote: bool,
) -> Result<(), CompletionError> {
  let ext_docs = schema_ext_of(schema)?.and_then(|extension| extension.docs).unwrap_or_default();
  let enum_docs = ext_docs.enum_values.as_deref().unwrap_or_default();

  let schema_docs = ext_docs
    .main
    .clone()
    .or_else(|| schema.get("description").and_then(Value::as_str).map(Into::into));

  if let Some(enum_values) = schema.get("enum").and_then(Value::as_array) {
    let mut enum_completions = Vec::new();
    for (index, enum_value) in enum_values.iter().enumerate() {
      if let Some((text, kind)) = schema_value_to_toml(enum_value, single_quote)? {
        let completion_docs = enum_docs.get(index).cloned().flatten().or_else(|| schema_docs.clone());
        let mut completion = value_completion(text.clone(), kind, completion_docs, replacement);
        completion.sort_text = Some(format!("{index}{text}"));
        enum_completions.push(completion);
      }
    }

    if !enum_completions.is_empty() {
      completions.extend(enum_completions);
      return Ok(());
    }
  }

  if let Some(completion) = explicit_value_completion(
    schema,
    [
      ("const", ext_docs.const_value.clone()),
      ("default", ext_docs.default_value.clone()),
    ],
    schema_docs.as_deref(),
    replacement,
    single_quote,
  )? {
    completions.push(completion);
    return Ok(());
  }

  let schema_types = match schema.get("type") {
    None => Vec::from(["object"]),
    Some(schema_type) if schema_type.is_null() => Vec::from(["object"]),
    Some(schema_type) if schema_type.is_string() => schema_type.as_str().map_or_else(Vec::new, |name| Vec::from([name])),
    Some(schema_type) => schema_type
      .as_array()
      .map_or_else(Vec::new, |types| types.iter().filter_map(Value::as_str).collect()),
  };

  for schema_type in schema_types {
    match schema_type {
      "string" => completions.push(snippet_completion(
        r#""""#,
        r#""$0""#,
        schema_docs.clone().or_else(|| Some("string".into())),
        replacement,
      )),
      "boolean" => {
        completions.push(snippet_completion(
          "true",
          "true$0",
          schema_docs.clone().or_else(|| Some("true value".into())),
          replacement,
        ));
        completions.push(snippet_completion(
          "false",
          "false$0",
          schema_docs.clone().or_else(|| Some("false value".into())),
          replacement,
        ));
      }
      "array" => completions.push(snippet_completion(
        "[]",
        "[$0]",
        schema_docs.clone().or_else(|| Some("array".into())),
        replacement,
      )),
      "object" => completions.push(snippet_completion(
        "{ }",
        "{ $0 }",
        schema_docs.clone().or_else(|| Some("object".into())),
        replacement,
      )),
      _ => {}
    }
  }
  Ok(())
}

/// Render one complete entry snippet from its semantic key path and schema.
///
/// # Errors
///
/// Returns a typed completion error when the schema's preferred value cannot be rendered
/// losslessly.
fn new_entry_snippet(keys: &Keys, schema: &Value, single_quote: bool) -> Result<String, CompletionError> {
  let value_snippet = default_value_snippet(schema, 0, single_quote)?;
  Ok(format!("{keys} = {value_snippet}"))
}

/// Render the preferred nested snippet for one schema branch.
///
/// # Errors
///
/// Returns a typed completion error when an explicit schema candidate or nested property cannot be
/// rendered losslessly.
fn default_value_snippet(schema: &Value, cursor_count: usize, single_quote: bool) -> Result<Cow<'static, str>, CompletionError> {
  if let Some(constant_value) = schema.get("const")
    && let Some((text, _)) = schema_value_to_toml(constant_value, single_quote)?
  {
    return Ok(format!("${{{cursor_count}:{text}}}").into());
  }

  if let Some(default_value) = schema.get("default")
    && let Some((text, _)) = schema_value_to_toml(default_value, single_quote)?
  {
    return Ok(format!("${{{cursor_count}:{text}}}").into());
  }

  if let Some(enum_values) = schema.get("enum").and_then(Value::as_array) {
    for enum_value in enum_values {
      if schema_value_to_toml(enum_value, single_quote)?.is_some() {
        return Ok(format!("${cursor_count}").into());
      }
    }
  }

  let mut init_keys = schema_ext_of(schema)?
    .and_then(|extension| extension.init_keys)
    .unwrap_or_default();

  if let Some(required) = schema.get("required").and_then(Value::as_array) {
    init_keys.extend(
      required
        .iter()
        .filter_map(|required_key| required_key.as_str().map(ToOwned::to_owned)),
    );
  }

  init_keys.dedup();

  if !init_keys.is_empty() {
    let nested_cursor = cursor_count.saturating_add(1);
    let mut snippet = String::from("{ ");

    for (index, init_key) in init_keys.iter().enumerate() {
      if index != 0 {
        snippet.push_str(", ");
      }
      snippet.push_str(init_key);
      snippet.push_str(" = ");
      let property_snippet = match schema
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get(init_key))
      {
        Some(property_schema) => default_value_snippet(property_schema, nested_cursor, single_quote)?,
        None => format!("{{ ${nested_cursor} }}").into(),
      };
      snippet.push_str(&property_snippet);
    }

    snippet.push_str(" }$0");
    return Ok(snippet.into());
  }

  Ok(empty_value_snippet(schema, cursor_count).into())
}

/// Render a type-directed empty snippet when no explicit value is available.
#[allow(
  clippy::single_call_fn,
  reason = "the name marks the terminating case of `default_value_snippet`'s recursion: the point where no explicit or nested value \
            remains and only the schema type may shape the placeholder"
)]
fn empty_value_snippet(schema: &Value, cursor_count: usize) -> String {
  if schema.is_schema_ref() {
    return format!("${cursor_count}");
  }

  let Some(schema_type) = schema.get("type") else {
    return format!("{{ ${cursor_count} }}");
  };
  if schema_type.is_null() {
    return format!("{{ ${cursor_count} }}");
  }
  schema_type.as_str().map_or_else(
    || format!("${cursor_count}"),
    |schema_type_name| match schema_type_name {
      "object" => format!("{{ ${cursor_count} }}"),
      "array" => format!("[${cursor_count}]"),
      "string" => format!(r#""${cursor_count}""#),
      "boolean" => format!("${{{cursor_count}:false}}"),
      _ => format!("${cursor_count}"),
    },
  )
}

#[cfg(test)]
mod tests {
  use std::fmt::Debug;
  use std::sync::Arc;

  use futures::executor::block_on;
  use lsp_types::CompletionItem;
  use lsp_types::CompletionItemKind;
  use lsp_types::CompletionParams;
  use lsp_types::CompletionResponse;
  use lsp_types::CompletionTextEdit;
  use lsp_types::Documentation;
  use lsp_types::InsertTextFormat;
  use lsp_types::MarkupContent;
  use lsp_types::Range;
  use serde_json::Value;
  use serde_json::json;
  use strict_test_support::ResultFailure;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_that;
  use taplo::dom::Keys;
  use taplo::dom::error::QueryError;
  use taplo::rowan::TextSize;
  use taplo_common::schema::associations::SchemaAssociation;
  use taplo_common::schema::associations::priority;
  use taplo_common::schema::associations::source;
  use taplo_common::schema::transport::LocalSchemaTransport;
  use taplo_lsp_async::Params;
  use taplo_lsp_async::rpc::RpcError;
  use url::Url;

  use super::super::test_support::FixtureFailure;
  use super::super::test_support::local_world;
  use super::super::test_support::parse_document;
  use super::super::test_support::position_params;
  use super::super::test_support::replace_local_document;
  use super::super::test_support::url as fixture_url;
  use super::CompletionError;
  use super::CompletionTarget;
  use super::accepts_array_of_objects;
  use super::accepts_object;
  use super::add_value_completions;
  use super::array_table_header_completions;
  use super::completion_depth;
  use super::completion_instance;
  use super::completion_local;
  use super::default_value_snippet;
  use super::documentation;
  use super::empty_value_snippet;
  use super::entry_key_completions;
  use super::entry_value_completions;
  use super::header_completion;
  use super::new_entry_completions;
  use super::schema_value_to_toml;
  use super::standalone_key_completions;
  use super::table_header_completions;
  use crate::LocalFuture;
  use crate::query::Query;
  use crate::world::DocumentSnapshot;
  use crate::world::DocumentState;
  use crate::world::DocumentUpdate;
  use crate::world::LocalWorld;
  use crate::world::ManualAssociationRule;
  use crate::world::ManualAssociationUpdate;
  use crate::world::TestEnvironment;
  use crate::world::WorldError;

  /// Schema traversal candidate retaining full path, relative path, and schema body.
  type SchemaCandidate = (Keys, Keys, Arc<Value>);

  /// Native request decoding and complete completion response.
  type CompletionRequest = Result<Result<Option<CompletionResponse>, RpcError>, ResultFailure<serde_json::Error>>;

  /// One installed document revision and its complete completion response.
  type RevisionCompletion = (Result<DocumentUpdate, Box<ResultFailure<WorldError>>>, CompletionRequest);

  /// Source, position, expected label sequence, and whether labels compare in sorted order.
  type CompletionCase = (&'static str, u32, &'static str, bool);

  /// Parsed document, cursor query, and the native completion classification.
  type CursorClassification = (DocumentState, Query, Result<Option<CompletionTarget>, RpcError>);

  /// World, association, and snapshot owners used across completion revisions.
  #[derive(Debug)]
  struct CompletionFixture {
    /// Local world retaining all document revisions and schema services.
    world:       LocalWorld<TestEnvironment>,
    /// Canonical source identity.
    document:    Url,
    /// Initial source installation and emitted effects.
    installed:   Result<DocumentUpdate, Box<ResultFailure<WorldError>>>,
    /// Manual association transition and its emitted effects.
    association: Result<ManualAssociationUpdate, WorldError>,
    /// Snapshot into whose services the schema was installed.
    snapshot:    Option<DocumentSnapshot<LocalSchemaTransport<TestEnvironment>>>,
  }

  /// Preserve a real parsed cursor and its completion classification.
  fn classify(source: &str, offset: u32) -> Result<CursorClassification, Box<ResultFailure<WorldError>>> {
    parse_document(source, "the completion-target fixture must parse").map(|document| {
      let query = Query::at(&document.dom, TextSize::new(offset));
      let classified = CompletionTarget::classify(&query, &document.dom, &document.mapper);
      (document, query, classified)
    })
  }

  /// Construct a complete schema candidate through the exact traversal path representation.
  fn completion_schema(full: &str, relative: &str, schema: Value) -> Result<SchemaCandidate, ResultFailure<QueryError>> {
    let full_path = ensure_ok(full.parse::<Keys>(), "the full completion path must parse")?;
    let relative_path = ensure_ok(relative.parse::<Keys>(), "the relative completion path must parse")?;
    Ok((full_path, relative_path, Arc::new(schema)))
  }

  /// Borrow completion labels in their observable protocol order.
  fn labels(completions: &[CompletionItem]) -> Vec<&str> {
    completions.iter().map(|completion| completion.label.as_str()).collect()
  }

  /// Borrow exact fields from a simple completion replacement.
  fn simple_text_edit(item: &CompletionItem) -> Option<(&Range, &str)> {
    match *item.text_edit.as_ref()? {
      CompletionTextEdit::Edit(ref edit) => Some((&edit.range, edit.new_text.as_str())),
      CompletionTextEdit::InsertAndReplace(_) => None,
    }
  }

  /// Execute a local completion request while retaining decoding and handler failures separately.
  fn completion_at<'operation>(
    world: &'operation LocalWorld<TestEnvironment>,
    document: &'operation Url,
    character: u32,
  ) -> LocalFuture<'operation, CompletionRequest> {
    Box::pin(async move {
      let parameters = position_params::<CompletionParams>(document, 0, character, "the completion request fixture must decode")?;
      Ok(completion_local(world, Params::from(Some(parameters))).await)
    })
  }

  /// Observe all requested revisions without dropping prior installs or protocol responses.
  fn complete_cases<'operation>(
    fixture: &'operation CompletionFixture,
    cases: impl IntoIterator<Item = CompletionCase> + 'operation,
  ) -> LocalFuture<'operation, Vec<(CompletionCase, RevisionCompletion)>> {
    Box::pin(async move {
      let mut observed = Vec::new();
      for case in cases {
        let installed = replace_local_document(
          &fixture.world,
          &fixture.document,
          case.0,
          "the completion document revision must install",
        )
        .await;
        let response = completion_at(&fixture.world, &fixture.document, case.1).await;
        observed.push((case, (installed, response)));
      }
      observed
    })
  }

  /// Borrow response labels without consuming the remaining completion protocol fields.
  fn response_labels(response: &CompletionRequest, sort: bool) -> Option<Vec<&str>> {
    let mut observed = match *response.as_ref().ok()?.as_ref().ok()?.as_ref()? {
      CompletionResponse::Array(ref items) => labels(items),
      CompletionResponse::List(ref list) => labels(&list.items),
    };
    if sort {
      observed.sort_unstable();
    }
    Some(observed)
  }

  /// Construct a schema-backed world while retaining every native setup outcome.
  fn schema_backed_completion_world() -> LocalFuture<'static, Result<CompletionFixture, FixtureFailure>> {
    Box::pin(async {
      let world = local_world()?;
      let document = fixture_url("file:///workspace/completion.toml", "the completion document URL must parse")?;
      let schema_url = fixture_url("https://example.com/completion-schema.json", "the completion schema URL must parse")?;
      let installed = replace_local_document(&world, &document, "", "the initial completion document must install").await;
      let association = world
        .associate_schema(ManualAssociationRule::Url(document.clone()), SchemaAssociation {
          meta:     json!({ "source": source::MANUAL }),
          url:      schema_url.clone(),
          priority: priority::MAX,
        })
        .await;
      let snapshot = world.document_snapshot(&document).await;
      if let Some(ref captured) = snapshot {
        captured.schemas.add_schema(
          &schema_url,
          Arc::new(json!({ "type": "object", "properties": {
        "flag": { "type": "boolean" }, "name": { "type": "string" },
        "table": { "type": "object", "properties": { "nested": { "const": 1 } } },
        "items": { "type": "array", "items": { "type": "object", "properties": { "id": { "type": "integer" } } } }
      } })),
        );
      }
      Ok(CompletionFixture {
        world,
        document,
        installed,
        association,
        snapshot,
      })
    })
  }

  /// Borrow rendered Markdown while preserving its complete completion owner.
  fn markdown(docs: Option<&Documentation>) -> Option<&str> {
    match *docs? {
      Documentation::MarkupContent(MarkupContent {
        ref value, ..
      })
      | Documentation::String(ref value) => Some(value),
    }
  }

  /// Render one expected editable-default placeholder.
  fn snippet_placeholder(tab_stop: u8, default_text: &str) -> String {
    format!("${{{tab_stop}:{default_text}}}")
  }

  #[test]
  fn completion_documentation_prefers_extension_content() -> Result<(), impl Debug> {
    let cases = [
      (
        json!({ "description": "description docs", "x-taplo": { "docs": { "main": "extension docs" } } }),
        Some("extension docs"),
      ),
      (json!({ "description": "description docs" }), Some("description docs")),
      (json!({ "description": "" }), None),
    ]
    .map(|(schema, expected)| {
      let docs = documentation(&schema);
      (schema, expected, docs)
    });
    ensure_that(
      cases,
      "completion documentation must prefer extensions, fall back to description, and omit empty content",
      |observed| {
        observed
          .iter()
          .all(|case| case.2.as_ref().is_ok_and(|docs| markdown(docs.as_ref()) == case.1))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn completion_targets_classify_each_supported_cursor_surface() -> Result<(), impl Debug> {
    let observed = [
      classify("[alpha]\n", 3),
      classify("[[alpha]]\n", 4),
      classify("", 0),
      classify("alpha = 1\n", 2),
      classify("root = { alpha = 1,  }\n", 20),
      classify("alpha = 1\n", 8),
      classify("values = [1]\n", 10),
      classify("alpha.", 6),
    ];
    ensure_that(observed, "completion classification must retain each cursor surface, lookup path and replacement boundary", |fixtures| {
      let [ref table, ref array_table, ref empty, ref key, ref inline, ref value, ref array_value, ref standalone] = *fixtures;
      table.as_ref().is_ok_and(|fixture| matches!(fixture.2, Ok(Some(CompletionTarget::TableHeader { prefix_length: 1, replacement: Some(_), .. }))))
        && array_table.as_ref().is_ok_and(|fixture| matches!(fixture.2, Ok(Some(CompletionTarget::ArrayTableHeader { prefix_length: 1, replacement: Some(_) }))))
        && empty.as_ref().is_ok_and(|fixture| matches!(fixture.2, Ok(Some(CompletionTarget::EmptyLine { ref lookup_path })) if lookup_path.is_empty()))
        && key.as_ref().is_ok_and(|fixture| matches!(fixture.2, Ok(Some(CompletionTarget::EntryKey { ref lookup_path, prefix_length: 1, replacement: Some(_), has_equals: true })) if lookup_path.is_empty()))
        && inline.as_ref().is_ok_and(|fixture| matches!(fixture.2, Ok(Some(CompletionTarget::InlineTableEntry { .. }))))
        && value.as_ref().is_ok_and(|fixture| matches!(fixture.2, Ok(Some(CompletionTarget::EntryValue { replacement: Some(_), single_quote: false, .. }))))
        && array_value.as_ref().is_ok_and(|fixture| matches!(fixture.2, Ok(Some(CompletionTarget::EntryValue { replacement: None, ref lookup_path, .. })) if !lookup_path.is_empty()))
        && standalone.as_ref().is_ok_and(|fixture| matches!(fixture.2, Ok(Some(CompletionTarget::StandaloneKey { replacement: Some(_), .. }))))
    }).map(drop).map_err(Box::new)
  }

  #[test]
  fn tolerant_completion_instances_preserve_semantics_indices_and_deep_shape() -> Result<(), impl Debug> {
    let tolerant = parse_document(
      "negative = -1\nvalid = 1\ninvalid = 999999999999999999999999999999\nnonfinite = inf\nvalues = [1, 999999999999999999999999999999, \
       inf]\nduplicate = 1\nduplicate = 2\ndate = 1979-05-27\n",
      "the tolerant completion-instance fixture must parse",
    )
    .map(|document| {
      let instance = completion_instance(&document.dom);
      (document, instance)
    });
    let depth = 10_000;
    let source = format!("deep = {}1{}\n", "[".repeat(depth), "]".repeat(depth));
    let deep = parse_document(&source, "the deeply nested completion-instance fixture must parse").map(|document| {
      let instance = completion_instance(&document.dom);
      (document, instance)
    });
    ensure_that(
      (tolerant, deep),
      "completion instances must preserve trustworthy values, duplicate winners, array positions and deep shape",
      |observed| {
        observed.0.as_ref().is_ok_and(|fixture| {
          fixture.1 == json!({ "negative": -1, "valid": 1, "values": [1, null, null], "duplicate": 2, "date": "1979-05-27" })
        }) && observed.1.as_ref().is_ok_and(|fixture| {
          fixture
            .1
            .get("deep")
            .and_then(|root| (0..depth).try_fold(root, |current, _| current.as_array().and_then(|array| array.first())))
            == Some(&json!(1))
        })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn schema_backed_handler_completes_header_surfaces() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let fixture = schema_backed_completion_world().await?;
      let cases = complete_cases(&fixture, [
        ("", 0, "flag,items,name,table,table.nested", true),
        ("[ta]\n", 3, "table,table.nested", false),
        ("[[it]]\n", 4, "items", false),
      ])
      .await;
      Ok::<_, FixtureFailure>((fixture, cases))
    });
    ensure_that(
      observed,
      "empty lines and headers must expose exactly their compatible schema paths",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        scenario.0.installed.is_ok()
          && scenario.0.association.is_ok()
          && scenario.0.snapshot.is_some()
          && scenario
            .1
            .iter()
            .all(|case| case.1.0.is_ok() && response_labels(&case.1.1, case.0.3).is_some_and(|names| names.join(",") == case.0.2))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn schema_backed_handler_completes_key_surfaces() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let fixture = schema_backed_completion_world().await?;
      let cases = complete_cases(&fixture, [
        ("fl", 2, "flag,items,name,table,table.nested", true),
        ("fl = false\n", 1, ",flag,items,name,table,table.nested", true),
      ])
      .await;
      Ok::<_, FixtureFailure>((fixture, cases))
    });
    ensure_that(
      observed,
      "standalone and assigned partial keys must expose complete paths while preserving the existing assignment",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        scenario.0.installed.is_ok()
          && scenario.0.association.is_ok()
          && scenario.0.snapshot.is_some()
          && scenario
            .1
            .iter()
            .all(|case| case.1.0.is_ok() && response_labels(&case.1.1, case.0.3).is_some_and(|names| names.join(",") == case.0.2))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn schema_backed_handler_completes_value_surfaces() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let fixture = schema_backed_completion_world().await?;
      let cases = complete_cases(&fixture, [
        ("flag = false\n", 9, "true,false", false),
        ("table = {  }\n", 10, "nested", false),
        ("name = ", 7, r#""""#, false),
      ])
      .await;
      Ok::<_, FixtureFailure>((fixture, cases))
    });
    ensure_that(
      observed,
      "value, inline-table and incomplete-value completion must preserve Boolean polarities and nested or string snippets",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        scenario.0.installed.is_ok()
          && scenario.0.association.is_ok()
          && scenario.0.snapshot.is_some()
          && scenario
            .1
            .iter()
            .all(|case| case.1.0.is_ok() && response_labels(&case.1.1, case.0.3).is_some_and(|names| names.join(",") == case.0.2))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn completion_handler_preserves_absence_and_typed_parameter_failures() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let unassociated = local_world()?;
      let disabled = local_world()?;
      let missing_document = fixture_url("file:///workspace/missing.toml", "the missing completion document URL must parse")?;
      let fixture = schema_backed_completion_world().await?;
      let unassociated_install = replace_local_document(
        &unassociated,
        &fixture.document,
        "",
        "the unassociated completion document must install",
      )
      .await;
      let unassociated_response = completion_at(&unassociated, &fixture.document, 0).await;
      let configuration = disabled
        .apply_configuration_values_local(Some(&json!({ "schema": { "enabled": false, "catalogs": [] } })), &[])
        .await;
      let disabled_install = replace_local_document(
        &disabled,
        &fixture.document,
        "",
        "the schema-disabled completion document must install",
      )
      .await;
      let disabled_response = completion_at(&disabled, &fixture.document, 0).await;
      let missing = completion_at(&fixture.world, &missing_document, 0).await;
      let rejected = completion_local(&fixture.world, Params::<CompletionParams>::from(None)).await;
      Ok::<_, FixtureFailure>((
        fixture,
        unassociated,
        disabled,
        unassociated_install,
        configuration,
        disabled_install,
        [unassociated_response, disabled_response, missing],
        rejected,
      ))
    });
    ensure_that(
      observed,
      "completion guards must preserve absent successes and the typed missing-parameters failure",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        scenario.0.installed.is_ok()
          && scenario.0.association.is_ok()
          && scenario.0.snapshot.is_some()
          && scenario.3.is_ok()
          && scenario.4.is_ok()
          && scenario.5.is_ok()
          && scenario.6.iter().all(|response| matches!(*response, Ok(Ok(None))))
          && scenario
            .7
            .as_ref()
            .is_err_and(|error| error.code == -32602 && error.details.is_some())
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn completion_target_rejects_value_context_without_a_semantic_node() -> Result<(), impl Debug> {
    let observed = parse_document("alpha = ", "the incomplete completion fixture must parse").map(|document| {
      let original = Query::at(&document.dom, TextSize::new(8));
      let mut query = Query::at(&document.dom, TextSize::new(8));
      if let Some(before) = query.before.as_mut() {
        before.dom_node = None;
      }
      if let Some(after) = query.after.as_mut() {
        after.dom_node = None;
      }
      let classified = CompletionTarget::classify(&query, &document.dom, &document.mapper);
      (document, original, query, classified)
    });
    ensure_that(
      observed,
      "an incomplete value cursor must retain value context and reject classification without a semantic node",
      |result| {
        result
          .as_ref()
          .is_ok_and(|fixture| fixture.1.in_entry_value() && matches!(fixture.3, Ok(None)))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn enum_completion_uses_only_convertible_values_and_suppresses_fallbacks() -> Result<(), impl Debug> {
    let schema = json!({ "enum": [1, null, 2], "const": 3, "default": 4, "type": "string", "description": "schema docs",
      "x-taplo": { "docs": { "enumValues": ["one docs", null, "two docs"] } } });
    let replacement = Range::default();
    let mut completions = Vec::new();
    let outcome = add_value_completions(&schema, Some(&replacement), &mut completions, false);
    ensure_that(
      (schema, replacement, outcome, completions),
      "convertible enum values must suppress fallbacks and retain source-index docs, insertion and replacement",
      |observed| {
        observed.2.is_ok()
          && labels(&observed.3) == ["1", "2"]
          && observed
            .3
            .first()
            .is_some_and(|item| markdown(item.documentation.as_ref()) == Some("one docs"))
          && observed
            .3
            .get(1)
            .is_some_and(|item| markdown(item.documentation.as_ref()) == Some("two docs"))
          && observed
            .3
            .iter()
            .all(|item| item.insert_text.is_some() && item.text_edit.is_some())
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn type_derived_completions_cover_absent_null_union_and_unknown_types() -> Result<(), impl Debug> {
    let observed = [
      (json!({}), "{ }"),
      (json!({ "type": null }), "{ }"),
      (json!({ "type": ["array", "object", "unknown"] }), "[],{ }"),
    ]
    .map(|(schema, expected)| {
      let mut completions = Vec::new();
      let outcome = add_value_completions(&schema, None, &mut completions, false);
      (schema, expected, outcome, completions)
    });
    ensure_that(
      observed,
      "type-derived completion must include only supported TOML value families",
      |cases| cases.iter().all(|case| case.2.is_ok() && labels(&case.3).join(",") == case.1),
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn completion_errors_map_to_internal_rpc_details_without_losing_the_typed_message() -> Result<(), impl Debug> {
    let rpc_error = RpcError::from(CompletionError::Lossy {
      original:   Box::new(json!(1)),
      round_trip: Box::new(json!(2)),
    });
    ensure_that(
      rpc_error,
      "completion failures must retain their stable typed message at the JSON-RPC boundary",
      |error| {
        (error.code, error.details.as_ref().and_then(Value::as_str))
          == (-32603, Some("schema completion value loses data during TOML conversion"))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn null_enum_const_and_default_values_fall_through_in_order() -> Result<(), impl Debug> {
    let cases = [
      (json!({ "enum": [null], "const": 3, "default": 4, "type": "string" }), vec!["3"]),
      (
        json!({ "enum": [], "const": null, "default": "fallback", "type": "boolean" }),
        vec![r#""fallback""#],
      ),
      (json!({ "enum": [null], "const": null, "default": null, "type": "boolean" }), vec![
        "true", "false",
      ]),
    ]
    .map(|(schema, expected)| {
      let mut completions = Vec::new();
      let outcome = add_value_completions(&schema, None, &mut completions, false);
      (schema, expected, outcome, completions)
    });
    let absent = schema_value_to_toml(&Value::Null, false);
    let rejected = schema_value_to_toml(&json!({ "bad": null }), false);
    ensure_that(
      (cases, absent, rejected),
      "null candidates must fall through enum, const, default and type while unrepresentable composites preserve decode failures",
      |observed| {
        observed.0.iter().all(|case| case.2.is_ok() && labels(&case.3) == case.1)
          && observed
            .0
            .first()
            .and_then(|case| case.3.first())
            .is_some_and(|item| item.text_edit.is_none() && item.insert_text.is_some())
          && matches!(observed.1, Ok(None))
          && matches!(observed.2, Err(CompletionError::Decode { .. }))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn schema_shape_filters_accept_only_matching_container_contracts() -> Result<(), impl Debug> {
    let objects = [
      (json!({}), true),
      (json!({ "type": null }), true),
      (json!({ "type": "object" }), true),
      (json!({ "type": ["string", "object"] }), true),
      (json!({ "type": "string" }), false),
      (json!({ "type": ["string", "boolean"] }), false),
    ]
    .map(|(schema, expected)| {
      let actual = accepts_object(&schema);
      (schema, expected, actual)
    });
    let arrays = [
      (json!({ "type": "array" }), true),
      (json!({ "type": "array", "items": {} }), true),
      (json!({ "type": "array", "items": { "type": null } }), true),
      (json!({ "type": "array", "items": { "type": "object" } }), true),
      (json!({ "type": "array", "items": { "type": "string" } }), false),
      (json!({ "type": "object", "items": { "type": "object" } }), false),
    ]
    .map(|(schema, expected)| {
      let actual = accepts_array_of_objects(&schema);
      (schema, expected, actual)
    });
    ensure_that(
      (objects, arrays),
      "container completion filters must preserve matching type and array-item contracts",
      |observed| observed.0.iter().chain(&observed.1).all(|case| case.1 == case.2),
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn header_builders_filter_existing_and_incompatible_schema_paths() -> Result<(), impl Debug> {
    let observed = (|| {
      let document = parse_document(
        "[existing]\nvalue = 1\n[pseudo.child]\n",
        "the header-completion DOM fixture must parse",
      )?;
      let replacement = Range::default();
      let schemas = vec![
        completion_schema("existing", "existing", json!({ "type": "object" }))?,
        completion_schema("missing", "missing", json!({ "type": "object", "description": "missing table" }))?,
        completion_schema("pseudo", "pseudo", json!({ "type": "object" }))?,
        completion_schema("scalar", "scalar", json!({ "type": "string" }))?,
      ];
      let current = ensure_ok("current".parse::<Keys>(), "the current header path must parse")?;
      let existing = ensure_ok("existing".parse::<Keys>(), "the existing header path must parse")?;
      let array_schemas = vec![
        completion_schema("items", "items", json!({ "type": "array", "items": { "type": "object" } }))?,
        completion_schema("open", "open", json!({ "type": "array" }))?,
        completion_schema("strings", "strings", json!({ "type": "array", "items": { "type": "string" } }))?,
        completion_schema("object", "object", json!({ "type": "object" }))?,
      ];
      let completions = table_header_completions(&document.dom, schemas.clone(), Some(&replacement), &current);
      let current_completions = table_header_completions(&document.dom, schemas.clone(), None, &existing);
      let arrays = array_table_header_completions(array_schemas.clone(), None);
      let invalid = header_completion(&existing, &json!({ "x-taplo": true }), None);
      Ok::<_, FixtureFailure>((
        document, replacement, schemas, array_schemas, completions, current_completions, arrays, invalid,
      ))
    })();
    ensure_that(
      observed,
      "header completion must filter existing and incompatible paths while preserving current-header eligibility and protocol fields",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        let Ok(ref items) = scenario.4 else {
          return false;
        };
        labels(items) == ["missing", "pseudo"]
          && items.first().is_some_and(|item| {
            (
              item.kind,
              item.insert_text.as_deref(),
              markdown(item.documentation.as_ref()),
              simple_text_edit(item),
            ) == (
              Some(CompletionItemKind::STRUCT),
              Some("missing"),
              Some("missing table"),
              Some((&scenario.1, "missing")),
            )
          })
          && scenario
            .5
            .as_ref()
            .is_ok_and(|current_items| labels(current_items) == ["existing", "missing", "pseudo"])
          && scenario
            .6
            .as_ref()
            .is_ok_and(|array_items| labels(array_items) == ["items", "open"])
          && matches!(scenario.7, Err(CompletionError::SchemaExtension(_)))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn entry_builders_preserve_presence_and_replacement_contracts() -> Result<(), impl Debug> {
    let observed = (|| {
      let document = parse_document("existing = 1\n[pseudo.child]\n", "the entry-completion DOM fixture must parse")?;
      let schemas = vec![
        completion_schema("existing", "existing", json!({ "default": 2 }))?,
        completion_schema("missing", "missing", json!({ "type": "string", "description": "missing entry" }))?,
        completion_schema("pseudo", "pseudo", json!({ "type": "object" }))?,
      ];
      let replacement = Range::default();
      let new_entries = new_entry_completions(&document.dom, schemas.clone());
      let standalone = standalone_key_completions(&document.dom, schemas.clone(), Some(&replacement));
      Ok::<_, FixtureFailure>((document, schemas, replacement, new_entries, standalone))
    })();
    ensure_that(
      observed,
      "entry builders must omit concrete paths and preserve snippet, documentation and replacement fields for missing or pseudo paths",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        let Ok(ref items) = scenario.3 else {
          return false;
        };
        labels(items) == ["missing", "pseudo"]
          && items
            .iter()
            .all(|item| (item.kind, item.insert_text_format) == (Some(CompletionItemKind::VARIABLE), Some(InsertTextFormat::SNIPPET)))
          && items.first().is_some_and(|item| {
            (item.insert_text.as_deref(), markdown(item.documentation.as_ref())) == (Some(r#"missing = "$0""#), Some("missing entry"))
          })
          && scenario.4.as_ref().is_ok_and(|standalone_items| {
            labels(standalone_items) == ["missing", "pseudo"] && standalone_items.iter().all(|item| item.text_edit.is_some())
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn entry_builders_preserve_equals_and_value_contracts() -> Result<(), impl Debug> {
    let observed = (|| {
      let replacement = Range::default();
      let key_schema = vec![completion_schema("feature", "feature", json!({ "default": true }))?];
      let value_schemas = vec![
        completion_schema("choice", "choice", json!({ "const": "fixed" }))?,
        completion_schema("flag", "flag", json!({ "type": "boolean" }))?,
      ];
      let assigned = entry_key_completions(key_schema.clone(), Some(&replacement), true);
      let unassigned = entry_key_completions(key_schema.clone(), None, false);
      let values = entry_value_completions(value_schemas.clone(), Some(&replacement), true);
      Ok::<_, ResultFailure<QueryError>>((replacement, key_schema, value_schemas, assigned, unassigned, values))
    })();
    ensure_that(
      observed,
      "assigned keys must preserve equals while unassigned keys and branch values retain complete snippets and replacements",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        let Ok(ref assigned) = scenario.3 else {
          return false;
        };
        let Ok(ref unassigned) = scenario.4 else {
          return false;
        };
        assigned.first().is_some_and(|item| {
          (item.insert_text.as_deref(), item.insert_text_format, simple_text_edit(item))
            == (Some("feature "), None, Some((&scenario.0, "feature ")))
        }) && unassigned.first().is_some_and(|item| {
          item.insert_text.as_deref() == Some(format!("feature = {}", snippet_placeholder(0, "true")).as_str())
            && item.insert_text_format == Some(InsertTextFormat::SNIPPET)
            && item.text_edit.is_none()
        }) && scenario
          .5
          .as_ref()
          .is_ok_and(|items| labels(items) == ["'fixed'", "true", "false"] && items.iter().all(|item| item.text_edit.is_some()))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn empty_value_snippets_cover_every_schema_type_polarity() -> Result<(), impl Debug> {
    let boolean_placeholder = snippet_placeholder(3, "false");
    let cases = [
      (json!({ "$ref": "#/$defs/value" }), "$3"),
      (json!({}), "{ $3 }"),
      (json!({ "type": null }), "{ $3 }"),
      (json!({ "type": ["string", "boolean"] }), "$3"),
      (json!({ "type": "object" }), "{ $3 }"),
      (json!({ "type": "array" }), "[$3]"),
      (json!({ "type": "string" }), r#""$3""#),
      (json!({ "type": "boolean" }), boolean_placeholder.as_str()),
      (json!({ "type": "integer" }), "$3"),
    ]
    .map(|(schema, expected)| {
      let actual = empty_value_snippet(&schema, 3);
      (schema, expected.to_owned(), actual)
    });
    let schema = json!({ "type": "object", "required": ["missing"], "properties": {} });
    let missing = default_value_snippet(&schema, 0, false);
    ensure_that(
      (cases, schema, missing),
      "empty snippets must preserve each schema type and generic nested placeholders for absent required properties",
      |observed| {
        observed.0.iter().all(|case| case.1 == case.2) && observed.2.as_ref().is_ok_and(|snippet| snippet == "{ missing = { $1 } }$0")
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn schema_value_conversion_preserves_value_kind() -> Result<(), impl Debug> {
    let scalar = schema_value_to_toml(&json!(1), false);
    let object = schema_value_to_toml(&json!({ "key": 1 }), false);
    ensure_that(
      (scalar, object),
      "schema value conversion must retain scalar text and distinguish structural completions",
      |observed| {
        observed.0.as_ref().is_ok_and(|value| {
          value
            .as_ref()
            .is_some_and(|candidate| candidate == &("1".into(), CompletionItemKind::VALUE))
        }) && observed.1.as_ref().is_ok_and(|value| {
          value
            .as_ref()
            .is_some_and(|candidate| candidate.1 == CompletionItemKind::STRUCT)
        })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn recursive_default_snippets_use_safe_precedence_and_saturating_cursors() -> Result<(), impl Debug> {
    let cases = [
      (
        json!({ "const": "fixed", "default": "other", "type": "string" }),
        r#"${0:"fixed"}"#.to_owned(),
      ),
      (
        json!({ "const": null, "default": true, "type": "string" }),
        snippet_placeholder(0, "true"),
      ),
      (json!({ "enum": [null, 1], "type": "string" }), "$0".to_owned()),
      (json!({ "enum": [null], "type": "string" }), r#""$0""#.to_owned()),
    ]
    .map(|(schema, expected)| {
      let actual = default_value_snippet(&schema, 0, false);
      (schema, expected, actual)
    });
    let recursive_schema = json!({ "type": "object", "required": ["name"], "properties": { "name": { "const": "fixed" } } });
    let recursive = default_value_snippet(&recursive_schema, usize::MAX, false);
    let multi_schema = json!({ "type": "object", "required": ["name", "enabled"], "properties": { "name": { "const": "fixed" }, "enabled": { "type": "boolean" } } });
    let multi = default_value_snippet(&multi_schema, 0, false);
    let depth = completion_depth(usize::MAX, 1);
    ensure_that(
      (cases, recursive_schema, recursive, multi_schema, multi, depth),
      "default snippets must retain precedence, all required keys and saturating nested cursor and traversal depths",
      |observed| {
        observed
          .0
          .iter()
          .all(|case| case.2.as_ref().is_ok_and(|actual| actual.as_ref() == case.1))
          && observed
            .2
            .as_ref()
            .is_ok_and(|snippet| snippet.contains(&format!("${{{}:\"fixed\"}}", usize::MAX)))
          && observed
            .4
            .as_ref()
            .is_ok_and(|snippet| snippet == r#"{ name = ${1:"fixed"}, enabled = ${1:false} }$0"#)
          && observed.5 == usize::MAX
      },
    )
    .map(drop)
    .map_err(Box::new)
  }
}
