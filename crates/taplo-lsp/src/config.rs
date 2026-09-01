//! Language-server initialization and behavior configuration.

use std::path::PathBuf;

use figment::Figment;
use figment::providers::Serialized;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use taplo::formatter::OptionsIncompleteCamel;
use taplo_common::HashMap;
use taplo_common::config::Rule;
use taplo_common::schema::associations::DEFAULT_CATALOGS;
use taplo_common::schema::cache::DEFAULT_CACHE_EXPIRATION_TIME;
use taplo_common::schema::cache::DEFAULT_LRU_CACHE_EXPIRATION_TIME;
use thiserror::Error;
use url::Url;

/// A typed failure while constructing or updating LSP configuration.
#[derive(Debug, Error)]
pub enum LspConfigError {
  /// Merging or decoding client configuration failed.
  #[error("invalid LSP configuration")]
  Decode {
    /// Underlying Figment failure.
    #[source]
    source: Box<figment::Error>,
  },
  /// A compiled-in default catalog URL is invalid.
  #[error("invalid built-in schema catalog URL `{catalog}`")]
  InvalidDefaultCatalog {
    /// Rejected catalog URL.
    catalog: &'static str,
    /// Underlying URL failure.
    #[source]
    source:  url::ParseError,
  },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Initialization options supplied by the embedding client.
pub struct InitConfig {
  /// Optional directory used for persistent schema cache entries.
  pub cache_path:            Option<PathBuf>,
  /// Client configuration section requested after initialization.
  #[serde(default = "default_configuration_section")]
  pub configuration_section: String,
}

impl Default for InitConfig {
  fn default() -> Self {
    Self {
      cache_path:            None,
      configuration_section: default_configuration_section(),
    }
  }
}

/// Return the historical client configuration section.
fn default_configuration_section() -> String {
  String::from("evenBetterToml")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Complete language-server behavior configuration.
pub struct LspConfig {
  /// Taplo configuration-file discovery behavior.
  pub taplo:      TaploConfig,
  /// Schema loading, association, and caching behavior.
  pub schema:     SchemaConfig,
  /// Completion behavior.
  pub completion: CompletionConfig,
  /// Syntax feature behavior.
  pub syntax:     SyntaxConfig,
  /// Formatter options applied to document-formatting requests.
  pub formatter:  OptionsIncompleteCamel,
  /// Client-provided Taplo file rules.
  pub rules:      Vec<Rule>,
}

impl LspConfig {
  /// Construct the default LSP configuration.
  ///
  /// # Errors
  ///
  /// Returns [`LspConfigError`] if a compiled-in catalog URL is invalid.
  pub fn new() -> Result<Self, LspConfigError> {
    Ok(Self {
      taplo:      TaploConfig::default(),
      schema:     SchemaConfig::new()?,
      completion: CompletionConfig::default(),
      syntax:     SyntaxConfig::default(),
      formatter:  OptionsIncompleteCamel::default(),
      rules:      Vec::new(),
    })
  }

  /// Merge one client JSON object into a cloned configuration and commit it atomically.
  ///
  /// # Errors
  ///
  /// Returns [`LspConfigError`] when Figment cannot merge or decode the value.
  pub fn update_from_json(&mut self, json: &Value) -> Result<(), LspConfigError> {
    let updated = Figment::new()
      .merge(Serialized::defaults(&self))
      .merge(Serialized::defaults(json))
      .extract()
      .map_err(|source| LspConfigError::Decode {
        source: Box::new(source)
      })?;
    *self = updated;
    Ok(())
  }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Completion-specific limits.
pub struct CompletionConfig {
  /// Maximum number of missing keys offered together.
  pub max_keys: usize,
}

impl Default for CompletionConfig {
  fn default() -> Self {
    Self {
      max_keys: 5
    }
  }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Syntax-feature switches.
pub struct SyntaxConfig {
  /// Whether semantic-token requests are enabled.
  pub semantic_tokens: bool,
}

impl Default for SyntaxConfig {
  fn default() -> Self {
    Self {
      semantic_tokens: true
    }
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Schema loading, association, link, and cache configuration.
pub struct SchemaConfig {
  /// Whether schema-backed language features are enabled.
  pub enabled:      bool,
  /// Regular-expression associations supplied by the client.
  pub associations: HashMap<String, String>,
  /// Catalog URLs consulted for schema associations.
  pub catalogs:     Vec<Url>,
  /// Whether document links should expose associated schemas.
  pub links:        bool,
  /// Memory and disk cache expiration policy.
  pub cache:        SchemaCacheConfig,
}

impl Default for SchemaConfig {
  fn default() -> Self {
    Self::without_catalogs()
  }
}

impl SchemaConfig {
  /// Construct the default schema configuration with validated built-in catalogs.
  ///
  /// # Errors
  ///
  /// Returns [`LspConfigError`] if a compiled-in catalog URL is invalid.
  #[allow(
    clippy::single_call_fn,
    reason = "the named fallible constructor separates built-in catalog parsing from `without_catalogs`, so `Default` stays infallible \
              while `LspConfig::new` composes the typed catalog failure"
  )]
  fn new() -> Result<Self, LspConfigError> {
    let mut config = Self::without_catalogs();
    for catalog in DEFAULT_CATALOGS {
      config
        .catalogs
        .push(catalog.parse().map_err(|source| LspConfigError::InvalidDefaultCatalog {
          catalog,
          source,
        })?);
    }
    Ok(config)
  }

  /// Construct schema defaults before fallible catalog parsing.
  fn without_catalogs() -> Self {
    Self {
      enabled:      true,
      associations: HashMap::default(),
      catalogs:     Vec::new(),
      links:        false,
      cache:        SchemaCacheConfig::default(),
    }
  }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Schema cache expiration configuration in seconds.
pub struct SchemaCacheConfig {
  /// In-memory entry lifetime in seconds.
  pub memory_expiration: u64,
  /// Persistent entry lifetime in seconds.
  pub disk_expiration:   u64,
}

impl Default for SchemaCacheConfig {
  fn default() -> Self {
    Self {
      memory_expiration: DEFAULT_LRU_CACHE_EXPIRATION_TIME.as_secs(),
      disk_expiration:   DEFAULT_CACHE_EXPIRATION_TIME.as_secs(),
    }
  }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Taplo configuration-file discovery settings.
pub struct TaploConfig {
  /// Configuration-file discovery behavior.
  pub config_file: TaploConfigFileConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Configuration-file discovery options.
pub struct TaploConfigFileConfig {
  /// Explicit configuration path, if supplied.
  pub path:    Option<PathBuf>,
  /// Whether configuration-file loading is enabled.
  pub enabled: bool,
}

impl Default for TaploConfigFileConfig {
  fn default() -> Self {
    Self {
      path:    None,
      enabled: true,
    }
  }
}
