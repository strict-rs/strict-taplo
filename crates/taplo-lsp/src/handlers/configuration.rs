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
#[derive(Debug)]
pub(super) struct ConfigurationEffects {
  /// Current schema associations after configuration replacement.
  pub(super) associations: Vec<DidChangeSchemaAssociationParams>,
  /// Current diagnostics for every open document.
  pub(super) diagnostics:  Vec<DiagnosticBatch>,
}

/// One configuration request together with the captured root identity of each scoped item.
#[derive(Debug)]
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
  use std::fmt::Debug;

  use futures::executor::block_on;
  use lsp_types::ConfigurationParams;
  use lsp_types::DidChangeConfigurationParams;
  use serde_json::json;
  use strict_test_support::ResultFailure;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_that;
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
  use crate::handlers::test_support::FixtureFailure;
  #[cfg(not(target_arch = "wasm32"))]
  use crate::handlers::test_support::concurrent_world;
  use crate::handlers::test_support::local_world;

  /// Parse one rooted configuration fixture URL.
  fn root(value: &str) -> Result<Url, ResultFailure<url::ParseError>> {
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

  /// Native request and transition outcomes for one execution family.
  #[derive(Debug)]
  struct ConfigurationObservation {
    /// Captured global configuration request.
    request: Result<ConfigurationRequest, ConfigurationError>,
    /// Pull response applied when the request was constructed.
    pull:    Option<Result<super::ConfigurationEffects, ConfigurationError>>,
    /// Push notification applied independently of request construction.
    push:    Result<super::ConfigurationEffects, ConfigurationError>,
  }

  #[test]
  fn configuration_responses_pair_every_scope_and_reject_incomplete_or_surplus_values() -> Result<(), impl Debug> {
    let observed = (|| {
      let first = root("file:///workspace/first")?;
      let second = root("file:///workspace/second")?;
      let request = captured_request(vec![first.clone(), second.clone()]);
      let complete = request.values(vec![json!({"global": true}), json!({"root": "first"}), json!({"root": "second"})]);
      let missing_global = request.values(Vec::new());
      let missing_scope = request.values(vec![json!({}), json!({})]);
      let surplus = request.values(vec![json!({}), json!({}), json!({}), json!({})]);
      Ok::<_, ResultFailure<url::ParseError>>((request, first, second, complete, missing_global, missing_scope, surplus))
    })();
    ensure_that(
      observed,
      "configuration responses must preserve captured root order and native missing or surplus value failures",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        scenario.3.as_ref().is_ok_and(|values| {
          *values
            == (json!({"global": true}), vec![
              (scenario.1.clone(), json!({"root": "first"})),
              (scenario.2.clone(), json!({"root": "second"})),
            ])
        }) && matches!(scenario.4, Err(ConfigurationError::MissingGlobalValue))
          && matches!(scenario.5, Err(ConfigurationError::MissingScopedValue { ref root }) if *root == scenario.2)
          && matches!(
            scenario.6,
            Err(ConfigurationError::SurplusValues {
              expected: 3, actual: 4
            })
          )
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn global_configuration_request_and_application_match_across_execution_families() -> Result<(), impl Debug> {
    let observed = block_on(async {
      let configuration = json!({ "schema": { "enabled": false, "catalogs": [] } });
      let local = local_world()?;
      #[cfg(not(target_arch = "wasm32"))]
      let concurrent = concurrent_world()?;
      let local_request = configuration_request_local(&local).await;
      let local_pull = match local_request.as_ref() {
        Ok(request) => Some(apply_configuration_response_local(&local, request, vec![configuration.clone()]).await),
        Err(_) => None,
      };
      let local_push = configuration_change_local(&local, DidChangeConfigurationParams {
        settings: configuration.clone(),
      })
      .await;
      let local_observation = ConfigurationObservation {
        request: local_request,
        pull:    local_pull,
        push:    local_push,
      };
      #[cfg(not(target_arch = "wasm32"))]
      let concurrent_observation = {
        let request = configuration_request_concurrent(&concurrent).await;
        let pull = match request.as_ref() {
          Ok(captured) => Some(apply_configuration_response_concurrent(&concurrent, captured, vec![configuration.clone()]).await),
          Err(_) => None,
        };
        let push = configuration_change_concurrent(&concurrent, DidChangeConfigurationParams {
          settings: configuration
        })
        .await;
        ConfigurationObservation {
          request,
          pull,
          push,
        }
      };
      Ok::<_, FixtureFailure>((
        local,
        #[cfg(not(target_arch = "wasm32"))]
        concurrent,
        [
          local_observation,
          #[cfg(not(target_arch = "wasm32"))]
          concurrent_observation,
        ],
      ))
    });
    let global_application_matches = |family: &ConfigurationObservation| {
      let Ok(ref request) = family.request else {
        return false;
      };
      let Some(Ok(ref pull)) = family.pull else {
        return false;
      };
      let Ok(ref push) = family.push else {
        return false;
      };
      request.captured_roots.is_empty()
        && request.params.items.len() == 1
        && request
          .params
          .items
          .first()
          .is_some_and(|item| item.scope_uri.is_none() && item.section.as_deref() == Some("evenBetterToml"))
        && pull.associations.is_empty()
        && pull.diagnostics.is_empty()
        && push.associations.is_empty()
        && push.diagnostics.is_empty()
    };
    ensure_that(
      observed,
      "both execution families must request one global section and apply pull and push configuration without client effects",
      |result| {
        let Ok(ref scenario) = *result else {
          return false;
        };
        #[cfg(not(target_arch = "wasm32"))]
        let observations = &scenario.2;
        #[cfg(target_arch = "wasm32")]
        let observations = &scenario.1;
        observations.iter().all(global_application_matches)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }
}
