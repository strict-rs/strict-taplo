//! Client configuration requests and execution-model-specific world transitions.

use lsp_types::ConfigurationItem;
use lsp_types::ConfigurationParams;
use lsp_types::DidChangeConfigurationParams;
use serde_json::Value;
use thiserror::Error;
use url::Url;

use super::diagnostics::DiagnosticBatch;
use super::diagnostics::DiagnosticError;
use crate::lsp_ext::notification::DidChangeSchemaAssociationParams;
use crate::world::WorldError;
use crate::world::WorldState;

/// Global configuration value paired with rooted configuration values.
type ConfigurationValues = (Value, Vec<(Url, Value)>);

/// A typed failure while requesting or applying client configuration.
#[derive(Debug, Error)]
pub(super) enum ConfigurationError {
  /// A workspace root cannot be represented as an LSP URI.
  #[error("workspace root `{root}` is not representable as an LSP URI")]
  UnsupportedWorkspaceUri {
    /// Rejected root URL.
    root: Url,
  },
  /// The client omitted the global configuration value.
  #[error("workspace/configuration response omitted the global value")]
  MissingGlobalValue,
  /// The client omitted a root-scoped configuration value.
  #[error("workspace/configuration response omitted the value for `{root}`")]
  MissingScopedValue {
    /// Root whose captured item has no response value.
    root: Url,
  },
  /// The client returned more values than requested.
  #[error("workspace/configuration returned {actual} values for {expected} requested items")]
  SurplusValues {
    /// Requested value count.
    expected: usize,
    /// Returned value count.
    actual:   usize,
  },
  /// Applying configuration to the world failed.
  #[error(transparent)]
  World(#[from] Box<WorldError>),
  /// Refreshing diagnostics after configuration failed.
  #[error(transparent)]
  Diagnostic(#[from] DiagnosticError),
}

/// Client-visible output after one committed configuration transition.
pub(super) struct ConfigurationEffects {
  /// Current schema associations after configuration replacement.
  pub(super) associations: Vec<DidChangeSchemaAssociationParams>,
  /// Current diagnostics for every open document.
  pub(super) diagnostics:  Vec<DiagnosticBatch>,
}

/// One configuration request together with the captured root identity of each scoped item.
pub(super) struct ConfigurationRequest {
  /// Standard LSP request parameters.
  pub(super) params: ConfigurationParams,
  /// Root identity corresponding to every item after the global item.
  captured_roots:    Vec<Url>,
}

impl ConfigurationRequest {
  /// Pair a complete client response with the captured root identities.
  ///
  /// # Errors
  ///
  /// Returns [`ConfigurationError`] when the response omits requested values or contains surplus
  /// values.
  fn values(&self, response: Vec<Value>) -> Result<ConfigurationValues, ConfigurationError> {
    let expected = self.captured_roots.len().saturating_add(1);
    if response.len() > expected {
      return Err(ConfigurationError::SurplusValues {
        expected,
        actual: response.len(),
      });
    }
    let mut values = response.into_iter();
    let global = values.next().ok_or(ConfigurationError::MissingGlobalValue)?;
    let mut scoped = Vec::with_capacity(self.captured_roots.len());
    for root in &self.captured_roots {
      let scoped_value = values.next().ok_or_else(|| ConfigurationError::MissingScopedValue {
        root: root.clone()
      })?;
      scoped.push((root.clone(), scoped_value));
    }
    Ok((global, scoped))
  }
}

/// Build the global and root-scoped configuration request from a stable topology snapshot.
///
/// # Errors
///
/// Returns [`ConfigurationError`] when a root URL cannot be represented on the LSP wire.
macro_rules! define_configuration_future_family {
  (
    $configuration_request:ident,
    $configuration_change:ident,
    $apply_configuration_response:ident,
    $configuration_effects:ident,
    $rooted_workspace_urls:ident,
    $apply_configuration_values:ident,
    $collect_document_diagnostics:ident;
    $future:ident,
    $environment:path,
    $transport:ident,
    $schema_execution:path;
    [$($value_bound:path),*]
  ) => {
    /// Build the global and root-scoped configuration request from a stable topology snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigurationError`] when a root URL cannot be represented on the LSP wire.
    #[allow(
      clippy::single_call_fn,
      reason = "one configuration-request builder per execution family, driven exactly once by its runtime family's client-configuration \
                exchange"
    )]
    pub(super) fn $configuration_request<E: $environment>(
      world: &WorldState<E, $transport<E>>,
    ) -> $future<'_, Result<ConfigurationRequest, ConfigurationError>> {
      Box::pin(async move {
        let init_config = world.init_config();
        let captured_roots = world.$rooted_workspace_urls().await;
        let mut items = Vec::with_capacity(captured_roots.len().saturating_add(1));
        items.push(ConfigurationItem {
          scope_uri: None,
          section:   Some(init_config.configuration_section.clone()),
        });
        for root in &captured_roots {
          let scope_uri = super::uri::to_uri(root).ok_or_else(|| ConfigurationError::UnsupportedWorkspaceUri {
            root: root.clone()
          })?;
          items.push(ConfigurationItem {
            scope_uri: Some(scope_uri),
            section:   Some(init_config.configuration_section.clone()),
          });
        }
        Ok(ConfigurationRequest {
          params: ConfigurationParams {
            items,
          },
          captured_roots,
        })
      })
    }

    /// Apply a push-style configuration notification to every workspace.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigurationError`] when validation or workspace reinitialization fails.
    #[allow(
      clippy::single_call_fn,
      reason = "one push-style configuration transition per execution family, registered exactly once by its runtime family"
    )]
    pub(super) fn $configuration_change<E: $environment>(
      world: &WorldState<E, $transport<E>>,
      params: DidChangeConfigurationParams,
    ) -> $future<'_, Result<ConfigurationEffects, ConfigurationError>> {
      Box::pin(async move {
        let associations = world
          .$apply_configuration_values(Some(&params.settings), &[])
          .await
          .map_err(Box::new)?;
        $configuration_effects(world, associations).await
      })
    }

    /// Apply a pull-style client configuration response to workspace state.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigurationError`] when the response shape, validation, or workspace
    /// reinitialization fails.
    #[allow(
      clippy::single_call_fn,
      reason = "the named pull-response step keeps captured-root pairing beside the request that captured it, and each execution family \
                expands it for exactly one caller"
    )]
    pub(super) fn $apply_configuration_response<'operation, E: $environment>(
      world: &'operation WorldState<E, $transport<E>>,
      request: &'operation ConfigurationRequest,
      response: Vec<Value>,
    ) -> $future<'operation, Result<ConfigurationEffects, ConfigurationError>> {
      Box::pin(async move {
        let (global, scoped) = request.values(response)?;
        let associations = world
          .$apply_configuration_values(Some(&global), &scoped)
          .await
          .map_err(Box::new)?;
        $configuration_effects(world, associations).await
      })
    }

    /// Collect deterministic post-configuration output from the committed world.
    fn $configuration_effects<E: $environment>(
      world: &WorldState<E, $transport<E>>,
      associations: Vec<DidChangeSchemaAssociationParams>,
    ) -> $future<'_, Result<ConfigurationEffects, ConfigurationError>> {
      Box::pin(async move {
        let diagnostics = super::$collect_document_diagnostics(world, |_| true).await?;
        Ok(ConfigurationEffects {
          associations,
          diagnostics,
        })
      })
    }
  };
}

define_lsp_execution_families!(
  handler
  define_configuration_future_family;
  (configuration_request_local, configuration_request_concurrent),
  (configuration_change_local, configuration_change_concurrent),
  (
    apply_configuration_response_local,
    apply_configuration_response_concurrent
  ),
  (configuration_effects_local, configuration_effects_concurrent),
  (rooted_workspace_urls, rooted_workspace_urls_concurrent),
  (
    apply_configuration_values_local,
    apply_configuration_values_concurrent
  ),
  (
    collect_document_diagnostics_local,
    collect_document_diagnostics_concurrent
  ),
);

#[cfg(test)]
mod tests {
  use futures::executor::block_on;
  use lsp_types::ConfigurationParams;
  use lsp_types::DidChangeConfigurationParams;
  use serde_json::json;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_ok;
  use url::Url;

  use super::ConfigurationError;
  use super::ConfigurationRequest;
  #[cfg(not(target_arch = "wasm32"))]
  use super::apply_configuration_response_concurrent;
  use super::apply_configuration_response_local;
  #[cfg(not(target_arch = "wasm32"))]
  use super::configuration_change_concurrent;
  use super::configuration_change_local;
  #[cfg(not(target_arch = "wasm32"))]
  use super::configuration_request_concurrent;
  use super::configuration_request_local;
  #[cfg(not(target_arch = "wasm32"))]
  use crate::handlers::test_support::concurrent_world;
  use crate::handlers::test_support::ensure_no_client_effects;
  use crate::handlers::test_support::local_world;

  /// Parse one rooted configuration fixture URL.
  fn root(value: &str) -> Result<Url, TestFailure> {
    ensure_ok(Url::parse(value), "the configuration root fixture must parse")
  }

  /// Construct one captured configuration request without wire items.
  #[allow(
    clippy::single_call_fn,
    reason = "the named fixture isolates the captured-root identities that response pairing actually depends on, so the pairing contract \
              can be tested without constructing wire items that play no part in it"
  )]
  fn captured_request(roots: Vec<Url>) -> ConfigurationRequest {
    ConfigurationRequest {
      params:         ConfigurationParams {
        items: Vec::new()
      },
      captured_roots: roots,
    }
  }

  /// Require one detached request to contain only the global configuration item.
  fn ensure_global_request(request: &ConfigurationRequest, context: &'static str) -> Result<(), TestFailure> {
    ensure(
      (
        request.captured_roots.len(),
        request.params.items.len(),
        request
          .params
          .items
          .first()
          .map(|item| (item.scope_uri.clone(), item.section.clone())),
      ) == (0, 1, Some((None, Some(String::from("evenBetterToml"))))),
      context,
    )
  }

  #[test]
  fn configuration_responses_pair_every_scope_and_reject_incomplete_or_surplus_values() -> Result<(), TestFailure> {
    let first = root("file:///workspace/first")?;
    let second = root("file:///workspace/second")?;
    let request = captured_request(vec![first.clone(), second.clone()]);
    let (global, scoped) = ensure_ok(
      request.values(vec![json!({"global": true}), json!({"root": "first"}), json!({"root": "second"})]),
      "a complete configuration response must pair every captured scope",
    )?;
    ensure(
      (global, scoped)
        == (json!({"global": true}), vec![
          (first, json!({"root": "first"})),
          (second.clone(), json!({"root": "second"})),
        ]),
      "configuration response pairing must preserve global value and captured root order",
    )?;

    ensure(
      matches!(request.values(Vec::new()), Err(ConfigurationError::MissingGlobalValue)),
      "an empty configuration response must retain the missing-global typed failure",
    )?;
    ensure(
      matches!(
        request.values(vec![json!({}), json!({})]),
        Err(ConfigurationError::MissingScopedValue {
          root
        }) if root == second
      ),
      "a short configuration response must identify the omitted captured root",
    )?;
    ensure(
      matches!(
        request.values(vec![json!({}), json!({}), json!({}), json!({})]),
        Err(ConfigurationError::SurplusValues {
          expected: 3, actual: 4
        })
      ),
      "a surplus configuration response must retain expected and actual cardinality",
    )
  }

  #[test]
  fn global_configuration_request_and_application_match_across_execution_families() -> Result<(), TestFailure> {
    block_on(async {
      let configuration = json!({
        "schema": {
          "enabled": false,
          "catalogs": []
        }
      });
      let local_world = local_world()?;
      let local_request = ensure_ok(
        configuration_request_local(&local_world).await,
        "the local configuration request must construct",
      )?;
      ensure_global_request(
        &local_request,
        "a detached local world must request exactly one unscoped configuration section",
      )?;
      let local_pull = ensure_ok(
        apply_configuration_response_local(&local_world, &local_request, vec![configuration.clone()]).await,
        "a complete local pull response must apply",
      )?;
      ensure_no_client_effects(
        &local_pull.associations,
        &local_pull.diagnostics,
        "configuration without open documents or associations must emit no local effects",
      )?;
      let local_push = ensure_ok(
        configuration_change_local(&local_world, DidChangeConfigurationParams {
          settings: configuration.clone(),
        })
        .await,
        "a complete local push notification must apply",
      )?;
      ensure_no_client_effects(
        &local_push.associations,
        &local_push.diagnostics,
        "configuration without open documents or associations must emit no pushed local effects",
      )?;

      #[cfg(not(target_arch = "wasm32"))]
      {
        let concurrent_world = concurrent_world()?;
        let concurrent_request = ensure_ok(
          configuration_request_concurrent(&concurrent_world).await,
          "the concurrent configuration request must construct",
        )?;
        ensure_global_request(
          &concurrent_request,
          "a detached concurrent world must request exactly one unscoped configuration section",
        )?;
        let concurrent_pull = ensure_ok(
          apply_configuration_response_concurrent(&concurrent_world, &concurrent_request, vec![configuration.clone()]).await,
          "a complete concurrent pull response must apply",
        )?;
        ensure_no_client_effects(
          &concurrent_pull.associations,
          &concurrent_pull.diagnostics,
          "configuration without open documents or associations must emit no concurrent effects",
        )?;
        let concurrent_push = ensure_ok(
          configuration_change_concurrent(&concurrent_world, DidChangeConfigurationParams {
            settings: configuration
          })
          .await,
          "a complete concurrent push notification must apply",
        )?;
        ensure_no_client_effects(
          &concurrent_push.associations,
          &concurrent_push.diagnostics,
          "configuration without open documents or associations must emit no pushed concurrent effects",
        )?;
      };
      Ok(())
    })
  }
}
