//! Progressive syntax, DOM, and schema diagnostics over lock-free document snapshots.

use crate::world::{DocumentSnapshot, DocumentState, World};
use lsp_async_stub::{Context, RequestWriter};
use lsp_types::{
    notification, Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, Location,
    PublishDiagnosticsParams, Uri,
};
use taplo::dom::node::Key;
use taplo_common::environment::Environment;
use url::Url;

/// Publish syntax, then DOM, then schema diagnostics, stopping after the first failing phase.
#[tracing::instrument(skip_all, fields(%document_url))]
pub(crate) async fn publish_diagnostics<E: Environment>(
    mut context: Context<World<E>>,
    document_url: Url,
) {
    let Some(document_uri) = crate::uri::to_uri(&document_url) else {
        tracing::warn!(%document_url, "document URL is not representable as an LSP URI");
        return;
    };

    let Some(snapshot) = context.document_snapshot(&document_url).await else {
        return;
    };
    let syntax = collect_syntax_errors(&snapshot.document);
    publish_phase(&mut context, document_uri.clone(), syntax.clone()).await;
    if !syntax.is_empty() {
        return;
    }

    let Some(snapshot) = context.document_snapshot(&document_url).await else {
        return;
    };
    let dom = collect_dom_errors(&snapshot.document, &document_uri);
    publish_phase(&mut context, document_uri.clone(), dom.clone()).await;
    if !dom.is_empty() {
        return;
    }

    let Some(snapshot) = context.document_snapshot(&document_url).await else {
        return;
    };
    let schema = collect_schema_errors(&snapshot, &document_url).await;
    publish_phase(&mut context, document_uri, schema).await;
}

/// Clear diagnostics for one closed document when its URL is wire-representable.
#[tracing::instrument(skip_all)]
pub(crate) async fn clear_diagnostics<E: Environment>(
    mut context: Context<World<E>>,
    document_url: Url,
) {
    let Some(uri) = crate::uri::to_uri(&document_url) else {
        tracing::warn!(%document_url, "document URL is not representable as an LSP URI");
        return;
    };
    publish_phase(&mut context, uri, Vec::new()).await;
}

/// Send one progressive diagnostics phase.
async fn publish_phase<E: Environment>(
    context: &mut Context<World<E>>,
    uri: Uri,
    diagnostics: Vec<Diagnostic>,
) {
    if let Err(error) = context
        .write_notification::<notification::PublishDiagnostics, _>(Some(
            PublishDiagnosticsParams {
                uri,
                diagnostics,
                version: None,
            },
        ))
        .await
    {
        tracing::error!(%error, "failed to publish diagnostics");
    }
}

/// Convert parser diagnostics, skipping only ranges that cannot be represented on the wire.
fn collect_syntax_errors(document: &DocumentState) -> Vec<Diagnostic> {
    document
        .parse
        .errors
        .iter()
        .filter_map(|error| {
            let range = crate::uri::to_lsp_range(&document.mapper, error.range)?;
            Some(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("Even Better TOML".into()),
                message: error.message.clone(),
                ..Diagnostic::default()
            })
        })
        .collect()
}

/// Convert semantic DOM diagnostics without fake ranges or partial related-information pairs.
fn collect_dom_errors(document: &DocumentState, document_uri: &Uri) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let Err(errors) = document.dom.validate() else {
        return diagnostics;
    };

    for error in errors {
        match &error {
            taplo::dom::Error::ConflictingKeys { key, other } => push_paired_dom_error(
                document,
                document_uri,
                PairedDomDiagnostic {
                    primary_key: key,
                    related_key: other,
                    message: error.to_string(),
                    primary_relation: "other key defined here",
                    related_relation: "other key defined here",
                },
                &mut diagnostics,
            ),
            taplo::dom::Error::ExpectedTable {
                not_table,
                required_by,
            } => push_paired_dom_error(
                document,
                document_uri,
                PairedDomDiagnostic {
                    primary_key: not_table,
                    related_key: required_by,
                    message: error.to_string(),
                    primary_relation: "required by this key",
                    related_relation: "table defined here",
                },
                &mut diagnostics,
            ),
            taplo::dom::Error::ExpectedArrayOfTables {
                not_array_of_tables,
                required_by,
            } => push_paired_dom_error(
                document,
                document_uri,
                PairedDomDiagnostic {
                    primary_key: not_array_of_tables,
                    related_key: required_by,
                    message: error.to_string(),
                    primary_relation: "required by this key",
                    related_relation: "array of tables defined here",
                },
                &mut diagnostics,
            ),
            taplo::dom::Error::InvalidEscapeSequence { .. }
            | taplo::dom::Error::Query(_) => {}
            taplo::dom::Error::UnexpectedSyntax { syntax } => {
                tracing::error!(?syntax, "unexpected syntax in DOM");
            }
        }
    }
    diagnostics
}

/// Source keys and relationship labels for one two-sided semantic diagnostic.
struct PairedDomDiagnostic<'a> {
    /// Primary key highlighted as the error.
    primary_key: &'a Key,
    /// Related key highlighted as contextual information.
    related_key: &'a Key,
    /// Shared diagnostic message.
    message: String,
    /// Relationship shown from the primary key.
    primary_relation: &'static str,
    /// Relationship shown from the related key.
    related_relation: &'static str,
}

/// Add both sides of a two-range DOM diagnostic only when both endpoints map successfully.
fn push_paired_dom_error(
    document: &DocumentState,
    document_uri: &Uri,
    diagnostic: PairedDomDiagnostic<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(primary_source) = diagnostic.primary_key.text_ranges().next() else {
        return;
    };
    let Some(related_source) = diagnostic.related_key.text_ranges().next() else {
        return;
    };
    let Some(primary) = crate::uri::to_lsp_range(&document.mapper, primary_source) else {
        return;
    };
    let Some(related) = crate::uri::to_lsp_range(&document.mapper, related_source) else {
        return;
    };

    diagnostics.push(Diagnostic {
        range: primary,
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some("Even Better TOML".into()),
        message: diagnostic.message.clone(),
        related_information: Some(vec![DiagnosticRelatedInformation {
            location: Location {
                uri: document_uri.clone(),
                range: related,
            },
            message: diagnostic.primary_relation.into(),
        }]),
        ..Diagnostic::default()
    });
    diagnostics.push(Diagnostic {
        range: related,
        severity: Some(DiagnosticSeverity::HINT),
        source: Some("Even Better TOML".into()),
        message: diagnostic.message,
        related_information: Some(vec![DiagnosticRelatedInformation {
            location: Location {
                uri: document_uri.clone(),
                range: primary,
            },
            message: diagnostic.related_relation.into(),
        }]),
        ..Diagnostic::default()
    });
}

/// Validate a clean snapshot against its active schema association.
async fn collect_schema_errors<E: Environment>(
    snapshot: &DocumentSnapshot<E>,
    document_url: &Url,
) -> Vec<Diagnostic> {
    if !snapshot.config.schema.enabled {
        return Vec::new();
    }
    let Some(association) = snapshot
        .schemas
        .associations()
        .association_for(document_url)
    else {
        return Vec::new();
    };

    let errors = match snapshot
        .schemas
        .validate_root(&association.url, &snapshot.document.dom)
        .await
    {
        Ok(errors) => errors,
        Err(error) => {
            tracing::error!(%error, "schema validation failed");
            return Vec::new();
        }
    };
    errors
        .into_iter()
        .filter_map(|error| {
            let source = error.text_ranges().next()?;
            let range = crate::uri::to_lsp_range(&snapshot.document.mapper, source)?;
            Some(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("Even Better TOML".into()),
                message: error.message,
                ..Diagnostic::default()
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{collect_dom_errors, collect_schema_errors, collect_syntax_errors};
    use crate::{
        config::LspConfig,
        test_support::TestEnvironment,
        world::{DocumentSnapshot, DocumentState},
    };
    use lsp_async_stub::util::Mapper;
    use std::sync::Arc;
    use strict_test_support::{ensure, ensure_ok, ensure_some, TestFailure};
    use taplo_common::{
        config::Config,
        schema::{
            associations::{priority, source, AssociationRule, SchemaAssociation},
            Schemas,
        },
    };
    use url::Url;

    /// Build a document whose parser, DOM, and mapper share one source.
    fn document(source: &str) -> DocumentState {
        let parse = taplo::parser::parse(source);
        let dom = parse.clone().into_dom();
        DocumentState {
            parse,
            dom,
            mapper: Mapper::new_utf16(source, false),
        }
    }

    /// Parse one URL fixture.
    fn url(value: &str) -> Result<Url, TestFailure> {
        ensure_ok(Url::parse(value), "the diagnostics fixture URL must parse")
    }

    #[test]
    fn syntax_and_dom_diagnostics_map_only_real_source_ranges() -> Result<(), TestFailure> {
        let invalid_source = "value = [\n";
        let invalid = document(invalid_source);
        ensure(
            !collect_syntax_errors(&invalid).is_empty(),
            "mappable parser errors must produce syntax diagnostics",
        )?;
        let invalid_unmapped = DocumentState {
            mapper: Mapper::new_utf16("", false),
            ..invalid.clone()
        };
        ensure(
            collect_syntax_errors(&invalid_unmapped).is_empty(),
            "parser errors absent from the mapper must be skipped",
        )?;

        let document_url = url("file:///workspace/file.toml")?;
        let document_uri = ensure_some(
            crate::uri::to_uri(&document_url),
            "the diagnostics document URL must map to an LSP URI",
        )?;
        let conflicting = document("key = 1\nkey = 2\n");
        ensure(
            conflicting.parse.errors.is_empty(),
            "the DOM conflict fixture must be syntactically clean",
        )?;
        let mapped = collect_dom_errors(&conflicting, &document_uri);
        ensure(
            mapped.len() == 2,
            "a two-sided key conflict must produce its primary and related diagnostics",
        )?;
        let conflicting_unmapped = DocumentState {
            mapper: Mapper::new_utf16("", false),
            ..conflicting
        };
        ensure(
            collect_dom_errors(&conflicting_unmapped, &document_uri).is_empty(),
            "an unmappable member must suppress the whole paired DOM diagnostic",
        )
    }

    #[test]
    fn schema_diagnostics_require_clean_owned_schema_inputs() -> Result<(), TestFailure> {
        let environment = TestEnvironment::default();
        let schemas = Schemas::new_offline(environment);
        let schema_url = url("https://example.com/schema.json")?;
        let document_url = url("file:///workspace/file.toml")?;

        futures::executor::block_on(async {
            schemas
                .add_schema(
                    &schema_url,
                    Arc::new(serde_json::json!({
                        "type": "object",
                        "properties": { "value": { "type": "integer" } }
                    })),
                )
                .await;
            schemas.associations().add(
                AssociationRule::Url(document_url.clone()),
                SchemaAssociation {
                    url: schema_url,
                    meta: serde_json::json!({ "source": source::MANUAL }),
                    priority: priority::MAX,
                },
            );
            let snapshot = DocumentSnapshot {
                document: document("value = \"text\"\n"),
                schemas: schemas.clone(),
                config: LspConfig::default(),
                taplo_config: Config::default(),
            };
            ensure(
                snapshot.document.parse.errors.is_empty(),
                "the schema-validation fixture must be syntactically clean",
            )?;
            ensure(
                snapshot.document.dom.validate().is_ok(),
                "the schema-validation fixture must be semantically valid TOML",
            )?;
            ensure(
                !collect_schema_errors(&snapshot, &document_url).await.is_empty(),
                "a clean document violating its associated schema must produce diagnostics",
            )?;

            let unrelated = url("file:///workspace/unrelated.toml")?;
            ensure(
                collect_schema_errors(&snapshot, &unrelated).await.is_empty(),
                "a document without an active association must not be schema-validated",
            )?;

            let mut disabled = snapshot;
            disabled.config.schema.enabled = false;
            ensure(
                collect_schema_errors(&disabled, &document_url).await.is_empty(),
                "schema disablement must suppress exposure without altering ownership",
            )
        })
    }
}
