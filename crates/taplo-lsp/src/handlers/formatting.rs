//! Document-formatting request handling over immutable world snapshots.

use lsp_types::DocumentFormattingParams;
use lsp_types::TextEdit;
use taplo::formatter;
use taplo::parser::Diagnostic as ParseDiagnostic;
use taplo_lsp_async::Params;
use taplo_lsp_async::rpc::RpcError;

use crate::world::WorldState;

/// Format one current document with merged client and Taplo configuration.
///
/// # Errors
///
/// Returns [`RpcError`] when parameters, paths, coordinates, formatting, or snapshot freshness
/// cannot be validated.
macro_rules! define_formatting_future_family {
  (
    $format:ident,
    $document_snapshot_for_uri:ident,
    $ensure_current_snapshot:ident;
    $future:ident,
    $environment:path,
    $transport:ident,
    $schema_execution:path;
    [$($value_bound:path),*]
  ) => {
    /// Format one current document with merged client and Taplo configuration.
    ///
    /// # Errors
    ///
    /// Returns [`RpcError`] when parameters, paths, coordinates, formatting, or snapshot freshness
    /// cannot be validated.
    #[allow(clippy::single_call_fn, reason = "one document-formatting entry point per execution family, registered exactly once by its runtime family")]
    pub(super) fn $format<E: $environment>(
      world: &WorldState<E, $transport<E>>,
      params: Params<DocumentFormattingParams>,
    ) -> $future<'_, Result<Option<Vec<TextEdit>>, RpcError>> {
      Box::pin(async move {
        let parameters = params.required()?;

        current_document_snapshot!(
          super::$document_snapshot_for_uri(world, &parameters.text_document.uri) => (document_uri, snapshot)
        );
        let document = &snapshot.document;

        let document_path = world
          .env
          .to_file_path_normalized(&document_uri)
          .map_err(|error| RpcError::invalid_request().with_details(error.to_string()))?
          .ok_or_else(|| {
            RpcError::invalid_request().with_details(format!(
              "invalid (non-local) uri for file: {document_uri}"
            ))
          })?;

        let tab_size = usize::try_from(parameters.options.tab_size).map_err(|source| {
          RpcError::invalid_params().with_details(format!(
            "tab size is not representable: {source}"
          ))
        })?;
        let mut formatting_options = formatter::Options {
          indent_string: if parameters.options.insert_spaces {
            " ".repeat(tab_size)
          } else {
            "\t".into()
          },
          ..Default::default()
        };

        if let Some(trailing_newline) = parameters.options.insert_final_newline {
          formatting_options.trailing_newline = trailing_newline;
        }

        formatting_options.update_camel(snapshot.config.formatter.clone());
        snapshot
          .taplo_config
          .update_format_options(&document_path, &mut formatting_options);

        let scopes = snapshot.taplo_config.format_scopes(&document_path);
        tracing::trace!(
            ?document_path,
            ?formatting_options,
            scopes = ?scopes.clone().collect::<Vec<_>>(),
            all_rules = ?snapshot.taplo_config.rule,
            matched_rules = ?snapshot.taplo_config.rules_for(&document_path).collect::<Vec<_>>(),
        );

        let edits = vec![TextEdit {
          range: document.mapper.all_range(),
          new_text: formatter::format_with_path_scopes(
            &document.dom,
            &formatting_options,
            &document
              .parse
              .diagnostics()
              .iter()
              .map(ParseDiagnostic::range)
              .collect::<Vec<_>>(),
            scopes.into_iter(),
          )
          .map_err(|error| RpcError::internal_error().with_details(error.to_string()))?,
        }];
        super::$ensure_current_snapshot(world, &document_uri, &snapshot).await?;
        Ok(Some(edits))
      })
    }
  };
}

define_checked_document_handler_execution_families!(
  define_formatting_future_family;
  (format_local, format_concurrent),
);

#[cfg(test)]
mod tests {
  use futures::executor::block_on;
  use lsp_types::DocumentFormattingParams;
  use serde_json::json;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use taplo_lsp_async::Params;

  use super::format_concurrent;
  use super::format_local;
  use crate::handlers::test_support::concurrent_world;
  use crate::handlers::test_support::local_world;
  use crate::handlers::test_support::url as fixture_url;

  /// Decode formatting parameters through their real wire representation.
  fn parameters(document_uri: &str, insert_spaces: bool, final_newline: Option<bool>) -> Result<DocumentFormattingParams, TestFailure> {
    ensure_ok(
      serde_json::from_value(json!({
        "textDocument": {
          "uri": document_uri
        },
        "options": {
          "tabSize": 2,
          "insertSpaces": insert_spaces,
          "insertFinalNewline": final_newline
        }
      })),
      "the document-formatting parameter fixture must decode",
    )
  }

  #[test]
  fn formatting_applies_client_indentation_and_returns_absence_or_typed_parameter_failure() -> Result<(), TestFailure> {
    block_on(async {
      let world = local_world()?;
      let configuration = json!({
        "schema": {
          "enabled": false,
          "catalogs": []
        },
        "formatter": {
          "indentEntries": true
        }
      });
      drop(ensure_ok(
        world.apply_configuration_values_local(Some(&configuration), &[]).await,
        "the formatting configuration must commit",
      )?);
      let document = fixture_url("file:///workspace/format.toml", "the formatting document URL must parse")?;
      drop(ensure_ok(
        world.replace_document(&document, "[table]\nvalue=1\n").await,
        "the formatting document must install",
      )?);

      let edits = ensure_some(
        ensure_ok(
          format_local(&world, Params::from(Some(parameters(document.as_str(), false, Some(false))?))).await,
          "tab-indented formatting must succeed",
        )?,
        "an installed document must return formatting edits",
      )?;
      let edit = ensure_some(edits.first(), "formatting must return its whole-document edit")?;
      ensure(
        (edit.new_text.as_str(), edit.range.end.line, edit.range.end.character) == ("[table]\n\tvalue = 1", 2, 0),
        "formatting must apply tab indentation, explicit final-newline removal, and the complete source range",
      )?;

      ensure(
        ensure_ok(
          format_local(
            &world,
            Params::from(Some(parameters("file:///workspace/missing.toml", true, Some(true))?)),
          )
          .await,
          "formatting an unopened document must remain a successful absent response",
        )?
        .is_none(),
        "formatting must not fabricate edits for an unopened document",
      )?;

      let concurrent = concurrent_world()?;
      drop(ensure_ok(
        concurrent
          .apply_configuration_values_concurrent(Some(&configuration), &[])
          .await,
        "the concurrent formatting configuration must commit",
      )?);
      drop(ensure_ok(
        concurrent.replace_document_concurrent(&document, "[table]\nvalue=1\n").await,
        "the concurrent formatting document must install",
      )?);
      let concurrent_edits = ensure_some(
        ensure_ok(
          format_concurrent(&concurrent, Params::from(Some(parameters(document.as_str(), false, Some(false))?))).await,
          "concurrent tab-indented formatting must succeed",
        )?,
        "an installed concurrent document must return formatting edits",
      )?;
      ensure(
        concurrent_edits == edits,
        "local and concurrent formatting families must return identical whole-document edits",
      )?;

      let missing_params_error = ensure_some(
        format_local(&world, Params::<DocumentFormattingParams>::from(None)).await.err(),
        "formatting without parameters must return a typed RPC error",
      )?;
      ensure(
        (missing_params_error.code, missing_params_error.details.is_some()) == (-32602, true),
        "formatting without parameters must preserve the standard typed invalid-params response",
      )
    })
  }

  #[test]
  fn formatting_rejects_open_non_local_documents_with_typed_context() -> Result<(), TestFailure> {
    block_on(async {
      let world = local_world()?;
      let document = fixture_url("untitled:format-buffer", "the non-local formatting document URL must parse")?;
      drop(ensure_ok(
        world.replace_document(&document, "value=1\n").await,
        "the non-local formatting document must install",
      )?);

      let error = ensure_some(
        format_local(&world, Params::from(Some(parameters(document.as_str(), true, Some(true))?)))
          .await
          .err(),
        "formatting an open non-local document must return a typed RPC error",
      )?;
      ensure(
        (error.code, error.details.as_ref().and_then(serde_json::Value::as_str))
          == (-32600, Some("invalid (non-local) uri for file: untitled:format-buffer")),
        "non-local formatting rejection must preserve the invalid-request code and exact URI context",
      )
    })
  }
}
