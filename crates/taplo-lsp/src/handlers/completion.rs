use lsp_async_stub::{rpc::Error, Context, Params};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionParams, CompletionResponse, CompletionTextEdit,
    Documentation, InsertTextFormat, MarkupContent, Range, TextEdit,
};
use serde_json::Value;
use std::borrow::Cow;
use taplo::dom::{node::TableKind, Keys, Node};
use taplo_common::{
    environment::Environment,
    schema::{ext::schema_ext_of, ValueExt},
};

use crate::{
    query::{lookup_keys, Query},
    world::World,
};

#[tracing::instrument(skip_all)]
pub async fn completion<E: Environment>(
    context: Context<World<E>>,
    params: Params<CompletionParams>,
) -> Result<Option<CompletionResponse>, Error> {
    let p = params.required()?;

    let Some(document_uri) = crate::uri::to_url(&p.text_document_position.text_document.uri)
    else {
        return Ok(None);
    };

    let Some(snapshot) = context.document_snapshot(&document_uri).await else {
        return Ok(None);
    };

    // All completions are tied to schemas.
    if !snapshot.config.schema.enabled {
        return Ok(None);
    }

    let doc = &snapshot.document;

    let Some(schema_association) = snapshot
        .schemas
        .associations()
        .association_for(&document_uri)
    else {
        return Ok(None);
    };

    let position = p.text_document_position.position;
    let Some(offset) = doc.mapper.offset(crate::uri::from_lsp_position(position)) else {
        tracing::error!(?position, "document position not found");
        return Ok(None);
    };

    let query = Query::at(&doc.dom, offset);

    let value = match serde_json::to_value(&doc.dom) {
        Ok(v) => v,
        Err(error) => {
            tracing::warn!(%error, "unable to serialize DOM");
            Value::Null
        }
    };

    if query.in_table_header() {
        let key_count = query.header_keys().len();

        let object_schemas = match snapshot
            .schemas
            .possible_schemas_from(
                &schema_association.url,
                &value,
                &Keys::empty(),
                completion_depth(key_count, snapshot.config.completion.max_keys),
            )
            .await
            .map(|s| {
                s.into_iter().filter(|(_, _, s)| {
                    s["type"].is_null()
                        || s["type"] == "object"
                        || s["type"]
                            .as_array()
                            .is_some_and(|arr| arr.iter().any(|v| v == "object"))
                })
            }) {
            Ok(s) => s,
            Err(error) => {
                tracing::error!(?error, "failed to collect schemas");
                return Ok(None);
            }
        };

        let key_range = query
            .header_key()
            .map(|key| key.text_range())
            .filter(|range| !range.is_empty());
        let key_lsp_range = key_range.and_then(|range| crate::uri::to_lsp_range(&doc.mapper, range));

        let node = query
            .dom_node()
            .cloned()
            .unwrap_or_else(|| (Keys::empty(), doc.dom.clone()));

        return Ok(Some(CompletionResponse::Array(
            object_schemas
                // Filter out existing tables in the dom.
                .filter(|(full_key, _, _)| match doc.dom.path(full_key) {
                    Some(n) => {
                        node.0 == *full_key
                            || n.as_table().is_some_and(|t| t.kind() == TableKind::Pseudo)
                    }
                    None => true,
                })
                .map(|(full_key, _, s)| {
                    let text = full_key.to_string();
                    CompletionItem {
                    label: text.clone(),
                    kind: Some(CompletionItemKind::STRUCT),
                    documentation: documentation(&s),
                    insert_text: Some(text.clone()),
                    text_edit: key_lsp_range.map(|range| {
                        CompletionTextEdit::Edit(TextEdit {
                            range,
                            new_text: text,
                        })
                    }),
                    ..Default::default()
                }})
                .collect(),
        )));
    }

    if query.in_table_array_header() {
        let key_count = query.header_keys().len();
        let array_of_objects_schemas = match snapshot
            .schemas
            .possible_schemas_from(
                &schema_association.url,
                &value,
                &Keys::empty(),
                completion_depth(key_count, snapshot.config.completion.max_keys),
            )
            .await
            .map(|s| {
                s.into_iter().filter(|(_, _, s)| {
                    s["type"] == "array"
                        && (s["items"]["type"] == "object" || s["items"]["type"].is_null())
                })
            }) {
            Ok(s) => s,
            Err(error) => {
                tracing::error!(?error, "failed to collect schemas");
                return Ok(None);
            }
        };

        let key_range = query
            .header_key()
            .map(|key| key.text_range())
            .filter(|range| !range.is_empty());
        let key_lsp_range = key_range.and_then(|range| crate::uri::to_lsp_range(&doc.mapper, range));

        return Ok(Some(CompletionResponse::Array(
            array_of_objects_schemas
                .map(|(full_key, _, s)| {
                    let text = full_key.to_string();
                    CompletionItem {
                    label: text.clone(),
                    kind: Some(CompletionItemKind::STRUCT),
                    documentation: documentation(&s),
                    insert_text: Some(text.clone()),
                    text_edit: key_lsp_range.map(|range| {
                        CompletionTextEdit::Edit(TextEdit {
                            range,
                            new_text: text,
                        })
                    }),
                    ..Default::default()
                }})
                .collect(),
        )));
    }

    if query.empty_line() {
        let parent_table = query.parent_table_or_array_table(&doc.dom);

        let schemas = match snapshot
            .schemas
            .possible_schemas_from(
                &schema_association.url,
                &value,
                &lookup_keys(doc.dom.clone(), &parent_table.0),
                completion_depth(0, snapshot.config.completion.max_keys),
            )
            .await
        {
            Ok(s) => s,
            Err(error) => {
                tracing::error!(?error, "failed to collect schemas");
                return Ok(None);
            }
        };

        return Ok(Some(CompletionResponse::Array(
            schemas
                .into_iter()
                // Filter out existing items.
                .filter(|(full_key, _, _)| match doc.dom.path(full_key) {
                    Some(n) => n.as_table().is_some_and(|t| t.kind() == TableKind::Pseudo),
                    None => true,
                })
                .map(|(_, relative_keys, schema)| CompletionItem {
                    label: relative_keys.to_string(),
                    kind: Some(CompletionItemKind::VARIABLE),
                    documentation: documentation(&schema),
                    insert_text_format: Some(InsertTextFormat::SNIPPET),
                    insert_text: Some(new_entry_snippet(&relative_keys, &schema, false)),
                    ..Default::default()
                })
                .collect(),
        )));
    }

    if query.in_entry_keys() {
        let mut parent_keys = if let Some((k, _)) = query.dom_node() {
            k.clone()
        } else {
            query.parent_table_or_array_table(&doc.dom).0
        };

        let entry_keys = query.entry_keys();

        parent_keys = parent_keys.skip_right(entry_keys.len());

        let schemas = match snapshot
            .schemas
            .possible_schemas_from(
                &schema_association.url,
                &value,
                &lookup_keys(doc.dom.clone(), &parent_keys),
                completion_depth(entry_keys.len(), snapshot.config.completion.max_keys),
            )
            .await
        {
            Ok(s) => s,
            Err(error) => {
                tracing::error!(?error, "failed to collect schemas");
                return Ok(None);
            }
        };

        let key_range = query
            .entry_key()
            .map(|key| key.text_range())
            .filter(|range| !range.is_empty());
        let key_lsp_range = key_range.and_then(|range| crate::uri::to_lsp_range(&doc.mapper, range));

        let has_eq = query.entry_has_eq();

        return Ok(Some(CompletionResponse::Array(
            schemas
                .into_iter()
                .map(|(_, relative_keys, schema)| CompletionItem {
                    label: relative_keys.to_string(),
                    kind: Some(CompletionItemKind::VARIABLE),
                    documentation: documentation(&schema),
                    text_edit: key_lsp_range.map(|range| {
                        CompletionTextEdit::Edit(TextEdit {
                            range,
                            new_text: if has_eq {
                                relative_keys.to_string() + " "
                            } else {
                                new_entry_snippet(&relative_keys, &schema, false)
                            },
                        })
                    }),
                    insert_text: Some(if has_eq {
                        relative_keys.to_string() + " "
                    } else {
                        new_entry_snippet(&relative_keys, &schema, false)
                    }),
                    insert_text_format: if has_eq {
                        None
                    } else {
                        Some(InsertTextFormat::SNIPPET)
                    },
                    ..Default::default()
                })
                .collect(),
        )));
    }

    if query.in_entry_value() {
        let Some((path, _)) = query.dom_node() else {
            return Ok(None);
        };

        // Pretty much same as the entry on an empty line
        if query.in_inline_table() {
            let schemas = match snapshot
                .schemas
                .possible_schemas_from(
                    &schema_association.url,
                    &value,
                    &lookup_keys(doc.dom.clone(), path),
                    completion_depth(0, snapshot.config.completion.max_keys),
                )
                .await
            {
                Ok(s) => s,
                Err(error) => {
                    tracing::error!(?error, "failed to collect schemas");
                    return Ok(None);
                }
            };

            return Ok(Some(CompletionResponse::Array(
                schemas
                    .into_iter()
                    // Filter out existing items.
                    .filter(|(full_key, _, _)| match doc.dom.path(full_key) {
                        Some(n) => n.as_table().is_some_and(|t| t.kind() == TableKind::Pseudo),
                        None => true,
                    })
                    .map(|(_, relative_keys, schema)| CompletionItem {
                        label: relative_keys.to_string(),
                        kind: Some(CompletionItemKind::VARIABLE),
                        documentation: documentation(&schema),
                        insert_text_format: Some(InsertTextFormat::SNIPPET),
                        insert_text: Some(new_entry_snippet(&relative_keys, &schema, false)),
                        ..Default::default()
                    })
                    .collect(),
            )));
        }

        let path = if query.is_inline() {
            lookup_keys(doc.dom.clone(), &path.clone())
        } else {
            let parent = query.parent_table_or_array_table(&doc.dom);
            let entry_key = query.entry_keys();
            lookup_keys(doc.dom.clone(), &parent.0.extend(entry_key))
        };

        let schemas = match snapshot
            .schemas
            .possible_schemas_from(
                &schema_association.url,
                &value,
                &path,
                completion_depth(0, snapshot.config.completion.max_keys),
            )
            .await
        {
            Ok(s) => s,
            Err(error) => {
                tracing::error!(?error, "failed to collect schemas");
                return Ok(None);
            }
        };

        let range = if query.in_array() {
            None
        } else {
            query
                .entry_value()
                .map(|k| k.text_range())
                .and_then(|range| crate::uri::to_lsp_range(&doc.mapper, range))
        };

        let mut completions = Vec::new();

        for (_, _, schema) in schemas {
            add_value_completions(
                &schema,
                range,
                &mut completions,
                query.is_single_quote_value(),
            );
        }

        return Ok(Some(CompletionResponse::Array(completions)));
    }

    // Only standalone keys left.
    // Almost the same as an empty line except we need to replace the incomplete keys.
    let mut parent_keys = if let Some((k, _)) = query.dom_node() {
        k.clone()
    } else {
        query.parent_table_or_array_table(&doc.dom).0
    };

    let entry_keys = query.entry_keys();

    parent_keys = parent_keys.skip_right(entry_keys.len());

    let schemas = match snapshot
        .schemas
        .possible_schemas_from(
            &schema_association.url,
            &value,
            &lookup_keys(doc.dom.clone(), &parent_keys),
            completion_depth(0, snapshot.config.completion.max_keys),
        )
        .await
    {
        Ok(s) => s,
        Err(error) => {
            tracing::error!(?error, "failed to collect schemas");
            return Ok(None);
        }
    };

    let replacement_range = crate::uri::to_lsp_range(&doc.mapper, entry_keys.all_text_range());
    Ok(Some(CompletionResponse::Array(
        schemas
            .into_iter()
            // Filter out existing items.
            .filter(|(full_key, _, _)| match doc.dom.path(full_key) {
                Some(n) => n.as_table().is_some_and(|t| t.kind() == TableKind::Pseudo),
                None => true,
            })
            .map(|(_, relative_keys, schema)| {
                let text = new_entry_snippet(&relative_keys, &schema, false);
                CompletionItem {
                label: relative_keys.to_string(),
                kind: Some(CompletionItemKind::VARIABLE),
                documentation: documentation(&schema),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                insert_text: Some(text.clone()),
                text_edit: replacement_range.map(|range| CompletionTextEdit::Edit(TextEdit {
                    range,
                    new_text: text,
                })),
                ..Default::default()
            }})
            .collect(),
    )))
}

fn documentation(schema: &Value) -> Option<Documentation> {
    schema_ext_of(schema)
        .and_then(|ext| ext.docs)
        .and_then(|docs| docs.main)
        .or_else(|| schema["description"].as_str().map(ToOwned::to_owned))
        .filter(|docs| !docs.is_empty())
        .map(markdown_documentation)
}

fn markdown_documentation(value: String) -> Documentation {
    Documentation::MarkupContent(MarkupContent {
        kind: lsp_types::MarkupKind::Markdown,
        value,
    })
}

fn completion_depth(prefix_length: usize, max_keys: usize) -> usize {
    prefix_length.saturating_add(max_keys)
}

fn schema_value_to_toml(
    value: &Value,
    single_quote: bool,
) -> Option<(String, CompletionItemKind)> {
    if value.is_null() {
        return None;
    }

    match serde_json::from_value::<Node>(value.clone()) {
        Ok(node) => {
            match serde_json::to_value(&node) {
                Ok(round_trip) if round_trip == *value => {}
                Ok(round_trip) => {
                    tracing::warn!(?value, ?round_trip, "schema value loses data during TOML conversion");
                    return None;
                }
                Err(error) => {
                    tracing::warn!(%error, "converted schema value cannot be serialized for verification");
                    return None;
                }
            }
            let kind = if matches!(&node, Node::Table(_)) {
                CompletionItemKind::STRUCT
            } else {
                CompletionItemKind::VALUE
            };
            let mut text = String::new();
            if let Err(error) = node.to_toml_fmt(&mut text, true, single_quote) {
                tracing::warn!(%error, "schema value cannot be rendered as TOML");
                return None;
            }
            Some((text, kind))
        }
        Err(error) => {
            tracing::warn!(%error, "schema value cannot be represented as TOML");
            None
        }
    }
}

fn value_completion(
    text: String,
    kind: CompletionItemKind,
    docs: Option<String>,
    range: Option<Range>,
) -> CompletionItem {
    CompletionItem {
        label: text.clone(),
        kind: Some(kind),
        documentation: docs.filter(|docs| !docs.is_empty()).map(markdown_documentation),
        insert_text: Some(text.clone()),
        text_edit: range.map(|range| {
            CompletionTextEdit::Edit(TextEdit {
                range,
                new_text: text,
            })
        }),
        ..Default::default()
    }
}

fn snippet_completion(
    label: &str,
    text: &str,
    docs: Option<String>,
    range: Option<Range>,
) -> CompletionItem {
    CompletionItem {
        label: label.into(),
        kind: Some(CompletionItemKind::VALUE),
        documentation: docs.filter(|docs| !docs.is_empty()).map(markdown_documentation),
        insert_text: Some(text.into()),
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        text_edit: range.map(|range| {
            CompletionTextEdit::Edit(TextEdit {
                range,
                new_text: text.into(),
            })
        }),
        ..Default::default()
    }
}

fn add_value_completions(
    schema: &Value,
    range: Option<Range>,
    completions: &mut Vec<CompletionItem>,
    single_quote: bool,
) {
    let ext = schema_ext_of(schema).unwrap_or_default();
    let ext_docs = ext.docs.unwrap_or_default();
    let enum_docs = ext_docs.enum_values.unwrap_or_default();

    let schema_docs = ext_docs
        .main
        .or_else(|| schema["description"].as_str().map(Into::into));

    if let Some(enum_values) = schema["enum"].as_array() {
        let enum_completions = enum_values
            .iter()
            .enumerate()
            .filter_map(|(index, value)| {
                let (text, kind) = schema_value_to_toml(value, single_quote)?;
                let docs = enum_docs
                    .get(index)
                    .cloned()
                    .flatten()
                    .or_else(|| schema_docs.clone());
                let mut completion = value_completion(text.clone(), kind, docs, range);
                completion.sort_text = Some(format!("{index}{text}"));
                Some(completion)
            })
            .collect::<Vec<_>>();

        if !enum_completions.is_empty() {
            completions.extend(enum_completions);
            return;
        }
    }

    if let Some((text, kind)) = schema
        .get("const")
        .and_then(|value| schema_value_to_toml(value, single_quote))
    {
        completions.push(value_completion(
            text,
            kind,
            ext_docs.const_value.or_else(|| schema_docs.clone()),
            range,
        ));
        return;
    }

    if let Some((text, kind)) = schema
        .get("default")
        .and_then(|value| schema_value_to_toml(value, single_quote))
    {
        completions.push(value_completion(
            text,
            kind,
            ext_docs.default_value.or_else(|| schema_docs.clone()),
            range,
        ));
        return;
    }

    let types = match schema["type"].clone() {
        Value::Null => Vec::from([Value::String("object".into())]),
        Value::String(s) => Vec::from([Value::String(s)]),
        Value::Array(tys) => tys,
        _ => Vec::new(),
    };

    for schema_type in types.iter().filter_map(Value::as_str) {
        match schema_type {
            "string" => completions.push(snippet_completion(
                r#""""#,
                r#""$0""#,
                schema_docs.clone().or_else(|| Some("string".into())),
                range,
            )),
            "boolean" => {
                completions.push(snippet_completion(
                    "true",
                    "true$0",
                    schema_docs.clone().or_else(|| Some("true value".into())),
                    range,
                ));
                completions.push(snippet_completion(
                    "false",
                    "false$0",
                    schema_docs.clone().or_else(|| Some("false value".into())),
                    range,
                ));
            }
            "array" => completions.push(snippet_completion(
                "[]",
                "[$0]",
                schema_docs.clone().or_else(|| Some("array".into())),
                range,
            )),
            "object" => completions.push(snippet_completion(
                "{ }",
                "{ $0 }",
                schema_docs.clone().or_else(|| Some("object".into())),
                range,
            )),
            _ => {}
        }
    }
}

fn new_entry_snippet(keys: &Keys, schema: &Value, single_quote: bool) -> String {
    let value = default_value_snippet(schema, 0, single_quote);
    format!("{keys} = {value}")
}

fn default_value_snippet(
    schema: &Value,
    cursor_count: usize,
    single_quote: bool,
) -> Cow<'static, str> {
    if let Some((text, _)) = schema
        .get("const")
        .and_then(|value| schema_value_to_toml(value, single_quote))
    {
        return format!("${{{cursor_count}:{text}}}").into();
    }

    if let Some((text, _)) = schema
        .get("default")
        .and_then(|value| schema_value_to_toml(value, single_quote))
    {
        return format!("${{{cursor_count}:{text}}}").into();
    }

    if schema["enum"].as_array().is_some_and(|values| {
        values
            .iter()
            .any(|value| schema_value_to_toml(value, single_quote).is_some())
    }) {
        return format!("${cursor_count}").into();
    }

    let mut init_keys = schema_ext_of(schema)
        .and_then(|ext| ext.init_keys)
        .unwrap_or_default();

    if let Some(required) = schema["required"].as_array() {
        init_keys.extend(
            required
                .iter()
                .filter_map(|value| value.as_str().map(ToOwned::to_owned)),
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
            snippet.push_str(&default_value_snippet(
                &schema["properties"][init_key],
                nested_cursor,
                single_quote,
            ));
        }

        snippet.push_str(" }$0");
        return snippet.into();
    }

    empty_value_snippet(schema, cursor_count).into()
}

fn empty_value_snippet(schema: &Value, cursor_count: usize) -> String {
    if schema.is_schema_ref() {
        return format!("${cursor_count}");
    }

    match &schema["type"] {
        Value::Null => format!("{{ ${cursor_count} }}"),
        Value::String(value) => match value.as_str() {
            "object" => format!("{{ ${cursor_count} }}"),
            "array" => format!("[${cursor_count}]"),
            "string" => format!(r#""${cursor_count}""#),
            "boolean" => format!("${{{cursor_count}:false}}"),
            _ => format!("${cursor_count}"),
        },
        _ => format!("${cursor_count}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        add_value_completions, completion_depth, default_value_snippet, documentation,
        schema_value_to_toml,
    };
    use lsp_types::{CompletionItemKind, Documentation, MarkupContent, Range};
    use serde_json::{json, Value};
    use strict_test_support::{ensure, ensure_contains, ensure_eq, ensure_some, TestFailure};

    /// Extract rendered Markdown from one completion documentation value.
    fn markdown(docs: &Option<Documentation>) -> Option<&str> {
        match docs {
            Some(Documentation::MarkupContent(MarkupContent { value, .. })) => Some(value),
            Some(Documentation::String(value)) => Some(value),
            None => None,
        }
    }

    #[test]
    fn completion_documentation_prefers_extension_content() -> Result<(), TestFailure> {
        let schema = json!({
            "description": "description docs",
            "x-taplo": { "docs": { "main": "extension docs" } }
        });
        let docs = ensure_some(documentation(&schema), "completion docs must exist")?;
        ensure(
            markdown(&Some(docs)) == Some("extension docs"),
            "extension main docs must override schema description",
        )?;
        ensure(
            markdown(&documentation(&json!({ "description": "description docs" })))
                == Some("description docs"),
            "schema description must backfill absent extension docs",
        )?;
        ensure(
            documentation(&json!({ "description": "" })).is_none(),
            "empty documentation must be omitted",
        )
    }

    #[test]
    fn enum_completion_uses_only_convertible_values_and_suppresses_fallbacks(
    ) -> Result<(), TestFailure> {
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
        add_value_completions(&schema, Some(Range::default()), &mut completions, false);
        ensure_eq(
            &completions.len(),
            &2,
            "a nonempty convertible enum must suppress const, default, and type completions",
        )?;
        ensure(
            completions
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>()
                == vec!["1", "2"],
            "invalid enum values must be omitted while valid values retain declaration order",
        )?;
        let first = ensure_some(completions.first(), "the first enum completion must exist")?;
        let second = ensure_some(completions.get(1), "the second enum completion must exist")?;
        ensure(
            markdown(&first.documentation) == Some("one docs"),
            "enum documentation must remain aligned by original schema index",
        )?;
        ensure(
            markdown(&second.documentation) == Some("two docs"),
            "later enum documentation must remain aligned after an invalid value is skipped",
        )?;
        ensure(
            completions
                .iter()
                .all(|item| item.insert_text.is_some() && item.text_edit.is_some()),
            "mapped enum completions must support both insertion and replacement",
        )
    }

    #[test]
    fn invalid_enum_const_and_default_values_fall_through_in_order() -> Result<(), TestFailure> {
        let const_schema = json!({
            "enum": [null, { "bad": null }],
            "const": 3,
            "default": 4,
            "type": "string"
        });
        let mut completions = Vec::new();
        add_value_completions(&const_schema, None, &mut completions, false);
        ensure(
            completions
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>()
                == vec!["3"],
            "an all-invalid enum must fall through to a valid const",
        )?;
        let const_completion = ensure_some(
            completions.first(),
            "the const fallback completion must exist",
        )?;
        ensure(
            const_completion.text_edit.is_none() && const_completion.insert_text.is_some(),
            "an unmappable optional range must retain insert text without a replacement edit",
        )?;

        let default_schema = json!({
            "enum": [],
            "const": null,
            "default": "fallback",
            "type": "boolean"
        });
        completions.clear();
        add_value_completions(&default_schema, None, &mut completions, false);
        ensure(
            completions
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>()
                == vec!["\"fallback\""],
            "empty enum and null const must fall through to a valid default",
        )?;

        let type_schema = json!({
            "enum": [null],
            "const": { "bad": null },
            "default": null,
            "type": "boolean"
        });
        completions.clear();
        add_value_completions(&type_schema, None, &mut completions, false);
        ensure(
            completions
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>()
                == vec!["true", "false"],
            "invalid enum, const, and default values must fall through to type snippets",
        )?;
        ensure(
            schema_value_to_toml(&Value::Null, false).is_none(),
            "null must never become bogus TOML completion text",
        )?;
        ensure(
            schema_value_to_toml(&json!({ "bad": null }), false).is_none(),
            "a nonconvertible composite value must be omitted",
        )
    }

    #[test]
    fn schema_value_conversion_preserves_value_kind() -> Result<(), TestFailure> {
        let scalar = ensure_some(
            schema_value_to_toml(&json!(1), false),
            "an integer schema value must convert",
        )?;
        ensure(
            scalar == ("1".into(), CompletionItemKind::VALUE),
            "scalar schema values must produce value completions",
        )?;
        let object = ensure_some(
            schema_value_to_toml(&json!({ "key": 1 }), false),
            "an object schema value must convert",
        )?;
        ensure(
            object.1 == CompletionItemKind::STRUCT,
            "object schema values must produce structural completions",
        )
    }

    #[test]
    fn recursive_default_snippets_use_safe_precedence_and_saturating_cursors(
    ) -> Result<(), TestFailure> {
        ensure_eq(
            &default_value_snippet(
                &json!({ "const": "fixed", "default": "other", "type": "string" }),
                0,
                false,
            )
            .as_ref(),
            &r#"${0:"fixed"}"#,
            "valid const must win over default and type snippet",
        )?;
        ensure_eq(
            &default_value_snippet(
                &json!({ "const": null, "default": true, "type": "string" }),
                0,
                false,
            )
            .as_ref(),
            &"${0:true}",
            "null const must fall through to a valid default",
        )?;
        ensure_eq(
            &default_value_snippet(&json!({ "enum": [null, 1], "type": "string" }), 0, false)
                .as_ref(),
            &"$0",
            "an enum placeholder must be used only when at least one value converts",
        )?;
        ensure_eq(
            &default_value_snippet(&json!({ "enum": [null], "type": "string" }), 0, false)
                .as_ref(),
            &r#""$0""#,
            "an all-invalid enum must fall through to type-derived syntax",
        )?;

        let recursive = default_value_snippet(
            &json!({
                "type": "object",
                "required": ["name"],
                "properties": { "name": { "const": "fixed" } }
            }),
            usize::MAX,
            false,
        );
        ensure_contains(
            recursive.as_ref(),
            &format!("${{{}:\"fixed\"}}", usize::MAX),
            "recursive cursor numbering must saturate instead of overflowing",
        )?;
        ensure_eq(
            &completion_depth(usize::MAX, 1),
            &usize::MAX,
            "completion traversal depth must saturate at the platform maximum",
        )
    }
}
