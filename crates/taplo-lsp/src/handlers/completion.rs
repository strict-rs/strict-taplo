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
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_contains;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use taplo::dom::Keys;
  use taplo::rowan::TextSize;
  use taplo_common::schema::associations::SchemaAssociation;
  use taplo_common::schema::associations::priority;
  use taplo_common::schema::associations::source;
  use taplo_lsp_async::Params;
  use taplo_lsp_async::rpc::RpcError;
  use taplo_lsp_async::util::Mapper;
  use url::Url;

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
  use crate::LocalTestFuture;
  use crate::query::Query;
  use crate::world::LocalWorld;
  use crate::world::ManualAssociationRule;
  use crate::world::TestEnvironment;

  /// One schema candidate in traversal shape: full path, relative path, and schema body.
  type SchemaCandidate = (Keys, Keys, Arc<Value>);

  /// Classify one real parsed cursor fixture through the production completion decision model.
  fn classify(source: &str, offset: u32) -> Result<Option<CompletionTarget>, TestFailure> {
    let document = parse_document(source, "the completion-target fixture must parse")?;
    ensure_ok(
      CompletionTarget::classify(&Query::at(&document.dom, TextSize::new(offset)), &document.dom, &document.mapper),
      "the completion target must project into LSP coordinates",
    )
  }

  /// Construct one schema candidate through the same exact-path representation used by traversal.
  fn completion_schema(full: &str, relative: &str, schema: Value) -> Result<SchemaCandidate, TestFailure> {
    let full_path = ensure_ok(full.parse::<Keys>(), "the full completion path must parse")?;
    let relative_path = ensure_ok(relative.parse::<Keys>(), "the relative completion path must parse")?;
    Ok((full_path, relative_path, Arc::new(schema)))
  }

  /// Return completion labels in their observable protocol order.
  fn labels(completions: &[CompletionItem]) -> Vec<&str> {
    completions.iter().map(|completion| completion.label.as_str()).collect()
  }

  /// Project a simple completion replacement into exact observable fields.
  fn simple_text_edit(item: &CompletionItem) -> Option<(&Range, &str)> {
    match *item.text_edit.as_ref()? {
      CompletionTextEdit::Edit(ref edit) => Some((&edit.range, edit.new_text.as_str())),
      CompletionTextEdit::InsertAndReplace(_) => None,
    }
  }

  /// Decode one complete completion request through its public wire shape.
  #[allow(
    clippy::single_call_fn,
    reason = "the named fixture fixes completion's request type at the shared positional decoder, so every scenario exercises the real \
              `CompletionParams` wire shape rather than a hand-built value"
  )]
  fn completion_params(document: &Url, line: u32, character: u32) -> Result<CompletionParams, TestFailure> {
    position_params(document, line, character, "the completion request fixture must decode")
  }

  /// Execute one local completion request against the current document revision.
  fn completion_at<'operation>(
    world: &'operation LocalWorld<TestEnvironment>,
    document: &'operation Url,
    character: u32,
    context: &'static str,
  ) -> LocalTestFuture<'operation, Option<CompletionResponse>> {
    Box::pin(async move {
      ensure_ok(
        completion_local(world, Params::from(Some(completion_params(document, 0, character)?))).await,
        context,
      )
    })
  }

  /// Install one source revision and execute completion through the local public handler family.
  #[allow(
    clippy::single_call_fn,
    reason = "the named helper binds source installation to the completion request that must observe it, so a scenario cannot query a \
              revision it never committed"
  )]
  fn complete_source<'operation>(
    world: &'operation LocalWorld<TestEnvironment>,
    document: &'operation Url,
    source_text: &'operation str,
    character: u32,
    context: &'static str,
  ) -> LocalTestFuture<'operation, Option<CompletionResponse>> {
    Box::pin(async move {
      replace_local_document(world, document, source_text, "the completion document revision must install").await?;
      completion_at(world, document, character, context).await
    })
  }

  /// Return owned labels from either supported LSP completion response representation.
  #[allow(
    clippy::single_call_fn,
    reason = "the named projection keeps the exhaustive `CompletionResponse` match in one place, so assertions observe labels without \
              depending on which wire representation the handler returns"
  )]
  fn response_labels(response: CompletionResponse) -> Vec<String> {
    match response {
      CompletionResponse::Array(items) => items.into_iter().map(|item| item.label).collect(),
      CompletionResponse::List(list) => list.items.into_iter().map(|item| item.label).collect(),
    }
  }

  /// Execute one schema-backed completion request and require a present response.
  fn completion_labels<'operation>(
    world: &'operation LocalWorld<TestEnvironment>,
    document: &'operation Url,
    source_text: &'operation str,
    character: u32,
    execution_context: &'static str,
    presence_context: &'static str,
  ) -> LocalTestFuture<'operation, Vec<String>> {
    Box::pin(async move {
      let response = ensure_some(
        complete_source(world, document, source_text, character, execution_context).await?,
        presence_context,
      )?;
      Ok(response_labels(response))
    })
  }

  /// Execute one completion request whose observable label contract is order-independent.
  fn sorted_completion_labels<'operation>(
    world: &'operation LocalWorld<TestEnvironment>,
    document: &'operation Url,
    source_text: &'operation str,
    character: u32,
    execution_context: &'static str,
    presence_context: &'static str,
  ) -> LocalTestFuture<'operation, String> {
    Box::pin(async move {
      let mut labels = completion_labels(world, document, source_text, character, execution_context, presence_context).await?;
      labels.sort();
      Ok(labels.join(","))
    })
  }

  /// Construct one document with an exact association and a complete in-memory schema.
  fn schema_backed_completion_world() -> LocalTestFuture<'static, (LocalWorld<TestEnvironment>, Url)> {
    Box::pin(async {
      let world = local_world()?;
      let document = fixture_url("file:///workspace/completion.toml", "the completion document URL must parse")?;
      replace_local_document(&world, &document, "", "the initial completion document must install").await?;
      let schema_url = fixture_url("https://example.com/completion-schema.json", "the completion schema URL must parse")?;
      drop(ensure_ok(
        world
          .associate_schema(ManualAssociationRule::Url(document.clone()), SchemaAssociation {
            meta:     json!({ "source": source::MANUAL }),
            url:      schema_url.clone(),
            priority: priority::MAX,
          })
          .await,
        "the exact completion schema association must commit",
      )?);
      let snapshot = ensure_some(
        world.document_snapshot(&document).await,
        "the associated completion document must expose a snapshot",
      )?;
      snapshot.schemas.add_schema(
        &schema_url,
        Arc::new(json!({
          "type": "object",
          "properties": {
            "flag": {
              "type": "boolean"
            },
            "name": {
              "type": "string"
            },
            "table": {
              "type": "object",
              "properties": {
                "nested": {
                  "const": 1
                }
              }
            },
            "items": {
              "type": "array",
              "items": {
                "type": "object",
                "properties": {
                  "id": {
                    "type": "integer"
                  }
                }
              }
            }
          }
        })),
      );
      Ok((world, document))
    })
  }

  /// Extract rendered Markdown from one completion documentation value.
  fn markdown(docs: Option<&Documentation>) -> Option<&str> {
    match *docs? {
      Documentation::MarkupContent(MarkupContent {
        ref value, ..
      })
      | Documentation::String(ref value) => Some(value),
    }
  }

  /// Render one expected LSP snippet placeholder with an editable default, such as `${0:true}`.
  fn snippet_placeholder(tab_stop: u8, default_text: &str) -> String {
    format!("${{{tab_stop}:{default_text}}}")
  }

  #[test]
  fn completion_documentation_prefers_extension_content() -> Result<(), TestFailure> {
    let schema = json!({
        "description": "description docs",
        "x-taplo": { "docs": { "main": "extension docs" } }
    });
    let docs = ensure_some(
      ensure_ok(documentation(&schema), "valid completion extension metadata must decode")?,
      "completion docs must exist",
    )?;
    ensure(
      markdown(Some(&docs)) == Some("extension docs"),
      "extension main docs must override schema description",
    )?;
    ensure(
      markdown(
        ensure_ok(
          documentation(&json!({ "description": "description docs" })),
          "an absent extension must retain schema documentation",
        )?
        .as_ref(),
      ) == Some("description docs"),
      "schema description must backfill absent extension docs",
    )?;
    ensure(
      ensure_ok(
        documentation(&json!({ "description": "" })),
        "empty schema documentation must decode",
      )?
      .is_none(),
      "empty documentation must be omitted",
    )
  }

  #[test]
  fn completion_targets_classify_each_supported_cursor_surface() -> Result<(), TestFailure> {
    ensure(
      matches!(
        classify("[alpha]\n", 3)?,
        Some(CompletionTarget::TableHeader {
          prefix_length: 1,
          replacement: Some(_),
          ..
        })
      ),
      "a cursor inside a table header must select table-header completion with its replacement range",
    )?;
    ensure(
      matches!(
        classify("[[alpha]]\n", 4)?,
        Some(CompletionTarget::ArrayTableHeader {
          prefix_length: 1,
          replacement:   Some(_),
        })
      ),
      "a cursor inside an array-table header must select array-table completion",
    )?;
    ensure(
      matches!(
        classify("", 0)?,
        Some(CompletionTarget::EmptyLine {
          lookup_path
        }) if lookup_path.is_empty()
      ),
      "an empty document must select root entry insertion",
    )?;
    let Some(CompletionTarget::EntryKey {
      lookup_path,
      prefix_length,
      replacement,
      has_equals,
    }) = classify("alpha = 1\n", 2)?
    else {
      return ensure(false, "a cursor inside an assigned key must select entry-key completion");
    };
    ensure(lookup_path.is_empty(), "a root entry key must retain the root schema lookup path")?;
    ensure_eq(
      &prefix_length,
      &1,
      "a one-segment entry key must retain one completed prefix segment",
    )?;
    ensure(replacement.is_some(), "an existing entry key must retain its replacement range")?;
    ensure(has_equals, "an assigned entry key must preserve its existing equals sign")?;
    ensure(
      matches!(
        classify("root = { alpha = 1,  }\n", 20)?,
        Some(CompletionTarget::InlineTableEntry { .. })
      ),
      "a cursor between inline-table entries must select inline entry insertion",
    )?;
    ensure(
      matches!(
        classify("alpha = 1\n", 8)?,
        Some(CompletionTarget::EntryValue {
          replacement: Some(_),
          single_quote: false,
          ..
        })
      ),
      "a cursor on a scalar value must select value replacement with basic-string preference",
    )?;
    let Some(CompletionTarget::EntryValue {
      replacement: value_replacement,
      lookup_path: value_lookup_path,
      ..
    }) = classify("values = [1]\n", 10)?
    else {
      return ensure(false, "a cursor on an array element must select entry-value completion");
    };
    ensure(
      (value_replacement.is_none(), value_lookup_path.is_empty()) == (true, false),
      "an array element must retain its semantic inline lookup path without replacing the complete array value",
    )?;
    ensure(
      matches!(
        classify("alpha.", 6)?,
        Some(CompletionTarget::StandaloneKey {
          replacement: Some(_),
          ..
        })
      ),
      "an incomplete standalone key must select complete-entry replacement",
    )
  }

  /// Preserve every trustworthy value while omitting or placeholder-projecting malformed values.
  #[test]
  fn tolerant_completion_instances_preserve_semantics_indices_and_deep_shape() -> Result<(), TestFailure> {
    let document = parse_document(
      "negative = -1\nvalid = 1\ninvalid = 999999999999999999999999999999\nnonfinite = inf\nvalues = [1, 999999999999999999999999999999, \
       inf]\nduplicate = 1\nduplicate = 2\ndate = 1979-05-27\n",
      "the tolerant completion-instance fixture must parse",
    )?;
    ensure_eq(
      &completion_instance(&document.dom),
      &json!({
        "negative": -1,
        "valid": 1,
        "values": [1, null, null],
        "duplicate": 2,
        "date": "1979-05-27"
      }),
      "completion projection must retain valid values, final duplicate winners, and array positions without inventing malformed table \
       values",
    )?;

    let depth = 10_000;
    let source = format!("deep = {}1{}\n", "[".repeat(depth), "]".repeat(depth));
    let deep_document = parse_document(&source, "the deeply nested completion-instance fixture must parse")?;
    let deep_instance = completion_instance(&deep_document.dom);
    let mut current = deep_instance.get("deep");
    for _ in 0..depth {
      current = current.and_then(Value::as_array).and_then(|array| array.first());
    }
    ensure(
      current == Some(&json!(1)),
      "completion projection must preserve deeply nested array shape without call-stack recursion",
    )
  }

  /// Exercise the empty-line and header completion targets through schema lookup and rendering.
  #[test]
  fn schema_backed_handler_completes_header_surfaces() -> Result<(), TestFailure> {
    block_on(async {
      let (world, document) = schema_backed_completion_world().await?;

      let empty_label_set = sorted_completion_labels(
        &world,
        &document,
        "",
        0,
        "empty-line completion must execute through the local handler",
        "an empty associated document must return root entry completions",
      )
      .await?;
      ensure_eq(
        &empty_label_set.as_str(),
        &"flag,items,name,table,table.nested",
        "empty-line completion must expose root and reachable nested schema paths",
      )?;

      let table_labels = completion_labels(
        &world,
        &document,
        "[ta]\n",
        3,
        "table-header completion must execute through the local handler",
        "an incomplete table header must return schema-backed header completions",
      )
      .await?
      .join(",");
      ensure_eq(
        &table_labels.as_str(),
        &"table,table.nested",
        "table-header completion must retain object-compatible paths, including unconstrained nested schemas",
      )?;

      let array_table_labels = completion_labels(
        &world,
        &document,
        "[[it]]\n",
        4,
        "array-table completion must execute through the local handler",
        "an incomplete array-table header must return schema-backed header completions",
      )
      .await?
      .join(",");
      ensure_eq(
        &array_table_labels.as_str(),
        &"items",
        "array-table completion must retain only compatible arrays of objects",
      )
    })
  }

  /// Exercise the standalone and assigned entry-key completion targets through schema lookup.
  #[test]
  fn schema_backed_handler_completes_key_surfaces() -> Result<(), TestFailure> {
    block_on(async {
      let (world, document) = schema_backed_completion_world().await?;

      let entry_key_label_set = sorted_completion_labels(
        &world,
        &document,
        "fl",
        2,
        "standalone-key completion must execute through the local handler",
        "an incomplete standalone key must return complete-entry replacements",
      )
      .await?;
      ensure_eq(
        &entry_key_label_set.as_str(),
        &"flag,items,name,table,table.nested",
        "standalone-key replacement must expose every compatible schema path",
      )?;

      let assigned_key_label_set = sorted_completion_labels(
        &world,
        &document,
        "fl = false\n",
        1,
        "assigned entry-key completion must execute through the local handler",
        "an assigned partial key must return schema-backed key replacements",
      )
      .await?;
      ensure_eq(
        &assigned_key_label_set.as_str(),
        &",flag,items,name,table,table.nested",
        "assigned entry-key completion must expose the current path and every compatible schema path while retaining the existing equals \
         sign",
      )
    })
  }

  /// Exercise the value, inline-table, and incomplete-value completion targets through rendering.
  #[test]
  fn schema_backed_handler_completes_value_surfaces() -> Result<(), TestFailure> {
    block_on(async {
      let (world, document) = schema_backed_completion_world().await?;

      let entry_value_labels = completion_labels(
        &world,
        &document,
        "flag = false\n",
        9,
        "entry-value completion must execute through the local handler",
        "a Boolean value must return schema-backed replacement values",
      )
      .await?
      .join(",");
      ensure_eq(
        &entry_value_labels.as_str(),
        &"true,false",
        "entry-value completion must render both Boolean schema polarities",
      )?;

      let inline_entry_labels = completion_labels(
        &world,
        &document,
        "table = {  }\n",
        10,
        "inline-table completion must execute through the local handler",
        "an inline table gap must return nested entry completions",
      )
      .await?
      .join(",");
      ensure_eq(
        &inline_entry_labels.as_str(),
        &"nested",
        "inline-table completion must traverse to the nested object schema",
      )?;

      let incomplete_value_labels = completion_labels(
        &world,
        &document,
        "name = ",
        7,
        "incomplete-value completion must execute through the local handler",
        "an incomplete assigned value must retain schema-backed value completions",
      )
      .await?
      .join(",");
      ensure_eq(
        &incomplete_value_labels.as_str(),
        &r#""""#,
        "an incomplete string value must offer the schema-directed empty string snippet",
      )
    })
  }

  /// Preserve absent success responses and typed request failures at every handler guard.
  #[test]
  fn completion_handler_preserves_absence_and_typed_parameter_failures() -> Result<(), TestFailure> {
    block_on(async {
      let (world, document) = schema_backed_completion_world().await?;
      let unassociated_world = local_world()?;
      replace_local_document(
        &unassociated_world,
        &document,
        "",
        "the unassociated completion document must install",
      )
      .await?;
      ensure(
        completion_at(
          &unassociated_world,
          &document,
          0,
          "completion without an association must remain an absent success",
        )
        .await?
        .is_none(),
        "an open document without a schema association must not fabricate completions",
      )?;

      let disabled_world = local_world()?;
      let disabled_configuration = json!({
        "schema": {
          "enabled": false,
          "catalogs": []
        }
      });
      drop(ensure_ok(
        disabled_world
          .apply_configuration_values_local(Some(&disabled_configuration), &[])
          .await,
        "the schema-disabled completion configuration must commit",
      )?);
      replace_local_document(
        &disabled_world,
        &document,
        "",
        "the schema-disabled completion document must install",
      )
      .await?;
      ensure(
        completion_at(
          &disabled_world,
          &document,
          0,
          "schema-disabled completion must remain an absent success",
        )
        .await?
        .is_none(),
        "schema-disabled documents must not request or fabricate completion candidates",
      )?;

      let missing_document = fixture_url("file:///workspace/missing.toml", "the missing completion document URL must parse")?;
      ensure(
        completion_at(
          &world,
          &missing_document,
          0,
          "completion for an unopened document must remain an absent success",
        )
        .await?
        .is_none(),
        "completion must not fabricate state for an unopened document",
      )?;
      let missing_params = ensure_some(
        completion_local(&world, Params::<CompletionParams>::from(None)).await.err(),
        "completion without parameters must return a typed invalid-params error",
      )?;
      ensure(
        (missing_params.code, missing_params.details.is_some()) == (-32602, true),
        "completion without parameters must retain the standard typed invalid-params error",
      )
    })
  }

  #[test]
  fn completion_target_rejects_value_context_without_a_semantic_node() -> Result<(), TestFailure> {
    let source = "alpha = ";
    let document = parse_document(source, "the incomplete completion fixture must parse")?;
    let mapper = ensure_ok(Mapper::new_utf16(source), "the incomplete completion mapper must construct")?;
    let mut query = Query::at(&document.dom, TextSize::new(8));
    ensure(query.in_entry_value(), "the incomplete assignment cursor must retain value context")?;
    if let Some(before) = query.before.as_mut() {
      before.dom_node = None;
    }
    if let Some(after) = query.after.as_mut() {
      after.dom_node = None;
    }
    ensure(
      ensure_ok(
        CompletionTarget::classify(&query, &document.dom, &mapper),
        "the incomplete target must project without a mapping failure",
      )?
      .is_none(),
      "an incomplete value without a semantic DOM node must not fabricate a completion lookup path",
    )
  }

  #[test]
  fn enum_completion_uses_only_convertible_values_and_suppresses_fallbacks() -> Result<(), TestFailure> {
    let schema = json!({
        "enum": [1, null, 2],
        "const": 3,
        "default": 4,
        "type": "string",
        "description": "schema docs",
        "x-taplo": {
            "docs": { "enumValues": ["one docs", null, "two docs"] }
        }
    });
    let mut completions = Vec::new();
    let replacement = Range::default();
    ensure_ok(
      add_value_completions(&schema, Some(&replacement), &mut completions, false),
      "valid enum values must produce completions",
    )?;
    ensure_eq(
      &completions.len(),
      &2,
      "a nonempty convertible enum must suppress const, default, and type completions",
    )?;
    ensure(
      completions.iter().map(|item| item.label.as_str()).collect::<Vec<_>>() == vec!["1", "2"],
      "invalid enum values must be omitted while valid values retain declaration order",
    )?;
    let first = ensure_some(completions.first(), "the first enum completion must exist")?;
    let second = ensure_some(completions.get(1), "the second enum completion must exist")?;
    ensure(
      markdown(first.documentation.as_ref()) == Some("one docs"),
      "enum documentation must remain aligned by original schema index",
    )?;
    ensure(
      markdown(second.documentation.as_ref()) == Some("two docs"),
      "later enum documentation must remain aligned after an invalid value is skipped",
    )?;
    ensure(
      completions
        .iter()
        .map(|item| (item.insert_text.is_some(), item.text_edit.is_some()))
        .collect::<Vec<_>>()
        == vec![(true, true), (true, true)],
      "mapped enum completions must support both insertion and replacement",
    )
  }

  #[test]
  fn type_derived_completions_cover_absent_null_union_and_unknown_types() -> Result<(), TestFailure> {
    for (schema, expected_labels) in [
      (json!({}), "{ }"),
      (json!({ "type": null }), "{ }"),
      (json!({ "type": ["array", "object", "unknown"] }), "[],{ }"),
    ] {
      let mut completions = Vec::new();
      ensure_ok(
        add_value_completions(&schema, None, &mut completions, false),
        "a schema type declaration must produce its supported fallback completions",
      )?;
      let labels = completions
        .iter()
        .map(|completion| completion.label.as_str())
        .collect::<Vec<_>>()
        .join(",");
      ensure_eq(
        &labels.as_str(),
        &expected_labels,
        "type-derived completion must include only supported TOML value families",
      )?;
    }
    Ok(())
  }

  #[test]
  fn completion_errors_map_to_internal_rpc_details_without_losing_the_typed_message() -> Result<(), TestFailure> {
    let rpc_error = RpcError::from(CompletionError::Lossy {
      original:   Box::new(json!(1)),
      round_trip: Box::new(json!(2)),
    });
    ensure(
      (rpc_error.code, rpc_error.details.as_ref().and_then(Value::as_str))
        == (-32603, Some("schema completion value loses data during TOML conversion")),
      "completion failures must retain their stable typed message at the JSON-RPC adapter boundary",
    )
  }

  #[test]
  fn null_enum_const_and_default_values_fall_through_in_order() -> Result<(), TestFailure> {
    let const_schema = json!({
        "enum": [null],
        "const": 3,
        "default": 4,
        "type": "string"
    });
    let mut completions = Vec::new();
    ensure_ok(
      add_value_completions(&const_schema, None, &mut completions, false),
      "a null enum value must fall through to a valid constant",
    )?;
    ensure(
      completions.iter().map(|item| item.label.as_str()).collect::<Vec<_>>() == vec!["3"],
      "an all-invalid enum must fall through to a valid const",
    )?;
    let const_completion = ensure_some(completions.first(), "the const fallback completion must exist")?;
    ensure(
      (const_completion.text_edit.is_some(), const_completion.insert_text.is_some()) == (false, true),
      "an unmappable optional range must retain insert text without a replacement edit",
    )?;

    let default_schema = json!({
        "enum": [],
        "const": null,
        "default": "fallback",
        "type": "boolean"
    });
    completions.clear();
    ensure_ok(
      add_value_completions(&default_schema, None, &mut completions, false),
      "an absent constant must fall through to a valid default",
    )?;
    ensure(
      completions.iter().map(|item| item.label.as_str()).collect::<Vec<_>>() == vec!["\"fallback\""],
      "empty enum and null const must fall through to a valid default",
    )?;

    let type_schema = json!({
        "enum": [null],
        "const": null,
        "default": null,
        "type": "boolean"
    });
    completions.clear();
    ensure_ok(
      add_value_completions(&type_schema, None, &mut completions, false),
      "absent literal values must fall through to type snippets",
    )?;
    ensure(
      completions.iter().map(|item| item.label.as_str()).collect::<Vec<_>>() == vec!["true", "false"],
      "null enum, const, and default values must fall through to type snippets",
    )?;
    ensure(
      ensure_ok(
        schema_value_to_toml(&Value::Null, false),
        "JSON null is an absent TOML completion value",
      )?
      .is_none(),
      "null must never become bogus TOML completion text",
    )?;
    ensure(
      matches!(
        schema_value_to_toml(&json!({ "bad": null }), false),
        Err(CompletionError::Decode { .. })
      ),
      "a nonconvertible composite value must return a typed conversion error",
    )
  }

  #[test]
  fn schema_shape_filters_accept_only_matching_container_contracts() -> Result<(), TestFailure> {
    for (schema, expected) in [
      (json!({}), true),
      (json!({ "type": null }), true),
      (json!({ "type": "object" }), true),
      (json!({ "type": ["string", "object"] }), true),
      (json!({ "type": "string" }), false),
      (json!({ "type": ["string", "boolean"] }), false),
    ] {
      ensure(
        accepts_object(&schema) == expected,
        "object completion eligibility must follow the schema type declaration",
      )?;
    }
    for (schema, expected) in [
      (json!({ "type": "array" }), true),
      (json!({ "type": "array", "items": {} }), true),
      (json!({ "type": "array", "items": { "type": null } }), true),
      (json!({ "type": "array", "items": { "type": "object" } }), true),
      (json!({ "type": "array", "items": { "type": "string" } }), false),
      (json!({ "type": "object", "items": { "type": "object" } }), false),
    ] {
      ensure(
        accepts_array_of_objects(&schema) == expected,
        "array-table completion eligibility must require array items that permit objects",
      )?;
    }
    Ok(())
  }

  #[test]
  fn header_builders_filter_existing_and_incompatible_schema_paths() -> Result<(), TestFailure> {
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
    let completions = ensure_ok(
      table_header_completions(&document.dom, schemas.clone(), Some(&replacement), &current),
      "standard table-header completions must build",
    )?;
    ensure(
      labels(&completions) == ["missing", "pseudo"],
      "standard table headers must omit concrete existing paths and non-object schemas",
    )?;
    let missing = ensure_some(completions.first(), "the missing table completion must exist")?;
    ensure(
      (
        missing.kind,
        missing.insert_text.as_deref(),
        markdown(missing.documentation.as_ref()),
        simple_text_edit(missing),
      ) == (
        Some(CompletionItemKind::STRUCT),
        Some("missing"),
        Some("missing table"),
        Some((&replacement, "missing")),
      ),
      "a table-header completion must carry its structural kind, documentation, insertion, and replacement edit",
    )?;

    let existing = ensure_ok("existing".parse::<Keys>(), "the existing header path must parse")?;
    let current_completions = ensure_ok(
      table_header_completions(&document.dom, schemas, None, &existing),
      "the current table header must remain completable",
    )?;
    ensure(
      labels(&current_completions) == ["existing", "missing", "pseudo"],
      "the currently edited concrete header must remain eligible while unrelated concrete paths stay filtered",
    )?;

    let array_schemas = vec![
      completion_schema("items", "items", json!({ "type": "array", "items": { "type": "object" } }))?,
      completion_schema("open", "open", json!({ "type": "array" }))?,
      completion_schema("strings", "strings", json!({ "type": "array", "items": { "type": "string" } }))?,
      completion_schema("object", "object", json!({ "type": "object" }))?,
    ];
    let array_completions = ensure_ok(
      array_table_header_completions(array_schemas, None),
      "array-table header completions must build",
    )?;
    ensure(
      labels(&array_completions) == ["items", "open"],
      "array-table headers must retain only schemas whose array items permit objects",
    )?;
    ensure(
      matches!(
        header_completion(&existing, &json!({ "x-taplo": true }), None,),
        Err(CompletionError::SchemaExtension(_))
      ),
      "malformed Taplo metadata must remain a typed header-completion failure",
    )
  }

  #[test]
  fn entry_builders_preserve_presence_and_replacement_contracts() -> Result<(), TestFailure> {
    let document = parse_document("existing = 1\n[pseudo.child]\n", "the entry-completion DOM fixture must parse")?;
    let schemas = vec![
      completion_schema("existing", "existing", json!({ "default": 2 }))?,
      completion_schema("missing", "missing", json!({ "type": "string", "description": "missing entry" }))?,
      completion_schema("pseudo", "pseudo", json!({ "type": "object" }))?,
    ];
    let new_entries = ensure_ok(
      new_entry_completions(&document.dom, schemas.clone()),
      "new-entry completions must build",
    )?;
    ensure(
      (
        labels(&new_entries),
        new_entries
          .iter()
          .map(|completion| (completion.kind, completion.insert_text_format))
          .collect::<Vec<_>>(),
      ) == (vec!["missing", "pseudo"], vec![
        (Some(CompletionItemKind::VARIABLE), Some(InsertTextFormat::SNIPPET)),
        (Some(CompletionItemKind::VARIABLE), Some(InsertTextFormat::SNIPPET)),
      ]),
      "new-entry completions must omit concrete paths and emit snippets for missing or pseudo paths",
    )?;
    let missing = ensure_some(new_entries.first(), "the missing entry completion must exist")?;
    ensure(
      (missing.insert_text.as_deref(), markdown(missing.documentation.as_ref())) == (Some(r#"missing = "$0""#), Some("missing entry")),
      "a new entry completion must retain its schema-directed snippet and documentation",
    )?;

    let replacement = Range::default();
    let standalone = ensure_ok(
      standalone_key_completions(&document.dom, schemas, Some(&replacement)),
      "standalone-key completions must build",
    )?;
    ensure(
      (
        labels(&standalone),
        standalone
          .iter()
          .map(|completion| completion.text_edit.is_some())
          .collect::<Vec<_>>(),
      ) == (vec!["missing", "pseudo"], vec![true, true]),
      "standalone-key completion must replace only missing or pseudo paths",
    )
  }

  #[test]
  fn entry_builders_preserve_equals_and_value_contracts() -> Result<(), TestFailure> {
    let replacement = Range::default();
    let key_schema = vec![completion_schema("feature", "feature", json!({ "default": true }))?];
    let assigned = ensure_ok(
      entry_key_completions(key_schema.clone(), Some(&replacement), true),
      "assigned entry-key completions must build",
    )?;
    let assigned_item = ensure_some(assigned.first(), "the assigned key completion must exist")?;
    ensure(
      (
        assigned_item.insert_text.as_deref(),
        assigned_item.insert_text_format,
        simple_text_edit(assigned_item),
      ) == (Some("feature "), None, Some((&replacement, "feature "))),
      "an assigned key completion must replace only the key and retain the equals-sign boundary",
    )?;
    let unassigned = ensure_ok(
      entry_key_completions(key_schema, None, false),
      "unassigned entry-key completions must build",
    )?;
    let unassigned_item = ensure_some(unassigned.first(), "the unassigned key completion must exist")?;
    let feature_snippet = snippet_placeholder(0, "true");
    let expected_entry_insert = format!("feature = {feature_snippet}");
    ensure(
      (
        unassigned_item.insert_text.as_deref(),
        unassigned_item.insert_text_format,
        unassigned_item.text_edit.is_some(),
      ) == (Some(expected_entry_insert.as_str()), Some(InsertTextFormat::SNIPPET), false),
      "an unassigned key completion must insert a complete schema-directed entry snippet",
    )?;

    let values = ensure_ok(
      entry_value_completions(
        vec![
          completion_schema("choice", "choice", json!({ "const": "fixed" }))?,
          completion_schema("flag", "flag", json!({ "type": "boolean" }))?,
        ],
        Some(&replacement),
        true,
      ),
      "entry-value completions must build",
    )?;
    ensure(
      (
        labels(&values),
        values
          .iter()
          .map(|completion| completion.text_edit.is_some())
          .collect::<Vec<_>>(),
      ) == (vec!["'fixed'", "true", "false"], vec![true, true, true]),
      "value completion must combine schema branches, honor literal-quote preference, and retain replacement edits",
    )
  }

  #[test]
  fn empty_value_snippets_cover_every_schema_type_polarity() -> Result<(), TestFailure> {
    let boolean_placeholder = snippet_placeholder(3, "false");
    for (schema, expected) in [
      (json!({ "$ref": "#/$defs/value" }), "$3"),
      (json!({}), "{ $3 }"),
      (json!({ "type": null }), "{ $3 }"),
      (json!({ "type": ["string", "boolean"] }), "$3"),
      (json!({ "type": "object" }), "{ $3 }"),
      (json!({ "type": "array" }), "[$3]"),
      (json!({ "type": "string" }), "\"$3\""),
      (json!({ "type": "boolean" }), boolean_placeholder.as_str()),
      (json!({ "type": "integer" }), "$3"),
    ] {
      let snippet = empty_value_snippet(&schema, 3);
      ensure_eq(
        &snippet.as_str(),
        &expected,
        "empty-value snippets must follow the schema type without inventing concrete values",
      )?;
    }
    let missing_property = ensure_ok(
      default_value_snippet(
        &json!({
          "type": "object",
          "required": ["missing"],
          "properties": {}
        }),
        0,
        false,
      ),
      "a required key without a property schema must retain a nested object placeholder",
    )?;
    ensure_eq(
      &missing_property.as_ref(),
      &"{ missing = { $1 } }$0",
      "a required key without a property schema must receive the generic nested object snippet",
    )
  }

  #[test]
  fn schema_value_conversion_preserves_value_kind() -> Result<(), TestFailure> {
    let scalar = ensure_some(
      ensure_ok(
        schema_value_to_toml(&json!(1), false),
        "an integer schema value must decode and render",
      )?,
      "an integer schema value must convert",
    )?;
    ensure(
      scalar == ("1".into(), CompletionItemKind::VALUE),
      "scalar schema values must produce value completions",
    )?;
    let object = ensure_some(
      ensure_ok(
        schema_value_to_toml(&json!({ "key": 1 }), false),
        "an object schema value must decode and render",
      )?,
      "an object schema value must convert",
    )?;
    ensure(
      object.1 == CompletionItemKind::STRUCT,
      "object schema values must produce structural completions",
    )
  }

  #[test]
  fn recursive_default_snippets_use_safe_precedence_and_saturating_cursors() -> Result<(), TestFailure> {
    ensure_eq(
      &ensure_ok(
        default_value_snippet(&json!({ "const": "fixed", "default": "other", "type": "string" }), 0, false),
        "a valid constant must produce a default snippet",
      )?
      .as_ref(),
      &r#"${0:"fixed"}"#,
      "valid const must win over default and type snippet",
    )?;
    ensure_eq(
      &ensure_ok(
        default_value_snippet(&json!({ "const": null, "default": true, "type": "string" }), 0, false),
        "a valid default must produce a snippet",
      )?
      .as_ref(),
      &snippet_placeholder(0, "true").as_str(),
      "null const must fall through to a valid default",
    )?;
    ensure_eq(
      &ensure_ok(
        default_value_snippet(&json!({ "enum": [null, 1], "type": "string" }), 0, false),
        "a partially representable enum must produce a placeholder",
      )?
      .as_ref(),
      &"$0",
      "an enum placeholder must be used only when at least one value converts",
    )?;
    ensure_eq(
      &ensure_ok(
        default_value_snippet(&json!({ "enum": [null], "type": "string" }), 0, false),
        "an enum without TOML values must fall through",
      )?
      .as_ref(),
      &r#""$0""#,
      "an all-invalid enum must fall through to type-derived syntax",
    )?;

    let recursive = ensure_ok(
      default_value_snippet(
        &json!({
            "type": "object",
            "required": ["name"],
            "properties": { "name": { "const": "fixed" } }
        }),
        usize::MAX,
        false,
      ),
      "recursive object defaults must produce a snippet",
    )?;
    ensure_contains(
      recursive.as_ref(),
      &format!("${{{}:\"fixed\"}}", usize::MAX),
      "recursive cursor numbering must saturate instead of overflowing",
    )?;
    let multi_key = ensure_ok(
      default_value_snippet(
        &json!({
          "type": "object",
          "required": ["name", "enabled"],
          "properties": {
            "name": {
              "const": "fixed"
            },
            "enabled": {
              "type": "boolean"
            }
          }
        }),
        0,
        false,
      ),
      "an object with multiple required keys must produce one complete snippet",
    )?;
    ensure_eq(
      &multi_key.as_ref(),
      &r#"{ name = ${1:"fixed"}, enabled = ${1:false} }$0"#,
      "multi-key object snippets must separate every required entry while sharing the nested cursor depth",
    )?;
    ensure_eq(
      &completion_depth(usize::MAX, 1),
      &usize::MAX,
      "completion traversal depth must saturate at the platform maximum",
    )
  }
}
