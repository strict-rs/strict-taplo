//! Standalone schema documentation links over immutable document snapshots.

use crate::world::World;
use lsp_async_stub::{rpc::Error, util::Mapper, Context, Params};
use lsp_types::{DocumentLink, DocumentLinkParams};
use taplo::dom::{node::Key, KeyOrIndex};
use taplo_common::{environment::Environment, schema::ext::schema_ext_of};
use url::Url;

#[tracing::instrument(skip_all)]
pub async fn links<E: Environment>(
    context: Context<World<E>>,
    params: Params<DocumentLinkParams>,
) -> Result<Option<Vec<DocumentLink>>, Error> {
    let params = params.required()?;
    let Some(document_uri) = crate::uri::to_url(&params.text_document.uri) else {
        return Ok(None);
    };
    let Some(snapshot) = context.document_snapshot(&document_uri).await else {
        return Ok(None);
    };
    if !snapshot.config.schema.enabled || !snapshot.config.schema.links {
        return Ok(None);
    }
    let Some(association) = snapshot
        .schemas
        .associations()
        .association_for(&document_uri)
    else {
        return Ok(Some(Vec::new()));
    };

    let mut links = Vec::new();
    for (keys, last_key, node) in snapshot.document.dom.flat_iter().filter_map(|(keys, node)| {
        match keys.iter().last().cloned() {
            Some(KeyOrIndex::Key(last_key)) => Some((keys, last_key, node)),
            _ => None,
        }
    }) {
        let value = match serde_json::to_value(&node) {
            Ok(value) => value,
            Err(error) => {
                tracing::debug!(%error, "invalid TOML value");
                continue;
            }
        };
        let schemas = match snapshot
            .schemas
            .schemas_at_path(&association.url, &value, &keys)
            .await
        {
            Ok(schemas) => schemas,
            Err(error) => {
                tracing::error!(%error, "failed to collect schemas");
                continue;
            }
        };
        for (_, schema) in schemas {
            links.extend(key_document_links(
                &schema,
                &last_key,
                &snapshot.document.mapper,
            ));
        }
    }
    Ok(Some(links))
}

/// Build standalone links for every mappable source occurrence of one schema-backed key.
fn key_document_links(schema: &serde_json::Value, key: &Key, mapper: &Mapper) -> Vec<DocumentLink> {
    let Some(key_link) = schema_ext_of(schema)
        .and_then(|extension| extension.links)
        .and_then(|external| external.key)
    else {
        return Vec::new();
    };
    let Some(target) = Url::parse(&key_link)
        .ok()
        .and_then(|url| crate::uri::to_uri(&url))
    else {
        tracing::warn!(%key_link, "invalid standalone schema link target");
        return Vec::new();
    };
    key.text_ranges()
        .filter_map(|source_range| {
            let range = crate::uri::to_lsp_range(mapper, source_range)?;
            Some(DocumentLink {
                range,
                target: Some(target.clone()),
                tooltip: None,
                data: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::key_document_links;
    use lsp_async_stub::util::Mapper;
    use strict_test_support::{ensure, ensure_some, TestFailure};
    use taplo::dom::{KeyOrIndex, Keys};

    /// Extract the first real key from one parsed document path.
    fn key(source: &str, path: &str) -> Result<taplo::dom::node::Key, TestFailure> {
        let dom = taplo::parser::parse(source).into_dom();
        let keys: Keys = path
            .parse()
            .map_err(|error: taplo::dom::Error| TestFailure::WasErr {
                context: "the document-link fixture path must parse",
                cause: error.to_string(),
            })?;
        ensure_some(
            keys.iter().find_map(|part| match part {
                KeyOrIndex::Key(key) => Some(key.clone()),
                KeyOrIndex::Index(_) => None,
            }),
            "the document-link fixture must contain a key",
        )
        .and_then(|parsed_key| {
            ensure(
                dom.path(&keys).is_some(),
                "the document-link fixture path must exist",
            )?;
            Ok(parsed_key)
        })
    }

    #[test]
    fn standalone_links_require_valid_targets_and_mappable_ranges() -> Result<(), TestFailure> {
        let source = "setting = 1\n";
        let key = key(source, "setting")?;
        let schema = serde_json::json!({
            "x-taplo": { "links": { "key": "https://example.com/docs" } }
        });
        let links = key_document_links(&schema, &key, &Mapper::new_utf16(source, false));
        let link = ensure_some(links.first(), "a valid standalone link must be emitted")?;
        ensure(
            link.range
                == lsp_types::Range::new(
                lsp_types::Position::new(0, 0),
                lsp_types::Position::new(0, 7),
            ),
            "standalone link range must cover the exact key source",
        )?;
        ensure(
            link.target
                .as_ref()
                .is_some_and(|target| target.as_str() == "https://example.com/docs"),
            "standalone link target must retain the schema URL",
        )?;

        ensure(
            key_document_links(
                &serde_json::json!({
                    "x-taplo": { "links": { "key": "not a URL" } }
                }),
                &key,
                &Mapper::new_utf16(source, false),
            )
            .is_empty(),
            "an invalid target URL must be skipped",
        )?;
        ensure(
            key_document_links(&schema, &key, &Mapper::new_utf16("", false)).is_empty(),
            "an unmappable key range must be skipped",
        )?;
        ensure(
            key_document_links(
                &serde_json::json!({ "description": "no standalone link" }),
                &key,
                &Mapper::new_utf16(source, false),
            )
            .is_empty(),
            "a schema without an external key link must emit nothing",
        )
    }
}
