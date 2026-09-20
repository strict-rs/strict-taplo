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
  use std::fmt::Debug;

  use futures::executor::block_on;
  use lsp_types::DocumentFormattingParams;
  use serde_json::json;
  use strict_test_support::ResultFailure;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_that;
  use taplo_lsp_async::Params;

  use super::format_concurrent;
  use super::format_local;
  use crate::handlers::test_support::FixtureFailure;
  use crate::handlers::test_support::concurrent_world;
  use crate::handlers::test_support::local_world;
  use crate::handlers::test_support::url as fixture_url;

  /// Decode formatting parameters through their real wire representation.
  fn parameters(
    document_uri: &str,
    insert_spaces: bool,
    final_newline: Option<bool>,
  ) -> Result<DocumentFormattingParams, ResultFailure<serde_json::Error>> {
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
  fn formatting_applies_client_indentation_and_returns_absence_or_typed_parameter_failure() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let world = local_world()?;
      let concurrent = concurrent_world()?;
      let document = fixture_url("file:///workspace/format.toml", "the formatting document URL must parse")?;
      let local_params = parameters(document.as_str(), false, Some(false))?;
      let concurrent_params = parameters(document.as_str(), false, Some(false))?;
      let missing_params = parameters("file:///workspace/missing.toml", true, Some(true))?;
      let configuration = json!({
        "schema": { "enabled": false, "catalogs": [] },
        "formatter": { "indentEntries": true }
      });
      let configured = world.apply_configuration_values_local(Some(&configuration), &[]).await;
      let installed = world.replace_document(&document, "[table]\nvalue=1\n").await;
      let edits = format_local(&world, Params::from(Some(local_params))).await;
      let missing = format_local(&world, Params::from(Some(missing_params))).await;
      let concurrent_configured = concurrent
        .apply_configuration_values_concurrent(Some(&configuration), &[])
        .await;
      let concurrent_installed = concurrent.replace_document_concurrent(&document, "[table]\nvalue=1\n").await;
      let concurrent_edits = format_concurrent(&concurrent, Params::from(Some(concurrent_params))).await;
      let rejected = format_local(&world, Params::<DocumentFormattingParams>::from(None)).await;
      Ok::<_, FixtureFailure>((
        world,
        concurrent,
        (configured, concurrent_configured),
        (installed, concurrent_installed),
        (edits, concurrent_edits, missing, rejected),
      ))
    });
    ensure_that(
      observed,
      "formatting must preserve indentation, full source ranges, execution-family parity and absence or parameter errors",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        scenario.2.0.is_ok()
          && scenario.2.1.is_ok()
          && scenario.3.0.is_ok()
          && scenario.3.1.is_ok()
          && scenario
            .4
            .0
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .and_then(|changes| changes.first())
            .is_some_and(|edit| (edit.new_text.as_str(), edit.range.end.line, edit.range.end.character) == ("[table]\n\tvalue = 1", 2, 0))
          && scenario.4.1 == scenario.4.0
          && matches!(scenario.4.2, Ok(None))
          && scenario
            .4
            .3
            .as_ref()
            .is_err_and(|error| (error.code, error.details.is_some()) == (-32602, true))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn formatting_rejects_open_non_local_documents_with_typed_context() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let world = local_world()?;
      let document = fixture_url("untitled:format-buffer", "the non-local formatting document URL must parse")?;
      let request = parameters(document.as_str(), true, Some(true))?;
      let installed = world.replace_document(&document, "value=1\n").await;
      let response = format_local(&world, Params::from(Some(request))).await;
      Ok::<_, FixtureFailure>((world, document, installed, response))
    });
    ensure_that(
      observed,
      "non-local formatting must retain the invalid-request code and exact URI context",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        scenario.2.is_ok()
          && scenario.3.as_ref().is_err_and(|error| {
            (error.code, error.details.as_ref().and_then(serde_json::Value::as_str))
              == (-32600, Some("invalid (non-local) uri for file: untitled:format-buffer"))
          })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }
}
